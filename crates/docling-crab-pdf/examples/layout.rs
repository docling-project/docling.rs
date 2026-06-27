//! `cargo run -p docling-crab-pdf --example layout -- file.pdf`
//! Runs layout detection on page 1 and prints the regions.

use docling_crab_pdf::layout::LayoutModel;
use docling_crab_pdf::PdfDocument;

fn main() {
    let path = std::env::args().nth(1).expect("usage: layout <file.pdf>");
    let bytes = std::fs::read(&path).expect("read pdf");
    let doc = PdfDocument::open(&bytes, None).expect("open pdf");
    let mut model = LayoutModel::load().expect("load layout model");
    for (i, page) in doc.pages.iter().enumerate().take(1) {
        let regions = model
            .predict(&page.image, page.width, page.height)
            .expect("predict");
        println!("page {} ({:.0}x{:.0}): {} regions", i + 1, page.width, page.height, regions.len());
        let mut rs = regions.clone();
        rs.sort_by(|a, b| a.t.total_cmp(&b.t));
        for r in &rs {
            println!(
                "  {:<16} {:.2}  [{:.0},{:.0},{:.0},{:.0}]",
                r.label, r.score, r.l, r.t, r.r, r.b
            );
        }
    }
}
