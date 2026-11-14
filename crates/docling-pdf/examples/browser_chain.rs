//! The browser's digital-PDF chain (`docling-wasm`'s `DigitalConverter`) run
//! natively: the text-layer page from `textparse`, layout over the native
//! render, `scanned::refine_regions` + `drop_duplicate_text_claims`, then
//! TableFormer for the table regions whose geometric reconstruction is not
//! trusted, `scanned::assemble_page_with_tables` and `finish_document`.
//!
//! The browser path shares every one of those steps with the native pipeline
//! but strings them together itself — and reads its cells from the text-layer
//! page, not the pipeline's — so a difference between the two shows up here
//! without a browser (#1129 did: 7 line cells where the pipeline had 44).
//! The raster is the native render, not pdf.js's, so the layout can differ
//! from the browser's in detail.
//!
//! Markdown goes to stdout; each table's verdict (geometric rows, why it is
//! or is not trusted, TableFormer's grid) to stderr. `--no-tf` leaves
//! TableFormer out (the lite profile, `add_page`), `--regions` lists the
//! refined regions and each table's geometric grid, `--cells` the line cells
//! of both paths.
//!
//! ```text
//! cargo run --release -p docling-pdf --example browser_chain -- file.pdf [--no-tf] [--regions] [--cells]
//! ```
use docling_pdf::assemble::{
    geometric_rows_share_lines, geometric_table_is_reliable, reconstruct_table,
};
use docling_pdf::layout::LayoutModel;
use docling_pdf::scanned::{
    assemble_page_with_tables, drop_duplicate_text_claims, finish_document, refine_regions,
};
use docling_pdf::tableformer::TableFormer;
use docling_pdf::PdfDocument;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .expect("usage: browser_chain file.pdf [--no-tf] [--regions] [--cells]");
    let no_tf = args.iter().any(|a| a == "--no-tf");
    let show_regions = args.iter().any(|a| a == "--regions");

    let bytes = std::fs::read(path).expect("read");
    let text_pages = docling_pdf::textparse::pdf_text_pages(&bytes);
    let rendered = PdfDocument::open(&bytes, None).expect("open");
    let mut layout = LayoutModel::load().expect("layout model");
    let mut tf = (!no_tf).then(|| TableFormer::load().expect("tableformer"));

    let mut out = Vec::new();
    for (mut page, native) in text_pages.into_iter().zip(&rendered.pages) {
        if args.iter().any(|a| a == "--cells") {
            for (name, cells) in [("browser", &page.cells), ("native", &native.cells)] {
                eprintln!("{name}: {} line cells", cells.len());
                for c in cells {
                    eprintln!(
                        "  {name} [{:.1} {:.1} {:.1} {:.1}] {}",
                        c.l, c.t, c.r, c.b, c.text
                    );
                }
            }
            eprintln!(
                "word cells: browser {}, native {}",
                page.word_cells.len(),
                native.word_cells.len()
            );
        }
        let regions = layout
            // The browser hands the layout model its raw bitmap (`layout_input`).
            .predict(
                docling_pdf::layout::LayoutSrc::Raw(&native.image),
                page.width,
                page.height,
            )
            .expect("layout");
        let regions = refine_regions(regions, &page.cells, page.width, page.height);
        let regions = drop_duplicate_text_claims(regions, &page.cells);
        page.image = native.image.clone();
        page.scale = native.scale;

        let mut table_rows = Vec::with_capacity(regions.len());
        for r in &regions {
            if show_regions {
                eprintln!(
                    "{:>14} {:.2} [{:.1} {:.1} {:.1} {:.1}]",
                    r.label, r.score, r.l, r.t, r.r, r.b
                );
            }
            if r.label != "table" {
                table_rows.push(None);
                continue;
            }
            let geometric = reconstruct_table(r, &page.cells);
            if show_regions {
                for row in &geometric {
                    eprintln!("    {row:?}");
                }
            }
            let dense = geometric_table_is_reliable(&geometric);
            let aligned = geometric_rows_share_lines(r, &page.cells);
            let reliable = dense && aligned;
            let grid = match tf.as_mut() {
                Some(tf) if !reliable => {
                    tf.predict_table_rows(&page.image, [r.l, r.t, r.r, r.b], &page.word_cells)
                }
                _ => None,
            };
            eprintln!(
                "table [{:.1} {:.1} {:.1} {:.1}]: geometric {} rows ({}), tableformer {}",
                r.l,
                r.t,
                r.r,
                r.b,
                geometric.len(),
                match (dense, aligned) {
                    (true, true) => "reliable",
                    (true, false) => "rows off their lines",
                    (false, _) => "not reliable",
                },
                match &grid {
                    Some(g) => format!(
                        "{}x{} with {} cells",
                        g.rows.len(),
                        g.rows.first().map_or(0, Vec::len),
                        g.cells.len()
                    ),
                    None => "none".to_string(),
                }
            );
            table_rows.push(grid);
        }
        out.push(assemble_page_with_tables(&page, regions, table_rows));
    }
    let doc = finish_document("browser_chain", out);
    print!("{}", doc.export_to_markdown());
}
