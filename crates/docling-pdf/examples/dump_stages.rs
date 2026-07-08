//! Dump preprocessing stages for render-parity debugging against docling.
//! Usage: `... --example dump_stages -- file.pdf <out_dir>`
use docling_pdf::PdfDocument;
use image::imageops;

fn main() {
    let path = std::env::args().nth(1).expect("pdf");
    let out = std::env::args().nth(2).expect("out_dir");
    let bytes = std::fs::read(&path).expect("read");
    let doc = PdfDocument::open(&bytes, None).expect("open");
    let page = &doc.pages[0];
    page.image.save(format!("{out}/my_page.png")).unwrap();
    println!("my_page: {}x{}", page.image.width(), page.image.height());

    let sf = 1024.0 / page.image.height() as f32;
    let pw = (page.image.width() as f32 * sf).round() as u32;
    let p1024 = imageops::thumbnail(&page.image, pw, 1024);
    p1024.save(format!("{out}/my_p1024.png")).unwrap();
    println!("my_p1024: {}x{}", p1024.width(), p1024.height());
}
