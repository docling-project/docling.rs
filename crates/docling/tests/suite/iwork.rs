//! Apple iWork input (#213, #318, #383): the iWork backend pinned against
//! committed Markdown groundtruth. The backend is pure Rust and deterministic
//! — no models, no gating. Two corpora (repo root):
//!
//! - `tests/data/pages/` mirrors upstream docling's Pages fixtures and its
//!   committed groundtruth (docling#4062): `.pages` is a conformance format,
//!   so the Markdown must match byte-for-byte (modulo the trailing newline
//!   upstream's test harness strips) and the JSON structure is cross-checked.
//!   Never regenerated here — it is upstream's output.
//! - `tests/data/iwork/` pins our own fixtures (Numbers/Keynote extension,
//!   the libetonyek Pages bundles). Regenerate after an intentional change with:
//!
//! ```bash
//! DOCLING_RS_REGEN=1 cargo test -p docling --test suite iwork::
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/iwork")
}

fn pages_corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/pages")
}

fn convert_pages(name: &str) -> docling::DoclingDocument {
    let source = SourceDocument::from_file(pages_corpus().join("sources").join(name)).unwrap();
    DocumentConverter::new()
        .convert(source)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .document
}

/// Upstream's Pages corpus (docling#4062): Markdown byte-for-byte against
/// docling's committed groundtruth.
#[test]
fn pages_fixtures_match_upstream_groundtruth() {
    let sources = pages_corpus().join("sources");
    let mut entries: Vec<_> = fs::read_dir(&sources)
        .expect("pages sources")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    assert_eq!(entries.len(), 4, "expected upstream's four Pages fixtures");
    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap();
        let md = convert_pages(name).export_to_markdown();
        let expected = fs::read_to_string(
            pages_corpus()
                .join("groundtruth")
                .join(format!("{name}.md")),
        )
        .unwrap_or_else(|_| panic!("{name}: missing upstream groundtruth"));
        // docling's test harness writes the groundtruth without the final
        // newline our serializer always emits.
        assert_eq!(
            md.trim_end_matches('\n'),
            expected.trim_end_matches('\n'),
            "{name}: Markdown drifted from upstream groundtruth"
        );
    }
}

/// The JSON shape upstream's groundtruth pins and our exporter can carry:
/// tables in the text flow, reviewer comments as bare notes-layer texts
/// referenced from the items they annotate (no `comment_section` group), and
/// character formatting on the items. The furniture layer (page header/footer,
/// footnotes) stays out of the JSON, as for every backend (see MIGRATION.md).
#[test]
fn pages_json_follows_upstream_structure() {
    let expected = |name: &str| -> serde_json::Value {
        let path = pages_corpus()
            .join("groundtruth")
            .join(format!("{name}.json"));
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    };
    // Body children on the body layer: upstream's JSON also lists the
    // furniture-layer items (page header/footer, footnotes) under the body,
    // which our exporter leaves out for every backend.
    let body_order = |v: &serde_json::Value| -> Vec<String> {
        v["body"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["$ref"].as_str().unwrap().to_string())
            .filter(|r| {
                let item = r.trim_start_matches("#/").split('/').fold(v, |acc, k| {
                    match k.parse::<usize>() {
                        Ok(i) => &acc[i],
                        Err(_) => &acc[k],
                    }
                });
                item["content_layer"] == "body"
            })
            .collect()
    };
    let texts = |v: &serde_json::Value, layer: &str| -> Vec<(String, String)> {
        v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["content_layer"] == layer)
            .map(|t| {
                (
                    t["label"].as_str().unwrap().to_string(),
                    t["text"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    for name in ["pages_2013.pages", "pages_iwork09.pages"] {
        let ours: serde_json::Value =
            serde_json::from_str(&convert_pages(name).export_to_json()).unwrap();
        let theirs = expected(name);
        // The table sits where its anchor is in the text, not appended.
        assert_eq!(body_order(&ours), body_order(&theirs), "{name}: body order");
        assert_eq!(
            texts(&ours, "body"),
            texts(&theirs, "body"),
            "{name}: body texts"
        );
        assert_eq!(ours["tables"], theirs["tables"], "{name}: tables");
    }
    let name = "pages_iwork09_comments.pages";
    let ours: serde_json::Value =
        serde_json::from_str(&convert_pages(name).export_to_json()).unwrap();
    let theirs = expected(name);
    assert_eq!(body_order(&ours), body_order(&theirs), "{name}: body order");
    assert_eq!(
        texts(&ours, "body"),
        texts(&theirs, "body"),
        "{name}: body texts"
    );
    assert_eq!(
        texts(&ours, "notes"),
        texts(&theirs, "notes"),
        "{name}: comments"
    );
    assert!(
        ours["groups"].as_array().unwrap().is_empty(),
        "{name}: no groups"
    );
    let refs = |v: &serde_json::Value| -> Vec<(String, serde_json::Value)> {
        v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t.get("comments").is_some())
            .map(|t| {
                (
                    t["self_ref"].as_str().unwrap().to_string(),
                    t["comments"].clone(),
                )
            })
            .collect()
    };
    assert_eq!(refs(&ours), refs(&theirs), "{name}: comment back-refs");
    let name = "pages_iwork09_formatted.pages";
    let ours: serde_json::Value =
        serde_json::from_str(&convert_pages(name).export_to_json()).unwrap();
    let theirs = expected(name);
    assert_eq!(body_order(&ours), body_order(&theirs), "{name}: body order");
    // Character formatting reaches the Markdown (bold/italic/strike/link
    // markers) and DocLang (inline runs); the JSON exporter carries text items
    // without docling's `formatting`/`hyperlink` fields for every backend.
    assert_eq!(
        texts(&ours, "body"),
        texts(&theirs, "body"),
        "{name}: body texts"
    );
}

#[test]
fn iwork_fixtures_match_groundtruth() {
    let regen = std::env::var_os("DOCLING_RS_REGEN").is_some();
    let sources = corpus().join("sources");
    let mut checked = 0;
    let mut entries: Vec<_> = fs::read_dir(&sources)
        .expect("iwork sources")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let source = SourceDocument::from_file(&path).expect("iwork fixture");
        let result = DocumentConverter::new().convert(source);
        // docling's fixture: Pages encrypts members with a compression method
        // ZIP does not define instead of setting the encryption flag — the
        // error must still say "password-protected".
        if name.contains("password_protected") {
            let err = result.expect_err("password-protected fixture fails");
            let text = err.to_string();
            assert!(
                text.contains("password-protected"),
                "{name}: expected a password-protected error, got: {text:?}"
            );
            // Typed (#636): not decryptable — no password opens an iWork
            // package here, so a caller must not prompt for one.
            let kind = err.encryption().expect("typed encryption error");
            assert!(
                matches!(kind, docling::EncryptionError::NotDecryptable(_))
                    && !kind.needs_password(),
                "{name}: {kind:?}"
            );
            checked += 1;
            continue;
        }
        let md = result
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .document
            .export_to_markdown();
        let gt_path = corpus().join("groundtruth").join(format!("{name}.md"));
        if regen {
            fs::write(&gt_path, &md).expect("write groundtruth");
        } else {
            let expected = fs::read_to_string(&gt_path)
                .unwrap_or_else(|_| panic!("{name}: missing groundtruth (DOCLING_RS_REGEN=1)"));
            assert_eq!(md, expected, "{name}: Markdown drifted from groundtruth");
        }
        checked += 1;
    }
    assert!(
        checked >= 7,
        "expected the full iwork corpus, saw {checked}"
    );
}

/// A pre-2013 Pages package (`index.xml`) with page furniture and a template
/// placeholder: only the body text survives, as in docling's IWA path.
#[test]
fn legacy_pages_furniture_and_ghost_text_stay_out() {
    let ns = "http://developer.apple.com/namespaces/sf";
    let xml = format!(
        r#"<?xml version="1.0"?>
<sl:document xmlns:sl="http://developer.apple.com/namespaces/sl" xmlns:sf="{ns}">
  <sf:stylesheet>
    <sf:paragraphstyle sf:name="Body" sf:ident="ps-body"/>
    <sf:paragraphstyle sf:name="Heading 1" sf:ident="ps-h1"/>
  </sf:stylesheet>
  <sf:text-storage>
    <sf:text-body>
      <sf:p sf:style="ps-h1">Real heading</sf:p>
      <sf:p sf:style="ps-body">Real body text.<sf:ghost-text>Lorem ipsum</sf:ghost-text> after.</sf:p>
    </sf:text-body>
    <sf:header><sf:text-body><sf:p>Running header</sf:p></sf:text-body></sf:header>
    <sf:footer><sf:text-body><sf:p>Page footer</sf:p></sf:text-body></sf:footer>
    <sf:footnotes><sf:text-storage><sf:text-body><sf:p>A footnote body</sf:p></sf:text-body></sf:text-storage></sf:footnotes>
  </sf:text-storage>
</sl:document>"#
    );
    let source = SourceDocument::from_bytes(
        "furniture.pages",
        docling::InputFormat::Pages,
        zip_with(&[("index.xml", xml.as_bytes())]),
    );
    let md = DocumentConverter::new()
        .convert(source)
        .unwrap()
        .document
        .export_to_markdown();
    assert_eq!(md, "## Real heading\n\nReal body text. after.\n");
}

/// A zip that is neither an IWA package nor an '09 document is reported as
/// such (docling's message), not as a generic parse failure.
#[test]
fn zip_without_pages_index_is_rejected() {
    let source = SourceDocument::from_bytes(
        "not_really.pages",
        docling::InputFormat::Pages,
        zip_with(&[("word/document.xml", b"<w:document/>")]),
    );
    let err = DocumentConverter::new().convert(source).unwrap_err();
    assert!(
        err.to_string().contains("not a Pages document"),
        "unexpected error: {err}"
    );
}

fn zip_with(members: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        for (name, bytes) in members {
            zip.start_file::<_, ()>(*name, Default::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}

/// A ZIP that is not a Keynote document of any generation — no `Index/`, no
/// `Index.zip`, no `index.apxl` (a Pages-style `index.xml` is not one) —
/// fails with docling's message naming the three layouts (#466), not a
/// generic parse failure.
#[test]
fn non_keynote_package_reports_clearly() {
    // A minimal zip with only an index.xml member.
    let mut buf = Vec::new();
    {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        zip.start_file::<_, ()>("index.xml", Default::default())
            .unwrap();
        zip.write_all(b"<document/>").unwrap();
        zip.finish().unwrap();
    }
    let source = SourceDocument::from_bytes("old", docling::InputFormat::Keynote, buf.clone());
    let err = DocumentConverter::new().convert(source).unwrap_err();
    assert!(
        err.to_string().contains("index.apxl"),
        "unexpected error: {err}"
    );
    // Numbers reads an iWork '09 `index.xml` (#1031): one without a sheet
    // is an empty spreadsheet, as in docling.
    let source = SourceDocument::from_bytes("old", docling::InputFormat::Numbers, buf);
    let doc = DocumentConverter::new().convert(source).unwrap().document;
    assert!(doc.export_to_markdown().trim().is_empty());
}
