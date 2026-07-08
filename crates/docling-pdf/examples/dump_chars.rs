//! Dump pdfium's raw char stream (codepoint + loose left/right) for a page,
//! optionally filtered to a substring window. Usage:
//!   ... --example dump_chars -- file.pdf [needle]
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dump_chars <pdf> [needle]");
    let needle = std::env::args().nth(2);
    let bytes = std::fs::read(&path).expect("read");
    let glyphs = docling_pdf::pdfium_backend::debug_glyphs(&bytes, 0);
    let text: String = glyphs.iter().map(|(c, _, _)| *c).collect();
    let start = needle
        .as_deref()
        .and_then(|n| text.find(n))
        .map(|b| text[..b].chars().count())
        .unwrap_or(0);
    println!("pdfium chars (ch / loose-left / loose-right / gap-to-prev):");
    let mut prev_r = f32::NAN;
    for (ch, l, r) in glyphs.iter().skip(start).take(20) {
        let gap = l - prev_r;
        println!("  {:?}  l={:7.2} r={:7.2}  gap={:+.2}", ch, l, r, gap);
        prev_r = *r;
    }
}
