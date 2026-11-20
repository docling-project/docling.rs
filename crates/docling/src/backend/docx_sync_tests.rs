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

/// The body docling's `_docx_with_xml_node` builds: a heading, a two-run
/// paragraph and a 1×2 table.
fn xml_node_body() -> String {
    format!(
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">first run </w:t></w:r><w:r><w:t>second run</w:t></w:r></w:p><w:tbl><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr>{}{}</w:tr></w:tbl>"#,
        tc("cell A"),
        tc("cell B"),
    )
}

fn heading_styles() -> (&'static str, String) {
    (
        "word/styles.xml",
        format!(
            r#"<w:styles xmlns:w="{W}"><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:pPr><w:outlineLvl w:val="0"/></w:pPr></w:style></w:styles>"#
        ),
    )
}

const XML_COMMENT: &str = "<!-- Created by docx4j 8.2.8 (Apache licensed) -->";

fn notes_texts(doc: &docling_core::DoclingDocument) -> Vec<String> {
    json(doc)["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["content_layer"] == "notes")
        .map(|t| t["text"].as_str().unwrap().to_string())
        .collect()
}

/// docling#4584 (#1157, `test_xml_comments_and_processing_instructions_keep_body_text`):
/// a comment or instruction anywhere in the document part keeps the body.
#[test]
fn xml_comments_and_processing_instructions_keep_body_text() {
    let body = xml_node_body();
    let cases: [(&str, String); 6] = [
        ("body", format!("{XML_COMMENT}{body}")),
        (
            "paragraph",
            body.replacen("<w:p>", &format!("<w:p>{XML_COMMENT}"), 1),
        ),
        (
            "run",
            body.replacen("<w:r>", &format!("<w:r>{XML_COMMENT}"), 1),
        ),
        (
            "table-cell",
            body.replacen("<w:tc>", &format!("<w:tc>{XML_COMMENT}"), 1),
        ),
        (
            "table-row",
            body.replacen("<w:tr>", &format!("<w:tr>{XML_COMMENT}"), 1),
        ),
        ("processing-instruction", format!("<?producer note?>{body}")),
    ];
    for (name, body) in cases {
        let doc = convert(docx(&body, STYLES_REL, &[heading_styles()]));
        let md = doc.export_to_markdown();
        for text in ["Title", "first run", "second run", "cell A", "cell B"] {
            assert!(md.contains(text), "{name}: {text} missing from {md}");
        }
        let v = json(&doc);
        let cells: Vec<&str> = v["tables"][0]["data"]["table_cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["text"].as_str().unwrap())
            .collect();
        assert_eq!(cells, ["cell A", "cell B"], "{name}");
    }
}

/// docling#4584 (`test_xml_comment_text_goes_to_notes`): the document
/// part's comments are notes-layer text, a blank one dropped; the text
/// around an inline comment joins; instructions leave nothing.
#[test]
fn xml_comment_text_goes_to_notes() {
    let body = format!("{XML_COMMENT}<!--   -->{}", xml_node_body())
        .replacen(
            "<w:p>",
            "<w:p xml:space=\"preserve\"><!-- indented -->\n  ",
            1,
        )
        .replacen(">first run <", ">first <!-- inline -->run <", 1)
        .replacen("<w:tc>", "<w:tc><?producer note?>", 1);
    let doc = convert(docx(&body, STYLES_REL, &[heading_styles()]));
    assert_eq!(
        notes_texts(&doc),
        [
            "Created by docx4j 8.2.8 (Apache licensed)",
            "indented",
            "inline"
        ]
    );
    let md = doc.export_to_markdown();
    assert!(md.contains("# Title"), "{md}");
    assert!(md.contains("first run second run"), "{md}");
    for gone in ["docx4j", "indented", "inline", "producer"] {
        assert!(!md.contains(gone), "{gone} in {md}");
    }
}

/// docling#4584 (`test_xml_comments_in_footnotes_are_removed`): comments
/// and instructions in the notes parts are removed, never notes text.
#[test]
fn xml_comments_in_footnotes_are_removed() {
    let separators = r#"<w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:type="continuationSeparator" w:id="0"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:footnote>"#;
    let footnotes = format!(
        r#"<w:footnotes xmlns:w="{W}">{separators}<w:footnote w:id="2"><w:p><!-- note --><w:r><w:t xml:space="preserve">Smith, J. <!-- inline -->(2020).</w:t></w:r><?producer note?><w:r><w:t xml:space="preserve"> Example Study.</w:t></w:r></w:p></w:footnote></w:footnotes>"#
    );
    let endnotes = format!(
        r#"<w:endnotes xmlns:w="{W}"><w:endnote w:id="2"><w:p><w:r><w:t>An endnote body.</w:t></w:r></w:p></w:endnote></w:endnotes>"#
    );
    let rels = r#"<Relationship Id="rIdF" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/><Relationship Id="rIdE" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/endnotes" Target="endnotes.xml"/>"#;
    let body = r#"<w:p><w:r><w:t>This is a claim that needs support</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r><w:r><w:endnoteReference w:id="2"/></w:r></w:p>"#;
    let doc = convert(docx(
        body,
        rels,
        &[
            ("word/footnotes.xml", footnotes),
            ("word/endnotes.xml", endnotes),
        ],
    ));
    let v = json(&doc);
    let notes: Vec<&str> = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["label"] == "footnote")
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        notes,
        ["Smith, J. (2020). Example Study.", "An endnote body."]
    );
    assert!(notes_texts(&doc).is_empty());
}

/// docling#4584 (`test_xml_comments_in_charts_are_removed`): a chart part's
/// comments and instructions keep its data.
#[test]
fn xml_comments_in_charts_are_removed() {
    use std::io::{Read, Write};
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/data/docx/sources/drawingml.docx");
    let bytes = std::fs::read(path).expect("drawingml.docx");
    let mut zin = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for i in 0..zin.len() {
        let mut f = zin.by_index(i).unwrap();
        let name = f.name().unwrap().to_string();
        let mut data = Vec::new();
        f.read_to_end(&mut data).unwrap();
        if name == "word/charts/chart1.xml" {
            let mut xml = String::from_utf8(data).unwrap();
            for (old, new) in [
                ("<c:plotArea>", "<c:plotArea><!-- x --><?producer note?>"),
                ("<c:v>4.3</c:v>", "<c:v>4<!-- x -->.3</c:v>"),
            ] {
                assert_eq!(xml.matches(old).count(), 1, "{old}");
                xml = xml.replace(old, new);
            }
            data = xml.into_bytes();
        }
        zw.start_file(name, opts).unwrap();
        zw.write_all(&data).unwrap();
    }
    let doc = convert(zw.finish().unwrap().into_inner());
    let md = doc.export_to_markdown();
    let row = md
        .lines()
        .find(|l| l.contains("Category 1"))
        .expect("the chart's data table");
    let cells: Vec<&str> = row
        .split('|')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    assert_eq!(cells, ["Category 1", "4.3", "2.4", "2"]);
}

#[test]
fn strip_comments_and_pis_shapes() {
    use crate::backend::ooxml::strip_comments_and_pis as strip;
    let decl = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
    let plain = format!("{decl}<a><b>x</b></a>");
    assert!(matches!(strip(&plain, None), std::borrow::Cow::Borrowed(_)));
    let mut notes = Vec::new();
    let xml =
        format!("{decl}<a><!-- one --><b>x<!--two-->y</b><![CDATA[<!-- kept -->]]><?pi z?></a>");
    assert_eq!(
        strip(&xml, Some(&mut notes)),
        format!("{decl}<a><b>xy</b><![CDATA[<!-- kept -->]]></a>")
    );
    assert_eq!(notes, ["one", "two"]);
    // An unterminated comment is left for the parser.
    assert_eq!(strip("<a><!-- open</a>", None), "<a><!-- open</a>");
}

/// docling#5322 (#1160, `test_footnote_hyperlink_keeps_its_target`): a link
/// in a footnote resolves against the notes part's relationships, and the
/// linked note is an inline group of `footnote` items.
#[test]
fn footnote_hyperlink_keeps_its_target() {
    let footnotes = format!(
        r#"<w:footnotes xmlns:w="{W}" xmlns:r="{R}"><w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:id="2"><w:p><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink r:id="rIdNote1"><w:r><w:t>the source</w:t></w:r></w:hyperlink></w:p><w:p><w:r><w:t>Second line.</w:t></w:r></w:p></w:footnote></w:footnotes>"#
    );
    let note_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdNote1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org/note" TargetMode="External"/></Relationships>"#;
    let rels = r#"<Relationship Id="rIdF" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>"#;
    let body = r#"<w:p><w:r><w:t>This is a claim that needs support</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p>"#;
    let doc = convert(docx(
        body,
        rels,
        &[
            ("word/footnotes.xml", footnotes),
            ("word/_rels/footnotes.xml.rels", note_rels.to_string()),
        ],
    ));
    let v = json(&doc);
    let texts = v["texts"].as_array().unwrap();
    let linked: Vec<(&str, &str)> = texts
        .iter()
        .filter(|t| t["label"] == "footnote" && t["hyperlink"].is_string())
        .map(|t| {
            (
                t["text"].as_str().unwrap(),
                t["hyperlink"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(linked, [("the source", "https://example.org/note")]);
    let parts: Vec<&str> = texts
        .iter()
        .filter(|t| t["label"] == "footnote")
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert_eq!(parts, ["See ", "the source", " Second line."]);
    let group = &v["groups"].as_array().unwrap()[0];
    assert_eq!(group["label"], "inline");
    assert_eq!(group["content_layer"], "furniture");
    let opts = docling_core::MarkdownExportOptions {
        layers: docling_core::ContentLayers::BODY.with(docling_core::ContentLayer::Furniture),
        ..Default::default()
    };
    let (md, _) = doc.export_to_markdown_with_options(&opts);
    assert!(
        md.contains("See [the source](https://example.org/note) Second line."),
        "{md}"
    );
    // The default Markdown leaves the note out.
    assert!(!doc.export_to_markdown().contains("the source"));
}

/// A note without a link stays one plain `footnote` text.
#[test]
fn footnote_without_a_link_stays_one_text() {
    let footnotes = format!(
        r#"<w:footnotes xmlns:w="{W}"><w:footnote w:id="2"><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Bold</w:t></w:r><w:r><w:t xml:space="preserve"> note.</w:t></w:r></w:p><w:p><w:r><w:t>Second.</w:t></w:r></w:p></w:footnote></w:footnotes>"#
    );
    let rels = r#"<Relationship Id="rIdF" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>"#;
    let body =
        r#"<w:p><w:r><w:t>Claim</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p>"#;
    let doc = convert(docx(body, rels, &[("word/footnotes.xml", footnotes)]));
    let v = json(&doc);
    let notes: Vec<&str> = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["label"] == "footnote")
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert_eq!(notes, ["Bold note. Second."]);
    assert!(v["groups"].as_array().unwrap().is_empty());
}

/// The JSON item at `#/<kind>/<n>`.
fn resolve<'v>(v: &'v Value, r: &Value) -> &'v Value {
    let path = r["$ref"].as_str().unwrap();
    let mut it = path.trim_start_matches("#/").split('/');
    let (kind, idx) = (it.next().unwrap(), it.next());
    match idx {
        Some(i) => &v[kind][i.parse::<usize>().unwrap()],
        None => &v[kind],
    }
}

fn children_texts(v: &Value, item: &Value) -> Vec<String> {
    item["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            resolve(v, c)["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .filter(|t| !t.trim().is_empty())
        .collect()
}

/// docling#5258 (#1161, `test_single_cell_table_in_cell_stays_inside_the_cell`
/// and `…_keeps_its_place_between_paragraphs`): a 1×1 table in a cell
/// unwraps into that cell, in place.
#[test]
fn single_cell_table_in_cell_stays_inside_the_cell() {
    let inner = |text: &str| {
        format!(
            "<w:tbl><w:tblGrid><w:gridCol/></w:tblGrid><w:tr>{}</w:tr></w:tbl>",
            tc(text)
        )
    };
    let grid = "<w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>";
    // python-docx's `cell.add_table` leaves the cell's empty paragraph
    // before the table and adds one after it.
    let body = format!(
        "<w:tbl>{grid}<w:tr>{}<w:tc>{}{}{}</w:tc></w:tr></w:tbl>{}",
        tc("left"),
        p(""),
        inner("boxed"),
        p(""),
        p("after the table"),
    );
    let doc = convert(docx(&body, "", &[]));
    let v = json(&doc);
    assert_eq!(v["tables"].as_array().unwrap().len(), 1);
    let table = &v["tables"][0];
    assert_eq!(table["children"].as_array().unwrap().len(), 1, "{table}");
    let group = resolve(&v, &table["children"][0]);
    assert_eq!(children_texts(&v, group), ["boxed"]);
    assert_eq!(children_texts(&v, &v["body"]), ["after the table"]);
    let md = doc.export_to_markdown();
    assert!(
        md.lines()
            .any(|l| l.starts_with('|') && l.contains("left") && l.contains("boxed")),
        "{md}"
    );

    let body = format!(
        "<w:tbl>{grid}<w:tr>{}<w:tc>{}{}{}</w:tc></w:tr></w:tbl>",
        tc("left"),
        p("before"),
        inner("boxed"),
        p("after"),
    );
    let v = json(&convert(docx(&body, "", &[])));
    let group = resolve(&v, &v["tables"][0]["children"][0]);
    assert_eq!(children_texts(&v, group), ["before", "boxed", "after"]);
}

/// docling#5309 (`test_table_right_after_a_list_closes_the_list`, flat and
/// nested): a table right after a list item is the list's sibling, and the
/// next item opens a new list.
#[test]
fn table_right_after_a_list_closes_the_list() {
    let numbering = (
        "word/numbering.xml",
        format!(
            r#"<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="•"/></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/><w:lvlText w:val="o"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
        ),
    );
    let item = |text: &str, ilvl: u8| {
        format!(
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
        )
    };
    let grid = "<w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>";
    let table = format!(
        "<w:tbl>{grid}<w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl>",
        tc("cell 00"),
        tc("cell 01"),
        tc("cell 10"),
        tc("cell 11")
    );
    for nested in [false, true] {
        let mut body = item("item", 0);
        if nested {
            body += &item("sub item", 1);
        }
        body += &table;
        body += &item("next item", 0);
        let rels = format!("{STYLES_REL}{NUMBERING_REL}");
        let doc = convert(docx(&body, &rels, &[styles(), numbering.clone()]));
        let v = json(&doc);
        assert_eq!(
            v["tables"][0]["parent"]["$ref"], "#/body",
            "nested={nested}"
        );
        for group in v["groups"].as_array().unwrap() {
            if group["label"] == "list" {
                for child in group["children"].as_array().unwrap() {
                    let c = resolve(&v, child);
                    assert_eq!(c["label"], "list_item", "nested={nested}: {group}");
                }
            }
        }
        let parent_of = |text: &str| {
            v["texts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["text"] == text)
                .map(|t| t["parent"]["$ref"].clone())
                .unwrap()
        };
        assert_ne!(parent_of("next item"), parent_of("item"), "nested={nested}");
        let md = doc.export_to_markdown();
        assert!(md.contains("\n\n| cell 00"), "nested={nested}: {md}");
    }
}

/// docling#4622 (#1162, `test_equation_next_to_symbol_font_text_is_kept`):
/// an inline equation is kept when its paragraph or cell holds Symbol text.
#[test]
fn equation_next_to_symbol_font_text_is_kept() {
    let omath = r#"<m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"><m:r><m:t>x=1</m:t></m:r></m:oMath>"#;
    let sym = |c: &str| format!(r#"<w:r><w:sym w:font="Symbol" w:char="{c}"/></w:r>"#);
    let grid = "<w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>";
    let body = format!(
        r#"<w:p><w:r><w:t xml:space="preserve">sym </w:t></w:r>{}{omath}</w:p><w:p>{}{omath}</w:p><w:tbl>{grid}<w:tr><w:tc><w:p><w:r><w:rPr><w:rFonts w:ascii="Symbol" w:hAnsi="Symbol"/></w:rPr><w:t>a</w:t></w:r>{omath}</w:p></w:tc>{}</w:tr></w:tbl>"#,
        sym("F061"),
        sym("F062"),
        tc("b"),
    );
    let v = json(&convert(docx(&body, "", &[])));
    let texts: Vec<(&str, &str)> = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["text"].as_str().unwrap(), t["label"].as_str().unwrap()))
        .collect();
    assert!(texts.contains(&("sym α", "text")), "{texts:?}");
    assert!(texts.contains(&("β", "text")), "{texts:?}");
    let formulas: Vec<&str> = texts
        .iter()
        .filter(|(_, l)| *l == "formula")
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(formulas, ["x=1", "x=1"]);
    let cells: Vec<&str> = v["tables"][0]["data"]["table_cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["text"].as_str().unwrap())
        .collect();
    assert_eq!(cells, ["α$x=1$", "b"]);
}
