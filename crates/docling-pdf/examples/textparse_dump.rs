//! Dump the pure-Rust text parser's line cells for a PDF page (text + top-left
//! box), for comparison against docling-parse. Usage:
//!   cargo run -p docling-pdf --example textparse_dump -- <pdf> [needle]
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: textparse_dump <pdf> [needle]");
    let needle = std::env::args().nth(2);
    let bytes = std::fs::read(&path).expect("read");
    let pages = docling_pdf::textparse::pdf_textlines(&bytes);
    // TSV mode: emit `pageidx\tl\tt\tr\tb\ttext` for the injection harness.
    if std::env::var("TSV_OUT").is_ok() {
        for (pi, (_w, _h, cells)) in pages.iter().enumerate() {
            for c in cells {
                let t = c.text.replace(['\t', '\n'], " ");
                println!(
                    "{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}",
                    pi, c.l, c.t, c.r, c.b, t
                );
            }
        }
        return;
    }
    for (pi, (w, h, cells)) in pages.iter().enumerate() {
        println!(
            "page {} ({:.0}x{:.0}) {} line cells",
            pi + 1,
            w,
            h,
            cells.len()
        );
        for c in cells {
            if let Some(n) = &needle {
                if !c.text.contains(n.as_str()) {
                    continue;
                }
            }
            println!("  l={:6.1} t={:6.1} r={:6.1} | {}", c.l, c.t, c.r, c.text);
        }
        if pi == 0 {
            break;
        }
    }
}
