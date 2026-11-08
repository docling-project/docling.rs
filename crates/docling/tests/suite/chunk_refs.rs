//! #1041: a chunk's `doc_items` refs name items of the document's own JSON
//! export — the item each chunk was built from — whatever the backend
//! numbers its items by (the flat nodes, or an item tree).

use std::path::{Path, PathBuf};

use docling::chunker::HierarchicalChunker;
use docling::{DocumentConverter, InputFormat, SourceDocument};
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn convert(src: SourceDocument) -> docling_core::DoclingDocument {
    DocumentConverter::new()
        .convert(src)
        .expect("conversion")
        .document
}

fn resolve<'a>(json: &'a Value, r: &str) -> Option<&'a Value> {
    let (bucket, idx) = r.strip_prefix("#/")?.split_once('/')?;
    json.get(bucket)?.get(idx.parse::<usize>().ok()?)
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The issue's document: the paragraphs after a captioned table point at
/// themselves, and the table's chunk opens with its caption, as docling's
/// `HierarchicalChunker` writes it (`Table 1: Revenue\n\n2024, Revenue = 10`,
/// `[#/texts/0, #/tables/0]`).
#[test]
fn captioned_table_does_not_shift_later_refs() {
    let html = "<html><body><table><caption>Table 1: Revenue</caption>\
                <tr><th>Year</th><th>Revenue</th></tr><tr><td>2024</td><td>10</td></tr></table>\
                <p>Revenue grew in 2024.</p><p>Costs were flat.</p></body></html>";
    let doc = convert(SourceDocument::from_bytes(
        "t.html",
        InputFormat::Html,
        html.as_bytes().to_vec(),
    ));
    let chunks = HierarchicalChunker.chunk(&doc);
    let got: Vec<(String, Vec<String>)> = chunks
        .iter()
        .map(|c| {
            (
                c.text.clone(),
                c.doc_items.iter().map(|i| i.self_ref.clone()).collect(),
            )
        })
        .collect();
    let want = [
        (
            "Table 1: Revenue\n\n2024, Revenue = 10",
            vec!["#/texts/0", "#/tables/0"],
        ),
        ("Revenue grew in 2024.", vec!["#/texts/1"]),
        ("Costs were flat.", vec!["#/texts/2"]),
    ];
    let want: Vec<(String, Vec<String>)> = want
        .iter()
        .map(|(t, r)| (t.to_string(), r.iter().map(|s| s.to_string()).collect()))
        .collect();
    assert_eq!(got, want);
}

/// Across backends — the flat-numbered ones and the item-tree ones (HTML,
/// DOCX, Markdown, JATS, ODF, LaTeX) — every text ref resolves to an item
/// whose text the chunk carries.
#[test]
fn text_refs_name_the_items_their_chunk_carries() {
    let root = repo_root().join("tests/data");
    let fixtures = [
        "html/sources/example_06.html",
        "html/sources/hyperlink_06.html",
        "docx/sources/docx_bold_tail.docx",
        "docx/sources/unit_test_formatting.docx",
        "md/sources/inline_and_formatting.md",
        "jats/sources/pntd.0008301.nxml",
        "odf/sources/odf_presentation_01.odp",
        "latex/sources/example_01.tex",
        "pptx/sources/powerpoint_comments.pptx",
        "webvtt/sources/webvtt_example_02.vtt",
    ];
    let mut failures = Vec::new();
    let mut checked = 0;
    for f in fixtures {
        let doc = convert(SourceDocument::from_file(root.join(f)).expect("fixture"));
        let json = doc.export_to_json_value();
        for c in HierarchicalChunker.chunk(&doc) {
            let text = norm(&c.text);
            for item in c
                .doc_items
                .iter()
                .filter(|i| i.self_ref.starts_with("#/texts/"))
            {
                checked += 1;
                let target = resolve(&json, &item.self_ref)
                    .and_then(|t| t["text"].as_str())
                    .map(norm);
                match target {
                    Some(t) if t.is_empty() || text.contains(&t) => {}
                    other => failures.push(format!(
                        "{f}: {} -> {other:?}, chunk {:?}",
                        item.self_ref,
                        text.chars().take(60).collect::<String>()
                    )),
                }
            }
        }
    }
    assert!(checked > 300, "only {checked} refs checked");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
