//! METS-GBS pages that cannot be read are left out, the other pages convert
//! (#1147, docling#5484's `test_page_larger_than_max_image_pixels_is_skipped`).
//! A page whose hOCR declares a size past the image cap is skipped — it was
//! converted with geometry its image contradicts — and one bad page image no
//! longer fails the whole archive. The result is `PartialSuccess` with one
//! page error; the pages keep their numbers.
//!
//! Needs the layout model; skips without it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use docling::{ConversionStatus, DocumentConverter, InputFormat, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ml_stack_ready() -> bool {
    let root = repo_root();
    root.join(".models/layout_heron.onnx").exists() && std::env::set_current_dir(&root).is_ok()
}

/// The upstream archive with page 1's hOCR page box replaced.
fn with_first_page_box(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut builder = tar::Builder::new(Vec::new());
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if path.to_string_lossy().ends_with("00000010.html") {
            let text = String::from_utf8(data).unwrap();
            assert!(text.contains(from), "the fixture's page box");
            data = text.replacen(from, to, 1).into_bytes();
        }
        let mut header = entry.header().clone();
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append(&header, data.as_slice()).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn page_numbers(json: &serde_json::Value) -> Vec<u64> {
    let mut pages: Vec<u64> = json["texts"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["prov"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p["page_no"].as_u64())
        .collect();
    pages.sort_unstable();
    pages.dedup();
    pages
}

#[test]
fn a_page_over_the_image_cap_is_skipped() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let fixture = std::fs::read("tests/data/mets_gbs/sources/32044009881525_select.tar.gz")
        .expect("METS fixture");
    let convert = |bytes: Vec<u8>| {
        DocumentConverter::new()
            .convert(SourceDocument::from_bytes(
                "32044009881525_select.tar.gz",
                InputFormat::MetsGbs,
                bytes,
            ))
            .expect("conversion")
    };
    let whole = convert(fixture.clone());
    assert_eq!(whole.status, ConversionStatus::Success);
    let whole_json: serde_json::Value =
        serde_json::from_str(&whole.document.export_to_json()).unwrap();
    assert_eq!(page_numbers(&whole_json), [1, 2, 3]);

    // docling's edit, `bbox 0 0 1582 2495;` → 20000 × 20000; past our
    // per-side cap of 30000 it takes 40000.
    let edited = with_first_page_box(&fixture, "bbox 0 0 1582 2495;", "bbox 0 0 40000 40000;");
    let cut = convert(edited);
    assert_eq!(cut.status, ConversionStatus::PartialSuccess);
    assert_eq!(cut.errors.len(), 1, "{:?}", cut.errors);
    assert_eq!(cut.errors[0].page_no, Some(1));
    assert!(cut.errors[0]
        .error_message
        .starts_with("Page 1 failed to parse"));
    let cut_json: serde_json::Value = serde_json::from_str(&cut.document.export_to_json()).unwrap();
    assert_eq!(
        page_numbers(&cut_json),
        [2, 3],
        "the pages keep their numbers"
    );
    // Pages 2 and 3 read as in the whole conversion.
    let page_texts = |json: &serde_json::Value, page: u64| -> Vec<String> {
        json["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["prov"][0]["page_no"].as_u64() == Some(page))
            .map(|t| t["text"].as_str().unwrap_or("").to_string())
            .collect()
    };
    for page in [2, 3] {
        assert_eq!(
            page_texts(&cut_json, page),
            page_texts(&whole_json, page),
            "page {page}"
        );
    }
}
