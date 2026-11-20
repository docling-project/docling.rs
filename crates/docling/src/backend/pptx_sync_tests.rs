//! The PPTX cases of the 2026-11 upstream sync (#1208), ported from
//! docling's `tests/test_backend_pptx*.py`: each test builds the deck
//! upstream's builds and asserts what upstream asserts, on the Markdown and
//! on docling's item tree (the JSON).

use serde_json::Value;

use std::io::Write;

use super::DeclarativeBackend;
use crate::format::InputFormat;
use crate::source::SourceDocument;

pub(super) const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
pub(super) const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
pub(super) const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const REL_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";

/// A ZIP of `parts`, stored.
pub(super) fn package(parts: &[(&str, String)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

/// A one-slide deck: `shapes` is the slide's `p:spTree` content, `rels`
/// extra slide relationships, `parts` extra package parts.
pub(super) fn deck(shapes: &str, rels: &str, parts: &[(&str, String)]) -> Vec<u8> {
    let presentation = format!(
        r#"<p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#
    );
    let pres_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL_SLIDE}" Target="slides/slide1.xml"/></Relationships>"#
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{shapes}</p:spTree></p:cSld></p:sld>"#
    );
    let slide_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    );
    let mut all = vec![
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", pres_rels),
        ("ppt/slides/slide1.xml", slide),
        ("ppt/slides/_rels/slide1.xml.rels", slide_rels),
    ];
    all.extend(parts.iter().map(|(n, c)| (*n, c.clone())));
    package(&all)
}

/// A text box at `(x, y)` EMU holding one paragraph per entry.
pub(super) fn text_box(id: u32, x: i64, y: i64, paras: &[&str], extra_nv: &str) -> String {
    let paras: String = paras
        .iter()
        .map(|t| format!("<a:p><a:r><a:t>{t}</a:t></a:r></a:p>"))
        .collect();
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="TextBox {id}">{extra_nv}</p:cNvPr><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="2000000" cy="400000"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/>{paras}</p:txBody></p:sp>"#
    )
}

pub(super) fn convert(bytes: Vec<u8>) -> docling_core::DoclingDocument {
    let src = SourceDocument::from_bytes("t.pptx", InputFormat::Pptx, bytes);
    super::PptxBackend.convert(&src).expect("pptx converts")
}

pub(super) fn json(doc: &docling_core::DoclingDocument) -> Value {
    serde_json::from_str(&doc.export_to_json()).unwrap()
}

/// docling#5384 (#1164, `test_malformed_shape_hyperlink_keeps_the_slide_text`
/// and `…_does_not_stop_the_next_file`): a shape link `urlsplit` / `AnyUrl`
/// reject is dropped and the shape's text kept; a valid one stays.
#[test]
fn malformed_shape_hyperlink_keeps_the_slide_text() {
    let link = r#"<a:hlinkClick r:id="rIdL"/>"#;
    let rel = |target: &str| {
        format!(
            r#"<Relationship Id="rIdL" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{target}" TargetMode="External"/>"#
        )
    };
    for (target, want) in [
        ("https://[broken", "Link text"),
        ("https://broken]", "Link text"),
        ("https://[not-ipv6]/", "Link text"),
        (
            "https://example.org/ok",
            "[Link text](https://example.org/ok)",
        ),
    ] {
        let shapes = text_box(2, 0, 0, &["Link text"], link);
        let doc = convert(deck(&shapes, &rel(target), &[]));
        let md = doc.export_to_markdown();
        assert!(md.contains(want), "{target}: {md}");
        if want == "Link text" {
            assert!(!md.contains("]("), "{target}: {md}");
            let v = json(&doc);
            assert!(
                v["texts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|t| t["hyperlink"].is_null()),
                "{target}"
            );
        }
    }
}
