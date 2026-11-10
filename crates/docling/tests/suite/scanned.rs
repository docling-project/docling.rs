//! Scanned-PDF end-to-end conformance: the OCR pipeline's markdown for the
//! `tests/data/scanned/` fixtures must match the mirrored docling groundtruth
//! — most importantly the **rotated** scans (`/Rotate` 90/180/270), which
//! regressed invisibly for a long time because nothing consumed this
//! groundtruth.
//!
//! One test walks all four fixtures serially: each conversion runs the full ML
//! stack (layout + OCR), and four concurrent model stacks would only fight for
//! memory. It needs the models (including the `ch` conformance OCR
//! pair the groundtruth was pinned against), so it skips cleanly on checkouts
//! without `download_dependencies.sh` — CI without models must stay green.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};

/// Workspace root (this crate lives at `crates/docling`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Whether every model the scanned path loads is present (layout +
/// the `ch` OCR recognition pair). Model resolution is CWD-relative with an
/// exe-dir fallback and tests run from the crate dir, so this also moves the
/// process to the repo root — the same view the CLI has when run from there.
fn ml_stack_ready() -> bool {
    let root = repo_root();
    let models_ready = ["layout_heron.onnx", "ocr_rec.onnx", "ppocr_keys_v1.txt"]
        .iter()
        .all(|m| root.join(".models").join(m).exists());
    models_ready && std::env::set_current_dir(&root).is_ok()
}

#[test]
fn scanned_fixtures_match_groundtruth() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let converter = DocumentConverter::new().ocr_lang("ch");
    for stem in [
        "ocr_test",
        "ocr_test_rotated_90",
        "ocr_test_rotated_180",
        "ocr_test_rotated_270",
    ] {
        let source = SourceDocument::from_file(format!("tests/data/scanned/sources/{stem}.pdf"))
            .expect("scanned fixture");
        let got = converter
            .convert(source)
            .expect("scanned conversion")
            .document
            .export_to_markdown();
        let want = std::fs::read_to_string(format!("tests/data/scanned/groundtruth/{stem}.md"))
            .expect("groundtruth");
        // The groundtruth files carry no trailing newline; normalize both ends.
        assert_eq!(
            got.trim_end(),
            want.trim_end(),
            "OCR markdown for {stem}.pdf diverged from tests/data/scanned/groundtruth/{stem}.md"
        );
    }
}

/// Content-based orientation detection (#225): the `ocr_test_raster*`
/// fixtures embed the *same* lossless raster physically rotated inside the
/// page (`/Rotate 0` on all of them — the metadata pass never fires), so the
/// only way to read them is to detect the orientation from the pixels. All
/// four must OCR byte-identically to the shared groundtruth: pixel-exact 90°
/// un-rotation means the recognizer sees the same bitmap every time.
#[test]
fn raster_rotated_fixtures_match_groundtruth() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let converter = DocumentConverter::new().ocr_lang("ch");
    let want = std::fs::read_to_string("tests/data/scanned/groundtruth/ocr_test_raster.md")
        .expect("groundtruth");
    for stem in [
        "ocr_test_raster",
        "ocr_test_raster_rot_90",
        "ocr_test_raster_rot_180",
        "ocr_test_raster_rot_270",
    ] {
        let source = SourceDocument::from_file(format!("tests/data/scanned/sources/{stem}.pdf"))
            .expect("raster fixture");
        let got = converter
            .convert(source)
            .expect("raster conversion")
            .document
            .export_to_markdown();
        assert_eq!(
            got.trim_end(),
            want.trim_end(),
            "orientation detection failed to normalize {stem}.pdf"
        );
    }
}

/// #1093: an image input's page is sized from the file's resolution, as
/// docling's `ImageDocumentBackend` sizes it (`pixels × 72 / dpi`): the QR
/// bill is 1021 × 1423 px at 300 DPI, so its page is 245.04 × 341.52 pt and
/// every box lies on it. The models still read the file's own pixels, so the
/// text is what the 1 px = 1 pt page read: the same Markdown either way.
#[test]
fn an_image_page_is_sized_from_its_resolution() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let source = SourceDocument::from_file("tests/data/scanned/sources/qr_bill_example.jpg")
        .expect("image fixture");
    let doc = DocumentConverter::new()
        .convert(source)
        .expect("image conversion")
        .document;
    let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    let size = &json["pages"]["1"]["size"];
    let (w, h) = (
        size["width"].as_f64().unwrap(),
        size["height"].as_f64().unwrap(),
    );
    assert!(
        (w - 245.04).abs() < 0.01 && (h - 341.52).abs() < 0.01,
        "{w} x {h}"
    );
    let mut boxes = 0;
    for kind in ["texts", "tables", "pictures"] {
        for item in json[kind].as_array().unwrap() {
            for prov in item["prov"].as_array().into_iter().flatten() {
                let b = &prov["bbox"];
                let (l, r) = (b["l"].as_f64().unwrap(), b["r"].as_f64().unwrap());
                let (t, bt) = (b["t"].as_f64().unwrap(), b["b"].as_f64().unwrap());
                assert!(l >= -0.01 && r <= w + 0.01, "{kind}: {b}");
                assert!(t.max(bt) <= h + 0.01 && t.min(bt) >= -0.01, "{kind}: {b}");
                boxes += 1;
            }
        }
    }
    assert!(
        boxes > 10,
        "the bill reads as text, not a picture: {boxes} boxes"
    );
}
