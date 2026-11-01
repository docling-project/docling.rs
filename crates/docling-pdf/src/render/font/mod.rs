//! PDF fonts for the renderer: a font dictionary becomes a [`LoadedFont`]
//! that splits strings into codes, gives each code its advance and its glyph
//! outline in text space, or — for Type 3 — its glyph procedure.
//!
//! Glyph programs: TrueType / OpenType (`/FontFile2`, `/FontFile3 /OpenType`)
//! and bare CFF (`/FontFile3 /Type1C`, `/CIDFontType0C`) through skrifa /
//! read-fonts (#952 — fontations, the maintained successor of ttf-parser),
//! Type 1 (`/FontFile`) through [`type1`]. Glyph selection follows ISO
//! 32000-1 9.6.6 / 9.7.4: a simple font's code goes through `/Differences`,
//! the base encoding or the program's built-in one to a glyph *name* (Type 1,
//! CFF) or through the `cmap` subtables to a glyph *index* (TrueType — (3,0)
//! symbol lookups with the `F0xx` convention, (1,0) by code, (3,1) by the
//! name's Unicode, `post` names, and the bare code when the font has no
//! `cmap`); a composite font's code goes through its CMap to a CID and
//! `/CIDToGIDMap` or the CFF charset to a glyph. A font without a program is
//! drawn with a host face chosen by [`fallback`].

pub mod cmap;
pub mod dotsection;
pub mod encodings;
pub mod fallback;
pub mod type1;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use lopdf::{Dictionary, Document, Object, ObjectId};
use read_fonts::ps::cff::CffFontRef;
use skrifa::font::charmap::{Encoding, EncodingKind};
use skrifa::font::glyph::outline::{DrawMemory, DrawOptions, OutlineSource, PathSink};
use skrifa::font::Font;
use skrifa::GlyphId;

use super::geom::Mat;
use super::objects::{
    as_dict, as_stream, deref, get, get_dict, get_int, get_name, get_num, name, num, nums,
};
use cmap::CMap;

/// A glyph outline in the program's own units with its advance.
pub struct GlyphPath {
    pub path: Option<tiny_skia::Path>,
    pub advance: f64,
}

/// The program that draws the glyphs.
enum Program {
    /// TrueType / OpenType (glyf or CFF table).
    Sfnt {
        font: Font,
        /// glyph units → text space: 1/unitsPerEm. For a CFF table that is
        /// the em the FontMatrix was normalized into (read-fonts folds a
        /// matrix that is not a plain 1/upem scale into the outline, as
        /// FreeType does), so the one factor covers both.
        matrix: Mat,
        /// CID → glyph index for a CID-keyed CFF inside the sfnt.
        cid_to_gid: Option<HashMap<u16, u16>>,
        has_cmap: bool,
        has_glyf: bool,
        /// Glyph names of a CFF-flavoured sfnt, for name lookups.
        names: HashMap<String, u16>,
    },
    /// A bare CFF program.
    Cff {
        font: Font,
        matrix: Mat,
        cid_to_gid: Option<HashMap<u16, u16>>,
        /// glyph name → glyph index over the charset, built once (the
        /// program's own lookup by name is a per-call scan).
        names: HashMap<String, u16>,
    },
    Type1 {
        font: type1::Type1Font,
        matrix: Mat,
    },
    /// A host face standing in for a non-embedded font.
    Fallback {
        face: Arc<fallback::FallbackFace>,
        matrix: Mat,
    },
    None,
}

/// Type 3 glyph procedures.
pub struct Type3 {
    pub font_matrix: Mat,
    pub char_procs: HashMap<String, ObjectId>,
    pub resources: Option<Dictionary>,
}

pub struct LoadedFont {
    pub composite: bool,
    pub type3: Option<Type3>,
    program: Program,
    /// Simple fonts: code → glyph name from `/Encoding` (`/Differences` over
    /// the base encoding), when the PDF gives one.
    encoding_names: HashMap<u8, String>,
    /// The base encoding was named explicitly (`/WinAnsiEncoding`, …).
    has_base_encoding: bool,
    /// The font is symbolic (descriptor flag 3) and the PDF names no encoding.
    symbolic: bool,
    /// Composite fonts: string → codes → CIDs.
    cmap: Option<CMap>,
    cid_to_gid: CidToGid,
    /// code (simple) or CID (composite) → advance in 1/1000 text space.
    widths: HashMap<u32, f64>,
    default_width: Option<f64>,
    /// Type 3: `/Widths` are in glyph space (through `FontMatrix`).
    /// `ToUnicode`, for a fallback face's cmap lookups.
    to_unicode: HashMap<u32, String>,
    /// Standard-14 metrics for a non-embedded font without `/Widths`.
    std14: Option<crate::std14::Std14Widths>,
    /// Glyph cache: code/CID → outline in text space (font size 1).
    cache: RefCell<HashMap<u32, Option<Rc<tiny_skia::Path>>>>,
    advance_cache: RefCell<HashMap<u32, f64>>,
    pub vertical: bool,
    is_dingbats: bool,
    is_symbol_face: bool,
}

enum CidToGid {
    Identity,
    Map(Vec<u8>),
}

fn descriptor<'a>(
    doc: &'a Document,
    fdict: &'a Dictionary,
    composite: bool,
) -> Option<&'a Dictionary> {
    let owner = if composite {
        descendant(doc, fdict)?
    } else {
        fdict
    };
    get_dict(doc, owner, b"FontDescriptor")
}

fn descendant<'a>(doc: &'a Document, fdict: &'a Dictionary) -> Option<&'a Dictionary> {
    match get(doc, fdict, b"DescendantFonts")? {
        Object::Array(a) => a.first().and_then(|o| as_dict(doc, o)),
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

fn base_font_name(doc: &Document, fdict: &Dictionary) -> String {
    let raw = get_name(doc, fdict, b"BaseFont")
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .unwrap_or_default();
    // Strip an `ABCDEF+` subset prefix.
    match raw.split_once('+') {
        Some((pre, rest)) if pre.len() == 6 && pre.bytes().all(|b| b.is_ascii_uppercase()) => {
            rest.to_string()
        }
        _ => raw,
    }
}

impl LoadedFont {
    pub fn load(doc: &Document, fdict: &Dictionary) -> LoadedFont {
        let subtype = get_name(doc, fdict, b"Subtype").unwrap_or(b"");
        let composite = subtype == b"Type0";
        let base_font = base_font_name(doc, fdict);
        let desc = descriptor(doc, fdict, composite);
        let flags = desc.and_then(|d| get_int(doc, d, b"Flags"));
        let symbolic_flag = flags.is_some_and(|f| f & 4 != 0 && f & 32 == 0);

        // --- the embedded program -------------------------------------------
        let mut program = Program::None;
        if let Some(d) = desc {
            if let Some(s) = d.get(b"FontFile2").ok().and_then(|o| as_stream(doc, o)) {
                if let Ok(data) = s.decompressed_content() {
                    program = sfnt_program(data).unwrap_or(Program::None);
                }
            }
            if matches!(program, Program::None) {
                if let Some(s) = d.get(b"FontFile3").ok().and_then(|o| as_stream(doc, o)) {
                    if let Ok(data) = s.decompressed_content() {
                        let st = get_name(doc, &s.dict, b"Subtype").unwrap_or(b"");
                        program = if st == b"OpenType"
                            || data.starts_with(b"OTTO")
                            || data.starts_with(&[0, 1, 0, 0])
                            || data.starts_with(b"true")
                        {
                            sfnt_program(data.clone())
                                .or_else(|| cff_program(data))
                                .unwrap_or(Program::None)
                        } else {
                            cff_program(data.clone())
                                .or_else(|| sfnt_program(data))
                                .unwrap_or(Program::None)
                        };
                    }
                }
            }
            if matches!(program, Program::None) {
                if let Some(s) = d.get(b"FontFile").ok().and_then(|o| as_stream(doc, o)) {
                    if let Ok(data) = s.decompressed_content() {
                        if let Some(f) = type1::Type1Font::parse(&data) {
                            let m =
                                Mat::from_slice(&f.font_matrix).unwrap_or(Mat::scale(0.001, 0.001));
                            program = Program::Type1 { font: f, matrix: m };
                        } else if let Some(p) =
                            cff_program(data.clone()).or_else(|| sfnt_program(data))
                        {
                            // Mislabelled programs happen.
                            program = p;
                        }
                    }
                }
            }
        }

        // --- Type 3 ----------------------------------------------------------
        let type3 = if subtype == b"Type3" {
            let fm = get(doc, fdict, b"FontMatrix")
                .and_then(|o| nums(doc, o))
                .and_then(|v| Mat::from_slice(&v))
                .unwrap_or(Mat::scale(0.001, 0.001));
            let mut char_procs = HashMap::new();
            if let Some(cp) = get_dict(doc, fdict, b"CharProcs") {
                for (k, v) in cp.iter() {
                    if let Object::Reference(id) = v {
                        char_procs.insert(String::from_utf8_lossy(k).into_owned(), *id);
                    }
                }
            }
            let resources = get_dict(doc, fdict, b"Resources").cloned();
            Some(Type3 {
                font_matrix: fm,
                char_procs,
                resources,
            })
        } else {
            None
        };

        // --- encodings -----------------------------------------------------
        let lower = base_font.to_ascii_lowercase();
        let is_symbol_face = lower.starts_with("symbol");
        let is_dingbats = lower.contains("dingbat");
        let mut encoding_names: HashMap<u8, String> = HashMap::new();
        let mut has_base_encoding = false;
        let enc = get(doc, fdict, b"Encoding");
        let mut cmap = None;
        if composite {
            cmap = Some(match enc {
                Some(Object::Name(n)) => CMap::predefined(n),
                Some(Object::Stream(s)) => {
                    let mut c = s
                        .decompressed_content()
                        .ok()
                        .map(|d| CMap::parse(&d))
                        .unwrap_or_else(CMap::identity_h);
                    if let Some(Object::Name(n)) = get(doc, &s.dict, b"UseCMap") {
                        if n.starts_with(b"Identity") && !c.identity {
                            // Codes not covered map to themselves.
                        }
                    }
                    if get_int(doc, &s.dict, b"WMode") == Some(1) {
                        c.vertical = true;
                    }
                    c
                }
                _ => CMap::identity_h(),
            });
        } else {
            let base_table: Option<&'static [(u8, &'static str)]> = match enc {
                Some(Object::Name(n)) => {
                    has_base_encoding = true;
                    encodings::by_name(n)
                }
                Some(Object::Dictionary(d)) => match get_name(doc, d, b"BaseEncoding") {
                    Some(n) => {
                        has_base_encoding = true;
                        encodings::by_name(n)
                    }
                    None => None,
                },
                _ => None,
            };
            // The implicit base: Standard for non-symbolic fonts; the face's
            // own encoding for Symbol/ZapfDingbats; nothing for other
            // symbolic fonts (their program's built-in encoding applies).
            let implicit: Option<&'static [(u8, &'static str)]> = if is_symbol_face
                && matches!(program, Program::None)
            {
                Some(encodings::SYMBOL)
            } else if is_dingbats && matches!(program, Program::None) {
                Some(encodings::ZAPF_DINGBATS)
            } else if !symbolic_flag || matches!(program, Program::None | Program::Fallback { .. })
            {
                Some(encodings::STANDARD)
            } else {
                None
            };
            if let Some(t) = base_table.or(implicit) {
                for &(c, n) in t {
                    encoding_names.insert(c, n.to_string());
                }
            }
            if let Some(Object::Dictionary(d)) = enc {
                if let Some(Object::Array(diffs)) = get(doc, d, b"Differences") {
                    let mut code: i64 = 0;
                    for el in diffs {
                        match deref(doc, el) {
                            Object::Integer(i) => code = *i,
                            Object::Real(r) => code = *r as i64,
                            Object::Name(n) => {
                                if (0..=255).contains(&code) {
                                    encoding_names.insert(
                                        code as u8,
                                        String::from_utf8_lossy(n).into_owned(),
                                    );
                                }
                                code += 1;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // --- widths ----------------------------------------------------------
        let mut widths = HashMap::new();
        let mut default_width = None;
        if composite {
            if let Some(dd) = descendant(doc, fdict) {
                default_width = Some(get_num(doc, dd, b"DW").unwrap_or(1000.0));
                if let Some(Object::Array(w)) = get(doc, dd, b"W") {
                    let mut i = 0;
                    while i < w.len() {
                        let c = w.get(i).map(|o| deref(doc, o)).and_then(num);
                        match (c, w.get(i + 1).map(|o| deref(doc, o))) {
                            (Some(c), Some(Object::Array(list))) => {
                                for (k, wv) in list.iter().enumerate() {
                                    if let Some(wv) = num(deref(doc, wv)) {
                                        widths.insert(c as u32 + k as u32, wv);
                                    }
                                }
                                i += 2;
                            }
                            (Some(c1), Some(o2)) => {
                                if let (Some(c2), Some(wv)) =
                                    (num(o2), w.get(i + 2).map(|o| deref(doc, o)).and_then(num))
                                {
                                    let (c1, c2) = (c1.max(0.0) as u32, c2.max(0.0) as u32);
                                    if c2 >= c1 && c2 - c1 < 65536 {
                                        for cid in c1..=c2 {
                                            widths.insert(cid, wv);
                                        }
                                    }
                                }
                                i += 3;
                            }
                            _ => break,
                        }
                    }
                }
            }
        } else {
            let first = get_int(doc, fdict, b"FirstChar").unwrap_or(0);
            if let Some(Object::Array(w)) = get(doc, fdict, b"Widths") {
                for (k, wv) in w.iter().enumerate() {
                    if let Some(wv) = num(deref(doc, wv)) {
                        let code = first + k as i64;
                        if (0..=255).contains(&code) {
                            widths.insert(code as u32, wv);
                        }
                    }
                }
                if !w.is_empty() {
                    default_width = Some(
                        desc.and_then(|d| get_num(doc, d, b"MissingWidth"))
                            .unwrap_or(0.0),
                    );
                }
            }
            if widths.is_empty() {
                if let Some(mw) = desc.and_then(|d| get_num(doc, d, b"MissingWidth")) {
                    if mw > 0.0 {
                        default_width = Some(mw);
                    }
                }
            }
        }
        let std14 = if widths.is_empty() && !composite && type3.is_none() {
            crate::std14::widths_for(base_font.as_bytes())
        } else {
            None
        };

        // --- CID → GID -------------------------------------------------------
        let mut cid_to_gid = CidToGid::Identity;
        if composite {
            if let Some(dd) = descendant(doc, fdict) {
                if let Some(Object::Stream(s)) = get(doc, dd, b"CIDToGIDMap") {
                    if let Ok(data) = s.decompressed_content() {
                        cid_to_gid = CidToGid::Map(data);
                    }
                }
            }
        }

        // --- fallback face ---------------------------------------------------
        if matches!(program, Program::None) && type3.is_none() {
            let ordering_cjk = composite
                && descendant(doc, fdict)
                    .and_then(|dd| get_dict(doc, dd, b"CIDSystemInfo"))
                    .and_then(|si| get(doc, si, b"Ordering"))
                    .and_then(|o| match o {
                        Object::String(s, _) => Some(s.clone()),
                        _ => None,
                    })
                    .is_some_and(|o| {
                        matches!(
                            o.as_slice(),
                            b"Japan1" | b"GB1" | b"CNS1" | b"Korea1" | b"KR"
                        )
                    });
            let mut style = fallback::style_for(&base_font, flags, None);
            if ordering_cjk {
                style.family = fallback::Family::Cjk;
            }
            if let Some(face) = fallback::face(style) {
                let upem = Some(f64::from(face.font.units_per_em()))
                    .filter(|u| *u > 0.0)
                    .unwrap_or(1000.0);
                program = Program::Fallback {
                    face,
                    matrix: Mat::scale(1.0 / upem, 1.0 / upem),
                };
            }
        }

        let to_unicode = get(doc, fdict, b"ToUnicode")
            .and_then(|o| match o {
                Object::Stream(s) => s.decompressed_content().ok(),
                _ => None,
            })
            .map(|d| crate::textparse::parse_tounicode(&d))
            .unwrap_or_default();

        let vertical = cmap.as_ref().is_some_and(|c| c.vertical);
        LoadedFont {
            composite,
            type3,
            program,
            encoding_names,
            has_base_encoding,
            symbolic: symbolic_flag && enc.is_none(),
            cmap,
            cid_to_gid,
            widths,
            default_width,
            to_unicode,
            std14,
            cache: RefCell::new(HashMap::new()),
            advance_cache: RefCell::new(HashMap::new()),
            vertical,
            is_dingbats,
            is_symbol_face,
        }
    }

    /// Split a string operand into `(code, cid, is_single_byte)`; `cid` is
    /// the code itself for a simple font.
    pub fn decode(&self, bytes: &[u8]) -> Vec<(u32, u32, bool)> {
        match &self.cmap {
            Some(c) => c
                .split(bytes)
                .into_iter()
                .map(|(code, n)| (code, c.cid(code), n == 1))
                .collect(),
            None => bytes
                .iter()
                .map(|&b| (u32::from(b), u32::from(b), true))
                .collect(),
        }
    }

    /// The horizontal advance of `cid` (a code for simple fonts) in text
    /// space (font size 1). Type 3 advances go through the font matrix.
    pub fn advance(&self, code: u32, cid: u32) -> f64 {
        let key = if self.composite { cid } else { code };
        if let Some(a) = self.advance_cache.borrow().get(&key) {
            return *a;
        }
        let a = self.advance_uncached(code, cid);
        self.advance_cache.borrow_mut().insert(key, a);
        a
    }

    fn advance_uncached(&self, code: u32, cid: u32) -> f64 {
        let key = if self.composite { cid } else { code };
        if let Some(t3) = &self.type3 {
            let w = self.widths.get(&key).copied().unwrap_or(0.0);
            let (ax, _) = t3.font_matrix.apply_vec(w, 0.0);
            return ax;
        }
        if let Some(w) = self.widths.get(&key) {
            return w / 1000.0;
        }
        if let Some(std) = &self.std14 {
            if let Some(ch) = self.unicode_of(code, cid) {
                if let Some(w) = std.width(ch) {
                    return w / 1000.0;
                }
            }
        }
        if let Some(dw) = self.default_width {
            if dw > 0.0 || !self.widths.is_empty() {
                return dw / 1000.0;
            }
        }
        // The program's own advance.
        if let Some(adv) = self.program_advance(code, cid) {
            return adv;
        }
        if self.default_width.is_some() {
            return self.default_width.unwrap_or(0.0) / 1000.0;
        }
        0.5
    }

    fn unicode_of(&self, code: u32, cid: u32) -> Option<char> {
        let key = if self.composite { cid } else { code };
        if let Some(s) = self
            .to_unicode
            .get(&key)
            .or_else(|| self.to_unicode.get(&code))
        {
            return s.chars().next();
        }
        if !self.composite {
            if let Some(n) = self.encoding_names.get(&(code as u8)) {
                return crate::textparse::glyph_name_to_char(n.as_bytes());
            }
            return char::from_u32(code).filter(|c| c.is_ascii_graphic() || *c == ' ');
        }
        if self.cmap.as_ref().is_some_and(|c| c.unicode_codes) {
            return char::from_u32(code);
        }
        None
    }

    /// A Type 3 font's procedure name for `code` (its `/Encoding /Differences`).
    pub fn type3_glyph_name(&self, code: u32) -> Option<String> {
        self.encoding_names.get(&(code as u8)).cloned()
    }

    /// The glyph name a simple font's code selects, if the PDF names one.
    fn glyph_name(&self, code: u32) -> Option<&str> {
        self.encoding_names.get(&(code as u8)).map(String::as_str)
    }

    /// The glyph outline for a code, in text space (font size 1), cached.
    pub fn glyph(&self, code: u32, cid: u32) -> Option<Rc<tiny_skia::Path>> {
        let key = if self.composite { cid } else { code };
        if let Some(p) = self.cache.borrow().get(&key) {
            return p.clone();
        }
        let built = self.build_glyph(code, cid).and_then(|g| {
            let m = self.program_matrix();
            g.path.and_then(|p| p.transform(m.to_ts())).map(Rc::new)
        });
        self.cache.borrow_mut().insert(key, built.clone());
        built
    }

    fn program_matrix(&self) -> Mat {
        match &self.program {
            Program::Sfnt { matrix, .. }
            | Program::Cff { matrix, .. }
            | Program::Type1 { matrix, .. }
            | Program::Fallback { matrix, .. } => *matrix,
            Program::None => Mat::scale(0.001, 0.001),
        }
    }

    pub fn has_program(&self) -> bool {
        !matches!(self.program, Program::None)
    }

    fn program_advance(&self, code: u32, cid: u32) -> Option<f64> {
        let g = self.build_glyph(code, cid)?;
        let (ax, _) = self.program_matrix().apply_vec(g.advance, 0.0);
        Some(ax)
    }

    fn build_glyph(&self, code: u32, cid: u32) -> Option<GlyphPath> {
        match &self.program {
            Program::None => None,
            Program::Type1 { font, .. } => {
                let name = self.type1_glyph_name(font, code)?;
                font.glyph(&name)
            }
            Program::Cff {
                font,
                cid_to_gid,
                names,
                ..
            } => {
                let gid = if self.composite {
                    let gid = match &self.cid_to_gid {
                        CidToGid::Map(m) => map_cid(m, cid),
                        CidToGid::Identity => cid,
                    };
                    match cid_to_gid {
                        Some(map) => gid16(*map.get(&(gid as u16))?),
                        None => gid16(gid as u16),
                    }
                } else {
                    self.cff_simple_gid(font, names, code)?
                };
                outline_glyph(font, gid)
            }
            Program::Sfnt {
                font,
                cid_to_gid,
                has_cmap,
                has_glyf,
                names,
                ..
            } => {
                let gid = if self.composite {
                    let gid = match &self.cid_to_gid {
                        CidToGid::Map(m) => map_cid(m, cid),
                        CidToGid::Identity => cid,
                    };
                    match cid_to_gid {
                        Some(map) => gid16(*map.get(&(gid as u16))?),
                        None => gid16(gid as u16),
                    }
                } else {
                    self.sfnt_simple_gid(font, names, code, *has_cmap, *has_glyf)?
                };
                outline_glyph(font, gid)
            }
            Program::Fallback { face, .. } => {
                let gid = self.fallback_gid(&face.font, code, cid)?;
                outline_glyph(&face.font, gid)
            }
        }
    }

    fn type1_glyph_name(&self, font: &type1::Type1Font, code: u32) -> Option<String> {
        let c = code as u8;
        // /Differences (or an explicit base encoding) first, then the
        // program's built-in encoding, then Standard.
        let pdf_name = self.glyph_name(code);
        let builtin = font.encoding.get(&c).cloned();
        let candidates: Vec<String> = if self.symbolic && !self.has_base_encoding {
            // Symbolic without /Encoding: the built-in encoding rules, but a
            // /Differences entry (already merged into encoding_names) wins.
            [pdf_name.map(str::to_string), builtin.clone()]
                .into_iter()
                .flatten()
                .collect()
        } else {
            [pdf_name.map(str::to_string), builtin.clone()]
                .into_iter()
                .flatten()
                .collect()
        };
        for cand in &candidates {
            if font.has_glyph(cand) {
                return Some(cand.clone());
            }
            // `uniXXXX` / AGL name → the program's own name for that character.
            if let Some(ch) = crate::textparse::glyph_name_to_char(cand.as_bytes()) {
                if let Some(n) = self.type1_name_for_char(font, ch) {
                    return Some(n);
                }
            }
        }
        if let Some(n) = encodings::lookup(encodings::STANDARD, c) {
            if font.has_glyph(n) {
                return Some(n.to_string());
            }
        }
        if let Some(b) = builtin {
            return Some(b);
        }
        None
    }

    fn type1_name_for_char(&self, font: &type1::Type1Font, ch: char) -> Option<String> {
        // Reverse AGL over the font's names (small fonts, done rarely).
        font.glyph_names()
            .find(|n| crate::textparse::glyph_name_to_char(n.as_bytes()) == Some(ch))
            .cloned()
    }

    fn cff_simple_gid(
        &self,
        font: &Font,
        names: &HashMap<String, u16>,
        code: u32,
    ) -> Option<GlyphId> {
        let c = code as u8;
        let by_name = |n: &str| -> Option<GlyphId> {
            names
                .get(n)
                .map(|g| gid16(*g))
                .or_else(|| font.glyph_by_name(n))
        };
        if let Some(name) = self.glyph_name(code) {
            if let Some(g) = by_name(name) {
                return Some(g);
            }
            if let Some(ch) = crate::textparse::glyph_name_to_char(name.as_bytes()) {
                // Another name for the same character (`uni0041` vs `A`).
                if let Some(std_name) = agl_name(ch) {
                    if let Some(g) = by_name(std_name) {
                        return Some(g);
                    }
                }
                let uni = format!("uni{:04X}", ch as u32);
                if let Some(g) = by_name(&uni) {
                    return Some(g);
                }
                if let Some(g) = names
                    .iter()
                    .find(|(n, _)| crate::textparse::glyph_name_to_char(n.as_bytes()) == Some(ch))
                    .map(|(_, g)| gid16(*g))
                {
                    return Some(g);
                }
            }
        }
        // The program's built-in encoding.
        if let Some(g) = cff_font(font)
            .and_then(|cff| cff.encoding())
            .and_then(|e| e.map(c))
        {
            if g.to_u32() != 0 {
                return Some(g);
            }
        }
        if let Some(n) = encodings::lookup(encodings::STANDARD, c) {
            if let Some(g) = by_name(n) {
                return Some(g);
            }
        }
        None
    }

    fn sfnt_simple_gid(
        &self,
        font: &Font,
        names: &HashMap<String, u16>,
        code: u32,
        has_cmap: bool,
        has_glyf: bool,
    ) -> Option<GlyphId> {
        let c = code as u8;
        // The cmap subtables, each addressable on its own — `map_code` reads
        // the code as stored in that subtable (no symbol offset, no Mac Roman
        // conversion) and treats a mapping to glyph 0 as none, which is what
        // the three lookups below want.
        let cm = font.charmap();
        let subtables: Vec<Encoding> = cm.encodings().filter(|e| e.sfnt_ids().is_some()).collect();
        let symbol_lookup = |code: u32| -> Option<GlyphId> {
            subtables
                .iter()
                .filter(|e| e.sfnt_ids() == Some((3, 0)))
                .find_map(|e| {
                    [code, 0xF000 + code, 0xF100 + code, 0xF200 + code]
                        .into_iter()
                        .find_map(|probe| cm.map_code(*e, probe))
                })
        };
        let mac_lookup = |code: u32| -> Option<GlyphId> {
            subtables
                .iter()
                .filter(|e| e.sfnt_ids().is_some_and(|(p, _)| p == 1))
                .find_map(|e| cm.map_code(*e, code))
        };
        let unicode_lookup = |ch: char| -> Option<GlyphId> {
            subtables
                .iter()
                .filter(|e| e.kind() == EncodingKind::Unicode)
                .find_map(|e| cm.map_code(*e, ch as u32))
        };
        let name = self.glyph_name(code);
        let symbolic_path = self.symbolic || !self.has_base_encoding && name.is_none();
        if symbolic_path {
            if let Some(g) = symbol_lookup(code) {
                return Some(g);
            }
            if let Some(g) = mac_lookup(code) {
                return Some(g);
            }
        }
        if let Some(n) = name {
            if let Some(ch) = crate::textparse::glyph_name_to_char(n.as_bytes()) {
                if let Some(g) = unicode_lookup(ch) {
                    return Some(g);
                }
                if let Some(g) = symbol_lookup(ch as u32) {
                    return Some(g);
                }
            }
            if let Some(g) = names
                .get(n)
                .map(|g| gid16(*g))
                .or_else(|| font.glyph_by_name(n))
            {
                if g.to_u32() != 0 {
                    return Some(g);
                }
            }
            // `gXX` / `glyphXX` / `index XX` names address glyphs directly.
            if let Some(g) = gid_from_name(n) {
                return Some(gid16(g));
            }
        }
        if !symbolic_path {
            if let Some(g) = symbol_lookup(code) {
                return Some(g);
            }
            if let Some(g) = mac_lookup(code) {
                return Some(g);
            }
        }
        if let Some(std) = encodings::lookup(encodings::STANDARD, c) {
            if let Some(ch) = crate::textparse::glyph_name_to_char(std.as_bytes()) {
                if let Some(g) = unicode_lookup(ch) {
                    return Some(g);
                }
            }
        }
        if !has_cmap || subtables.is_empty() {
            // No usable cmap: the code is the glyph index.
            let _ = has_glyf;
            return Some(gid16(code as u16));
        }
        None
    }

    fn fallback_gid(&self, font: &Font, code: u32, cid: u32) -> Option<GlyphId> {
        let cm = font.charmap();
        // Symbol / Dingbats faces are addressed by glyph name.
        if !self.composite && (self.is_symbol_face || self.is_dingbats) {
            if let Some(n) = self.glyph_name(code) {
                if let Some(g) = font.glyph_by_name(n) {
                    return Some(g);
                }
            }
        }
        if !self.composite {
            if let Some(n) = self.glyph_name(code) {
                if let Some(ch) = crate::textparse::glyph_name_to_char(n.as_bytes()) {
                    if let Some(g) = cm.map_unicode(ch) {
                        return Some(g);
                    }
                }
                if let Some(g) = font.glyph_by_name(n) {
                    return Some(g);
                }
            }
        }
        let ch = self.unicode_of(code, cid)?;
        cm.map_unicode(ch)
    }
}

/// A 16-bit glyph index as the font model's identifier.
fn gid16(g: u16) -> GlyphId {
    GlyphId::new(u32::from(g))
}

/// The CFF program behind `font` — a bare CFF, or an OpenType `CFF ` table.
fn cff_font(font: &Font) -> Option<&CffFontRef<'_>> {
    font.outline_glyphs().cff_font()
}

fn map_cid(map: &[u8], cid: u32) -> u32 {
    let i = cid as usize * 2;
    match (map.get(i), map.get(i + 1)) {
        (Some(hi), Some(lo)) => (u32::from(*hi) << 8) | u32::from(*lo),
        _ => 0,
    }
}

fn gid_from_name(n: &str) -> Option<u16> {
    for prefix in ["glyph", "index", "cid", "g", "G"] {
        if let Some(rest) = n.strip_prefix(prefix) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                return rest.parse().ok();
            }
        }
    }
    None
}

/// The Standard-encoding name of a character (the reverse of the AGL subset
/// used here), for CFF fonts named by convention.
fn agl_name(ch: char) -> Option<&'static str> {
    encodings::STANDARD
        .iter()
        .chain(encodings::WIN_ANSI.iter())
        .find(|(_, n)| crate::textparse::glyph_name_to_char(n.as_bytes()) == Some(ch))
        .map(|(_, n)| *n)
}

struct Builder(tiny_skia::PathBuilder);

impl PathSink for Builder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

/// The outline of `gid` in font units with its advance: `hmtx`'s where the
/// program has one, the charstring's for a bare CFF. `None` for a glyph index
/// the program does not have; a glyph that fails part-way through keeps what
/// it drew, as the renderer always did.
fn outline_glyph(font: &Font, gid: GlyphId) -> Option<GlyphPath> {
    if gid.to_u32() >= font.num_glyphs() {
        return None;
    }
    let mut b = Builder(tiny_skia::PathBuilder::new());
    if let Some(glyph) = font.outline_glyphs().get(gid) {
        let _ = glyph.draw(&DrawOptions::new(), DrawMemory::Scratch, &mut b);
    }
    let advance = f64::from(font.glyph_metrics().h_advance(gid));
    Some(GlyphPath {
        path: b.0.finish(),
        advance,
    })
}

/// glyph units → text space for a program: 1/unitsPerEm. For a CFF program
/// the em is the one read-fonts normalized the FontMatrix into (a matrix
/// that is not a plain scale is applied to the outline itself), so the plain
/// factor is exact for both; a degenerate em falls back to 1/1000.
fn em_matrix(font: &Font) -> Mat {
    let upem = f64::from(font.units_per_em());
    if upem.is_finite() && upem > 0.0 {
        Mat::scale(1.0 / upem, 1.0 / upem)
    } else {
        Mat::scale(0.001, 0.001)
    }
}

fn sfnt_program(mut data: Vec<u8>) -> Option<Program> {
    dotsection::strip_sfnt(&mut data);
    let font = Font::new(data, 0)?;
    if !matches!(font.kind(), skrifa::font::Kind::Sfnt(..)) {
        return None;
    }
    let outlines = font.outline_glyphs();
    let has_glyf = outlines.source() == Some(OutlineSource::Glyf);
    let has_cmap = font.charmap().encodings().any(|e| e.sfnt_ids().is_some());
    let (cid_to_gid, names) = match outlines.cff_font() {
        Some(cff) if !has_glyf => (cid_map(cff), cff_names(cff)),
        _ => (None, HashMap::new()),
    };
    let matrix = em_matrix(&font);
    Some(Program::Sfnt {
        font,
        matrix,
        cid_to_gid,
        has_cmap,
        has_glyf,
        names,
    })
}

fn cff_program(mut data: Vec<u8>) -> Option<Program> {
    dotsection::strip(&mut data);
    let font = Font::new(data, 0)?;
    if !matches!(font.kind(), skrifa::font::Kind::Cff(..)) {
        return None;
    }
    let (cid_to_gid, names) = match cff_font(&font) {
        Some(cff) => (cid_map(cff), cff_names(cff)),
        None => (None, HashMap::new()),
    };
    let matrix = em_matrix(&font);
    Some(Program::Cff {
        font,
        matrix,
        cid_to_gid,
        names,
    })
}

/// glyph name → glyph index over the whole charset (name-keyed fonts only).
fn cff_names(cff: &CffFontRef<'_>) -> HashMap<String, u16> {
    let mut map = HashMap::new();
    if cff.is_cid() {
        return map;
    }
    let Some(charset) = cff.charset() else {
        return map;
    };
    for g in 0..cff.num_glyphs().min(u32::from(u16::MAX)) {
        let Some(sid) = charset.string_id(GlyphId::new(g)) else {
            continue;
        };
        if let Some(n) = cff.string(sid) {
            map.entry(String::from_utf8_lossy(n).into_owned())
                .or_insert(g as u16);
        }
    }
    map
}

/// CID → GID for a CID-keyed CFF (`None` for a name-keyed one): the charset
/// of a CID-keyed font maps each glyph to its CID.
fn cid_map(cff: &CffFontRef<'_>) -> Option<HashMap<u16, u16>> {
    if !cff.is_cid() {
        return None;
    }
    let charset = cff.charset()?;
    let n = cff.num_glyphs().min(u32::from(u16::MAX));
    let mut map = HashMap::with_capacity(n as usize);
    for g in 0..n {
        if let Some(cid) = charset.string_id(GlyphId::new(g)) {
            map.entry(cid.to_u16()).or_insert(g as u16);
        }
    }
    Some(map)
}

/// A per-document font cache keyed by the font dictionary's object id.
#[derive(Default)]
pub struct FontCache {
    by_id: HashMap<ObjectId, Rc<LoadedFont>>,
    /// Direct (unreferenced) font dictionaries, keyed by their debug print.
    by_repr: HashMap<String, Rc<LoadedFont>>,
}

impl FontCache {
    pub fn get(&mut self, doc: &Document, obj: &Object) -> Option<Rc<LoadedFont>> {
        match obj {
            Object::Reference(id) => {
                if let Some(f) = self.by_id.get(id) {
                    return Some(f.clone());
                }
                let d = as_dict(doc, obj)?;
                let f = Rc::new(LoadedFont::load(doc, d));
                self.by_id.insert(*id, f.clone());
                Some(f)
            }
            Object::Dictionary(d) => {
                let key = format!("{d:?}");
                if let Some(f) = self.by_repr.get(&key) {
                    return Some(f.clone());
                }
                let f = Rc::new(LoadedFont::load(doc, d));
                self.by_repr.insert(key, f.clone());
                Some(f)
            }
            _ => None,
        }
    }
}

#[allow(dead_code)]
fn _unused(_: &Dictionary) -> Option<&[u8]> {
    None.and_then(name)
}
