//! Dump the Rust parser's word cells for a page: `<pdf> <page_index_0based>`.
//! TSV: l<tab>t<tab>r<tab>b<tab>text  (top-left page-point coords).
fn main() {
    let path = std::env::args().nth(1).expect("pdf");
    let pi: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let bytes = std::fs::read(&path).unwrap();
    let pages = docling_pdf::textparse::pdf_words(&bytes);
    if let Some((_, _, cells)) = pages.get(pi) {
        for c in cells {
            let t = c.text.replace(['\t', '\n'], " ");
            println!("{:.2}\t{:.2}\t{:.2}\t{:.2}\t{}", c.l, c.t, c.r, c.b, t);
        }
    }
}
