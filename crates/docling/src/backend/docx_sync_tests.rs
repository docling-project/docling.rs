//! The DOCX cases of the 2026-11 upstream sync (#1208), ported from
//! docling's `tests/test_backend_msword*.py`: each test builds the package
//! upstream's builds and asserts what upstream asserts, on the Markdown and
//! on docling's item tree (the JSON).

use serde_json::Value;

use super::docx_chain_parity::package;
use super::DeclarativeBackend;
use crate::format::InputFormat;
use crate::source::SourceDocument;

pub(super) const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
pub(super) const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// A package around `body` (the `w:body` content) with `rels` extra
/// relationships (`<Relationship …/>` elements) and `parts` besides.
pub(super) fn docx(body: &str, rels: &str, parts: &[(&str, String)]) -> Vec<u8> {
    let document = format!(
        r#"<w:document xmlns:w="{W}" xmlns:r="{R}"><w:body>{body}<w:sectPr/></w:body></w:document>"#
    );
    let rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    );
    let mut all = vec![
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", rels),
    ];
    all.extend(parts.iter().map(|(n, c)| (*n, c.clone())));
    package(&all)
}

pub(super) fn convert(bytes: Vec<u8>) -> docling_core::DoclingDocument {
    let src = SourceDocument::from_bytes("t.docx", InputFormat::Docx, bytes);
    super::DocxBackend::default()
        .convert(&src)
        .expect("docx converts")
}

pub(super) fn json(doc: &docling_core::DoclingDocument) -> Value {
    serde_json::from_str(&doc.export_to_json()).unwrap()
}

pub(super) fn p(text: &str) -> String {
    format!(r#"<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#)
}

fn tc(text: &str) -> String {
    format!("<w:tc>{}</w:tc>", p(text))
}

/// A table cell as `(text, row, col, row_span, col_span)`.
type Cell = (String, u64, u64, u64, u64);

/// The JSON's first table as its size and cells.
fn table_cells(doc: &docling_core::DoclingDocument) -> (u64, u64, Vec<Cell>) {
    let v = json(doc);
    let data = &v["tables"][0]["data"];
    let mut cells: Vec<_> = data["table_cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let n = |k: &str| c[k].as_u64().unwrap();
            (
                c["text"].as_str().unwrap().to_string(),
                n("start_row_offset_idx"),
                n("start_col_offset_idx"),
                n("row_span"),
                n("col_span"),
            )
        })
        .collect();
    cells.sort_by_key(|c| (c.1, c.2));
    (
        data["num_rows"].as_u64().unwrap(),
        data["num_cols"].as_u64().unwrap(),
        cells,
    )
}

fn cell(t: &str, r: u64, c: u64, rs: u64, cs: u64) -> Cell {
    (t.to_string(), r, c, rs, cs)
}

/// docling#4393 (#1156, `test_table_rows_and_cells_wrapped_in_custom_xml_or_content_control`):
/// rows wrapped in `w:customXml` or a content control are rows, a cell
/// wrapped in `w:customXml` is a cell.
#[test]
fn table_rows_and_cells_wrapped_in_custom_xml_or_content_control() {
    let grid = "<w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>";
    let body = format!(
        "<w:tbl>{grid}<w:tr>{}{}</w:tr>\
         <w:customXml><w:tr>{}{}</w:tr></w:customXml>\
         <w:sdt><w:sdtContent><w:tr>{}{}</w:tr></w:sdtContent></w:sdt>\
         <w:tr>{}<w:customXml>{}</w:customXml></w:tr></w:tbl>",
        tc("A1"),
        tc("A2"),
        tc("B1"),
        tc("B2"),
        tc("C1"),
        tc("C2"),
        tc("D1"),
        tc("D2"),
    );
    let doc = convert(docx(&body, "", &[]));
    let (rows, cols, cells) = table_cells(&doc);
    assert_eq!((rows, cols), (4, 2));
    let texts: Vec<&str> = cells.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(texts, ["A1", "A2", "B1", "B2", "C1", "C2", "D1", "D2"]);
    let md = doc.export_to_markdown();
    assert!(md.contains("| D1 ") && md.contains("| D2 "), "{md}");
}

/// docling#4393 (`test_vertical_merge_continues_into_a_wrapped_row`): a
/// `vMerge` continuing in a wrapped row extends the merge.
#[test]
fn vertical_merge_continues_into_a_wrapped_row() {
    let grid = "<w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>";
    let merged = format!(
        r#"<w:tc><w:tcPr><w:vMerge w:val="restart"/></w:tcPr>{}</w:tc>"#,
        p("merged")
    );
    let cont = format!(r#"<w:tc><w:tcPr><w:vMerge/></w:tcPr>{}</w:tc>"#, p(""));
    let body = format!(
        "<w:tbl>{grid}<w:tr>{merged}{}</w:tr>\
         <w:customXml><w:tr>{cont}{}</w:tr></w:customXml>\
         <w:tr>{}{}</w:tr></w:tbl>",
        tc("A2"),
        tc("B2"),
        tc("C1"),
        tc("C2"),
    );
    let (rows, _, cells) = table_cells(&convert(docx(&body, "", &[])));
    assert_eq!(rows, 3);
    let m = cells.iter().find(|c| c.0 == "merged").expect("merged cell");
    assert_eq!(m.3, 2, "{cells:?}");
}

/// docling#4393 (`test_wrapped_row_wider_than_the_declared_grid_widens_the_table`):
/// a wrapped row wider than a one-column grid widens the table (the 1×1
/// unwrap no longer fires on the first row alone).
#[test]
fn wrapped_row_wider_than_the_declared_grid_widens_the_table() {
    let body = format!(
        "<w:tbl><w:tblGrid><w:gridCol/></w:tblGrid><w:tr>{}</w:tr>\
         <w:customXml><w:tr>{}{}</w:tr></w:customXml></w:tbl>",
        tc("A1"),
        tc("B1"),
        tc("B2"),
    );
    let (rows, cols, cells) = table_cells(&convert(docx(&body, "", &[])));
    assert_eq!((rows, cols), (2, 2));
    for want in [
        cell("A1", 0, 0, 1, 1),
        cell("B1", 1, 0, 1, 1),
        cell("B2", 1, 1, 1, 1),
    ] {
        assert!(
            cells
                .iter()
                .any(|c| (c.0.as_str(), c.1, c.2) == (want.0.as_str(), want.1, want.2)),
            "{cells:?}"
        );
    }
}

/// docling#5249 (#1159, `test_hyperlink_preserves_external_fragment`): an
/// external hyperlink keeps its `w:anchor` fragment.
#[test]
fn hyperlink_preserves_external_fragment() {
    for (target, anchor, want) in [
        (
            "https://example.org/guide",
            "installation",
            "https://example.org/guide#installation",
        ),
        ("manual.docx", "Installation", "manual.docx#Installation"),
        (
            "https://example.org/guide#installation",
            "",
            "https://example.org/guide#installation",
        ),
    ] {
        let anchor_attr = if anchor.is_empty() {
            String::new()
        } else {
            format!(r#" w:anchor="{anchor}""#)
        };
        let body = format!(
            r#"<w:p><w:hyperlink r:id="rId5"{anchor_attr}><w:r><w:t>Install</w:t></w:r></w:hyperlink></w:p>"#
        );
        let rel = format!(
            r#"<Relationship Id="rId5" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{target}" TargetMode="External"/>"#
        );
        let doc = convert(docx(&body, &rel, &[]));
        assert_eq!(
            doc.export_to_markdown().trim(),
            format!("[Install]({want})")
        );
        let v = json(&doc);
        let link = v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|t| t["hyperlink"].as_str())
            .expect("a linked text");
        assert_eq!(link, want);
    }
}

/// A paragraph with an optional style, numbering and direct outline level.
fn outlined(text: &str, style: Option<&str>, num: Option<u32>, outline: Option<u8>) -> String {
    let style = style.map_or(String::new(), |s| format!(r#"<w:pStyle w:val="{s}"/>"#));
    let num = num.map_or(String::new(), |n| {
        format!(r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="{n}"/></w:numPr>"#)
    });
    let outline = outline.map_or(String::new(), |l| format!(r#"<w:outlineLvl w:val="{l}"/>"#));
    format!(
        r#"<w:p><w:pPr>{style}{num}{outline}</w:pPr><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#
    )
}

const STYLES_REL: &str = r#"<Relationship Id="rIdS" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#;
const NUMBERING_REL: &str = r#"<Relationship Id="rIdN" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>"#;

fn styles() -> (&'static str, String) {
    (
        "word/styles.xml",
        format!(
            r#"<w:styles xmlns:w="{W}">
<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>
<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="BodyText"><w:name w:val="Body Text"/><w:basedOn w:val="Normal"/></w:style>
</w:styles>"#
        ),
    )
}

fn markdown_blocks(doc: &docling_core::DoclingDocument) -> Vec<String> {
    doc.export_to_markdown()
        .split("\n\n")
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect()
}

/// docling#4129 (#1158, `test_outline_level_on_the_paragraph_is_detected_without_a_heading_style`).
#[test]
fn outline_level_on_the_paragraph_is_detected_without_a_heading_style() {
    let body = outlined("Larger channel bandwidth", None, None, Some(1)) + &p("Body text.");
    let doc = convert(docx(&body, STYLES_REL, &[styles()]));
    assert_eq!(
        markdown_blocks(&doc),
        ["### Larger channel bandwidth", "Body text."]
    );
    let v = json(&doc);
    let head = &v["texts"][0];
    assert_eq!(head["label"], "section_header", "{head}");
    assert_eq!(head["level"], 2, "{head}");
    // Through a style with no outline level of its own, too.
    let body =
        outlined("Larger channel bandwidth", Some("BodyText"), None, Some(1)) + &p("Body text.");
    let doc = convert(docx(&body, STYLES_REL, &[styles()]));
    assert_eq!(
        markdown_blocks(&doc),
        ["### Larger channel bandwidth", "Body text."]
    );
}

/// docling#4129 (`test_numbered_paragraph_with_outline_level_is_a_heading_not_a_list_item`).
#[test]
fn numbered_paragraph_with_outline_level_is_a_heading_not_a_list_item() {
    let numbering = (
        "word/numbering.xml",
        format!(
            r#"<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
        ),
    );
    let body = [
        outlined("Introduction", None, Some(1), Some(0)),
        p("Body text."),
        outlined("Discussion", None, Some(1), Some(0)),
        p("Body text."),
    ]
    .concat();
    let rels = format!("{STYLES_REL}{NUMBERING_REL}");
    let doc = convert(docx(&body, &rels, &[styles(), numbering]));
    assert_eq!(
        markdown_blocks(&doc),
        [
            "## 1 Introduction",
            "Body text.",
            "## 2 Discussion",
            "Body text."
        ]
    );
    let v = json(&doc);
    let headers: Vec<&str> = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["label"] == "section_header")
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert_eq!(headers, ["1 Introduction", "2 Discussion"]);
    assert!(v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .all(|g| g["label"] != "list"));
}

/// A paragraph-level 9 (body text) masks the style's outline level: a
/// heading-named style keeps the level in its name, any other is text; and
/// `use_outline_level_for_headings = false` keeps a paragraph's level from
/// promoting a style no name marks a heading.
#[test]
fn paragraph_outline_body_text_and_the_option() {
    let body = outlined("Named", Some("Heading2"), None, Some(9))
        + &outlined("Plain", Some("BodyText"), None, Some(9));
    let doc = convert(docx(&body, STYLES_REL, &[styles()]));
    assert_eq!(markdown_blocks(&doc), ["### Named", "Plain"]);

    let body = outlined("Outlined", Some("BodyText"), None, Some(1));
    let src = SourceDocument::from_bytes(
        "t.docx",
        InputFormat::Docx,
        docx(&body, STYLES_REL, &[styles()]),
    );
    let backend = super::DocxBackend {
        use_outline_level_for_headings: false,
        ..Default::default()
    };
    let doc = backend.convert(&src).expect("docx converts");
    assert_eq!(markdown_blocks(&doc), ["Outlined"]);
    assert_eq!(json(&doc)["texts"][0]["label"], "text");
}
