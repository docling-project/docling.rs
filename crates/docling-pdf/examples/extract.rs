//! Smoke test: `cargo run -p docling-pdf --example extract -- file.pdf`

fn main() {
    let path = std::env::args().nth(1).expect("usage: extract <file.pdf>");
    let bytes = std::fs::read(&path).expect("read pdf");
    match docling_pdf::PdfDocument::open(&bytes, None) {
        Ok(doc) => {
            for (i, page) in doc.pages.iter().enumerate() {
                println!(
                    "--- page {} ({:.0}x{:.0}, {} cells) ---",
                    i + 1,
                    page.width,
                    page.height,
                    page.cells.len()
                );
                for c in page.cells.iter().take(8) {
                    println!(
                        "  [{:.0},{:.0},{:.0},{:.0}] {:?}",
                        c.l, c.t, c.r, c.b, c.text
                    );
                }
            }
        }
        Err(e) => eprintln!("ERROR: {e}"),
    }
}
