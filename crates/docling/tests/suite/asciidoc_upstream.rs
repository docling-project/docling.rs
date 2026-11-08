//! AsciiDoc against docling's own output, for every document upstream's
//! `tests/test_backend_asciidoc.py` converts: lists that nest by indent and
//! by marker kind, end at the first other block and leave abbreviations alone
//! (#774, docling#4721/#4822/#4827), `*bold*` / `_italic_` / `` `mono` ``
//! marks in paragraphs (#775, docling#4824), tables kept whole across blank
//! and wrapped lines and unclosed block delimiters (#776,
//! docling#4761/#4662), and the 2.135 fixes around them (comments, block
//! attributes, one-cell table rows, escaped pipes, block titles —
//! docling#4431/#4378).
//!
//! `tests/data/asciidoc/upstream/<name>.{expected.md,json}` is **not** our
//! output: it is what docling 2.140.0's `AsciiDocBackend` writes for
//! `<name>.adoc` (`.convert()`, then `export_to_markdown()` and
//! `export_to_dict()`). Each `<name>` is the upstream test that converts the
//! document (`_2`, `_3`… for a test's further documents); upstream's corpus
//! files are mirrored in `tests/data/asciidoc/sources/` instead. The
//! `edge_*` documents are our own probes of where docling.rs used to differ:
//! the line under a bare table (dropped), a paragraph pending when a list
//! opens (a line of the list, an inline one glued to the item), an
//! unterminated literal block (it keeps the file's last, empty line), and
//! comments inside example blocks and lists, around a paragraph's lines, a
//! `----` inside `////` and an unclosed `////` (#863, captured from
//! docling 2.142.1). Re-create
//! a reference with upstream's backend, never from our own output. All 56
//! were re-run through docling 2.142.1: only the two comment documents
//! (`comment_block_hides_content`, `line_comments_not_body_text`) moved —
//! their comments are notes-layer items since docling#4663 (2.141, #863) —
//! and those two JSON references are 2.142.1's. `edge_comments_in_blocks_and_lists`
//! is docling `main`'s at 90234a7: its `/////` comment block closes only on
//! an identical line since docling#4394, so it runs to the end.
//!
//! The documents whose output the inline whitespace contract moves (#997:
//! `**bold**,` with no invented space, `**st**rong`) are docling 2.148.0's.
//! All 56 were re-run through docling 2.149.0 for #1024/#1025: four moved,
//! and those references are 2.149.0's — a block title is one line
//! (docling#4391: `block_title_without_floating_target_keeps_content`,
//! `text_before_block_title_is_not_merged_with_text_after`) and a span takes
//! its grid positions (docling#5010: `incomplete_table_does_not_emit_an_empty_table_2`,
//! `rowspan_only_cell_specifier_keeps_the_row`).
use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};
use serde_json::Value;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/asciidoc/upstream")
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

/// Every document's Markdown and JSON item tree equal docling's.
#[test]
fn documents_follow_docling() {
    let mut docs: Vec<PathBuf> = fs::read_dir(dir())
        .expect("asciidoc/upstream fixtures")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "adoc"))
        .collect();
    docs.sort();
    assert_eq!(docs.len(), 57);
    let mut failures = Vec::new();
    for path in &docs {
        let name = path.file_stem().unwrap().to_str().unwrap();
        let document = DocumentConverter::new()
            .convert(SourceDocument::from_file(path).expect("document"))
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .document;
        let want_md = fs::read_to_string(dir().join(format!("{name}.expected.md"))).unwrap();
        let got_md = document.export_to_markdown();
        if got_md.trim_end() != want_md.trim_end() {
            failures.push(format!(
                "{name} md:\n--- docling\n{want_md}\n--- ours\n{got_md}"
            ));
        }
        let mut want: Value =
            serde_json::from_str(&fs::read_to_string(dir().join(format!("{name}.json"))).unwrap())
                .unwrap();
        let mut got: Value = serde_json::from_str(&document.export_to_json()).unwrap();
        normalize(&mut want);
        normalize(&mut got);
        if got != want {
            failures.push(format!("{name} json: item tree differs from docling's"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
