//! HEIC/HEIF image input (#211), end to end. Compiled with the `heif`
//! feature (pure-Rust heifer, #1249; in the default set) and, like the other pipeline
//! e2es, skips cleanly when the ONNX models aren't installed —
//! model-free CI stays green.

#![cfg(feature = "heif")]

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn heic_photo_ocrs_through_the_image_pipeline() {
    let root = repo_root();
    for needed in [".models/layout_heron_int8.onnx", ".models/ocr_rec_en.onnx"] {
        if !root.join(needed).exists() {
            eprintln!("skipping heic e2e: {needed} not found");
            return;
        }
    }
    // Model resolution is CWD-relative; tests run from the crate dir.
    std::env::set_current_dir(&root).expect("chdir to repo root");
    let source = SourceDocument::from_file("tests/data/heif/sources/inspection_report.heic")
        .expect("heic fixture");
    let md = DocumentConverter::new()
        .convert(source)
        .expect("heic conversion")
        .document
        .export_to_markdown();
    // Big, plain, high-contrast text — asserts the plumbing (heifer decode →
    // layout + OCR), not recognition quality.
    for needle in ["Site Inspection Report", "Building 7", "sprinkler"] {
        assert!(md.contains(needle), "{needle:?} missing:\n{md}");
    }
}
