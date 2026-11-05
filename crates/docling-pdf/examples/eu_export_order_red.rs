//! Public-source reproduction (#984): text-layer export must keep a page
//! header ahead of the page's own text. Usage: `eu_export_order_red <EU 537/2014 DE PDF>`.
use std::{env, fs};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = env::args_os().nth(1).ok_or("expected one source PDF")?;
    let document = docling_pdf::convert_text_layer_pages_opts(
        &fs::read(source)?,
        "EU 537/2014, 2024-01-09 consolidation",
        None,
        docling_pdf::TextOptions {
            word_gap: None,
            keep_typography: true,
        },
    )?;
    let text = document
        .export_to_text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cases = [
        (
            "eu-537-art5-page7-header",
            "02014R0537 — DE — 09.01.2024 — 001.001 — 7",
            "Für die Zwecke dieses Absatzes wird",
        ),
        (
            "eu-537-art16-page18-header",
            "02014R0537 — DE — 09.01.2024 — 001.001 — 18",
            "Artikel 16 Bestellung",
        ),
    ];
    let mut failures = 0;
    for (id, header, body) in cases {
        let headers: Vec<_> = text.match_indices(header).collect();
        let bodies: Vec<_> = text.match_indices(body).collect();
        let passed = headers.len() == 1 && bodies.len() == 1 && headers[0].0 < bodies[0].0;
        println!(
            "{id}: passed={passed} header={:?} body={:?}",
            headers.first().map(|x| x.0),
            bodies.first().map(|x| x.0)
        );
        failures += usize::from(!passed);
    }
    assert_eq!(failures, 0, "text-layer exporter moved source page content");
    Ok(())
}
