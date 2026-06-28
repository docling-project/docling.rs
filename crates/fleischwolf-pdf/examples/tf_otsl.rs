//! Verify TableFormer inference: run it on every table region of a PDF and print
//! the predicted OTSL structure. Usage: `... --example tf_otsl -- file.pdf`

use fleischwolf_pdf::layout::LayoutModel;
use fleischwolf_pdf::tableformer::TableFormer;
use fleischwolf_pdf::PdfDocument;
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
        for r in regions.iter().filter(|r| r.label == "table") {
            let s = page.scale;
            let x = (r.l * s).max(0.0) as u32;
            let y = (r.t * s).max(0.0) as u32;
            let w = ((r.r - r.l) * s) as u32;
            let h = ((r.b - r.t) * s) as u32;
            let crop = imageops::crop_imm(&page.image, x, y, w, h).to_image();
            let otsl = tf.predict_otsl(&crop).expect("predict");
            let rows = otsl.iter().filter(|&&t| t == 9).count();
            let cols = otsl.iter().take_while(|&&t| t != 9).count();
            println!(
                "page {} table {}x{}px -> {} tokens, {} rows x {} cols",
                pi + 1,
                w,
                h,
                otsl.len(),
                rows,
                cols
            );
            println!(
                "  {}",
                otsl.iter().map(|&t| name(t)).collect::<Vec<_>>().join(" ")
            );
        }
    }
}
