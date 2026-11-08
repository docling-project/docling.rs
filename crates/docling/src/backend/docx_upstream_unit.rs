//! docling's DOCX fixes from 2.130–2.142 that upstream pins with unit tests
//! only (no groundtruth file to mirror): the tests are ported here, each
//! against both walks — the flat `docx.rs` chain through its Markdown, the
//! `docx_tree.rs` item tree through the parents and markers the JSON writes.

use super::DeclarativeBackend;
use crate::{InputFormat, SourceDocument};
use docling_core::tree::{ItemTree, TreeKind};
use docling_core::DoclingDocument;
use std::io::Write;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

fn package(parts: &[(&str, String)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(content.as_bytes()).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

fn convert(bytes: Vec<u8>) -> DoclingDocument {
    let src = SourceDocument::from_bytes("t.docx", InputFormat::Docx, bytes);
    super::DocxBackend::default()
        .convert(&src)
        .expect("converts")
}

fn tree(doc: &DoclingDocument) -> &ItemTree {
    doc.tree
        .as_ref()
        .expect("the DOCX backend builds an item tree")
}

/// The flat chain's Markdown — checked against Python docling 2.142.1's
/// `export_to_markdown()` on the same documents built with python-docx.
fn md(doc: &DoclingDocument) -> String {
    doc.export_to_markdown().trim_end().to_string()
}

fn text_of(kind: &TreeKind) -> &str {
    match kind {
        TreeKind::Text { text, .. } | TreeKind::Code { text, .. } => text,
        _ => "",
    }
}

/// The live tree item whose text is `t`.
fn item(tree: &ItemTree, t: &str) -> usize {
    tree.items
        .iter()
        .position(|it| text_of(&it.kind) == t && !it.deleted)
        .unwrap_or_else(|| panic!("no item {t:?}"))
}

fn parent(tree: &ItemTree, i: usize) -> Option<usize> {
    tree.items[i].parent
}

fn marker(tree: &ItemTree, t: &str) -> String {
    match &tree.items[item(tree, t)].kind {
        TreeKind::Text {
            list: Some(meta), ..
        } => meta.marker.clone(),
        other => panic!("{t:?} is no list item: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// docling#4465: a list indented under an open list nests under its item.

/// python-docx's default template, cut to what the list tests read: the
/// "List Bullet" / "List Number" styles each link their own `numId` at
/// `w:ilvl` 0, and the levels differ only by their left indentation (360
/// twips a level). `extra` is appended to the styles.
fn list_styles(extra: &str) -> String {
    let style = |id: &str, name: &str, num: u32| {
        format!(
            r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="{name}"/><w:basedOn w:val="Normal"/><w:pPr><w:numPr><w:numId w:val="{num}"/></w:numPr></w:pPr></w:style>"#
        )
    };
    format!(
        r#"<w:styles xmlns:w="{W}"><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/></w:style>{}{}{}{}{}{extra}</w:styles>"#,
        style("ListBullet", "List Bullet", 1),
        style("ListBullet2", "List Bullet 2", 2),
        style("ListNumber", "List Number", 3),
        style("ListNumber2", "List Number 2", 4),
        style("ListNumber3", "List Number 3", 5),
    )
}

/// One `abstractNum` + `num` per list style; `indents[k]` is the
/// `w:ind w:left` of `numId` k + 1's level 0 (`None` = no `w:ind`).
fn list_numbering(indents: [Option<u32>; 5]) -> String {
    let fmts = ["bullet", "bullet", "decimal", "decimal", "decimal"];
    let mut out = format!(r#"<w:numbering xmlns:w="{W}">"#);
    for (k, (fmt, ind)) in fmts.iter().zip(indents).enumerate() {
        let text = if *fmt == "bullet" { "&#8226;" } else { "%1." };
        let ind = ind
            .map(|l| format!(r#"<w:pPr><w:ind w:left="{l}" w:hanging="360"/></w:pPr>"#))
            .unwrap_or_default();
        out.push_str(&format!(
            r#"<w:abstractNum w:abstractNumId="{k}"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="{fmt}"/><w:lvlText w:val="{text}"/>{ind}</w:lvl></w:abstractNum>"#
        ));
    }
    for k in 0..fmts.len() {
        out.push_str(&format!(
            r#"<w:num w:numId="{}"><w:abstractNumId w:val="{k}"/></w:num>"#,
            k + 1
        ));
    }
    out.push_str("</w:numbering>");
    out
}

const STOCK_INDENTS: [Option<u32>; 5] = [Some(360), Some(720), Some(360), Some(720), Some(1080)];

fn styled(text: &str, style: &str) -> String {
    let run = if text.is_empty() {
        String::new()
    } else {
        format!("<w:r><w:t>{text}</w:t></w:r>")
    };
    format!(r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr>{run}</w:p>"#)
}

fn list_docx(body: &str, styles: String, numbering: String) -> DoclingDocument {
    convert(package(&[
        (
            "word/document.xml",
            format!(r#"<w:document xmlns:w="{W}"><w:body>{body}<w:sectPr/></w:body></w:document>"#),
        ),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
    ]))
}

fn styled_paragraphs(paragraphs: &[(&str, &str)]) -> DoclingDocument {
    let body: String = paragraphs.iter().map(|(t, s)| styled(t, s)).collect();
    list_docx(&body, list_styles(""), list_numbering(STOCK_INDENTS))
}

/// `test_list_styles_nest_by_indentation`.
#[test]
fn list_styles_nest_by_indentation() {
    let doc = styled_paragraphs(&[
        ("apple", "ListBullet"),
        ("banana", "ListBullet"),
        ("banana split", "ListBullet2"),
        ("banana bread", "ListBullet2"),
        ("cherry", "ListBullet"),
        ("first", "ListNumber"),
        ("first-a", "ListNumber2"),
        ("second", "ListNumber"),
    ]);
    assert_eq!(
        md(&doc),
        "- apple\n- banana\n    - banana split\n    - banana bread\n- cherry\n\n\
         1. first\n    1. first-a\n2. second"
    );
    let t = tree(&doc);
    let bullets = parent(t, item(t, "banana")).unwrap();
    let sub_bullets = parent(t, item(t, "banana split")).unwrap();
    // The sub-list hangs from the item above it (docling#4863), which hangs
    // from the outer list.
    assert_eq!(parent(t, sub_bullets), Some(item(t, "banana")));
    assert_eq!(parent(t, item(t, "banana")), Some(bullets));
    assert_eq!(parent(t, item(t, "banana bread")), Some(sub_bullets));
    assert_eq!(parent(t, item(t, "cherry")), Some(bullets));
    let numbers = parent(t, item(t, "first")).unwrap();
    let sub_numbers = parent(t, item(t, "first-a")).unwrap();
    assert_eq!(parent(t, sub_numbers), Some(item(t, "first")));
    assert_eq!(parent(t, item(t, "second")), Some(numbers));
    assert_eq!(marker(t, "second"), "2.");
}

/// `test_lists_at_the_same_indentation_stay_siblings`.
#[test]
fn lists_at_the_same_indentation_stay_siblings() {
    let doc = styled_paragraphs(&[
        ("bullet", "ListBullet"),
        ("bullet-a", "ListBullet2"),
        ("bullet-1", "ListNumber2"),
        ("number", "ListNumber"),
    ]);
    let t = tree(&doc);
    let bullets = parent(t, item(t, "bullet")).unwrap();
    let sub_bullets = parent(t, item(t, "bullet-a")).unwrap();
    let sub_numbers = parent(t, item(t, "bullet-1")).unwrap();
    assert_eq!(parent(t, sub_bullets), Some(item(t, "bullet")));
    assert_eq!(parent(t, sub_numbers), Some(item(t, "bullet")));
    assert_ne!(sub_numbers, sub_bullets);
    let numbers = parent(t, item(t, "number")).unwrap();
    assert_ne!(numbers, bullets);
    assert_eq!(parent(t, numbers), None);
    assert_eq!(parent(t, bullets), None);
    assert_eq!(
        md(&doc),
        "- bullet\n    - bullet-a\n    1. bullet-1\n\n1. number"
    );
}

/// `test_list_styles_return_to_the_outer_list`.
#[test]
fn list_styles_return_to_the_outer_list() {
    let doc = styled_paragraphs(&[
        ("one", "ListNumber"),
        ("one-a", "ListNumber2"),
        ("one-a-i", "ListNumber3"),
        ("two", "ListNumber"),
        ("two-a", "ListNumber2"),
        ("three", "ListNumber"),
    ]);
    let t = tree(&doc);
    let outer = parent(t, item(t, "one"));
    assert_eq!(parent(t, item(t, "two")), outer);
    assert_eq!(parent(t, item(t, "three")), outer);
    assert_eq!(
        ["one", "two", "three"].map(|x| marker(t, x)),
        ["1.", "2.", "3."]
    );
    let one_a_group = parent(t, item(t, "one-a"));
    let deepest = parent(t, item(t, "one-a-i")).unwrap();
    assert_eq!(parent(t, deepest), Some(item(t, "one-a")));
    assert_eq!(parent(t, one_a_group.unwrap()), Some(item(t, "one")));
    let second_sub_list = parent(t, item(t, "two-a")).unwrap();
    assert_eq!(parent(t, second_sub_list), Some(item(t, "two")));
    assert_ne!(Some(second_sub_list), one_a_group);
    // "two-a" resumes Word's "List Number 2" list (one `numId`), so its
    // counter goes on, as Word numbers it.
    assert_eq!(marker(t, "two-a"), "2.");
    assert_eq!(
        md(&doc),
        "1. one\n    1. one-a\n        1. one-a-i\n2. two\n    2. two-a\n3. three"
    );
}

/// `test_blank_paragraph_closes_a_nested_list_style`: the group kept for
/// reuse across the blank is the outer list's — an outer item after it
/// resumes the outer list, a nested item is not attached to it.
#[test]
fn blank_paragraph_closes_a_nested_list_style() {
    let convert_with = |last: (&str, &str)| {
        let body: String = [
            ("Section", "Heading1"),
            ("first", "ListNumber"),
            ("first-a", "ListNumber2"),
            ("", "Normal"),
            last,
        ]
        .iter()
        .map(|(t, s)| styled(t, s))
        .collect();
        list_docx(&body, list_styles(""), list_numbering(STOCK_INDENTS))
    };
    let doc = convert_with(("second", "ListNumber"));
    let t = tree(&doc);
    assert_eq!(parent(t, item(t, "second")), parent(t, item(t, "first")));
    assert_eq!(marker(t, "second"), "2.");
    assert_eq!(
        md(&doc),
        "## Section\n\n1. first\n    1. first-a\n2. second"
    );

    let doc = convert_with(("first-b", "ListNumber2"));
    let t = tree(&doc);
    assert_ne!(parent(t, item(t, "first-b")), parent(t, item(t, "first")));
    assert_eq!(
        md(&doc),
        "## Section\n\n1. first\n    1. first-a\n\n2. first-b"
    );
}

/// `test_list_nesting_follows_word_indentation_precedence`: the paragraph's
/// own `w:ind` (spelled `w:start`) overrides its numbering level's; when
/// neither sets one, the style's `basedOn` chain decides.
#[test]
fn list_nesting_follows_word_indentation_precedence() {
    // "List Number 2" inherits its indentation from a base style once its
    // numbering level no longer sets one.
    let styles = list_styles(
        r#"<w:style w:type="paragraph" w:styleId="IndentedBase"><w:name w:val="Indented Base"/><w:pPr><w:ind w:left="720"/></w:pPr></w:style>"#,
    )
    .replace(
        r#"<w:style w:type="paragraph" w:styleId="ListNumber2"><w:name w:val="List Number 2"/><w:basedOn w:val="Normal"/>"#,
        r#"<w:style w:type="paragraph" w:styleId="ListNumber2"><w:name w:val="List Number 2"/><w:basedOn w:val="IndentedBase"/>"#,
    );
    let numbering = list_numbering([Some(360), Some(720), Some(360), None, Some(1080)]);
    let body = format!(
        "{}{}{}{}",
        styled("outer", "ListBullet"),
        styled("by style", "ListNumber2"),
        styled("back", "ListBullet"),
        // "List Number" is numbered at the "List Bullet" indentation.
        r#"<w:p><w:pPr><w:pStyle w:val="ListNumber"/><w:ind w:start="720"/></w:pPr><w:r><w:t>by paragraph</w:t></w:r></w:p>"#,
    );
    let doc = list_docx(&body, styles, numbering);
    let t = tree(&doc);
    let outer = parent(t, item(t, "outer"));
    assert_eq!(parent(t, item(t, "back")), outer);
    let by_style = parent(t, item(t, "by style")).unwrap();
    assert_eq!(parent(t, parent(t, by_style).unwrap()), outer);
    let by_paragraph = parent(t, item(t, "by paragraph")).unwrap();
    assert_eq!(parent(t, parent(t, by_paragraph).unwrap()), outer);
    assert_eq!(
        md(&doc),
        "- outer\n    1. by style\n- back\n    1. by paragraph"
    );
}

/// The same `numId` keeps nesting by `w:ilvl` alone, whatever the
/// indentation says: a regression net for the lists docling always read.
#[test]
fn one_numbering_still_nests_by_ilvl() {
    let numbering = format!(
        r#"<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/><w:pPr><w:ind w:left="720"/></w:pPr></w:lvl><w:lvl w:ilvl="1"><w:numFmt w:val="decimal"/><w:lvlText w:val="%2."/><w:pPr><w:ind w:left="1440"/></w:pPr></w:lvl></w:abstractNum><w:num w:numId="7"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
    );
    let item = |t: &str, ilvl: u8| {
        format!(
            r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{ilvl}"/><w:numId w:val="7"/></w:numPr></w:pPr><w:r><w:t>{t}</w:t></w:r></w:p>"#
        )
    };
    let body = [item("a", 0), item("a1", 1), item("b", 0)].concat();
    let doc = list_docx(&body, list_styles(""), numbering);
    assert_eq!(md(&doc), "1. a\n    - 1.1. a1\n2. b");
}

// ---------------------------------------------------------------------------
// Inline content: footnotes (docling#4735), links (#4576, #4563/#4613), code.

/// A package around `body` with `rels` (extra `Relationship` elements) and
/// `parts` (extra package parts).
fn body_docx(body: &str, rels: &str, parts: &[(&str, String)]) -> DoclingDocument {
    let mut all = vec![
        (
            "word/document.xml",
            format!(
                r#"<w:document xmlns:w="{W}" xmlns:r="{R}"><w:body>{body}<w:sectPr/></w:body></w:document>"#
            ),
        ),
        (
            "word/_rels/document.xml.rels",
            format!(r#"<Relationships xmlns="{RELS}">{rels}</Relationships>"#),
        ),
    ];
    all.extend(parts.iter().map(|(n, c)| (*n, c.clone())));
    convert(package(&all))
}

/// `test_footnote_body_text_is_not_dropped_outside_plain_paragraphs`: a
/// note whose text sits in a tracked insertion, a content control or a
/// table is kept — the note body is read at any depth, through the inline
/// wrappers a body paragraph is read through.
#[test]
fn footnote_text_outside_plain_paragraphs_is_kept() {
    for (note_body, expected) in [
        (
            "<w:p><w:ins w:id='10' w:author='A' w:date='2024-01-02T03:04:05Z'>\
             <w:r><w:t>inserted footnote text</w:t></w:r></w:ins></w:p>",
            "inserted footnote text",
        ),
        (
            "<w:p><w:sdt><w:sdtPr/><w:sdtContent>\
             <w:r><w:t>sdt footnote text</w:t></w:r></w:sdtContent></w:sdt></w:p>",
            "sdt footnote text",
        ),
        (
            "<w:tbl><w:tblPr/><w:tblGrid><w:gridCol w:w='2000'/></w:tblGrid>\
             <w:tr><w:tc><w:tcPr/><w:p><w:r><w:t>note table cell</w:t></w:r></w:p>\
             </w:tc></w:tr></w:tbl>",
            "note table cell",
        ),
    ] {
        let footnotes = format!(
            r#"<w:footnotes xmlns:w="{W}"><w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:id="2">{note_body}</w:footnote></w:footnotes>"#
        );
        let doc = body_docx(
            r#"<w:p><w:r><w:t>Body</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p>"#,
            r#"<Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>"#,
            &[("word/footnotes.xml", footnotes)],
        );
        let t = tree(&doc);
        let note = item(t, expected);
        assert!(
            matches!(&t.items[note].kind, TreeKind::Text { label, .. } if label == "footnote"),
            "{expected:?}"
        );
        assert!(
            format!("{:?}", doc.nodes).contains(expected),
            "the flat chain lost {expected:?}"
        );
    }
}

/// `test_hyperlink_keeps_runs_nested_in_a_wrapper`: a link's runs may sit
/// in a tracked insertion, a smart tag, custom XML or a simple field inside
/// the link — python-docx's `Hyperlink.text` reads the direct `w:r` only.
#[test]
fn hyperlink_keeps_runs_nested_in_a_wrapper() {
    for wrapper in ["ins", "smartTag", "customXml", "fldSimple"] {
        let doc = body_docx(
            &format!(
                r#"<w:p><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink r:id="rId1"><w:r><w:t xml:space="preserve">the </w:t></w:r><w:{wrapper}><w:r><w:t>docs</w:t></w:r></w:{wrapper}></w:hyperlink></w:p>"#
            ),
            &format!(
                r#"<Relationship Id="rId1" Type="{HYPERLINK}" Target="https://example.com/" TargetMode="External"/>"#
            ),
            &[],
        );
        let t = tree(&doc);
        let links: Vec<(&str, &str)> = t
            .items
            .iter()
            .filter_map(|it| match &it.kind {
                TreeKind::Text {
                    text,
                    hyperlink: Some(h),
                    ..
                } => Some((text.as_str(), h.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(links, [("the docs", "https://example.com/")], "{wrapper}");
        assert_eq!(
            md(&doc),
            "See [the docs](https://example.com/)",
            "{wrapper}"
        );
    }
}

/// `test_fragment_only_rel_does_not_crash_backend` (docling#4563/#4613): a
/// hyperlink whose relationship is a fragment-only target — dropped from
/// the package, as docling's `_sanitize_docx` drops it — keeps its text,
/// unlinked, in one text item with the text around it.
#[test]
fn dangling_fragment_hyperlink_keeps_its_text() {
    let doc = body_docx(
        r#"<w:p><w:r><w:t xml:space="preserve">Before </w:t></w:r><w:hyperlink r:id="rId999"><w:r><w:t>internal link</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> after</w:t></w:r></w:p>"#,
        &format!(
            r##"<Relationship Id="rId999" Type="{HYPERLINK}" Target="#_Proc%C3%A9dures_sp%C3%A9ciales"/>"##
        ),
        &[],
    );
    let t = tree(&doc);
    let texts: Vec<&TreeKind> = t
        .items
        .iter()
        .filter(|it| matches!(it.kind, TreeKind::Text { .. }))
        .map(|it| &it.kind)
        .collect();
    assert_eq!(texts.len(), 1);
    assert!(matches!(
        texts[0],
        TreeKind::Text { text, hyperlink: None, .. } if text == "Before internal link after"
    ));
    assert_eq!(md(&doc), "Before internal link after");
}

/// `test_docx_code_call_pattern_is_bounded` (docling#4973).
#[test]
fn code_call_pattern_is_bounded() {
    use super::docx::is_code_call;
    assert!(is_code_call("x = foo(1, 2)"));
    assert!(is_code_call("set()"));
    assert!(!is_code_call("party(ies)"));
    for unit in ["a(", "a(,"] {
        let text = unit.repeat(64_000 / unit.len());
        let start = std::time::Instant::now();
        assert!(!is_code_call(&text));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
    // Argument lists past the bound no longer count as a call.
    assert!(!is_code_call(&format!("foo({}, 1)", "a".repeat(300))));
    // The bounds, at their edges, and the first run stopping at the first
    // code-like character.
    assert!(is_code_call(&format!("foo({}, 1)", "a".repeat(200))));
    assert!(is_code_call(&format!("foo(a,{})", "b".repeat(200))));
    assert!(!is_code_call(&format!("foo(a,{})", "b".repeat(201))));
    assert!(is_code_call("print(sys.argv)"));
    assert!(is_code_call("f( )"));
    assert!(!is_code_call("(1, 2)"));
}

// ---------------------------------------------------------------------------
// docling#4480: `use_outline_level_for_headings`.

/// A style carrying `w:outlineLvl` only for the table of contents is a
/// heading by default — how a localized heading style is found — and body
/// text with the option off; a style named a heading keeps its outline
/// level either way.
#[test]
fn outline_level_headings_follow_the_option() {
    let styles = format!(
        r#"<w:styles xmlns:w="{W}"><w:style w:type="paragraph" w:styleId="Clause"><w:name w:val="Clause"/><w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:pPr><w:outlineLvl w:val="2"/></w:pPr></w:style></w:styles>"#
    );
    let bytes = package(&[
        (
            "word/document.xml",
            format!(
                r#"<w:document xmlns:w="{W}"><w:body>{}{}<w:sectPr/></w:body></w:document>"#,
                styled("Named", "Heading1"),
                styled("Clause text", "Clause"),
            ),
        ),
        ("word/styles.xml", styles),
    ]);
    let src = SourceDocument::from_bytes("t.docx", InputFormat::Docx, bytes);
    let labels = |doc: &DoclingDocument| -> Vec<(String, Option<u8>)> {
        tree(doc)
            .items
            .iter()
            .filter_map(|it| match &it.kind {
                TreeKind::Text { label, level, .. } => Some((label.clone(), *level)),
                _ => None,
            })
            .collect()
    };
    let on = super::DocxBackend::default().convert(&src).unwrap();
    assert_eq!(md(&on), "#### Named\n\n### Clause text");
    assert_eq!(
        labels(&on),
        [
            ("section_header".to_string(), Some(3)),
            ("section_header".to_string(), Some(2))
        ]
    );
    let off = super::DocxBackend {
        use_outline_level_for_headings: false,
        ..Default::default()
    }
    .convert(&src)
    .unwrap();
    assert_eq!(md(&off), "#### Named\n\nClause text");
    assert_eq!(
        labels(&off),
        [
            ("section_header".to_string(), Some(3)),
            ("text".to_string(), None)
        ]
    );
}

// ---------------------------------------------------------------------------
// docling#4863, the iWork half: a Pages sub-list hangs from the item above.

/// `test_sub_list_belongs_to_the_item_above_it` — the JSON nests a deeper
/// level's group under the list item before it.
#[test]
fn pages_sub_list_belongs_to_the_item_above_it() {
    let bullet = r#"<sf:list-label-typeinfo><sf:text-label sf:type="bullet" sf:format="*"/></sf:list-label-typeinfo>"#;
    let xml = format!(
        r#"<?xml version="1.0"?><sl:document xmlns:sl="http://developer.apple.com/namespaces/sl" xmlns:sf="http://developer.apple.com/namespaces/sf">
          <sf:stylesheet><sf:liststyle sf:ident="ls-bullet"><sf:list-label-typeinfo sf:type="none"/>{bullet}{bullet}{bullet}</sf:liststyle></sf:stylesheet>
          <sf:text-storage><sf:text-body>
            <sf:p sf:list-style="ls-bullet" sf:list-level="1">outer one</sf:p>
            <sf:p sf:list-style="ls-bullet" sf:list-level="2">inner a</sf:p>
            <sf:p sf:list-style="ls-bullet" sf:list-level="3">inner a.1</sf:p>
            <sf:p sf:list-style="ls-bullet" sf:list-level="2">inner b</sf:p>
            <sf:p sf:list-style="ls-bullet" sf:list-level="1">outer two</sf:p>
          </sf:text-body></sf:text-storage></sl:document>"#
    );
    let src = SourceDocument::from_bytes(
        "nested.pages",
        InputFormat::Pages,
        package(&[("index.xml", xml)]),
    );
    let doc = super::IworkBackend.convert(&src).expect("converts");
    let json = doc.export_to_json_value();
    let texts = json["texts"].as_array().unwrap();
    let by_text = |t: &str| {
        texts
            .iter()
            .find(|x| x["text"] == t)
            .unwrap_or_else(|| panic!("no {t:?}"))
    };
    let parent = |t: &str| by_text(t)["parent"]["$ref"].as_str().unwrap().to_string();
    let self_ref = |t: &str| by_text(t)["self_ref"].as_str().unwrap().to_string();
    let group_parent = |g: &str| {
        let i: usize = g.rsplit('/').next().unwrap().parse().unwrap();
        json["groups"][i]["parent"]["$ref"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(parent("outer two"), parent("outer one"));
    let inner = parent("inner a");
    assert_eq!(group_parent(&inner), self_ref("outer one"));
    assert_eq!(parent("inner b"), inner);
    assert_eq!(group_parent(&parent("inner a.1")), self_ref("inner a"));
}

// ---------------------------------------------------------------------------
// docling#4958 (after 2.142.1): hidden text (`w:vanish`) leaves the default
// export — `test_backend_msword_hidden.py`.

const M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";

/// A package around `body`, with the list and heading styles of
/// [`list_styles`] plus `extra_styles`.
fn hidden_docx(body: &str, extra_styles: &str) -> DoclingDocument {
    convert(package(&[
        (
            "word/document.xml",
            format!(
                r#"<w:document xmlns:w="{W}" xmlns:m="{M}"><w:body>{body}<w:sectPr/></w:body></w:document>"#
            ),
        ),
        ("word/styles.xml", list_styles(extra_styles)),
        ("word/numbering.xml", list_numbering(STOCK_INDENTS)),
    ]))
}

fn run(text: &str) -> String {
    format!(r#"<w:r><w:t xml:space="preserve">{text}</w:t></w:r>"#)
}

fn hidden_run(text: &str) -> String {
    format!(r#"<w:r><w:rPr><w:vanish/></w:rPr><w:t xml:space="preserve">{text}</w:t></w:r>"#)
}

/// Each live text item's layer (`body` when unset), by text.
fn layers(doc: &DoclingDocument) -> std::collections::HashMap<String, String> {
    tree(doc)
        .items
        .iter()
        .filter(|it| !it.deleted)
        .filter_map(|it| match &it.kind {
            TreeKind::Text { text, .. } if !text.is_empty() => Some((
                text.clone(),
                it.layer
                    .map(|l| format!("{l:?}").to_lowercase())
                    .unwrap_or_else(|| "body".into()),
            )),
            _ => None,
        })
        .collect()
}

/// The JSON's table cells (`tables[i]`), row-major.
fn table_cells(doc: &DoclingDocument, i: usize) -> Vec<String> {
    doc.export_to_json_value()["tables"][i]["data"]["table_cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["text"].as_str().unwrap().to_string())
        .collect()
}

/// `test_docx_hidden_text_is_left_out_of_the_default_export`.
#[test]
fn hidden_text_is_left_out_of_the_default_export() {
    let body = format!(
        "<w:p>{}{}{}</w:p><w:p>{}</w:p>\
         <w:tbl><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr>\
         <w:tc><w:p>{}</w:p></w:tc><w:tc><w:p>{}</w:p></w:tc></w:tr></w:tbl>",
        run("Shown start. "),
        hidden_run("Hidden run. "),
        run("Shown end."),
        hidden_run("Hidden paragraph."),
        run("Shown cell"),
        hidden_run("Hidden cell"),
    );
    let doc = hidden_docx(&body, "");
    let l = layers(&doc);
    assert_eq!(l["Shown start. Shown end."], "body");
    assert_eq!(l["Hidden paragraph."], "invisible");
    assert_eq!(table_cells(&doc, 0), ["Shown cell", ""]);
    assert!(!md(&doc).contains("Hidden"), "{}", md(&doc));
    assert!(md(&doc).starts_with("Shown start. Shown end."));
}

/// `test_docx_hidden_text_follows_style_inheritance`: hidden by a character
/// or paragraph style, and an explicit `w:vanish w:val="0"` on a run wins
/// over a hidden style.
#[test]
fn hidden_text_follows_style_inheritance() {
    let styles = r#"<w:style w:type="character" w:styleId="HiddenChars"><w:name w:val="Hidden Chars"/><w:rPr><w:vanish/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="HiddenPara"><w:name w:val="Hidden Para"/><w:rPr><w:vanish/></w:rPr></w:style>"#;
    let body = format!(
        r#"<w:p>{}<w:r><w:rPr><w:rStyle w:val="HiddenChars"/></w:rPr><w:t>Hidden by character style.</w:t></w:r></w:p>{}<w:p><w:pPr><w:pStyle w:val="HiddenPara"/></w:pPr>{}<w:r><w:rPr><w:vanish w:val="0"/></w:rPr><w:t>Shown by override.</w:t></w:r></w:p>"#,
        run("Shown. "),
        styled("Hidden by paragraph style.", "HiddenPara"),
        run("Hidden by paragraph style too. "),
    );
    let doc = hidden_docx(&body, styles);
    let l = layers(&doc);
    assert_eq!(l["Shown."], "body");
    assert_eq!(l["Hidden by paragraph style."], "invisible");
    assert_eq!(l["Shown by override."], "body");
    assert!(!md(&doc).contains("Hidden"), "{}", md(&doc));
}

/// `test_docx_hidden_heading_and_list_item`: a hidden heading or list item
/// is invisible; its list stays on the body layer, one tight list.
#[test]
fn hidden_heading_and_list_item() {
    let body = format!(
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr>{}</w:p><w:p><w:pPr><w:pStyle w:val="ListBullet"/></w:pPr>{}</w:p>{}{}"#,
        hidden_run("Hidden heading"),
        hidden_run("Hidden item"),
        styled("Shown item 1", "ListBullet"),
        styled("Shown item 2", "ListBullet"),
    );
    let doc = hidden_docx(&body, "");
    let l = layers(&doc);
    assert_eq!(l["Hidden heading"], "invisible");
    assert_eq!(l["Hidden item"], "invisible");
    assert_eq!(l["Shown item 1"], "body");
    let t = tree(&doc);
    let lists: Vec<_> = t
        .items
        .iter()
        .filter(|it| matches!(&it.kind, TreeKind::Group { label, .. } if label == "list"))
        .map(|it| it.layer)
        .collect();
    assert_eq!(lists, [None]);
    assert_eq!(md(&doc), "- Shown item 1\n- Shown item 2");
}

/// `test_docx_hidden_text_next_to_equations`: hidden runs beside an
/// equation are dropped and the equation kept — in a paragraph, a list
/// item and a table cell; an equation with only hidden text beside it is a
/// standalone formula.
#[test]
fn hidden_text_next_to_equations() {
    let math = r#"<m:oMath><m:r><m:t>x=1</m:t></m:r></m:oMath>"#;
    let secret = hidden_run(" secret");
    let body = format!(
        r#"<w:p>{}{math}{secret}</w:p><w:p><w:pPr><w:pStyle w:val="ListBullet"/></w:pPr>{}{math}{secret}</w:p><w:p>{math}{secret}</w:p><w:tbl><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr><w:tc><w:p>{}{math}{secret}</w:p></w:tc><w:tc><w:p>{}</w:p></w:tc></w:tr></w:tbl>"#,
        run("Value "),
        run("Item "),
        run("Cell "),
        run("Other"),
    );
    let doc = hidden_docx(&body, "");
    assert_eq!(
        md(&doc),
        "Value $x=1$\n\n- Item $x=1$\n\n$$x=1$$\n\n| Cell $x=1$   | Other   |\n|--------------|---------|"
    );
    assert!(!format!("{:?}", doc.nodes).contains("secret"));
    assert!(!doc.export_to_json().contains("secret"));
}

/// `test_docx_hidden_runs_in_links_and_content_controls`: hidden runs in a
/// hyperlink or a content control are dropped; a fully hidden hyperlink
/// adds nothing to its paragraph.
#[test]
fn hidden_runs_in_links_and_content_controls() {
    let body = format!(
        r#"<w:p>{}<w:hyperlink w:anchor="target">{}</w:hyperlink><w:sdt><w:sdtContent>{}{}</w:sdtContent></w:sdt></w:p>"#,
        run("Shown start."),
        hidden_run("Hidden link."),
        hidden_run("Hidden field."),
        run(" Shown field."),
    );
    let doc = hidden_docx(&body, "");
    assert_eq!(md(&doc), "Shown start. Shown field.");
    assert!(!doc.export_to_json().contains("Hidden"));
}

/// `test_hidden_footnote_paragraph_is_left_out`: a note has no layer per
/// paragraph, so a hidden one is left out of the note's text — found in the
/// notes part even when the body has no `w:vanish`.
#[test]
fn hidden_footnote_paragraph_is_left_out() {
    let footnotes = format!(
        r#"<w:footnotes xmlns:w="{W}"><w:footnote w:id="2"><w:p><w:r><w:t>Shown note.</w:t></w:r></w:p><w:p><w:r><w:rPr><w:vanish/></w:rPr><w:t>Hidden note.</w:t></w:r></w:p></w:footnote></w:footnotes>"#
    );
    let doc = body_docx(
        r#"<w:p><w:r><w:t>Body</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r></w:p>"#,
        r#"<Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/>"#,
        &[("word/footnotes.xml", footnotes)],
    );
    let notes: Vec<&str> = tree(&doc)
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            TreeKind::Text { label, text, .. } if label == "footnote" => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notes, ["Shown note."]);
    assert!(!format!("{:?}", doc.nodes).contains("Hidden"));
}

// ---------------------------------------------------------------------------
// docling#5123 (#1017): a `w:num`'s `w:lvlOverride` — its `w:lvl` replaces
// the abstract level, its `w:startOverride` (0..=32767) sets the start.
// `tests/test_backend_msword_list_overrides.py`.

const LETTERED_LEVEL: &str = r#"<w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%1)"/></w:lvl>"#;

/// `_numbering_instances_docx`: instances sharing one decimal `%1.` abstract
/// level, `overrides` = (numId, the `w:lvlOverride` content for level 0);
/// `paragraphs` = (text, numId), `None` = no numbering.
fn numbering_instances_docx(
    overrides: &[(u32, Option<&str>)],
    paragraphs: &[(&str, Option<u32>)],
) -> DoclingDocument {
    let mut numbering = format!(
        r#"<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="90"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum>"#
    );
    for (id, ov) in overrides {
        let ov = ov
            .map(|o| format!(r#"<w:lvlOverride w:ilvl="0">{o}</w:lvlOverride>"#))
            .unwrap_or_default();
        numbering.push_str(&format!(
            r#"<w:num w:numId="{id}"><w:abstractNumId w:val="90"/>{ov}</w:num>"#
        ));
    }
    numbering.push_str("</w:numbering>");
    let body: String = paragraphs
        .iter()
        .map(|(t, id)| {
            let ppr = id
                .map(|id| {
                    format!(
                        r#"<w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="{id}"/></w:numPr></w:pPr>"#
                    )
                })
                .unwrap_or_default();
            format!("<w:p>{ppr}<w:r><w:t>{t}</w:t></w:r></w:p>")
        })
        .collect();
    list_docx(&body, list_styles(""), numbering)
}

fn markers(doc: &DoclingDocument, texts: &[&str]) -> Vec<String> {
    texts.iter().map(|t| marker(tree(doc), t)).collect()
}

/// `test_start_override_sets_the_start_of_a_numbering_instance`.
#[test]
fn start_override_sets_the_start_of_a_numbering_instance() {
    let doc = numbering_instances_docx(
        &[(91, None), (92, Some(r#"<w:startOverride w:val="5"/>"#))],
        &[
            ("alpha", Some(91)),
            ("beta", Some(91)),
            ("The list starts again at 5:", None),
            ("epsilon", Some(92)),
            ("zeta", Some(92)),
            ("A note between two items.", None),
            ("eta", Some(92)),
        ],
    );
    assert_eq!(
        markers(&doc, &["alpha", "beta", "epsilon", "zeta", "eta"]),
        ["1.", "2.", "5.", "6.", "7."]
    );
    assert_eq!(
        md(&doc),
        "1. alpha\n2. beta\n\nThe list starts again at 5:\n\n5. epsilon\n6. zeta\n\nA note between two items.\n\n7. eta"
    );
}

/// `test_level_override_replaces_the_abstract_level`.
#[test]
fn level_override_replaces_the_abstract_level() {
    let with_start = format!(r#"<w:startOverride w:val="3"/>{LETTERED_LEVEL}"#);
    let doc = numbering_instances_docx(
        &[(93, Some(LETTERED_LEVEL)), (94, Some(&with_start))],
        &[
            ("apple", Some(93)),
            ("banana", Some(93)),
            ("More fruit:", None),
            ("cherry", Some(94)),
            ("date", Some(94)),
        ],
    );
    assert_eq!(
        markers(&doc, &["apple", "banana", "cherry", "date"]),
        ["a)", "b)", "c)", "d)"]
    );
    assert_eq!(
        md(&doc),
        "- a) apple\n- b) banana\n\nMore fruit:\n\n- c) cherry\n- d) date"
    );
}

/// `test_invalid_start_override_keeps_the_start_of_the_level`.
#[test]
fn invalid_start_override_keeps_the_start_of_the_level() {
    for so in [
        r#"<w:startOverride w:val=""/>"#,
        r#"<w:startOverride w:val="five"/>"#,
        r#"<w:startOverride w:val="-1"/>"#,
        r#"<w:startOverride w:val="32768"/>"#,
        r#"<w:startOverride w:val="1000000000000000000000000000000"/>"#,
        "<w:startOverride/>",
    ] {
        let ov = format!("{so}{LETTERED_LEVEL}");
        let doc = numbering_instances_docx(
            &[(95, Some(&ov))],
            &[("apple", Some(95)), ("banana", Some(95))],
        );
        assert_eq!(markers(&doc, &["apple", "banana"]), ["a)", "b)"], "{so}");
    }
}
