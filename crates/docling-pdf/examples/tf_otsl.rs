//! Verify TableFormer inference: run it on every table region of a PDF and print
//! the predicted OTSL structure. Usage: `... --example tf_otsl -- file.pdf`

use docling_pdf::layout::LayoutModel;
use docling_pdf::tableformer::TableFormer;
use docling_pdf::PdfDocument;
use image::imageops;

fn name(t: i64) -> &'static str {
    match t {
        4 => "ecel",
        5 => "fcel",
        6 => "lcel",
        7 => "ucel",
        8 => "xcel",
        9 => "nl",
        10 => "ched",
        11 => "rhed",
        12 => "srow",
        _ => "?",
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: tf_otsl <pdf>");
    let bytes = std::fs::read(&path).expect("read");
    let doc = PdfDocument::open(&bytes, None).expect("open");
    let mut layout = LayoutModel::load().expect("layout");
    let mut tf = TableFormer::load().expect("tableformer models missing");
    for (pi, page) in doc.pages.iter().enumerate() {
        let regions = layout
            .predict(&page.image, page.width, page.height)
            .expect("layout");
        // docling resizes the whole page to 1024px height (cv2.INTER_AREA), then
        // crops the table bbox out of *that*. Replicate exactly.
        let sf = 1024.0 / page.image.height() as f32;
        let pw1024 = (page.image.width() as f32 * sf) as u32; // docling: int(w*r)
        let page1024 = docling_pdf::resample::inter_area(&page.image, pw1024, 1024);
        for r in regions.iter().filter(|r| r.label == "table") {
            // bbox (points) → 1024px-page coords: scale*sf = 1024/page_h_pt;
            // docling rounds the crop edges.
            let k = 1024.0 / page.height;
            let x = (r.l * k).round().max(0.0) as u32;
            let y = (r.t * k).round().max(0.0) as u32;
            let x2 = (r.r * k).round() as u32;
            let y2 = (r.b * k).round() as u32;
            let (w, h) = (x2 - x, y2 - y);
            let crop = imageops::crop_imm(&page1024, x, y, w, h).to_image();
            let cells = tf.predict_table_structure(&crop).expect("predict");
            println!(
                "page {} table {}x{}px -> {} cells",
                pi + 1,
                w,
                h,
                cells.len()
            );
            for c in &cells {
                println!(
                    "  r{} c{} {}x{} {} | cxcywh {:.4} {:.4} {:.4} {:.4}",
                    c.row,
                    c.col,
                    c.colspan,
                    c.rowspan,
                    name(c.tag),
                    c.cx,
                    c.cy,
                    c.w,
                    c.h
                );
            }
        }
    }
}
