//! A scan under a partial text layer (#999, docling#4928): the reporter's
//! case — one typed-in native word, `420` in Helvetica 14, drawn over
//! `scanned_chart_table.pdf`'s page image
//! (`tests/fixtures/partial_text_layer/scan_with_native_word.pdf`). The one
//! word put the page on the text-layer path, its table region was not
//! text-less, so it was never OCR'd, and TableFormer matched the native word
//! alone: the seven-row table read `| 420 |`. Needs the models; skips
//! without them so CI without models stays green.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ml_stack_ready() -> bool {
    let root = repo_root();
    let models_ready = ["layout_heron.onnx", "ocr_rec.onnx", "ppocr_keys_v1.txt"]
        .iter()
        .all(|m| root.join(".models").join(m).exists())
        && root.join(".models/tableformer").exists();
    models_ready && std::env::set_current_dir(&root).is_ok()
}

#[test]
fn a_typed_in_word_does_not_hide_the_scanned_table() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/partial_text_layer/scan_with_native_word.pdf");
    let md = DocumentConverter::new()
        .convert(SourceDocument::from_file(path).expect("fixture"))
        .expect("conversion")
        .document
        .export_to_markdown();
    // The OCR'd header and rows reach the table beside the native value…
    for row in [
        "Aquifer system",
        "Ogallala",
        "Guarani",
        "Volga",
        "540",
        "7.4",
    ] {
        assert!(md.contains(row), "{row:?} missing from the table: {md}");
    }
    let rows: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
    assert_eq!(rows.len(), 9, "header, rule and seven rows: {md}");
    // …and the value is read once: the native `420` wins over the OCR line
    // on its own pixels (docling's PDF-first merge).
    assert_eq!(md.matches("420").count(), 1, "{md}");
}
