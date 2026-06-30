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

use lopdf::{Dictionary, Document, Object};

use crate::pdfium_backend::Glyph;

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
            if let Some(enc) = &self.simple_encoding {
                if let Some(&ch) = enc.get(&(code as u8)) {
                    return (Some(decompose_ligatures(&ch.to_string())), w);
                }
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

    let (ascent, descent) = font_ascent_descent(doc, fdict, two_byte);

    Font {
        two_byte,
        to_unicode,
        widths,
        default_width,
        simple_encoding,
        ascent,
        descent,
        hash: hash_name(name),
    }
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
    let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
    pages.sort_by_key(|(n, _)| *n);
    pages
        .into_iter()
        .map(|(_, pid)| {
            let (w, h) = page_size(&doc, pid);
            let glyphs = page_glyphs(&doc, pid);
            let cells = crate::dp_lines::line_cells(&glyphs, h, true);
            (w, h, cells)
        })
        .collect()
}

/// Extract every glyph on a page as a native-coordinate [`Glyph`].
pub(crate) fn page_glyphs(doc: &Document, page_id: lopdf::ObjectId) -> Vec<Glyph> {
    let mut out = Vec::new();
    let fonts_raw = doc.get_page_fonts(page_id).unwrap_or_default();
    let fonts: HashMap<Vec<u8>, Font> = fonts_raw
        .iter()
        .map(|(name, dict)| (name.clone(), parse_font(doc, name, dict)))
        .collect();
    let Ok(content_bytes) = doc.get_page_content(page_id) else {
        return out;
    };
    let Ok(content) = lopdf::content::Content::decode(&content_bytes) else {
        return out;
    };

    // Graphics + text state. `q`/`Q` save and restore the whole graphics state,
    // which includes the text parameters (Tc, Tw, Tz, TL, Tfs, Trise, font) —
    // *not* the text matrix (that is reset by BT). Saving only the CTM let a Tc
    // set inside a `q…Q` block leak out and drift every later glyph.
    #[allow(clippy::type_complexity)]
    let mut gstate_stack: Vec<(Mat, f64, f64, f64, f64, f64, f64, Option<&Font>)> = Vec::new();
    let mut ctm = Mat::ID;
    let mut tm = Mat::ID;
    let mut tlm = Mat::ID;
    let mut font: Option<&Font> = None;
    let mut fsize = 0.0f64;
    let mut tc = 0.0f64; // char spacing
    let mut tw = 0.0f64; // word spacing
    let mut th = 1.0f64; // horizontal scale (Tz/100)
    let mut tl = 0.0f64; // leading
    let mut trise = 0.0f64;

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
                if let (Some(f), Some(Object::String(s, _))) = (font, operands.last()) {
                    show_text(f, s, fsize, tc, tw, th, trise, &mut tm, ctm, &mut out);
                }
            }
            "TJ" => {
                if let (Some(f), Some(Object::Array(arr))) = (font, operands.first()) {
                    for el in arr {
                        match el {
                            Object::String(s, _) => {
                                show_text(f, s, fsize, tc, tw, th, trise, &mut tm, ctm, &mut out)
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
            _ => {}
        }
    }
    out
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

/// Resolve common Adobe glyph names to Unicode (the subset our corpus uses):
/// `uniXXXX`, `bullet`, `space`, single ASCII-letter names, etc.
fn glyph_name_to_char(name: &[u8]) -> Option<char> {
    let s = std::str::from_utf8(name).ok()?;
    if let Some(hex) = s.strip_prefix("uni") {
        if let Ok(cp) = u32::from_str_radix(hex.get(0..4)?, 16) {
            return char::from_u32(cp);
        }
    }
    Some(match s {
        "space" => ' ',
        "bullet" => '\u{2022}',
        "periodcentered" => '\u{00B7}',
        "hyphen" => '-',
        "endash" => '\u{2013}',
        "emdash" => '\u{2014}',
        "quoteright" => '\u{2019}',
        "quoteleft" => '\u{2018}',
        "quotedblleft" => '\u{201C}',
        "quotedblright" => '\u{201D}',
        "fi" => '\u{FB01}',
        "fl" => '\u{FB02}',
        _ => return None,
    })
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
