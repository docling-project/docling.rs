//! docling-core's Markdown formatting fallback (docling-core#661, #1205):
//! `formatting_fallback_mode` `none` drops underline and sub/superscript,
//! `html` writes `<u>`, `<sub>`, `<sup>`; the plain text drops them in both
//! modes, and every other export is the same either way. Ported from
//! docling-core's `test_serialization.py::test_md_formatting_fallback_mode`
//! — its document as docling JSON — and on the HTML page that writes it.

use docling::{DocumentConverter, InputFormat, SourceDocument};
use docling_core::{DoclingDocument, FormattingFallback};

/// `test_md_formatting_fallback_mode`'s document: an inline group of a
/// subscript, a superscript and an underlined run with markup in it.
const DOC_JSON: &str = r##"{"schema_name":"DoclingDocument","version":"1.11.0","name":"test","body":{"self_ref":"#/body","children":[{"$ref":"#/groups/0"}],"content_layer":"body","name":"_root_","label":"unspecified"},"groups":[{"self_ref":"#/groups/0","parent":{"$ref":"#/body"},"children":[{"$ref":"#/texts/0"},{"$ref":"#/texts/1"},{"$ref":"#/texts/2"},{"$ref":"#/texts/3"},{"$ref":"#/texts/4"},{"$ref":"#/texts/5"}],"content_layer":"body","name":"group","label":"inline"}],"texts":[{"self_ref":"#/texts/0","parent":{"$ref":"#/groups/0"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":"Water is H","text":"Water is H"},{"self_ref":"#/texts/1","parent":{"$ref":"#/groups/0"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":"2","text":"2","formatting":{"bold":false,"italic":false,"underline":false,"strikethrough":false,"script":"sub"}},{"self_ref":"#/texts/2","parent":{"$ref":"#/groups/0"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":"O, energy is E=mc","text":"O, energy is E=mc"},{"self_ref":"#/texts/3","parent":{"$ref":"#/groups/0"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":"2","text":"2","formatting":{"bold":false,"italic":false,"underline":false,"strikethrough":false,"script":"super"}},{"self_ref":"#/texts/4","parent":{"$ref":"#/groups/0"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":", ","text":", "},{"self_ref":"#/texts/5","parent":{"$ref":"#/groups/0"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":"<tag>","text":"<tag>","formatting":{"bold":false,"italic":false,"underline":true,"strikethrough":false,"script":"baseline"}}],"pictures":[],"tables":[],"key_value_items":[],"form_items":[],"pages":{}}"##;

const HTML: &str = "<html><body><p>Water is H<sub>2</sub>O, energy is E=mc<sup>2</sup>, \
                    <u>&lt;tag&gt;</u></p></body></html>";

const PLAIN: &str = "Water is H2O, energy is E=mc2, <tag>";

fn convert(
    name: &str,
    format: InputFormat,
    bytes: &[u8],
    mode: FormattingFallback,
) -> DoclingDocument {
    DocumentConverter::new()
        .markdown_formatting_fallback(mode)
        .convert(SourceDocument::from_bytes(name, format, bytes.to_vec()))
        .expect("convert")
        .document
}

/// `(mode, expected)` exactly as upstream parametrizes the test.
fn cases() -> [(FormattingFallback, &'static str); 2] {
    [
        (
            FormattingFallback::None,
            "Water is H2O, energy is E=mc2, &lt;tag&gt;",
        ),
        (
            FormattingFallback::Html,
            "Water is H<sub>2</sub>O, energy is E=mc<sup>2</sup>, <u>&lt;tag&gt;</u>",
        ),
    ]
}

#[test]
fn markdown_formatting_fallback_mode() {
    for (name, format, bytes) in [
        ("doc.json", InputFormat::JsonDocling, DOC_JSON.as_bytes()),
        ("page.html", InputFormat::Html, HTML.as_bytes()),
    ] {
        for (mode, want) in cases() {
            let doc = convert(name, format, bytes, mode);
            assert_eq!(doc.export_to_markdown().trim_end(), want, "{name} {mode}");
            assert_eq!(doc.export_to_text(), PLAIN, "{name} {mode}");
        }
    }
}

/// The streamed Markdown agrees with the buffered one.
#[test]
fn streamed_markdown_has_the_tags() {
    let doc = convert(
        "page.html",
        InputFormat::Html,
        HTML.as_bytes(),
        FormattingFallback::Html,
    );
    let mut streamer =
        docling_core::MarkdownStreamer::new(false, docling_core::ImageMode::Placeholder, false);
    let mut md = streamer.push(&doc.nodes, &doc.links);
    md.push_str(&streamer.finish());
    assert_eq!(md, doc.export_to_markdown());
}

/// Markdown only: every other export reads the same under either mode, and
/// none carries the conversion's marks.
#[test]
fn other_exports_are_unchanged() {
    for (name, format, bytes) in [
        ("doc.json", InputFormat::JsonDocling, DOC_JSON.as_bytes()),
        ("page.html", InputFormat::Html, HTML.as_bytes()),
    ] {
        let none = convert(name, format, bytes, FormattingFallback::None);
        let html = convert(name, format, bytes, FormattingFallback::Html);
        let exports = |d: &DoclingDocument| {
            vec![
                d.export_to_json(),
                d.export_to_json_value().to_string(),
                d.export_to_html(),
                d.export_to_doclang(),
                d.export_to_latex(),
                d.export_to_pandoc_json(),
                d.export_to_text(),
                docling_core::chunker::HierarchicalChunker
                    .chunk(d)
                    .into_iter()
                    .map(|c| c.text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ]
        };
        let (a, b) = (exports(&none), exports(&html));
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x, y, "{name}: export {i}");
            assert!(
                !y.contains(|c| ('\u{FDD0}'..='\u{FDD5}').contains(&c)),
                "{name}: export {i} kept a mark"
            );
        }
        let mut compact = Vec::new();
        html.write_json(&mut compact).unwrap();
        let mut want = Vec::new();
        none.write_json(&mut want).unwrap();
        assert_eq!(compact, want, "{name}: write_json");
    }
}
