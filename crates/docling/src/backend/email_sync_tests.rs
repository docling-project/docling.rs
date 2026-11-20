//! The email cases of the 2026-11 upstream sync (#1208), ported from
//! docling's `tests/test_backend_email.py`: each test builds the message
//! upstream's builds and asserts what upstream asserts. The expected
//! Markdown is what docling 2.151.1 writes for the same bytes.

use serde_json::Value;

use super::email::EmailBackend;
use super::DeclarativeBackend;
use crate::format::InputFormat;
use crate::source::SourceDocument;
use docling_core::DoclingDocument;

/// One MIME part: `(content type with parameters, extra headers, body)`.
pub(super) type Part<'a> = (&'a str, &'a str, &'a str);

/// A message with `headers`, one body part, or a `multipart/{kind}` of
/// `parts`.
pub(super) fn message(headers: &str, kind: Option<&str>, parts: &[Part]) -> Vec<u8> {
    let mut m = format!("{headers}MIME-Version: 1.0\r\n");
    match kind {
        None => {
            let (ct, extra, body) = parts[0];
            m.push_str(&format!("Content-Type: {ct}\r\n{extra}\r\n{body}\r\n"));
        }
        Some(kind) => {
            m.push_str(&format!(
                "Content-Type: multipart/{kind}; boundary=\"BOUNDARY\"\r\n\r\n"
            ));
            for (ct, extra, body) in parts {
                m.push_str(&format!(
                    "--BOUNDARY\r\nContent-Type: {ct}\r\n{extra}\r\n{body}\r\n"
                ));
            }
            m.push_str("--BOUNDARY--\r\n");
        }
    }
    m.into_bytes()
}

pub(super) fn convert_with(eml: Vec<u8>, backend: &EmailBackend) -> DoclingDocument {
    let src = SourceDocument::from_bytes("t.eml", InputFormat::Email, eml);
    backend.convert(&src).unwrap()
}

pub(super) fn convert(eml: Vec<u8>) -> DoclingDocument {
    convert_with(eml, &EmailBackend::default())
}

/// `(text, content_layer)` of every text item.
pub(super) fn texts(doc: &DoclingDocument) -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["text"].as_str().unwrap().to_string(),
                t["content_layer"].as_str().unwrap_or("body").to_string(),
            )
        })
        .collect()
}

pub(super) fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

/// `export_to_markdown(included_content_layers={BODY, FURNITURE})`.
pub(super) fn md_all(doc: &DoclingDocument) -> String {
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
    doc.export_to_markdown_with_options(&options)
        .0
        .trim_end()
        .to_string()
}

pub(super) fn md(doc: &DoclingDocument) -> String {
    doc.export_to_markdown().trim_end().to_string()
}

const HTML: &str = "<html><body><p>Body: 1 &lt; 2 &amp; 3 &gt; 2, a_b.</p>\
                    <footer>Footer: terms &amp; conditions, c_d.</footer></body></html>";

/// An HTML-only message, or a blank `text/plain` beside the HTML.
fn html_message(subject: &str, html: &str, blank_plain: bool) -> Vec<u8> {
    let headers = format!("Subject: {subject}\r\n");
    let html_part = ("text/html; charset=\"utf-8\"", "", html);
    if blank_plain {
        message(
            &headers,
            Some("alternative"),
            &[("text/plain; charset=\"utf-8\"", "", " \r\n"), html_part],
        )
    } else {
        message(&headers, None, &[html_part])
    }
}

/// docling#5228 (`test_email_html_body_keeps_furniture_outside_the_body`,
/// html-only and blank-plain): the HTML body goes through the HTML backend
/// and its `<footer>` stays furniture, escaped once in Markdown.
#[test]
fn html_body_keeps_furniture_outside_the_body() {
    for blank_plain in [false, true] {
        let doc = convert(html_message("HTML furniture", HTML, blank_plain));
        let footer = "Footer: terms & conditions, c_d.";
        assert_eq!(
            texts(&doc),
            pairs(&[
                ("HTML furniture", "body"),
                ("Body: 1 < 2 & 3 > 2, a_b.", "body"),
                (footer, "furniture"),
            ]),
            "blank plain: {blank_plain}"
        );
        assert!(!md(&doc).contains("Footer:"));
        let all = md_all(&doc);
        assert_eq!(all.matches("Footer: terms &amp; conditions, c\\_d.").count(), 1, "{all}");
        assert!(all.contains("Body: 1 &lt; 2 &amp; 3 &gt; 2, a\\_b."), "{all}");
    }
}

/// docling#5228 (`test_email_with_only_html_furniture_does_not_fall_back_to_raw_html`).
#[test]
fn only_html_furniture_does_not_fall_back_to_raw_html() {
    for blank_plain in [false, true] {
        let doc = convert(html_message(
            "Footer only email",
            "<html><body><footer>Footer only.</footer></body></html>",
            blank_plain,
        ));
        assert_eq!(
            texts(&doc),
            pairs(&[("Footer only email", "body"), ("Footer only.", "furniture")])
        );
        assert_eq!(md(&doc), "# Footer only email");
        assert_eq!(md_all(&doc), "# Footer only email\n\nFooter only.");
    }
}

/// docling#5228 (`test_email_keeps_nonempty_plain_body_instead_of_html_furniture`).
#[test]
fn nonempty_plain_body_wins_over_html_furniture() {
    let doc = convert(message(
        "Subject: Plain preferred\r\n",
        Some("alternative"),
        &[
            ("text/plain; charset=\"utf-8\"", "", "Plain body.\r\n\r\nShared footer.\r\n"),
            (
                "text/html; charset=\"utf-8\"",
                "",
                "<html><body><p>HTML body.</p><footer>Shared footer.</footer></body></html>",
            ),
        ],
    ));
    assert_eq!(
        texts(&doc),
        pairs(&[
            ("Plain preferred", "body"),
            ("Plain body.", "body"),
            ("Shared footer.", "body"),
        ])
    );
    assert_eq!(md_all(&doc), "# Plain preferred\n\nPlain body.\n\nShared footer.");
}

/// docling#5228 (`test_email_keeps_furniture_from_each_html_part_once`).
#[test]
fn furniture_from_each_html_part_once() {
    let html = "text/html; charset=\"utf-8\"";
    let doc = convert(message(
        "Subject: Multipart HTML\r\n",
        Some("mixed"),
        &[
            (html, "", "<html><body> </body></html>"),
            (
                html,
                "",
                "<html><body><p>First body.</p><footer>First footer.</footer></body></html>",
            ),
            (
                html,
                "",
                "<html><body><p>Second body.</p><footer>Second footer.</footer></body></html>",
            ),
        ],
    ));
    assert_eq!(
        texts(&doc),
        pairs(&[
            ("Multipart HTML", "body"),
            ("First body.", "body"),
            ("First footer.", "furniture"),
            ("Second body.", "body"),
            ("Second footer.", "furniture"),
        ])
    );
    assert_eq!(md(&doc), "# Multipart HTML\n\nFirst body.\n\nSecond body.");
}

/// The HTML body is the HTML backend's Markdown — a heading, bold, a list
/// and a table, each block a paragraph whose line breaks print as hard
/// breaks (docling 2.151.1's output) — where mail-parser's html-to-text
/// flattened it to `HeadPara bold.  \nonetwoAB12`.
#[test]
fn rich_html_body_keeps_its_structure() {
    let doc = convert(message(
        "Subject: Rich\r\n",
        None,
        &[(
            "text/html; charset=\"utf-8\"",
            "",
            "<html><body><h2>Head</h2><p>Para <b>bold</b>.</p><ul><li>one</li><li>two</li></ul>\
             <table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table></body></html>",
        )],
    ));
    assert_eq!(
        md(&doc),
        "# Rich\n\n## Head\n\nPara **bold**.\n\n- one  \n- two\n\n\
         |   A |   B |  \n|-----|-----|  \n|   1 |   2 |"
    );
}
