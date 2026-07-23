//! `convert_text_layer` (the `pdf-text` / wasm32 path) must produce the same
//! extraction as the full pipeline's `no_ocr` flag: both run the pure-Rust
//! text parser through the orphan-region assembly, differing only in how the
//! page is opened (lopdf vs pdfium). Runs under the default (ml) feature so
//! both entries exist to compare.

#![cfg(feature = "ml")]

#[test]
fn text_layer_matches_no_ocr() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/pdf/sources/code_and_formula.pdf"
    ))
    .expect("corpus pdf");

    // Tests run with CWD = the crate dir; point the pdfium loader at the
    // checkout's `.pdfium/lib` (the no_ocr side still opens pages via pdfium).
    if std::env::var("PDFIUM_DYNAMIC_LIB_PATH").is_err() {
        std::env::set_var(
            "PDFIUM_DYNAMIC_LIB_PATH",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../.pdfium/lib"),
        );
    }

    let text_layer = docling_pdf::convert_text_layer(&bytes, "code_and_formula.pdf")
        .expect("text-layer conversion");
    let no_ocr = match docling_pdf::convert_with_options(
        &bytes,
        None,
        "code_and_formula.pdf",
        true, // no_table_former (moot: no_ocr skips it anyway)
        true, // no_ocr — the path convert_text_layer mirrors
        docling_pdf::EnrichmentOptions::default(),
        None,
        None,
    ) {
        Ok(doc) => doc,
        // The comparison baseline needs the pdfium shared library, which CI's
        // model-free test job doesn't have — the equivalence claim is only
        // checkable where a full local setup exists (scripts/dev/pdfium.sh).
        Err(docling_pdf::PdfError::Pdfium(e)) if e.contains("LoadLibraryError") => {
            eprintln!("skipping equivalence check: pdfium unavailable ({e})");
            return;
        }
        Err(e) => panic!("no_ocr conversion: {e:?}"),
    };

    let a = text_layer.export_to_markdown();
    assert!(!a.trim().is_empty(), "text layer should extract");
    assert_eq!(a, no_ocr.export_to_markdown());
}

#[test]
fn scanned_pdf_yields_empty_document() {
    // No content stream at all — the no-text-layer contract is an empty doc,
    // not an error (callers decide whether to fall back to OCR).
    let doc = docling_pdf::convert_text_layer(b"%PDF-1.4\n%%EOF", "scan.pdf").expect("no error");
    assert!(doc.nodes.is_empty());
}
