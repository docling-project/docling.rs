//! Dump the Rust parser's raw glyphs for a page: `<pdf> <page_index>`.
fn main() {
    let path = std::env::args().nth(1).expect("pdf");
    let pi: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bytes = std::fs::read(&path).unwrap();
    let gs = docling_pdf::textparse::debug_glyphs(&bytes, pi);
    let s: String = gs.iter().map(|g| g.0).collect();
    println!("{s}");
    for (ch, ll, lr, lb, lt) in &gs {
        eprintln!(
            "{:?} x0={:7.2} x1={:7.2} yb={:7.2} yt={:7.2}",
            ch, ll, lr, lb, lt
        );
    }
}
