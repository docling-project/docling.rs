//! DocLang input against docling's own output: mixed-content text and
//! captions (#780, docling-core#865). A located `<text>` with formatting
//! runs, and any mixed `<caption>`, `<footnote>`, `<page_header>` or
//! `<page_footer>`, is a host item of its label — its provenance and layer
//! kept — over an inline group of the runs; a mixed caption is no longer
//! flattened (`Table1: results`) or, for a table, dropped from the Markdown.
//!
//! `tests/data/doclang/upstream/<name>.{expected.md,json,expected.html}` is
//! **not** our output: it is what docling 2.140.0's `DocLangDocumentBackend`
//! (docling-core 2.102.0's `DocLangDocDeserializer`) writes for
//! `<name>.dclg` (`.convert()`, then `export_to_markdown()`,
//! `export_to_dict()` and `export_to_html()`). Re-create a reference with
//! upstream's backend, never from our own output.
//!
//! Recaptured with docling 2.148.0 / docling-core 2.106.0 for DocLang 0.9's
//! reading rule and the faithful inline join (#997): a whitespace sequence
//! reads as one space on its run, none at the block's edges. One field moved
//! by hand: 2.148's deserializer names the document `Document`; the
//! references keep the file stem, as the 2.140 capture did.
use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};
use serde_json::Value;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/doclang/upstream")
}

/// Drop what never depends on the conversion: the file's origin, the schema
/// version and the document name (docling names a DocLang document
/// `Document`, we name it after the file).
fn normalize(v: &mut Value) {
    if let Value::Object(m) = v {
        m.remove("name");
    }
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("origin");
                m.remove("version");
                m.values_mut().for_each(walk);
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(v);
}

/// The `<div class='page'>` of an HTML export: the content, without the
/// stylesheet and the `<title>`.
fn page(html: &str) -> &str {
    let start = html.find("<div class='page'>").unwrap_or(0);
    let end = html[start..]
        .find("</div>\n</body>")
        .map_or(html.len(), |e| start + e);
    &html[start..end]
}

/// Every document's Markdown, JSON item tree and HTML page equal docling's.
#[test]
fn mixed_text_and_captions_follow_docling() {
    let mut docs: Vec<PathBuf> = fs::read_dir(dir())
        .expect("doclang/upstream fixtures")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "dclg"))
        .collect();
    docs.sort();
    assert_eq!(docs.len(), 3);
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
        let want_html = fs::read_to_string(dir().join(format!("{name}.expected.html"))).unwrap();
        let got_html = document.export_to_html();
        if page(&got_html).trim() != page(&want_html).trim() {
            failures.push(format!(
                "{name} html:\n--- docling\n{}\n--- ours\n{}",
                page(&want_html),
                page(&got_html)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
