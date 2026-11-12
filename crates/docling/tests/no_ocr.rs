//! #244: `no_ocr` — layout + TableFormer without OCR (docling's independent
//! `do_ocr=False`; `skip_ocr` is its pre-2.0 name, #611) — and the
//! missing-OCR-model degradation.
//!
//! A **separate integration-test file on purpose**: these tests point
//! `DOCLING_OCR_REC_ONNX` at a nonexistent path (a missing model must
//! degrade, not fail), and env vars are process-global — `pages.rs` sets the
//! same vars to *real* paths for its OCR tests, including the proof that
//! `no_ocr` never runs OCR on a scan. Each tests/ file is its own binary and
//! process, so the sabotage can't race a sibling suite.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Layout (+ TableFormer) models, with the OCR recognition model deliberately
/// pointed into the void — every test in this binary must work without it.
fn layout_ready_ocr_sabotaged() -> bool {
    let m = repo_root().join(".models");
    let layout = ["layout_heron_int8.onnx", "layout_heron.onnx"]
        .iter()
        .map(|f| m.join(f))
        .find(|p| p.exists());
    let Some(layout) = layout else {
        return false;
    };
    std::env::set_var("DOCLING_LAYOUT_ONNX", layout);
    std::env::set_var(
        "DOCLING_OCR_REC_ONNX",
        m.join("definitely-not-a-model.onnx"),
    );
    let tf = m.join("tableformer");
    let dec = ["decoder_kv.onnx", "decoder_int8.onnx", "decoder.onnx"]
        .iter()
        .map(|f| tf.join(f))
        .find(|p| p.exists());
    if let Some(dec) = dec {
        if tf.join("encoder.onnx").exists() && tf.join("bbox.onnx").exists() {
            std::env::set_var("DOCLING_TABLEFORMER_ENCODER", tf.join("encoder.onnx"));
            std::env::set_var("DOCLING_TABLEFORMER_DECODER", dec);
            std::env::set_var("DOCLING_TABLEFORMER_BBOX", tf.join("bbox.onnx"));
        }
    }
    true
}

/// A digital page with a real table: `no_ocr` — docling's `--no-ocr` since
/// 2.0 (#611) — must keep the layout + table structure that
/// `text_layer_only` throws away, and convert with the OCR model
/// unloadable. `skip_ocr`, its pre-2.0 name, gives the same document.
#[test]
fn no_ocr_keeps_layout_and_tables() {
    if !layout_ready_ocr_sabotaged() {
        eprintln!("skipping: the layout model is not present");
        return;
    }
    let path = repo_root().join("tests/data/pdf/sources/2305.03393v1-pg9.pdf");
    let src = SourceDocument::from_file(&path).expect("table fixture");
    let doc = DocumentConverter::new()
        .no_ocr(true)
        .convert(src)
        .expect("no_ocr conversion succeeds")
        .document;
    let md = doc.export_to_markdown();
    // Structure survived: the fixture's table serializes as a Markdown grid
    // (the text-layer path yields flat paragraphs — no pipes).
    assert!(
        md.contains('|'),
        "expected a Markdown table from layout + TableFormer, got:\n{md}"
    );
    // ...and its digital text layer still reads out.
    assert!(!md.trim().is_empty());
    let alias = DocumentConverter::new()
        .skip_ocr(true)
        .convert(SourceDocument::from_file(&path).expect("table fixture"))
        .expect("skip_ocr conversion succeeds")
        .document
        .export_to_markdown();
    assert_eq!(alias, md, "no_ocr and skip_ocr are one switch");
}

/// #1114: a table pasted as an image under a text caption, converted without
/// OCR. The table region claims no text-layer cell; it used to be dropped,
/// and the caption the matcher had handed it went with it. docling keeps an
/// empty `TableItem` with its caption — so the caption prints in the
/// Markdown, and the JSON has the table (no rows) with the caption as its
/// child.
#[test]
fn a_raster_table_keeps_its_caption_without_ocr() {
    if !layout_ready_ocr_sabotaged() {
        eprintln!("skipping: the layout model is not present");
        return;
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/raster_table/raster_table_caption.pdf");
    let doc = DocumentConverter::new()
        .no_ocr(true)
        .convert(SourceDocument::from_file(&path).expect("fixture"))
        .expect("conversion succeeds")
        .document;
    let caption = "Table 1: Decision matrix for risk insurance";
    let md = doc.export_to_markdown();
    assert!(
        md.lines().any(|l| l == caption),
        "the caption is text-layer text and must print:\n{md}"
    );
    assert!(!md.contains('|'), "no grid of blank cells:\n{md}");
    let json = doc.export_to_json_value();
    let tables = json["tables"].as_array().expect("tables");
    assert_eq!(tables.len(), 1, "the table stays an item");
    assert_eq!(tables[0]["data"]["num_rows"], 0);
    assert_eq!(tables[0]["data"]["num_cols"], 0);
    // The label rule (`Table 1:` next to a table) makes the line a caption
    // whatever the layout model called it, and the table owns it.
    let item = json["texts"]
        .as_array()
        .expect("texts")
        .iter()
        .find(|t| t["text"] == caption)
        .expect("the caption item");
    assert_eq!(item["label"], "caption");
    assert_eq!(item["parent"]["$ref"], "#/tables/0");
    assert_eq!(tables[0]["captions"][0]["$ref"], item["self_ref"]);
}

/// The degradation half of #244: OCR *wanted* (no flag) but the model is
/// missing — the conversion warns and completes instead of erroring (the
/// pre-#244 behavior surfaced as a 422 through docling-serve).
#[test]
fn missing_ocr_model_degrades_instead_of_erroring() {
    if !layout_ready_ocr_sabotaged() {
        eprintln!("skipping: the layout model is not present");
        return;
    }
    let path = repo_root().join("tests/data/scanned/sources/ocr_test_raster.pdf");
    let src = SourceDocument::from_file(&path).expect("scanned fixture");
    let doc = DocumentConverter::new()
        .convert(src)
        .expect("a missing OCR model must degrade, not fail the conversion")
        .document;
    assert!(doc.export_to_markdown().trim().is_empty());
}
