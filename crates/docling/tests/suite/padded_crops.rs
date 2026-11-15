//! A scan under a tiny native text layer reads every line once (#1133): the
//! reporter's page — a JPEG scan plus one native page number
//! (`tests/fixtures/padded_crops/scan_with_page_number.pdf`, `gen.py`). The
//! page number puts the page on the text-layer path, whose text-less regions
//! are detected on half-a-line-padded crops (#945); the `Opening Hours`
//! heading touches its paragraph, the paragraph's crop saw most of the
//! heading too, and both detections were recognised into the heading:
//! `Opening Hours Opemiig Hours`. Needs the models; skips without them so
//! CI without models stays green.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ml_stack_ready() -> bool {
    let root = repo_root();
    let models_ready = ["layout_heron.onnx", "ocr_det.onnx"]
        .iter()
        .all(|m| root.join(".models").join(m).exists());
    models_ready && std::env::set_current_dir(&root).is_ok()
}

#[test]
fn a_heading_touching_its_paragraph_is_read_once() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/padded_crops/scan_with_page_number.pdf");
    let md = DocumentConverter::new()
        .convert(SourceDocument::from_file(path).expect("fixture"))
        .expect("conversion")
        .document
        .export_to_markdown();
    for heading in ["## Opening Hours", "## Borrowing Rules", "## Study Rooms"] {
        assert!(
            md.lines().any(|l| l.trim() == heading),
            "{heading:?} not read exactly once: {md}"
        );
    }
    assert_eq!(md.matches("Hours").count(), 1, "{md}");
    assert_eq!(md.matches("public holidays.").count(), 1, "{md}");
}
