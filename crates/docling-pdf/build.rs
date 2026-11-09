//! Writes Adobe's CID -> Unicode tables (data/mapping-resources-pdf/) in the
//! compact form `textparse::collection_text` reads.

use std::collections::HashMap;
use std::path::Path;
use std::{env, fs};

#[path = "src/tounicode.rs"]
mod tounicode;

fn main() {
    let dir = Path::new("data/mapping-resources-pdf/pdf2unicode");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/tounicode.rs");
    println!("cargo:rerun-if-changed={}", dir.display());
    let out = env::var_os("OUT_DIR").expect("OUT_DIR");
    for name in [
        "Adobe-Japan1-UCS2",
        "Adobe-GB1-UCS2",
        "Adobe-CNS1-UCS2",
        "Adobe-Korea1-UCS2",
        "Adobe-KR-UCS2",
    ] {
        let cmap = fs::read(dir.join(name)).expect("pdf2unicode table");
        fs::write(
            Path::new(&out).join(name),
            compact(&tounicode::parse_tounicode(&cmap)),
        )
        .expect("write table");
    }
}

/// The CID count, three bytes per CID (0 for none, a code point, or 0x110000 +
/// an offset into the strings), then the length-prefixed UTF-8 strings.
fn compact(table: &HashMap<u32, String>) -> Vec<u8> {
    let n = table.keys().max().map_or(0, |&m| m + 1);
    let mut out = n.to_le_bytes().to_vec();
    let mut strings = Vec::new();
    for cid in 0..n {
        let value = match table.get(&cid).and_then(|s| text(s)) {
            None => 0,
            Some(s) if s.chars().count() == 1 => s.chars().next().unwrap() as u32,
            Some(s) => {
                let at = 0x110000 + strings.len() as u32;
                assert!(s.len() < 256 && at < 1 << 24);
                strings.push(s.len() as u8);
                strings.extend_from_slice(s.as_bytes());
                at
            }
        };
        out.extend_from_slice(&value.to_be_bytes()[1..]);
    }
    out.extend(strings);
    out
}

/// Adobe adds variation selectors for glyph variants, and maps CIDs without a
/// character to U+0000, U+FFFD or a control code.
fn text(s: &str) -> Option<String> {
    let s: String = s
        .chars()
        .filter(|c| !matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}'))
        .collect();
    (!s.is_empty() && !s.chars().any(|c| c.is_control() || c == '\u{FFFD}')).then_some(s)
}
