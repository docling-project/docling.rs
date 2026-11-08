//! PPTX shape geometry and links against docling's own output: shapes
//! inside a moved, resized, nested or flipped group (#766, docling#4859),
//! a hyperlink on a whole shape (#767, docling#4787), and their DocLang
//! locations on a top-left origin like docling's.
//!
//! `tests/data/pptx/shapes/<name>.{md,json}` is **not** our output: it is
//! what docling 2.140.0's PowerPoint backend writes for `<name>.pptx`
//! (`MsPowerpointDocumentBackend(...).convert()`, then `export_to_markdown()`
//! and `export_to_dict()`; `<name>.dclg`, where present, its
//! `export_to_doclang()` by docling 2.148 / docling-core 2.106, DocLang 0.9). The decks are upstream's test decks, rebuilt by
//! `make_decks.py` next to them. Re-create a reference with upstream's
//! backend, never from our own output.
use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};
use serde_json::Value;

const DECKS: &[&str] = &[
    "group_bbox",
    "list_link",
    "relative_link",
    "shape_link",
    "slide_jump",
];

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pptx/shapes")
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

fn convert(name: &str) -> docling_core::DoclingDocument {
    let deck = dir().join(format!("{name}.pptx"));
    DocumentConverter::new()
        .convert(SourceDocument::from_file(&deck).expect("deck"))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .document
}

/// Every deck's Markdown and JSON item tree (provenance boxes and links
/// included) equal docling's.
#[test]
fn shapes_follow_docling() {
    let mut failures = Vec::new();
    for name in DECKS {
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
        if got != want {
            failures.push(format!(
                "{name}.json: item tree differs from docling's\n--- docling\n{}\n--- ours\n{}",
                serde_json::to_string_pretty(&want["texts"]).unwrap(),
                serde_json::to_string_pretty(&got["texts"]).unwrap()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The issue's case in slide inches: a text box in a group moved to
/// (4in, 3in) is drawn there, not at its child-space `(1in, 2in)`.
#[test]
fn a_moved_group_moves_its_shapes() {
    let document = convert("group_bbox");
    let json: Value = serde_json::from_str(&document.export_to_json()).unwrap();
    let moved = json["texts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["text"] == "moved")
        .expect("moved text");
    let bbox = &moved["prov"][0]["bbox"];
    let inch = |v: &Value| v.as_f64().unwrap() / 914_400.0;
    assert_eq!(
        [
            inch(&bbox["l"]),
            inch(&bbox["t"]),
            inch(&bbox["r"]),
            inch(&bbox["b"])
        ],
        [4.0, 3.0, 5.0, 4.0]
    );
}

/// Each text item's link, by text.
fn links(name: &str) -> Vec<(String, Option<String>)> {
    let json: Value = serde_json::from_str(&convert(name).export_to_json()).unwrap();
    json["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["text"].as_str().unwrap().to_string(),
                t["hyperlink"].as_str().map(str::to_string),
            )
        })
        .collect()
}

/// docling#4787's four cases: every paragraph or list item of a linked
/// shape carries the link, a target without a scheme stays a path, and a
/// jump to another slide is no link.
#[test]
fn a_link_on_a_whole_shape_reaches_its_text() {
    let some = |s: &str| Some(s.to_string());
    assert_eq!(
        links("shape_link"),
        [
            ("No link here".into(), None),
            ("Watch the talk".into(), some("https://example.com/talk")),
            ("Second line".into(), some("https://example.com/talk")),
        ]
    );
    let deck = convert("shape_link");
    assert!(deck
        .export_to_markdown()
        .contains("[Watch the talk](https://example.com/talk)"));
    // DocLang: docling-core's `<href>` head, ahead of the location tokens.
    let doclang = deck.export_to_doclang();
    assert!(
        doclang.contains("<text>\n    <href uri=\"https://example.com/talk\"/>\n    <location")
            && doclang.contains("Watch the talk"),
        "{doclang}"
    );
    assert_eq!(
        links("list_link"),
        [
            ("First point".into(), some("https://example.com/points")),
            ("Second point".into(), some("https://example.com/points")),
        ]
    );
    assert_eq!(
        links("relative_link"),
        [("Open the appendix".into(), some("appendix/notes.docx"))]
    );
    assert_eq!(links("slide_jump"), [("Go to the next slide".into(), None)]);
}

/// DocLang equals docling's for the decks with a stored `.dclg`: each
/// shape's `<location>` measured from the slide's top-left corner, as
/// docling-core derives it from the top-left provenance box (the y axis used
/// to be flipped), and a linked item's `<href>` ahead of its location.
/// (`slide_jump`'s empty second slide adds a trailing `<page_break/>` docling
/// leaves out, so it has no `.dclg`.)
#[test]
fn doclang_follows_docling() {
    let mut checked = 0;
    for name in DECKS {
        let Ok(want) = fs::read_to_string(dir().join(name).with_extension("dclg")) else {
            continue;
        };
        checked += 1;
        assert_eq!(convert(name).export_to_doclang(), want, "{name}");
    }
    assert_eq!(checked, 4);
}
