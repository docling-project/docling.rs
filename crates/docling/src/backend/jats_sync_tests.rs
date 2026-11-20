//! The JATS cases of the 2026-11 upstream sync (#1208), ported from
//! docling's `tests/test_backend_jats.py`: each test builds the article
//! upstream's `convert_jats_body` / `convert_jats_article_meta` build and
//! asserts what upstream asserts, on the Markdown and on the item tree.

use serde_json::Value;

use super::jats::JatsBackend;
use super::DeclarativeBackend;
use crate::format::InputFormat;
use crate::source::SourceDocument;
use docling_core::DoclingDocument;

fn convert(xml: &str) -> DoclingDocument {
    let src = SourceDocument::from_bytes("t.nxml", InputFormat::XmlJats, xml.as_bytes().to_vec());
    JatsBackend::default().convert(&src).unwrap()
}

/// docling's `convert_jats_body`.
fn body_doc(body: &str) -> DoclingDocument {
    convert(&format!(
        r#"<article xmlns:xlink="http://www.w3.org/1999/xlink"><front><article-meta>
        <title-group><article-title>Body Test</article-title></title-group>
        </article-meta></front><body>{body}</body></article>"#
    ))
}

/// docling's `convert_jats_article_meta` (a `<body>` only when given).
fn meta_doc(meta: &str, body: &str) -> DoclingDocument {
    let body = if body.is_empty() {
        String::new()
    } else {
        format!("<body>{body}</body>")
    };
    convert(&format!(
        r#"<article xmlns:xlink="http://www.w3.org/1999/xlink" article-type="research-article"><front><article-meta>{meta}</article-meta></front>{body}</article>"#
    ))
}

fn json(doc: &DoclingDocument) -> Value {
    serde_json::from_str(&doc.export_to_json()).unwrap()
}

/// docling#5244: a heading joins its non-empty `label` and `title`.
#[test]
fn section_heading_keeps_title_next_to_label() {
    let md = body_doc(
        "<sec><label>1.</label><title>Introduction</title><p>Intro text.</p>
          <sec><label>1.1.</label><title>Background</title><p>Background text.</p></sec>
        </sec>
        <sec><title>Methods</title><p>Methods text.</p></sec>
        <sec><label>3.</label><p>Unnamed section text.</p></sec>
        <sec><label>4.</label><title/><p>Empty title text.</p></sec>",
    )
    .export_to_markdown();
    for want in [
        "## 1. Introduction\n\nIntro text.",
        "### 1.1. Background\n\nBackground text.",
        "## Methods\n\nMethods text.",
        "## 3.\n\nUnnamed section text.",
        "## 4.\n\nEmpty title text.",
    ] {
        assert!(md.contains(want), "{want:?} in\n{md}");
    }
}

/// docling#5244: an abstract section's heading joins label and title too.
#[test]
fn abstract_section_heading_keeps_title_next_to_label() {
    let md = meta_doc(
        "<title-group><article-title>Labelled Abstract Test</article-title></title-group>
      <abstract>
        <sec><label>1.</label><title>Background</title><p>Background text.</p></sec>
      </abstract>",
        "",
    )
    .export_to_markdown();
    assert!(md.contains("### 1. Background\n\nBackground text."), "{md}");
}

/// `(label, text)` of every text item in the JSON, in order.
fn labelled_texts(doc: &DoclingDocument) -> Vec<(String, String)> {
    json(doc)["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["label"].as_str().unwrap().to_string(),
                t["text"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// docling#5283 (`test_jats_preformat_is_code_block`): a `<preformat>` or
/// `<code>` is one code item with its line breaks and indentation, in a
/// section or inside a paragraph — dropped, or run into the paragraph, before.
#[test]
fn preformat_is_code_block() {
    let code = "for i in range(3):\n    print(i)";
    for body in [
        "<p>Run it:</p><preformat>{code}</preformat><p>Done.</p>",
        "<p>Run it:<preformat>{code}</preformat>Done.</p>",
        "<p>Run it:</p><code>{code}</code><p>Done.</p>",
    ] {
        let body = body.replace("{code}", code);
        let doc = body_doc(&format!("<sec><title>T</title>{body}</sec>"));
        let items: Vec<_> = labelled_texts(&doc)
            .into_iter()
            .filter(|(l, _)| l == "text" || l == "code")
            .collect();
        let want = [("text", "Run it:"), ("code", code), ("text", "Done.")]
            .map(|(l, t)| (l.to_string(), t.to_string()));
        assert_eq!(items, want, "{body}");
        let md = doc.export_to_markdown();
        assert!(md.contains(&format!("Run it:\n\n```\n{code}\n```\n\nDone.")), "{md}");
    }
}

/// docling#5283 (`test_jats_code_block_keeps_its_language`).
#[test]
fn code_block_keeps_its_language() {
    let doc = body_doc(r#"<sec><title>T</title><code language="python">print(1)</code></sec>"#);
    let v = json(&doc);
    let code: Vec<_> = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["label"] == "code")
        .collect();
    assert_eq!(code.len(), 1);
    assert_eq!(code[0]["text"], "print(1)");
    assert_eq!(code[0]["code_language"], "Python");
}

/// docling#5290 (`test_jats_footnote_group_keeps_external_link_target`): a
/// `<fn-group>` footnote keeps its `<ext-link>` target.
#[test]
fn footnote_group_keeps_external_link_target() {
    let doc = convert(
        r#"<article xmlns:xlink="http://www.w3.org/1999/xlink"><front><article-meta><title-group><article-title>Back Matter Test</article-title></title-group></article-meta></front><body><p>Body text.</p></body><back><fn-group><fn><p>See <ext-link ext-link-type="uri" xlink:href="https://example.org/data">the data</ext-link> now.</p></fn></fn-group></back></article>"#,
    );
    let v = json(&doc);
    let texts = v["texts"].as_array().unwrap();
    let links: Vec<_> = texts.iter().filter(|t| t["text"] == "the data").collect();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["label"], "footnote");
    assert_eq!(links[0]["hyperlink"], "https://example.org/data");
    for text in ["See ", " now."] {
        let run = texts.iter().find(|t| t["text"] == text).unwrap();
        assert!(run.get("hyperlink").is_none_or(Value::is_null), "{run}");
    }
    let md = doc.export_to_markdown();
    assert!(
        md.contains("- See [the data](https://example.org/data) now."),
        "{md}"
    );
}
