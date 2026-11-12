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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lopdf::{Dictionary, Document, Object};

use crate::pdfium_backend::Glyph;
use crate::tounicode::parse_tounicode;

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
    /// Forms decoded once and not (yet) cached (#994). A form enters
    /// [`Self::forms`] on its *second* sighting: a shared header/footer form
    /// is decoded twice and then hits the cache on every later page, while
    /// a generator's one-form-per-page body (each page's chart in its own
    /// form, used once) is dropped with its page instead of every decoded
    /// `Content` — many times its stream's size — living until the document
    /// ends. Caching on first use made peak RSS grow ~45 MB per page on such
    /// files (7 GiB at 60 pages of the issue's repro, flat ~0.6 GiB now).
    /// Only ids are kept here, so the set itself stays tiny.
    forms_seen: HashSet<lopdf::ObjectId>,
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
    /// For codes ToUnicode lacks (ISO 32000-1, 9.10.2).
    cid_fallback: CidFallback,
    /// code → glyph advance, in 1000-unit glyph space.
    widths: HashMap<u32, f64>,
    default_width: f64,
    /// 1-byte fallback decoding when ToUnicode lacks a code (WinAnsi-ish).
    simple_encoding: Option<HashMap<u8, char>>,
    /// code → raw `/Differences` glyph name, for placeholder names (`g115`,
    /// `SM590000`) that have no Unicode mapping; they override the base
    /// encoding (see [`unknown_name_text`]).
    fallback_names: HashMap<u8, String>,
    /// The unmapped codes whose embedded glyph paints nothing — semantic
    /// spaces (docling-parse 7.24's blank glyph programs: the redp5110
    /// list's `/g3`). Every other unmapped code reads U+FFFD.
    blank_codes: HashSet<u32>,
    /// code → char from the embedded Type1 font program's own `/Encoding` vector,
    /// used only as a last resort for glyphs the base encoding leaves unmapped
    /// (standard TeX math fonts: `λ`, `≤`, …).
    program_encoding: HashMap<u8, char>,
    /// A symbolic simple font without a declared base encoding: the text of
    /// each code its embedded program's builtin encoding resolves
    /// (docling-parse 7.25, #1113 — 2203.01017's `Dingbats` check marks are
    /// `a19` / `a23`, `✓` / `✗`, not the `3` / `7` the Latin tables read).
    /// It outranks every encoding table but a `/Differences` entry
    /// ([`Font::diff_codes`]); empty for every other font and in a build
    /// without the renderer.
    builtin: HashMap<u8, String>,
    /// The codes `/Differences` names.
    diff_codes: HashSet<u8>,
    ascent: f64,
    descent: f64,
    /// code → its embedded glyph's ink extent `(y_min, y_max)` in 1000-unit
    /// em, for a font whose descriptor metrics docling-parse rejects
    /// ([`font_ascent_descent`]'s third value): docling-parse 7.24 boxes such
    /// a font's glyphs by their outline's control box, so a glyph drawn
    /// under the baseline — 2305.03393's `_` in `PMC5406406_003_01.png` —
    /// is a cell of its own, not part of the word. Empty for every other
    /// font, a glyph with no ink, and a build without the renderer.
    ink_y: HashMap<u32, (f64, f64)>,
    hash: u64,
    /// Whether the font's docling-parse name marks a math face
    /// ([`is_math_font_name`]) — for the word contraction's local
    /// attachments (#921).
    math: bool,
    /// Weight and slant read from the `/BaseFont` name (the heading
    /// hierarchy's style signal, #302).
    style: crate::font_style::FontStyle,
}

impl Font {
    /// The text of `code` and its advance; the flag marks a blank unmapped
    /// glyph, whose box has no height.
    ///
    /// A code no source maps to Unicode is visibly painted but carries no
    /// text: docling-parse 7.24 emits U+FFFD for it, and docling turns a NUL
    /// into U+FFFD too (docling#5070), so the loss stays visible and counted
    /// by the text-quality check instead of silently dropping the glyph
    /// (`mL⁻¹` reading `mL1`). A glyph whose embedded program paints nothing
    /// is a space — a word boundary — instead.
    fn decode_code(&self, code: u32) -> (Option<String>, f64, bool) {
        let w = self
            .widths
            .get(&code)
            .copied()
            .unwrap_or(self.default_width);
        if let Some(s) = self.to_unicode.get(&code) {
            return (Some(decompose_ligatures(s)), w, false);
        }
        let unmapped = |font: &Font| {
            let blank = font.blank_codes.contains(&code);
            (
                Some(if blank { " " } else { "\u{FFFD}" }.to_string()),
                w,
                blank,
            )
        };
        if !self.two_byte {
            // A placeholder `/Differences` name (no Unicode) overrides the
            // base encoding. docling-parse 7.24 reads a bare glyph index
            // (`g115`, `gid00043`) as unmapped and any other unknown name as
            // its text (`SM590000`, the redp5110 bullets).
            if let Some(name) = self.fallback_names.get(&(code as u8)) {
                return match unknown_name_text(name) {
                    Some(text) => (Some(text), w, false),
                    None => unmapped(self),
                };
            }
            // docling-parse 7.25's `get_correct_character`: after
            // `/Differences` and `/ToUnicode`, a symbolic font's builtin
            // encoding — "the producer's own statement of what each code
            // draws" — before any base-font or Latin table.
            if !self.diff_codes.contains(&(code as u8)) {
                if let Some(text) = self.builtin.get(&(code as u8)) {
                    return (Some(decompose_ligatures(text)), w, false);
                }
            }
            if let Some(enc) = &self.simple_encoding {
                if let Some(&ch) = enc.get(&(code as u8)) {
                    return (Some(decompose_ligatures(&ch.to_string())), w, false);
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
                return (Some(decompose_ligatures(&ch.to_string())), w, false);
            }
        } else {
            match self.cid_fallback {
                CidFallback::None => {}
                CidFallback::UnicodeCodes { .. } => {
                    if let Some(ch) = unicode_cmap_char(code).filter(|c| !c.is_control()) {
                        return (Some(ch.to_string()), w, false);
                    }
                }
                CidFallback::Collection(table) => {
                    if let Some(s) = collection_text(table, code) {
                        return (Some(decompose_ligatures(&s)), w, false);
                    }
                }
            }
        }
        unmapped(self)
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
                // U+FB05 is LATIN SMALL LIGATURE LONG S T (`ſt`): `st`, as
                // docling's ligature map and Unicode's decomposition (#1001).
                '\u{FB05}' => "st",
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
/// docling-parse's `has_math_positioning_semantics`, the font-name half: a
/// TeX math or symbol face (`cmmi`, `cmsy`, `cmex`, `cmr`, `cmbx`, `msam`,
/// `msbm`) or any name saying `math`, `symbol` or `mtextra`.
fn is_math_font_name(name: &[u8]) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "cmmi", "cmsy", "cmex", "cmr", "cmbx", "msam", "msbm", "math", "symbol", "mtextra",
    ]
    .iter()
    .any(|t| lower.windows(t.len()).any(|w| w == t.as_bytes()))
}

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

    let mut to_unicode = fdict
        .get(b"ToUnicode")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_stream().ok())
        .and_then(|s| s.decompressed_content().ok())
        .map(|data| parse_tounicode(&data))
        .unwrap_or_default();
    if !two_byte {
        if let Some(diffs) = differences_array(doc, fdict) {
            reconcile_stale_tounicode(&mut to_unicode, &diffs);
        }
    }
    let sanitizer_name = sanitizer_font_name(doc, fdict, two_byte);
    // Symbol / ZapfDingbats mapped into the private-use "symbol charset"
    // (`U+F000` + byte): docling-parse 7.25 reads the glyph's text through
    // the face's own encoding instead (docling-parse#431, #1113 — redp5110's
    // `U+F0AE` is `→`, a Word bullet `U+F0B7` is `•`).
    let raw_base = fdict.get(b"BaseFont").ok().and_then(|o| o.as_name().ok());
    let names: Vec<&[u8]> = std::iter::once(sanitizer_name.as_slice())
        .chain(raw_base)
        .collect();
    if let Some(face) = crate::symbol_fonts::symbol_face(&names) {
        for value in to_unicode.values_mut() {
            if let Some(ch) = crate::symbol_fonts::fold_private_use(face, value) {
                *value = ch.to_string();
            }
        }
    }

    let cid_fallback = if two_byte {
        cid_fallback(doc, fdict)
    } else {
        CidFallback::None
    };

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

    let (ascent, descent, ink_boxes) = font_ascent_descent(doc, fdict, two_byte);
    let mut font = Font {
        two_byte,
        to_unicode,
        cid_fallback,
        widths,
        default_width,
        simple_encoding,
        fallback_names,
        blank_codes: HashSet::new(),
        program_encoding,
        builtin: HashMap::new(),
        diff_codes: differences_codes(doc, fdict),
        ascent,
        descent,
        ink_y: HashMap::new(),
        hash: hash_name(&sanitizer_name),
        math: is_math_font_name(&sanitizer_name),
        style: crate::font_style::parse_font_style(&String::from_utf8_lossy(
            &base_font_name(fdict).unwrap_or_else(|| name.to_vec()),
        )),
    };
    font.blank_codes = blank_codes(doc, fdict, &font);
    if !two_byte && subtype != b"Type3".as_slice() && uses_builtin_encoding(doc, fdict) {
        font.builtin = builtin_encoding(doc, fdict, &font);
    }
    if ink_boxes {
        font.ink_y = ink_extents(doc, fdict, &font);
    }
    font
}

/// docling-parse 7.25's `uses_builtin_encoding` for a simple, non-Type 3
/// font: the descriptor flags it symbolic (bit 3) and the PDF declares no
/// base encoding — no `/Encoding` name, no `/BaseEncoding` in an `/Encoding`
/// dictionary (a bare `/Differences` overrides only the codes it lists,
/// 9.6.6.1).
fn uses_builtin_encoding(doc: &Document, fdict: &Dictionary) -> bool {
    let symbolic = fdict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|o| as_dict(doc, o))
        .and_then(|fd| fd.get(b"Flags").ok())
        .and_then(|o| deref(doc, o))
        .and_then(num)
        .is_some_and(|f| (f as i64) & 4 != 0);
    let declared_base = match fdict.get(b"Encoding").ok().and_then(|o| deref(doc, o)) {
        Some(Object::Name(_)) => true,
        Some(Object::Dictionary(d)) => d.get(b"BaseEncoding").is_ok(),
        _ => false,
    };
    symbolic && !declared_base
}

/// The codes a simple font's `/Differences` names.
fn differences_codes(doc: &Document, fdict: &Dictionary) -> HashSet<u8> {
    let mut codes = HashSet::new();
    let mut code = 0i64;
    for el in differences_array(doc, fdict).unwrap_or_default() {
        match el {
            Object::Integer(i) => code = i,
            Object::Real(r) => code = r as i64,
            Object::Name(_) => {
                if (0..=255).contains(&code) {
                    codes.insert(code as u8);
                }
                code += 1;
            }
            _ => {}
        }
    }
    codes
}

/// [`Font::builtin`]: read from the embedded program only when a declared
/// code is left to it — not covered by `/ToUnicode` or `/Differences`
/// (docling-parse reads the program lazily, on the first code that needs it).
#[cfg(feature = "render")]
fn builtin_encoding(doc: &Document, fdict: &Dictionary, font: &Font) -> HashMap<u8, String> {
    let mut codes: Vec<u32> = font.widths.keys().copied().collect();
    if codes.is_empty() {
        codes.extend(0..=255);
    }
    let needed = codes.iter().any(|&c| {
        c <= 255 && !font.to_unicode.contains_key(&c) && !font.diff_codes.contains(&(c as u8))
    });
    if !needed {
        return HashMap::new();
    }
    crate::render::font::LoadedFont::load(doc, fdict).builtin_encoding_text()
}

/// Without the renderer there is no program to read: the font keeps the
/// encoding tables' reading.
#[cfg(not(feature = "render"))]
fn builtin_encoding(_doc: &Document, _fdict: &Dictionary, _font: &Font) -> HashMap<u8, String> {
    HashMap::new()
}

/// The ink extents of `font`'s declared codes ([`Font::ink_y`]), read from
/// its embedded program; nothing past a CID font's first 4096 codes.
#[cfg(feature = "render")]
fn ink_extents(doc: &Document, fdict: &Dictionary, font: &Font) -> HashMap<u32, (f64, f64)> {
    const MAX_CODES: usize = 4096;
    let loaded = crate::render::font::LoadedFont::load(doc, fdict);
    if !loaded.has_program() {
        return HashMap::new();
    }
    let mut codes: Vec<u32> = font.widths.keys().copied().collect();
    if !font.two_byte {
        codes.extend(0..=255);
        codes.sort_unstable();
        codes.dedup();
    }
    codes
        .into_iter()
        .take(MAX_CODES)
        .filter_map(|c| Some((c, loaded.glyph_ink_y(c)??)))
        .collect()
}

/// Without the renderer there is no program to read: the font keeps its
/// loose box (docling-parse's own fallback when FreeType resolves no glyph).
#[cfg(not(feature = "render"))]
fn ink_extents(_doc: &Document, _fdict: &Dictionary, _font: &Font) -> HashMap<u32, (f64, f64)> {
    HashMap::new()
}

/// The codes `font` leaves unmapped whose embedded glyph paints nothing
/// ([`Font::blank_codes`]). The program is loaded only when the font has
/// unmapped codes among those it declares widths for; without the
/// renderer (the wasm text-layer build) there is no program to ask, and
/// every unmapped glyph reads U+FFFD.
#[cfg(feature = "render")]
fn blank_codes(doc: &Document, fdict: &Dictionary, font: &Font) -> HashSet<u32> {
    // A CID font can declare tens of thousands of widths: past this many
    // unmapped codes nothing is classified (the glyphs read U+FFFD).
    const MAX_CANDIDATES: usize = 4096;
    let unmapped: Vec<u32> = font
        .widths
        .keys()
        .copied()
        .filter(|&c| font.decode_code(c).0.as_deref() == Some("\u{FFFD}"))
        .take(MAX_CANDIDATES + 1)
        .collect();
    if unmapped.is_empty() || unmapped.len() > MAX_CANDIDATES {
        return HashSet::new();
    }
    let loaded = crate::render::font::LoadedFont::load(doc, fdict);
    unmapped
        .into_iter()
        .filter(|&c| loaded.glyph_has_ink(c) == Some(false))
        .collect()
}

#[cfg(not(feature = "render"))]
fn blank_codes(_doc: &Document, _fdict: &Dictionary, _font: &Font) -> HashSet<u32> {
    HashSet::new()
}

#[derive(Clone, Copy)]
enum CidFallback {
    None,
    UnicodeCodes { utf16: bool },
    Collection(&'static [u8]),
}

/// A Unicode CMap code retains its original bytes for ToUnicode lookups;
/// a four-byte UTF-16 code is a packed surrogate pair, not a scalar value.
pub(crate) fn unicode_cmap_char(code: u32) -> Option<char> {
    if code <= 0xffff {
        char::from_u32(code)
    } else {
        let units = [(code >> 16) as u16, code as u16];
        let mut chars = char::decode_utf16(units);
        let ch = chars.next()?.ok()?;
        chars.next().is_none().then_some(ch)
    }
}

#[cfg(not(feature = "cid-collections"))]
fn collection_table(_ordering: &[u8]) -> Option<&'static [u8]> {
    None
}

#[cfg(feature = "cid-collections")]
fn collection_table(ordering: &[u8]) -> Option<&'static [u8]> {
    macro_rules! table {
        ($file:literal) => {
            include_bytes!(concat!(env!("OUT_DIR"), "/", $file))
        };
    }
    Some(match ordering {
        b"Japan1" => table!("Adobe-Japan1-UCS2"),
        b"GB1" => table!("Adobe-GB1-UCS2"),
        b"CNS1" => table!("Adobe-CNS1-UCS2"),
        b"Korea1" => table!("Adobe-Korea1-UCS2"),
        b"KR" => table!("Adobe-KR-UCS2"),
        _ => return None,
    })
}

/// The text of `cid` in a table build.rs wrote: the CID count, then three
/// bytes per CID (0 for none, a code point, or 0x110000 + the offset of a
/// length-prefixed UTF-8 string after the entries).
pub(crate) fn collection_text(table: &[u8], cid: u32) -> Option<String> {
    let n = u32::from_le_bytes(table.get(..4)?.try_into().ok()?) as usize;
    let at = 4 + 3 * usize::try_from(cid).ok().filter(|&c| c < n)?;
    let &[a, b, c] = table.get(at..at + 3)? else {
        return None;
    };
    match u32::from_be_bytes([0, a, b, c]) {
        0 => None,
        v @ ..0x110000 => char::from_u32(v).map(String::from),
        v => {
            let at = 4 + 3 * n + (v - 0x110000) as usize;
            let len = *table.get(at)? as usize;
            std::str::from_utf8(table.get(at + 1..at + 1 + len)?)
                .ok()
                .map(str::to_owned)
        }
    }
}

fn cid_fallback(doc: &Document, fdict: &Dictionary) -> CidFallback {
    let encoding = fdict
        .get(b"Encoding")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_name().ok())
        .unwrap_or(&[]);
    if encoding.starts_with(b"Uni")
        && (encoding.windows(4).any(|w| w == b"UCS2") || encoding.windows(5).any(|w| w == b"UTF16"))
    {
        return CidFallback::UnicodeCodes {
            utf16: encoding.windows(5).any(|w| w == b"UTF16"),
        };
    }
    if encoding != b"Identity-H" && encoding != b"Identity-V" {
        // Other CMaps need code → CID tables that are not bundled.
        return CidFallback::None;
    }
    cid_collection(doc, fdict).map_or(CidFallback::None, CidFallback::Collection)
}

/// The CID → Unicode table of the character collection a Type0 font uses.
pub(crate) fn cid_collection(doc: &Document, fdict: &Dictionary) -> Option<&'static [u8]> {
    let Object::Array(descendants) = deref(doc, fdict.get(b"DescendantFonts").ok()?)? else {
        return None;
    };
    let descendant = as_dict(doc, descendants.first()?)?;
    let info = as_dict(doc, descendant.get(b"CIDSystemInfo").ok()?)?;
    let registry = deref(doc, info.get(b"Registry").ok()?)?.as_str().ok()?;
    let ordering = deref(doc, info.get(b"Ordering").ok()?)?.as_str().ok()?;
    (registry == b"Adobe").then(|| collection_table(ordering))?
}

/// A simple font's `/Encoding` `/Differences` array, dereferenced.
fn differences_array(doc: &Document, fdict: &Dictionary) -> Option<Vec<Object>> {
    let enc = fdict.get(b"Encoding").ok().and_then(|o| deref(doc, o))?;
    let diffs = enc.as_dict().ok()?.get(b"Differences").ok()?;
    match deref(doc, diffs)? {
        Object::Array(a) => Some(a.clone()),
        _ => None,
    }
}

/// A ToUnicode CMap copied from the *original* font while the glyphs were
/// re-encoded: the encrypted `2206.01062_pg3` fixture (a Quartz re-save of
/// pdfTeX output) keeps the T1-encoding CMap, keyed on the old codes, next
/// to a `/Differences [33 /f_f_i /f_i /f_f /f_l]` that names the ligatures at
/// new ones, so the CMap maps code 33 to `!` — its own identity — and
/// `difficulties` read `di ! culties`, in docling's pdfium chain too. A
/// ToUnicode entry that is the code's identity, for a code whose
/// `/Differences` name resolves to something else, is that stale copy: the
/// name wins. A CMap that disagrees any other way is left alone — ToUnicode
/// is the producer's word on what the glyph means.
fn reconcile_stale_tounicode(to_unicode: &mut HashMap<u32, String>, diffs: &[Object]) {
    let mut code = 0u8;
    for el in diffs {
        match el {
            Object::Integer(i) => code = *i as u8,
            Object::Name(name) => {
                if let Some(ch) = glyph_name_to_char(name) {
                    let identity = (code as char).to_string();
                    if ch.to_string() != identity
                        && to_unicode.get(&u32::from(code)) == Some(&identity)
                    {
                        to_unicode.insert(u32::from(code), ch.to_string());
                    }
                }
                code = code.wrapping_add(1);
            }
            _ => {}
        }
    }
}

/// Collect `/Differences` entries whose glyph name is a GID placeholder
/// (`g115`, `cid42`, `glyph7`, `index9`, `SM590000`) with no Unicode mapping
/// (subsetted fonts with no ToUnicode); [`unknown_name_text`] decides what
/// each reads as. The placeholder-name restriction keeps real Adobe glyph
/// names on the normal path so this never invents garbage on the clean files.
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

/// The reading text docling-parse 7.24 keeps for a `/Differences` glyph name
/// no table resolves (`resolve_unknown_name`): the name without any
/// `.suffix` — `None` when that is a bare glyph index
/// (`(gid|glyph|g|cid|index)\d+`, any case), which names a glyph of the
/// font program but carries no text (docling-parse#238), so it is unmapped.
fn unknown_name_text(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    (!is_index_name(stem)).then(|| stem.to_string())
}

/// docling-parse's index-style glyph name, `(gid|glyph|g|cid|index)` then
/// hexadecimal digits, case-insensitive (`re_gid`): `/gid00043`, `/g43`,
/// `/cid43`, `/index43` from the subset generators, and since docling-parse
/// 7.25 (#389 in docling-parse#431, #1113) Skia/PDF's `g%X` — `/g15C`,
/// `/gA1` from Chrome's print-to-PDF and Notion exports, which read as
/// their base encoding's letters before. The name points into the font
/// program and carries no text.
fn is_index_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["gid", "glyph", "g", "cid", "index"].iter().any(|p| {
        lower
            .strip_prefix(p)
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_hexdigit()))
    })
}

/// A glyph name that is a synthetic placeholder, not a real Adobe name:
/// `g115`, `cid42`, `glyph7`, `index9`, `G12`, or a short-prefix code name like
/// `SM590000` (IBM BookMaster). These carry no Unicode meaning (docling-parse
/// keeps the code-like names as text, `SM590000`, and reads the glyph indexes
/// as unmapped, see [`unknown_name_text`]). `afii####` / `uni####` are real Adobe names
/// and excluded. The restriction keeps genuine glyph names on the Unicode path.
fn is_gid_name(name: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(name) else {
        return false;
    };
    if s.starts_with("afii") || s.starts_with("uni") {
        return false;
    }
    if is_index_name(s) {
        return true;
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

/// The font's ascent and descent (1000-unit em) for its loose char boxes,
/// and whether docling-parse 7.24 rejects its descriptor metrics
/// (`invalid_vertical_metrics`, `init_ascent_and_descent`) — then it boxes
/// each glyph by its embedded outline instead ([`Font::ink_y`]):
///
/// - an `/Ascent` or `/Descent` beyond ±8000 (`MAX_PLAUSIBLE_VERTICAL_METRIC`)
///   is ignored for the `/FontBBox` edge, when that is usable;
/// - metrics that do not straddle the baseline (`descent <= 0 <= ascent`,
///   `descent < ascent` — `/Ascent 0 /Descent 0`, or a math font's two
///   positive values) give way to a valid `/FontBBox`.
fn font_ascent_descent(doc: &Document, fdict: &Dictionary, two_byte: bool) -> (f64, f64, bool) {
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
    let read = |key: &[u8]| fd.and_then(|d| d.get(key).ok()).and_then(num);
    let plausible = |v: f64| v.is_finite() && v.abs() <= 8000.0;
    let bbox = fd
        .and_then(|d| d.get(b"FontBBox").ok())
        .and_then(|o| deref(doc, o))
        .and_then(|o| match o {
            Object::Array(a) if a.len() == 4 => {
                let v: Vec<f64> = a.iter().filter_map(num).collect();
                (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
            }
            _ => None,
        });
    let usable = |v: f64| v.abs() > 1e-3 && plausible(v);
    let mut invalid = false;
    let asc = match read(b"Ascent") {
        Some(v) if plausible(v) => v,
        Some(_) => {
            invalid = true;
            bbox.map(|b| b[3]).filter(|&v| usable(v)).unwrap_or(750.0)
        }
        None => 750.0,
    };
    let desc = match read(b"Descent") {
        Some(v) if plausible(v) => v,
        Some(_) => {
            invalid = true;
            bbox.map(|b| b[1]).filter(|&v| usable(v)).unwrap_or(-250.0)
        }
        None => -250.0,
    };
    if !(desc <= 0.0 && asc >= 0.0 && desc < asc) {
        if let Some(b) = bbox.filter(|b| {
            b[1] <= 0.0 && b[3] >= 0.0 && b[1] < b[3] && plausible(b[1]) && plausible(b[3])
        }) {
            return (b[3], b[1], true);
        }
    }
    // Some subsetted fonts carry a degenerate FontDescriptor (`/Ascent 0
    // /Descent 0`) and no usable FontBBox either: the real metrics live in
    // the font program. A zero-height loose box gives the line cells zero
    // area and the layout's region/text assignment drops them (2305's
    // References list lost every prose line, keeping only the URLs), so
    // fall back to typical text metrics.
    if asc - desc <= 1.0 {
        return (750.0, -250.0, invalid);
    }
    (asc, desc, invalid)
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
        Object::Real(r) => Some(real(*r)),
        _ => None,
    }
}

/// lopdf keeps a real as an `f32`; docling-parse reads the same token as a
/// double. The shortest decimal that round-trips the `f32` is the token the
/// file wrote (for any real written with the precision `f32` holds), so
/// `90.144 + 1.44` meets `91.584` here as it does there.
fn real(r: f32) -> f64 {
    use std::fmt::Write as _;
    let mut buf = String::with_capacity(16);
    let _ = write!(buf, "{r}");
    buf.parse().unwrap_or(r as f64)
}

/// Decode a PDF string literal into codes, retaining packed UTF-16 pairs
/// as the original four-byte keys used by ToUnicode.
fn codes(font: &Font, bytes: &[u8]) -> Vec<u32> {
    if font.two_byte {
        let mut units = bytes
            .chunks(2)
            .map(|c| {
                if c.len() == 2 {
                    ((c[0] as u32) << 8) | c[1] as u32
                } else {
                    c[0] as u32
                }
            })
            .peekable();
        let utf16 = matches!(font.cid_fallback, CidFallback::UnicodeCodes { utf16: true });
        let mut out = Vec::with_capacity(bytes.len() / 2 + 1);
        while let Some(mut code) = units.next() {
            if utf16 && (0xd800..=0xdbff).contains(&code) {
                if let Some(low) = units.next_if(|u| (0xdc00..=0xdfff).contains(u)) {
                    code = (code << 16) | low;
                }
            }
            out.push(code);
        }
        out
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
pub(crate) fn pad_short_xref_entries(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
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
    /// Image placements (#999), in the cells' frame — see
    /// [`crate::pdfium_backend::PdfPage::bitmaps`].
    pub bitmaps: Vec<[f32; 4]>,
    /// The page's shapes for the reading-order separators (#1092).
    pub shapes: PageShapes,
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
    /// How words split where no space glyph does (#921); from
    /// `DOCLING_RS_PDF_WORD_GAP` unless [`Self::set_word_gap`] says.
    word_gap: crate::dp_lines::WordGap,
    /// Whether [`Self::text_page`]'s page keeps typographic punctuation as
    /// drawn (#921); `DOCLING_RS_PDF_KEEP_TYPOGRAPHY` unless
    /// [`Self::set_keep_typography`] says.
    keep_typography: bool,
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
            word_gap: crate::dp_lines::WordGap::from_env(),
            keep_typography: crate::assemble::keep_typography_from_env(),
        })
    }

    /// The word-gap rule the line and word cells are built with (#921);
    /// `None` keeps the environment's (`DOCLING_RS_PDF_WORD_GAP`, else
    /// `adaptive`).
    pub fn set_word_gap(&mut self, gap: Option<crate::dp_lines::WordGap>) {
        if let Some(g) = gap {
            self.word_gap = g;
        }
    }

    /// Whether the pages [`Self::text_page`] builds keep typographic
    /// punctuation as drawn (#921); `None` keeps the environment's
    /// (`DOCLING_RS_PDF_KEEP_TYPOGRAPHY`, else folding).
    pub fn set_keep_typography(&mut self, keep: Option<bool>) {
        if let Some(k) = keep {
            self.keep_typography = k;
        }
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

    /// The characters of the 0-based page `index` as the script-position
    /// stage measures them (#1005, docling's `read_page_chars`): every
    /// visible, non-blank glyph with its font-metric box, baseline and drawn
    /// size in the unrotated page frame (y up), and beside it the same box in
    /// the display frame (`/Rotate` applied, top-left origin: the frame the
    /// items' boxes live in). Invisible text (`Tr` 3 / 7, an OCR layer over a
    /// scan) is left out, as docling leaves it: it follows the scanned lines,
    /// not a typeset baseline.
    pub(crate) fn script_chars(
        &mut self,
        index: usize,
    ) -> Vec<(crate::script_position::ScriptChar, [f32; 4])> {
        let Some(&pid) = self.pages.get(index) else {
            return Vec::new();
        };
        let (w, h) = page_size(&self.doc, pid);
        let rotation = page_rotation(&self.doc, pid);
        page_glyphs_cached(&self.doc, pid, &mut self.caches)
            .iter()
            .filter(|g| !g.invisible && !g.ch.is_whitespace() && g.ll.is_finite())
            .filter_map(|g| {
                let [_, y, size] = g.pen?;
                let c = crate::script_position::ScriptChar {
                    text: g.ch,
                    left: g.ll,
                    bottom: g.lb,
                    right: g.lr,
                    top: g.lt,
                    baseline: y,
                    size,
                    horizontal: g.quad.is_none(),
                };
                let (l, t, r, b) = crate::pdfium_backend::to_display_frame(
                    (g.ll, h - g.lt, g.lr, h - g.lb),
                    rotation,
                    w,
                    h,
                );
                Some((c, [l, t, r, b]))
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
        let (glyphs, inks, bitmaps, shapes) =
            page_glyphs_and_inks(&self.doc, pid, &mut self.caches);
        let (prose, words) =
            crate::dp_lines::line_and_word_cells_with(&glyphs, h, true, self.word_gap);
        PageParserCells {
            prose,
            words,
            code: crate::pdfium_backend::code_cells_from_glyphs(&glyphs, h),
            checkboxes: crate::checkbox::find(&inks, h),
            bitmaps,
            shapes,
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
        let (w, h, (glyphs, inks, _, shapes)) = match self.pages.get(index) {
            Some(&pid) => {
                let (w, h) = page_size(&self.doc, pid);
                (w, h, page_glyphs_and_inks(&self.doc, pid, &mut self.caches))
            }
            None => (0.0, 0.0, Default::default()),
        };
        let (mut prose, mut words) =
            crate::dp_lines::line_and_word_cells_with(&glyphs, h, true, self.word_gap);
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
            bitmaps: Vec::new(),
            shapes,
            #[cfg(feature = "ocr-prep")]
            image: image::RgbImage::new(1, 1),
            links: Vec::new(),
            rotation: 0,
            keep_typography: self.keep_typography,
            #[cfg(feature = "render")]
            embedded_images: Vec::new(),
            #[cfg(feature = "render")]
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
    /// The text rendering mode (`Tr`).
    tr: i64,
}

impl TextState {
    const INIT: TextState = TextState {
        tc: 0.0,
        tw: 0.0,
        th: 1.0,
        tl: 0.0,
        trise: 0.0,
        fsize: 0.0,
        tr: 0,
    };
}

/// pdfium's `CPDF_Page::GetPageRotation` — the inherited `/Rotate`, in
/// quarter turns, as degrees (`pdf_meta`'s rule).
fn page_rotation(doc: &Document, pid: lopdf::ObjectId) -> u16 {
    let mut id = pid;
    for _ in 0..32 {
        let Ok(dict) = doc.get_dictionary(id) else {
            return 0;
        };
        if let Some(obj) = dict.get(b"Rotate").ok().and_then(|o| deref(doc, o)) {
            let quarter = (num(obj).unwrap_or(0.0) as i64 / 90).rem_euclid(4);
            return (quarter * 90) as u16;
        }
        match dict.get(b"Parent").ok().and_then(|o| o.as_reference().ok()) {
            Some(parent) => id = parent,
            None => return 0,
        }
    }
    0
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
/// (#609) — the page's image placements (#999, `[l, t, r, b]` top-left
/// page points clipped to the page, the cells' frame), and its shapes for the
/// reading-order separators (#1092, [`PageShapes`]).
fn page_glyphs_and_inks(
    doc: &Document,
    page_id: lopdf::ObjectId,
    caches: &mut DocCaches,
) -> (
    Vec<Glyph>,
    Vec<crate::checkbox::Ink>,
    Vec<[f32; 4]>,
    PageShapes,
) {
    let mut out = Vec::new();
    let mut inks = Vec::new();
    // get_page_content returns the assembled content-stream bytes directly
    // (an empty Vec when the page has none).
    let content_bytes = doc.get_page_content(page_id);
    let Ok(content) = lopdf::content::Content::decode(&content_bytes) else {
        return (out, inks, Vec::new(), PageShapes::default());
    };
    let mut bitmaps = Vec::new();
    let mut page_shapes = PageShapes::default();
    if let Some(res) = page_res(doc, page_id) {
        // pdfium's page matrix: user space translated so the display box's
        // lower-left corner is the origin (see [`PageBox`]).
        let pb = page_box(doc, page_id);
        let base = Mat {
            e: -(pb.l as f64),
            f: -(pb.b as f64),
            ..Mat::ID
        };
        let mut shapes = Shapes::default();
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
            ShapeGs::INIT,
            &mut shapes,
        );
        out.retain(|g| on_page(g, pb.w, pb.h));
        drop_overprinted_spaces(&mut out);
        drop_shadow_runs(&mut out);
        let (w, h) = (f64::from(pb.w), f64::from(pb.h));
        inks.retain(|ink| match *ink {
            crate::checkbox::Ink::Bitmap { l, b, r, t } => {
                let (l, r, b, t) = (l.max(0.0), r.min(w), b.max(0.0), t.min(h));
                if r > l && t > b {
                    bitmaps.push([l as f32, (h - t) as f32, r as f32, (h - b) as f32]);
                }
                false
            }
            _ => true,
        });
        let top_left =
            |[l, b, r, t]: [f64; 4]| [l as f32, (h - t) as f32, r as f32, (h - b) as f32];
        page_shapes.lines = shapes.lines.into_iter().map(top_left).collect();
        if shapes.boxes.len() <= MAX_SHAPE_BOXES {
            page_shapes.boxes = connected_boxes(&shapes.boxes)
                .into_iter()
                .map(top_left)
                .collect();
        } else {
            docling_core::debug_log!(
                "textparse: {} painted paths on the page; their boxes are not merged",
                shapes.boxes.len()
            );
        }
    }
    (out, inks, bitmaps, page_shapes)
}

/// A page's shapes for the reading-order separators (#1092), in the cells'
/// frame (`[l, t, r, b]`, top-left page points): docling-parse's
/// `get_shape_lines` and `get_connected_shape_bounding_boxes`.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct PageShapes {
    pub lines: Vec<[f32; 4]>,
    pub boxes: Vec<[f32; 4]>,
}

/// Drop a run of glyphs painted again over itself — a text shadow. A word
/// processor exports a heading with a shadow effect as the same run drawn
/// twice: the shadow, then the text over it, shifted by a fraction of a point
/// (0.5–0.8 pt up and left, black under red, in the manuals that showed it).
/// docling-parse keeps both copies and reads `Safety instructions Safety
/// instructions`; pdfium's text page, behind docling's pypdfium2 backend,
/// drops the second, and the page shows the heading once. A deliberate
/// divergence (`docs/MIGRATION.md` §4 item 25): a glyph that repeats an
/// earlier one — same character, font and size, its pen shifted from the
/// earlier pen by at most 0.15 em on each axis, and by less than the earlier
/// glyph's step to its own successor — starts a copy when the glyphs after it
/// repeat the earlier one's successors by the same offset and size, and the
/// whole stretch that keeps the offset is dropped. Two glyphs at least: a lone
/// mark stamped twice is left alone, and so is the per-glyph re-stamp the
/// sanitizer already folds (`dp_lines`, the overprint duplicate). Text that
/// really repeats sits an advance apart or more — a doubled letter (`ll`,
/// 0.22 em in Helvetica), a word set twice (`very very`), the same text on
/// the next line or in the next table cell — and is kept, and so is a run of
/// one character set tighter than a shadow's shift (dot leaders squeezed by
/// `Tc`): its glyph two on is farther than the next one. A line stamped two
/// or three times at a small offset to fake bold reads once the same way. The
/// original must start within the 1024 glyphs before its copy, so a shadowed
/// block longer than that is not caught; a shadowed heading is drawn right
/// after itself. A rotated run, whose pen positions are not compared here, is
/// left alone.
fn drop_shadow_runs(glyphs: &mut Vec<Glyph>) {
    // How far back a copy's original may start.
    const LOOKBACK: usize = 1024;
    // The largest shift of a copy, per axis, in ems of the glyph's size.
    const NEAR_EM: f32 = 0.15;
    // How much the shift and the size may vary along a copy, in ems.
    const SAME_EM: f32 = 0.01;
    let pen = |g: &Glyph| if g.quad.is_none() { g.pen } else { None };
    let n = glyphs.len();
    let mut keep = vec![true; n];
    // The glyphs a copy may start on, by character and font, in stream
    // order: kept (not part of a copy), upright, sized and not whitespace. A
    // copy repeats both, so a glyph walks only its own list back through the
    // window; scanning the whole window nearly doubled a `--text-layer-only`
    // read of pages without a shadow.
    let mut earlier: HashMap<(char, u64), Vec<usize>> = HashMap::new();
    let mut j = 0;
    while j < n {
        let b = &glyphs[j];
        let start = pen(b).filter(|p| p[2] > 0.0 && !b.ch.is_whitespace());
        let mut copy = 0;
        if let Some([xj, yj, sj]) = start {
            let lo = j.saturating_sub(LOOKBACK);
            let candidates = earlier.entry((b.ch, b.font)).or_default();
            for &i in candidates.iter().rev().take_while(|&&i| i >= lo) {
                let Some([xi, yi, si]) = pen(&glyphs[i]) else {
                    continue;
                };
                if (si - sj).abs() > SAME_EM * sj {
                    continue;
                }
                let (dx, dy) = (xj - xi, yj - yi);
                if dx.abs() > NEAR_EM * sj || dy.abs() > NEAR_EM * sj {
                    continue;
                }
                // A shadow is shifted: the characters one glyph expands to
                // (a code-like name read as `SM590000`, a decomposed
                // ligature) share the glyph's pen and are not a copy.
                if dx.abs() + dy.abs() <= SAME_EM * sj {
                    continue;
                }
                // And it lies over its original, nearer to it than the
                // original's next glyph is: one character set tighter than
                // half the largest shift (dot leaders squeezed by `Tc`) meets
                // its own successors at one offset two glyphs on.
                if let Some([xn, yn, _]) = glyphs.get(i + 1).and_then(pen) {
                    let step = (xn - xi).abs().max((yn - yi).abs());
                    if step > SAME_EM * sj && dx.abs().max(dy.abs()) >= step {
                        continue;
                    }
                }
                // The copy goes on while each next glyph repeats the
                // original's next one by the same shift; the original
                // ends before the copy starts.
                let mut m = 1;
                while j + m < n && i + m < j {
                    let (a, b) = (&glyphs[i + m], &glyphs[j + m]);
                    let (Some([xa, ya, sa]), Some([xb, yb, sb])) = (pen(a), pen(b)) else {
                        break;
                    };
                    if a.ch != b.ch
                        || a.font != b.font
                        || (sa - sb).abs() > SAME_EM * sj
                        || ((xb - xa) - dx).abs() > SAME_EM * sj
                        || ((yb - ya) - dy).abs() > SAME_EM * sj
                    {
                        break;
                    }
                    m += 1;
                }
                if m >= 2 {
                    copy = m;
                    break;
                }
            }
            if copy == 0 {
                candidates.push(j);
            }
        }
        if copy > 0 {
            keep[j..j + copy].iter_mut().for_each(|k| *k = false);
            j += copy;
        } else {
            j += 1;
        }
    }
    let mut it = keep.into_iter();
    glyphs.retain(|_| it.next().unwrap_or(true));
}

/// How many marked-content spans may be open for one's `/ActualText` to be
/// read (see [`apply_actual_text`]); real documents nest a few.
const MAX_ACTUAL_TEXT_DEPTH: usize = 64;

/// Put a marked-content span's `/ActualText` in place of the glyphs drawn
/// inside it, as docling-parse does since 7.9 (`apply_actual_text`,
/// docling-parse#300, on by default; PDF 32000-1 §14.9.4). A writer draws a
/// glyph cluster whose characters the font cannot spell this way: a base
/// letter and a zero-advance accent whose code collides with a Latin letter
/// (`Oreši` read as `OresMi`, docling#3783), or the Arabic marks of a
/// LibreOffice font subset whose `/ToUnicode` skips them, so the byte reads as
/// WinAnsi (`منعًا` as `منع8ا`). docling-parse's rules, cell for cell: a span
/// with no glyphs (over graphics) is left alone, and so is a text implausibly
/// long for it (more than 8 characters and 4 per cell: `/Alt` written into
/// the wrong key); an empty text removes the glyphs; as many characters as
/// cells replace them one by one, keeping each cell's geometry; any other
/// count puts the whole text on the span's first cell, over the union of the
/// span's boxes, its baseline running to the cell that reaches furthest along
/// it (an accent drawn after its base letter trails behind it). Nested spans
/// apply innermost first, so the outer text wins. docling-parse has a cell per
/// character code; a code whose `/ToUnicode` spells several characters is
/// several glyphs here, all on the code's pen, and counts as one cell.
///
/// Each `EMC` rewrites the glyphs under its span, so a crafted nest of spans
/// over the same glyphs would cost glyphs × depth: past
/// [`MAX_ACTUAL_TEXT_DEPTH`] open spans, a span still pairs with its `EMC` but
/// its `/ActualText` is not read.
fn apply_actual_text(out: &mut Vec<Glyph>, begin: usize, text: &str) {
    let begin = begin.min(out.len());
    let mut cells: Vec<std::ops::Range<usize>> = Vec::new();
    for i in begin..out.len() {
        match cells.last_mut() {
            Some(cell) if out[i].pen.is_some() && out[i].pen == out[cell.end - 1].pen => {
                cell.end = i + 1;
            }
            _ => cells.push(i..i + 1),
        }
    }
    if cells.is_empty() {
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > (4 * cells.len()).max(8) {
        return;
    }
    if chars.is_empty() {
        out.truncate(begin);
        return;
    }
    if chars.len() == cells.len() {
        // Each cell's first glyph takes its character, the rest of the cell
        // goes: one pass, writing behind the cells read.
        for (w, (cell, &ch)) in cells.iter().zip(&chars).enumerate() {
            out[begin + w] = Glyph {
                ch,
                ..out[cell.start]
            };
        }
        out.truncate(begin + cells.len());
        return;
    }
    // The leading edge (quad corners 0 and 3) stays the first cell's; the
    // trailing one (1 and 2) is the cell's that reaches furthest along the
    // baseline, projected on it so a rotated run keeps its direction.
    let quad_of = |g: &Glyph| {
        g.quad
            .unwrap_or([g.ll, g.lb, g.lr, g.lb, g.lr, g.lt, g.ll, g.lt])
    };
    let mut merged = Glyph { ..out[begin] };
    let mut quad = quad_of(&merged);
    let dir = (quad[2] - quad[0], quad[3] - quad[1]);
    let along = |q: &[f32; 8]| q[2] * dir.0 + q[3] * dir.1;
    let mut reach = along(&quad);
    for g in &out[begin + 1..] {
        merged.l = merged.l.min(g.l);
        merged.b = merged.b.min(g.b);
        merged.r = merged.r.max(g.r);
        merged.t = merged.t.max(g.t);
        merged.ll = merged.ll.min(g.ll);
        merged.lb = merged.lb.min(g.lb);
        merged.lr = merged.lr.max(g.lr);
        merged.lt = merged.lt.max(g.lt);
        let q = quad_of(g);
        if along(&q) > reach {
            reach = along(&q);
            quad[2..6].copy_from_slice(&q[2..6]);
        }
    }
    if merged.quad.is_some() {
        merged.quad = Some(quad);
    }
    out.truncate(begin);
    // The characters of one cell share its box, as a ligature's do.
    out.extend(chars.iter().map(|&ch| Glyph { ch, ..merged }));
}

/// Drop a space glyph the next glyph is drawn over (#992). A typesetter
/// that sets a superscript footnote marker as `( 1 )` — space glyphs around
/// the raised digit — and then cancels each space's advance with a negative
/// `TJ` offset (Arbortext on EUR-Lex: the `1` starts 0.05 pt *before* its
/// space begins) renders `(1)`; the spaces exist in the content stream
/// only. docling-parse keeps them as characters and its line cell reads
/// `( 1 )`; pdftotext and pdfium, which break words by geometry alone,
/// read `(1)`, and so does the page. A deliberate divergence
/// (`docs/MIGRATION.md` §4 item 22): a space whose successor in stream
/// order sits on the same line and starts within the first 30 % of the
/// space's advance — the pen went back to (or before) where the space
/// began, so it left no gap — is not a character of the text. A space
/// followed by a glyph on another line (a trailing space before the
/// cursor jumps to the next line's left margin, far to the left of it) or
/// by one that starts past that mark (a kerned pair, a justified word gap
/// pulled in a little) is kept; the word-gap rules then see the geometry
/// as before.
fn drop_overprinted_spaces(glyphs: &mut Vec<Glyph>) {
    if glyphs.len() < 2 {
        return;
    }
    let overprinted = |sp: &Glyph, next: &Glyph| {
        if sp.ch != ' ' || next.ch == ' ' {
            return false;
        }
        // Upright glyphs only: a rotated run's loose boxes are the extents
        // of its quads, every glyph of a 90° line shares one left edge and
        // the advance runs along y, so the test below would read every
        // space of a landscape slide drawn through a 90° `cm` as
        // overprinted (`Nullammollis`, odf_presentation_02).
        if sp.quad.is_some() || next.quad.is_some() {
            return false;
        }
        let width = sp.lr - sp.ll;
        if !width.is_finite() || width <= 0.0 || !next.ll.is_finite() {
            return false;
        }
        // Same line: the loose boxes overlap vertically (a raised digit's box
        // still overlaps its neighbours' em boxes).
        let same_line = next.lb < sp.lt && next.lt > sp.lb;
        same_line && next.ll >= sp.ll - 0.1 * width && next.ll <= sp.ll + 0.3 * width
    };
    let mut keep = vec![true; glyphs.len()];
    for i in 0..glyphs.len() - 1 {
        if overprinted(&glyphs[i], &glyphs[i + 1]) {
            keep[i] = false;
        }
    }
    let mut it = keep.into_iter();
    glyphs.retain(|_| it.next().unwrap_or(true));
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
    shape_init: ShapeGs,
    shapes: &mut Shapes,
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
    let mut gstate_stack: Vec<(Mat, f64, f64, f64, f64, f64, f64, i64, Option<&Arc<Font>>)> =
        Vec::new();
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
    let mut tr = init.tr; // rendering mode

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
    // Open marked-content sequences (`BMC`/`BDC` … `EMC`): where each one's
    // glyphs start in `out`, and its inline `/ActualText`, if any.
    let mut marked: Vec<(usize, Option<String>)> = Vec::new();
    // The reading-order shapes (#1092): docling-parse's view of the path
    // under construction, its graphics state and the pending clip.
    let mut shape_gs = shape_init;
    let mut shape_stack: Vec<ShapeGs> = Vec::new();
    let mut shape_path: Vec<ShapeSub> = Vec::new();
    let mut clip_pending = false;
    let ext_gstates = res
        .get(b"ExtGState")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok());
    let colorspaces = res
        .get(b"ColorSpace")
        .ok()
        .and_then(|o| deref(doc, o))
        .and_then(|o| o.as_dict().ok());

    for op in &content.operations {
        let operands = &op.operands;
        let shown = out.len();
        match op.operator.as_str() {
            "q" => {
                gstate_stack.push((ctm, tc, tw, th, tl, trise, fsize, tr, font));
                fill_stack.push(fill_light);
                shape_stack.push(shape_gs.clone());
            }
            "BMC" => marked.push((out.len(), None)),
            // Only an inline property list is read: a named one (`/OC /MC0`)
            // would need the `/Properties` resource, as in docling-parse.
            "BDC" => {
                let actual = operands
                    .get(1)
                    .filter(|_| marked.len() < MAX_ACTUAL_TEXT_DEPTH)
                    .and_then(|o| o.as_dict().ok())
                    .and_then(|d| d.get(b"ActualText").ok())
                    .and_then(|o| match o {
                        Object::String(bytes, _) => Some(crate::info::decode_text(bytes)),
                        _ => None,
                    });
                marked.push((out.len(), actual));
            }
            "EMC" => {
                if let Some((begin, Some(text))) = marked.pop() {
                    apply_actual_text(out, begin, &text);
                }
                // Shows no text: the invisible-mode stamp below has nothing
                // new to mark, and the span may have shortened `out`.
                continue;
            }
            "Q" => {
                fill_light = fill_stack.pop().unwrap_or(false);
                if let Some(gs) = shape_stack.pop() {
                    shape_gs = gs;
                }
                if let Some((c, a, b, h, l, r, fs, mode, f)) = gstate_stack.pop() {
                    ctm = c;
                    tc = a;
                    tw = b;
                    th = h;
                    tl = l;
                    trise = r;
                    fsize = fs;
                    tr = mode;
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
            "Tr" => tr = op_f(operands, 0) as i64,
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
                shape_path.push(ShapeSub::at(cur));
            }
            "l" => {
                let p = ctm.apply(op_f(operands, 0), op_f(operands, 1));
                path.push(PathPiece::Line(cur, p));
                cur = p;
                if let Some(sub) = shape_path.last_mut() {
                    sub.lines.push((sub.cur, p));
                    sub.pts.push(p);
                    sub.cur = p;
                }
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
                    if let Some(sub) = shape_path.last_mut() {
                        sub.pts.extend(pts.iter().copied());
                        sub.cur = end;
                    }
                }
            }
            "h" => {
                if cur != start {
                    path.push(PathPiece::Line(cur, start));
                }
                cur = start;
                // docling-parse closes the subpath and opens the next one at
                // its start, where a following `l` continues.
                if let Some(sub) = shape_path.last_mut() {
                    sub.closed = true;
                    sub.pts.push(sub.start);
                    let at = sub.start;
                    shape_path.push(ShapeSub::at(at));
                }
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
                // `m` + three `l` + `h`, as docling-parse builds it.
                let c = [
                    ctm.apply(x, y),
                    ctm.apply(x + w, y),
                    ctm.apply(x + w, y + h),
                    ctm.apply(x, y + h),
                ];
                shape_path.push(ShapeSub {
                    start: c[0],
                    cur: c[3],
                    pts: vec![c[0], c[1], c[2], c[3], c[0]],
                    lines: vec![(c[0], c[1]), (c[1], c[2]), (c[2], c[3])],
                    closed: true,
                });
                shape_path.push(ShapeSub::at(c[0]));
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
                // `s`, `f`, `f*`, `b`, `b*` close the last subpath first.
                if !matches!(op, "S" | "B" | "B*") {
                    if let Some(sub) = shape_path.last_mut() {
                        sub.closed = true;
                    }
                }
                let scale = (ctm.a * ctm.d - ctm.b * ctm.c).abs().sqrt();
                record_shape(
                    &shape_path,
                    stroke,
                    fill,
                    &shape_gs,
                    if scale > 0.0 { scale } else { 1.0 },
                    shapes,
                );
                // A pending `W` takes effect after the painting operator.
                if std::mem::take(&mut clip_pending) {
                    capture_clip(&shape_path, &mut shape_gs.clips);
                }
                shape_path.clear();
            }
            "n" => {
                path.clear();
                if std::mem::take(&mut clip_pending) {
                    capture_clip(&shape_path, &mut shape_gs.clips);
                }
                shape_path.clear();
            }
            "W" | "W*" => clip_pending = true,
            "w" => shape_gs.line_width = op_f(operands, 0),
            "gs" => {
                let params = operands
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .and_then(|n| ext_gstates?.get(n).ok())
                    .and_then(|o| deref(doc, o))
                    .and_then(|o| o.as_dict().ok());
                if let Some(params) = params {
                    let get = |k: &[u8]| params.get(k).ok().and_then(num);
                    if let Some(lw) = get(b"LW") {
                        shape_gs.line_width = lw;
                    }
                    if let Some(a) = get(b"CA") {
                        shape_gs.stroke_alpha = a;
                    }
                    if let Some(a) = get(b"ca") {
                        shape_gs.fill_alpha = a;
                    }
                }
            }
            // The fill colour, by its components: gray `g`, `rg`, CMYK `k`,
            // and `sc`/`scn` by operand count (a pattern name is not light).
            "g" | "rg" | "k" | "sc" | "scn" => {
                // docling-parse: a colour resolves a pattern fill; a pattern
                // name in `scn` makes it one.
                if matches!(op.operator.as_str(), "scn")
                    && matches!(operands.last(), Some(Object::Name(_)))
                {
                    shape_gs.fill_pattern = true;
                } else if !operands.is_empty() {
                    shape_gs.fill_pattern = false;
                }
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
            "cs" => {
                fill_light = false;
                // A Pattern space carries no colour until `scn` names one.
                let pattern = match operands.first() {
                    Some(Object::Name(n)) if n.as_slice() == b"Pattern" => true,
                    Some(Object::Name(n)) => colorspaces
                        .and_then(|cs| cs.get(n).ok())
                        .and_then(|o| deref(doc, o))
                        .is_some_and(|o| match o {
                            Object::Name(f) => f.as_slice() == b"Pattern",
                            Object::Array(a) => a
                                .first()
                                .and_then(|f| f.as_name().ok())
                                .is_some_and(|f| f == b"Pattern"),
                            _ => false,
                        }),
                    _ => false,
                };
                if pattern {
                    shape_gs.fill_pattern = true;
                }
            }
            // An inline image: the unit square under the CTM (#999).
            "BI" => inks.push(image_ink(ctm)),
            "Do" => {
                // Invoke a Form XObject: bulk body text in many PDFs lives inside
                // a form, reached only here. An image XObject carries no text,
                // only its placement (#999).
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
                    let is_image = stream
                        .dict
                        .get(b"Subtype")
                        .ok()
                        .and_then(|o| o.as_name().ok())
                        == Some(b"Image".as_slice());
                    if is_image {
                        inks.push(image_ink(ctm));
                    }
                    continue;
                }
                // Decode a reused form's content once per document
                // (headers/footers and bulk body text invoke the same form on
                // every page); see `DocCaches::forms_seen` for single-use ones.
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
                            if !caches.forms_seen.insert(id) {
                                caches.forms.insert(id, Arc::clone(&c));
                            }
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
                    tr,
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
                    shape_gs.clone(),
                    shapes,
                );
            }
            _ => {}
        }
        // Invisible text (`Tr` 3, or 7: clip only) — an OCR layer over a scan
        // follows the scanned lines, not a typeset baseline (#1005).
        if matches!(tr, 3 | 7) {
            for g in &mut out[shown..] {
                g.invisible = true;
            }
        }
    }
}

/// An image placement's box — the unit square under `ctm`, y-up page
/// points — as an [`Ink::Bitmap`](crate::checkbox::Ink::Bitmap).
fn image_ink(ctm: Mat) -> crate::checkbox::Ink {
    let pts = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].map(|(x, y)| ctm.apply(x, y));
    let (mut l, mut b, mut r, mut t) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for (x, y) in pts {
        (l, b, r, t) = (l.min(x), b.min(y), r.max(x), t.max(y));
    }
    crate::checkbox::Ink::Bitmap { l, b, r, t }
}

/// The page's vector shapes as docling-parse's page decoder reports them to
/// docling's reading order (#1092): `get_shape_lines` — every stroked,
/// axis-aligned straight segment, clipped — and the boxes
/// `get_connected_shape_bounding_boxes` merges — every visible painted path,
/// its stroke's half width included, clipped. `[l, b, r, t]`, page points,
/// bottom-left origin, in the frame `run_content` walks (the display box's
/// lower-left corner at the origin, before `/Rotate`).
#[derive(Default, Clone, Debug, PartialEq)]
pub(crate) struct Shapes {
    pub lines: Vec<[f64; 4]>,
    pub boxes: Vec<[f64; 4]>,
}

/// More painted paths than this on a page and their boxes are not merged —
/// docling-parse's merge is quadratic in them, and a page that dense (a map,
/// a seat plan, text drawn as outlines) yields no trustworthy rule anyway;
/// the stroked lines still count.
const MAX_SHAPE_BOXES: usize = 10_000;

/// One subpath of the shape path under construction, page space: what
/// docling-parse's `page_item<PAGE_SHAPE>` keeps of it.
struct ShapeSub {
    start: (f64, f64),
    cur: (f64, f64),
    /// Every point the path passes or is controlled by — a curve's control
    /// points included, as docling-parse's segment data has them.
    pts: Vec<(f64, f64)>,
    /// Its straight segments, in order.
    lines: Vec<((f64, f64), (f64, f64))>,
    closed: bool,
}

impl ShapeSub {
    fn at(p: (f64, f64)) -> Self {
        ShapeSub {
            start: p,
            cur: p,
            pts: vec![p],
            lines: Vec::new(),
            closed: false,
        }
    }

    /// `keep_shape`: two points at least, spanning more than a speck (the
    /// box diagonal, squared, over 1e-3).
    fn kept(&self) -> bool {
        let Some((l, b, r, t)) = bbox_of(&self.pts) else {
            return false;
        };
        self.pts.len() >= 2 && (r - l).powi(2) + (t - b).powi(2) >= 1e-3
    }
}

fn bbox_of(pts: &[(f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    let (&first, rest) = pts.split_first()?;
    Some(rest.iter().fold(
        (first.0, first.1, first.0, first.1),
        |(l, b, r, t), &(x, y)| (l.min(x), b.min(y), r.max(x), t.max(y)),
    ))
}

/// The graphics state docling-parse's shapes read: line width (user space),
/// stroke and fill alpha (`CA` / `ca`), whether the fill colour is a pattern
/// not yet resolved to a paint, and the clip paths' boxes. Saved and
/// restored with `q` / `Q`.
#[derive(Clone)]
struct ShapeGs {
    line_width: f64,
    stroke_alpha: f64,
    fill_alpha: f64,
    fill_pattern: bool,
    clips: Vec<[f64; 4]>,
}

impl ShapeGs {
    const INIT: ShapeGs = ShapeGs {
        line_width: 1.0,
        stroke_alpha: 1.0,
        fill_alpha: 1.0,
        fill_pattern: false,
        clips: Vec::new(),
    };
}

/// docling-parse's `bbox_intersects`: the boxes share area (a degenerate box
/// lying inside the other counts).
fn boxes_intersect(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] && a[2] > b[0] && a[1] < b[3] && a[3] > b[1]
}

/// `clipped_bbox`: a box cut to every clip path's box in turn; `None` once
/// one of them misses it.
fn clipped(mut bbox: [f64; 4], clips: &[[f64; 4]]) -> Option<[f64; 4]> {
    for c in clips {
        if !boxes_intersect(bbox, *c) {
            return None;
        }
        bbox = [
            bbox[0].max(c[0]),
            bbox[1].max(c[1]),
            bbox[2].min(c[2]),
            bbox[3].min(c[3]),
        ];
    }
    Some(bbox)
}

/// `get_shape_lines`' `maybe_add_line` + `clip_axis_aligned_segment`: a
/// straight segment that is horizontal or vertical within 1e-3 (not both: a
/// point), cut to every clip path's box along its axis and dropped when one
/// misses it.
fn shape_line(a: (f64, f64), z: (f64, f64), clips: &[[f64; 4]], out: &mut Vec<[f64; 4]>) {
    const TOL: f64 = 1e-3;
    let horizontal = (z.1 - a.1).abs() <= TOL;
    let vertical = (z.0 - a.0).abs() <= TOL;
    if horizontal == vertical {
        return;
    }
    let mut bbox = [a.0.min(z.0), a.1.min(z.1), a.0.max(z.0), a.1.max(z.1)];
    for c in clips {
        if horizontal {
            if a.1 < c[1] - TOL || a.1 > c[3] + TOL {
                return;
            }
            bbox[0] = bbox[0].max(c[0]);
            bbox[2] = bbox[2].min(c[2]);
            if bbox[0] > bbox[2] + TOL {
                return;
            }
        } else {
            if a.0 < c[0] - TOL || a.0 > c[2] + TOL {
                return;
            }
            bbox[1] = bbox[1].max(c[1]);
            bbox[3] = bbox[3].min(c[3]);
            if bbox[1] > bbox[3] + TOL {
                return;
            }
        }
    }
    out.push(bbox);
}

/// `capture_pending_clip`: each subpath of the clipping path with a segment
/// (two points or more) bounds what later operators paint — appended, which
/// approximates the intersection the spec asks for, as docling-parse does.
fn capture_clip(subs: &[ShapeSub], clips: &mut Vec<[f64; 4]>) {
    for sub in subs.iter().filter(|s| s.pts.len() >= 2) {
        if let Some((l, b, r, t)) = bbox_of(&sub.pts) {
            clips.push([l, b, r, t]);
        }
    }
}

/// docling-parse's `register_paths` for the reading-order geometry: a painted
/// path's kept subpaths become shape lines (when its stroke shows) and a
/// shape box (when anything of it shows). A fill whose colour is an
/// unresolved pattern paints nothing, so a fill-only path is no shape and a
/// fill-and-stroke one keeps its stroke. `scale` maps the user-space line
/// width to page space (the CTM's `sqrt|det|`; a hairline stays 0).
fn record_shape(
    subs: &[ShapeSub],
    stroke: bool,
    fill: bool,
    gs: &ShapeGs,
    scale: f64,
    shapes: &mut Shapes,
) {
    let kept: Vec<&ShapeSub> = subs.iter().filter(|s| s.kept()).collect();
    if kept.is_empty() {
        return;
    }
    let fill = fill && !gs.fill_pattern;
    if gs.fill_pattern && !stroke {
        return;
    }
    let strokes_visible = stroke && gs.stroke_alpha > 0.0;
    let visible = strokes_visible || (fill && gs.fill_alpha > 0.0);
    if strokes_visible {
        for sub in &kept {
            for &(a, z) in &sub.lines {
                shape_line(a, z, &gs.clips, &mut shapes.lines);
            }
            if sub.closed {
                shape_line(sub.cur, sub.start, &gs.clips, &mut shapes.lines);
            }
        }
    }
    if visible {
        let pts: Vec<(f64, f64)> = kept.iter().flat_map(|s| s.pts.iter().copied()).collect();
        if let Some((l, b, r, t)) = bbox_of(&pts) {
            let pad = if strokes_visible {
                (gs.line_width * scale).max(0.0) * 0.5
            } else {
                0.0
            };
            if let Some(bbox) = clipped([l - pad, b - pad, r + pad, t + pad], &gs.clips) {
                shapes.boxes.push(bbox);
            }
        }
    }
}

/// `get_connected_shape_bounding_boxes`: each box in turn (unless already
/// taken) grows by every untaken box its growing union touches, until none
/// does — docling-parse's greedy pass, so a component finished early keeps a
/// box a later one would also have reached. Tolerance 0, as docling calls
/// it: two `re` that meet in the file meet here because [`real`] reads their
/// operands as the doubles docling-parse reads.
fn connected_boxes(boxes: &[[f64; 4]]) -> Vec<[f64; 4]> {
    let touches =
        |a: [f64; 4], b: [f64; 4]| a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1];
    let mut taken = vec![false; boxes.len()];
    let mut out = Vec::new();
    for i in 0..boxes.len() {
        if taken[i] {
            continue;
        }
        taken[i] = true;
        let mut component = boxes[i];
        let mut changed = true;
        while changed {
            changed = false;
            for j in 0..boxes.len() {
                if !taken[j] && touches(component, boxes[j]) {
                    let b = boxes[j];
                    component = [
                        component[0].min(b[0]),
                        component[1].min(b[1]),
                        component[2].max(b[2]),
                        component[3].max(b[3]),
                    ];
                    taken[j] = true;
                    changed = true;
                }
            }
        }
        out.push(component);
    }
    out
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
        let (text, w, blank) = font.decode_code(code);
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
        // A blank unmapped glyph keeps its advance but no ink height
        // (docling-parse collapses its ascent and descent).
        let (desc, asc) = if blank {
            (0.0, 0.0)
        } else if let Some(&(lo, hi)) = font.ink_y.get(&code) {
            (lo / 1000.0, hi / 1000.0)
        } else {
            (font.descent / 1000.0, font.ascent / 1000.0)
        };
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
        // The pen's origin (rise included) and the font size's extent from
        // it, as docling-parse maps them (`map_text_point(0, rise)` and
        // `(0, rise + font_size)`).
        let (ox, oy) = trm.apply(0.0, 0.0);
        let (sx, sy) = trm.apply(0.0, 1.0);
        let pen = Some([ox as f32, oy as f32, (sx - ox).hypot(sy - oy) as f32]);
        if let Some(s) = text {
            // A run may map one code to multiple chars (ligature/fraction); share box.
            for ch in s.chars() {
                // A NUL from the `/ToUnicode` is an unmapped glyph too
                // (docling#5070): U+FFFD.
                let ch = if ch == '\u{0}' { '\u{FFFD}' } else { ch };
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
                    pen,
                    math_font: font.math,
                    invisible: false,
                });
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
    } else if base_name == b"WinAnsiEncoding" {
        let mut m = winansi_table();
        // PDF 32000-1:2008 Annex D.2, note 3 to the Latin set: "In
        // WinAnsiEncoding, all unused codes greater than 40 [octal] map to the
        // bullet character" — pdfium and pdf.js read them so. ReportLab draws
        // its default list bullet as `\177` in a non-embedded WinAnsi
        // Helvetica, which read U+FFFD before (#1102). Only a font that names
        // WinAnsiEncoding gets them: the no-/Encoding fallback above stands
        // in for a built-in encoding, where these codes stay undefined.
        for b in [0x7Fu8, 0x81, 0x8D, 0x8F, 0x90, 0x9D] {
            m.insert(b, '\u{2022}');
        }
        m
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

/// The Adobe Glyph List's names for Latin-1 Supplement and Latin Extended-A
/// (plus `Euro`, `florin`, the comma-accent pairs): what a Type1 font's
/// `/Differences` or builtin encoding calls an accented or stroked letter.
/// 2203.01017v2 names `Ł` as `/Lslash` in a NimbusRomNo9L subset with no
/// ToUnicode, and the resolver, which knew only the ASCII, ligature and symbol
/// names, dropped the glyph outright (`Ł ukasz Kaiser` read `ukasz Kaiser`).
/// Sorted by name for the binary search in [`glyph_name_to_char`].
const AGL_LATIN: &[(&str, char)] = &[
    ("AE", '\u{00C6}'),
    ("Aacute", '\u{00C1}'),
    ("Abreve", '\u{0102}'),
    ("Acircumflex", '\u{00C2}'),
    ("Adieresis", '\u{00C4}'),
    ("Agrave", '\u{00C0}'),
    ("Amacron", '\u{0100}'),
    ("Aogonek", '\u{0104}'),
    ("Aring", '\u{00C5}'),
    ("Atilde", '\u{00C3}'),
    ("Cacute", '\u{0106}'),
    ("Ccaron", '\u{010C}'),
    ("Ccedilla", '\u{00C7}'),
    ("Ccircumflex", '\u{0108}'),
    ("Cdotaccent", '\u{010A}'),
    ("Dcaron", '\u{010E}'),
    ("Dcroat", '\u{0110}'),
    ("Eacute", '\u{00C9}'),
    ("Ebreve", '\u{0114}'),
    ("Ecaron", '\u{011A}'),
    ("Ecircumflex", '\u{00CA}'),
    ("Edieresis", '\u{00CB}'),
    ("Edotaccent", '\u{0116}'),
    ("Egrave", '\u{00C8}'),
    ("Emacron", '\u{0112}'),
    ("Eng", '\u{014A}'),
    ("Eogonek", '\u{0118}'),
    ("Eth", '\u{00D0}'),
    ("Euro", '\u{20AC}'),
    ("Gbreve", '\u{011E}'),
    ("Gcircumflex", '\u{011C}'),
    ("Gcommaaccent", '\u{0122}'),
    ("Gdotaccent", '\u{0120}'),
    ("Hbar", '\u{0126}'),
    ("Hcircumflex", '\u{0124}'),
    ("IJ", '\u{0132}'),
    ("Iacute", '\u{00CD}'),
    ("Ibreve", '\u{012C}'),
    ("Icircumflex", '\u{00CE}'),
    ("Idieresis", '\u{00CF}'),
    ("Idotaccent", '\u{0130}'),
    ("Igrave", '\u{00CC}'),
    ("Imacron", '\u{012A}'),
    ("Iogonek", '\u{012E}'),
    ("Itilde", '\u{0128}'),
    ("Jcircumflex", '\u{0134}'),
    ("Kcommaaccent", '\u{0136}'),
    ("Lacute", '\u{0139}'),
    ("Lcaron", '\u{013D}'),
    ("Lcommaaccent", '\u{013B}'),
    ("Ldot", '\u{013F}'),
    ("Lslash", '\u{0141}'),
    ("Nacute", '\u{0143}'),
    ("Ncaron", '\u{0147}'),
    ("Ncommaaccent", '\u{0145}'),
    ("Ntilde", '\u{00D1}'),
    ("OE", '\u{0152}'),
    ("Oacute", '\u{00D3}'),
    ("Obreve", '\u{014E}'),
    ("Ocircumflex", '\u{00D4}'),
    ("Odieresis", '\u{00D6}'),
    ("Ograve", '\u{00D2}'),
    ("Ohungarumlaut", '\u{0150}'),
    ("Omacron", '\u{014C}'),
    ("Oslash", '\u{00D8}'),
    ("Otilde", '\u{00D5}'),
    ("Racute", '\u{0154}'),
    ("Rcaron", '\u{0158}'),
    ("Rcommaaccent", '\u{0156}'),
    ("Sacute", '\u{015A}'),
    ("Scaron", '\u{0160}'),
    ("Scedilla", '\u{015E}'),
    ("Scircumflex", '\u{015C}'),
    ("Scommaaccent", '\u{0218}'),
    ("Tbar", '\u{0166}'),
    ("Tcaron", '\u{0164}'),
    ("Tcommaaccent", '\u{0162}'),
    ("Thorn", '\u{00DE}'),
    ("Uacute", '\u{00DA}'),
    ("Ubreve", '\u{016C}'),
    ("Ucircumflex", '\u{00DB}'),
    ("Udieresis", '\u{00DC}'),
    ("Ugrave", '\u{00D9}'),
    ("Uhungarumlaut", '\u{0170}'),
    ("Umacron", '\u{016A}'),
    ("Uogonek", '\u{0172}'),
    ("Uring", '\u{016E}'),
    ("Utilde", '\u{0168}'),
    ("Wcircumflex", '\u{0174}'),
    ("Yacute", '\u{00DD}'),
    ("Ycircumflex", '\u{0176}'),
    ("Ydieresis", '\u{0178}'),
    ("Zacute", '\u{0179}'),
    ("Zcaron", '\u{017D}'),
    ("Zdotaccent", '\u{017B}'),
    ("aacute", '\u{00E1}'),
    ("abreve", '\u{0103}'),
    ("acircumflex", '\u{00E2}'),
    ("acute", '\u{00B4}'),
    ("adieresis", '\u{00E4}'),
    ("ae", '\u{00E6}'),
    ("agrave", '\u{00E0}'),
    ("amacron", '\u{0101}'),
    ("aogonek", '\u{0105}'),
    ("aring", '\u{00E5}'),
    ("atilde", '\u{00E3}'),
    ("brokenbar", '\u{00A6}'),
    ("cacute", '\u{0107}'),
    ("ccaron", '\u{010D}'),
    ("ccedilla", '\u{00E7}'),
    ("ccircumflex", '\u{0109}'),
    ("cdotaccent", '\u{010B}'),
    ("cedilla", '\u{00B8}'),
    ("cent", '\u{00A2}'),
    ("currency", '\u{00A4}'),
    ("dcaron", '\u{010F}'),
    ("dcroat", '\u{0111}'),
    ("dieresis", '\u{00A8}'),
    ("dotlessi", '\u{0131}'),
    ("eacute", '\u{00E9}'),
    ("ebreve", '\u{0115}'),
    ("ecaron", '\u{011B}'),
    ("ecircumflex", '\u{00EA}'),
    ("edieresis", '\u{00EB}'),
    ("edotaccent", '\u{0117}'),
    ("egrave", '\u{00E8}'),
    ("emacron", '\u{0113}'),
    ("eng", '\u{014B}'),
    ("eogonek", '\u{0119}'),
    ("eth", '\u{00F0}'),
    ("exclamdown", '\u{00A1}'),
    ("florin", '\u{0192}'),
    ("gbreve", '\u{011F}'),
    ("gcircumflex", '\u{011D}'),
    ("gcommaaccent", '\u{0123}'),
    ("gdotaccent", '\u{0121}'),
    ("germandbls", '\u{00DF}'),
    ("guillemotleft", '\u{00AB}'),
    ("guillemotright", '\u{00BB}'),
    ("hbar", '\u{0127}'),
    ("hcircumflex", '\u{0125}'),
    ("iacute", '\u{00ED}'),
    ("ibreve", '\u{012D}'),
    ("icircumflex", '\u{00EE}'),
    ("idieresis", '\u{00EF}'),
    ("igrave", '\u{00EC}'),
    ("ij", '\u{0133}'),
    ("imacron", '\u{012B}'),
    ("iogonek", '\u{012F}'),
    ("itilde", '\u{0129}'),
    ("jcircumflex", '\u{0135}'),
    ("kcommaaccent", '\u{0137}'),
    ("kgreenlandic", '\u{0138}'),
    ("lacute", '\u{013A}'),
    ("lcaron", '\u{013E}'),
    ("lcommaaccent", '\u{013C}'),
    ("ldot", '\u{0140}'),
    ("logicalnot", '\u{00AC}'),
    ("longs", '\u{017F}'),
    ("lslash", '\u{0142}'),
    ("macron", '\u{00AF}'),
    ("nacute", '\u{0144}'),
    ("napostrophe", '\u{0149}'),
    ("ncaron", '\u{0148}'),
    ("ncommaaccent", '\u{0146}'),
    ("ntilde", '\u{00F1}'),
    ("oacute", '\u{00F3}'),
    ("obreve", '\u{014F}'),
    ("ocircumflex", '\u{00F4}'),
    ("odieresis", '\u{00F6}'),
    ("oe", '\u{0153}'),
    ("ograve", '\u{00F2}'),
    ("ohungarumlaut", '\u{0151}'),
    ("omacron", '\u{014D}'),
    ("onehalf", '\u{00BD}'),
    ("onequarter", '\u{00BC}'),
    ("onesuperior", '\u{00B9}'),
    ("ordfeminine", '\u{00AA}'),
    ("ordmasculine", '\u{00BA}'),
    ("oslash", '\u{00F8}'),
    ("otilde", '\u{00F5}'),
    ("paragraph", '\u{00B6}'),
    ("questiondown", '\u{00BF}'),
    ("racute", '\u{0155}'),
    ("rcaron", '\u{0159}'),
    ("rcommaaccent", '\u{0157}'),
    ("sacute", '\u{015B}'),
    ("scaron", '\u{0161}'),
    ("scedilla", '\u{015F}'),
    ("scircumflex", '\u{015D}'),
    ("scommaaccent", '\u{0219}'),
    ("section", '\u{00A7}'),
    ("sfthyphen", '\u{00AD}'),
    ("sterling", '\u{00A3}'),
    ("tbar", '\u{0167}'),
    ("tcaron", '\u{0165}'),
    ("tcommaaccent", '\u{0163}'),
    ("thorn", '\u{00FE}'),
    ("threequarters", '\u{00BE}'),
    ("threesuperior", '\u{00B3}'),
    ("twosuperior", '\u{00B2}'),
    ("uacute", '\u{00FA}'),
    ("ubreve", '\u{016D}'),
    ("ucircumflex", '\u{00FB}'),
    ("udieresis", '\u{00FC}'),
    ("ugrave", '\u{00F9}'),
    ("uhungarumlaut", '\u{0171}'),
    ("umacron", '\u{016B}'),
    ("uogonek", '\u{0173}'),
    ("uring", '\u{016F}'),
    ("utilde", '\u{0169}'),
    ("wcircumflex", '\u{0175}'),
    ("yacute", '\u{00FD}'),
    ("ycircumflex", '\u{0177}'),
    ("ydieresis", '\u{00FF}'),
    ("yen", '\u{00A5}'),
    ("zacute", '\u{017A}'),
    ("zcaron", '\u{017E}'),
    ("zdotaccent", '\u{017C}'),
];

/// docling-parse 7.25's reading text of a PostScript glyph name in a
/// program's builtin encoding (`builtin_font_encoding::glyph_name_to_utf8`,
/// #1113): `.notdef` and an index-style name (`g12`, `gid00012`, `cid12`,
/// `index12`) carry none; a stylistic `.suffix` is dropped; then the glyph
/// tables (the symbol fonts' names, [`glyph_name_to_char`]'s), a `uniXXXX`
/// sequence (a ligature's code points), a `uXXXX`–`uXXXXXX` name.
pub(crate) fn builtin_glyph_text(name: &str) -> Option<String> {
    if name.is_empty() || name == ".notdef" {
        return None;
    }
    let name = match name.find('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    let lower = name.to_ascii_lowercase();
    let index_name = ["gid", "glyph", "g", "cid", "index"].iter().any(|p| {
        lower
            .strip_prefix(p)
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    });
    if index_name {
        return None;
    }
    if let Some(c) = crate::symbol_fonts::symbol_glyph_char(name) {
        return Some(c.to_string());
    }
    let hex_points = |hex: &str, width: usize| -> Option<String> {
        (!hex.is_empty()
            && hex.len().is_multiple_of(width)
            && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| {
            (0..hex.len())
                .step_by(width)
                .map(|i| {
                    u32::from_str_radix(&hex[i..i + width], 16)
                        .ok()
                        .and_then(char::from_u32)
                })
                .collect::<Option<String>>()
        })
        .flatten()
    };
    if let Some(hex) = name.strip_prefix("uni") {
        if let Some(t) = hex_points(hex, 4) {
            return Some(t);
        }
    }
    if let Some(hex) = name.strip_prefix('u') {
        if (4..=6).contains(&hex.len()) {
            if let Some(t) = hex_points(hex, hex.len()) {
                return Some(t);
            }
        }
    }
    glyph_name_to_char(name.as_bytes()).map(String::from)
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
            if let Ok(i) = AGL_LATIN.binary_search_by_key(&s, |(n, _)| n) {
                return Some(AGL_LATIN[i].1);
            }
            // Strip an AGL `.suffix` (oldstyle/small-cap variant) and retry.
            if let Some((base, _)) = s.split_once('.') {
                if !base.is_empty() {
                    return glyph_name_to_char(base.as_bytes());
                }
            }
            // An AGL ligature name joins its components with `_` (`f_f_i`,
            // `f_l` — what a re-encoded Libertine subset calls them); joined,
            // the presentation-form arms above resolve them.
            if s.contains('_') {
                let joined: String = s.chars().filter(|&c| c != '_').collect();
                if !joined.is_empty() && joined.len() < s.len() {
                    return glyph_name_to_char(joined.as_bytes());
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
        // The rest of Windows-1252's 0x80–0x9F row. Left out, the German
        // opening quote `„` (0x84) vanished from the text layer while its
        // closing `“` (0x93) stayed (#922); `€ ‚ ƒ † ‡ ˆ ‰ Š ‹ Œ Ž ˜ ™ š › œ ž Ÿ`
        // were lost the same way. 0x81/0x8D/0x8F/0x90/0x9D (and 0x7F) are
        // unused; a font naming WinAnsiEncoding reads them as bullets
        // (`simple_encoding_table`, #1102).
        (0x80, '\u{20AC}'),
        (0x82, '\u{201A}'),
        (0x83, '\u{0192}'),
        (0x84, '\u{201E}'),
        (0x86, '\u{2020}'),
        (0x87, '\u{2021}'),
        (0x88, '\u{02C6}'),
        (0x89, '\u{2030}'),
        (0x8A, '\u{0160}'),
        (0x8B, '\u{2039}'),
        (0x8C, '\u{0152}'),
        (0x8E, '\u{017D}'),
        (0x98, '\u{02DC}'),
        (0x99, '\u{2122}'),
        (0x9A, '\u{0161}'),
        (0x9B, '\u{203A}'),
        (0x9C, '\u{0153}'),
        (0x9E, '\u{017E}'),
        (0x9F, '\u{0178}'),
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
mod glyph_names {
    /// `/Lslash` and the other Latin-1 / Latin Extended-A glyph names a Type1
    /// `/Differences` uses resolve (2203.01017v2's `Łukasz`); the table the
    /// lookup bisects is sorted and the ligature / symbol arms still win.
    #[test]
    fn latin_glyph_names_resolve() {
        for w in super::AGL_LATIN.windows(2) {
            assert!(
                w[0].0 < w[1].0,
                "AGL_LATIN not sorted at {:?} / {:?}",
                w[0].0,
                w[1].0
            );
        }
        for (name, ch) in [
            ("Lslash", 'Ł'),
            ("lslash", 'ł'),
            ("Aacute", 'Á'),
            ("aacute", 'á'),
            ("Scaron", 'Š'),
            ("germandbls", 'ß'),
            ("oe", 'œ'),
            ("Ydieresis", 'Ÿ'),
            ("Euro", '€'),
            ("Lslash.sc", 'Ł'),
            ("fi", '\u{FB01}'),
            ("Omega", '\u{03A9}'),
        ] {
            assert_eq!(
                super::glyph_name_to_char(name.as_bytes()),
                Some(ch),
                "{name}"
            );
        }
        assert_eq!(super::glyph_name_to_char(b"g123"), None);
        // AGL ligature names: components joined with `_`.
        assert_eq!(super::glyph_name_to_char(b"f_f_i"), Some('\u{FB03}'));
        assert_eq!(super::glyph_name_to_char(b"f_l"), Some('\u{FB02}'));
        assert_eq!(super::glyph_name_to_char(b"a_b"), None);
    }

    /// The stale-CMap case: ToUnicode maps a code to its own identity while
    /// `/Differences` names a ligature there — the name wins; a CMap that
    /// maps the code anywhere else, or a code with no entry, is untouched.
    #[test]
    fn a_stale_tounicode_yields_to_differences() {
        use lopdf::Object;
        use std::collections::HashMap;
        let mut tu: HashMap<u32, String> = HashMap::new();
        tu.insert(33, "!".into()); // identity — stale
        tu.insert(34, "x".into()); // the producer meant it
        let diffs = vec![
            Object::Integer(33),
            Object::Name(b"f_f_i".to_vec()),
            Object::Name(b"f_i".to_vec()),
            Object::Name(b"f_f".to_vec()),
            Object::Integer(65),
            Object::Name(b"A".to_vec()), // identity on both sides: nothing to do
        ];
        super::reconcile_stale_tounicode(&mut tu, &diffs);
        assert_eq!(tu.get(&33).map(String::as_str), Some("\u{FB03}"));
        assert_eq!(tu.get(&34).map(String::as_str), Some("x"));
        assert_eq!(tu.get(&35), None);
        assert_eq!(tu.get(&65), None);
    }
}

#[cfg(test)]
mod page_box_frame {
    use super::*;
    use lopdf::dictionary;

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

    /// One page whose `/Contents` is an array of separate streams.
    fn pdf_content_streams(contents: &[&[u8]]) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let streams = contents
            .iter()
            .map(|content| {
                Object::Reference(
                    doc.add_object(lopdf::Stream::new(Dictionary::new(), content.to_vec())),
                )
            })
            .collect::<Vec<_>>();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => Object::Reference(font_id) },
        });
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 400.into()],
            "Contents" => streams,
            "Resources" => Object::Reference(resources_id),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        });
        doc.trailer.set("Root", Object::Reference(catalog_id));
        let mut out = Vec::new();
        doc.save_to(&mut out).expect("save synthetic PDF");
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

    #[test]
    fn leading_comment_content_stream_does_not_hide_later_text() {
        // The comment has its own EOL; lopdf inserts another while assembling
        // the streams, which is the boundary its 0.44 parser stopped at.
        let (_, glyphs) = only_page(&pdf_content_streams(&[
            b"% CANON_PFINF_TYPE2_TEXTOFF\n",
            b"BT /F1 14 Tf 40 300 Td (Second stream text) Tj ET",
        ]));
        assert_eq!(text(&glyphs), "Second stream text");
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
    /// two-byte EOL) used to make lopdf reject the whole file. Lopdf 0.45
    /// accepts it directly, so exercise the fallback itself and prove it still
    /// produces the exact 20-byte entries lopdf 0.44 needed.
    #[test]
    fn short_xref_entries_are_padded_for_fallback() {
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
            lopdf::Document::load_mem(&broken).is_ok(),
            "lopdf 0.45 should accept 19-byte entries"
        );

        let repaired = super::pad_short_xref_entries(&broken).expect("repair short entries");
        assert_eq!(repaired, good, "repair should recreate the valid xref");
        assert!(
            lopdf::Document::load_mem(&repaired).is_ok(),
            "the bytes produced for lopdf 0.44 must load"
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
            cells(&repaired),
            from_good,
            "repair must match the good parse"
        );
    }

    /// The same generator overstates `/Length`. Lopdf 0.45 recovers at
    /// `endstream`, so exercise our fallback directly and prove it still writes
    /// the exact length lopdf 0.44 needed without moving any bytes.
    #[test]
    fn overstated_stream_length_is_fixed_for_fallback() {
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
        // Lopdf 0.45 keeps the stream despite the overstated length.
        let raw = lopdf::Document::load_mem(&broken).expect("still loads");
        assert!(
            raw.get_pages()
                .into_values()
                .any(|p| !raw.get_page_content(p).is_empty()),
            "lopdf 0.45 should recover content at endstream"
        );

        let repaired = super::fix_stream_lengths(&broken);
        assert_eq!(repaired.len(), broken.len(), "repair must preserve offsets");
        assert_eq!(repaired, good, "repair should restore the true length");
        let repaired_doc = lopdf::Document::load_mem(&repaired).expect("repaired PDF loads");
        assert!(
            repaired_doc
                .get_pages()
                .into_values()
                .any(|p| !repaired_doc.get_page_content(p).is_empty()),
            "the bytes produced for lopdf 0.44 must retain page content"
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
        assert_eq!(text(&repaired), expected);
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
    pub(super) fn pdf_with_content(fontdict: &[u8], content: &[u8]) -> Vec<u8> {
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
    pub(super) fn cells(pdf: &[u8]) -> Vec<crate::pdfium_backend::TextCell> {
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

    /// WinAnsiEncoding's unused codes above octal 40 are bullets (PDF
    /// 32000-1 Annex D.2) — ReportLab's default list bullet is `\177` in a
    /// non-embedded WinAnsi Helvetica and read U+FFFD (#1102). A font
    /// without an `/Encoding` keeps them undefined.
    #[test]
    fn winansi_unused_codes_are_bullets() {
        let read = |fontdict: &[u8], text: &[u8]| -> String {
            cells(&pdf_with_font(fontdict, text))
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let winansi = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        assert_eq!(read(winansi, b"\\177 Cut costs"), "\u{2022} Cut costs");
        for code in [0x7Fu8, 0x81, 0x8D, 0x8F, 0x90, 0x9D] {
            assert_eq!(
                read(winansi, &[b'x', code, b'x']),
                "x\u{2022}x",
                "{code:#x}"
            );
        }
        let dict = b"<</Type/Font/Subtype/TrueType/BaseFont/Arial\
                     /Encoding<</Type/Encoding/BaseEncoding/WinAnsiEncoding>>>>";
        assert_eq!(read(dict, b"\\177 Cut costs"), "\u{2022} Cut costs");
        let bare = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>";
        assert!(!read(bare, b"x\\177x").contains('\u{2022}'));
    }

    /// Unmapped glyphs read U+FFFD (#1003, docling#5070): a ToUnicode NUL —
    /// docling's `test_threaded_page_backend_replaces_nul_with_replacement_char`
    /// case, `mg.mL \x00 1` — and a bare glyph-index `/Differences` name,
    /// while an unknown code-like name keeps its text without the slash
    /// (docling-parse 7.24's `SM590000`).
    #[test]
    fn unmapped_glyphs_read_as_the_replacement_char() {
        let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
            1 begincodespacerange <00> <FF> endcodespacerange\n\
            1 beginbfchar <7e> <0000> endbfchar\n\
            endcmap CMapName currentdict /CMap defineresource pop end end";
        let stream = [
            format!("<</Length {}>>stream\n", cmap.len()).as_bytes(),
            cmap.as_slice(),
            b"\nendstream",
        ]
        .concat();
        let pdf = pdf_with_fonts(
            &[
                b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/ToUnicode 6 0 R\
                   /Encoding<</Type/Encoding/Differences[1/g115 2/SM590000]>>>>",
                &stream,
            ],
            b"BT /F1 12 Tf 72 700 Td (mg.mL ~ 1 \x01 x \x02 y) Tj ET\n",
        );
        let text: String = cells(&pdf)
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(text, "mg.mL \u{FFFD} 1 \u{FFFD} x SM590000 y");
    }

    /// redp5110's subset font names every glyph by index (`/g115/g3 /g40…`):
    /// the painted ones read U+FFFD, the blank `/g3` program is a space, so
    /// the line keeps its words (docling-parse 7.24's line cell).
    #[cfg(feature = "render")]
    #[test]
    fn a_blank_glyph_program_is_a_space() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/pdf/sources/redp5110_sampled.pdf");
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let lines: Vec<String> = super::pdf_textlines(&bytes)
            .into_iter()
            .flat_map(|(_, _, c)| c)
            .map(|c| c.text)
            .collect();
        let want = "\u{FFFD} \u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD} \
                    \u{FFFD}\u{FFFD}\u{FFFD} ";
        assert!(
            lines.iter().any(|l| l.starts_with(want)),
            "no `{want}` line in {:?}",
            lines
                .iter()
                .filter(|l| l.contains('\u{FFFD}'))
                .collect::<Vec<_>>()
        );
    }

    /// 2305.03393's figure-caption font (`SFRM0900`) declares `/Ascent 0
    /// /Descent 0`, which docling-parse 7.24 rejects: its glyphs are boxed by
    /// their outline, so the underscores of `"PMC2807444_006_00.png"` — ink
    /// under the baseline — are line cells of their own, as docling-parse's
    /// are (same boxes to 0.01 pt), not part of the file name.
    #[cfg(feature = "render")]
    #[test]
    fn an_invalid_metric_font_is_boxed_by_its_ink() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/pdf/sources/2305.03393v1.pdf");
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let pages = super::pdf_textlines(&bytes);
        let line = |text: &str| {
            pages[9]
                .2
                .iter()
                .find(|c| c.text == text)
                .unwrap_or_else(|| panic!("no `{text}` line cell"))
                .clone()
        };
        let name = line("\"PMC2807444");
        let under: Vec<_> = pages[9].2.iter().filter(|c| c.text == "_").collect();
        assert_eq!(under.len(), 2, "two `_` cells");
        for u in under {
            assert!(
                u.t > name.b,
                "`_` under the digits' ink: {:?} vs {:?}",
                u,
                name
            );
            assert!(
                (u.b - u.t - 0.403).abs() < 0.01,
                "an outline-tight box: {u:?}"
            );
        }
    }

    /// docling-parse's descriptor checks (`init_ascent_and_descent`): metrics
    /// that do not straddle the baseline give way to a valid `/FontBBox` and
    /// mark the font for outline boxes; an implausible one is ignored for its
    /// `/FontBBox` edge; plain metrics are kept.
    #[test]
    fn descriptor_metrics_follow_docling_parse() {
        use lopdf::{dictionary, Object};
        let font = |asc: Option<i64>, desc: Option<i64>, bbox: bool| {
            let mut fd = dictionary! { "Type" => "FontDescriptor", "FontName" => "X" };
            if let Some(a) = asc {
                fd.set("Ascent", a);
            }
            if let Some(d) = desc {
                fd.set("Descent", d);
            }
            if bbox {
                let b: Vec<Object> = [-196, -322, 1502, 937].map(Object::from).to_vec();
                fd.set("FontBBox", b);
            }
            let f = dictionary! {
                "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "X",
                "FontDescriptor" => fd,
            };
            super::font_ascent_descent(&lopdf::Document::with_version("1.5"), &f, false)
        };
        assert_eq!(font(Some(0), Some(0), true), (937.0, -322.0, true));
        assert_eq!(font(Some(700), Some(200), true), (937.0, -322.0, true));
        assert_eq!(font(Some(90000), Some(-250), true), (937.0, -250.0, true));
        assert_eq!(font(Some(718), Some(-207), true), (718.0, -207.0, false));
        assert_eq!(font(Some(0), Some(0), false), (750.0, -250.0, false));
        assert_eq!(font(None, None, true), (750.0, -250.0, false));
    }

    /// redp5110 p. 14 draws its navigation arrows in `SymbolMT`, whose
    /// `/ToUnicode` maps them into the private-use symbol charset
    /// (`U+F0AE`): read through Symbol's encoding they are `→`, as
    /// docling-parse 7.25 reads them (#1113).
    #[test]
    fn symbol_private_use_values_read_as_the_glyphs() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/pdf/sources/redp5110_sampled.pdf");
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let lines: Vec<String> = super::pdf_textlines(&bytes)
            .into_iter()
            .flat_map(|(_, _, c)| c)
            .map(|c| c.text)
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| l.ends_with("click Schemas → HR_SCHEMA→")),
            "{:?}",
            lines
                .iter()
                .filter(|l| l.contains("Schemas"))
                .collect::<Vec<_>>()
        );
        assert!(lines.iter().all(|l| !l.contains('\u{F0AE}')));
    }

    /// 2203.01017's p. 4 table marks its datasets with an embedded Type 1
    /// `Dingbats` — symbolic, no `/Encoding`, no `/ToUnicode` — whose
    /// program encodes code `3` as `/a19` and `7` as `/a23`: `✓` and `✗`
    /// through the builtin encoding, as docling-parse 7.25 reads them
    /// (#1113), where the Latin tables read the digits.
    #[cfg(feature = "render")]
    #[test]
    fn a_symbolic_font_reads_through_its_builtin_encoding() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/pdf/sources/2203.01017v2.pdf");
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let pages = super::pdf_textlines(&bytes);
        let text: Vec<&str> = pages[3].2.iter().map(|c| c.text.as_str()).collect();
        let page = text.join("\n");
        assert_eq!(page.matches('✓').count(), 11, "{text:?}");
        assert_eq!(page.matches('✗').count(), 1, "{text:?}");
        assert!(text.contains(&"✓ ✗ 145k JPEG"), "{text:?}");
    }

    /// docling-parse 7.25's `glyph_name_to_utf8`.
    #[test]
    fn builtin_glyph_names_read_as_docling_parse() {
        let t = |n: &str| super::builtin_glyph_text(n);
        assert_eq!(t("a19").as_deref(), Some("✓"));
        assert_eq!(t("arrowright").as_deref(), Some("→"));
        assert_eq!(t("A").as_deref(), Some("A"));
        assert_eq!(t("one.oldstyle").as_deref(), Some("1"), "suffix dropped");
        assert_eq!(t("uni00660069").as_deref(), Some("fi"), "a uni sequence");
        assert_eq!(t("u1F600").as_deref(), Some("😀"));
        assert_eq!(t("g12"), None, "an index name");
        assert_eq!(t("GID00012"), None);
        assert_eq!(t("cid7"), None);
        assert_eq!(t(".notdef"), None);
        assert_eq!(t(""), None);
        assert_eq!(t("nosuchglyphname"), None);
    }

    /// The page's image placements reach the cells (#999): the scan under
    /// the #999 fixture's one native word is the whole page.
    #[test]
    fn image_placements_are_the_pages_bitmaps() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../docling/tests/fixtures/partial_text_layer/scan_with_native_word.pdf");
        let bytes = std::fs::read(path).expect("fixture");
        let mut parser = super::PageTextParser::open(&bytes).expect("parser");
        let page = parser.cells(0);
        assert_eq!(page.bitmaps, vec![[0.0, 0.0, 612.0, 792.0]]);
        assert_eq!(page.words.len(), 1);
        assert!(page.checkboxes.is_empty());
    }

    /// A `bfrange` whose destination is a surrogate pair steps its last code
    /// unit (#1003: equations.pdf's `H𝛂`, `<1d59> <1d5a> <d835dec2>`); a
    /// single-unit base keeps counting in code points.
    #[test]
    fn surrogate_pair_bfrange_maps_each_code() {
        let map = super::parse_tounicode(
            b"1 beginbfrange <1d59> <1d5a> <d835dec2> endbfrange\n\
              1 beginbfrange <0010> <0011> <0041> endbfrange",
        );
        assert_eq!(map.get(&0x1d59).map(String::as_str), Some("\u{1D6C2}"));
        assert_eq!(map.get(&0x1d5a).map(String::as_str), Some("\u{1D6C3}"));
        assert_eq!(map.get(&0x11).map(String::as_str), Some("B"));
    }

    /// Which unknown glyph names are text and which are bare indexes
    /// (docling-parse's `resolve_unknown_name`; Skia's hexadecimal `g%X`
    /// since 7.25, #1113).
    #[test]
    fn unknown_glyph_names_follow_docling_parse() {
        for name in [
            "g115", "gid00043", "Glyph7", "cid42", "index9", "g3.alt", "g15C", "gA1", "G1F",
        ] {
            assert_eq!(super::unknown_name_text(name), None, "{name}");
            assert!(
                super::is_gid_name(name.split('.').next().unwrap().as_bytes()),
                "{name}"
            );
        }
        assert!(!super::is_gid_name(b"glyphs"), "not hexadecimal");
        assert!(!super::is_gid_name(b"uni0041"));
        assert_eq!(
            super::unknown_name_text("SM590000").as_deref(),
            Some("SM590000")
        );
        assert_eq!(
            super::unknown_name_text("SM590000.sc").as_deref(),
            Some("SM590000")
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

    /// Under `legacy` the font barrier itself is untouched: two *different*
    /// faces (regular and bold Helvetica, which docling-parse names apart) set
    /// flush against each other stay separate line cells, as
    /// `enforce_same_font` demands. docling-parse 7 dropped the barrier — its
    /// line runs follow geometry alone — so under the default `adaptive` the
    /// flush pair is one line and one word.
    #[test]
    fn different_faces_keep_the_font_barrier_only_under_legacy() {
        let f1 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        let f2 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>";
        let content = b"BT 72 720 Td /F2 12 Tf (Total:) Tj /F1 12 Tf (42) Tj ET\n";
        let pdf = pdf_with_fonts(&[f1, f2], content);
        let lines = |gap| {
            let mut p = super::PageTextParser::open(&pdf).expect("opens");
            p.set_word_gap(Some(gap));
            p.cells(0)
                .prose
                .into_iter()
                .map(|c| c.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(crate::dp_lines::WordGap::Legacy), ["Total:", "42"]);
        assert_eq!(lines(crate::dp_lines::WordGap::Adaptive), ["Total:42"]);
    }

    /// #992: a space glyph whose advance the next `TJ` offset takes back —
    /// EUR-Lex's superscript footnote marker `( 1 )`, set with spaces the
    /// digit is then drawn over — is not a character of the text: the page
    /// shows `(1)`, pdftotext and pdfium read `(1)`, and so does the line
    /// cell here (docling-parse keeps the spaces, `( 1 )`). A space the next
    /// glyph only kerns into (−100/1000 em, well past the space's start) is
    /// a real gap and stays, as does one before a jump to the next line.
    #[test]
    fn a_space_the_next_glyph_is_drawn_over_is_not_a_character() {
        let f1 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        let lines = |content: &[u8]| {
            let pdf = pdf_with_content(f1, content);
            super::PageTextParser::open(&pdf)
                .expect("opens")
                .cells(0)
                .prose
                .into_iter()
                .map(|c| c.text)
                .collect::<Vec<_>>()
        };
        // Helvetica's space is 278/1000 em: an offset of 300 moves the pen
        // back past where the space began, 100 leaves two thirds of it.
        assert_eq!(
            lines(b"BT /F1 12 Tf 72 700 Td [(EG \\() ( ) 300 (1) ( ) 300 (\\) erf)] TJ ET\n"),
            ["EG (1) erf"]
        );
        assert_eq!(
            lines(b"BT /F1 12 Tf 72 700 Td [(EG \\() ( ) 100 (1) ( ) 100 (\\) erf)] TJ ET\n"),
            ["EG ( 1 ) erf"]
        );
        // A trailing space, then the next line starts at the left margin.
        assert_eq!(
            lines(b"BT /F1 12 Tf 72 700 Td (first line ) Tj 0 -14 Td (second) Tj ET\n"),
            ["first line", "second"]
        );
        // A line drawn rotated 90° (a landscape slide on a portrait page,
        // #528): its glyphs' loose boxes stack on one left edge, which is
        // not an overprint — the space stays.
        assert_eq!(
            lines(b"BT 0 1 -1 0 300 100 Tm /F1 12 Tf (Nullam mollis) Tj ET\n"),
            ["Nullam mollis"]
        );
    }

    /// A text shadow: a word processor exports a heading with a shadow
    /// effect as the same run drawn twice — the shadow in black, then the
    /// text in colour over it, shifted up and left by a fraction of a point.
    /// pdfium's text page reads the heading once and so does the line cell
    /// here (docling-parse reads `Safety instructions Safety instructions`).
    /// #1092: docling-parse's view of a page's shapes — a stroked straight
    /// rule is a shape line and a shape box, a thin filled box only a box, a
    /// curve or a diagonal no line, a path outside the clip nothing, and a
    /// fill in an unresolved Pattern space nothing at all. In the cells'
    /// frame: top-left, on the 595 × 842 test page.
    #[test]
    fn the_page_shapes_follow_docling_parse() {
        let f1 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        let shapes = |content: &[u8]| {
            let pdf = pdf_with_content(f1, content);
            super::PageTextParser::open(&pdf)
                .expect("opens")
                .cells(0)
                .shapes
        };
        let s = shapes(b"2 w 72 400 m 540 400 l S\n");
        assert_eq!(s.lines, vec![[72.0, 442.0, 540.0, 442.0]]);
        // The stroke's half width pads the box.
        assert_eq!(s.boxes, vec![[71.0, 441.0, 541.0, 443.0]]);

        let s = shapes(b"72 300 468 1 re f\n");
        assert!(s.lines.is_empty(), "a fill strokes no line");
        assert_eq!(s.boxes, vec![[72.0, 541.0, 540.0, 542.0]]);

        // A stroked rectangle: four edges, `h`'s closing one included.
        let s = shapes(b"100 100 200 50 re S\n");
        assert_eq!(s.lines.len(), 4);

        let s = shapes(b"72 400 m 100 450 l 200 300 300 500 400 400 c S\n");
        assert!(s.lines.is_empty(), "neither a diagonal nor a curve");
        assert_eq!(s.boxes.len(), 1);

        let s = shapes(b"q 0 0 300 842 re W n 72 400 m 540 400 l S 400 100 m 400 700 l S Q\n");
        assert_eq!(
            s.lines,
            vec![[72.0, 442.0, 300.0, 442.0]],
            "cut to the clip"
        );
        assert_eq!(s.boxes.len(), 1, "the vertical rule is clipped away");

        let s = shapes(b"/Pattern cs /P0 scn 72 300 468 1 re f\n");
        assert!(s.lines.is_empty() && s.boxes.is_empty());

        // Two touching boxes merge into one component.
        let s = shapes(b"72 300 100 1 re f 172 300 100 1 re f 400 600 10 10 re f\n");
        assert_eq!(
            s.boxes,
            vec![[72.0, 541.0, 272.0, 542.0], [400.0, 232.0, 410.0, 242.0]]
        );

        // A table rule a word processor draws piece by piece, cell borders
        // and their corner squares: `90.144 + 1.44` is `91.584` in the file
        // and in docling-parse's doubles, not in lopdf's `f32` reals cast to
        // `f64`, where every such joint missed by a hair and the rule fell
        // into pieces: `real` reads the operands as the file wrote them. A
        // gap the producer left (`147.5 + 159.86` = 307.36, then 307.37)
        // still parts two components, as in docling-parse.
        let s = shapes(
            b"56.64 690.22 33.504 1.44 re f 90.144 690.22 1.44 1.44 re f \
              91.584 690.22 54.48 1.44 re f 146.06 690.22 1.44 1.44 re f \
              147.5 690.22 159.86 1.44 re f 307.37 690.22 1.44 1.44 re f \
              308.81 690.22 50.304 1.44 re f\n",
        );
        let spans: Vec<(f32, f32)> = s.boxes.iter().map(|b| (b[0], b[2])).collect();
        assert_eq!(spans, vec![(56.64, 307.36), (307.37, 359.114)]);
    }

    /// A transparent stroke or fill is no shape (`CA` / `ca` 0).
    #[test]
    fn transparent_paths_are_no_shapes() {
        let rule = |closed| super::ShapeSub {
            start: (0.0, 0.0),
            cur: (100.0, 0.0),
            pts: vec![(0.0, 0.0), (100.0, 0.0)],
            lines: vec![((0.0, 0.0), (100.0, 0.0))],
            closed,
        };
        let gs = |stroke_alpha, fill_alpha| super::ShapeGs {
            stroke_alpha,
            fill_alpha,
            ..super::ShapeGs::INIT
        };
        let mut shapes = super::Shapes::default();
        super::record_shape(&[rule(false)], true, false, &gs(0.0, 1.0), 1.0, &mut shapes);
        assert_eq!(shapes, super::Shapes::default());
        super::record_shape(&[rule(true)], false, true, &gs(1.0, 0.0), 1.0, &mut shapes);
        assert_eq!(shapes, super::Shapes::default());
        super::record_shape(&[rule(false)], true, false, &gs(0.5, 0.0), 1.0, &mut shapes);
        assert_eq!(shapes.lines.len(), 1);
    }

    /// Text that really repeats sits an advance apart or more and stays: a
    /// word set twice, a doubled letter, the same words on the next line.
    #[test]
    fn a_run_drawn_again_over_itself_is_read_once() {
        let f1 = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica-Bold/Encoding/WinAnsiEncoding>>";
        let lines = |content: &[u8]| {
            let pdf = pdf_with_content(f1, content);
            super::PageTextParser::open(&pdf)
                .expect("opens")
                .cells(0)
                .prose
                .into_iter()
                .map(|c| c.text)
                .collect::<Vec<_>>()
        };
        // The shadow, then the text 0.5 pt up and left of it, at 18 pt.
        assert_eq!(
            lines(
                b"BT 0 g /F1 18 Tf 1 0 0 1 72 720 Tm (Safety instructions) Tj ET\n\
                  BT 0.8 0 0 rg /F1 18 Tf 1 0 0 1 71.5 720.5 Tm (Safety instructions) Tj ET\n"
            ),
            ["Safety instructions"]
        );
        // 0.8 pt at 14 pt, and the line under it untouched.
        assert_eq!(
            lines(
                b"BT /F1 14 Tf 1 0 0 1 72 656 Tm (Maintenance) Tj ET\n\
                  BT /F1 14 Tf 1 0 0 1 71.2 656.8 Tm (Maintenance) Tj ET\n\
                  BT /F1 11 Tf 1 0 0 1 72 632 Tm (Clean the filter) Tj ET\n"
            ),
            ["Maintenance", "Clean the filter"]
        );
        // A word set twice, a doubled letter, the same words a line apart.
        assert_eq!(
            lines(b"BT /F1 12 Tf 72 700 Td (very very small) Tj ET\n"),
            ["very very small"]
        );
        assert_eq!(
            lines(b"BT /F1 12 Tf 72 700 Td (Hello, all) Tj ET\n"),
            ["Hello, all"]
        );
        assert_eq!(
            lines(b"BT /F1 12 Tf 72 700 Td (Total) Tj 0 -14 Td (Total) Tj ET\n"),
            ["Total", "Total"]
        );
        // Dot leaders squeezed by `Tc` to 0.05, 0.07 and 0.1 em: two dots on,
        // a dot sits within a shadow's shift and the dots after it repeat by
        // the same offset, but the next dot is nearer still.
        for tc in ["-2.28", "-2.08", "-1.78"] {
            let content = format!(
                "BT /F1 10 Tf 72 700 Td (Contents) Tj {tc} Tc (..........) Tj 0 Tc ( 5) Tj ET\n"
            );
            assert_eq!(
                lines(content.as_bytes()),
                ["Contents.......... 5"],
                "Tc {tc}"
            );
        }
    }

    /// docling-parse's `test_unit_actual_text.py` (docling-parse#300): a
    /// marked-content span's `/ActualText` replaces the glyphs drawn in it.
    #[test]
    fn actual_text_replaces_the_glyphs_of_its_span() {
        let helvetica = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica/Encoding/WinAnsiEncoding>>";
        // A non-embedded TrueType whose only code, 0x4D (`M` in WinAnsi), is a
        // zero-advance caron: drawn after `s` it makes `š`.
        let caron = b"<</Type/Font/Subtype/TrueType/BaseFont/SynthCaron/FirstChar 77/LastChar 77\
            /Widths[0]/Encoding/WinAnsiEncoding/FontDescriptor<</Type/FontDescriptor\
            /FontName/SynthCaron/Flags 32/FontBBox[-540 -200 0 800]/ItalicAngle 0/Ascent 800\
            /Descent -200/CapHeight 700/StemV 80>>>>";
        // Code 1 is the `fi` ligature: two glyphs on one pen, one cell.
        let ligature = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica\
            /Encoding<</Type/Encoding/BaseEncoding/WinAnsiEncoding/Differences[1/fi]>>>>";
        let text = |content: &str| {
            let pdf = pdf_with_fonts(&[helvetica, caron, ligature], content.as_bytes());
            super::PageTextParser::open(&pdf)
                .expect("opens")
                .cells(0)
                .prose
                .into_iter()
                .map(|c| c.text)
                .collect::<String>()
        };
        // Unmarked, the caron reads as its WinAnsi letter.
        assert_eq!(
            text("BT /F1 24 Tf 40 50 Td (Ore) Tj (s) Tj /F2 24 Tf <4D> Tj /F1 24 Tf (i) Tj ET"),
            "OresMi"
        );
        // Two cells, one character: the accent composes onto its base letter.
        assert_eq!(
            text(
                "BT /F1 24 Tf 40 50 Td (Ore) Tj /Span <</ActualText <FEFF0161>>> BDC \
                  (s) Tj /F2 24 Tf <4D> Tj EMC /F1 24 Tf (i) Tj ET"
            ),
            "Oreši"
        );
        // As many characters as cells: one for one.
        assert_eq!(
            text("BT /F1 24 Tf 40 50 Td /Span <</ActualText (ab)>> BDC (xy) Tj EMC (z) Tj ET"),
            "abz"
        );
        // A cell of two glyphs takes one character.
        assert_eq!(
            text("BT /F3 24 Tf 40 50 Td <01> Tj /F1 24 Tf (a) Tj ET"),
            "fia"
        );
        assert_eq!(
            text(
                "BT /F3 24 Tf 40 50 Td /Span <</ActualText (XY)>> BDC <01> Tj \
                  /F1 24 Tf (a) Tj EMC (z) Tj ET"
            ),
            "XYz"
        );
        // An empty text declares the glyphs not text (a visual line-break
        // hyphen); its advance may still read as a gap.
        let hyphen = text(
            "BT /F1 24 Tf 40 50 Td (exam) Tj /Span <</ActualText <FEFF>>> BDC (-) Tj EMC \
             (ple) Tj ET",
        );
        assert!(!hyphen.contains('-'), "{hyphen}");
        assert_eq!(hyphen.replace(' ', ""), "example");
        // `/Alt` written into the wrong key: implausibly long, ignored.
        assert_eq!(
            text(
                "BT /F1 24 Tf 40 50 Td /Span <</ActualText (A long description that belongs \
                  in Alt not here)>> BDC (ab) Tj EMC ET"
            ),
            "ab"
        );
        // Over graphics only: nothing to replace.
        assert_eq!(
            text(
                "BT /F1 24 Tf 40 50 Td (x) Tj ET /Span <</ActualText (ghost)>> BDC \
                  10 10 50 50 re f EMC"
            ),
            "x"
        );
        // A named property list is not resolved.
        assert_eq!(
            text("BT /F1 24 Tf 40 50 Td /OC /MC0 BDC (ok) Tj EMC ET"),
            "ok"
        );
        // Nested spans: the outer text describes the whole run.
        assert_eq!(
            text(
                "BT /F1 24 Tf 40 50 Td /Span <</ActualText (AB)>> BDC (a) Tj \
                  /Span <</ActualText (Z)>> BDC (b) Tj EMC EMC ET"
            ),
            "AB"
        );
        // A span nested past the depth limit keeps its glyphs.
        let nested = |depth: usize| {
            text(&format!(
                "BT /F1 24 Tf 40 50 Td {}/Span <</ActualText (z)>> BDC (a) Tj EMC{} ET",
                "/X BMC ".repeat(depth),
                " EMC".repeat(depth)
            ))
        };
        assert_eq!(nested(super::MAX_ACTUAL_TEXT_DEPTH - 1), "z");
        assert_eq!(nested(super::MAX_ACTUAL_TEXT_DEPTH), "a");
    }

    /// One for one, each character keeps its cell's box, and a cell of
    /// several glyphs on one pen (a ligature's) becomes its first glyph.
    #[test]
    fn actual_text_one_for_one_keeps_each_cells_box() {
        let glyph = |ch, x: f32, pen: f32| super::Glyph {
            ch,
            l: x,
            b: 0.0,
            r: x + 5.0,
            t: 10.0,
            ll: x,
            lb: -2.0,
            lr: x + 5.0,
            lt: 10.0,
            font: 0,
            quad: None,
            pen: Some([pen, 0.0, 10.0]),
            math_font: false,
            invisible: false,
        };
        let mut out = vec![
            glyph('o', 0.0, 0.0),
            glyph('f', 10.0, 10.0),
            glyph('i', 15.0, 10.0),
            glyph('a', 20.0, 20.0),
        ];
        super::apply_actual_text(&mut out, 1, "XY");
        let got: Vec<(char, f32)> = out.iter().map(|g| (g.ch, g.l)).collect();
        assert_eq!(got, [('o', 0.0), ('X', 10.0), ('Y', 20.0)]);
    }

    /// The identity follows docling-parse's precedence: a `/FontDescriptor/FontName`
    /// wins over `/BaseFont`, so two dictionaries that share a `/BaseFont` but
    /// describe different programs are two fonts, and two that share the
    /// descriptor name are one whatever their `/BaseFont` says.
    #[test]
    fn math_faces_are_named_like_docling_parse_names_them() {
        for name in [
            "ABCDEF+CMR7",
            "CMMI10",
            "MSBM10",
            "Cambria-Math",
            "Symbol",
            "MTExtra",
        ] {
            assert!(super::is_math_font_name(name.as_bytes()), "{name}");
        }
        for name in ["Helvetica", "ABCDEF+TimesNewRomanPSMT", "Arial-BoldMT"] {
            assert!(!super::is_math_font_name(name.as_bytes()), "{name}");
        }
    }

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

/// #994: decoded Form XObject content is cached per document only from a
/// form's second sighting, so one-form-per-page documents do not hold every
/// page's decoded form until the document ends.
#[cfg(test)]
mod form_cache {
    use lopdf::content::{Content, Operation};
    use lopdf::{dictionary, Document, Object, Stream};

    fn form(doc: &mut Document, text: &str) -> lopdf::ObjectId {
        let ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Td", vec![40.into(), 500.into()]),
            Operation::new("Tj", vec![Object::string_literal(text)]),
            Operation::new("ET", vec![]),
        ];
        let data = Content { operations: ops }.encode().unwrap();
        doc.add_object(Stream::new(
            dictionary! { "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 595.into(), 842.into()] },
            data,
        ))
    }

    #[test]
    fn single_use_forms_are_not_retained_shared_ones_are() {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font = doc.add_object(
            dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" },
        );
        let header = form(&mut doc, "Header");
        let mut kids = Vec::new();
        let mut own_forms = Vec::new();
        for p in 0..3 {
            let own = form(&mut doc, &format!("Body{p}"));
            own_forms.push(own);
            let content = doc.add_object(Stream::new(
                dictionary! {},
                b"q /Hd Do Q q /Fx Do Q".to_vec(),
            ));
            kids.push(Object::Reference(doc.add_object(dictionary! {
                "Type" => "Page", "Parent" => pages_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Contents" => content,
                "Resources" => dictionary! {
                    "Font" => dictionary! { "F1" => font },
                    "XObject" => dictionary! { "Hd" => header, "Fx" => own },
                },
            })));
        }
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 3 }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);

        let mut caches = super::DocCaches::default();
        let mut pages: Vec<_> = doc.get_pages().into_iter().collect();
        pages.sort_by_key(|(n, _)| *n);
        for (i, (_, pid)) in pages.iter().enumerate() {
            let text: String = super::page_glyphs_cached(&doc, *pid, &mut caches)
                .iter()
                .map(|g| g.ch)
                .collect();
            // Both forms' text is still read, cached or not.
            assert!(text.contains("Header"), "page {i}: {text:?}");
            assert!(text.contains(&format!("Body{i}")), "page {i}: {text:?}");
        }
        // The shared header form is cached from its second page on; the
        // per-page forms, each used once, are not held.
        assert!(caches.forms.contains_key(&header));
        for own in &own_forms {
            assert!(!caches.forms.contains_key(own));
        }
        assert_eq!(caches.forms.len(), 1);
    }
}

#[cfg(test)]
mod cid_collections {
    use super::base14_fonts::{cells, pdf_with_content};

    fn type0(encoding: &str) -> Vec<u8> {
        format!(
            "<</Type/Font/Subtype/Type0/BaseFont/Test-Font-Light/Encoding/{encoding}\
             /DescendantFonts[<</Type/Font/Subtype/CIDFontType0/BaseFont/Test-Font-Light\
             /CIDSystemInfo<</Registry(Adobe)/Ordering(Japan1)/Supplement 7>>/DW 1000>>]>>"
        )
        .into_bytes()
    }

    fn text(fontdict: &[u8], hex: &str) -> String {
        let content = format!("BT /F1 12 Tf 72 700 Td <{hex}> Tj ET\n");
        cells(&pdf_with_content(fontdict, content.as_bytes()))
            .iter()
            .map(|c| c.text.as_str())
            .collect()
    }

    #[test]
    #[cfg(feature = "cid-collections")]
    fn identity_codes_read_through_the_collection() {
        assert_eq!(text(&type0("Identity-H"), "0CD40E8A07A0"), "日本語");
    }

    #[test]
    fn unicode_keyed_cmaps_read_the_code() {
        assert_eq!(text(&type0("UniJIS-UCS2-H"), "65E5672C8A9E"), "日本語");
    }

    #[test]
    #[cfg(feature = "cid-collections")]
    fn collection_ranges_keep_supplementary_characters() {
        assert_eq!(text(&type0("Identity-H"), "0CD44507450807A0"), "日𢭏𢭐語");
        let text = |o: &[u8], cid| super::collection_text(super::collection_table(o).unwrap(), cid);
        assert_eq!(text(b"Japan1", 0x2714).as_deref(), Some("🄐"));
        assert_eq!(text(b"CNS1", 0x3a32).as_deref(), Some("𡤃"));
    }

    #[test]
    #[cfg(feature = "cid-collections")]
    fn collection_entries_are_plain_characters() {
        assert_eq!(text(&type0("Identity-H"), "046D"), "逢");
        let text = |o: &[u8], cid| super::collection_text(super::collection_table(o).unwrap(), cid);
        assert_eq!(text(b"Korea1", 8752), None);
        assert_eq!(text(b"KR", 11704), None);
        assert_eq!(text(b"Japan1", 0x1ffd).as_deref(), Some("sec"));
    }

    #[test]
    fn tounicode_ranges_increment_the_utf16_string() {
        let map = super::parse_tounicode(
            b"3 beginbfrange\n\
              <01> <02> <d842dfb7>\n\
              <03> <04> <00660069>\n\
              <05> <06> <0061>\n\
              endbfrange",
        );
        assert_eq!(map[&1], "𠮷");
        assert_eq!(map[&2], "𠮸");
        assert_eq!(map[&3], "fi");
        assert_eq!(map[&4], "fj");
        assert_eq!(map[&5], "a");
        assert_eq!(map[&6], "b");
    }

    #[test]
    fn tounicode_skips_lone_surrogates_overflows_and_huge_ranges() {
        let map = super::parse_tounicode(
            b"2 beginbfchar\n<01> <d842>\n<02> <0041>\nendbfchar\n\
              4 beginbfrange\n\
              <10> <11> [<dc00> <0042>]\n\
              <20> <22> <0041fffe>\n\
              <30> <31> <d842dfff>\n\
              <00010000> <ffffffff> <>\n\
              endbfrange",
        );
        assert!(!map.contains_key(&0x01));
        assert_eq!(map[&0x02], "A");
        assert!(!map.contains_key(&0x10));
        assert_eq!(map[&0x11], "B");
        assert_eq!(map[&0x20], "A\u{fffe}");
        assert_eq!(map[&0x21], "A\u{ffff}");
        assert!(!map.contains_key(&0x22));
        assert_eq!(map[&0x30], "\u{20bff}");
        assert!(!map.contains_key(&0x31));
        assert_eq!(map[&0x1ffff], "");
        assert_eq!(map.len(), 5 + 0x10000);
    }

    #[test]
    fn utf16_pairs_are_one_glyph_and_one_advance() {
        let font = type0("UniJIS-UTF16-H");
        assert_eq!(text(&font, "65E5D842DFB78A9E"), "日𠮷語");
        let content = b"BT /F1 12 Tf 72 700 Td <65E5D842DFB78A9E> Tj ET\n";
        let pdf = pdf_with_content(&font, content);
        let doc = lopdf::Document::load_mem(&pdf).unwrap();
        let glyphs = super::page_glyphs(&doc, doc.get_pages()[&1]);
        assert_eq!(glyphs.len(), 3);
        assert!((glyphs[2].l - glyphs[0].l - 24.0).abs() < 0.001);
    }

    #[test]
    fn utf16_codes_preserve_tounicode_keys_and_identity_cids() {
        use lopdf::{dictionary, Document};
        let doc = Document::with_version("1.4");
        let dict = dictionary! {"Subtype" => "Type0", "Encoding" => "UniJIS-UTF16-H"};
        let mut font = super::parse_font(&doc, b"F1", &dict);
        assert_eq!(
            super::codes(&font, &[0xd8, 0x42, 0xdf, 0xb7]),
            vec![0xd842dfb7]
        );
        font.to_unicode.insert(0xd842dfb7, "override".into());
        assert_eq!(font.decode_code(0xd842dfb7).0.as_deref(), Some("override"));
        assert_eq!(
            super::codes(&font, &[0xd8, 0x42, 0x65, 0xe5]),
            vec![0xd842, 0x65e5]
        );
        let dict = dictionary! {"Subtype" => "Type0", "Encoding" => "Identity-H"};
        let font = super::parse_font(&doc, b"F1", &dict);
        assert_eq!(
            super::codes(&font, &[0xd8, 0x42, 0xdf, 0xb7]),
            vec![0xd842, 0xdfb7]
        );
    }
}
