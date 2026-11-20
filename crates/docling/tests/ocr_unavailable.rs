//! #1210: a conversion whose OCR model cannot be loaded degrades (#244) —
//! it succeeds, and the text OCR would have read is missing — and says so
//! on its result (`ConversionResult::ocr_status`) every time, not only in
//! the once-per-process stderr warning. Its own test binary: it points
//! `DOCLING_OCR_REC_ONNX` at nothing for the whole process. Needs the
//! layout model (the pipeline itself must load); skips without it.

use std::path::{Path, PathBuf};

use docling::{ConversionStatus, DocumentConverter, OcrStatus, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn a_missing_ocr_model_is_reported_on_every_result() {
    if !repo_root().join(".models/layout_heron.onnx").exists()
        || std::env::set_current_dir(repo_root()).is_err()
    {
        eprintln!("skipping: layout model not found");
        return;
    }
    // SAFETY: the only test in this binary, set before any thread starts.
    unsafe { std::env::set_var("DOCLING_OCR_REC_ONNX", "/nonexistent/ocr_rec.onnx") };
    let scan =
        || SourceDocument::from_file("tests/data/scanned/sources/ocr_test.pdf").expect("fixture");
    let converter = DocumentConverter::new();
    // Twice through one converter: the second conversion is past the
    // one-time warning, and still has to say it.
    for attempt in 1..=2 {
        let result = converter.convert(scan()).expect("degrades, not fails");
        assert_eq!(
            result.status,
            ConversionStatus::Success,
            "attempt {attempt}"
        );
        match &result.ocr_status {
            Some(OcrStatus::Unavailable { reason }) => {
                assert!(reason.contains("/nonexistent/ocr_rec.onnx"), "{reason}")
            }
            other => panic!("attempt {attempt}: expected unavailable, got {other:?}"),
        }
    }
    // A warm pipeline keeps the failed load (`OcrSlot::Missing`); every
    // conversion on it reports it too.
    let warm = docling::WarmPipeline::new();
    let warm_converter = DocumentConverter::new().warm_pipeline(&warm);
    for attempt in 1..=2 {
        let result = warm_converter.convert(scan()).expect("degrades");
        assert!(
            result
                .ocr_status
                .as_ref()
                .is_some_and(OcrStatus::is_unavailable),
            "warm attempt {attempt}: {:?}",
            result.ocr_status
        );
    }
    // Nothing asked for OCR: no status, whatever the engine's state.
    let skipped = DocumentConverter::new()
        .no_ocr(true)
        .convert(scan())
        .expect("no OCR");
    assert_eq!(skipped.ocr_status, None);
    let digital = DocumentConverter::new()
        .convert(SourceDocument::from_file("tests/data/pdf/sources/2305.03393v1-pg9.pdf").unwrap())
        .expect("digital PDF");
    assert_eq!(digital.ocr_status, None, "a born-digital page needs no OCR");
}
