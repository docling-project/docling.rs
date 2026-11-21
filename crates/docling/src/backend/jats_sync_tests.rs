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
        assert!(
            md.contains(&format!("Run it:\n\n```\n{code}\n```\n\nDone.")),
            "{md}"
        );
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

/// `export_to_markdown(included_content_layers={BODY, FURNITURE})`.
fn md_with_furniture(doc: &DoclingDocument) -> String {
    let options = docling_core::MarkdownExportOptions {
        layers: docling_core::ContentLayers {
            body: true,
            furniture: true,
            notes: false,
            invisible: false,
            background: false,
        },
        ..Default::default()
    };
    doc.export_to_markdown_with_options(&options).0
}

/// docling's `_furniture_statements`: the `(text, hyperlink)` runs of each
/// furniture text, the runs of one inline group together, in order.
fn furniture_statements(doc: &DoclingDocument) -> Vec<Vec<(String, Option<String>)>> {
    let v = json(doc);
    let mut keys: Vec<String> = Vec::new();
    let mut runs: Vec<Vec<(String, Option<String>)>> = Vec::new();
    for item in v["texts"].as_array().unwrap() {
        if item["content_layer"] != "furniture" {
            continue;
        }
        let mut key = item["self_ref"].as_str().unwrap().to_string();
        if let Some(parent) = item["parent"]["$ref"].as_str() {
            if let Some(idx) = parent.strip_prefix("#/groups/") {
                let group = &v["groups"][idx.parse::<usize>().unwrap()];
                if group["label"] == "inline" {
                    assert_eq!(group["content_layer"], "furniture");
                    key = parent.to_string();
                }
            }
        }
        let run = (
            item["text"].as_str().unwrap().to_string(),
            item["hyperlink"].as_str().map(str::to_string),
        );
        match keys.iter().position(|k| *k == key) {
            Some(i) => runs[i].push(run),
            None => {
                keys.push(key);
                runs.push(vec![run]);
            }
        }
    }
    runs
}

fn runs(v: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    v.iter()
        .map(|(t, h)| (t.to_string(), h.map(str::to_string)))
        .collect()
}

/// docling#4866 (`test_jats_license_and_funding_statements_are_furniture`).
#[test]
fn license_and_funding_statements_are_furniture() {
    let doc = meta_doc(
        r#"<title-group><article-title>Control</article-title></title-group>
      <permissions><license xlink:href="https://example.org/c3-licence"><license-p>CHARLIE-LICENCE terms of the
      <ext-link ext-link-type="uri" xlink:href="https://example.org/c3-licence">licence</ext-link>.</license-p></license></permissions>
      <abstract><p>CHARLIE-ABSTRACT paragraph.</p></abstract>
      <funding-group><funding-statement>CHARLIE-FUNDING statement.</funding-statement></funding-group>"#,
        "<sec><title>S</title><p>Body.</p></sec>",
    );
    assert_eq!(
        furniture_statements(&doc),
        [
            runs(&[
                ("CHARLIE-LICENCE terms of the ", None),
                ("licence", Some("https://example.org/c3-licence")),
                (".", None),
            ]),
            runs(&[("CHARLIE-FUNDING statement.", None)]),
        ]
    );
    let body = doc.export_to_markdown();
    assert!(body.contains("CHARLIE-ABSTRACT paragraph."));
    assert!(!body.contains("CHARLIE-LICENCE") && !body.contains("CHARLIE-FUNDING"));
    let md = md_with_furniture(&doc);
    let at = |s: &str| md.find(s).unwrap_or_else(|| panic!("{s} in {md}"));
    assert!(at("Body.") < at("CHARLIE-LICENCE") && at("CHARLIE-LICENCE") < at("CHARLIE-FUNDING"));
}

/// docling#4866 (`test_jats_copyright_and_each_license_paragraph_are_kept_in_order`).
#[test]
fn copyright_and_each_license_paragraph_are_kept_in_order() {
    let doc = meta_doc(
        r#"<title-group><article-title>Permissions</article-title></title-group>
      <permissions>
        <copyright-statement>© The Authors 2026.</copyright-statement>
        <copyright-year>2026</copyright-year>
        <license license-type="cc-by" xlink:href="https://creativecommons.org/licenses/by/4.0/">
          <license-p>Distributed under the CC BY licence.</license-p>
          <license-p>Funded by SCOAP<sup>3</sup></license-p>
        </license>
      </permissions>
      <funding-group>
        <award-group><funding-source>Agency</funding-source></award-group>
        <funding-statement>First grant.</funding-statement>
        <funding-statement>Second grant.</funding-statement>
      </funding-group>"#,
        "",
    );
    assert_eq!(
        furniture_statements(&doc),
        [
            runs(&[("© The Authors 2026.", None)]),
            runs(&[("Distributed under the CC BY licence.", None)]),
            runs(&[("Funded by SCOAP", None), ("3", None)]),
            runs(&[("First grant.", None)]),
            runs(&[("Second grant.", None)]),
        ]
    );
    let v = json(&doc);
    let supers: Vec<_> = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["formatting"]["script"] == "super")
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert_eq!(supers, ["3"]);
}

/// docling#5216 (`test_jats_author_footnotes_are_furniture[none|single|multiple]`).
#[test]
fn author_footnotes_are_furniture() {
    let cases: [&[&str]; 3] = [
        &[],
        &["Author contribution marker."],
        &["Author contribution marker.", "Conflict statement marker."],
    ];
    for notes in cases {
        let fns: String = notes
            .iter()
            .map(|n| format!("<fn><p>{n}</p></fn>"))
            .collect();
        let author_notes = if notes.is_empty() {
            String::new()
        } else {
            format!("<author-notes>{fns}</author-notes>")
        };
        let doc = meta_doc(
            &format!(
                "<title-group><article-title>Author Footnote Test</article-title></title-group>
                {author_notes}"
            ),
            "<p>Body text.</p>",
        );
        let v = json(&doc);
        let footnotes: Vec<_> = v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["label"] == "footnote")
            .collect();
        let texts: Vec<_> = footnotes
            .iter()
            .map(|t| t["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, notes);
        assert!(footnotes.iter().all(|t| t["content_layer"] == "furniture"));
        let want: Vec<_> = notes.iter().map(|n| runs(&[(n, None)])).collect();
        assert_eq!(furniture_statements(&doc), want);
        let body = ["# Author Footnote Test", "Body text."];
        assert_eq!(doc.export_to_markdown().trim_end(), body.join("\n\n"));
        let all: Vec<&str> = body.iter().chain(notes).copied().collect();
        assert_eq!(md_with_furniture(&doc).trim_end(), all.join("\n\n"));
    }
}

/// docling#5216
/// (`test_jats_author_footnote_preserves_formatting_link_formula_and_paragraphs`).
#[test]
fn author_footnote_preserves_formatting_link_formula_and_paragraphs() {
    let doc = meta_doc(
        r#"<title-group><article-title>Author Footnote Test</article-title></title-group>
        <author-notes>
          <fn><label>1</label><p>See <italic><ext-link
            xlink:href="https://example.org/statement">disclosure</ext-link></italic>
            and <inline-formula><tex-math>x+1</tex-math></inline-formula>.</p>
            <p>Further details.</p></fn>
        </author-notes>"#,
        "",
    );
    let v = json(&doc);
    let texts = v["texts"].as_array().unwrap();
    let disclosure: Vec<_> = texts
        .iter()
        .filter(|t| t["label"] == "footnote" && t["text"] == "disclosure")
        .collect();
    assert_eq!(disclosure.len(), 1);
    let disclosure = disclosure[0];
    assert_eq!(disclosure["formatting"]["italic"], true);
    assert_eq!(disclosure["hyperlink"], "https://example.org/statement");
    assert!(texts
        .iter()
        .filter(|t| t["label"] == "footnote")
        .all(|t| t["content_layer"] == "furniture"));
    let formula = texts.iter().find(|t| t["label"] == "formula").unwrap();
    assert_eq!(formula["text"], "x+1");
    assert_eq!(formula["content_layer"], "furniture");
    assert_eq!(disclosure["parent"], formula["parent"]);
    let group = disclosure["parent"]["$ref"].as_str().unwrap();
    let group = &v["groups"][group
        .strip_prefix("#/groups/")
        .unwrap()
        .parse::<usize>()
        .unwrap()];
    assert_eq!(group["label"], "inline");
    assert_eq!(group["content_layer"], "furniture");
    assert_eq!(
        doc.export_to_markdown().trim_end(),
        "# Author Footnote Test"
    );
    let md = md_with_furniture(&doc);
    assert!(!md.contains("Author Notes"), "{md}");
    assert!(md.contains("1 See"), "{md}");
    assert!(md.contains("https://example.org/statement"), "{md}");
    assert!(md.contains("$x+1$. Further details."), "{md}");
}

/// docling#5216
/// (`test_jats_author_footnotes_share_furniture_with_article_statements`).
#[test]
fn author_footnotes_share_furniture_with_article_statements() {
    let doc = meta_doc(
        "<title-group><article-title>Article metadata</article-title></title-group>
        <author-notes>
          <fn><p>First author note.</p></fn>
          <fn><p>Second author note.</p></fn>
        </author-notes>
        <permissions><license><license-p>License statement.</license-p></license></permissions>
        <funding-group><funding-statement>Funding statement.</funding-statement></funding-group>",
        "<p>Body text.</p><fn-group><fn><p>Body footnote.</p></fn></fn-group>",
    );
    assert_eq!(
        furniture_statements(&doc),
        [
            runs(&[("First author note.", None)]),
            runs(&[("Second author note.", None)]),
            runs(&[("License statement.", None)]),
            runs(&[("Funding statement.", None)]),
        ]
    );
    let v = json(&doc);
    let body_fn = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["text"] == "Body footnote.")
        .unwrap();
    assert_eq!(body_fn["label"], "footnote");
    assert_eq!(body_fn["content_layer"], "body");
}

/// docling#5017 (`test_stream_loads_local_figure`): bytes plus the `file:`
/// URI (or path) of where they live — the file itself absent — resolve a
/// local figure under `image_sources=local`; a root-only `file:///` names no
/// directory, and without the opt-in nothing is read.
#[test]
fn in_memory_source_names_its_local_location() {
    use crate::backend::images::{ImagePolicy, ImageSources};
    let root = std::env::temp_dir().join(format!("docling-jats-uri-{}", std::process::id()));
    let dir = root.join("space 中文");
    std::fs::create_dir_all(&dir).unwrap();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(7, 5, image::Rgb([0, 255, 0])))
        .save_with_format(dir.join("figure.png"), image::ImageFormat::Png)
        .unwrap();
    let xml = r#"<article xmlns:xlink="http://www.w3.org/1999/xlink"><front><article-meta><title-group><article-title>Figure test</article-title></title-group></article-meta></front><body><p>Before figure.</p><fig><label>Fig. 1</label><caption><p>Green figure.</p></caption><graphic xlink:href="figure.png"/></fig><p>After figure.</p></body></article>"#;
    let path = dir.join("article.nxml");
    // `Path::as_uri`: every byte outside the unreserved set escaped.
    let encoded: String = path
        .to_string_lossy()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let file_uri = format!("file://{encoded}");
    let localhost = file_uri.replacen("file:///", "file://LoCaLhOsT/", 1);
    let image_of = |uri: &str, sources: ImageSources| {
        let src = SourceDocument::from_bytes("article.nxml", InputFormat::XmlJats, xml.into())
            .with_source_uri(uri);
        let doc = JatsBackend {
            images: ImagePolicy::new(sources),
        }
        .convert(&src)
        .unwrap();
        doc.nodes.iter().find_map(|n| match n {
            docling_core::Node::Picture { image, .. } => Some(image.is_some()),
            _ => None,
        })
    };
    if !cfg!(windows) {
        for uri in [path.to_string_lossy().as_ref(), &file_uri, &localhost] {
            assert_eq!(image_of(uri, ImageSources::Local), Some(true), "{uri}");
            assert_eq!(image_of(uri, ImageSources::None), Some(false), "{uri}");
        }
    }
    assert_eq!(image_of("file:///", ImageSources::Local), Some(false));
    let _ = std::fs::remove_dir_all(&root);
}
