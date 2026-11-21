//! Hyperlink targets and their text scopes through DocLang (docling-core#1019,
//! #1200) — docling-core's `tests/test_doclang_hyperlinks.py`, ported: the
//! same documents built as item trees, their DocLang held to the string
//! docling-core 2.107.1's `export_to_doclang()` writes for them, and read back
//! to the runs they were built from.

use docling::{DocumentConverter, InputFormat, SourceDocument};
use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind};
use docling_core::{DoclingDocument, Table, TableCell};
use serde_json::Value;

fn read(xml: &str) -> DoclingDocument {
    DocumentConverter::new()
        .convert(SourceDocument::from_bytes(
            "t.dclg",
            InputFormat::XmlDoclang,
            xml.as_bytes().to_vec(),
        ))
        .expect("doclang")
        .document
}

/// `_text_runs`: every text item with text, in reading order, as
/// `(text, hyperlink, bold)`.
type Run = (String, Option<String>, bool);

fn runs(doc: &DoclingDocument) -> Vec<Run> {
    let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    let mut out = Vec::new();
    fn walk(v: &Value, r: &str, out: &mut Vec<Run>) {
        let item = if r == "#/body" {
            &v["body"]
        } else {
            let (bucket, i) = r[2..].split_once('/').unwrap();
            &v[bucket][i.parse::<usize>().unwrap()]
        };
        if r.starts_with("#/texts/") {
            if let Some(t) = item["text"].as_str().filter(|t| !t.is_empty()) {
                out.push((
                    t.to_string(),
                    item["hyperlink"].as_str().map(str::to_string),
                    item["formatting"]["bold"].as_bool() == Some(true),
                ));
            }
        }
        for c in item["children"].as_array().into_iter().flatten() {
            walk(v, c["$ref"].as_str().unwrap(), out);
        }
    }
    walk(&v, "#/body", &mut out);
    out
}

fn run(text: &str, link: Option<&str>, bold: bool) -> Run {
    (text.to_string(), link.map(str::to_string), bold)
}

fn text(label: &str, text: &str, link: Option<&str>, bold: bool) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting: bold.then(|| Formatting {
            bold: true,
            ..Default::default()
        }),
        hyperlink: link.map(str::to_string),
        level: None,
        list: (label == "list_item").then(|| ListMeta {
            enumerated: false,
            marker: String::new(),
        }),
    }
}

fn heading(level: u8, label_text: &str, link: Option<&str>) -> TreeKind {
    TreeKind::Text {
        label: "section_header".into(),
        text: label_text.into(),
        orig: None,
        formatting: None,
        hyperlink: link.map(str::to_string),
        level: Some(level),
        list: None,
    }
}

fn group(label: &str) -> TreeKind {
    TreeKind::Group {
        label: label.into(),
        name: "group".into(),
    }
}

fn new_tree() -> ItemTree {
    ItemTree {
        inline_whitespace: true,
        ..Default::default()
    }
}

fn cell(text: &str, col: usize) -> TableCell {
    TableCell {
        text: text.into(),
        bbox: None,
        start_row: 0,
        start_col: col,
        row_span: 1,
        col_span: 1,
        column_header: false,
        row_header: false,
        row_section: false,
    }
}

/// A one-row table of plain cells, its rich ones added by the caller.
fn table(cells: &[&str]) -> TreeKind {
    let cells: Vec<TableCell> = cells.iter().enumerate().map(|(i, t)| cell(t, i)).collect();
    TreeKind::Table {
        table: Table {
            rows: vec![cells.iter().map(|c| c.text.clone()).collect()],
            cells: Some(cells),
            ..Default::default()
        },
        rich_cells: Vec::new(),
        captions: Vec::new(),
        footnotes: Vec::new(),
    }
}

fn rich_cell(t: &mut ItemTree, table: usize, col: usize, group: usize) {
    if let TreeKind::Table { rich_cells, .. } = &mut t.items[table].kind {
        rich_cells.push((0, col, group));
    }
}

fn document(tree: ItemTree) -> DoclingDocument {
    let mut doc = DoclingDocument::new("links");
    doc.tree = Some(tree);
    doc
}

/// docling-core's DocLang for the document, then the runs it reads back.
fn roundtrip(tree: ItemTree, want_xml: &str) -> (DoclingDocument, DoclingDocument) {
    let doc = document(tree);
    let xml = doc.export_to_doclang();
    assert_eq!(xml, want_xml);
    let back = read(&xml);
    (doc, back)
}

/// `test_inline_link_roundtrip`: a styled linked run between plain ones,
/// for an absolute URL, a `mailto:`, a relative path and a fragment.
#[test]
fn inline_link_roundtrip() {
    for (target, attr) in [
        (
            "https://example.com/docs?a=1&b=2#part",
            "https://example.com/docs?a=1&amp;b=2#part",
        ),
        (
            "mailto:maintainer@example.com",
            "mailto:maintainer@example.com",
        ),
        ("../guide.md#example", "../guide.md#example"),
        ("#details", "#details"),
    ] {
        let mut t = new_tree();
        let g = t.add(None, None, group("inline"));
        t.add(Some(g), None, text("text", "before ", None, false));
        let mut linked = text("text", "linked", Some(target), true);
        if let TreeKind::Text { formatting, .. } = &mut linked {
            formatting.as_mut().unwrap().italic = true;
        }
        t.add(Some(g), None, linked);
        t.add(Some(g), None, text("text", " after", None, false));
        let (doc, back) = roundtrip(
            t,
            &format!(
                "<doclang version=\"0.9\">\n  <text>before <text><href uri=\"{attr}\"/><italic><bold>linked</bold></italic></text> after</text>\n</doclang>"
            ),
        );
        assert_eq!(runs(&back), runs(&doc), "{target}");
        assert_eq!(
            runs(&back),
            vec![
                run("before ", None, false),
                run("linked", Some(target), true),
                run(" after", None, false)
            ]
        );
    }
}

/// `test_read_href_scope`: an href applies to all content of its element —
/// runs inherit their host's, an inner one overrides it.
#[test]
fn read_href_scope() {
    let cases: Vec<(&str, Vec<Run>)> = vec![
        (
            r#"<text><href uri="https://example.com/?a=1&amp;b=2#part"/>linked</text>"#,
            vec![run(
                "linked",
                Some("https://example.com/?a=1&b=2#part"),
                false,
            )],
        ),
        (
            r#"<text><href uri="https://example.com/"/>before <bold>linked</bold> after</text>"#,
            vec![
                run("before ", Some("https://example.com/"), false),
                run("linked", Some("https://example.com/"), true),
                run(" after", Some("https://example.com/"), false),
            ],
        ),
        (
            r#"<text><href uri="https://example.com/outer"/>before <text><href uri="https://example.com/inner"/>inner</text> after</text>"#,
            vec![
                run("before ", Some("https://example.com/outer"), false),
                run("inner", Some("https://example.com/inner"), false),
                run(" after", Some("https://example.com/outer"), false),
            ],
        ),
        (
            r#"<text>before <text><href uri="https://example.com/"/>linked</text> after</text>"#,
            vec![
                run("before ", None, false),
                run("linked", Some("https://example.com/"), false),
                run(" after", None, false),
            ],
        ),
    ];
    for (body, want) in cases {
        let doc = read(&format!("<doclang version=\"0.9\">{body}</doclang>"));
        assert_eq!(runs(&doc), want, "{body}");
    }
}

/// `test_adjacent_links_preserve_text_and_targets`: two adjacent links in a
/// paragraph, a heading, a list item and a table cell keep their text and
/// targets.
#[test]
fn adjacent_links_preserve_text_and_targets() {
    let pair = r#"<text><href uri="https://example.com/first"/>first</text><text><href uri="https://example.com/second"/>second</text>"#;
    for container in ["paragraph", "heading", "list", "table"] {
        let mut t = new_tree();
        let g = match container {
            "paragraph" => t.add(None, None, group("inline")),
            "heading" => {
                let h = t.add(None, None, heading(1, "", None));
                t.add(Some(h), None, group("inline"))
            }
            "list" => {
                let l = t.add(None, None, group("list"));
                let li = t.add(Some(l), None, text("list_item", "", None, false));
                t.add(Some(li), None, group("inline"))
            }
            _ => {
                let tb = t.add(None, None, table(&["firstsecond"]));
                let g = t.add(Some(tb), None, group("inline"));
                rich_cell(&mut t, tb, 0, g);
                g
            }
        };
        t.add(
            Some(g),
            None,
            text("text", "first", Some("https://example.com/first"), false),
        );
        t.add(
            Some(g),
            None,
            text("text", "second", Some("https://example.com/second"), false),
        );
        let want = match container {
            "paragraph" => format!("  <text>{pair}</text>"),
            "heading" => format!("  <heading level=\"2\">{pair}</heading>"),
            "list" => "  <list>\n    <ldiv/>\n    <text>\n      <href uri=\"https://example.com/first\"/>\n      first\n    </text>\n    <text>\n      <href uri=\"https://example.com/second\"/>\n      second\n    </text>\n  </list>".to_string(),
            _ => format!("  <table>\n    <fcel/>\n    <text>{pair}</text>\n    <nl/>\n  </table>"),
        };
        let (doc, back) = roundtrip(t, &format!("<doclang version=\"0.9\">\n{want}\n</doclang>"));
        assert_eq!(runs(&back), runs(&doc), "{container}");
    }
}

/// `test_conversation_links_survive_dclx`: linked headings, a linked rich
/// cell next to a plain one, a linked list item and a linked code block.
#[test]
fn conversation_links_survive() {
    let mut t = new_tree();
    t.add(
        None,
        None,
        text(
            "title",
            "Source",
            Some("https://github.com/owner/repo/issues/1"),
            false,
        ),
    );
    t.add(
        None,
        None,
        heading(1, "Labels", Some("https://github.com/owner/repo/labels")),
    );
    let tb = t.add(None, None, table(&["bug", "Unexpected behavior"]));
    let g = t.add(Some(tb), None, group("inline"));
    t.add(
        Some(g),
        None,
        text(
            "text",
            "bug",
            Some("https://github.com/owner/repo/labels/bug"),
            false,
        ),
    );
    rich_cell(&mut t, tb, 0, g);
    let l = t.add(None, None, group("list"));
    t.add(
        Some(l),
        None,
        text(
            "list_item",
            "Comment",
            Some("https://github.com/owner/repo/issues/1#issuecomment-2"),
            false,
        ),
    );
    t.add(
        None,
        None,
        TreeKind::Code {
            text: "\n    value = \"<tag>&amp;</tag>\"  \n\n".into(),
            orig: None,
            language: Some("python".into()),
            formatting: None,
            hyperlink: Some("https://github.com/owner/repo/pull/3#discussion_r4".into()),
            captions: Vec::new(),
        },
    );
    let want = concat!(
        "<doclang version=\"0.9\">\n",
        "  <heading>\n    <href uri=\"https://github.com/owner/repo/issues/1\"/>\n    Source\n  </heading>\n",
        "  <heading level=\"2\">\n    <href uri=\"https://github.com/owner/repo/labels\"/>\n    Labels\n  </heading>\n",
        "  <table>\n    <fcel/>\n    <text>\n      <href uri=\"https://github.com/owner/repo/labels/bug\"/>\n      bug\n    </text>\n",
        "    <fcel/>\n    Unexpected behavior\n    <nl/>\n  </table>\n",
        "  <list>\n    <ldiv/>\n    <text>\n      <href uri=\"https://github.com/owner/repo/issues/1#issuecomment-2\"/>\n      Comment\n    </text>\n  </list>\n",
        "  <code>\n    <label value=\"Python\"/>\n    <href uri=\"https://github.com/owner/repo/pull/3#discussion_r4\"/>\n",
        "    <content><![CDATA[\n    value = \"<tag>&amp;</tag>\"  \n\n]]></content>\n  </code>\n",
        "</doclang>"
    );
    let (doc, back) = roundtrip(t, want);
    assert_eq!(runs(&back), runs(&doc));
    let v: Value = serde_json::from_str(&back.export_to_json()).unwrap();
    assert_eq!(
        v["tables"][0]["data"]["grid"][0][1]["text"],
        "Unexpected behavior"
    );
}

/// `test_linked_host_with_unlinked_run_roundtrip`: a footnote whose own link
/// covers no text of its own, over a plain and a linked run.
#[test]
fn linked_host_with_unlinked_run_roundtrip() {
    let mut t = new_tree();
    let f = t.add(
        None,
        None,
        text("footnote", "", Some("https://example.com/host"), false),
    );
    let g = t.add(Some(f), None, group("inline"));
    t.add(Some(g), None, text("text", "plain ", None, false));
    t.add(
        Some(g),
        None,
        text("text", "linked", Some("https://example.com/run"), false),
    );
    let (doc, back) = roundtrip(
        t,
        "<doclang version=\"0.9\">\n  <footnote>plain <text><href uri=\"https://example.com/run\"/>linked</text></footnote>\n</doclang>",
    );
    assert_eq!(runs(&back), runs(&doc));
}

/// `test_heading_own_href_is_not_copied_to_runs`: a heading's own link
/// stays on the heading when its body holds no linked runs.
#[test]
fn heading_own_href_is_not_copied_to_runs() {
    let doc = read(
        r#"<doclang version="0.9"><heading level="2"><href uri="https://example.com/"/>a <formula>x</formula></heading></doclang>"#,
    );
    let links: Vec<Option<String>> = runs(&doc).into_iter().map(|r| r.1).collect();
    assert_eq!(links, vec![Some("https://example.com/".to_string())]);
}

/// `test_link_shared_by_all_runs_moves_to_enclosing_element`: one `<href>`
/// on the paragraph for runs that all share it.
#[test]
fn link_shared_by_all_runs_moves_to_enclosing_element() {
    let mut t = new_tree();
    let g = t.add(None, None, group("inline"));
    t.add(
        Some(g),
        None,
        text("text", "plain ", Some("https://example.com/"), false),
    );
    t.add(
        Some(g),
        None,
        text("text", "bold", Some("https://example.com/"), true),
    );
    let (doc, back) = roundtrip(
        t,
        "<doclang version=\"0.9\">\n  <text>\n    <href uri=\"https://example.com/\"/>\n    plain <bold>bold</bold>\n  </text>\n</doclang>",
    );
    assert_eq!(runs(&back), runs(&doc));
}

/// `test_heading_with_shared_link_keeps_its_runs`: a heading never takes its
/// runs' shared link (it reads formatted-only content as flat text), so the
/// runs keep it and the heading, with no text for its own, has none.
#[test]
fn heading_with_shared_link_keeps_its_runs() {
    let mut t = new_tree();
    let h = t.add(None, None, heading(1, "", Some("https://example.com/")));
    let g = t.add(Some(h), None, group("inline"));
    t.add(
        Some(g),
        None,
        text("text", "a ", Some("https://example.com/"), false),
    );
    t.add(
        Some(g),
        None,
        text("text", "b", Some("https://example.com/"), true),
    );
    let (doc, back) = roundtrip(
        t,
        "<doclang version=\"0.9\">\n  <heading level=\"2\"><text><href uri=\"https://example.com/\"/><content>a </content></text><text><href uri=\"https://example.com/\"/><bold>b</bold></text></heading>\n</doclang>",
    );
    assert_eq!(runs(&back), runs(&doc));
    let v: Value = serde_json::from_str(&back.export_to_json()).unwrap();
    assert_eq!(v["texts"][0]["label"], "section_header");
    assert!(v["texts"][0]["hyperlink"].is_null());
}

/// The Markdown of a read document carries the links on the runs that have
/// them, as docling-core's Markdown export of the deserialized document does.
#[test]
fn markdown_keeps_the_link_scopes() {
    let cases = [
        (
            r#"<text><href uri="https://example.com/"/>before <bold>linked</bold> after</text>"#,
            "[before](https://example.com/) [**linked**](https://example.com/) [after](https://example.com/)",
        ),
        (
            r#"<text>before <text><href uri="https://example.com/"/><bold><content>linked </content></bold></text>after</text>"#,
            "before [**linked**](https://example.com/) after",
        ),
        (
            r#"<heading>A <text><href uri="https://example.com/h"/>linked</text> heading</heading>"#,
            "# A [linked](https://example.com/h) heading",
        ),
        (
            r#"<heading level="2"><href uri="https://example.com/"/>plain head</heading>"#,
            "## [plain head](https://example.com/)",
        ),
        (
            r#"<list><ldiv/><text><href uri="https://e.com/li"/>item link</text><content> tail</content><ldiv/><text><href uri="https://e.com/li2"/>only link</text></list>"#,
            "- [item link](https://e.com/li) tail\n- [only link](https://e.com/li2)",
        ),
        (
            r#"<table><ched/>A<nl/><fcel/><text><href uri="https://e.com/c"/>cell</text><nl/></table>"#,
            "| A                       |\n|-------------------------|\n| [cell](https://e.com/c) |",
        ),
        (
            r#"<footnote><href uri="https://e.com/f"/>foot</footnote>"#,
            "[foot](https://e.com/f)",
        ),
        (
            r#"<formula><href uri="https://e.com/f"/>E=mc^2</formula>"#,
            "[$$E=mc^2$$](https://e.com/f)",
        ),
        // A caption's own link has no place in docling's Markdown.
        (
            r#"<picture><caption><href uri="https://e.com/cap"/>Cap text</caption></picture>"#,
            "Cap text\n\n<!-- image -->",
        ),
        (r#"<text><href uri="../guide.md#x"/>guide</text>"#, "[guide](../guide.md#x)"),
        (r#"<text><href uri=""/>empty</text>"#, "empty"),
        // pydantic's `AnyUrl` gives a bare host its `/`.
        (
            r#"<text><href uri="https://example.com"/>bare host</text>"#,
            "[bare host](https://example.com/)",
        ),
    ];
    for (body, want) in cases {
        let doc = read(&format!("<doclang version=\"0.9\">{body}</doclang>"));
        assert_eq!(doc.export_to_markdown().trim_end(), want, "{body}");
    }
}
