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
use std::sync::Arc;

use lopdf::{Dictionary, Document, Object};

use crate::pdfium_backend::Glyph;

/// Per-document caches for the content-stream interpreter. Fonts are indirect
/// objects shared by many pages, but were fully re-parsed — ToUnicode CMap
/// decompression + tokenization, embedded Type1 program scan, width tables —
/// for **every page and every Form XObject invocation**; decoded form content
/// streams were likewise re-inflated on every `Do`. Cached per document,
/// keyed by the referenced object id (fonts also by resource name — the
/// `/Font` entry a page binds the object to). Inline (non-reference) dicts
/// are rare and stay uncached.
#[derive(Default)]
struct DocCaches {
    fonts: HashMap<(lopdf::ObjectId, Vec<u8>), Arc<Font>>,
    forms: HashMap<lopdf::ObjectId, Arc<lopdf::content::Content>>,
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
    /// Weight and slant read from the `/BaseFont` name (the heading
    /// hierarchy's style signal, #302).
    style: crate::font_style::FontStyle,
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

/// The font identity the line sanitizer's `enforce_same_font` compares —
/// docling-parse's `page_font::init_font_name`: `/FontDescriptor/FontName`
/// (the descendant CIDFont's for a Type0), else `/Name`, else `/BaseFont`
/// (its `base_font`), else the literal `"null"`. The resource key (`/F1`,
/// `/F2`) is deliberately **not** part of it: a producer that references one
/// face twice — the same Helvetica under two keys, one re-encoded so a code
/// maps to the `fi` glyph — draws one word across the two resources, and
/// docling-parse reads both as the same font. Keyed by resource name (as
/// this port was until #728) the `fi` ligature, which bridges fonts, glued
/// itself to the preceding run while the font barrier then cut it off from
/// its own word (`fi nancial`); docling's `financial` needs the two
/// resources to compare equal.
fn sanitizer_font_name(doc: &Document, fdict: &Dictionary, two_byte: bool) -> Vec<u8> {
    let descriptor_font_name = |owner: &Dictionary| {
        owner
            .get(b"FontDescriptor")
            .ok()
            .and_then(|o| as_dict(doc, o))
            .and_then(|fd| fd.get(b"FontName").ok())
            .and_then(|o| deref(doc, o))
            .and_then(|o| o.as_name().ok().map(<[u8]>::to_vec))
    };
    if let Some(n) = descriptor_font_name(fdict) {
        return n;
    }
    if two_byte {
        let descendant = fdict
            .get(b"DescendantFonts")
            .ok()
            .and_then(|o| deref(doc, o))
            .and_then(|o| match o {
                Object::Array(a) => a.first(),
                _ => None,
            })
            .and_then(|o| as_dict(doc, o));
        if let Some(n) = descendant.and_then(descriptor_font_name) {
            return n;
        }
    }
    if let Some(n) = fdict
        .get(b"Name")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_name().ok())
    {
        return n.to_vec();
    }
    // docling-parse's `base_font` keeps the subset prefix (`ABCDEF+Helvetica`):
    // two subsets of one face are two fonts there, and so here.
    if let Some(n) = fdict.get(b"BaseFont").ok().and_then(|o| o.as_name().ok()) {
        return n.to_vec();
    }
    b"null".to_vec()
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

    let (mut widths, mut default_width) = if two_byte {
        cid_widths(doc, fdict)
    } else {
        simple_widths(doc, fdict)
    };

    let simple_encoding = if two_byte {
        None
    } else {
        Some(simple_encoding_table(doc, fdict))
    };

    // A standard-14 font referenced without an embedded program usually ships
    // no `/Widths` and no `/FontDescriptor` either (ReportLab's default, #187).
    // Every advance then resolved to 0, the cells collapsed to zero width, and
    // the page's whole text layer was silently dropped — while pdfium, with
    // its built-in metrics, reads the same file fine. Fill the widths from the
    // built-in Adobe Core 14 AFM tables via the font's own code→char decode
    // (base encoding + `/Differences`), so an explicit `/Widths` always wins.
    if !two_byte && widths.is_empty() && default_width == 0.0 {
        if let Some(std14) = base_font_name(fdict).and_then(|n| crate::std14::widths_for(&n)) {
            if let Some(enc) = &simple_encoding {
                for (&code, &ch) in enc {
                    if let Some(w) = std14.width(ch) {
                        widths.insert(u32::from(code), w);
                    }
                }
            }
            // Codes the table misses still advance a typical width instead of
            // stacking at x=0 (the failure mode this whole branch fixes).
            default_width = 500.0;
        }
    }
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
        hash: hash_name(&sanitizer_font_name(doc, fdict, two_byte)),
        style: crate::font_style::parse_font_style(&String::from_utf8_lossy(
            &base_font_name(fdict).unwrap_or_else(|| name.to_vec()),
        )),
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

/// The `/BaseFont` name with any `ABCDEF+` subset prefix stripped (#187).
fn base_font_name(fdict: &Dictionary) -> Option<Vec<u8>> {
    let name = fdict.get(b"BaseFont").ok()?.as_name().ok()?;
    let stripped = match name.iter().position(|&b| b == b'+') {
        Some(i) if i == 6 => &name[i + 1..],
        _ => name,
    };
    Some(stripped.to_vec())
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
pub(crate) fn parse_tounicode(data: &[u8]) -> HashMap<u32, String> {
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

/// A page's display box, in PDF user space: the `/CropBox` clipped to the
/// `/MediaBox`, both inherited through the page tree and normalized — a
/// missing or empty MediaBox is US Letter, an empty CropBox is the MediaBox
/// (pdfium's `CPDF_Page::UpdateDimensions`). pdfium reports the page size from
/// this box, renders exactly it, and translates every content coordinate so
/// its lower-left corner is the origin (`m_PageMatrix`); docling's backends
/// inherit that frame, so text cells, `prov` boxes and destinations all count
/// from the CropBox corner, not the MediaBox one. The parser used to flip
/// glyphs with the MediaBox *height* and no translation at all, so a page
/// whose boxes do not start at (0, 0) — a trimmed book page with
/// `MediaBox [-56 -58 576 723]` / `CropBox [1 -0.6 519 666]`, or a LaTeX
/// figure cropped to `[156 147 637 391]` — had its text displaced against the
/// rendered bitmap by the box offset, the bottom lines pushed past the page
/// edge and clamped to `t = b`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PageBox {
    /// Left edge, user space.
    pub l: f32,
    /// Bottom edge, user space.
    pub b: f32,
    pub w: f32,
    pub h: f32,
}

impl PageBox {
    /// Top edge, user space — the y that becomes `0` in the y-down frame.
    pub fn top(&self) -> f32 {
        self.b + self.h
    }
}

/// A page-tree rect attribute (`/MediaBox`, `/CropBox`), inherited from the
/// nearest ancestor that sets it, as normalized `(l, b, r, t)`.
fn inherited_rect(
    doc: &Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Option<(f32, f32, f32, f32)> {
    let mut id = page_id;
    for _ in 0..32 {
        let dict = doc.get_object(id).ok()?.as_dict().ok()?;
        if let Some(Object::Array(a)) = dict.get(key).ok().and_then(|o| deref(doc, o)) {
            let v: Vec<f32> = a.iter().filter_map(|o| num(o).map(|x| x as f32)).collect();
            if v.len() == 4 && v.iter().all(|x| x.is_finite()) {
                return Some((
                    v[0].min(v[2]),
                    v[1].min(v[3]),
                    v[0].max(v[2]),
                    v[1].max(v[3]),
                ));
            }
            return None;
        }
        id = dict.get(b"Parent").ok()?.as_reference().ok()?;
    }
    None
}

pub(crate) fn page_box(doc: &Document, page_id: lopdf::ObjectId) -> PageBox {
    let nonempty = |r: &(f32, f32, f32, f32)| r.2 > r.0 && r.3 > r.1;
    let media = inherited_rect(doc, page_id, b"MediaBox")
        .filter(nonempty)
        .unwrap_or((0.0, 0.0, 612.0, 792.0));
    let crop = inherited_rect(doc, page_id, b"CropBox")
        .map(|c| {
            (
                c.0.max(media.0),
                c.1.max(media.1),
                c.2.min(media.2),
                c.3.min(media.3),
            )
        })
        .filter(nonempty)
        .unwrap_or(media);
    PageBox {
        l: crop.0,
        b: crop.1,
        w: crop.2 - crop.0,
        h: crop.3 - crop.1,
    }
}

/// Page size (width, height) in PDF points — the display box's, like pdfium's
/// `FPDF_GetPageWidthF/HeightF`.
fn page_size(doc: &Document, page_id: lopdf::ObjectId) -> (f32, f32) {
    let pb = page_box(doc, page_id);
    (pb.w, pb.h)
}

/// Localize where a page's text is lost, for the `text_layer` diagnostic.
/// Extraction can come up empty at three different points — no content stream
/// reached the parser, the stream did not decode into operators, or it ran but
/// produced no glyphs (fonts/encodings) — and from the outside all three look
/// the same. Report them per page.
pub fn content_diagnosis(bytes: &[u8]) -> String {
    let Some(doc) = load_document(bytes) else {
        return "document does not load".into();
    };
    let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
    pages.sort_by_key(|(n, _)| *n);
    let mut out = String::new();
    let mut caches = DocCaches::default();
    for (n, pid) in pages.into_iter().take(4) {
        let content_bytes = doc.get_page_content(pid);
        let ops = lopdf::content::Content::decode(&content_bytes)
            .map(|c| c.operations.len())
            .ok();
        let res = page_res(&doc, pid);
        let fonts = res.map(|r| fonts_from_res(&doc, r, &mut caches).len());
        let glyphs = page_glyphs_cached(&doc, pid, &mut caches).len();
        out.push_str(&format!(
            "\n   page {n}: content {} B, ops {}, resources {}, fonts {}, glyphs {}",
            content_bytes.len(),
            ops.map_or("UNDECODABLE".to_string(), |n| n.to_string()),
            if res.is_some() { "ok" } else { "MISSING" },
            fonts.map_or("-".to_string(), |n| n.to_string()),
            glyphs,
        ));
    }
    out
}

/// Is this "text layer" a vestige rather than the document's text?
///
/// Scanned forms often carry a handful of typed-in strings — a date filled
/// into three form fields, say — on top of pages that are otherwise images.
/// Treating that as a real text layer is the worst of both worlds: the text
/// path proudly extracts thirteen characters, and no OCR ever runs on the
/// letter the pages actually show. The reported form did exactly this (3
/// lines, 13 chars, 3 pages).
///
/// The rule is deliberately tight so genuinely sparse *digital* documents are
/// not misrouted into OCR: only a document averaging at most one line per page
/// **and** totalling fewer than 32 characters is called vestigial.
pub fn text_layer_is_vestigial(pages: &[crate::pdfium_backend::PdfPage]) -> bool {
    let mut tally = TextLayerTally::default();
    pages.iter().for_each(|p| tally.add(p));
    tally.is_vestigial()
}

/// [`text_layer_is_vestigial`]'s counts, accumulated a page at a time so a
/// streaming caller need not keep the pages around to decide.
#[derive(Debug, Default, Clone, Copy)]
pub struct TextLayerTally {
    pages: usize,
    lines: usize,
    chars: usize,
}

impl TextLayerTally {
    pub fn add(&mut self, page: &crate::pdfium_backend::PdfPage) {
        self.pages += 1;
        self.lines += page.cells.len();
        self.chars += page
            .cells
            .iter()
            .map(|c| c.text.chars().count())
            .sum::<usize>();
    }

    /// The verdict over the pages added so far.
    pub fn is_vestigial(&self) -> bool {
        self.lines == 0 || (self.lines <= self.pages && self.chars < 32)
    }

    /// Whether the pages added so far already rule vestigial out for a
    /// document of `total_pages`, whatever the remaining pages hold — both
    /// counts only grow, so a caller may stop parsing pages it does not need.
    pub fn proves_text(&self, total_pages: usize) -> bool {
        self.lines > 0 && (self.lines > total_pages || self.chars >= 32)
    }
}

/// Why the cross-reference repair did or did not fire, for the `text_layer`
/// diagnostic. A PDF that will not load is indistinguishable from a scan in
/// production (both convert to nothing), so the reason has to be askable.
pub fn xref_repair_status(bytes: &[u8]) -> String {
    if Document::load_mem(bytes).is_ok() {
        return "loads unaided; no repair needed".into();
    }
    match pad_short_xref_entries(bytes) {
        Ok(fixed) => match Document::load_mem(&fixed) {
            Ok(_) => "repaired: cross-reference entries padded to 20 bytes".into(),
            Err(e) => format!("padded the entries, but it still will not load: {e}"),
        },
        Err(why) => format!("repair declined — {why}"),
    }
}

/// Load a PDF, repairing the one malformation that otherwise costs us the whole
/// document: **19-byte cross-reference entries**.
///
/// The spec fixes an xref entry at 20 bytes — `nnnnnnnnnn ggggg n` plus a
/// *two*-byte EOL. Some generators (an Austrian telecom's invoices, for one)
/// emit a bare LF instead, making each entry 19 bytes. lopdf rejects the file
/// outright (`invalid file trailer`) where pdfium reads it happily, so a
/// perfectly good text layer looked to the browser exactly like a scan and cost
/// ten seconds of OCR.
///
/// Padding is only attempted when it cannot move anything the xref points at:
/// a single `xref` section that begins after the last object. The repair then
/// has to prove itself — the padded bytes are used only if they load — so a
/// mis-repair degrades to today's behaviour rather than to silent garbage.
pub(crate) fn load_document(bytes: &[u8]) -> Option<Document> {
    open_document(bytes, None).ok()
}

/// Why [`open_document`] could not hand back a readable document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenError {
    /// lopdf cannot read the file even after the repairs.
    Unreadable,
    /// The file is encrypted and the password given (or none) does not open it.
    Password,
}

/// Load `bytes` with the document's password. lopdf decrypts while reading —
/// with `password`, or the empty user password most "protected" PDFs carry
/// (what every viewer opens silently) — so every reader downstream sees plain
/// streams; a file the password does not open loads with its streams still
/// encrypted, which is reported as [`OpenError::Password`] rather than handed
/// on as a document whose every stream decodes to nothing.
pub(crate) fn open_document(bytes: &[u8], password: Option<&str>) -> Result<Document, OpenError> {
    let Some(doc) = load_document_raw(bytes, password) else {
        // lopdf refuses to load at all under a *wrong* password (a missing
        // one loads the file with its streams still encrypted); tell the two
        // apart by loading without it.
        if password.is_some() && load_document_raw(bytes, None).is_some_and(|d| d.is_encrypted()) {
            return Err(OpenError::Password);
        }
        return Err(OpenError::Unreadable);
    };
    if doc.is_encrypted() {
        return Err(OpenError::Password);
    }
    Ok(doc)
}

fn load_options(password: Option<&str>) -> lopdf::LoadOptions {
    lopdf::LoadOptions {
        password: password.map(str::to_string),
        ..lopdf::LoadOptions::default()
    }
}

fn load_document_raw(bytes: &[u8], password: Option<&str>) -> Option<Document> {
    // Try progressively more repair, and accept a candidate only once the pages
    // actually carry content — a document whose streams were dropped still
    // "loads", so loading alone is not evidence the repair helped. A
    // well-formed file returns on the first attempt and pays for nothing.
    let mut fallback = None;
    if let Some(doc) = best_effort_load(bytes, password, &mut fallback) {
        return Some(doc);
    }
    let xref_fixed = pad_short_xref_entries(bytes).ok();
    if let Some(fixed) = &xref_fixed {
        if let Some(doc) = best_effort_load(fixed, password, &mut fallback) {
            return Some(doc);
        }
    }
    // Both defects can coexist, and the second only becomes visible once the
    // first is repaired, so build on whatever the previous step produced.
    let lengths_fixed = fix_stream_lengths(xref_fixed.as_deref().unwrap_or(bytes));
    if let Some(doc) = best_effort_load(&lengths_fixed, password, &mut fallback) {
        return Some(doc);
    }
    fallback
}

/// Load `data`, returning it only when its pages carry content; a document that
/// merely parses is remembered as the fallback for when nothing does better.
fn best_effort_load(
    data: &[u8],
    password: Option<&str>,
    fallback: &mut Option<Document>,
) -> Option<Document> {
    match Document::load_mem_with_options(data, load_options(password)) {
        Ok(doc) if has_page_content(&doc) => Some(doc),
        Ok(doc) => {
            fallback.get_or_insert(doc);
            None
        }
        Err(_) => None,
    }
}

/// Does any page actually hand us a content stream? A document whose streams
/// were dropped still parses — it simply has nothing to read — so this is what
/// tells a successful repair from a pointless one.
fn has_page_content(doc: &Document) -> bool {
    doc.get_pages()
        .into_values()
        .take(4)
        .any(|pid| !doc.get_page_content(pid).is_empty())
}

/// Correct `/Length` values that disagree with where `endstream` actually is.
///
/// The same generator that writes short xref entries also overstates its
/// content-stream lengths by a byte or two. lopdf trusts `/Length`, reads past
/// the data, fails to find `endstream` there and drops the stream — the object
/// comes back as a bare dictionary, so the page has no content at all and the
/// document looks like a scan. pdfium instead trusts `endstream`, which is what
/// this does.
///
/// The rewrite is length-preserving: the corrected number is written over the
/// old digits and padded with spaces, so every byte offset in the file — and
/// therefore the whole cross-reference table — stays valid.
fn fix_stream_lengths(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let mut i = 0;
    while let Some(rel) = find(&out[i..], b"stream") {
        let kw = i + rel;
        i = kw + 6;
        // Skip `endstream` (the keyword we are measuring *to*).
        if kw >= 3 && &out[kw - 3..kw] == b"end" {
            continue;
        }
        // The stream data starts after the EOL that follows the keyword.
        let mut data = kw + 6;
        if out.get(data..data + 2) == Some(b"\r\n".as_slice()) {
            data += 2;
        } else if matches!(out.get(data), Some(b'\n' | b'\r')) {
            data += 1;
        }
        let Some(end) = find(&out[data..], b"endstream").map(|r| data + r) else {
            continue;
        };
        // `/Length <digits>` in the dictionary just before the keyword.
        let dict_start = out[..kw].iter().rposition(|&c| c == b'<').unwrap_or(0);
        let Some(lrel) = find(&out[dict_start..kw], b"/Length") else {
            continue;
        };
        let mut d = dict_start + lrel + 7;
        while matches!(out.get(d), Some(b' ')) {
            d += 1;
        }
        let digits = out[d..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            continue;
        }
        let declared: usize = match std::str::from_utf8(&out[d..d + digits])
            .ok()
            .and_then(|s| s.parse().ok())
        {
            Some(v) => v,
            None => continue,
        };
        let actual = end - data;
        // Only shrink, and only when the new value fits the space the old one
        // occupied — growing the number would move every following byte.
        let replacement = actual.to_string();
        if actual == declared || replacement.len() > digits {
            continue;
        }
        out[d..d + digits].fill(b' ');
        out[d..d + replacement.len()].copy_from_slice(replacement.as_bytes());
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Rewrite a classic cross-reference table's entries to the spec's 20 bytes,
/// or `None` when the file's shape makes that unsafe (see [`load_document`]).
fn pad_short_xref_entries(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    // Exactly one xref section, and it must start after every object, so that
    // growing it shifts nothing the table's offsets refer to.
    let is_boundary = |i: usize| i == 0 || matches!(bytes[i - 1], b'\n' | b'\r');
    let mut starts = (0..bytes.len().saturating_sub(4))
        .filter(|&i| &bytes[i..i + 4] == b"xref" && is_boundary(i));
    let xref_at = starts
        .next()
        .ok_or("no classic `xref` section (an xref stream?)")?;
    if starts.next().is_some() {
        return Err("more than one xref section (incremental update)");
    }
    let last_obj = bytes
        .windows(3)
        .rposition(|w| w == b"obj")
        .ok_or("no objects found")?;
    if last_obj > xref_at {
        return Err("an object follows the xref — padding would move it");
    }

    let mut out = bytes[..xref_at].to_vec();
    out.extend_from_slice(b"xref\n");
    let mut i = xref_at + 4;
    let skip_ws = |i: &mut usize| {
        while matches!(bytes.get(*i), Some(b'\r' | b'\n' | b' ')) {
            *i += 1;
        }
    };
    loop {
        skip_ws(&mut i);
        // Either the next subsection header ("first count") or the trailer.
        if bytes[i..].starts_with(b"trailer") {
            out.extend_from_slice(&bytes[i..]);
            return Ok(out);
        }
        let header_end = i + bytes[i..]
            .iter()
            .position(|c| matches!(c, b'\n' | b'\r'))
            .ok_or("subsection header runs off the end")?;
        let header = std::str::from_utf8(&bytes[i..header_end])
            .map_err(|_| "subsection header is not text")?
            .trim();
        let mut parts = header.split_whitespace();
        let count: usize = parts
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or("unparseable subsection header")?;
        if parts.next().is_some() || count == 0 {
            return Err("unexpected subsection header shape");
        }
        out.extend_from_slice(header.as_bytes());
        out.push(b'\n');
        i = header_end;
        for _ in 0..count {
            skip_ws(&mut i);
            // `nnnnnnnnnn ggggg n` — the 18 bytes before whatever EOL follows.
            let entry = bytes.get(i..i + 18).ok_or("xref entry runs off the end")?;
            let well_formed = entry[..10].iter().all(u8::is_ascii_digit)
                && entry[10] == b' '
                && entry[11..16].iter().all(u8::is_ascii_digit)
                && entry[16] == b' '
                && matches!(entry[17], b'n' | b'f');
            if !well_formed {
                return Err("xref entry is not `nnnnnnnnnn ggggg n`");
            }
            out.extend_from_slice(entry);
            out.extend_from_slice(b" \n"); // the spec's 2-byte EOL -> 20 bytes
            i += 18;
        }
    }
}

/// Debug: raw glyph stream `(ch, ll, lr, lb, lt)` (native coords) for page
/// `index`, before the sanitizer. For comparing char cells to docling-parse.
pub fn debug_glyphs(bytes: &[u8], index: usize) -> Vec<(char, f32, f32, f32, f32)> {
    let Some(doc) = load_document(bytes) else {
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
    let Some(doc) = load_document(bytes) else {
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
    let Some(doc) = load_document(bytes) else {
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
    /// Drawn checkbox squares (#609), in the cells' frame.
    pub checkboxes: Vec<crate::checkbox::CheckBox>,
}

/// The parser text layer, driven one page at a time: the document is loaded
/// (and repaired, see [`load_document`]) once, the font/form caches persist
/// across pages, and each page's glyphs are parsed only when asked for.
///
/// The eager whole-document walk this replaces ran *before* the first page
/// was rendered, so on a long PDF it was a serial prefix the page-worker pool
/// sat idle through — 6.2 s on the 1913-page .NET reference, in front of a
/// pipeline that otherwise overlaps parsing with inference — and a `--pages`
/// window still paid for every page in the file. Pulling pages on demand
/// keeps the parse on the producer thread but interleaved with rendering,
/// and skips unselected pages entirely. Output per page is unchanged: same
/// glyph walk, same shared caches, same contraction.
pub struct PageTextParser {
    doc: Document,
    caches: DocCaches,
    /// Page object ids in document order (page 1 first).
    pages: Vec<lopdf::ObjectId>,
}

impl PageTextParser {
    /// Load the document; `None` when lopdf cannot read it at all.
    pub fn open(bytes: &[u8]) -> Option<Self> {
        Self::open_with_password(bytes, None)
    }

    /// [`open`](Self::open) with the document's password (an encrypted file
    /// the password does not open reads as `None` here; the pipeline reports
    /// it through [`crate::pdf_meta::PdfMeta::open_with_password`] first).
    pub fn open_with_password(bytes: &[u8], password: Option<&str>) -> Option<Self> {
        let doc = open_document(bytes, password).ok()?;
        let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
        pages.sort_by_key(|(n, _)| *n);
        Some(Self {
            doc,
            caches: DocCaches::default(),
            pages: pages.into_iter().map(|(_, pid)| pid).collect(),
        })
    }

    /// The glyph boxes and font styles of the 0-based page `index` — the
    /// heading-hierarchy stage's style signal (#302): every non-space glyph's
    /// box (font ascent + descent at its size — the font-size proxy pdfium's
    /// loose char box also gave) in top-left coordinates, with the weight
    /// class and slant its `/BaseFont` name declares. Empty for a page
    /// without a text layer (a scan), and the stage falls back to its other
    /// signals.
    pub(crate) fn glyph_styles(
        &mut self,
        index: usize,
    ) -> Vec<crate::heading_hierarchy::GlyphStyle> {
        let Some(&pid) = self.pages.get(index) else {
            return Vec::new();
        };
        let (_w, h) = page_size(&self.doc, pid);
        let glyphs = page_glyphs_cached(&self.doc, pid, &mut self.caches);
        // Font hash → style, over the fonts the walk just parsed (an inline,
        // uncached font dictionary reads as unstyled).
        let styles: HashMap<u64, crate::font_style::FontStyle> = self
            .caches
            .fonts
            .values()
            .map(|f| (f.hash, f.style))
            .collect();
        glyphs
            .iter()
            .filter(|g| !g.ch.is_whitespace() && g.ll.is_finite())
            .map(|g| {
                let st = styles.get(&g.font).copied().unwrap_or_default();
                crate::heading_hierarchy::GlyphStyle {
                    l: g.ll,
                    t: h - g.lt,
                    r: g.lr,
                    b: h - g.lb,
                    height: g.height(),
                    weight_cls: crate::font_style::weight_class(st.weight),
                    italic: st.italic,
                    styled: st.known,
                }
            })
            .collect()
    }

    /// Prose, word and code cells of the 0-based page `index` — empty for an
    /// index the parser's page tree doesn't have (a damaged file whose page
    /// tree disagrees with the object model's count).
    pub fn cells(&mut self, index: usize) -> PageParserCells {
        let Some(&pid) = self.pages.get(index) else {
            return PageParserCells::default();
        };
        let (_w, h) = page_size(&self.doc, pid);
        let (glyphs, inks) = page_glyphs_and_inks(&self.doc, pid, &mut self.caches);
        let (prose, words) = crate::dp_lines::line_and_word_cells(&glyphs, h, true);
        PageParserCells {
            prose,
            words,
            code: crate::pdfium_backend::code_cells_from_glyphs(&glyphs, h),
            checkboxes: crate::checkbox::find(&inks, h),
        }
    }

    /// Pages in the document's page tree.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The 0-based page `index` as the text-layer-only conversion sees it —
    /// one element of [`pdf_text_pages`], parsed on demand so a caller can
    /// drop each page once it is assembled instead of holding the whole
    /// document's cells. A blank page (no cells) for an index the page tree
    /// doesn't have.
    pub fn text_page(&mut self, index: usize) -> crate::pdfium_backend::PdfPage {
        let (w, h, (glyphs, inks)) = match self.pages.get(index) {
            Some(&pid) => {
                let (w, h) = page_size(&self.doc, pid);
                (w, h, page_glyphs_and_inks(&self.doc, pid, &mut self.caches))
            }
            None => (0.0, 0.0, Default::default()),
        };
        let (mut prose, mut words) = crate::dp_lines::line_and_word_cells(&glyphs, h, true);
        drop_overpainted_cells(&mut prose);
        drop_overpainted_cells(&mut words);
        crate::pdfium_backend::PdfPage {
            #[cfg(feature = "ocr-prep")]
            image_layout: None,
            width: w,
            height: h,
            // Cells are native PDF points; there is no rendered bitmap.
            scale: 1.0,
            cells: prose,
            code_cells: crate::pdfium_backend::code_cells_from_glyphs(&glyphs, h),
            word_cells: words,
            checkboxes: crate::checkbox::find(&inks, h),
            #[cfg(feature = "ocr-prep")]
            image: image::RgbImage::new(1, 1),
            links: Vec::new(),
            rotation: 0,
            #[cfg(feature = "ml")]
            pending_image: None,
        }
    }
}

/// Full parser text layer: prose + word + code cells per page, glyphs parsed once.
/// `prose`/`words` come from the docling-parse contraction ([`crate::dp_lines`]);
/// `code` splits only at the parser's own space glyphs (monospace keeps its
/// source spacing). The eager form of [`PageTextParser`].
pub fn pdf_all_cells(bytes: &[u8]) -> Vec<PageParserCells> {
    let Some(mut parser) = PageTextParser::open(bytes) else {
        return Vec::new();
    };
    (0..parser.pages.len()).map(|i| parser.cells(i)).collect()
}

/// Whole pages for the text-layer-only conversion ([`crate::convert_text_layer`]):
/// the parser's prose/word/code cells plus page geometry, assembled into
/// [`PdfPage`]s with no rendered image and no link annotations. Everything here
/// is pure Rust (lopdf), so it compiles without the `ml` feature — including on
/// wasm32. A page the parser can't read (no text layer) comes back with empty
/// cells; there is no pdfium fallback on this path.
pub fn pdf_text_pages(bytes: &[u8]) -> Vec<crate::pdfium_backend::PdfPage> {
    let Some(mut parser) = PageTextParser::open(bytes) else {
        return Vec::new();
    };
    (0..parser.page_count())
        .map(|i| parser.text_page(i))
        .collect()
}

/// Drop line cells that are *painted over each other* — glyphs used as artwork.
///
/// Some generators draw their logo with a symbol font: on the reporting
/// invoice, a `TeleLogo` Type1 paints the T-Mobile mark by stacking the glyphs
/// encoded as `"` and `==` on top of one another, and the flat text-layer
/// output opened with that garbage. Nothing in the font metadata gives it away
/// (the *text* fonts in the same file are also flagged symbolic, and the logo
/// font names its glyphs `quotedbl` &c.), but the geometry does: two cells with
/// different text where one lies inside the other on the same line is
/// physically impossible for prose — ink from two words never occupies the
/// same box. Both cells of such a pair are paint, not text.
///
/// Containment (not mere overlap) keeps this narrow: adjacent words touch but
/// never contain each other, and a same-text near-duplicate (double-draw faux
/// bold) is left alone for the sanitizer's usual handling. Applied on the
/// flat/browser path only — the ML pipeline's text layer is byte-pinned by the
/// PDF corpus, and there the layout model already sinks logo marks into
/// `picture` regions.
fn drop_overpainted_cells(cells: &mut Vec<crate::pdfium_backend::TextCell>) {
    let mut paint = vec![false; cells.len()];
    for i in 0..cells.len() {
        for j in 0..cells.len() {
            if i == j || cells[i].text == cells[j].text {
                continue;
            }
            let (a, b) = (&cells[i], &cells[j]);
            // Same line band: the vertical overlap covers most of the shorter.
            let vo = (a.b.min(b.b) - a.t.max(b.t)).max(0.0);
            if vo < 0.6 * (a.b - a.t).min(b.b - b.t) {
                continue;
            }
            // `a` horizontally inside `b` (with a small tolerance).
            let ho = (a.r.min(b.r) - a.l.max(b.l)).max(0.0);
            if ho >= 0.8 * (a.r - a.l) && (a.r - a.l) <= (b.r - b.l) {
                paint[i] = true;
                paint[j] = true;
            }
        }
    }
    let mut keep = paint.iter().map(|p| !p);
    cells.retain(|_| keep.next().unwrap());
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
) -> HashMap<Vec<u8>, Arc<Font>> {
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
                        Arc::clone(f)
                    } else if let Some(fdict) = deref(doc, value).and_then(|o| o.as_dict().ok()) {
                        let f = Arc::new(parse_font(doc, name, fdict));
                        caches.fonts.insert(key, Arc::clone(&f));
                        f
                    } else {
                        continue;
                    }
                }
                _ => {
                    if let Some(fdict) = deref(doc, value).and_then(|o| o.as_dict().ok()) {
                        Arc::new(parse_font(doc, name, fdict))
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
/// [`PageTextParser::glyph_styles`] for the given **1-based** pages of a
/// document, keyed by page number — a separate, on-demand pass (no
/// rendering), so the extraction pipeline stays byte-identical whether or not
/// the heading-hierarchy stage runs. Empty when lopdf cannot open the file.
pub(crate) fn glyph_styles(
    bytes: &[u8],
    pages: &[usize],
) -> HashMap<usize, Vec<crate::heading_hierarchy::GlyphStyle>> {
    let mut out = HashMap::new();
    let Some(mut parser) = PageTextParser::open(bytes) else {
        return out;
    };
    for &page_no in pages {
        if page_no == 0 {
            continue;
        }
        let styles = parser.glyph_styles(page_no - 1);
        if !styles.is_empty() {
            out.insert(page_no, styles);
        }
    }
    out
}

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
    page_glyphs_and_inks(doc, page_id, caches).0
}

/// [`page_glyphs_cached`] plus the small painted path pieces the same walk
/// passed — what [`crate::checkbox::find`] looks for checkbox squares in
/// (#609) — in the glyphs' frame.
fn page_glyphs_and_inks(
    doc: &Document,
    page_id: lopdf::ObjectId,
    caches: &mut DocCaches,
) -> (Vec<Glyph>, Vec<crate::checkbox::Ink>) {
    let mut out = Vec::new();
    let mut inks = Vec::new();
    // lopdf 0.44: get_page_content returns the assembled content-stream bytes
    // directly (an empty Vec when the page has none).
    let content_bytes = doc.get_page_content(page_id);
    let Ok(content) = lopdf::content::Content::decode(&content_bytes) else {
        return (out, inks);
    };
    if let Some(res) = page_res(doc, page_id) {
        // pdfium's page matrix: user space translated so the display box's
        // lower-left corner is the origin (see [`PageBox`]).
        let pb = page_box(doc, page_id);
        let base = Mat {
            e: -(pb.l as f64),
            f: -(pb.b as f64),
            ..Mat::ID
        };
        run_content(
            doc,
            res,
            &content,
            base,
            TextState::INIT,
            0,
            caches,
            &mut out,
            &mut inks,
        );
        out.retain(|g| on_page(g, pb.w, pb.h));
    }
    (out, inks)
}

/// Whether a glyph is on the page (#529): docling-parse keeps a character
/// only when its whole box lies inside the display box — the CropBox, or
/// the MediaBox without one — edges included, so a FrameMaker print slug
/// drawn beside the CropBox or a tiled page's neighbouring text beyond the
/// MediaBox never becomes a cell, and a line crossing the edge is cut at the
/// last glyph that fits (`Crossing the rig`). The box is docling-parse's
/// char box — the advance by the font's ascent/descent, the loose box here
/// (its axis-aligned extent for rotated text) — in the frame `page_glyphs`
/// already moved to the display box's corner, so the page is `[0, w] × [0,
/// h]`. A glyph without a finite box is kept, as before. (A standard-14 font
/// without a FontDescriptor gets the 750 / −250 default ascent / descent
/// here where docling-parse reads the AFM's — Helvetica's 718 / −207 — so
/// for those a glyph within ~0.04 em of the top or bottom edge can fall the
/// other way; horizontally the advance is the same.)
fn on_page(g: &Glyph, w: f32, h: f32) -> bool {
    // f32 noise only: docling-parse already drops a glyph 0.01 pt over.
    const EPS: f32 = 1e-3;
    let (l, b, r, t) = if [g.ll, g.lb, g.lr, g.lt].iter().all(|v| v.is_finite()) {
        (g.ll, g.lb, g.lr, g.lt)
    } else {
        (g.l, g.b, g.r, g.t)
    };
    if ![l, b, r, t].iter().all(|v| v.is_finite()) {
        return true;
    }
    l >= -EPS && b >= -EPS && r <= w + EPS && t <= h + EPS
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
    inks: &mut Vec<crate::checkbox::Ink>,
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
    let mut gstate_stack: Vec<(Mat, f64, f64, f64, f64, f64, f64, Option<&Arc<Font>>)> = Vec::new();
    let mut ctm = base_ctm;
    let mut tm = Mat::ID;
    let mut tlm = Mat::ID;
    let mut font: Option<&Arc<Font>> = None;
    let mut fsize = init.fsize;
    let mut tc = init.tc; // char spacing
    let mut tw = init.tw; // word spacing
    let mut th = init.th; // horizontal scale (Tz/100)
    let mut tl = init.tl; // leading
    let mut trise = init.trise;

    let op_f = |operands: &[Object], i: usize| operands.get(i).and_then(num).unwrap_or(0.0);
    // The path under construction, already in page space (a path is built
    // under one CTM — `cm` is not allowed inside it), for the checkbox
    // squares (#609). Text extraction never reads it.
    let mut path: Vec<PathPiece> = Vec::new();
    let (mut cur, mut start) = ((0.0, 0.0), (0.0, 0.0));
    // Whether the fill colour is (near) white, saved/restored with `q`/`Q`:
    // a white-filled stroked box is a checkbox outline, a coloured one a
    // chart legend's swatch. The initial fill colour is black.
    let mut fill_light = false;
    let mut fill_stack: Vec<bool> = Vec::new();

    for op in &content.operations {
        let operands = &op.operands;
        match op.operator.as_str() {
            "q" => {
                gstate_stack.push((ctm, tc, tw, th, tl, trise, fsize, font));
                fill_stack.push(fill_light);
            }
            "Q" => {
                fill_light = fill_stack.pop().unwrap_or(false);
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
            "m" => {
                cur = ctm.apply(op_f(operands, 0), op_f(operands, 1));
                start = cur;
            }
            "l" => {
                let p = ctm.apply(op_f(operands, 0), op_f(operands, 1));
                path.push(PathPiece::Line(cur, p));
                cur = p;
            }
            "c" | "v" | "y" => {
                let pts: Vec<(f64, f64)> = (0..operands.len() / 2)
                    .map(|k| ctm.apply(op_f(operands, 2 * k), op_f(operands, 2 * k + 1)))
                    .collect();
                if let Some(&end) = pts.last() {
                    path.push(PathPiece::Curve(
                        std::iter::once(cur).chain(pts.iter().copied()).collect(),
                    ));
                    cur = end;
                }
            }
            "h" => {
                if cur != start {
                    path.push(PathPiece::Line(cur, start));
                }
                cur = start;
            }
            "re" => {
                let (x, y, w, h) = (
                    op_f(operands, 0),
                    op_f(operands, 1),
                    op_f(operands, 2),
                    op_f(operands, 3),
                );
                path.push(PathPiece::Rect([
                    ctm.apply(x, y),
                    ctm.apply(x + w, y),
                    ctm.apply(x + w, y + h),
                    ctm.apply(x, y + h),
                ]));
                cur = ctm.apply(x, y);
                start = cur;
            }
            "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" => {
                let op = op.operator.as_str();
                if matches!(op, "s" | "b" | "b*") && cur != start {
                    path.push(PathPiece::Line(cur, start));
                }
                let stroke = matches!(op, "S" | "s" | "B" | "B*" | "b" | "b*");
                let fill = !matches!(op, "S" | "s");
                paint_path(&path, stroke, fill, fill_light, inks);
                path.clear();
            }
            "n" => path.clear(),
            // The fill colour, by its components: gray `g`, `rg`, CMYK `k`,
            // and `sc`/`scn` by operand count (a pattern name is not light).
            "g" | "rg" | "k" | "sc" | "scn" => {
                let v: Vec<f64> = operands
                    .iter()
                    .map(num)
                    .collect::<Option<_>>()
                    .unwrap_or_default();
                fill_light = match v.as_slice() {
                    [g] => *g >= 0.9,
                    [r, g, b] => r.min(*g).min(*b) >= 0.9,
                    [c, m, y, k] => c.max(*m).max(*y).max(*k) <= 0.1,
                    _ => false,
                };
            }
            "cs" => fill_light = false,
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
                        let c = Arc::new(c);
                        if let Some(id) = form_id {
                            caches.forms.insert(id, Arc::clone(&c));
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
                    inks,
                );
            }
            _ => {}
        }
    }
}

/// One piece of a path under construction, page space (see `run_content`).
enum PathPiece {
    Line((f64, f64), (f64, f64)),
    /// An `re`: its four corners, counter-clockwise from `(x, y)`.
    Rect([(f64, f64); 4]),
    /// A Bézier segment: its start and control/end points (their hull).
    Curve(Vec<(f64, f64)>),
}

/// Record a painted path's pieces for the checkbox finder (#609): a stroked
/// line is a [`Seg`](crate::checkbox::Ink::Seg), a stroked rectangle that
/// stays axis-aligned a [`Rect`](crate::checkbox::Ink::Rect) (one filled
/// white too — still an outline), and a coloured fill or a curve only its
/// bounding box, a [`Blot`](crate::checkbox::Ink::Blot): a possible tick
/// mark, or a swatch. White fills draw nothing and are dropped. Only pieces small
/// enough to be a checkbox edge or a mark inside one are kept.
fn paint_path(
    path: &[PathPiece],
    stroke: bool,
    fill: bool,
    fill_light: bool,
    inks: &mut Vec<crate::checkbox::Ink>,
) {
    use crate::checkbox::Ink;
    let bbox = |pts: &mut dyn Iterator<Item = (f64, f64)>| {
        pts.fold(
            (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ),
            |(l, b, r, t), (x, y)| (l.min(x), b.min(y), r.max(x), t.max(y)),
        )
    };
    let mut keep = |ink: Ink| {
        if ink.worth_keeping() {
            inks.push(ink);
        }
    };
    if fill && !fill_light {
        // A coloured fill is a blot — a tick mark, or (the size of a square)
        // a legend swatch, which `checkbox::find` then rules out. A white
        // fill is no ink on paper: it only leaves the stroke, if any, below.
        let (l, b, r, t) = bbox(&mut path.iter().flat_map(|p| match p {
            PathPiece::Line(a, z) => vec![*a, *z],
            PathPiece::Rect(c) => c.to_vec(),
            PathPiece::Curve(c) => c.clone(),
        }));
        if l.is_finite() {
            keep(Ink::Blot { l, b, r, t });
        }
        return;
    }
    if !stroke {
        return;
    }
    for piece in path {
        match piece {
            PathPiece::Line(a, z) => keep(Ink::Seg {
                x0: a.0,
                y0: a.1,
                x1: z.0,
                y1: z.1,
            }),
            PathPiece::Rect(c) => {
                let axis = ((c[0].1 - c[1].1).abs() < 1e-6 && (c[1].0 - c[2].0).abs() < 1e-6)
                    || ((c[0].0 - c[1].0).abs() < 1e-6 && (c[1].1 - c[2].1).abs() < 1e-6);
                if axis {
                    let (l, b, r, t) = bbox(&mut c.iter().copied());
                    keep(Ink::Rect { l, b, r, t });
                } else {
                    for k in 0..4 {
                        let (a, z) = (c[k], c[(k + 1) % 4]);
                        keep(Ink::Seg {
                            x0: a.0,
                            y0: a.1,
                            x1: z.0,
                            y1: z.1,
                        });
                    }
                }
            }
            PathPiece::Curve(c) => {
                let (l, b, r, t) = bbox(&mut c.iter().copied());
                keep(Ink::Blot { l, b, r, t });
            }
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
        let (desc, asc) = (font.descent / 1000.0, font.ascent / 1000.0);
        let (x0, y0) = trm.apply(0.0, desc);
        let (x1, y1) = trm.apply(w0, desc);
        let (x2, y2) = trm.apply(w0, asc);
        let (x3, y3) = trm.apply(0.0, asc);
        // Upright = the baseline runs left-to-right along +x. Only then is the
        // loose box the rectangle spanned by the baseline's x and the em's y
        // (a synthetic oblique's skew is ignored, as it always was). A rotated
        // matrix — the `0 s -s 0 tx ty Tm` landscape pages are built with
        // (#528) — collapsed that rectangle to zero width, and every such run
        // vanished; those glyphs keep their real quad (docling-parse's char
        // rect) and its extent instead.
        let upright = trm.a > 0.0 && trm.b.abs() <= 1e-6 * trm.a;
        let (left, bot, right, top, quad) = if upright {
            (x0.min(x1), y0.min(y3), x0.max(x1), y0.max(y3), None)
        } else {
            let (xs, ys) = ([x0, x1, x2, x3], [y0, y1, y2, y3]);
            let fold = |v: [f64; 4], f: fn(f64, f64) -> f64| v.into_iter().reduce(f).unwrap();
            let q = [x0, y0, x1, y1, x2, y2, x3, y3].map(|v| v as f32);
            (
                fold(xs, f64::min),
                fold(ys, f64::min),
                fold(xs, f64::max),
                fold(ys, f64::max),
                Some(q),
            )
        };
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
                        quad,
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
    } else if base_name.is_empty() {
        // No PDF /Encoding at all: the font's *built-in* encoding applies. For
        // the standard TeX math fonts that is their fixed TeX layout — falling
        // back to StandardEncoding read CMSY's braces as `f`/`g`, `→` as `!`,
        // `∈` as `2` (2203's `{ahn,…}` author line). The font program (often
        // CFF, which this parser does not read) carries the same mapping;
        // docling-parse decodes it from there.
        tex_math_builtin(fdict).unwrap_or_else(winansi_table)
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

/// The fixed built-in encodings of the standard TeX math fonts (TeXbook
/// Appendix F), keyed off the base font name: `CMSY*` (symbols; `CMBSY` is its
/// bold) and `CMMI*` (math italic). These fonts ship no PDF `/Encoding` and no
/// ToUnicode, and their program is usually CFF — without this table the codes
/// fell through to StandardEncoding and rendered as the wrong ASCII.
fn tex_math_builtin(fdict: &Dictionary) -> Option<HashMap<u8, char>> {
    const CMSY: [char; 128] = [
        '−', '·', '×', '∗', '÷', '⋄', '±', '∓', '⊕', '⊖', '⊗', '⊘', '⊙', '◯', '∘', '•', '≍', '≡',
        '⊆', '⊇', '≤', '≥', '≼', '≽', '∼', '≈', '⊂', '⊃', '≪', '≫', '≺', '≻', '←', '→', '↑', '↓',
        '↔', '↗', '↘', '≃', '⇐', '⇒', '⇑', '⇓', '⇔', '↖', '↙', '∝', '′', '∞', '∈', '∋', '△', '▽',
        '\u{338}', '↦', '∀', '∃', '¬', '∅', 'ℜ', 'ℑ', '⊤', '⊥', 'ℵ', 'A', 'B', 'C', 'D', 'E', 'F',
        'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
        'Y', 'Z', '∪', '∩', '⊎', '∧', '∨', '⊢', '⊣', '⌊', '⌋', '⌈', '⌉', '{', '}', '⟨', '⟩', '|',
        '∥', '↕', '⇕', '\\', '≀', '√', '∐', '∇', '∫', '⊔', '⊓', '⊑', '⊒', '§', '†', '‡', '¶', '♣',
        '♢', '♡', '♠',
    ];
    const CMMI: [char; 128] = [
        'Γ', 'Δ', 'Θ', 'Λ', 'Ξ', 'Π', 'Σ', 'Υ', 'Φ', 'Ψ', 'Ω', 'α', 'β', 'γ', 'δ', 'ε', 'ζ', 'η',
        'θ', 'ι', 'κ', 'λ', 'μ', 'ν', 'ξ', 'π', 'ρ', 'σ', 'τ', 'υ', 'φ', 'χ', 'ψ', 'ω', 'ϵ', 'ϑ',
        'ϖ', 'ϱ', 'ς', 'ϕ', '↼', '↽', '⇀', '⇁', '↩', '↪', '▷', '◁', '0', '1', '2', '3', '4', '5',
        '6', '7', '8', '9', '.', ',', '<', '/', '>', '⋆', '∂', 'A', 'B', 'C', 'D', 'E', 'F', 'G',
        'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y',
        'Z', '♭', '♮', '♯', '⌣', '⌢', 'ℓ', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k',
        'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ı', 'ȷ', '℘',
        '\u{20d7}', '⁀',
    ];
    let name = base_font_name(fdict)?;
    let up = name.to_ascii_uppercase();
    let table: &[char; 128] = if up.starts_with(b"CMSY") || up.starts_with(b"CMBSY") {
        &CMSY
    } else if up.starts_with(b"CMMI") {
        &CMMI
    } else {
        return None;
    };
    Some(
        table
            .iter()
            .enumerate()
            .map(|(i, &c)| (i as u8, c))
            .collect(),
    )
}

/// Resolve an Adobe glyph name to Unicode: `uniXXXX`, single ASCII letters, the
/// digit/punctuation names from the Adobe Glyph List, and common typographic
/// names. A `.suffix` (`one.taboldstyle`, `a.sc`) is stripped and the base name
/// retried — docling renders these as the base character.
pub(crate) fn glyph_name_to_char(name: &[u8]) -> Option<char> {
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

/// MacRomanEncoding: ASCII plus the whole high range, 0x80–0xFF, as Mac OS
/// Roman maps it (#744) — the table pdfium, pdftotext and pdf.js decode with.
/// Two codes follow the PDF reference instead of Apple's later revisions:
/// 0xDB is `currency` (¤, not €), and 0xF0 (`apple`, a private-use glyph) is
/// left unmapped. 0xCA, the PDF table's second `space`, reads as a no-break
/// space.
fn macroman_table() -> HashMap<u8, char> {
    #[rustfmt::skip]
    const HIGH: [char; 128] = [
        'Ä', 'Å', 'Ç', 'É', 'Ñ', 'Ö', 'Ü', 'á',  // 0x80
        'à', 'â', 'ä', 'ã', 'å', 'ç', 'é', 'è',  // 0x88
        'ê', 'ë', 'í', 'ì', 'î', 'ï', 'ñ', 'ó',  // 0x90
        'ò', 'ô', 'ö', 'õ', 'ú', 'ù', 'û', 'ü',  // 0x98
        '†', '°', '¢', '£', '§', '•', '¶', 'ß',  // 0xA0
        '®', '©', '™', '´', '¨', '≠', 'Æ', 'Ø',  // 0xA8
        '∞', '±', '≤', '≥', '¥', 'µ', '∂', '∑',  // 0xB0
        '∏', 'π', '∫', 'ª', 'º', 'Ω', 'æ', 'ø',  // 0xB8
        '¿', '¡', '¬', '√', 'ƒ', '≈', '∆', '«',  // 0xC0
        '»', '…', '\u{00A0}', 'À', 'Ã', 'Õ', 'Œ', 'œ',  // 0xC8
        '–', '—', '“', '”', '‘', '’', '÷', '◊',  // 0xD0
        'ÿ', 'Ÿ', '⁄', '¤', '‹', '›', 'ﬁ', 'ﬂ',  // 0xD8
        '‡', '·', '‚', '„', '‰', 'Â', 'Ê', 'Á',  // 0xE0
        'Ë', 'È', 'Í', 'Î', 'Ï', 'Ì', 'Ó', 'Ô',  // 0xE8
        '\0', 'Ò', 'Ú', 'Û', 'Ù', 'ı', 'ˆ', '˜',  // 0xF0
        '¯', '˘', '˙', '˚', '¸', '˝', '˛', 'ˇ',  // 0xF8
    ];
    let mut m = HashMap::new();
    for b in 0x20u8..=0x7e {
        m.insert(b, b as char);
    }
    for (b, &c) in (0x80u8..=0xFF).zip(HIGH.iter()) {
        if c != '\0' {
            m.insert(b, c);
        }
    }
    m
}

#[cfg(test)]
mod page_box_frame {
    use super::*;

    /// One page, `boxes` spliced into the page dictionary verbatim, one text
    /// run at user-space `(x, y)`.
    fn pdf(boxes: &str, x: f32, y: f32) -> Vec<u8> {
        pdf_content(
            boxes,
            &format!("BT /F1 12 Tf {x} {y} Td (First printing) Tj ET\n"),
        )
    }

    /// One page, `boxes` spliced into the page dictionary, `content` as its
    /// content stream (font `/F1` = Helvetica).
    fn pdf_content(boxes: &str, content: &str) -> Vec<u8> {
        let objs: Vec<String> = vec![
            "<</Type/Catalog/Pages 2 0 R>>".into(),
            format!("<</Type/Pages/Kids[3 0 R]/Count 1{boxes}>>"),
            "<</Type/Page/Parent 2 0 R/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>".into(),
            format!("<</Length {}>>stream\n{content}endstream", content.len()),
            "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".into(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj{body}endobj\n", i + 1).as_bytes());
        }
        let xref_at = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes(),
        );
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{xref_at}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn only_page(bytes: &[u8]) -> (PageBox, Vec<Glyph>) {
        let doc = load_document(bytes).expect("loads");
        let pid = *doc.get_pages().values().next().expect("one page");
        (page_box(&doc, pid), page_glyphs(&doc, pid))
    }

    /// The trimmed-book-page shape: MediaBox and CropBox both away from the
    /// origin (inherited from `/Pages`). The display box is the CropBox, and a
    /// glyph's coordinates count from its lower-left corner — the same numbers
    /// the same text gets on a page whose CropBox *is* `[0 0 w h]`.
    #[test]
    fn glyphs_count_from_the_cropbox_corner_like_pdfium() {
        let (pb, shifted) = only_page(&pdf(
            "/MediaBox[-56.505 -58.25 576.303 723.31]/CropBox[1.095 -0.65 518.703 665.71]",
            37.0 + 1.095,
            58.0 - 0.65,
        ));
        assert!((pb.l - 1.095).abs() < 1e-3 && (pb.b + 0.65).abs() < 1e-3);
        assert!(
            (pb.w - 517.608).abs() < 1e-3 && (pb.h - 666.36).abs() < 1e-3,
            "{pb:?}"
        );
        let (pb0, plain) = only_page(&pdf("/MediaBox[0 0 517.608 666.36]", 37.0, 58.0));
        assert!((pb0.w - pb.w).abs() < 1e-3 && (pb0.h - pb.h).abs() < 1e-3);
        assert_eq!(shifted.len(), plain.len());
        assert!(!plain.is_empty());
        for (a, b) in shifted.iter().zip(&plain) {
            assert!(
                (a.l - b.l).abs() < 1e-3 && (a.b - b.b).abs() < 1e-3,
                "{:?} vs {:?}",
                (a.l, a.b),
                (b.l, b.b)
            );
        }
        assert!((plain[0].l - 37.0).abs() < 1e-3, "{}", plain[0].l);
    }

    fn text(glyphs: &[Glyph]) -> String {
        glyphs.iter().map(|g| g.ch).collect()
    }

    /// #529: text drawn outside the display box — a print slug beside the
    /// CropBox, a tiled page's neighbour beyond the MediaBox — is dropped,
    /// glyph by glyph like docling-parse, instead of being clamped onto the
    /// page edge; a line crossing the edge keeps the glyphs that fit.
    #[test]
    fn glyphs_outside_the_display_box_are_dropped() {
        let (_, g) = only_page(&pdf_content(
            "/MediaBox[0 0 400 400]/CropBox[100 100 400 400]",
            "BT /F1 14 Tf 120 300 Td (Visible.) Tj ET\nBT /F1 8 Tf 5 40 Td (Slug) Tj ET\n",
        ));
        assert_eq!(text(&g), "Visible.");
        let (_, g) = only_page(&pdf_content(
            "/MediaBox[0 0 300 400]",
            "BT /F1 14 Tf 40 300 Td (On page) Tj ET\nBT /F1 14 Tf 400 250 Td (Beyond) Tj ET\n",
        ));
        assert_eq!(text(&g), "On page");
        // docling-parse: "Crossing the right edge" on a 300 pt page → "Crossing the rig".
        let (_, g) = only_page(&pdf_content(
            "/MediaBox[0 0 300 400]",
            "BT /F1 14 Tf 200 250 Td (Crossing the right edge) Tj ET\n",
        ));
        assert_eq!(text(&g), "Crossing the rig");
    }

    /// The containment test is docling-parse's: the whole char box (advance
    /// × the font's ascent / descent) inside the display box, edges
    /// included — a Helvetica `W` at 50 pt (47.2 wide) ending exactly on the
    /// right edge stays, 0.01 pt further it goes; likewise on the left.
    #[test]
    fn a_glyph_on_the_edge_stays_one_over_it_goes() {
        let w_at = |x: f32, y: f32| {
            let (_, g) = only_page(&pdf_content(
                "/MediaBox[0 0 300 400]",
                &format!("BT /F1 50 Tf {x} {y} Td (W) Tj ET\n"),
            ));
            text(&g)
        };
        assert_eq!(w_at(252.8, 200.0), "W");
        assert_eq!(w_at(252.81, 200.0), "");
        assert_eq!(w_at(0.0, 200.0), "W");
        assert_eq!(w_at(-0.01, 200.0), "");
        // Vertically the box is the font's ascent / descent: a baseline far
        // enough up keeps the descender on the page, one at 0 does not.
        assert_eq!(w_at(100.0, 20.0), "W");
        assert_eq!(w_at(100.0, 0.0), "");
        assert_eq!(w_at(100.0, 380.0), "");
    }

    /// pdfium's fallbacks: no MediaBox → Letter; a CropBox is clipped to the
    /// MediaBox, and one that misses it entirely is ignored.
    #[test]
    fn page_box_follows_pdfium_fallbacks() {
        let (pb, _) = only_page(&pdf("", 10.0, 10.0));
        assert_eq!((pb.l, pb.b, pb.w, pb.h), (0.0, 0.0, 612.0, 792.0));
        let (pb, _) = only_page(&pdf(
            "/MediaBox[0 0 500 700]/CropBox[-100 100 600 900]",
            10.0,
            10.0,
        ));
        assert_eq!((pb.l, pb.b, pb.w, pb.h), (0.0, 100.0, 500.0, 600.0));
        let (pb, _) = only_page(&pdf(
            "/MediaBox[0 0 500 700]/CropBox[800 800 900 900]",
            10.0,
            10.0,
        ));
        assert_eq!((pb.l, pb.b, pb.w, pb.h), (0.0, 0.0, 500.0, 700.0));
        // Reversed corners normalize.
        let (pb, _) = only_page(&pdf("/MediaBox[500 700 0 0]", 10.0, 10.0));
        assert_eq!((pb.l, pb.b, pb.w, pb.h), (0.0, 0.0, 500.0, 700.0));
    }
}

#[cfg(test)]
mod xref_repair {
    /// Build a tiny one-page PDF whose cross-reference entries are either the
    /// spec's 20 bytes (`two_byte_eol`) or the 19-byte form some generators
    /// emit — everything else about the two files is identical.
    fn pdf_with_xref(two_byte_eol: bool) -> Vec<u8> {
        let content = b"BT /F1 12 Tf 72 700 Td (Invoice 922769430725) Tj ET\n";
        let stream = format!("<</Length {}>>stream\n", content.len()).into_bytes();
        let objs: Vec<Vec<u8>> = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 595 842]/Contents 4 0 R\
               /Resources<</Font<</F1 5 0 R>>>>>>"
                .to_vec(),
            [stream.as_slice(), content.as_slice(), b"endstream"].concat(),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_vec(),
        ];

        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"endobj\n");
        }
        let xref_at = out.len();
        let eol: &[u8] = if two_byte_eol { b" \n" } else { b"\n" };
        out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f");
        out.extend_from_slice(eol);
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n").as_bytes());
            out.extend_from_slice(eol);
        }
        out.extend_from_slice(
            format!("trailer<</Size {}/Root 1 0 R>>\n", objs.len() + 1).as_bytes(),
        );
        out.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());
        out
    }

    /// A 19-byte cross-reference entry (a bare LF where the spec wants a
    /// two-byte EOL) makes lopdf reject the whole file, so a readable text
    /// layer used to look exactly like a scan — in the browser that meant ten
    /// seconds of OCR for nothing. The repair must recover *the same* parse the
    /// well-formed file gives.
    #[test]
    fn short_xref_entries_still_parse() {
        let good = pdf_with_xref(true);
        let broken = pdf_with_xref(false);
        assert!(
            broken.len() < good.len(),
            "the broken file is the shorter one"
        );
        assert!(
            lopdf::Document::load_mem(&good).is_ok(),
            "the control file must load unaided"
        );
        assert!(
            lopdf::Document::load_mem(&broken).is_err(),
            "lopdf rejects 19-byte entries — if this ever passes, drop the repair"
        );

        let cells = |b: &[u8]| -> Vec<String> {
            super::pdf_textlines(b)
                .into_iter()
                .flat_map(|(_, _, c)| c.into_iter().map(|c| c.text))
                .collect()
        };
        let from_good = cells(&good);
        assert!(
            from_good.iter().any(|t| t.contains("922769430725")),
            "control text: {from_good:?}"
        );
        assert_eq!(
            cells(&broken),
            from_good,
            "repair must match the good parse"
        );
    }

    /// The same generator overstates `/Length`, so lopdf reads past the data,
    /// misses `endstream` and drops the stream — the object comes back as a
    /// bare dictionary and the page has no content at all. Trust `endstream`
    /// instead, and do it without moving a single byte.
    #[test]
    fn overstated_stream_length_still_yields_content() {
        let good = pdf_with_xref(true);
        // Inflate the content stream's /Length by one, exactly as the invoice
        // that prompted this does.
        let broken = {
            let at = good
                .windows(8)
                .position(|w| w == b"/Length ")
                .expect("a /Length")
                + 8;
            let digits = good[at..].iter().take_while(|c| c.is_ascii_digit()).count();
            let n: usize = std::str::from_utf8(&good[at..at + digits])
                .unwrap()
                .parse()
                .unwrap();
            let inflated = (n + 1).to_string();
            assert_eq!(inflated.len(), digits, "keep the digit count");
            let mut b = good.clone();
            b[at..at + digits].copy_from_slice(inflated.as_bytes());
            b
        };
        assert_eq!(broken.len(), good.len(), "the defect must not move bytes");
        // lopdf alone loses the stream: the page parses but carries no content.
        let raw = lopdf::Document::load_mem(&broken).expect("still loads");
        assert!(
            raw.get_pages()
                .into_values()
                .all(|p| raw.get_page_content(p).is_empty()),
            "lopdf should drop the stream — if it stops, drop this repair"
        );
        // Ours recovers the same text the well-formed file gives.
        let text = |b: &[u8]| -> Vec<String> {
            super::pdf_textlines(b)
                .into_iter()
                .flat_map(|(_, _, c)| c.into_iter().map(|c| c.text))
                .collect()
        };
        let expected = text(&good);
        assert!(!expected.is_empty(), "control must produce text");
        assert_eq!(text(&broken), expected);
    }

    /// The repair only fires where padding cannot move an object: it declines a
    /// file whose xref precedes an object (an incremental update), rather than
    /// shifting every offset the table records.
    #[test]
    fn repair_declines_when_padding_would_move_objects() {
        let mut incremental = pdf_with_xref(false);
        incremental.extend_from_slice(b"6 0 obj<</Type/Whatever>>endobj\n");
        let declined = super::pad_short_xref_entries(&incremental).unwrap_err();
        assert!(
            declined.contains("object follows the xref"),
            "reason: {declined}"
        );
    }
}

/// #187: standard-14 fonts referenced without an embedded program (and thus
/// usually without `/Widths` or a `/FontDescriptor`) must decode with the
/// built-in Adobe Core 14 metrics instead of collapsing every cell to zero
/// width — the failure mode where a valid text layer was silently dropped
/// while pdfium read the same file fine.
#[cfg(test)]
mod base14_fonts {
    /// A one-page PDF whose single `Tj` uses `fontdict` (no embedded program).
    fn pdf_with_font(fontdict: &[u8], text: &[u8]) -> Vec<u8> {
        let content = [b"BT /F1 12 Tf 72 700 Td (".as_slice(), text, b") Tj ET\n"].concat();
        pdf_with_content(fontdict, &content)
    }

    /// A one-page A4 PDF drawing `content` with `fontdict` as `/F1`.
    fn pdf_with_content(fontdict: &[u8], content: &[u8]) -> Vec<u8> {
        pdf_with_fonts(&[fontdict], content)
    }

    /// A one-page A4 PDF drawing `content` with `fontdicts[i]` bound as
    /// `/F{i+1}` (objects 5, 6, …).
    fn pdf_with_fonts(fontdicts: &[&[u8]], content: &[u8]) -> Vec<u8> {
        let stream = format!("<</Length {}>>stream\n", content.len()).into_bytes();
        let font_res: String = fontdicts
            .iter()
            .enumerate()
            .map(|(i, _)| format!("/F{} {} 0 R", i + 1, i + 5))
            .collect();
        let mut objs: Vec<Vec<u8>> = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            format!(
                "<</Type/Page/Parent 2 0 R/MediaBox[0 0 595 842]/Contents 4 0 R\
                 /Resources<</Font<<{font_res}>>>>>>"
            )
            .into_bytes(),
            [stream.as_slice(), content, b"endstream"].concat(),
        ];
        objs.extend(fontdicts.iter().map(|d| d.to_vec()));
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"endobj\n");
        }
        let xref_at = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer<</Size {}/Root 1 0 R>>\n", objs.len() + 1).as_bytes(),
        );
        out.extend_from_slice(format!("startxref\n{xref_at}\n%%EOF\n").as_bytes());
        out
    }

    /// The parsed cells of the only page.
    fn cells(pdf: &[u8]) -> Vec<crate::pdfium_backend::TextCell> {
        super::pdf_textlines(pdf)
            .into_iter()
            .flat_map(|(_, _, c)| c)
            .collect()
    }

    /// Text drawn with a rotated text matrix reads as one line in its own
    /// reading order, boxed by the run's axis-aligned extent (#528). The
    /// landscape case is `0 s -s 0 tx ty Tm` on a `/Rotate 90` page; every
    /// such run used to collapse to zero width and vanish (90°/270°/tilted)
    /// or read backwards (180°). Expected boxes are docling-parse 7.22's line
    /// cells for the same runs (y-up, PDF points).
    #[test]
    fn rotated_text_matrix_reads_in_order() {
        const HELV: &[u8] = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";
        const TEXT: &str = "Upright text on a rotated page.";
        for (tm, [l, b, r, t]) in [
            ("0 14 -14 0 200 100", [189.9, 100.0, 202.9, 289.1]), // 90°
            ("-14 0 0 -14 350 300", [160.9, 289.9, 350.0, 302.9]), // 180°
            ("0 -14 14 0 200 500", [197.1, 310.9, 210.1, 500.0]), // 270°
            (
                "9.8995 9.8995 -9.8995 9.8995 100 100",
                [92.9, 98.0, 235.8, 240.8],
            ), // 45°
        ] {
            let content = format!("BT /F1 1 Tf {tm} Tm ({TEXT}) Tj ET\n");
            let pdf = pdf_with_content(HELV, content.as_bytes());
            let cs = cells(&pdf);
            assert_eq!(cs.len(), 1, "{tm}: {cs:?}");
            let c = &cs[0];
            assert_eq!(c.text, TEXT, "{tm}");
            // `cells` is top-left-origin on the 842 pt page. The descriptor-
            // less base-14 face gets the parser's 1-em box where docling-parse
            // reads Helvetica's AFM (718/−207) — the same ≤ 0.6 pt
            // across-the-baseline offset upright text has — so allow 1 pt.
            let got = [c.l, 842.0 - c.b, c.r, 842.0 - c.t];
            for (g, w) in got.iter().zip([l, b, r, t]) {
                assert!(
                    (g - w).abs() < 1.0,
                    "{tm}: box {got:?} vs docling-parse {:?}",
                    [l, b, r, t]
                );
            }
            let words: Vec<String> = super::pdf_words(&pdf)
                .into_iter()
                .flat_map(|(_, _, c)| c)
                .map(|c| c.text)
                .collect();
            assert_eq!(words, TEXT.split(' ').collect::<Vec<_>>(), "{tm}");
        }
    }

    /// A rotated run past the page edge (a cover's spine title) is dropped
    /// by the on-page test (#529) — measured on the quad's extent, not the
    /// zero-width box rotated glyphs used to get — instead of clamping onto
    /// the edge as a zero-width cell. One inside the page is kept.
    #[test]
    fn rotated_text_off_the_page_is_dropped() {
        const HELV: &[u8] = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";
        let off = pdf_with_content(
            HELV,
            b"BT /F1 1 Tf 0 14 -14 0 -4 200 Tm (Spine title) Tj ET\n",
        );
        assert!(cells(&off).is_empty(), "{:?}", cells(&off));
        let on = pdf_with_content(
            HELV,
            b"BT /F1 1 Tf 0 14 -14 0 30 200 Tm (Margin note) Tj ET\n",
        );
        assert_eq!(cells(&on).len(), 1);
    }

    /// Upright text keeps the plain loose rectangle — no quad, and the
    /// sanitizer path is the one every pinned PDF baseline was made with.
    #[test]
    fn upright_glyphs_carry_no_quad() {
        let only_page = |pdf: &[u8]| {
            let doc = super::load_document(pdf).expect("loads");
            let pid = *doc.get_pages().values().next().expect("one page");
            super::page_glyphs(&doc, pid)
        };
        let pdf = pdf_with_font(b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>", b"Plain");
        let glyphs = only_page(&pdf);
        assert!(!glyphs.is_empty());
        assert!(glyphs.iter().all(|g| g.quad.is_none() && g.lr > g.ll));
        // A 90° glyph's style height is its font size, not its advance.
        let pdf = pdf_with_content(
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
            b"BT /F1 1 Tf 0 14 -14 0 200 100 Tm (W) Tj ET\n",
        );
        let glyphs = only_page(&pdf);
        let g = &glyphs[0];
        assert!(g.quad.is_some());
        // 14 pt × the face's 1-em box (no descriptor: ascent − descent = 1).
        assert!((g.height() - 14.0).abs() < 0.05, "{}", g.height());
    }

    /// #609: ReportLab's chart axis sets each tick label with the *same* `Tm`
    /// inside its own `q … cm … Q`, so only the CTM stacks `3`…`8` one under
    /// another at one x. Each tick is its own line cell with its own box (as
    /// docling-parse reads it), not one `345678` cell boxed like the first.
    #[test]
    fn ticks_stacked_by_cm_are_separate_line_cells() {
        let mut content = String::new();
        for (k, tick) in "345678".chars().enumerate() {
            let y = 300.0 + 25.92 * k as f64;
            content.push_str(&format!(
                "q 1 0 0 1 125 {y} cm BT /F1 10 Tf 1 0 0 1 -5 -4 Tm ({tick}) Tj ET Q\n"
            ));
        }
        let pdf = pdf_with_content(
            b"<</Type/Font/Subtype/Type1/BaseFont/Times-Roman>>",
            content.as_bytes(),
        );
        let cells = cells(&pdf);
        let texts: Vec<&str> = cells.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["3", "4", "5", "6", "7", "8"]);
        for (k, c) in cells.iter().enumerate() {
            // Top-left y of a tick whose baseline is 4 pt under its cm origin.
            let base = 842.0 - (300.0 + 25.92 * k as f32 - 4.0);
            assert!((c.l - 120.0).abs() < 0.1, "{k}: l {}", c.l);
            assert!(
                c.t < base && base - c.t < 10.0,
                "{k}: t {} base {base}",
                c.t
            );
        }
    }

    /// #609: the path walk finds drawn checkbox squares — ReportLab's four
    /// `m … l S` edges under a `cm`, and a stroked `re` with a tick inside
    /// (checked) — in the text cells' top-left frame, and a stroked box
    /// filled white; a filled bar, a big frame and a colour-filled legend
    /// swatch are not checkboxes.
    #[test]
    fn drawn_checkbox_squares_are_found() {
        let content = b"q 1 0 0 1 119.52 449.04 cm \
            n 0 12.96 m 12.96 12.96 l S n 0 0 m 12.96 0 l S \
            n 0 0 m 0 12.96 l S n 12.96 0 m 12.96 12.96 l S Q\n\
            200 400 10 10 re S 202 405 m 204.5 402 l 208.5 408.5 l S\n\
            300 400 12 12 re f 50 50 100 100 re S\n\
            q 0.2 0.4 0.8 rg 400 400 12 12 re B Q q 1 g 500 400 12 12 re B Q\n";
        let pdf = pdf_with_content(b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>", content);
        let mut parser = super::PageTextParser::open(&pdf).expect("parses");
        let mut boxes = parser.cells(0).checkboxes;
        boxes.sort_by(|a, b| a.l.total_cmp(&b.l));
        let got: Vec<([i32; 4], bool)> = boxes
            .iter()
            .map(|c| {
                let r = |v: f32| (v * 100.0).round() as i32;
                ([r(c.l), r(c.t), r(c.r), r(c.b)], c.checked)
            })
            .collect();
        // Page height 842: y-up 449.04..462.0 → top-left 380.0..392.96.
        assert_eq!(
            got,
            [
                ([11952, 38000, 13248, 39296], false),
                ([20000, 43200, 21000, 44200], true),
                ([50000, 43000, 51200, 44200], false),
            ]
        );
    }

    /// Every standard-14 alias/style decodes with real (positive-width) boxes.
    #[test]
    fn standard14_faces_get_builtin_widths() {
        for fontdict in [
            // ReportLab's default: base-14 Helvetica, WinAnsi, nothing else.
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>".as_slice(),
            // No /Encoding at all (StandardEncoding-ish default).
            b"<</Type/Font/Subtype/Type1/BaseFont/Times-BoldItalic>>",
            // Substitution aliases + a subset prefix.
            b"<</Type/Font/Subtype/TrueType/BaseFont/Arial,Bold>>",
            b"<</Type/Font/Subtype/Type1/BaseFont/ABCDEF+Courier-Oblique>>",
        ] {
            let pdf = pdf_with_font(fontdict, b"Words have width now");
            let cs = cells(&pdf);
            let text: String = cs
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                text.contains("Words have width now"),
                "{}: text lost: {text:?}",
                String::from_utf8_lossy(fontdict)
            );
            assert!(
                cs.iter().all(|c| c.r > c.l),
                "{}: zero-width cells: {cs:?}",
                String::from_utf8_lossy(fontdict)
            );
        }
    }

    /// MacRomanEncoding without a ToUnicode CMap (#744) — Word for Mac's
    /// Quartz output: the whole high range decodes, Danish æ ø å, é, ü, ß
    /// and § included, through the plain encoding name and as the base of a
    /// `/Differences` dictionary alike.
    #[test]
    fn macroman_fonts_keep_accented_letters() {
        let sentence = b"Bl\x8cb\xbergr\xbfd p\x8c \xbfen \xd0 caf\x8e M\x9fller, Stra\xa7e \xa4 3";
        for (fontdict, want) in [
            (
                b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/MacRomanEncoding>>"
                    .as_slice(),
                "Blåbærgrød på øen – café Müller, Straße § 3",
            ),
            (
                b"<</Type/Font/Subtype/TrueType/BaseFont/ABCDEF+Arial\
                   /Encoding<</Type/Encoding/BaseEncoding/MacRomanEncoding/Differences[51/four]>>>>",
                "Blåbærgrød på øen – café Müller, Straße § 4",
            ),
        ] {
            let text: String = cells(&pdf_with_font(fontdict, sentence))
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(text, want, "{}", String::from_utf8_lossy(fontdict));
        }

        // The table itself: all of 0x80–0xFF but the Apple logo, ¤ at 0xDB.
        let table = super::macroman_table();
        assert_eq!(
            (0x80u8..=0xFF).filter(|b| table.contains_key(b)).count(),
            127
        );
        assert_eq!(table.get(&0xF0), None);
        assert_eq!(table[&0xDB], '¤');
        assert_eq!(
            [0x80, 0x8C, 0xAE, 0xAF, 0xBE, 0xBF, 0xFF].map(|b| table[&b]),
            ['Ä', 'å', 'Æ', 'Ø', 'æ', 'ø', 'ˇ']
        );
    }

    /// An explicit `/Widths` array always wins over the built-in metrics, and a
    /// non-standard face without `/Widths` stays as before (no invented boxes).
    #[test]
    fn explicit_widths_win_and_unknown_faces_are_untouched() {
        // Helvetica with explicit 100/1000-em widths: the word's box must be
        // ~4×100 units at 12pt = 4.8pt wide — far narrower than the ~2.7×
        // wider built-in Helvetica advances would make it.
        let explicit = pdf_with_font(
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/FirstChar 65\
               /Widths[100 100 100 100]/Encoding/WinAnsiEncoding>>",
            b"ABBA",
        );
        let builtin = pdf_with_font(
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>",
            b"ABBA",
        );
        let w = |pdf: &[u8]| {
            let cs = cells(pdf);
            assert_eq!(cs.len(), 1, "one word cell: {cs:?}");
            cs[0].r - cs[0].l
        };
        let (we, wb) = (w(&explicit), w(&builtin));
        assert!(
            (we - 4.8).abs() < 0.1,
            "explicit widths must win: got {we}, want 4×100×12/1000"
        );
        assert!(
            wb > 2.0 * we,
            "built-in Helvetica is much wider: {wb} vs {we}"
        );

        // An unknown face with no /Widths: still parses (text kept), but no
        // built-in table applies — the old zero-width behavior is preserved
        // rather than inventing Helvetica metrics for an arbitrary font.
        let unknown = pdf_with_font(
            b"<</Type/Font/Subtype/Type1/BaseFont/FancyCorp-Display>>",
            b"Mystery",
        );
        let cs = cells(&unknown);
        let text: String = cs.iter().map(|c| c.text.as_str()).collect();
        assert!(text.contains("Mystery"), "text still decodes: {cs:?}");
    }
    /// #728: one face bound under two resource keys — `/F1` Helvetica in
    /// WinAnsi, `/F2` the same Helvetica re-encoded so code 0x81 is the `fi`
    /// glyph — with every fi-word drawn in `/F2` right after an `/F1` run.
    /// docling-parse names a font by `/FontDescriptor/FontName` → `/Name` →
    /// `/BaseFont`, never by the resource key, so both resources are the same
    /// font and the line reads `financial`. Keyed by resource key the `fi`
    /// ligature (font-neutral in `applicable_for_merge`) joined the `/F1`
    /// run and the font barrier then split it from `nancial`.
    #[test]
    fn same_face_under_two_resource_keys_is_one_font_for_the_sanitizer() {
        let f1 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        let f2 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding<</Type/Encoding\
                   /BaseEncoding/WinAnsiEncoding/Differences[129/fi]>>>>";
        let content = b"BT 12 TL 72 720 Td \
            /F1 12 Tf (The ) Tj /F2 12 Tf (\\201nancial position) Tj \
            /F1 12 Tf ( of the company is shown in ) Tj /F2 12 Tf (\\201gure 3) Tj \
            /F1 12 Tf ( with the ) Tj /F2 12 Tf (\\201rst results.) Tj \
            T* T* /F2 12 Tf (Same words in /F2 only: \\201nancial, \\201gure, \\201rst.) Tj ET\n";
        let cs = cells(&pdf_with_fonts(&[f1, f2], content));
        let lines: Vec<&str> = cs.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            lines,
            [
                "The financial position of the company is shown in figure 3 with the first results.",
                "Same words in /F2 only: financial, figure, first.",
            ],
            "{cs:?}"
        );
    }

    /// The font barrier itself is untouched: two *different* faces (regular
    /// and bold Helvetica, which docling-parse names apart) set flush against
    /// each other stay separate line cells, as `enforce_same_font` demands.
    #[test]
    fn different_faces_still_keep_the_font_barrier() {
        let f1 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        let f2 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>";
        let content = b"BT 72 720 Td /F2 12 Tf (Total:) Tj /F1 12 Tf (42) Tj ET\n";
        let cs = cells(&pdf_with_fonts(&[f1, f2], content));
        let texts: Vec<&str> = cs.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["Total:", "42"], "{cs:?}");
    }

    /// The identity follows docling-parse's precedence: a `/FontDescriptor/FontName`
    /// wins over `/BaseFont`, so two dictionaries that share a `/BaseFont` but
    /// describe different programs are two fonts, and two that share the
    /// descriptor name are one whatever their `/BaseFont` says.
    #[test]
    fn sanitizer_font_name_follows_docling_parse_precedence() {
        use lopdf::{dictionary, Document, Object};
        let mut doc = Document::with_version("1.4");
        let fd_a =
            doc.add_object(dictionary! { "Type" => "FontDescriptor", "FontName" => "AAAAAA+Face" });
        let fd_b =
            doc.add_object(dictionary! { "Type" => "FontDescriptor", "FontName" => "BBBBBB+Face" });
        let with = |fd: Option<lopdf::ObjectId>, base: &str| {
            let mut d = dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => base };
            if let Some(id) = fd {
                d.set("FontDescriptor", Object::Reference(id));
            }
            d
        };
        let name = |d: &lopdf::Dictionary| {
            String::from_utf8(super::sanitizer_font_name(&doc, d, false)).unwrap()
        };
        assert_eq!(name(&with(Some(fd_a), "Face")), "AAAAAA+Face");
        assert_eq!(name(&with(Some(fd_b), "Face")), "BBBBBB+Face");
        assert_eq!(name(&with(None, "ABCDEF+Face")), "ABCDEF+Face");
        assert_eq!(
            name(&dictionary! { "Type" => "Font", "Name" => "F7" }),
            "F7"
        );
        assert_eq!(name(&dictionary! { "Type" => "Font" }), "null");
    }
}

#[cfg(test)]
mod overpainted {
    use crate::pdfium_backend::TextCell;

    fn cell(text: &str, l: f32, t: f32, r: f32, b: f32) -> TextCell {
        TextCell {
            text: text.into(),
            l,
            t,
            r,
            b,
        }
    }

    /// The reporting invoice's logo: a `"` painted inside a `==` on one band —
    /// artwork drawn with glyphs. Both cells go; the real text on the next
    /// band stays.
    #[test]
    fn stacked_logo_glyphs_are_dropped() {
        let mut cells = vec![
            cell("\"", 72.7, 21.5, 86.4, 31.5),
            cell("==", 59.4, 21.5, 99.6, 31.5),
            cell("Herr", 65.2, 151.3, 81.7, 161.3),
        ];
        super::drop_overpainted_cells(&mut cells);
        assert_eq!(cells.len(), 1, "cells: {cells:?}");
        assert_eq!(cells[0].text, "Herr");
    }

    /// Adjacent words on a line touch but never contain each other — prose is
    /// untouched, and so is a same-text near-duplicate (double-drawn faux
    /// bold), which is not evidence of artwork.
    #[test]
    fn prose_and_double_draw_are_kept() {
        let mut cells = vec![
            cell("Telefon", 354.3, 133.2, 381.5, 143.2),
            cell("0676/2000", 387.3, 133.2, 428.7, 143.2),
            cell("Bold", 100.0, 50.0, 130.0, 60.0),
            cell("Bold", 100.3, 50.0, 130.3, 60.0),
        ];
        super::drop_overpainted_cells(&mut cells);
        assert_eq!(cells.len(), 4);
    }
}

#[cfg(test)]
mod vestigial_layer {
    use crate::pdfium_backend::{PdfPage, TextCell};

    fn page_with(texts: &[&str]) -> PdfPage {
        let cells = texts
            .iter()
            .enumerate()
            .map(|(i, t)| TextCell {
                text: t.to_string(),
                l: 10.0,
                t: 10.0 + 12.0 * i as f32,
                r: 90.0,
                b: 20.0 + 12.0 * i as f32,
            })
            .collect();
        PdfPage::from_cells(595.0, 842.0, 1.0, cells)
    }

    /// The reported scanned form: three typed-in field values ("03", "05",
    /// "2025") over three image pages. That must read as *no usable layer*,
    /// so the browser routes the document to OCR instead of extracting
    /// thirteen characters and skipping the letter entirely.
    #[test]
    fn typed_in_form_fields_are_not_a_text_layer() {
        let pages = vec![
            page_with(&["03", "05", "2025"]),
            page_with(&[]),
            page_with(&[]),
        ];
        assert!(super::text_layer_is_vestigial(&pages));
        assert!(super::text_layer_is_vestigial(&[page_with(&[])]));
    }

    /// A short but genuine digital document — one page, a few real lines —
    /// keeps the fast text path.
    #[test]
    fn sparse_but_real_documents_pass() {
        let one_pager = vec![page_with(&[
            "Confidential briefing",
            "Prepared for the board meeting",
            "Do not distribute",
        ])];
        assert!(!super::text_layer_is_vestigial(&one_pager));
    }
}
