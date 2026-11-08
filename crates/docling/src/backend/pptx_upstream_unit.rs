//! docling's PPTX fixes that upstream pins with unit tests only (no
//! groundtruth file to mirror), ported here on decks built in memory the way
//! python-pptx's default template writes them — cut to the parts the backend
//! reads.

use super::DeclarativeBackend;
use crate::{InputFormat, SourceDocument};
use docling_core::DoclingDocument;
use serde_json::Value;
use std::io::Write;

const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const REL_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
const REL_LAYOUT: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
const REL_MASTER: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster";
const REL_IMAGE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";

fn zip(parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(content).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

/// A one-slide deck: `shapes` is the slide's `p:spTree` content, `layout` the
/// layout's (`None` = no layout part), `media` = `(rId, part name, bytes)`
/// images the slide relates to.
fn deck(shapes: &str, layout: Option<&str>, media: &[(&str, &str, Vec<u8>)]) -> DoclingDocument {
    let ct = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="png" ContentType="image/png"/></Types>"#.to_string();
    let presentation = format!(
        r#"<p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#
    );
    let pres_rels = format!(
        r#"<Relationships xmlns="{RELS}"><Relationship Id="rId2" Type="{REL_SLIDE}" Target="slides/slide1.xml"/></Relationships>"#
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{shapes}</p:spTree></p:cSld></p:sld>"#
    );
    let mut slide_rels = format!(r#"<Relationships xmlns="{RELS}">"#);
    if layout.is_some() {
        slide_rels.push_str(&format!(
            r#"<Relationship Id="rId1" Type="{REL_LAYOUT}" Target="../slideLayouts/slideLayout1.xml"/>"#
        ));
    }
    for (rid, name, _) in media {
        slide_rels.push_str(&format!(
            r#"<Relationship Id="{rid}" Type="{REL_IMAGE}" Target="../media/{name}"/>"#
        ));
    }
    slide_rels.push_str("</Relationships>");
    let mut parts: Vec<(String, Vec<u8>)> = vec![
        ("[Content_Types].xml".into(), ct.into_bytes()),
        ("ppt/presentation.xml".into(), presentation.into_bytes()),
        (
            "ppt/_rels/presentation.xml.rels".into(),
            pres_rels.into_bytes(),
        ),
        ("ppt/slides/slide1.xml".into(), slide.into_bytes()),
        (
            "ppt/slides/_rels/slide1.xml.rels".into(),
            slide_rels.into_bytes(),
        ),
    ];
    if let Some(layout) = layout {
        parts.push((
            "ppt/slideLayouts/slideLayout1.xml".into(),
            format!(
                r#"<p:sldLayout xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree>{layout}</p:spTree></p:cSld></p:sldLayout>"#
            )
            .into_bytes(),
        ));
        parts.push((
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels".into(),
            format!(
                r#"<Relationships xmlns="{RELS}"><Relationship Id="rId1" Type="{REL_MASTER}" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#
            )
            .into_bytes(),
        ));
        // The default template's master: bullets at every body level, none
        // in the title.
        parts.push((
            "ppt/slideMasters/slideMaster1.xml".into(),
            format!(
                r#"<p:sldMaster xmlns:p="{P}" xmlns:a="{A}"><p:cSld><p:spTree/></p:cSld><p:txStyles><p:titleStyle><a:lvl1pPr/></p:titleStyle><p:bodyStyle><a:lvl1pPr><a:buChar char="•"/></a:lvl1pPr><a:lvl2pPr><a:buChar char="–"/></a:lvl2pPr></p:bodyStyle><p:otherStyle><a:lvl1pPr/></p:otherStyle></p:txStyles></p:sldMaster>"#
            )
            .into_bytes(),
        ));
    }
    for (_, name, bytes) in media {
        parts.push((format!("ppt/media/{name}"), bytes.clone()));
    }
    let parts: Vec<(&str, Vec<u8>)> = parts.iter().map(|(n, b)| (n.as_str(), b.clone())).collect();
    let src = SourceDocument::from_bytes("t.pptx", InputFormat::Pptx, zip(&parts));
    super::PptxBackend.convert(&src).expect("converts")
}

fn text_box(id: u32, top_in: i64, paragraphs: &str) -> String {
    let top = top_in * 914400;
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="TextBox {id}"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="914400" y="{top}"/><a:ext cx="3657600" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/>{paragraphs}</p:txBody></p:sp>"#
    )
}

fn para(p_pr: &str, text: &str) -> String {
    format!("<a:p>{p_pr}<a:r><a:t>{text}</a:t></a:r></a:p>")
}

fn json(doc: &DoclingDocument) -> Value {
    doc.export_to_json_value()
}

/// `(marker, text)` of every list item, in document order.
fn list_markers(doc: &DoclingDocument) -> Vec<(String, String)> {
    json(doc)["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["label"] == "list_item")
        .map(|t| {
            (
                t["marker"].as_str().unwrap_or_default().to_string(),
                t["text"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn md(doc: &DoclingDocument) -> String {
    doc.export_to_markdown().trim_end().to_string()
}

// ---------------------------------------------------------------------------
// docling#5122 (#1018): `tests/test_backend_pptx_list_markers.py`.

/// `_save_numbered_deck`: one text box per list, every paragraph numbered
/// with an `a:buAutoNum` carrying `attrs`.
fn numbered_deck(lists: &[(&str, &[&str])]) -> DoclingDocument {
    let shapes: String = lists
        .iter()
        .enumerate()
        .map(|(i, (attrs, texts))| {
            let p_pr = format!("<a:pPr><a:buAutoNum {attrs}/></a:pPr>");
            let paras: String = texts.iter().map(|t| para(&p_pr, t)).collect();
            text_box(i as u32 + 2, i as i64 + 1, &paras)
        })
        .collect();
    deck(&shapes, None, &[])
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(m, t)| (m.to_string(), t.to_string()))
        .collect()
}

/// `test_pptx_numbered_list_marker_follows_auto_number_type`.
#[test]
fn numbered_list_marker_follows_auto_number_type() {
    let doc = numbered_deck(&[
        (r#"type="alphaLcPeriod""#, &["Plan", "Build", "Test"]),
        (
            r#"type="romanUcPeriod""#,
            &["Scope", "Risks", "Costs", "Staff"],
        ),
        (r#"type="arabicParenR""#, &["Draft", "Review"]),
        (
            r#"type="alphaUcParenBoth" startAt="3""#,
            &["Annex", "Index"],
        ),
    ]);
    assert_eq!(
        list_markers(&doc),
        pairs(&[
            ("a.", "Plan"),
            ("b.", "Build"),
            ("c.", "Test"),
            ("I.", "Scope"),
            ("II.", "Risks"),
            ("III.", "Costs"),
            ("IV.", "Staff"),
            ("1.", "Draft"),
            ("2.", "Review"),
            ("(C)", "Annex"),
            ("(D)", "Index"),
        ])
    );
    assert_eq!(
        md(&doc),
        "- a. Plan\n- b. Build\n- c. Test\n\n\
         - I. Scope\n- II. Risks\n- III. Costs\n- IV. Staff\n\n\
         1. Draft\n2. Review\n\n\
         - (C) Annex\n- (D) Index"
    );
}

/// `test_pptx_numbered_list_ignores_out_of_range_start_at`.
#[test]
fn numbered_list_ignores_out_of_range_start_at() {
    for start_at in ["0", "32768", "2000000000"] {
        let attrs = format!(r#"type="alphaLcPeriod" startAt="{start_at}""#);
        let doc = numbered_deck(&[(&attrs, &["Plan", "Build"])]);
        assert_eq!(
            list_markers(&doc),
            pairs(&[("a.", "Plan"), ("b.", "Build")]),
            "startAt={start_at}"
        );
    }
}

/// `test_pptx_numbered_list_marker_follows_layout_auto_number_type`: the
/// default template's "Title and Content" layout, its body placeholder's
/// list style setting `romanLcParenR` at level 1.
#[test]
fn numbered_list_marker_follows_layout_auto_number_type() {
    let layout = r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p/></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Content Placeholder 2"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle><a:lvl1pPr><a:buAutoNum type="romanLcParenR"/></a:lvl1pPr></a:lstStyle><a:p/></p:txBody></p:sp>"#;
    let shapes = format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{}</p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Content Placeholder 2"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{}{}</p:txBody></p:sp>"#,
        para("", "Steps"),
        para("", "Prepare"),
        para("", "Ship"),
    );
    let doc = deck(&shapes, Some(layout), &[]);
    assert_eq!(md(&doc), "# Steps\n\n- i) Prepare\n- ii) Ship");
}

// ---------------------------------------------------------------------------
// docling#4989 (#1018): `tests/test_backend_pptx_picture_fill.py`.

/// A 64×32 red PNG.
fn png() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(64, 32, image::Rgb([255, 0, 0]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// `_picture_filled_shape_pptx`: a rectangle filled with the PNG through
/// `a:blipFill/a:blip` — `blip_attr` is its `r:embed` / `r:link`.
fn picture_filled_deck(text: &str, blip_attr: &str) -> DoclingDocument {
    let body = if text.is_empty() {
        "<a:p/>".to_string()
    } else {
        para("", text)
    };
    let shape = format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Rectangle 1"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="914400" y="914400"/><a:ext cx="1828800" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:blipFill><a:blip {blip_attr}/><a:stretch><a:fillRect/></a:stretch></a:blipFill></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/>{body}</p:txBody></p:sp>"#
    );
    deck(&shape, None, &[("rId2", "image1.png", png())])
}

fn texts(doc: &DoclingDocument) -> Vec<String> {
    json(doc)["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn pictures(doc: &DoclingDocument) -> Vec<Value> {
    json(doc)["pictures"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// `test_pptx_picture_filled_shape_is_extracted`.
#[test]
fn picture_filled_shape_is_extracted() {
    let doc = picture_filled_deck("", r#"r:embed="rId2""#);
    let pics = pictures(&doc);
    assert_eq!(pics.len(), 1);
    let image = &pics[0]["image"];
    assert_eq!(image["mimetype"], "image/png");
    assert_eq!(image["size"]["width"], 64.0);
    assert_eq!(image["size"]["height"], 32.0);
    let prov = &pics[0]["prov"][0];
    assert_eq!(prov["page_no"], 1);
    let bbox = &prov["bbox"];
    assert!(bbox["r"].as_f64().unwrap() > bbox["l"].as_f64().unwrap());
}

/// `test_pptx_picture_filled_shape_keeps_its_text`.
#[test]
fn picture_filled_shape_keeps_its_text() {
    let doc = picture_filled_deck("Caption on the fill", r#"r:embed="rId2""#);
    assert_eq!(pictures(&doc).len(), 1);
    assert_eq!(texts(&doc), ["Caption on the fill"]);
    // The picture comes first, then the shape's text.
    assert!(md(&doc).ends_with("Caption on the fill"), "{}", md(&doc));
    assert!(md(&doc).starts_with("<!-- image -->"), "{}", md(&doc));
}

/// `test_pptx_linked_picture_fill_is_ignored`.
#[test]
fn linked_picture_fill_is_ignored() {
    let doc = picture_filled_deck("Linked fill", r#"r:link="rId2""#);
    assert!(pictures(&doc).is_empty());
    assert_eq!(texts(&doc), ["Linked fill"]);
    assert_eq!(md(&doc), "Linked fill");
}

/// `test_pptx_picture_fill_with_missing_part_is_skipped`.
#[test]
fn picture_fill_with_missing_part_is_skipped() {
    let doc = picture_filled_deck("Dangling fill", r#"r:embed="rId999""#);
    assert!(pictures(&doc).is_empty());
    assert_eq!(texts(&doc), ["Dangling fill"]);
    assert_eq!(md(&doc), "Dangling fill");
}

/// `test_picture_fill_rid_with_empty_embed_warns` /
/// `test_picture_fill_rid_without_picture_fill`: an empty `r:embed` and a
/// non-picture fill add no picture.
#[test]
fn empty_embed_and_solid_fill_add_no_picture() {
    let doc = picture_filled_deck("Empty embed", r#"r:embed="""#);
    assert!(pictures(&doc).is_empty());
    assert_eq!(texts(&doc), ["Empty embed"]);
    let solid = text_box(2, 1, &para("", "Solid fill")).replace(
        "<a:avLst/></a:prstGeom>",
        "<a:avLst/></a:prstGeom><a:solidFill/>",
    );
    let doc = deck(&solid, None, &[]);
    assert!(pictures(&doc).is_empty());
    assert_eq!(texts(&doc), ["Solid fill"]);
}
