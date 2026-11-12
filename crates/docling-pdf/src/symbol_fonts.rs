//! docling-parse 7.25's reading of the two symbol core fonts (docling-parse#431,
//! docling#1113): Symbol's and ZapfDingbats' built-in encodings as text.
//!
//! Office exporters embed Symbol (as `SymbolMT`) and ZapfDingbats with a
//! `/ToUnicode` that sends every glyph to the "symbol charset" private-use
//! block, `U+F000` plus the byte code, so a bullet, an arrow, a Greek letter
//! reaches the text layer as an invisible private-use character
//! (redp5110's `U+F0AE` for `→`). docling-parse 7.25 folds such a value back
//! through the font's own encoding when the font is one of these two
//! (`fold_symbol_font_private_use`); every other font keeps the value, since
//! CJK gaiji and Arabic presentation fonts use the same block for real text,
//! and Wingdings — no core font — keeps its private-use codes (the list
//! bullets [`crate::assemble`] strips).
//!
//! Data: the Adobe Core 14 AFM encodings of `Symbol` and `ZapfDingbats`
//! (redistributable per Adobe's AFM license), each glyph name read through
//! docling-parse's glyph tables (the Adobe Glyph List, BSD 3-Clause, and its
//! `zapfdingbats.txt`, first entry wins: `additional`, `glyphlist`,
//! `zapfdingbats`, `scripts`, `missing`), with docling-parse's rewrite of the
//! Glyph List's legacy private-use delimiter pieces to Unicode 3.2's
//! (`bracerighttp` U+F8FC → `⎫` U+23AB, `parenlefttp` → `⎛`, …).

/// One of the two core symbol fonts, as docling-parse matches a font to its
/// base fonts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SymbolFace {
    Symbol,
    ZapfDingbats,
}

/// The core symbol font a font name resolves to: docling-parse's
/// `matched_base_font` — the descriptor's font name first (`font_name`, see
/// `sanitizer_font_name`), then `/BaseFont` — lowercased, matched by the
/// longest built-in font name it contains (`base_fonts::find_corresponding_font`),
/// so `ABCDEF+SymbolMT` and `OpenSymbol` are Symbol. Only the two symbol
/// faces' names are looked for: a descriptor name matching some other
/// built-in face (`CMSY10`) would stop docling-parse before `/BaseFont`, a
/// combination no corpus font has.
pub(crate) fn symbol_face(names: &[&[u8]]) -> Option<SymbolFace> {
    names.iter().find_map(|n| {
        let lower = n.to_ascii_lowercase();
        let has = |t: &str| lower.windows(t.len()).any(|w| w == t.as_bytes());
        if has("zapfdingbats") {
            Some(SymbolFace::ZapfDingbats)
        } else if has("symbol") {
            Some(SymbolFace::Symbol)
        } else {
            None
        }
    })
}

/// The text the face's built-in encoding gives `code`, if it defines it.
pub(crate) fn face_text(face: SymbolFace, code: u8) -> Option<char> {
    let table: &[(u8, char)] = match face {
        SymbolFace::Symbol => &SYMBOL_TEXT,
        SymbolFace::ZapfDingbats => &ZAPF_DINGBATS_TEXT,
    };
    table
        .binary_search_by_key(&code, |&(c, _)| c)
        .ok()
        .map(|i| table[i].1)
}

/// docling-parse's `fold_symbol_font_private_use`: a `/ToUnicode` value that
/// is one code point in `U+F020..=U+F0FF` whose low byte the face encodes
/// reads as that glyph's text; anything else is left as it is.
pub(crate) fn fold_private_use(face: SymbolFace, value: &str) -> Option<char> {
    let mut chars = value.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !('\u{F020}'..='\u{F0FF}').contains(&c) {
        return None;
    }
    face_text(face, (c as u32 - 0xF000) as u8)
}

#[rustfmt::skip]
const SYMBOL_TEXT: [(u8, char); 189] = [
    (0x20, '\u{0020}'), (0x21, '\u{0021}'), (0x22, '\u{2200}'), (0x23, '\u{0023}'), (0x24, '\u{2203}'), (0x25, '\u{0025}'),
    (0x26, '\u{0026}'), (0x27, '\u{220B}'), (0x28, '\u{0028}'), (0x29, '\u{0029}'), (0x2A, '\u{2217}'), (0x2B, '\u{002B}'),
    (0x2C, '\u{002C}'), (0x2D, '\u{2212}'), (0x2E, '\u{002E}'), (0x2F, '\u{002F}'), (0x30, '\u{0030}'), (0x31, '\u{0031}'),
    (0x32, '\u{0032}'), (0x33, '\u{0033}'), (0x34, '\u{0034}'), (0x35, '\u{0035}'), (0x36, '\u{0036}'), (0x37, '\u{0037}'),
    (0x38, '\u{0038}'), (0x39, '\u{0039}'), (0x3A, '\u{003A}'), (0x3B, '\u{003B}'), (0x3C, '\u{003C}'), (0x3D, '\u{003D}'),
    (0x3E, '\u{003E}'), (0x3F, '\u{003F}'), (0x40, '\u{2245}'), (0x41, '\u{0391}'), (0x42, '\u{0392}'), (0x43, '\u{03A7}'),
    (0x44, '\u{2206}'), (0x45, '\u{0395}'), (0x46, '\u{03A6}'), (0x47, '\u{0393}'), (0x48, '\u{0397}'), (0x49, '\u{0399}'),
    (0x4A, '\u{03D1}'), (0x4B, '\u{039A}'), (0x4C, '\u{039B}'), (0x4D, '\u{039C}'), (0x4E, '\u{039D}'), (0x4F, '\u{039F}'),
    (0x50, '\u{03A0}'), (0x51, '\u{0398}'), (0x52, '\u{03A1}'), (0x53, '\u{03A3}'), (0x54, '\u{03A4}'), (0x55, '\u{03A5}'),
    (0x56, '\u{03C2}'), (0x57, '\u{2126}'), (0x58, '\u{039E}'), (0x59, '\u{03A8}'), (0x5A, '\u{0396}'), (0x5B, '\u{005B}'),
    (0x5C, '\u{2234}'), (0x5D, '\u{005D}'), (0x5E, '\u{22A5}'), (0x5F, '\u{005F}'), (0x60, '\u{F8E5}'), (0x61, '\u{03B1}'),
    (0x62, '\u{03B2}'), (0x63, '\u{03C7}'), (0x64, '\u{03B4}'), (0x65, '\u{03B5}'), (0x66, '\u{03C6}'), (0x67, '\u{03B3}'),
    (0x68, '\u{03B7}'), (0x69, '\u{03B9}'), (0x6A, '\u{03D5}'), (0x6B, '\u{03BA}'), (0x6C, '\u{03BB}'), (0x6D, '\u{00B5}'),
    (0x6E, '\u{03BD}'), (0x6F, '\u{03BF}'), (0x70, '\u{03C0}'), (0x71, '\u{03B8}'), (0x72, '\u{03C1}'), (0x73, '\u{03C3}'),
    (0x74, '\u{03C4}'), (0x75, '\u{03C5}'), (0x76, '\u{03D6}'), (0x77, '\u{03C9}'), (0x78, '\u{03BE}'), (0x79, '\u{03C8}'),
    (0x7A, '\u{03B6}'), (0x7B, '\u{007B}'), (0x7C, '\u{007C}'), (0x7D, '\u{007D}'), (0x7E, '\u{223C}'), (0xA0, '\u{20AC}'),
    (0xA1, '\u{03D2}'), (0xA2, '\u{2032}'), (0xA3, '\u{2264}'), (0xA4, '\u{2044}'), (0xA5, '\u{221E}'), (0xA6, '\u{0192}'),
    (0xA7, '\u{2663}'), (0xA8, '\u{2666}'), (0xA9, '\u{2665}'), (0xAA, '\u{2660}'), (0xAB, '\u{2194}'), (0xAC, '\u{2190}'),
    (0xAD, '\u{2191}'), (0xAE, '\u{2192}'), (0xAF, '\u{2193}'), (0xB0, '\u{00B0}'), (0xB1, '\u{00B1}'), (0xB2, '\u{2033}'),
    (0xB3, '\u{2265}'), (0xB4, '\u{00D7}'), (0xB5, '\u{221D}'), (0xB6, '\u{2202}'), (0xB7, '\u{2022}'), (0xB8, '\u{00F7}'),
    (0xB9, '\u{2260}'), (0xBA, '\u{2261}'), (0xBB, '\u{2248}'), (0xBC, '\u{2026}'), (0xBD, '\u{23D0}'), (0xBE, '\u{23AF}'),
    (0xBF, '\u{21B5}'), (0xC0, '\u{2135}'), (0xC1, '\u{2111}'), (0xC2, '\u{211C}'), (0xC3, '\u{2118}'), (0xC4, '\u{2297}'),
    (0xC5, '\u{2295}'), (0xC6, '\u{2205}'), (0xC7, '\u{2229}'), (0xC8, '\u{222A}'), (0xC9, '\u{2283}'), (0xCA, '\u{2287}'),
    (0xCB, '\u{2284}'), (0xCC, '\u{2282}'), (0xCD, '\u{2286}'), (0xCE, '\u{2208}'), (0xCF, '\u{2209}'), (0xD0, '\u{2220}'),
    (0xD1, '\u{2207}'), (0xD2, '\u{F6DA}'), (0xD3, '\u{F6D9}'), (0xD4, '\u{F6DB}'), (0xD5, '\u{220F}'), (0xD6, '\u{221A}'),
    (0xD7, '\u{22C5}'), (0xD8, '\u{00AC}'), (0xD9, '\u{2227}'), (0xDA, '\u{2228}'), (0xDB, '\u{21D4}'), (0xDC, '\u{21D0}'),
    (0xDD, '\u{21D1}'), (0xDE, '\u{21D2}'), (0xDF, '\u{21D3}'), (0xE0, '\u{25CA}'), (0xE1, '\u{2329}'), (0xE2, '\u{F8E8}'),
    (0xE3, '\u{F8E9}'), (0xE4, '\u{F8EA}'), (0xE5, '\u{2211}'), (0xE6, '\u{239B}'), (0xE7, '\u{239C}'), (0xE8, '\u{239D}'),
    (0xE9, '\u{23A1}'), (0xEA, '\u{23A2}'), (0xEB, '\u{23A3}'), (0xEC, '\u{23A7}'), (0xED, '\u{23A8}'), (0xEE, '\u{23A9}'),
    (0xEF, '\u{23AA}'), (0xF1, '\u{232A}'), (0xF2, '\u{222B}'), (0xF3, '\u{2320}'), (0xF4, '\u{23AE}'), (0xF5, '\u{2321}'),
    (0xF6, '\u{239E}'), (0xF7, '\u{239F}'), (0xF8, '\u{23A0}'), (0xF9, '\u{23A4}'), (0xFA, '\u{23A5}'), (0xFB, '\u{23A6}'),
    (0xFC, '\u{23AB}'), (0xFD, '\u{23AC}'), (0xFE, '\u{23AD}'),
];
#[rustfmt::skip]
const ZAPF_DINGBATS_TEXT: [(u8, char); 202] = [
    (0x20, '\u{0020}'), (0x21, '\u{2701}'), (0x22, '\u{2702}'), (0x23, '\u{2703}'), (0x24, '\u{2704}'), (0x25, '\u{260E}'),
    (0x26, '\u{2706}'), (0x27, '\u{2707}'), (0x28, '\u{2708}'), (0x29, '\u{2709}'), (0x2A, '\u{261B}'), (0x2B, '\u{261E}'),
    (0x2C, '\u{270C}'), (0x2D, '\u{270D}'), (0x2E, '\u{270E}'), (0x2F, '\u{270F}'), (0x30, '\u{2710}'), (0x31, '\u{2711}'),
    (0x32, '\u{2712}'), (0x33, '\u{2713}'), (0x34, '\u{2714}'), (0x35, '\u{2715}'), (0x36, '\u{2716}'), (0x37, '\u{2717}'),
    (0x38, '\u{2718}'), (0x39, '\u{2719}'), (0x3A, '\u{271A}'), (0x3B, '\u{271B}'), (0x3C, '\u{271C}'), (0x3D, '\u{271D}'),
    (0x3E, '\u{271E}'), (0x3F, '\u{271F}'), (0x40, '\u{2720}'), (0x41, '\u{2721}'), (0x42, '\u{2722}'), (0x43, '\u{2723}'),
    (0x44, '\u{2724}'), (0x45, '\u{2725}'), (0x46, '\u{2726}'), (0x47, '\u{2727}'), (0x48, '\u{2605}'), (0x49, '\u{2729}'),
    (0x4A, '\u{272A}'), (0x4B, '\u{272B}'), (0x4C, '\u{272C}'), (0x4D, '\u{272D}'), (0x4E, '\u{272E}'), (0x4F, '\u{272F}'),
    (0x50, '\u{2730}'), (0x51, '\u{2731}'), (0x52, '\u{2732}'), (0x53, '\u{2733}'), (0x54, '\u{2734}'), (0x55, '\u{2735}'),
    (0x56, '\u{2736}'), (0x57, '\u{2737}'), (0x58, '\u{2738}'), (0x59, '\u{2739}'), (0x5A, '\u{273A}'), (0x5B, '\u{273B}'),
    (0x5C, '\u{273C}'), (0x5D, '\u{273D}'), (0x5E, '\u{273E}'), (0x5F, '\u{273F}'), (0x60, '\u{2740}'), (0x61, '\u{2741}'),
    (0x62, '\u{2742}'), (0x63, '\u{2743}'), (0x64, '\u{2744}'), (0x65, '\u{2745}'), (0x66, '\u{2746}'), (0x67, '\u{2747}'),
    (0x68, '\u{2748}'), (0x69, '\u{2749}'), (0x6A, '\u{274A}'), (0x6B, '\u{274B}'), (0x6C, '\u{25CF}'), (0x6D, '\u{274D}'),
    (0x6E, '\u{25A0}'), (0x6F, '\u{274F}'), (0x70, '\u{2750}'), (0x71, '\u{2751}'), (0x72, '\u{2752}'), (0x73, '\u{25B2}'),
    (0x74, '\u{25BC}'), (0x75, '\u{25C6}'), (0x76, '\u{2756}'), (0x77, '\u{25D7}'), (0x78, '\u{2758}'), (0x79, '\u{2759}'),
    (0x7A, '\u{275A}'), (0x7B, '\u{275B}'), (0x7C, '\u{275C}'), (0x7D, '\u{275D}'), (0x7E, '\u{275E}'), (0x80, '\u{2768}'),
    (0x81, '\u{2769}'), (0x82, '\u{276A}'), (0x83, '\u{276B}'), (0x84, '\u{276C}'), (0x85, '\u{276D}'), (0x86, '\u{276E}'),
    (0x87, '\u{276F}'), (0x88, '\u{2770}'), (0x89, '\u{2771}'), (0x8A, '\u{2772}'), (0x8B, '\u{2773}'), (0x8C, '\u{2774}'),
    (0x8D, '\u{2775}'), (0xA1, '\u{2761}'), (0xA2, '\u{2762}'), (0xA3, '\u{2763}'), (0xA4, '\u{2764}'), (0xA5, '\u{2765}'),
    (0xA6, '\u{2766}'), (0xA7, '\u{2767}'), (0xA8, '\u{2663}'), (0xA9, '\u{2666}'), (0xAA, '\u{2665}'), (0xAB, '\u{2660}'),
    (0xAC, '\u{2460}'), (0xAD, '\u{2461}'), (0xAE, '\u{2462}'), (0xAF, '\u{2463}'), (0xB0, '\u{2464}'), (0xB1, '\u{2465}'),
    (0xB2, '\u{2466}'), (0xB3, '\u{2467}'), (0xB4, '\u{2468}'), (0xB5, '\u{2469}'), (0xB6, '\u{2776}'), (0xB7, '\u{2777}'),
    (0xB8, '\u{2778}'), (0xB9, '\u{2779}'), (0xBA, '\u{277A}'), (0xBB, '\u{277B}'), (0xBC, '\u{277C}'), (0xBD, '\u{277D}'),
    (0xBE, '\u{277E}'), (0xBF, '\u{277F}'), (0xC0, '\u{2780}'), (0xC1, '\u{2781}'), (0xC2, '\u{2782}'), (0xC3, '\u{2783}'),
    (0xC4, '\u{2784}'), (0xC5, '\u{2785}'), (0xC6, '\u{2786}'), (0xC7, '\u{2787}'), (0xC8, '\u{2788}'), (0xC9, '\u{2789}'),
    (0xCA, '\u{278A}'), (0xCB, '\u{278B}'), (0xCC, '\u{278C}'), (0xCD, '\u{278D}'), (0xCE, '\u{278E}'), (0xCF, '\u{278F}'),
    (0xD0, '\u{2790}'), (0xD1, '\u{2791}'), (0xD2, '\u{2792}'), (0xD3, '\u{2793}'), (0xD4, '\u{2794}'), (0xD5, '\u{2192}'),
    (0xD6, '\u{2194}'), (0xD7, '\u{2195}'), (0xD8, '\u{2798}'), (0xD9, '\u{2799}'), (0xDA, '\u{279A}'), (0xDB, '\u{279B}'),
    (0xDC, '\u{279C}'), (0xDD, '\u{279D}'), (0xDE, '\u{279E}'), (0xDF, '\u{279F}'), (0xE0, '\u{27A0}'), (0xE1, '\u{27A1}'),
    (0xE2, '\u{27A2}'), (0xE3, '\u{27A3}'), (0xE4, '\u{27A4}'), (0xE5, '\u{27A5}'), (0xE6, '\u{27A6}'), (0xE7, '\u{27A7}'),
    (0xE8, '\u{27A8}'), (0xE9, '\u{27A9}'), (0xEA, '\u{27AA}'), (0xEB, '\u{27AB}'), (0xEC, '\u{27AC}'), (0xED, '\u{27AD}'),
    (0xEE, '\u{27AE}'), (0xEF, '\u{27AF}'), (0xF1, '\u{27B1}'), (0xF2, '\u{27B2}'), (0xF3, '\u{27B3}'), (0xF4, '\u{27B4}'),
    (0xF5, '\u{27B5}'), (0xF6, '\u{27B6}'), (0xF7, '\u{27B7}'), (0xF8, '\u{27B8}'), (0xF9, '\u{27B9}'), (0xFA, '\u{27BA}'),
    (0xFB, '\u{27BB}'), (0xFC, '\u{27BC}'), (0xFD, '\u{27BD}'), (0xFE, '\u{27BE}'),
];

/// docling-parse's text for a glyph name of the two symbol fonts — every
/// name their AFMs use (`a19` → `✓`, `arrowright` → `→`, `bracerighttp` →
/// `⎫`), from the same glyph tables as [`face_text`]. The builtin-encoding
/// reader asks it first ([`crate::textparse::builtin_glyph_text`]): the
/// ZapfDingbats names are in no other table here, and the delimiter pieces'
/// Unicode 3.2 rewrite is docling-parse's own.
pub(crate) fn symbol_glyph_char(name: &str) -> Option<char> {
    SYMBOL_FONT_GLYPHS
        .binary_search_by_key(&name, |&(n, _)| n)
        .ok()
        .map(|i| SYMBOL_FONT_GLYPHS[i].1)
}

#[rustfmt::skip]
const SYMBOL_FONT_GLYPHS: [(&str, char); 390] = [
    ("Alpha", '\u{0391}'), ("Beta", '\u{0392}'), ("Chi", '\u{03A7}'), ("Delta", '\u{2206}'),
    ("Epsilon", '\u{0395}'), ("Eta", '\u{0397}'), ("Euro", '\u{20AC}'), ("Gamma", '\u{0393}'),
    ("Ifraktur", '\u{2111}'), ("Iota", '\u{0399}'), ("Kappa", '\u{039A}'), ("Lambda", '\u{039B}'),
    ("Mu", '\u{039C}'), ("Nu", '\u{039D}'), ("Omega", '\u{2126}'), ("Omicron", '\u{039F}'),
    ("Phi", '\u{03A6}'), ("Pi", '\u{03A0}'), ("Psi", '\u{03A8}'), ("Rfraktur", '\u{211C}'),
    ("Rho", '\u{03A1}'), ("Sigma", '\u{03A3}'), ("Tau", '\u{03A4}'), ("Theta", '\u{0398}'),
    ("Upsilon", '\u{03A5}'), ("Upsilon1", '\u{03D2}'), ("Xi", '\u{039E}'), ("Zeta", '\u{0396}'),
    ("a1", '\u{2701}'), ("a10", '\u{2721}'), ("a100", '\u{275E}'), ("a101", '\u{2761}'),
    ("a102", '\u{2762}'), ("a103", '\u{2763}'), ("a104", '\u{2764}'), ("a105", '\u{2710}'),
    ("a106", '\u{2765}'), ("a107", '\u{2766}'), ("a108", '\u{2767}'), ("a109", '\u{2660}'),
    ("a11", '\u{261B}'), ("a110", '\u{2665}'), ("a111", '\u{2666}'), ("a112", '\u{2663}'),
    ("a117", '\u{2709}'), ("a118", '\u{2708}'), ("a119", '\u{2707}'), ("a12", '\u{261E}'),
    ("a120", '\u{2460}'), ("a121", '\u{2461}'), ("a122", '\u{2462}'), ("a123", '\u{2463}'),
    ("a124", '\u{2464}'), ("a125", '\u{2465}'), ("a126", '\u{2466}'), ("a127", '\u{2467}'),
    ("a128", '\u{2468}'), ("a129", '\u{2469}'), ("a13", '\u{270C}'), ("a130", '\u{2776}'),
    ("a131", '\u{2777}'), ("a132", '\u{2778}'), ("a133", '\u{2779}'), ("a134", '\u{277A}'),
    ("a135", '\u{277B}'), ("a136", '\u{277C}'), ("a137", '\u{277D}'), ("a138", '\u{277E}'),
    ("a139", '\u{277F}'), ("a14", '\u{270D}'), ("a140", '\u{2780}'), ("a141", '\u{2781}'),
    ("a142", '\u{2782}'), ("a143", '\u{2783}'), ("a144", '\u{2784}'), ("a145", '\u{2785}'),
    ("a146", '\u{2786}'), ("a147", '\u{2787}'), ("a148", '\u{2788}'), ("a149", '\u{2789}'),
    ("a15", '\u{270E}'), ("a150", '\u{278A}'), ("a151", '\u{278B}'), ("a152", '\u{278C}'),
    ("a153", '\u{278D}'), ("a154", '\u{278E}'), ("a155", '\u{278F}'), ("a156", '\u{2790}'),
    ("a157", '\u{2791}'), ("a158", '\u{2792}'), ("a159", '\u{2793}'), ("a16", '\u{270F}'),
    ("a160", '\u{2794}'), ("a161", '\u{2192}'), ("a162", '\u{27A3}'), ("a163", '\u{2194}'),
    ("a164", '\u{2195}'), ("a165", '\u{2799}'), ("a166", '\u{279B}'), ("a167", '\u{279C}'),
    ("a168", '\u{279D}'), ("a169", '\u{279E}'), ("a17", '\u{2711}'), ("a170", '\u{279F}'),
    ("a171", '\u{27A0}'), ("a172", '\u{27A1}'), ("a173", '\u{27A2}'), ("a174", '\u{27A4}'),
    ("a175", '\u{27A5}'), ("a176", '\u{27A6}'), ("a177", '\u{27A7}'), ("a178", '\u{27A8}'),
    ("a179", '\u{27A9}'), ("a18", '\u{2712}'), ("a180", '\u{27AB}'), ("a181", '\u{27AD}'),
    ("a182", '\u{27AF}'), ("a183", '\u{27B2}'), ("a184", '\u{27B3}'), ("a185", '\u{27B5}'),
    ("a186", '\u{27B8}'), ("a187", '\u{27BA}'), ("a188", '\u{27BB}'), ("a189", '\u{27BC}'),
    ("a19", '\u{2713}'), ("a190", '\u{27BD}'), ("a191", '\u{27BE}'), ("a192", '\u{279A}'),
    ("a193", '\u{27AA}'), ("a194", '\u{27B6}'), ("a195", '\u{27B9}'), ("a196", '\u{2798}'),
    ("a197", '\u{27B4}'), ("a198", '\u{27B7}'), ("a199", '\u{27AC}'), ("a2", '\u{2702}'),
    ("a20", '\u{2714}'), ("a200", '\u{27AE}'), ("a201", '\u{27B1}'), ("a202", '\u{2703}'),
    ("a203", '\u{2750}'), ("a204", '\u{2752}'), ("a205", '\u{276E}'), ("a206", '\u{2770}'),
    ("a21", '\u{2715}'), ("a22", '\u{2716}'), ("a23", '\u{2717}'), ("a24", '\u{2718}'),
    ("a25", '\u{2719}'), ("a26", '\u{271A}'), ("a27", '\u{271B}'), ("a28", '\u{271C}'),
    ("a29", '\u{2722}'), ("a3", '\u{2704}'), ("a30", '\u{2723}'), ("a31", '\u{2724}'),
    ("a32", '\u{2725}'), ("a33", '\u{2726}'), ("a34", '\u{2727}'), ("a35", '\u{2605}'),
    ("a36", '\u{2729}'), ("a37", '\u{272A}'), ("a38", '\u{272B}'), ("a39", '\u{272C}'),
    ("a4", '\u{260E}'), ("a40", '\u{272D}'), ("a41", '\u{272E}'), ("a42", '\u{272F}'),
    ("a43", '\u{2730}'), ("a44", '\u{2731}'), ("a45", '\u{2732}'), ("a46", '\u{2733}'),
    ("a47", '\u{2734}'), ("a48", '\u{2735}'), ("a49", '\u{2736}'), ("a5", '\u{2706}'),
    ("a50", '\u{2737}'), ("a51", '\u{2738}'), ("a52", '\u{2739}'), ("a53", '\u{273A}'),
    ("a54", '\u{273B}'), ("a55", '\u{273C}'), ("a56", '\u{273D}'), ("a57", '\u{273E}'),
    ("a58", '\u{273F}'), ("a59", '\u{2740}'), ("a6", '\u{271D}'), ("a60", '\u{2741}'),
    ("a61", '\u{2742}'), ("a62", '\u{2743}'), ("a63", '\u{2744}'), ("a64", '\u{2745}'),
    ("a65", '\u{2746}'), ("a66", '\u{2747}'), ("a67", '\u{2748}'), ("a68", '\u{2749}'),
    ("a69", '\u{274A}'), ("a7", '\u{271E}'), ("a70", '\u{274B}'), ("a71", '\u{25CF}'),
    ("a72", '\u{274D}'), ("a73", '\u{25A0}'), ("a74", '\u{274F}'), ("a75", '\u{2751}'),
    ("a76", '\u{25B2}'), ("a77", '\u{25BC}'), ("a78", '\u{25C6}'), ("a79", '\u{2756}'),
    ("a8", '\u{271F}'), ("a81", '\u{25D7}'), ("a82", '\u{2758}'), ("a83", '\u{2759}'),
    ("a84", '\u{275A}'), ("a85", '\u{276F}'), ("a86", '\u{2771}'), ("a87", '\u{2772}'),
    ("a88", '\u{2773}'), ("a89", '\u{2768}'), ("a9", '\u{2720}'), ("a90", '\u{2769}'),
    ("a91", '\u{276C}'), ("a92", '\u{276D}'), ("a93", '\u{276A}'), ("a94", '\u{276B}'),
    ("a95", '\u{2774}'), ("a96", '\u{2775}'), ("a97", '\u{275B}'), ("a98", '\u{275C}'),
    ("a99", '\u{275D}'), ("aleph", '\u{2135}'), ("alpha", '\u{03B1}'), ("ampersand", '\u{0026}'),
    ("angle", '\u{2220}'), ("angleleft", '\u{2329}'), ("angleright", '\u{232A}'), ("approxequal", '\u{2248}'),
    ("arrowboth", '\u{2194}'), ("arrowdblboth", '\u{21D4}'), ("arrowdbldown", '\u{21D3}'), ("arrowdblleft", '\u{21D0}'),
    ("arrowdblright", '\u{21D2}'), ("arrowdblup", '\u{21D1}'), ("arrowdown", '\u{2193}'), ("arrowhorizex", '\u{23AF}'),
    ("arrowleft", '\u{2190}'), ("arrowright", '\u{2192}'), ("arrowup", '\u{2191}'), ("arrowvertex", '\u{23D0}'),
    ("asteriskmath", '\u{2217}'), ("bar", '\u{007C}'), ("beta", '\u{03B2}'), ("braceex", '\u{23AA}'),
    ("braceleft", '\u{007B}'), ("braceleftbt", '\u{23A9}'), ("braceleftmid", '\u{23A8}'), ("bracelefttp", '\u{23A7}'),
    ("braceright", '\u{007D}'), ("bracerightbt", '\u{23AD}'), ("bracerightmid", '\u{23AC}'), ("bracerighttp", '\u{23AB}'),
    ("bracketleft", '\u{005B}'), ("bracketleftbt", '\u{23A3}'), ("bracketleftex", '\u{23A2}'), ("bracketlefttp", '\u{23A1}'),
    ("bracketright", '\u{005D}'), ("bracketrightbt", '\u{23A6}'), ("bracketrightex", '\u{23A5}'), ("bracketrighttp", '\u{23A4}'),
    ("bullet", '\u{2022}'), ("carriagereturn", '\u{21B5}'), ("chi", '\u{03C7}'), ("circlemultiply", '\u{2297}'),
    ("circleplus", '\u{2295}'), ("club", '\u{2663}'), ("colon", '\u{003A}'), ("comma", '\u{002C}'),
    ("congruent", '\u{2245}'), ("copyrightsans", '\u{F8E9}'), ("copyrightserif", '\u{F6D9}'), ("degree", '\u{00B0}'),
    ("delta", '\u{03B4}'), ("diamond", '\u{2666}'), ("divide", '\u{00F7}'), ("dotmath", '\u{22C5}'),
    ("eight", '\u{0038}'), ("element", '\u{2208}'), ("ellipsis", '\u{2026}'), ("emptyset", '\u{2205}'),
    ("epsilon", '\u{03B5}'), ("equal", '\u{003D}'), ("equivalence", '\u{2261}'), ("eta", '\u{03B7}'),
    ("exclam", '\u{0021}'), ("existential", '\u{2203}'), ("five", '\u{0035}'), ("florin", '\u{0192}'),
    ("four", '\u{0034}'), ("fraction", '\u{2044}'), ("gamma", '\u{03B3}'), ("gradient", '\u{2207}'),
    ("greater", '\u{003E}'), ("greaterequal", '\u{2265}'), ("heart", '\u{2665}'), ("infinity", '\u{221E}'),
    ("integral", '\u{222B}'), ("integralbt", '\u{2321}'), ("integralex", '\u{23AE}'), ("integraltp", '\u{2320}'),
    ("intersection", '\u{2229}'), ("iota", '\u{03B9}'), ("kappa", '\u{03BA}'), ("lambda", '\u{03BB}'),
    ("less", '\u{003C}'), ("lessequal", '\u{2264}'), ("logicaland", '\u{2227}'), ("logicalnot", '\u{00AC}'),
    ("logicalor", '\u{2228}'), ("lozenge", '\u{25CA}'), ("minus", '\u{2212}'), ("minute", '\u{2032}'),
    ("mu", '\u{00B5}'), ("multiply", '\u{00D7}'), ("nine", '\u{0039}'), ("notelement", '\u{2209}'),
    ("notequal", '\u{2260}'), ("notsubset", '\u{2284}'), ("nu", '\u{03BD}'), ("numbersign", '\u{0023}'),
    ("omega", '\u{03C9}'), ("omega1", '\u{03D6}'), ("omicron", '\u{03BF}'), ("one", '\u{0031}'),
    ("parenleft", '\u{0028}'), ("parenleftbt", '\u{239D}'), ("parenleftex", '\u{239C}'), ("parenlefttp", '\u{239B}'),
    ("parenright", '\u{0029}'), ("parenrightbt", '\u{23A0}'), ("parenrightex", '\u{239F}'), ("parenrighttp", '\u{239E}'),
    ("partialdiff", '\u{2202}'), ("percent", '\u{0025}'), ("period", '\u{002E}'), ("perpendicular", '\u{22A5}'),
    ("phi", '\u{03C6}'), ("phi1", '\u{03D5}'), ("pi", '\u{03C0}'), ("plus", '\u{002B}'),
    ("plusminus", '\u{00B1}'), ("product", '\u{220F}'), ("propersubset", '\u{2282}'), ("propersuperset", '\u{2283}'),
    ("proportional", '\u{221D}'), ("psi", '\u{03C8}'), ("question", '\u{003F}'), ("radical", '\u{221A}'),
    ("radicalex", '\u{F8E5}'), ("reflexsubset", '\u{2286}'), ("reflexsuperset", '\u{2287}'), ("registersans", '\u{F8E8}'),
    ("registerserif", '\u{F6DA}'), ("rho", '\u{03C1}'), ("second", '\u{2033}'), ("semicolon", '\u{003B}'),
    ("seven", '\u{0037}'), ("sigma", '\u{03C3}'), ("sigma1", '\u{03C2}'), ("similar", '\u{223C}'),
    ("six", '\u{0036}'), ("slash", '\u{002F}'), ("space", '\u{0020}'), ("spade", '\u{2660}'),
    ("suchthat", '\u{220B}'), ("summation", '\u{2211}'), ("tau", '\u{03C4}'), ("therefore", '\u{2234}'),
    ("theta", '\u{03B8}'), ("theta1", '\u{03D1}'), ("three", '\u{0033}'), ("trademarksans", '\u{F8EA}'),
    ("trademarkserif", '\u{F6DB}'), ("two", '\u{0032}'), ("underscore", '\u{005F}'), ("union", '\u{222A}'),
    ("universal", '\u{2200}'), ("upsilon", '\u{03C5}'), ("weierstrass", '\u{2118}'), ("xi", '\u{03BE}'),
    ("zero", '\u{0030}'), ("zeta", '\u{03B6}'),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_are_sorted() {
        for t in [&SYMBOL_TEXT[..], &ZAPF_DINGBATS_TEXT[..]] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        }
        assert!(SYMBOL_FONT_GLYPHS.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(symbol_glyph_char("a19"), Some('✓'));
        assert_eq!(symbol_glyph_char("a23"), Some('✗'));
        assert_eq!(symbol_glyph_char("bracerighttp"), Some('⎫'));
        assert_eq!(symbol_glyph_char("arrowright"), Some('→'));
        assert_eq!(symbol_glyph_char("nosuchname"), None);
    }

    /// The faces docling-parse matches, and the folds the issue lists:
    /// redp5110's `U+F0AE` → `→`, Symbol's bullet, ZapfDingbats' check mark,
    /// the legacy delimiter piece → Unicode 3.2's.
    #[test]
    fn private_use_values_fold_through_the_face() {
        assert_eq!(symbol_face(&[b"NKEKHF+SymbolMT"]), Some(SymbolFace::Symbol));
        assert_eq!(symbol_face(&[b"OpenSymbol"]), Some(SymbolFace::Symbol));
        assert_eq!(
            symbol_face(&[b"ZapfDingbats"]),
            Some(SymbolFace::ZapfDingbats)
        );
        assert_eq!(symbol_face(&[b"Wingdings-Regular"]), None);
        assert_eq!(symbol_face(&[b"F3", b"Symbol"]), Some(SymbolFace::Symbol));
        let sym = SymbolFace::Symbol;
        assert_eq!(fold_private_use(sym, "\u{F0AE}"), Some('→'));
        assert_eq!(fold_private_use(sym, "\u{F0B7}"), Some('•'));
        assert_eq!(fold_private_use(sym, "\u{F0FC}"), Some('⎫'));
        assert_eq!(fold_private_use(sym, "\u{F028}"), Some('('));
        assert_eq!(
            fold_private_use(sym, "\u{F0FF}"),
            None,
            "not in the encoding"
        );
        assert_eq!(fold_private_use(sym, "\u{F01F}"), None, "below the block");
        assert_eq!(
            fold_private_use(sym, "\u{F0AE}x"),
            None,
            "one code point only"
        );
        assert_eq!(
            fold_private_use(SymbolFace::ZapfDingbats, "\u{F033}"),
            Some('✓')
        );
    }
}
