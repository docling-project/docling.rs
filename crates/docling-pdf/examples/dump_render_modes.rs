//! Diagnose a duplicated / hidden PDF text layer.
//!
//! Finds the first page whose text objects contain `needle` and lists that page's
//! text objects, each tagged visible or INVISIBLE (text render mode 3), with their
//! bounding box and text. A hidden duplicate layer — e.g. the plain-text copy
//! web exporters stash behind a syntax-highlighted code block — shows up as
//! INVISIBLE objects repeating the visible text, usually at a different position.
//!
//! Usage:
//!   cargo run -p docling-pdf --example dump_render_modes -- file.pdf "LangVersion"

use docling_pdf::pdfium_backend::{debug_text_objects, page_count};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dump_render_modes <pdf> <needle>");
    let needle = std::env::args().nth(2).expect("needle substring required");
    let bytes = std::fs::read(&path).expect("read pdf");
    let pages = page_count(&bytes, None).expect("page count");

    for p in 0..pages as i32 {
        let objs = debug_text_objects(&bytes, p);
        if !objs.iter().any(|o| o.text.contains(&needle)) {
            continue;
        }
        let visible = objs.iter().filter(|o| !o.invisible).count();
        let invisible = objs.iter().filter(|o| o.invisible).count();
        println!(
            "page {p}: {} text object(s) — {visible} visible, {invisible} INVISIBLE\n",
            objs.len()
        );
        for o in &objs {
            let tag = if o.invisible {
                "INVISIBLE"
            } else {
                "visible  "
            };
            let text: String = o.text.chars().take(70).collect();
            println!(
                "  [{tag}] l={:8.2} b={:8.2} r={:8.2} t={:8.2}  {text:?}",
                o.l, o.b, o.r, o.t
            );
        }
        return;
    }
    println!("needle {needle:?} not found in any text object across {pages} page(s)");
}
