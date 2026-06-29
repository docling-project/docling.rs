//! Dump layout regions (label, bbox, text) for debugging reading order.
use fleischwolf_pdf::layout::LayoutModel;
use fleischwolf_pdf::PdfDocument;

fn main() {
    let path = std::env::args().nth(1).expect("pdf");
    let bytes = std::fs::read(&path).expect("read");
    let doc = PdfDocument::open(&bytes, None).expect("open");
    let mut layout = LayoutModel::load().expect("layout");
    for (pi, page) in doc.pages.iter().enumerate() {
        let regions = layout
            .predict(&page.image, page.width, page.height)
            .expect("layout");
        for r in &regions {
            // crude text: cells whose center is inside the region
            let txt: String = page
                .cells
                .iter()
                .filter(|c| {
                    let (cx, cy) = ((c.l + c.r) / 2.0, (c.t + c.b) / 2.0);
                    cx >= r.l && cx <= r.r && cy >= r.t && cy <= r.b
                })
                .map(|c| c.text.trim())
                .collect::<Vec<_>>()
                .join(" ");
            let tail: String = txt
                .chars()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            println!(
                "p{} {:>14} t={:6.1} b={:6.1} | …{}",
                pi + 1,
                r.label,
                r.t,
                r.b,
                tail
            );
        }
        // raw line cells in extraction order (to inspect RTL ordering)
        if std::env::var("DUMP_CELLS").is_ok() {
            for (ci, c) in page.cells.iter().enumerate() {
                let snip: String = c.text.chars().take(50).collect();
                println!(
                    "   CELL[{ci}] t={:6.1} l={:6.1} r={:6.1} | {}",
                    c.t, c.l, c.r, snip
                );
            }
        }
    }
}
