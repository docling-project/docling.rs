//! PPTX content layers against docling's own output: hidden slides and
//! hidden shapes (#754, docling#4647/#4733), footer placeholders (#755,
//! docling#4746).
//!
//! `tests/data/pptx/hidden_layers/<name>.{md,json,dclg}` is **not** our
//! output: it is what docling 2.140.0's PowerPoint backend writes for
//! `<name>.pptx` (`MsPowerpointDocumentBackend(...).convert()`, then
//! `export_to_markdown()` and `export_to_dict()`); the `.dclg` is the
//! `document.xml` of `save_as_doclang_archive()` by docling 2.148 /
//! docling-core 2.106 (DocLang 0.9), less the part's final newline — the
//! DocLang that `export_to_doclang()` here mirrors. The decks are upstream's test decks, rebuilt by
//! `make_decks.py` next to them. Re-create a reference with upstream's
//! backend, never from our own output.
use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};
use serde_json::Value;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pptx/hidden_layers")
}

/// Drop what never depends on the conversion: the file's origin (hash, path)
/// and the schema version stamp.
fn normalize(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("origin");
            m.remove("version");
            m.values_mut().for_each(normalize);
        }
        Value::Array(a) => a.iter_mut().for_each(normalize),
        _ => {}
    }
}

/// docling.rs keeps a picture's alt text as its description (#676,
/// `created_by: "alt_text"`); docling does not. python-pptx's `add_picture`
/// writes the image's file name as that alt text, so it is the one
/// deliberate difference in these decks.
fn drop_alt_text(doc: &mut Value) {
    for pic in doc["pictures"].as_array_mut().into_iter().flatten() {
        let alt = pic["meta"]["description"]["created_by"] == "alt_text";
        if alt {
            pic.as_object_mut().unwrap().remove("meta");
            pic["annotations"]
                .as_array_mut()
                .unwrap()
                .retain(|a| a["provenance"] != "alt_text");
        }
    }
}

fn convert(name: &str) -> docling_core::DoclingDocument {
    let deck = dir().join(format!("{name}.pptx"));
    DocumentConverter::new()
        .convert(SourceDocument::from_file(&deck).expect("deck"))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .document
}

/// Every deck's Markdown and JSON item tree equal docling's.
#[test]
fn hidden_and_furniture_layers_follow_docling() {
    let mut decks: Vec<String> = fs::read_dir(dir())
        .expect("hidden_layers fixtures")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "pptx"))
        .map(|p| p.file_stem().unwrap().to_str().unwrap().to_string())
        .collect();
    decks.sort();
    assert!(!decks.is_empty());
    let mut failures = Vec::new();
    for name in &decks {
        let document = convert(name);
        let base = dir().join(name);
        let want_md = fs::read_to_string(base.with_extension("md")).expect("reference .md");
        let got_md = document.export_to_markdown();
        if got_md.trim_end() != want_md.trim_end() {
            failures.push(format!(
                "{name}.md:\n--- docling\n{want_md}\n--- ours\n{got_md}"
            ));
        }
        let mut want: Value =
            serde_json::from_str(&fs::read_to_string(base.with_extension("json")).unwrap())
                .unwrap();
        let mut got: Value = serde_json::from_str(&document.export_to_json()).unwrap();
        normalize(&mut want);
        normalize(&mut got);
        drop_alt_text(&mut got);
        if got != want {
            failures.push(format!("{name}.json: item tree differs from docling's"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Every deck's DocLang equals docling's: a hidden slide's title and a
/// hidden table or picture keep their location, the `<layer>` token ahead
/// of it, as hidden text always did; a hidden picture's image stays a data
/// URI, as docling's archive keeps every picture off the body.
#[test]
fn doclang_follows_docling() {
    let mut checked = 0;
    for entry in fs::read_dir(dir()).expect("hidden_layers fixtures") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|e| e != "dclg") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap();
        let want = fs::read_to_string(&path).unwrap();
        assert_eq!(convert(name).export_to_doclang(), want, "{name}");
        checked += 1;
    }
    assert_eq!(checked, 5);
}

/// The hidden content is kept, on the invisible layer: an export that asks
/// for every layer shows it, the speaker notes of a hidden slide stay notes.
#[test]
fn hidden_content_is_kept_on_the_invisible_layer() {
    let all = docling::MarkdownExportOptions {
        layers: docling::ContentLayers::ALL,
        ..Default::default()
    };
    let slide = convert("hidden_slide")
        .export_to_markdown_with_options(&all)
        .0;
    for text in ["Hidden slide", "Backup text", "Notes for Hidden slide"] {
        assert!(slide.contains(text), "{text}: {slide}");
    }
    let group = convert("hidden_group")
        .export_to_markdown_with_options(&all)
        .0;
    for text in [
        "Hidden group first",
        "Hidden group second",
        "Shown group hidden member",
    ] {
        assert!(group.contains(text), "{text}: {group}");
    }
}
