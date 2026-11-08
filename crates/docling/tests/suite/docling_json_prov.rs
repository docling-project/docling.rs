//! #899: a docling-JSON input keeps its provenance. docling's own
//! `multi_page.json` (five pages, 53 items with a `prov`) read back and
//! exported again carries every page and every box, and its DocLang export
//! is the one docling-core 2.106 derives from the same file (the
//! `<location>` tokens and `<page_break/>`s). Needs no model.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, InputFormat, SourceDocument};
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn convert(bytes: Vec<u8>) -> docling_core::DoclingDocument {
    DocumentConverter::new()
        .convert(SourceDocument::from_bytes(
            "multi_page.json",
            InputFormat::JsonDocling,
            bytes,
        ))
        .expect("docling-JSON input")
        .document
}

fn source() -> Vec<u8> {
    std::fs::read(repo_root().join("tests/data/pdf/groundtruth/multi_page.json")).expect("fixture")
}

/// Every `texts` item by its text, with its first `prov` entry.
fn provs(root: &Value) -> Vec<(String, String, Value)> {
    root["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["text"].as_str().unwrap_or("").to_string(),
                t["label"].as_str().unwrap_or("").to_string(),
                t["prov"][0].clone(),
            )
        })
        .collect()
}

#[test]
fn pages_and_prov_survive_the_json_round_trip() {
    let src: Value = serde_json::from_slice(&source()).unwrap();
    let out: Value = serde_json::from_str(&convert(source()).export_to_json()).unwrap();

    // The page map, sizes included.
    let pages = out["pages"].as_object().expect("pages");
    assert_eq!(pages.len(), 5);
    for (no, page) in src["pages"].as_object().unwrap() {
        assert_eq!(pages[no]["size"], page["size"], "page {no}");
        assert_eq!(pages[no]["page_no"], page["page_no"], "page {no}");
    }

    let want = provs(&src);
    let got = provs(&out);
    assert_eq!(got.len(), want.len(), "every text item is back");
    let with_prov = got.iter().filter(|(_, _, p)| p.is_object()).count();
    assert_eq!(with_prov, 53, "every item keeps its prov");
    for (text, label, prov) in &want {
        let (_, _, back) = got
            .iter()
            .find(|(t, ..)| t == text)
            .unwrap_or_else(|| panic!("{text:?} missing from the export"));
        assert_eq!(back["page_no"], prov["page_no"], "{text:?}");
        assert_eq!(back["bbox"]["coord_origin"], prov["bbox"]["coord_origin"]);
        // A list item's box travels on the 512 grid (see `grid_location`):
        // within a grid cell. Anything else is exact to the serializer's
        // two decimals.
        let tolerance = if label == "list_item" { 1.0 } else { 0.011 };
        for k in ["l", "t", "r", "b"] {
            let (a, b) = (
                back["bbox"][k].as_f64().unwrap(),
                prov["bbox"][k].as_f64().unwrap(),
            );
            assert!((a - b).abs() <= tolerance, "{text:?} {k}: {a} vs {b}");
        }
        if label != "list_item" {
            assert_eq!(back["charspan"], prov["charspan"], "{text:?}");
        }
    }
}

/// The DocLang export carries the location tokens and page breaks
/// docling-core derives from the same provenance: byte-for-byte its own
/// render of the file (docling-core 2.106, DocLang 0.9), but for the list
/// markers — docling writes the item's `marker` (`·`, `1.`) into the
/// `<ldiv>`, which this backend does not carry to DocLang (not part of #899).
#[test]
fn doclang_export_matches_docling_core() {
    let want = std::fs::read_to_string(
        repo_root().join("crates/docling/tests/fixtures/json_prov/multi_page.doclang"),
    )
    .expect("reference render");
    let got = convert(source()).export_to_doclang();
    let norm = |s: &str| {
        // `<ldiv>` / `<marker>m</marker>` / `</ldiv>` → `<ldiv/>`.
        let lines: Vec<&str> = s.lines().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let l = lines[i];
            if l.trim() == "<ldiv>"
                && lines
                    .get(i + 1)
                    .is_some_and(|m| m.trim().starts_with("<marker>"))
                && lines.get(i + 2).is_some_and(|c| c.trim() == "</ldiv>")
            {
                out.push(l.replace("<ldiv>", "<ldiv/>"));
                i += 3;
                continue;
            }
            out.push(l.to_string());
            i += 1;
        }
        out.join("\n")
    };
    assert_eq!(norm(&got), norm(&want));
}

/// The markers and wrappers feed the JSON and DocLang exports only: the
/// Markdown of the file is what it was without any `prov` or `pages`.
#[test]
fn markdown_is_unchanged_by_the_provenance() {
    let mut bare: Value = serde_json::from_slice(&source()).unwrap();
    bare["pages"] = serde_json::json!({});
    for key in ["texts", "tables", "pictures"] {
        for item in bare[key].as_array_mut().into_iter().flatten() {
            item["prov"] = serde_json::json!([]);
        }
    }
    let with = convert(source()).export_to_markdown();
    let without = convert(serde_json::to_vec(&bare).unwrap()).export_to_markdown();
    assert_eq!(with, without);
    assert!(with.contains("The Evolution of the Word Processor"));
}
