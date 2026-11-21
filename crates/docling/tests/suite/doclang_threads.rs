//! An item over several boxes through DocLang (#1207): docling-core's
//! `DocLangTextSerializer` writes one element per `prov` entry, the
//! fragments sharing a `<thread thread_id="N"/>` head, a `<page_break/>`
//! between two on different pages; its deserializer joins a thread back into
//! one item. Ported from docling-core's `test_serialization_doclang.py`
//! (`test_multi_prov_text_emits_thread`,
//! `test_cross_page_paragraph_emits_thread_and_page_break`,
//! `test_thread_ids_unique_across_fragmented_components`).
//!
//! `tests/data/doclang/upstream/threads/<name>.{input.json,dclg,
//! deserialized.json}` are docling-core's own fixtures
//! (`tests/data/doc/<name>/{input.json,serialized.dclg.xml,deserialized.json}`
//! at docling-core 2.107.1): the document, docling-core's DocLang for it
//! (written without the version attribute), and what docling-core reads that
//! DocLang back as.

use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, InputFormat, SourceDocument};
use docling_core::DoclingDocument;
use serde_json::Value;

const CASES: [&str; 2] = ["multi_prov_thread", "cross_page_paragraph"];

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/doclang/upstream/threads")
}

fn convert(name: &str, format: InputFormat, bytes: Vec<u8>) -> DoclingDocument {
    DocumentConverter::new()
        .convert(SourceDocument::from_bytes(name, format, bytes))
        .expect("convert")
        .document
}

fn json(doc: &DoclingDocument) -> Value {
    serde_json::from_str(&doc.export_to_json()).unwrap()
}

/// Each text item's text and `prov`.
fn texts(v: &Value) -> Vec<(Value, Value)> {
    v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["text"].clone(), t["prov"].clone()))
        .collect()
}

/// `test_multi_prov_text_emits_thread` and its cross-page twin: the
/// document's DocLang is docling-core's, byte for byte.
#[test]
fn multi_prov_text_writes_a_thread() {
    for name in CASES {
        let input = fs::read(dir().join(format!("{name}.input.json"))).unwrap();
        let doc = convert(&format!("{name}.json"), InputFormat::JsonDocling, input);
        let want = fs::read_to_string(dir().join(format!("{name}.dclg"))).unwrap();
        let got = doc
            .export_to_doclang()
            .replacen("<doclang version=\"0.9\">", "<doclang>", 1);
        assert_eq!(got.trim_end(), want.trim_end(), "{name}");
        assert_eq!(
            got.matches("<thread thread_id=\"1\"/>").count(),
            2,
            "{name}"
        );
        assert_eq!(
            got.contains("<page_break/>"),
            name == "cross_page_paragraph",
            "{name}"
        );
    }
}

/// A docling-JSON input's further `prov` entries are kept: the JSON export
/// writes the item as it was read.
#[test]
fn multi_prov_text_keeps_every_prov() {
    for name in CASES {
        let input = fs::read(dir().join(format!("{name}.input.json"))).unwrap();
        let want: Value = serde_json::from_slice(&input).unwrap();
        let doc = convert(&format!("{name}.json"), InputFormat::JsonDocling, input);
        assert_eq!(texts(&json(&doc)), texts(&want), "{name}");
    }
}

/// docling-core's deserializer on its own DocLang: the thread is one item,
/// its text joined, one `prov` entry per fragment with its `charspan`, each
/// on its page; the Markdown prints one paragraph.
#[test]
fn thread_reads_back_as_one_item() {
    for name in CASES {
        let xml = fs::read(dir().join(format!("{name}.dclg"))).unwrap();
        let doc = convert(&format!("{name}.dclg"), InputFormat::XmlDoclang, xml);
        let want: Value = serde_json::from_str(
            &fs::read_to_string(dir().join(format!("{name}.deserialized.json"))).unwrap(),
        )
        .unwrap();
        let got = json(&doc);
        assert_eq!(texts(&got), texts(&want), "{name}");
        assert_eq!(got["pages"], want["pages"], "{name}");
        let text = want["texts"][0]["text"].as_str().unwrap();
        assert_eq!(doc.export_to_markdown().trim_end(), text, "{name}");
    }
}

/// `test_thread_ids_unique_across_fragmented_components`, its paragraph
/// half: each fragmented item takes the next `thread_id`, in reading order.
/// (Upstream also threads a list whose items sit on two pages; the flat
/// writer keeps such a list one `<list>`, unthreaded.)
#[test]
fn thread_ids_are_document_scoped() {
    let input = fs::read(dir().join("multi_prov_thread.input.json")).unwrap();
    let mut v: Value = serde_json::from_slice(&input).unwrap();
    // A second fragmented paragraph after the first.
    let mut second = v["texts"][0].clone();
    second["self_ref"] = "#/texts/1".into();
    v["texts"].as_array_mut().unwrap().push(second);
    v["body"]["children"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"$ref": "#/texts/1"}));
    let doc = convert(
        "two.json",
        InputFormat::JsonDocling,
        serde_json::to_vec(&v).unwrap(),
    );
    let xml = doc.export_to_doclang();
    assert_eq!(xml.matches("thread_id=\"1\"").count(), 2, "{xml}");
    assert_eq!(xml.matches("thread_id=\"2\"").count(), 2, "{xml}");
    assert!(!xml.contains("thread_id=\"3\""), "{xml}");
    // And both read back as the two items they were.
    let back = convert("two.dclg", InputFormat::XmlDoclang, xml.into_bytes());
    assert_eq!(texts(&json(&back)).len(), 2);
}
