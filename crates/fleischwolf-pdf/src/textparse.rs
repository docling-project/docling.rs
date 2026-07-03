//! Pure-Rust PDF text extraction (replacing pdfium's glyph layer).
//!
//! pdfium reports *rendered* glyph boxes, which diverge from docling's
//! `docling-parse` C++ parser at exactly the points that drive conformance:
//! generated spaces get a zero-width box, combining diacritics get a real-width
//! box, and ligature/fraction glyphs land at different x. This module instead
//! reconstructs each glyph's box from the **font's own advance widths** and the
//! PDF text/graphics matrices — the same information docling-parse uses — so a
//! space is as wide as the font says and a combining mark has zero advance.
//!
//! The output is the same [`Glyph`] stream pdfium produces (native PDF
//! coordinates, y-up), fed straight into the existing docling-parse line
//! sanitizer ([`crate::dp_lines`]). Only the digital text layer is handled here;
//! pages without one still fall back to OCR upstream.

use std::collections::HashMap;
use std::rc::Rc;

use lopdf::{Dictionary, Document, Object};

use crate::pdfium_backend::Glyph;

/// Per-document caches for the content-stream interpreter. Fonts are indirect
/// objects shared by many pages, but were fully re-parsed — ToUnicode CMap
/// decompression + tokenization, embedded Type1 program scan, width tables —
/// for **every page and every Form XObject invocation**; decoded form content
/// streams were likewise re-inflated on every `Do`. Cached per document,
/// keyed by the referenced object id (fonts also by resource name, which
/// feeds the docling-parse font hash). Inline (non-reference) dicts are rare
/// and stay uncached.
#[derive(Default)]
struct DocCaches {
    fonts: HashMap<(lopdf::ObjectId, Vec<u8>), Rc<Font>>,
    forms: HashMap<lopdf::ObjectId, Rc<lopdf::content::Content>>,
}

/// A 2×3 affine matrix `[a b c d e f]`: maps `(x,y)` → `(a·x+c·y+e, b·x+d·y+f)`.
#[derive(Clone, Copy)]
struct Mat {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Mat {
    const ID: Mat = Mat {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// `self ∘ m`: the matrix that applies `self` first, then `m`.
    fn then(self, m: Mat) -> Mat {
        Mat {
            a: self.a * m.a + self.b * m.c,
            b: self.a * m.b + self.b * m.d,
            c: self.c * m.a + self.d * m.c,
            d: self.c * m.b + self.d * m.d,
            e: self.e * m.a + self.f * m.c + m.e,
            f: self.e * m.b + self.f * m.d + m.f,
        }
    }

    fn apply(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
}

/// A parsed font: how to turn raw string bytes into (unicode, advance) pairs.
struct Font {
    /// 2-byte codes (Type0 / Identity-H) vs 1-byte (simple fonts).
    two_byte: bool,
    /// code → Unicode string (from ToUnicode; may be multi-char, e.g. ligatures).
    to_unicode: HashMap<u32, String>,
    /// code → glyph advance, in 1000-unit glyph space.
    widths: HashMap<u32, f64>,
    default_width: f64,
    /// 1-byte fallback decoding when ToUnicode lacks a code (WinAnsi-ish).
    simple_encoding: Option<HashMap<u8, char>>,
    /// code → raw `/Differences` glyph name, for GID-style names (`g115`) that
    /// have no Unicode mapping. docling-parse emits these verbatim as `/g115`
    /// (see the redp5110 bulleted list); matching it keeps no text skipped.
    fallback_names: HashMap<u8, String>,
    /// code → char from the embedded Type1 font program's own `/Encoding` vector,
    /// used only as a last resort for glyphs the base encoding leaves unmapped
    /// (standard TeX math fonts: `λ`, `≤`, …).
    program_encoding: HashMap<u8, char>,
    ascent: f64,
    descent: f64,
    hash: u64,
}

impl Font {
    fn decode_code(&self, code: u32) -> (Option<String>, f64) {
        let w = self
            .widths
            .get(&code)
            .copied()
            .unwrap_or(self.default_width);
        if let Some(s) = self.to_unicode.get(&code) {
            return (Some(decompose_ligatures(s)), w);
        }
        if !self.two_byte {
            // A GID-style `/Differences` name (no Unicode) overrides the base
            // encoding, matching docling's verbatim `/g115` fallback.
            if let Some(name) = self.fallback_names.get(&(code as u8)) {
                return (Some(format!("/{name}")), w);
            }
            if let Some(enc) = &self.simple_encoding {
                if let Some(&ch) = enc.get(&(code as u8)) {
                    return (Some(decompose_ligatures(&ch.to_string())), w);
                }
            }
            // Last resort: the embedded Type1 font program's own `/Encoding`
            // vector (`dup N /glyphname put`). Standard TeX math fonts (CMMI, CMSY,
            // …) ship no PDF `/Encoding` and no ToUnicode, so a glyph like `λ`
            // (CMMI code 21 → `/lambda`) or `≤` (CMSY code 20 → `/lessequal`) has
            // no other mapping and would otherwise be silently dropped. docling
            // recovers these from the same font program. This only fills codes the
            // base encoding left unmapped, so it never changes an existing decode.
            if let Some(&ch) = self.program_encoding.get(&(code as u8)) {
                return (Some(decompose_ligatures(&ch.to_string())), w);
            }
        }
        (None, w)
    }
}

/// Spell out Latin presentation-form ligatures (`ﬁ`→`fi`, `ﬃ`→`ffi`, …) the way
/// docling does, so `configuration`/`difficult` don't keep the ligature glyph.
/// The chars share the ligature's box, so the line sanitizer recomposes them.
fn decompose_ligatures(s: &str) -> String {
    if !s.chars().any(|c| ('\u{FB00}'..='\u{FB06}').contains(&c)) {
        return s.to_string();
    }
    s.chars()
        .map(|c| {
            match c {
                '\u{FB00}' => "ff",
                '\u{FB01}' => "fi",
                '\u{FB02}' => "fl",
                '\u{FB03}' => "ffi",
                '\u{FB04}' => "ffl",
                '\u{FB05}' => "ft",
                '\u{FB06}' => "st",
                _ => return c.to_string(),
            }
            .to_string()
        })
        .collect()
}

fn hash_name(name: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    h.finish()
}

/// Resolve a possibly-indirect object to a dictionary.
fn as_dict<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Dictionary> {
    match obj {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
        _ => None,
    }
}

fn deref<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    }
}

/// Parse one font dictionary into a [`Font`].
fn parse_font(doc: &Document, name: &[u8], fdict: &Dictionary) -> Font {
    let subtype: &[u8] = fdict
        .get(b"Subtype")
        .ok()
        .and_then(|o| o.as_name().ok())
        .unwrap_or(&[]);
    let two_byte = subtype == b"Type0".as_slice();

    let to_unicode = fdict
        .get(b"ToUnicode")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_stream().ok())
        .and_then(|s| s.decompressed_content().ok())
        .map(|data| parse_tounicode(&data))
        .unwrap_or_default();

    let (widths, default_width) = if two_byte {
        cid_widths(doc, fdict)
    } else {
        simple_widths(doc, fdict)
    };

    let simple_encoding = if two_byte {
        None
    } else {
        Some(simple_encoding_table(doc, fdict))
    };
    let fallback_names = if two_byte {
        HashMap::new()
    } else {
        differences_gid_names(doc, fdict)
    };
    let program_encoding = if two_byte {
        HashMap::new()
    } else {
        type1_program_encoding(doc, fdict)
    };

    let (ascent, descent) = font_ascent_descent(doc, fdict, two_byte);

    Font {
        two_byte,
        to_unicode,
        widths,
        default_width,
        simple_encoding,
        fallback_names,
        program_encoding,
        ascent,
        descent,
        hash: hash_name(name),
    }
}

/// Collect `/Differences` entries whose glyph name is a GID placeholder
/// (`g115`, `cid42`, `glyph7`, `index9`) with no Unicode mapping. docling-parse
/// emits such glyphs as the literal name `/g115`; mapping them here keeps the
/// text from being silently dropped (subsetted fonts with no ToUnicode). The
/// GID-name restriction keeps real Adobe glyph names on the normal path so this
/// never invents garbage on the clean files.
fn differences_gid_names(doc: &Document, fdict: &Dictionary) -> HashMap<u8, String> {
    let mut map = HashMap::new();
    let Some(Object::Dictionary(enc)) = fdict.get(b"Encoding").ok().and_then(|o| deref(doc, o))
    else {
        return map;
    };
    let Some(Object::Array(diffs)) = enc.get(b"Differences").ok().and_then(|o| deref(doc, o))
    else {
        return map;
    };
    let mut code = 0u8;
    for el in diffs {
        match el {
            Object::Integer(i) => code = *i as u8,
            Object::Name(name) => {
                if glyph_name_to_char(name).is_none() && is_gid_name(name) {
                    map.insert(code, String::from_utf8_lossy(name).into_owned());
                }
                code = code.wrapping_add(1);
            }
            _ => {}
        }
    }
    map
}

/// Parse the embedded Type1 font program's built-in `/Encoding` vector
/// (`dup <code> /<glyphname> put` entries in the clear-text header before
/// `eexec`) into `code → char`. This is how docling recovers glyphs from
/// standard TeX math fonts (CMMI/CMSY/…) that carry no PDF `/Encoding` and no
/// ToUnicode — e.g. CMMI's `dup 21 /lambda` or CMSY's `dup 20 /lessequal`.
/// Only `FontFile` (Type1) is parsed; CFF (`FontFile3`) and TrueType
/// (`FontFile2`) store their encoding in a binary table and are left alone.
fn type1_program_encoding(doc: &Document, fdict: &Dictionary) -> HashMap<u8, char> {
    let mut map = HashMap::new();
    let Some(desc) = fdict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok())
    else {
        return map;
    };
    let Some(data) = desc
        .get(b"FontFile")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_stream().ok())
        .and_then(|s| s.decompressed_content().ok())
    else {
        return map;
    };
    // The clear-text header (PostScript) ends at `eexec`; the rest is encrypted.
    let head_end = data
        .windows(5)
        .position(|w| w == b"eexec")
        .unwrap_or(data.len());
    let head = String::from_utf8_lossy(&data[..head_end]);
    // Scan for `dup <code> /<name> put` tokens.
    let toks: Vec<&str> = head.split_whitespace().collect();
    for w in toks.windows(4) {
        if w[0] == "dup" && w[3] == "put" {
            if let (Ok(code), Some(name)) = (w[1].parse::<u32>(), w[2].strip_prefix('/')) {
                if code <= 255 {
                    if let Some(ch) = glyph_name_to_char(name.as_bytes()) {
                        map.insert(code as u8, ch);
                    }
                }
            }
        }
    }
    map
}

/// A glyph name that is a synthetic placeholder, not a real Adobe name:
/// `g115`, `cid42`, `glyph7`, `index9`, `G12`, or a short-prefix code name like
/// `SM590000` (IBM BookMaster). These carry no Unicode meaning, and docling-parse
/// emits them verbatim (`/SM590000`). `afii####` / `uni####` are real Adobe names
/// and excluded. The restriction keeps genuine glyph names on the Unicode path.
fn is_gid_name(name: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(name) else {
        return false;
    };
    if s.starts_with("afii") || s.starts_with("uni") {
        return false;
    }
    for prefix in ["g", "G", "cid", "CID", "glyph", "index"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    // Short alpha prefix (≤3 letters) followed by a run of ≥3 digits — synthetic
    // code names like `SM590000`, distinct from real Adobe names (whole words or
    // letter+`.suffix` variants).
    let alpha = s.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
    let digits = s.len() - alpha;
    (1..=3).contains(&alpha)
        && digits >= 3
        && s.as_bytes()[alpha..].iter().all(|b| b.is_ascii_digit())
}

fn font_ascent_descent(doc: &Document, fdict: &Dictionary, two_byte: bool) -> (f64, f64) {
    // For Type0, the descriptor lives on the descendant CIDFont.
    let descr_owner = if two_byte {
        fdict
            .get(b"DescendantFonts")
            .ok()
            .and_then(|o| deref(doc, o))
            .and_then(|o| match o {
                Object::Array(a) => a.first(),
                _ => None,
            })
            .and_then(|o| as_dict(doc, o))
    } else {
        Some(fdict)
    };
    let fd = descr_owner
        .and_then(|d| d.get(b"FontDescriptor").ok())
        .and_then(|o| as_dict(doc, o));
    let asc = fd
        .and_then(|d| d.get(b"Ascent").ok())
        .and_then(|o| {
            o.as_float()
                .ok()
                .or_else(|| o.as_i64().ok().map(|i| i as f32))
        })
        .unwrap_or(750.0) as f64;
    let desc = fd
        .and_then(|d| d.get(b"Descent").ok())
        .and_then(|o| {
            o.as_float()
                .ok()
                .or_else(|| o.as_i64().ok().map(|i| i as f32))
        })
        .unwrap_or(-250.0) as f64;
    // Some subsetted fonts carry a degenerate FontDescriptor (`/Ascent 0
    // /Descent 0`) — the real metrics live in the font program. That collapses
    // the loose box to zero height, so the line cells get zero area and the
    // layout's region/text assignment drops them (2305's References list lost
    // every prose line, keeping only the URLs). Fall back to typical text metrics
    // so the box has height.
    if asc - desc <= 1.0 {
        return (750.0, -250.0);
    }
    (asc, desc)
}

/// Simple-font widths: `/FirstChar` + `/Widths` array, `/MissingWidth` default.
fn simple_widths(doc: &Document, fdict: &Dictionary) -> (HashMap<u32, f64>, f64) {
    let mut map = HashMap::new();
    let first = fdict
        .get(b"FirstChar")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0) as u32;
    if let Some(Object::Array(arr)) = fdict.get(b"Widths").ok().and_then(|o| deref(doc, o)) {
        for (i, w) in arr.iter().enumerate() {
            if let Some(w) = num(w) {
                map.insert(first + i as u32, w);
            }
        }
    }
    let dw = fdict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|o| as_dict(doc, o))
        .and_then(|d| d.get(b"MissingWidth").ok())
        .and_then(num)
        .unwrap_or(0.0);
    (map, dw)
}

/// CIDFont widths: the `/W` array on the descendant font (`/DW` default = 1000).
fn cid_widths(doc: &Document, fdict: &Dictionary) -> (HashMap<u32, f64>, f64) {
    let mut map = HashMap::new();
    let Some(desc) = fdict
        .get(b"DescendantFonts")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| match o {
            Object::Array(a) => a.first(),
            _ => None,
        })
        .and_then(|o| as_dict(doc, o))
    else {
        return (map, 1000.0);
    };
    let dw = desc.get(b"DW").ok().and_then(num).unwrap_or(1000.0);
    if let Some(Object::Array(w)) = desc.get(b"W").ok().and_then(|o| deref(doc, o)) {
        let mut i = 0;
        while i < w.len() {
            let c = w.get(i).and_then(num);
            match (c, w.get(i + 1)) {
                // `c [w1 w2 ...]`: consecutive CIDs starting at c.
                (Some(c), Some(Object::Array(list))) => {
                    for (k, wv) in list.iter().enumerate() {
                        if let Some(wv) = num(wv) {
                            map.insert(c as u32 + k as u32, wv);
                        }
                    }
                    i += 2;
                }
                // `c_first c_last w`: a run all of width w.
                (Some(c1), Some(o2)) => {
                    if let (Some(c2), Some(wv)) = (num(o2), w.get(i + 2).and_then(num)) {
                        for cid in c1 as u32..=c2 as u32 {
                            map.insert(cid, wv);
                        }
                    }
                    i += 3;
                }
                _ => break,
            }
        }
    }
    (map, dw)
}

fn num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// Parse a ToUnicode CMap's `bfchar` / `bfrange` sections into code→string.
fn parse_tounicode(data: &[u8]) -> HashMap<u32, String> {
    let text = String::from_utf8_lossy(data);
    let mut map = HashMap::new();
    let hex = |s: &str| -> Option<Vec<u16>> {
        let s = s.trim();
        if !s.starts_with('<') || !s.ends_with('>') {
            return None;
        }
        let h = &s[1..s.len() - 1];
        let bytes: Vec<u8> = (0..h.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
            .collect();
        Some(
            bytes
                .chunks(2)
                .map(|c| {
                    if c.len() == 2 {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        c[0] as u16
                    }
                })
                .collect(),
        )
    };
    let u16s_to_string = |u: &[u16]| String::from_utf16_lossy(u);
    let code_of = |u: &[u16]| u.iter().fold(0u32, |acc, &x| (acc << 16) | x as u32);

    // Tokenize by structure, not whitespace: CMap hex groups are often written
    // back-to-back with no separators (`<21><21><0054>`), so scan for `<…>`
    // groups, `[`/`]` brackets, and bareword keywords.
    let tokens: Vec<String> = {
        let bytes = text.as_bytes();
        let mut toks = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_whitespace() {
                i += 1;
            } else if c == b'<' {
                let start = i;
                while i < bytes.len() && bytes[i] != b'>' {
                    i += 1;
                }
                i += 1; // include '>'
                toks.push(String::from_utf8_lossy(&bytes[start..i.min(bytes.len())]).into_owned());
            } else if c == b'[' || c == b']' {
                toks.push((c as char).to_string());
                i += 1;
            } else {
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && bytes[i] != b'<'
                    && bytes[i] != b'['
                    && bytes[i] != b']'
                {
                    i += 1;
                }
                toks.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            }
        }
        toks
    };
    let tokens: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "beginbfchar" => {
                i += 1;
                while i + 1 < tokens.len() && tokens[i] != "endbfchar" {
                    if let (Some(src), Some(dst)) = (hex(tokens[i]), hex(tokens[i + 1])) {
                        map.insert(code_of(&src), u16s_to_string(&dst));
                    }
                    i += 2;
                }
            }
            "beginbfrange" => {
                i += 1;
                while i + 2 < tokens.len() && tokens[i] != "endbfrange" {
                    let (Some(lo), Some(hi)) = (hex(tokens[i]), hex(tokens[i + 1])) else {
                        i += 1;
                        continue;
                    };
                    let lo = code_of(&lo);
                    let hi = code_of(&hi);
                    if tokens[i + 2] == "[" {
                        // `<lo> <hi> [ <d0> <d1> ... ]`: one dst per code in the range.
                        let mut j = i + 3;
                        let mut code = lo;
                        while j < tokens.len() && tokens[j] != "]" {
                            if let Some(dst) = hex(tokens[j]) {
                                map.insert(code, u16s_to_string(&dst));
                            }
                            code += 1;
                            j += 1;
                        }
                        i = j + 1;
                    } else if let Some(dst) = hex(tokens[i + 2]) {
                        // `<lo> <hi> <dst>`: consecutive Unicode from a base.
                        let base = code_of(&dst);
                        for (k, code) in (lo..=hi).enumerate() {
                            if let Some(ch) = char::from_u32(base + k as u32) {
                                map.insert(code, ch.to_string());
                            }
                        }
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    map
}

/// Decode a PDF string literal in a Tj/TJ operand into raw code units.
fn codes(font: &Font, bytes: &[u8]) -> Vec<u32> {
    if font.two_byte {
        bytes
            .chunks(2)
            .map(|c| {
                if c.len() == 2 {
                    ((c[0] as u32) << 8) | c[1] as u32
                } else {
                    c[0] as u32
                }
            })
            .collect()
    } else {
        bytes.iter().map(|&b| b as u32).collect()
    }
}

/// Page size (width, height) in PDF points from the MediaBox.
fn page_size(doc: &Document, page_id: lopdf::ObjectId) -> (f32, f32) {
    let mb = doc
        .get_object(page_id)
        .ok()
        .and_then(|o| o.as_dict().ok())
        .and_then(|d| {
            // MediaBox may be inherited; lopdf resolves via get_page... fall back to a guess.
            d.get(b"MediaBox").ok().cloned()
        })
        .or_else(|| {
            doc.get_dictionary(page_id)
                .ok()
                .and_then(|d| d.get(b"MediaBox").ok().cloned())
        });
    if let Some(Object::Array(a)) = mb {
        let v: Vec<f32> = a.iter().filter_map(|o| num(o).map(|x| x as f32)).collect();
        if v.len() == 4 {
            return ((v[2] - v[0]).abs(), (v[3] - v[1]).abs());
        }
    }
    (612.0, 792.0)
}

/// Debug: raw glyph stream `(ch, ll, lr, lb, lt)` (native coords) for page
/// `index`, before the sanitizer. For comparing char cells to docling-parse.
pub fn debug_glyphs(bytes: &[u8], index: usize) -> Vec<(char, f32, f32, f32, f32)> {
    let Ok(doc) = Document::load_mem(bytes) else {
        return Vec::new();
    };
    let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
    pages.sort_by_key(|(n, _)| *n);
    let Some((_, pid)) = pages.get(index) else {
        return Vec::new();
    };
    page_glyphs(&doc, *pid)
        .into_iter()
        .map(|g| (g.ch, g.ll, g.lr, g.lb, g.lt))
        .collect()
}

/// Public entry: per-page (width, height, line cells) for a PDF, via the Rust
/// text parser + the docling-parse line sanitizer. Used by the pipeline and the
/// `textparse_dump` example.
pub fn pdf_textlines(bytes: &[u8]) -> Vec<(f32, f32, Vec<crate::pdfium_backend::TextCell>)> {
    let Ok(doc) = Document::load_mem(bytes) else {
        return Vec::new();
    };
    let mut caches = DocCaches::default();
    let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
    pages.sort_by_key(|(n, _)| *n);
    pages
        .into_iter()
        .map(|(_, pid)| {
            let (w, h) = page_size(&doc, pid);
            let glyphs = page_glyphs_cached(&doc, pid, &mut caches);
            let cells = crate::dp_lines::line_cells(&glyphs, h, true);
            (w, h, cells)
        })
        .collect()
}

/// Debug/diagnostic entry: per-page (width, height, word cells) for a PDF, via
/// the Rust parser glyphs run through the docling-parse word grouping. Used to
/// compare parser word cells against docling-parse's `word_cells` oracle (roadmap
/// item 6).
pub fn pdf_words(bytes: &[u8]) -> Vec<(f32, f32, Vec<crate::pdfium_backend::TextCell>)> {
    let Ok(doc) = Document::load_mem(bytes) else {
        return Vec::new();
    };
    let mut caches = DocCaches::default();
    let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
    pages.sort_by_key(|(n, _)| *n);
    pages
        .into_iter()
        .map(|(_, pid)| {
            let (w, h) = page_size(&doc, pid);
            let glyphs = page_glyphs_cached(&doc, pid, &mut caches);
            let cells = crate::dp_lines::word_cells(&glyphs, h, true);
            (w, h, cells)
        })
        .collect()
}

/// One page's text cells from the pure-Rust parser: prose line cells, per-word
/// cells, and code line cells — all from a single glyph parse. Replaces the
/// pdfium text path (roadmap item 6) when the parser drop is enabled.
#[derive(Default)]
pub struct PageParserCells {
    pub prose: Vec<crate::pdfium_backend::TextCell>,
    pub words: Vec<crate::pdfium_backend::TextCell>,
    pub code: Vec<crate::pdfium_backend::TextCell>,
}

/// Full parser text layer: prose + word + code cells per page, glyphs parsed once.
/// `prose`/`words` come from the docling-parse contraction ([`crate::dp_lines`]);
/// `code` splits only at the parser's own space glyphs (monospace keeps its
/// source spacing). Used by the pipeline to retire pdfium's text path.
pub fn pdf_all_cells(bytes: &[u8]) -> Vec<PageParserCells> {
    let Ok(doc) = Document::load_mem(bytes) else {
        return Vec::new();
    };
    let mut caches = DocCaches::default();
    let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
    pages.sort_by_key(|(n, _)| *n);
    pages
        .into_iter()
        .map(|(_, pid)| {
            let (_w, h) = page_size(&doc, pid);
            let glyphs = page_glyphs_cached(&doc, pid, &mut caches);
            let (prose, words) = crate::dp_lines::line_and_word_cells(&glyphs, h, true);
            PageParserCells {
                prose,
                words,
                code: crate::pdfium_backend::code_cells_from_glyphs(&glyphs, h),
            }
        })
        .collect()
}

/// The text-state scalars inherited by a Form XObject when it is invoked via
/// `Do` (the PDF graphics state includes the text parameters, but not the text
/// matrices, which a form re-establishes inside its own `BT`/`ET`).
#[derive(Clone, Copy)]
struct TextState {
    tc: f64,
    tw: f64,
    th: f64,
    tl: f64,
    trise: f64,
    fsize: f64,
}

impl TextState {
    const INIT: TextState = TextState {
        tc: 0.0,
        tw: 0.0,
        th: 1.0,
        tl: 0.0,
        trise: 0.0,
        fsize: 0.0,
    };
}

/// The effective `/Resources` dictionary for a page (inline or via reference,
/// falling back to an inherited one from a `/Parent`).
fn page_res(doc: &Document, page_id: lopdf::ObjectId) -> Option<&Dictionary> {
    let (inline, ids) = doc.get_page_resources(page_id).ok()?;
    if let Some(d) = inline {
        return Some(d);
    }
    ids.into_iter().find_map(|id| doc.get_dictionary(id).ok())
}

/// Build the code→[`Font`] map for a resources dictionary's `/Font` sub-dict,
/// reusing the per-document cache for fonts referenced indirectly (the common
/// case — the same font objects recur on every page).
fn fonts_from_res(
    doc: &Document,
    res: &Dictionary,
    caches: &mut DocCaches,
) -> HashMap<Vec<u8>, Rc<Font>> {
    let mut map = HashMap::new();
    let font_dict = res
        .get(b"Font")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok());
    if let Some(fd) = font_dict {
        for (name, value) in fd.iter() {
            let font = match value {
                Object::Reference(id) => {
                    let key = (*id, name.clone());
                    if let Some(f) = caches.fonts.get(&key) {
                        Rc::clone(f)
                    } else if let Some(fdict) = deref(doc, value).and_then(|o| o.as_dict().ok()) {
                        let f = Rc::new(parse_font(doc, name, fdict));
                        caches.fonts.insert(key, Rc::clone(&f));
                        f
                    } else {
                        continue;
                    }
                }
                _ => {
                    if let Some(fdict) = deref(doc, value).and_then(|o| o.as_dict().ok()) {
                        Rc::new(parse_font(doc, name, fdict))
                    } else {
                        continue;
                    }
                }
            };
            map.insert(name.clone(), font);
        }
    }
    map
}

/// Extract every glyph on a page as a native-coordinate [`Glyph`].
pub(crate) fn page_glyphs(doc: &Document, page_id: lopdf::ObjectId) -> Vec<Glyph> {
    page_glyphs_cached(doc, page_id, &mut DocCaches::default())
}

/// [`page_glyphs`] with an explicit per-document cache, so a multi-page walk
/// parses each font / decodes each form once instead of once per page.
fn page_glyphs_cached(
    doc: &Document,
    page_id: lopdf::ObjectId,
    caches: &mut DocCaches,
) -> Vec<Glyph> {
    let mut out = Vec::new();
    let Ok(content_bytes) = doc.get_page_content(page_id) else {
        return out;
    };
    let Ok(content) = lopdf::content::Content::decode(&content_bytes) else {
        return out;
    };
    if let Some(res) = page_res(doc, page_id) {
        run_content(
            doc,
            res,
            &content,
            Mat::ID,
            TextState::INIT,
            0,
            caches,
            &mut out,
        );
    }
    out
}

/// Run a content stream's operators, emitting glyphs into `out`. Recurses into
/// Form XObjects on `Do` (bulk body text in heavy PDFs lives inside a form, not
/// the page content stream). `res` is the resources dict in scope (the page's,
/// or the form's own); `base_ctm` is the CTM at the point of invocation.
#[allow(clippy::too_many_arguments)]
fn run_content(
    doc: &Document,
    res: &Dictionary,
    content: &lopdf::content::Content,
    base_ctm: Mat,
    init: TextState,
    depth: u32,
    caches: &mut DocCaches,
    out: &mut Vec<Glyph>,
) {
    let fonts = fonts_from_res(doc, res, caches);
    let xobjects = res
        .get(b"XObject")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok());

    // Graphics + text state. `q`/`Q` save and restore the whole graphics state,
    // which includes the text parameters (Tc, Tw, Tz, TL, Tfs, Trise, font) —
    // *not* the text matrix (that is reset by BT). Saving only the CTM let a Tc
    // set inside a `q…Q` block leak out and drift every later glyph.
    #[allow(clippy::type_complexity)]
    let mut gstate_stack: Vec<(Mat, f64, f64, f64, f64, f64, f64, Option<&Rc<Font>>)> = Vec::new();
    let mut ctm = base_ctm;
    let mut tm = Mat::ID;
    let mut tlm = Mat::ID;
    let mut font: Option<&Rc<Font>> = None;
    let mut fsize = init.fsize;
    let mut tc = init.tc; // char spacing
    let mut tw = init.tw; // word spacing
    let mut th = init.th; // horizontal scale (Tz/100)
    let mut tl = init.tl; // leading
    let mut trise = init.trise;

    let op_f = |operands: &[Object], i: usize| operands.get(i).and_then(num).unwrap_or(0.0);

    for op in &content.operations {
        let operands = &op.operands;
        match op.operator.as_str() {
            "q" => gstate_stack.push((ctm, tc, tw, th, tl, trise, fsize, font)),
            "Q" => {
                if let Some((c, a, b, h, l, r, fs, f)) = gstate_stack.pop() {
                    ctm = c;
                    tc = a;
                    tw = b;
                    th = h;
                    tl = l;
                    trise = r;
                    fsize = fs;
                    font = f;
                }
            }
            "cm" => {
                let m = Mat {
                    a: op_f(operands, 0),
                    b: op_f(operands, 1),
                    c: op_f(operands, 2),
                    d: op_f(operands, 3),
                    e: op_f(operands, 4),
                    f: op_f(operands, 5),
                };
                ctm = m.then(ctm);
            }
            "BT" => {
                tm = Mat::ID;
                tlm = Mat::ID;
            }
            "ET" => {}
            "Tf" => {
                if let Some(Object::Name(n)) = operands.first() {
                    font = fonts.get(n.as_slice());
                }
                fsize = op_f(operands, 1);
            }
            "Td" => {
                tlm = Mat {
                    a: 1.0,
                    b: 0.0,
                    c: 0.0,
                    d: 1.0,
                    e: op_f(operands, 0),
                    f: op_f(operands, 1),
                }
                .then(tlm);
                tm = tlm;
            }
            "TD" => {
                tl = -op_f(operands, 1);
                tlm = Mat {
                    a: 1.0,
                    b: 0.0,
                    c: 0.0,
                    d: 1.0,
                    e: op_f(operands, 0),
                    f: op_f(operands, 1),
                }
                .then(tlm);
                tm = tlm;
            }
            "Tm" => {
                tlm = Mat {
                    a: op_f(operands, 0),
                    b: op_f(operands, 1),
                    c: op_f(operands, 2),
                    d: op_f(operands, 3),
                    e: op_f(operands, 4),
                    f: op_f(operands, 5),
                };
                tm = tlm;
            }
            "T*" => {
                tlm = Mat {
                    a: 1.0,
                    b: 0.0,
                    c: 0.0,
                    d: 1.0,
                    e: 0.0,
                    f: -tl,
                }
                .then(tlm);
                tm = tlm;
            }
            "Tc" => tc = op_f(operands, 0),
            "Tw" => tw = op_f(operands, 0),
            "Tz" => th = op_f(operands, 0) / 100.0,
            "TL" => tl = op_f(operands, 0),
            "Ts" => trise = op_f(operands, 0),
            "Tj" | "'" | "\"" => {
                if op.operator == "'" || op.operator == "\"" {
                    // move to next line first
                    tlm = Mat {
                        a: 1.0,
                        b: 0.0,
                        c: 0.0,
                        d: 1.0,
                        e: 0.0,
                        f: -tl,
                    }
                    .then(tlm);
                    tm = tlm;
                }
                if op.operator == "\"" {
                    // `aw ac string "` sets word- and char-spacing before
                    // showing the string (PDF 32000-1 §9.4.3), persisting after.
                    tw = op_f(operands, 0);
                    tc = op_f(operands, 1);
                }
                if let (Some(f), Some(Object::String(s, _))) = (font, operands.last()) {
                    show_text(f, s, fsize, tc, tw, th, trise, &mut tm, ctm, out);
                }
            }
            "TJ" => {
                if let (Some(f), Some(Object::Array(arr))) = (font, operands.first()) {
                    for el in arr {
                        match el {
                            Object::String(s, _) => {
                                show_text(f, s, fsize, tc, tw, th, trise, &mut tm, ctm, out)
                            }
                            other => {
                                if let Some(adj) = num(other) {
                                    // negative number moves text right (PDF: subtract)
                                    let tx = -adj / 1000.0 * fsize * th;
                                    tm = Mat {
                                        a: 1.0,
                                        b: 0.0,
                                        c: 0.0,
                                        d: 1.0,
                                        e: tx,
                                        f: 0.0,
                                    }
                                    .then(tm);
                                }
                            }
                        }
                    }
                }
            }
            "Do" => {
                // Invoke a Form XObject: bulk body text in many PDFs lives inside
                // a form, reached only here. Image XObjects are skipped (no text).
                if depth >= 8 {
                    continue;
                }
                let Some(Object::Name(n)) = operands.first() else {
                    continue;
                };
                let obj = xobjects.and_then(|d| d.get(n.as_slice()).ok());
                let form_id = match obj {
                    Some(Object::Reference(id)) => Some(*id),
                    _ => None,
                };
                let stream = obj
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_stream().ok());
                let Some(stream) = stream else { continue };
                let is_form = stream
                    .dict
                    .get(b"Subtype")
                    .ok()
                    .and_then(|o| o.as_name().ok())
                    == Some(b"Form".as_slice());
                if !is_form {
                    continue;
                }
                // Decode the form's content once per document (headers/footers
                // and bulk body text invoke the same form on every page).
                let cached = form_id.and_then(|id| caches.forms.get(&id).cloned());
                let form_content = match cached {
                    Some(c) => c,
                    None => {
                        let Ok(data) = stream.decompressed_content() else {
                            continue;
                        };
                        let Ok(c) = lopdf::content::Content::decode(&data) else {
                            continue;
                        };
                        let c = Rc::new(c);
                        if let Some(id) = form_id {
                            caches.forms.insert(id, Rc::clone(&c));
                        }
                        c
                    }
                };
                // The form's /Matrix maps form space into the CTM at invocation.
                let form_mat = match stream.dict.get(b"Matrix").ok() {
                    Some(Object::Array(a)) if a.len() == 6 => {
                        let v: Vec<f64> = a.iter().filter_map(num).collect();
                        if v.len() == 6 {
                            Mat {
                                a: v[0],
                                b: v[1],
                                c: v[2],
                                d: v[3],
                                e: v[4],
                                f: v[5],
                            }
                        } else {
                            Mat::ID
                        }
                    }
                    _ => Mat::ID,
                };
                // The form's own /Resources, falling back to the inherited ones.
                let form_res = stream
                    .dict
                    .get(b"Resources")
                    .ok()
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_dict().ok())
                    .unwrap_or(res);
                let state = TextState {
                    tc,
                    tw,
                    th,
                    tl,
                    trise,
                    fsize,
                };
                run_content(
                    doc,
                    form_res,
                    &form_content,
                    form_mat.then(ctm),
                    state,
                    depth + 1,
                    caches,
                    out,
                );
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn show_text(
    font: &Font,
    bytes: &[u8],
    fsize: f64,
    tc: f64,
    tw: f64,
    th: f64,
    trise: f64,
    tm: &mut Mat,
    ctm: Mat,
    out: &mut Vec<Glyph>,
) {
    for code in codes(font, bytes) {
        let (text, w) = font.decode_code(code);
        let w0 = w / 1000.0; // advance in text-space (em) units
                             // The glyph→user transform: scale glyph space by font size, then Tm, CTM.
        let scale = Mat {
            a: fsize * th,
            b: 0.0,
            c: 0.0,
            d: fsize,
            e: 0.0,
            f: trise,
        };
        let trm = scale.then(*tm).then(ctm);
        // Box in glyph space (1000-unit em): x 0..w, y descent..ascent.
        let (x0, y0) = trm.apply(0.0, font.descent / 1000.0);
        let (x1, _y1) = trm.apply(w0, font.descent / 1000.0);
        let (_x2, y2) = trm.apply(0.0, font.ascent / 1000.0);
        let (left, right) = (x0.min(x1), x0.max(x1));
        let (bot, top) = (y0.min(y2), y0.max(y2));
        if let Some(s) = text {
            // A run may map one code to multiple chars (ligature/fraction); share box.
            for ch in s.chars() {
                if ch != '\u{0}' {
                    out.push(Glyph {
                        ch,
                        l: left as f32,
                        b: bot as f32,
                        r: right as f32,
                        t: top as f32,
                        ll: left as f32,
                        lb: bot as f32,
                        lr: right as f32,
                        lt: top as f32,
                        font: font.hash,
                    });
                }
            }
        }
        // Advance the text matrix. Word spacing applies to single-byte code 32.
        let is_space = !font.two_byte && code == 32;
        let tx = (w0 * fsize + tc + if is_space { tw } else { 0.0 }) * th;
        *tm = Mat {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: tx,
            f: 0.0,
        }
        .then(*tm);
    }
}

/// Build a simple font's code→char table from its `/Encoding`: the base
/// encoding (WinAnsi / MacRoman) plus any `/Differences` overrides (glyph names
/// resolved through a small Adobe-glyph-name subset).
fn simple_encoding_table(doc: &Document, fdict: &Dictionary) -> HashMap<u8, char> {
    let enc = fdict.get(b"Encoding").ok().and_then(|o| deref(doc, o));
    let base_name = match enc {
        Some(Object::Name(n)) => n.clone(),
        Some(Object::Dictionary(d)) => d
            .get(b"BaseEncoding")
            .ok()
            .and_then(|o| o.as_name().ok())
            .map(|n| n.to_vec())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let mut m = if base_name == b"MacRomanEncoding" {
        macroman_table()
    } else {
        winansi_table()
    };
    // Apply /Differences: `code /glyphname /glyphname ... code ...`.
    if let Some(Object::Dictionary(d)) = enc {
        if let Some(Object::Array(diffs)) = d.get(b"Differences").ok().and_then(|o| deref(doc, o)) {
            let mut code = 0u8;
            for el in diffs {
                match el {
                    Object::Integer(i) => code = *i as u8,
                    Object::Name(name) => {
                        if let Some(ch) = glyph_name_to_char(name) {
                            m.insert(code, ch);
                        }
                        code = code.wrapping_add(1);
                    }
                    _ => {}
                }
            }
        }
    }
    m
}

/// Resolve an Adobe glyph name to Unicode: `uniXXXX`, single ASCII letters, the
/// digit/punctuation names from the Adobe Glyph List, and common typographic
/// names. A `.suffix` (`one.taboldstyle`, `a.sc`) is stripped and the base name
/// retried — docling renders these as the base character.
fn glyph_name_to_char(name: &[u8]) -> Option<char> {
    let s = std::str::from_utf8(name).ok()?;
    if let Some(hex) = s.strip_prefix("uni") {
        if let Ok(cp) = u32::from_str_radix(hex.get(0..4)?, 16) {
            return char::from_u32(cp);
        }
    }
    // Single ASCII letter names (`A`, `m`) map to themselves.
    if s.len() == 1 {
        let b = s.as_bytes()[0];
        if b.is_ascii_alphabetic() {
            return Some(b as char);
        }
    }
    let resolved = match s {
        "space" => ' ',
        "exclam" => '!',
        "quotedbl" => '"',
        "numbersign" => '#',
        "dollar" => '$',
        "percent" => '%',
        "ampersand" => '&',
        "quotesingle" => '\'',
        "parenleft" => '(',
        "parenright" => ')',
        "asterisk" => '*',
        "plus" => '+',
        "comma" => ',',
        "hyphen" => '-',
        "period" => '.',
        "slash" => '/',
        "zero" => '0',
        "one" => '1',
        "two" => '2',
        "three" => '3',
        "four" => '4',
        "five" => '5',
        "six" => '6',
        "seven" => '7',
        "eight" => '8',
        "nine" => '9',
        "colon" => ':',
        "semicolon" => ';',
        "less" => '<',
        "equal" => '=',
        "greater" => '>',
        "question" => '?',
        "at" => '@',
        "bracketleft" => '[',
        "backslash" => '\\',
        "bracketright" => ']',
        "asciicircum" => '^',
        "underscore" => '_',
        "grave" => '`',
        "braceleft" => '{',
        "bar" => '|',
        "braceright" => '}',
        "asciitilde" => '~',
        "bullet" => '\u{2022}',
        "periodcentered" => '\u{00B7}',
        "endash" => '\u{2013}',
        "emdash" => '\u{2014}',
        "quoteright" => '\u{2019}',
        "quoteleft" => '\u{2018}',
        "quotedblleft" => '\u{201C}',
        "quotedblright" => '\u{201D}',
        "quotedblbase" => '\u{201E}',
        "quotesinglbase" => '\u{201A}',
        // Latin f-ligatures named in `/Differences` (e.g. 2305's body font). These
        // map to the presentation-form code points, which `decompose_ligatures`
        // then spells back out (`ff`→"ff") — without them the glyph decodes to
        // nothing and the sanitizer fills the gap with a space (`di erences`).
        "ff" => '\u{FB00}',
        "fi" => '\u{FB01}',
        "fl" => '\u{FB02}',
        "ffi" => '\u{FB03}',
        "ffl" => '\u{FB04}',
        "ft" => '\u{FB05}',
        "st" => '\u{FB06}',
        "degree" => '\u{00B0}',
        "trademark" => '\u{2122}',
        "registered" => '\u{00AE}',
        "copyright" => '\u{00A9}',
        "ellipsis" => '\u{2026}',
        "minus" => '\u{2212}',
        "fraction" => '\u{2044}',
        "nbspace" => '\u{00A0}',
        // Greek + math glyph names (standard Adobe Glyph List). Standard TeX math
        // fonts (CMMI/CMSY/…) name their glyphs this way in the embedded font
        // program's `/Encoding`; without these a `λ`/`≤` decodes to nothing and is
        // dropped from body text (`and λ set to 0.5` → `and set to 0.5`).
        "alpha" => '\u{03B1}',
        "beta" => '\u{03B2}',
        "gamma" => '\u{03B3}',
        "delta" => '\u{03B4}',
        "epsilon" | "epsilon1" => '\u{03B5}',
        "zeta" => '\u{03B6}',
        "eta" => '\u{03B7}',
        "theta" | "theta1" => '\u{03B8}',
        "iota" => '\u{03B9}',
        "kappa" => '\u{03BA}',
        "lambda" => '\u{03BB}',
        "mu" => '\u{03BC}',
        "nu" => '\u{03BD}',
        "xi" => '\u{03BE}',
        "omicron" => '\u{03BF}',
        "pi" | "pi1" => '\u{03C0}',
        "rho" | "rho1" => '\u{03C1}',
        "sigma" => '\u{03C3}',
        "sigma1" => '\u{03C2}',
        "tau" => '\u{03C4}',
        "upsilon" => '\u{03C5}',
        "phi" | "phi1" => '\u{03C6}',
        "chi" => '\u{03C7}',
        "psi" => '\u{03C8}',
        "omega" | "omega1" => '\u{03C9}',
        "Gamma" => '\u{0393}',
        "Delta" => '\u{0394}',
        "Theta" => '\u{0398}',
        "Lambda" => '\u{039B}',
        "Xi" => '\u{039E}',
        "Pi" => '\u{03A0}',
        "Sigma" => '\u{03A3}',
        "Upsilon" => '\u{03A5}',
        "Phi" => '\u{03A6}',
        "Psi" => '\u{03A8}',
        "Omega" => '\u{03A9}',
        "lessequal" => '\u{2264}',
        "greaterequal" => '\u{2265}',
        "notequal" => '\u{2260}',
        "approxequal" => '\u{2248}',
        "equivalence" => '\u{2261}',
        "element" => '\u{2208}',
        "plusminus" => '\u{00B1}',
        "multiply" => '\u{00D7}',
        "divide" => '\u{00F7}',
        "infinity" => '\u{221E}',
        "partialdiff" => '\u{2202}',
        "gradient" => '\u{2207}',
        "summation" => '\u{2211}',
        "product" => '\u{220F}',
        "integral" => '\u{222B}',
        "radical" => '\u{221A}',
        "proportional" => '\u{221D}',
        "arrowright" => '\u{2192}',
        "arrowleft" => '\u{2190}',
        "arrowup" => '\u{2191}',
        "arrowdown" => '\u{2193}',
        "arrowboth" => '\u{2194}',
        "arrowdblright" => '\u{21D2}',
        "logicaland" => '\u{2227}',
        "logicalor" => '\u{2228}',
        "intersection" => '\u{2229}',
        "union" => '\u{222A}',
        "similar" => '\u{223C}',
        "congruent" => '\u{2245}',
        "dotmath" => '\u{22C5}',
        "asteriskmath" => '\u{2217}',
        _ => {
            // Strip an AGL `.suffix` (oldstyle/small-cap variant) and retry.
            if let Some((base, _)) = s.split_once('.') {
                if !base.is_empty() {
                    return glyph_name_to_char(base.as_bytes());
                }
            }
            return None;
        }
    };
    Some(resolved)
}

/// Minimal WinAnsiEncoding (Latin-1-ish) for simple fonts lacking ToUnicode.
fn winansi_table() -> HashMap<u8, char> {
    let mut m = HashMap::new();
    for b in 0x20u8..=0x7e {
        m.insert(b, b as char);
    }
    // High range: Windows-1252 printable points that differ from Latin-1.
    let extra: &[(u8, char)] = &[
        (0x91, '\u{2018}'),
        (0x92, '\u{2019}'),
        (0x93, '\u{201C}'),
        (0x94, '\u{201D}'),
        (0x95, '\u{2022}'),
        (0x96, '\u{2013}'),
        (0x97, '\u{2014}'),
        (0x85, '\u{2026}'),
        (0xA0, '\u{00A0}'),
    ];
    for &(b, c) in extra {
        m.insert(b, c);
    }
    for b in 0xA1u8..=0xFF {
        m.entry(b).or_insert(b as char);
    }
    m
}

/// Minimal MacRomanEncoding: ASCII plus the high-range points our corpus hits
/// (notably 0xA5 = bullet, used as a list marker).
fn macroman_table() -> HashMap<u8, char> {
    let mut m = HashMap::new();
    for b in 0x20u8..=0x7e {
        m.insert(b, b as char);
    }
    let high: &[(u8, char)] = &[
        (0xA5, '\u{2022}'), // bullet
        (0xD0, '\u{2013}'), // endash
        (0xD1, '\u{2014}'), // emdash
        (0xD2, '\u{201C}'),
        (0xD3, '\u{201D}'),
        (0xD4, '\u{2018}'),
        (0xD5, '\u{2019}'),
        (0xCA, '\u{00A0}'),
        (0xC9, '\u{2026}'),
        (0xDE, '\u{FB01}'),
        (0xDF, '\u{FB02}'),
    ];
    for &(b, c) in high {
        m.insert(b, c);
    }
    m
}
