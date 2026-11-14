//! The Adobe Glyph List: the glyph name → Unicode table a PDF font's
//! `/Differences` and builtin-encoding names are read with, in full.
//!
//! [`crate::textparse::glyph_name_to_char`] knows the names a Latin or TeX
//! document uses — its own arms, a Latin table, the TeX math rules
//! ([`crate::glyph_names`]), the Symbol font's names. Everything else the
//! Glyph List defines fell through: the `afii` names of a Cyrillic, Hebrew
//! or Arabic font re-encoded with `/Differences` and no ToUnicode
//! (`afii10017` is `А`), the remaining Greek, arrows, box drawing, Latin
//! Extended Additional. An unresolved name read the base encoding's letter
//! at that code, or U+FFFD below 0x20 — plausible but wrong text.
//!
//! The list is `data/adobe-glyph-list/glyphlist.txt`, unmodified, compiled
//! in under the `glyph-list` feature (the wasm build leaves it out, as it
//! does the CID tables). It is Adobe's, BSD-3-Clause: the notice sits in the
//! file's header and in `data/adobe-glyph-list/LICENSE.txt`, which every
//! published binary carries as `LICENSE-adobe-glyph-list.txt`.
//!
//! Read as docling-parse reads it (`font_glyphs::read_file_hex`), with three
//! differences:
//!
//! - a name that stands for a *sequence* (`dalethatafpatah`, two Hebrew code
//!   points) is left out: the resolver returns one character;
//! - a private-use value is left out — the small-cap, swash and old-style
//!   variants (`Asmall` is U+F761) would read as tofu — except the Symbol
//!   font's delimiter pieces, which docling-parse rewrites to the Unicode
//!   3.2 characters (`bracelefttp` U+F8F1 → `⎧`), as here;
//! - the angle brackets are the mathematical ones: the list gives the
//!   deprecated U+2329 / U+232A for `angleleft` / `angleright`; in a PDF
//!   these are `⟨` / `⟩`, as the TeX names read ([`crate::glyph_names`]).

#[cfg(feature = "glyph-list")]
mod list {
    use std::collections::HashMap;
    use std::sync::OnceLock;

    static DATA: &str = include_str!("../data/adobe-glyph-list/glyphlist.txt");

    /// docling-parse's `legacy_pua` (`glyphs.h`): the Glyph List's
    /// private-use codes for the Symbol font's bracket, brace, parenthesis
    /// and extender pieces, and the Unicode 3.2 characters that replaced
    /// them.
    fn legacy_piece(cp: u32) -> Option<u32> {
        Some(match cp {
            0xF8EB => 0x239B,
            0xF8EC => 0x239C,
            0xF8ED => 0x239D,
            0xF8F6 => 0x239E,
            0xF8F7 => 0x239F,
            0xF8F8 => 0x23A0,
            0xF8EE => 0x23A1,
            0xF8EF => 0x23A2,
            0xF8F0 => 0x23A3,
            0xF8F9 => 0x23A4,
            0xF8FA => 0x23A5,
            0xF8FB => 0x23A6,
            0xF8F1 => 0x23A7,
            0xF8F2 => 0x23A8,
            0xF8F3 => 0x23A9,
            0xF8F4 => 0x23AA,
            0xF8FC => 0x23AB,
            0xF8FD => 0x23AC,
            0xF8FE => 0x23AD,
            0xF8F5 => 0x23AE,
            0xF8E7 => 0x23AF,
            0xF8E6 => 0x23D0,
            _ => return None,
        })
    }

    /// One line's character: `None` for a sequence, a private-use value, a
    /// control or anything that is no scalar value.
    fn value(hex: &str) -> Option<char> {
        let cp = u32::from_str_radix(hex, 16).ok()?;
        let cp = legacy_piece(cp).unwrap_or(cp);
        let cp = match cp {
            0x2329 => 0x27E8,
            0x232A => 0x27E9,
            c => c,
        };
        let ch = char::from_u32(cp)?;
        (!ch.is_control() && !(0xE000..=0xF8FF).contains(&cp)).then_some(ch)
    }

    pub(super) fn table() -> &'static HashMap<&'static str, char> {
        static TABLE: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
        TABLE.get_or_init(|| {
            DATA.lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| {
                    let (name, hex) = l.split_once(';')?;
                    Some((name, value(hex)?))
                })
                .collect()
        })
    }
}

/// The character the Adobe Glyph List gives `name`; `None` for a name it
/// does not define or leaves to a sequence or the private-use area, and in a
/// build without the `glyph-list` feature.
pub(crate) fn lookup(name: &str) -> Option<char> {
    #[cfg(feature = "glyph-list")]
    {
        list::table().get(name).copied()
    }
    #[cfg(not(feature = "glyph-list"))]
    {
        let _ = name;
        None
    }
}

#[cfg(all(test, feature = "glyph-list"))]
mod tests {
    use super::*;

    /// The vendored file is the Glyph List, whole: 4,282 names, of which the
    /// 82 sequences and the private-use variants are left out.
    #[test]
    fn the_list_is_whole() {
        let lines = include_str!("../data/adobe-glyph-list/glyphlist.txt")
            .lines()
            .filter(|l| !l.starts_with('#'))
            .count();
        assert_eq!(lines, 4282);
        let t = list::table();
        assert!(t.len() > 3500 && t.len() <= 4282 - 82, "{} names", t.len());
        assert!(t
            .values()
            .all(|&c| { !c.is_control() && !('\u{E000}'..='\u{F8FF}').contains(&c) }));
    }

    #[test]
    fn names_resolve_as_the_list_defines_them() {
        for (name, want) in [
            ("afii10017", 'А'),   // Cyrillic
            ("afii57664", 'א'),   // Hebrew
            ("afii57415", 'ا'),   // Arabic
            ("Abreveacute", 'Ắ'), // Latin Extended Additional
            ("arrowdblup", '⇑'),
            ("SF100000", '─'),    // box drawing
            ("bracelefttp", '⎧'), // docling-parse's Unicode 3.2 piece
            ("angleleft", '⟨'),   // not the deprecated U+2329
            ("angleright", '⟩'),
        ] {
            assert_eq!(lookup(name), Some(want), "{name}");
        }
        assert_eq!(lookup("Asmall"), None, "a private-use variant");
        assert_eq!(lookup("dalethatafpatah"), None, "a sequence");
        assert_eq!(lookup("controlSTX"), None, "a control");
        assert_eq!(lookup("nosuchglyphname"), None);
    }

    /// The wheels take the notice from the binding's own directory (maturin
    /// ships its `LICENSE-*`), so that copy has to follow this one. Skipped
    /// where the binding is not checked out next to the crate (a packaged
    /// docling-pdf).
    #[test]
    fn the_wheel_notice_is_this_notice() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let Ok(copy) =
            std::fs::read_to_string(root.join("../docling-py/LICENSE-adobe-glyph-list.txt"))
        else {
            return;
        };
        assert_eq!(
            copy,
            include_str!("../data/adobe-glyph-list/LICENSE.txt"),
            "copy data/adobe-glyph-list/LICENSE.txt over crates/docling-py/LICENSE-adobe-glyph-list.txt"
        );
    }
}
