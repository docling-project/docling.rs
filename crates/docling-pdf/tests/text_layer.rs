//! `convert_text_layer` (the `pdf-text` / wasm32 path) must produce the same
//! extraction as the full pipeline's `no_ocr` flag: both run the pure-Rust
//! text parser through the orphan-region assembly, differing only in how the
//! page is opened (lopdf vs pdfium). Runs under the default (ml) feature so
//! both entries exist to compare. (The scanned-table e2e below shares this
//! binary rather than its own: every docling-pdf test target statically links
//! onnxruntime, so one target carries all the pipeline e2es.)

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
        true,  // no_table_former (moot: no_ocr skips it anyway)
        true,  // no_ocr — the path convert_text_layer mirrors
        false, // force_full_page_ocr (ignored under no_ocr)
        false, // no_text_panels (moot: no_ocr never demotes)
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

/// #173: a scanned (image-only) page with an uncaptioned bar chart above a
/// bordered data table. The chart must stay a picture (line-height uniformity
/// gate) and the table must extract with its OCR'd cell text — region-scoped
/// OCR skips table interiors, so the words come from the dedicated
/// `ocr_table_words` pass; without it the cell matcher saw no words and the
/// table dissolved. Needs pdfium + the layout/OCR/TableFormer models, so it
/// skips (like the pdfium skip above) on a model-free CI checkout.
#[test]
fn scanned_page_extracts_table_and_keeps_chart() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if std::env::var("PDFIUM_DYNAMIC_LIB_PATH").is_err() {
        std::env::set_var("PDFIUM_DYNAMIC_LIB_PATH", root.join(".pdfium/lib"));
    }
    for needed in [
        ".pdfium/lib/libpdfium.so",
        "models/layout_heron_int8.onnx",
        "models/ocr_rec_en.onnx",
        "models/tableformer/encoder.onnx",
    ] {
        if !root.join(needed).exists() {
            eprintln!("skipping scanned-table e2e: {needed} not found");
            return;
        }
    }
    // Model resolution is CWD-relative; tests run from the crate dir.
    std::env::set_current_dir(&root).expect("chdir to repo root");
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/ocr/sources/scanned_chart_table.pdf"
    ))
    .expect("scanned fixture");
    let doc =
        docling_pdf::convert(&bytes, None, "scanned_chart_table.pdf").expect("scanned conversion");
    let md = doc.export_to_markdown();
    assert!(
        md.contains("<!-- image -->"),
        "uncaptioned chart must keep its picture crop:\n{md}"
    );
    // The 8x4 bordered table: header + 7 body rows, cells carrying OCR text.
    let rows: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
    assert!(
        rows.len() >= 9,
        "expected a header + 7-row table, got {} pipe rows:\n{md}",
        rows.len()
    );
    for cell in ["Aquifer", "Guarani", "Nubian", "380", "540"] {
        assert!(md.contains(cell), "table cell {cell:?} missing:\n{md}");
    }
}

#[test]
fn scanned_pdf_yields_empty_document() {
    // No content stream at all — the no-text-layer contract is an empty doc,
    // not an error (callers decide whether to fall back to OCR).
    let doc = docling_pdf::convert_text_layer(b"%PDF-1.4\n%%EOF", "scan.pdf").expect("no error");
    assert!(doc.nodes.is_empty());
}
