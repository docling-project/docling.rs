//! Dump pdfium's raw char stream (codepoint + x) for a page, to compare char
//! extraction against docling-parse. Usage: `... --example dump_chars -- file.pdf`
fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_chars <pdf>");
    let bytes = std::fs::read(&path).expect("read");
    let glyphs = fleischwolf_pdf::pdfium_backend::debug_glyphs(&bytes, 0);
    println!("pdfium CHAR order (first 16):");
    for (ch, l, _b, _r, _t) in glyphs.iter().take(16) {
        println!("  {:?} U+{:04X}  xl={:.1}", ch, *ch as u32, l);
    }
}
