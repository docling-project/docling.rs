//! Glyph names of the TeX math fonts (#1121) — the names Computer Modern,
//! Latin Modern and the AMS symbol fonts give their glyphs, which are not
//! Adobe Glyph List names, so [`crate::textparse::glyph_name_to_char`] had
//! no reading for them.
//!
//! A TeX math font names its glyphs in `/Differences` or in its program's
//! builtin encoding, with no ToUnicode. Unresolved, a glyph at a code below
//! 0x20 read U+FFFD (`rt�A(M)�` for `rt(A(M))`: `parenleftbig` /
//! `parenrightbig` at codes 0 and 1 of the math extension font) and one
//! above it the Latin letter at that code (`Z` for `integraldisplay`, `P`
//! for `summationtext`, `I` for MSAM's `trianglerightsld`).
//!
//! Most of these names are a base name and a size: the extension font
//! (CMEX) carries every delimiter in four sizes and every big operator in
//! two, and the size says nothing about the character. So the reading is
//! by rule — strip the size, read the base — with a short list for the rest.
//! docling-parse reads the same names from glyph lists (PDFBox's additions to
//! the Adobe Glyph List, lcdf-typetools' `texglyphlist`); the characters here
//! agree with its, with one exception spelled out below.

/// Delimiter sizes, `\big` to `\Bigg`: `parenleftbig`, `bracketrightBigg`.
/// Longest first, so `bigg` is not read as `big` + `g`.
const DELIMITER_SIZES: [&str; 4] = ["Bigg", "bigg", "Big", "big"];

/// The delimiters that come in sizes, by base name. The plain names are
/// Glyph List names the resolver knows; these are the ones it does not.
fn delimiter(base: &str) -> Option<char> {
    Some(match base {
        "parenleft" => '(',
        "parenright" => ')',
        "bracketleft" => '[',
        "bracketright" => ']',
        "braceleft" => '{',
        "braceright" => '}',
        "slash" => '/',
        "backslash" => '\\',
        "floorleft" => '\u{230A}',
        "floorright" => '\u{230B}',
        "ceilingleft" => '\u{2308}',
        "ceilingright" => '\u{2309}',
        // `\langle` / `\rangle`: the mathematical angle brackets, as the TeX
        // math tables in `textparse` read them. docling-parse's lists give
        // the CJK punctuation U+3008 / U+3009 (or its deprecated twin
        // U+2329 / U+232A) — a deliberate divergence.
        "angbracketleft" => '\u{27E8}',
        "angbracketright" => '\u{27E9}',
        "radical" => '\u{221A}',
        _ => return None,
    })
}

/// The big operators, in text and display size: `summationtext`,
/// `integraldisplay`.
fn operator(base: &str) -> Option<char> {
    Some(match base {
        "summation" => '\u{2211}',
        "product" => '\u{220F}',
        "coproduct" => '\u{2210}',
        "integral" => '\u{222B}',
        "contintegral" => '\u{222E}',
        "union" => '\u{22C3}',
        "intersection" => '\u{22C2}',
        "unionmulti" => '\u{2A04}',
        "unionsq" => '\u{2A06}',
        "logicaland" => '\u{22C0}',
        "logicalor" => '\u{22C1}',
        "circledot" => '\u{2A00}',
        "circleplus" => '\u{2A01}',
        "circlemultiply" => '\u{2A02}',
        _ => return None,
    })
}

/// The character a TeX math-font glyph name stands for.
pub(crate) fn lookup(name: &str) -> Option<char> {
    for size in DELIMITER_SIZES {
        if let Some(c) = name.strip_suffix(size).and_then(delimiter) {
            return Some(c);
        }
    }
    for size in ["display", "text"] {
        if let Some(c) = name.strip_suffix(size).and_then(operator) {
            return Some(c);
        }
    }
    Some(match name {
        // The plain angle brackets and floor / ceiling: no size, same
        // characters.
        "angbracketleft" | "angbracketright" | "floorleft" | "floorright" | "ceilingleft"
        | "ceilingright" => delimiter(name)?,
        // The extensible pieces a tall delimiter or arrow is stacked from.
        "vextendsingle" => '\u{2223}',
        "vextenddouble" => '\u{2225}',
        "radicalvertex" | "radicalbt" | "radicaltp" => '\u{221A}',
        "arrowtp" => '\u{2191}',
        "arrowbt" => '\u{2193}',
        // Glyph List names the resolver's own tables leave out, which a
        // Computer Modern / Latin Modern text or math font's encoding uses:
        // the spacing accents, a few marks and symbols. Unresolved in a
        // `/Differences`, each read the Latin letter at its code (`prime`
        // read `0`, `dagger` read `y`).
        "prime" => '\u{2032}',
        "breve" => '\u{02D8}',
        "caron" => '\u{02C7}',
        "circumflex" => '\u{02C6}',
        "dotaccent" => '\u{02D9}',
        "hungarumlaut" => '\u{02DD}',
        "ogonek" => '\u{02DB}',
        "ring" => '\u{02DA}',
        "tilde" => '\u{02DC}',
        "dotlessj" => '\u{0237}',
        "dagger" => '\u{2020}',
        "daggerdbl" => '\u{2021}',
        "perthousand" => '\u{2030}',
        "guilsinglleft" => '\u{2039}',
        "guilsinglright" => '\u{203A}',
        "Ifractur" => '\u{2111}',
        "Rfractur" => '\u{211C}',
        "coproduct" => '\u{2210}',
        "unionmulti" => '\u{228E}',
        "circledot" => '\u{2299}',
        "latticetop" => '\u{22A4}',
        // CMSY / LM math symbols.
        "bardbl" => '\u{2016}',
        "mapsto" => '\u{21A6}',
        "owner" => '\u{220B}',
        "negationslash" => '\u{0338}',
        "nabla" => '\u{2207}',
        "turnstileleft" => '\u{22A2}',
        "turnstileright" => '\u{22A3}',
        "follows" => '\u{227B}',
        "precedes" => '\u{227A}',
        "followsequal" => '\u{2AB0}',
        "precedesequal" => '\u{2AAF}',
        "lessmuch" => '\u{226A}',
        "greatermuch" => '\u{226B}',
        "equivasymptotic" => '\u{224D}',
        "similarequal" => '\u{2243}',
        "minusplus" => '\u{2213}',
        "circleminus" => '\u{2296}',
        "circledivide" => '\u{2298}',
        "circlecopyrt" => '\u{25EF}',
        "openbullet" => '\u{2218}',
        "diamondmath" => '\u{22C4}',
        "wreathproduct" => '\u{2240}',
        "unionsq" => '\u{2294}',
        "intersectionsq" => '\u{2293}',
        "subsetsqequal" => '\u{2291}',
        "supersetsqequal" => '\u{2292}',
        "triangle" => '\u{25B3}',
        "triangleinv" => '\u{25BD}',
        "arrowboth" => '\u{2194}',
        "arrowbothv" => '\u{2195}',
        "arrowdblbothv" => '\u{21D5}',
        "arrownortheast" => '\u{2197}',
        "arrowsoutheast" => '\u{2198}',
        "arrownorthwest" => '\u{2196}',
        "arrowsouthwest" => '\u{2199}',
        // CMMI.
        "star" => '\u{22C6}',
        "triangleright" => '\u{25B7}',
        "triangleleft" => '\u{25C1}',
        "harpoonleftup" => '\u{21BC}',
        "harpoonleftdown" => '\u{21BD}',
        "harpoonrightup" => '\u{21C0}',
        "harpoonrightdown" => '\u{21C1}',
        "flat" => '\u{266D}',
        "natural" => '\u{266E}',
        "sharp" => '\u{266F}',
        "slurbelow" => '\u{2323}',
        "slurabove" => '\u{2322}',
        "lscript" => '\u{2113}',
        "weierstrass" => '\u{2118}',
        "vector" => '\u{20D7}',
        "tie" => '\u{2040}',
        // The AMS symbol fonts: the solid triangles `amsthm` opens an
        // example with (1805.06723's `▶ Example 4.` read `I Example 4.`).
        "trianglerightsld" => '\u{25B6}',
        "triangleleftsld" => '\u{25C0}',
        "squaresolid" => '\u{25A0}',
        "square" => '\u{25A1}',
        "lozenge" => '\u{25CA}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every delimiter in every size, every operator in both, reads as its
    /// base character; a name that only looks like one does not.
    #[test]
    fn sizes_are_stripped() {
        for (name, want) in [
            ("parenleftbig", '('),
            ("parenleftBig", '('),
            ("parenleftbigg", '('),
            ("parenrightBigg", ')'),
            ("bracketleftBig", '['),
            ("bracketrightbigg", ']'),
            ("braceleftbig", '{'),
            ("bracerightBigg", '}'),
            ("angbracketleftbig", '⟨'),
            ("angbracketrightBigg", '⟩'),
            ("floorleftbig", '⌊'),
            ("ceilingrightBig", '⌉'),
            ("slashbigg", '/'),
            ("backslashBig", '\\'),
            ("radicalbig", '√'),
            ("radicalBigg", '√'),
            ("summationtext", '∑'),
            ("summationdisplay", '∑'),
            ("producttext", '∏'),
            ("integraldisplay", '∫'),
            ("contintegraltext", '∮'),
            ("uniondisplay", '⋃'),
            ("intersectiontext", '⋂'),
            ("logicalanddisplay", '⋀'),
            ("circleplustext", '⨁'),
        ] {
            assert_eq!(lookup(name), Some(want), "{name}");
        }
        for name in [
            "big",
            "Bigg",
            "text",
            "display",
            "contextdisplay",
            "hatwidebig",
        ] {
            assert_eq!(lookup(name), None, "{name}");
        }
    }

    #[test]
    fn the_listed_names_resolve() {
        for (name, want) in [
            ("vextendsingle", '∣'),
            ("vextenddouble", '∥'),
            ("bardbl", '‖'),
            ("mapsto", '↦'),
            ("angbracketleft", '⟨'),
            ("trianglerightsld", '▶'),
            ("triangleleftsld", '◀'),
        ] {
            assert_eq!(lookup(name), Some(want), "{name}");
        }
        assert_eq!(lookup("nosuchglyphname"), None);
    }
}
