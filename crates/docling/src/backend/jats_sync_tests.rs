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
