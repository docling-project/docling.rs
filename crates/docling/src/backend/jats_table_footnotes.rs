//! docling's `tests/test_backend_jats_table_footnotes.py` (docling#4818,
//! #1010): every nonempty `<fn>` directly under `table-wrap/table-wrap-foot`
//! is one `footnote` item of the table — empty, parented to the table,
//! referenced from its `footnotes` in source order — holding an inline group
//! of its runs (`footnote` runs, formulas `formula` runs), which keep the
//! source whitespace between them. A bare `<p>` in the foot and the
//! article-level `<fn-group>` handling are unchanged.
//!
//! Two upstream cases have no equivalent here: the label-filtered exports
//! (`export_to_markdown(labels=…)`; our exports take no label set) and the
//! chunk-metadata labels, which the hierarchical chunker test below checks
//! in the form our chunker reports (#1041's refs).

use serde_json::Value;

use super::JatsBackend;
use crate::backend::DeclarativeBackend;
use crate::format::InputFormat;
use crate::source::SourceDocument;
use docling_core::DoclingDocument;

fn convert(body: &str) -> DoclingDocument {
    let xml = format!(
        r#"<!DOCTYPE article
PUBLIC "-//NLM//DTD JATS (Z39.96) Journal Archiving and Interchange DTD v1.2 20190208//EN" "JATS-archivearticle1.dtd">
<article xmlns:xlink="http://www.w3.org/1999/xlink" article-type="research-article">
  <front><article-meta>
    <title-group><article-title>Table Footnote Test</article-title></title-group>
  </article-meta></front>
  <body>
    {body}
  </body>
</article>
"#
    );
    let src = SourceDocument::from_bytes(
        "table-footnotes.nxml",
        InputFormat::XmlJats,
        xml.into_bytes(),
    );
    JatsBackend::default().convert(&src).unwrap()
}

fn table_wrap(id: &str, label: &str, caption: &str, cell: &str, foot: Option<&str>) -> String {
    let foot = foot
        .map(|f| format!("<table-wrap-foot>{f}</table-wrap-foot>"))
        .unwrap_or_default();
    format!(
        r#"
    <table-wrap id="{id}">
      <label>{label}</label>
      <caption><p>{caption}</p></caption>
      <table>
        <thead><tr><th>Sample</th><th>Value</th></tr></thead>
        <tbody><tr><td>{cell}</td><td>7</td></tr></tbody>
      </table>
      {foot}
    </table-wrap>
    "#
    )
}

const SIMPLE_FOOT: &str = r#"<fn id="t1fn1"><p>Values are means of three runs.</p></fn>"#;

fn simple() -> String {
    table_wrap(
        "t1",
        "Table 1",
        "Synthetic yields.",
        "Alpha",
        Some(SIMPLE_FOOT),
    )
}

/// The JSON export with a resolver.
struct Json(Value);

impl Json {
    fn of(doc: &DoclingDocument) -> Json {
        Json(serde_json::from_str(&doc.export_to_json()).unwrap())
    }

    fn get(&self, r: &Value) -> &Value {
        let path = r["$ref"].as_str().unwrap().trim_start_matches("#/");
        let (kind, idx) = path.split_once('/').unwrap();
        &self.0[kind][idx.parse::<usize>().unwrap()]
    }

    fn tables(&self) -> &Vec<Value> {
        self.0["tables"].as_array().unwrap()
    }

    fn table(&self, i: usize) -> &Value {
        &self.tables()[i]
    }

    /// The table's footnote items, checked to be text items.
    fn footnotes(&self, table: &Value) -> Vec<&Value> {
        table["footnotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                assert!(r["$ref"].as_str().unwrap().starts_with("#/texts/"));
                self.get(r)
            })
            .collect()
    }

    /// The leaf inline runs under an item, in order (through inline groups).
    fn runs<'a>(&'a self, item: &'a Value) -> Vec<&'a Value> {
        let mut out = Vec::new();
        for r in item["children"].as_array().unwrap() {
            let child = self.get(r);
            if child["self_ref"].as_str().unwrap().starts_with("#/texts/") {
                out.push(child);
            } else {
                assert_eq!(child["label"], "inline");
                out.extend(self.runs(child));
            }
        }
        out
    }

    fn texts(&self, item: &Value) -> Vec<String> {
        self.runs(item)
            .iter()
            .map(|r| r["text"].as_str().unwrap().to_string())
            .collect()
    }

    /// A note's plain text: its runs concatenated.
    fn note_text(&self, item: &Value) -> String {
        self.texts(item).concat()
    }

    /// The note is an empty `footnote` item owned by `table`.
    fn assert_wrapper(&self, table: &Value, item: &Value) {
        assert_eq!(item["label"], "footnote");
        assert_eq!(item["text"], "");
        assert_eq!(item["parent"]["$ref"], table["self_ref"]);
    }

    fn footnote_items(&self) -> Vec<&Value> {
        self.0["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["label"] == "footnote")
            .collect()
    }
}

fn assert_ordered(haystack: &str, needles: &[&str]) {
    let mut last = 0;
    for needle in needles {
        let at = haystack[last..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing (in order) from:\n{haystack}"));
        last += at + needle.len();
    }
}

fn script(run: &Value) -> Option<&str> {
    run["formatting"]["script"].as_str()
}

#[test]
fn simple_table_footnote_is_one_wrapper_owned_by_table() {
    let doc = convert(&(simple() + "<p>Paragraph after table.</p>"));
    let j = Json::of(&doc);
    assert_eq!(j.tables().len(), 1);
    let table = j.table(0);
    let notes = j.footnotes(table);
    assert_eq!(notes.len(), 1);
    j.assert_wrapper(table, notes[0]);
    let runs = j.runs(notes[0]);
    assert_eq!(j.texts(notes[0]), ["Values are means of three runs."]);
    assert!(runs[0].get("formatting").is_none_or(Value::is_null));
    assert!(runs[0].get("hyperlink").is_none_or(Value::is_null));
    let texts: Vec<&str> = j
        .footnote_items()
        .iter()
        .filter_map(|t| t["text"].as_str().filter(|s| !s.is_empty()))
        .collect();
    assert_eq!(texts, ["Values are means of three runs."]);
}

#[test]
fn literal_xref_note_stays_one_plain_wrapper() {
    let foot = r#"<fn id="x1"><p>From sample [<xref ref-type="bibr" rid="r1">8</xref>], measured.</p></fn><fn id="x2"><p>See <xref ref-type="table-fn" rid="x1">1</xref> now.</p></fn>"#;
    let doc = convert(&table_wrap("t1", "Table 1", "Xref.", "Alpha", Some(foot)));
    let j = Json::of(&doc);
    let table = j.table(0);
    let notes = j.footnotes(table);
    let texts: Vec<String> = notes.iter().map(|n| j.note_text(n)).collect();
    assert_eq!(texts, ["From sample [8], measured.", "See 1 now."]);
    for note in &notes {
        j.assert_wrapper(table, note);
        let runs = j.runs(note);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].get("formatting").is_none_or(Value::is_null));
    }
    for out in [doc.export_to_markdown(), doc.export_to_html()] {
        assert!(out.contains("From sample [8], measured."), "{out}");
        assert!(out.contains("See 1 now."), "{out}");
        assert!(!out.contains("[ 8 ]"), "{out}");
    }
}

#[test]
fn multiple_notes_and_tables_keep_source_order() {
    let body = table_wrap(
        "t1",
        "Table 1",
        "First caption.",
        "Alpha",
        Some(
            r#"<fn id="a1"><p>First table note one.</p></fn><fn id="a2"><p>First table note two.</p></fn>"#,
        ),
    ) + "<p>Between tables.</p>"
        + &table_wrap(
            "t2",
            "Table 2",
            "Second caption.",
            "Beta",
            Some(r#"<fn id="b1"><p>Second table note.</p></fn>"#),
        );
    let doc = convert(&body);
    let j = Json::of(&doc);
    assert_eq!(j.tables().len(), 2);
    let (first, second) = (j.table(0), j.table(1));
    let first_notes = j.footnotes(first);
    let second_notes = j.footnotes(second);
    for n in &first_notes {
        j.assert_wrapper(first, n);
    }
    for n in &second_notes {
        j.assert_wrapper(second, n);
    }
    let t: Vec<String> = first_notes.iter().map(|n| j.note_text(n)).collect();
    assert_eq!(t, ["First table note one.", "First table note two."]);
    let t: Vec<String> = second_notes.iter().map(|n| j.note_text(n)).collect();
    assert_eq!(t, ["Second table note."]);
    assert_ordered(
        &doc.export_to_markdown(),
        &[
            "Table 1 First caption.",
            "Alpha",
            "First table note one.",
            "First table note two.",
            "Between tables.",
            "Table 2 Second caption.",
            "Beta",
            "Second table note.",
        ],
    );
}

#[test]
fn labels_and_missing_ids_are_kept_as_note_content() {
    let foot = r#"<fn><label>a</label><p>Note without an id.</p></fn><fn id="n2"><label>b</label><p>Note with an id.</p></fn>"#;
    let doc = convert(&table_wrap("t1", "Table 1", "Labels.", "Alpha", Some(foot)));
    let j = Json::of(&doc);
    let table = j.table(0);
    let notes = j.footnotes(table);
    assert_eq!(notes.len(), 2);
    for n in &notes {
        j.assert_wrapper(table, n);
    }
    let texts: Vec<String> = notes.iter().map(|n| j.note_text(n)).collect();
    assert!(texts[0].starts_with('a') && texts[0].contains("Note without an id."));
    assert!(texts[1].starts_with('b') && texts[1].contains("Note with an id."));
}

#[test]
fn styled_linked_and_superscript_runs_are_preserved() {
    let foot = r#"<fn id="s1"><p><sup>*</sup>Measured at <italic>room</italic> <bold>temperature</bold>; see <ext-link ext-link-type="uri" xlink:href="https://example.org/method">method</ext-link>.</p></fn>"#;
    let doc = convert(&table_wrap("t1", "Table 1", "Styles.", "Alpha", Some(foot)));
    let j = Json::of(&doc);
    let table = j.table(0);
    let notes = j.footnotes(table);
    assert_eq!(notes.len(), 1);
    j.assert_wrapper(table, notes[0]);
    let runs = j.runs(notes[0]);
    assert!(runs.iter().all(|r| r["label"] == "footnote"));
    let by = |t: &str| *runs.iter().find(|r| r["text"] == t).unwrap();
    assert_eq!(script(by("*")), Some("super"));
    assert_eq!(by("room")["formatting"]["italic"], true);
    assert_eq!(by("temperature")["formatting"]["bold"], true);
    assert_eq!(by("method")["hyperlink"], "https://example.org/method");
    let texts = j.texts(notes[0]);
    let pos = |t: &str| texts.iter().position(|x| x == t).unwrap();
    assert!(pos("*") < pos("room") && pos("room") < pos("method"));
}

#[test]
fn runs_follow_source_whitespace_around_script_markers() {
    let foot = r#"<fn id="c1"><p><sup>a</sup>CO<sub>2</sub>e, carbon equivalent.</p></fn>"#;
    let doc = convert(&table_wrap(
        "t1",
        "Table 1",
        "Scripts.",
        "Alpha",
        Some(foot),
    ));
    let j = Json::of(&doc);
    let table = j.table(0);
    let notes = j.footnotes(table);
    assert_eq!(notes.len(), 1);
    j.assert_wrapper(table, notes[0]);
    // The source has no space after the marker, so no run adds one.
    assert_eq!(j.texts(notes[0]), ["a", "CO", "2", "e, carbon equivalent."]);
    let scripts: Vec<Option<&str>> = j.runs(notes[0]).iter().map(|r| script(r)).collect();
    assert_eq!(scripts, [Some("super"), None, Some("sub"), None]);
    assert_eq!(j.note_text(notes[0]), "aCO2e, carbon equivalent.");
}

#[test]
fn inline_formula_is_a_formula_run_inside_the_wrapper() {
    let foot = r#"<fn id="f1"><p>Error bound is <inline-formula><tex-math>\epsilon &lt; 0.5</tex-math></inline-formula> for all rows.</p></fn>"#;
    let doc = convert(&table_wrap(
        "t1",
        "Table 1",
        "Formula.",
        "Alpha",
        Some(foot),
    ));
    let j = Json::of(&doc);
    let table = j.table(0);
    let notes = j.footnotes(table);
    assert_eq!(notes.len(), 1);
    j.assert_wrapper(table, notes[0]);
    let runs: Vec<(String, String)> = j
        .runs(notes[0])
        .iter()
        .map(|r| {
            (
                r["label"].as_str().unwrap().into(),
                r["text"].as_str().unwrap().into(),
            )
        })
        .collect();
    let want = [
        ("footnote", "Error bound is "),
        ("formula", "\\epsilon < 0.5"),
        ("footnote", " for all rows."),
    ];
    assert_eq!(runs, want.map(|(l, t)| (l.to_string(), t.to_string())));
    assert_ordered(
        &doc.export_to_markdown(),
        &[
            "Alpha",
            "Error bound is",
            "$\\epsilon < 0.5$",
            "for all rows.",
        ],
    );
}

/// Upstream checks `HierarchicalChunker`'s `doc_items` labels; here the
/// note reaches one chunk of its own, after the table.
#[test]
fn hierarchical_chunks_carry_the_note_text_once() {
    let note = "Chunked table note";
    let foot = format!(
        "<fn><p>{note} with <italic>emphasis</italic> and <inline-formula><tex-math>a^2</tex-math></inline-formula>.</p></fn>"
    );
    let doc = convert(
        &(table_wrap("t1", "Table 1", "Chunking.", "Alpha", Some(&foot))
            + "<p>Paragraph after table.</p>"),
    );
    let chunks = docling_core::chunker::HierarchicalChunker.chunk(&doc);
    let with_note: Vec<_> = chunks.iter().filter(|c| c.text.contains(note)).collect();
    assert_eq!(with_note.len(), 1, "{chunks:#?}");
}

#[test]
fn single_run_note_exports_once_with_inline_formatting() {
    let cases = [
        (
            "<bold>Styled note.</bold>",
            "**Styled note.**",
            "<strong>Styled note.</strong>",
        ),
        (
            r#"<ext-link xlink:href="https://example.org/note">Linked note.</ext-link>"#,
            "[Linked note.](https://example.org/note)",
            r#"<a href="https://example.org/note">Linked note.</a>"#,
        ),
        (
            "<inline-formula><tex-math>a^2</tex-math></inline-formula>",
            "$a^2$",
            "<msup><mi>a</mi><mn>2</mn></msup>",
        ),
    ];
    for (content, markdown, html) in cases {
        let foot = format!("<fn><p>{content}</p></fn>");
        let doc = convert(
            &(table_wrap("t1", "Table 1", "Single run.", "Alpha", Some(&foot))
                + "<p>After table.</p>"),
        );
        let j = Json::of(&doc);
        let table = j.table(0);
        let note = j.footnotes(table)[0];
        j.assert_wrapper(table, note);
        assert_eq!(j.runs(note).len(), 1, "{content}");
        let md = doc.export_to_markdown();
        assert_ordered(&md, &["Alpha", markdown, "After table."]);
        assert_eq!(md.matches(markdown).count(), 1, "{md}");
        assert!(!md.contains("$$"), "{md}");
        let out = doc.export_to_html();
        assert_ordered(&out, &["</table>", html, "After table."]);
        assert_eq!(out.matches(html).count(), 1, "{out}");
    }
}

#[test]
fn tables_without_notes_or_with_empty_notes_have_no_footnotes() {
    let body = table_wrap("t1", "Table 1", "No foot.", "Alpha", None)
        + &table_wrap(
            "t2",
            "Table 2",
            "Empty fn.",
            "Beta",
            Some(r#"<fn id="e1"><p>   </p></fn><fn/>"#),
        )
        + "<p>Closing paragraph.</p>";
    let doc = convert(&body);
    let j = Json::of(&doc);
    assert_eq!(j.tables().len(), 2);
    assert!(j
        .tables()
        .iter()
        .all(|t| t["footnotes"].as_array().unwrap().is_empty()));
    assert!(j.footnote_items().is_empty());
    assert!(doc.export_to_markdown().contains("Closing paragraph."));
}

#[test]
fn captions_and_cells_are_not_duplicated() {
    let doc = convert(&simple());
    let j = Json::of(&doc);
    let table = j.table(0);
    assert_eq!(table["captions"].as_array().unwrap().len(), 1);
    let captions: Vec<&str> = j.0["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["label"] == "caption")
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert_eq!(captions, ["Table 1 Synthetic yields."]);
    let cells: Vec<&str> = table["data"]["table_cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["text"].as_str().unwrap())
        .collect();
    assert_eq!(cells, ["Sample", "Value", "Alpha", "7"]);
    let md = doc.export_to_markdown();
    assert_eq!(md.matches("Synthetic yields.").count(), 1);
    assert_eq!(md.matches("Alpha").count(), 1);
    assert_eq!(md.matches("Values are means of three runs.").count(), 1);
}

#[test]
fn existing_fn_group_output_is_unchanged() {
    let body = simple()
        + r#"
    <sec>
      <title>Notes</title>
      <fn-group>
        <fn id="g1"><label>1</label><p>Article level note.</p></fn>
      </fn-group>
    </sec>"#;
    let doc = convert(&body);
    let j = Json::of(&doc);
    let headings: Vec<&str> = j.0["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["label"] == "section_header")
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert!(headings.contains(&"Footnotes"), "{headings:?}");
    let lists: Vec<&Value> = j.0["groups"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["label"] == "list" && g["name"] == "footnotes")
        .collect();
    assert_eq!(lists.len(), 1);
    let mut list_texts = Vec::new();
    for r in lists[0]["children"].as_array().unwrap() {
        let item = j.get(r);
        list_texts.push(item["text"].as_str().unwrap().to_string());
        list_texts.extend(j.texts(item));
    }
    let joined = list_texts.join(" ");
    assert!(joined.contains("Article level note."), "{joined}");
    // Table notes stay out of the article-level footnote list.
    assert!(
        !joined.contains("Values are means of three runs."),
        "{joined}"
    );
    let notes: Vec<String> = j
        .footnotes(j.table(0))
        .iter()
        .map(|n| j.note_text(n))
        .collect();
    assert_eq!(notes, ["Values are means of three runs."]);
}

#[test]
fn bare_paragraph_in_table_wrap_foot_is_unchanged() {
    let doc = convert(&table_wrap(
        "t1",
        "Table 1",
        "Bare.",
        "Alpha",
        Some("<p>Bare foot paragraph.</p>"),
    ));
    let j = Json::of(&doc);
    assert!(j.table(0)["footnotes"].as_array().unwrap().is_empty());
    assert!(j.footnote_items().is_empty());
    // Bare table-wrap-foot paragraphs stay dropped, as upstream keeps them.
    assert!(!doc.export_to_json().contains("Bare foot paragraph."));
}

/// Upstream round-trips through `model_validate_json`; here the JSON export
/// reads back through the docling-JSON reader to the same Markdown.
#[test]
fn json_roundtrip_preserves_footnote_refs_and_runs() {
    let foot = r#"<fn id="r1"><p>Plain roundtrip note.</p></fn><fn id="r2"><p>Rich <bold>roundtrip</bold> note.</p></fn>"#;
    let doc = convert(&table_wrap(
        "t1",
        "Table 1",
        "Roundtrip.",
        "Alpha",
        Some(foot),
    ));
    let j = Json::of(&doc);
    let table = j.table(0);
    let notes = j.footnotes(table);
    assert_eq!(notes.len(), 2);
    for n in &notes {
        j.assert_wrapper(table, n);
    }
    assert_eq!(j.note_text(notes[0]), "Plain roundtrip note.");
    assert_eq!(j.texts(notes[1]), ["Rich ", "roundtrip", " note."]);
    assert_eq!(j.runs(notes[1])[1]["formatting"]["bold"], true);
    let src = SourceDocument::from_bytes(
        "t.json",
        InputFormat::JsonDocling,
        doc.export_to_json().into_bytes(),
    );
    let restored = crate::DocumentConverter::new()
        .convert(src)
        .unwrap()
        .document;
    assert_eq!(restored.export_to_markdown(), doc.export_to_markdown());
}

#[test]
fn markdown_places_notes_after_table_before_following_content() {
    let foot = r#"<fn id="m1"><p>Plain markdown note.</p></fn><fn id="m2"><p>Note with <italic>emphasis</italic>.</p></fn>"#;
    let doc = convert(
        &(table_wrap("t1", "Table 1", "Markdown.", "Alpha", Some(foot))
            + "<p>Paragraph after table.</p>"),
    );
    let md = doc.export_to_markdown();
    assert_ordered(
        &md,
        &[
            "Table 1 Markdown.",
            "| Alpha",
            "Plain markdown note.",
            "*emphasis*",
            "Paragraph after table.",
        ],
    );
    assert_eq!(md.matches("Plain markdown note.").count(), 1);
    assert_eq!(md.matches("emphasis").count(), 1);
}

#[test]
fn html_places_notes_after_table_before_following_content() {
    let foot = r#"<fn id="h1"><p>Plain html note.</p></fn><fn id="h2"><p>Note with <sup>sup</sup> text.</p></fn>"#;
    let doc = convert(
        &(table_wrap("t1", "Table 1", "Html.", "Alpha", Some(foot))
            + "<p>Paragraph after table.</p>"),
    );
    let html = doc.export_to_html();
    assert_ordered(
        &html,
        &[
            "<table>",
            "Alpha",
            "</table>",
            "Plain html note.",
            "<sup>sup</sup>",
            "Paragraph after table.",
        ],
    );
    assert_eq!(html.matches("Plain html note.").count(), 1);
    assert_eq!(html.matches("<sup>sup</sup>").count(), 1);
}

fn single_note(foot: &str) -> (DoclingDocument, Vec<String>) {
    let doc = convert(&table_wrap(
        "t1",
        "Table 1",
        "Paragraphs.",
        "Alpha",
        Some(foot),
    ));
    let j = Json::of(&doc);
    let table = j.table(0);
    let notes = j.footnotes(table);
    assert_eq!(notes.len(), 1, "{foot}");
    j.assert_wrapper(table, notes[0]);
    let texts = j.texts(notes[0]);
    (doc, texts)
}

#[test]
fn paragraphs_in_one_note_keep_a_word_boundary() {
    let (doc, texts) = single_note(r#"<fn id="p1"><p>alpha</p><p>beta</p></fn>"#);
    assert_eq!(texts, ["alpha beta"]);
    assert!(doc.export_to_markdown().contains("alpha beta"));
    // Source line breaks between paragraphs give the same single space.
    for foot in [
        "<fn id=\"p2\"><p>alpha</p>\n  <p>beta</p></fn>",
        // Leading whitespace in the next paragraph is the boundary.
        r#"<fn id="p3"><p>alpha</p><p> beta</p></fn>"#,
        "<fn id=\"p4\"><p>alpha</p><p>\nbeta</p></fn>",
    ] {
        assert_eq!(single_note(foot).1, ["alpha beta"], "{foot}");
    }
}

#[test]
fn paragraph_boundary_skips_comments_and_keeps_label_and_tails() {
    let (_, t) =
        single_note(r#"<fn id="c1"><p>Before</p><!-- hidden --><?probe data?><p>after</p></fn>"#);
    assert_eq!(t, ["Before after"]);
    // A paragraph is a block, so it is also kept apart from a preceding label.
    let (_, t) = single_note(r#"<fn id="l1"><label>a</label><p>One.</p><p>Two.</p></fn>"#);
    assert_eq!(t, ["a One. Two."]);
    // Text around paragraphs keeps its source order and its existing join.
    let (_, t) = single_note(r#"<fn id="t1">lead <p>alpha</p>mid<p>beta</p> end</fn>"#);
    assert_eq!(t, ["lead alphamid beta end"]);
}

#[test]
fn paragraph_boundary_keeps_styled_and_formula_runs() {
    let (doc, t) = single_note(r#"<fn id="s1"><p><bold>alpha</bold></p><p>beta</p></fn>"#);
    assert_eq!(t, ["alpha", " beta"]);
    assert!(doc.export_to_markdown().contains("**alpha** beta"));
    let foot = r#"<fn id="f1"><p>x</p><p><inline-formula><tex-math>a^2</tex-math></inline-formula></p><tex-math>b^2</tex-math><p>y</p></fn>"#;
    let doc = convert(&table_wrap(
        "t1",
        "Table 1",
        "Paragraphs.",
        "Alpha",
        Some(foot),
    ));
    let j = Json::of(&doc);
    let note = j.footnotes(j.table(0))[0];
    let runs: Vec<(String, String)> = j
        .runs(note)
        .iter()
        .map(|r| {
            (
                r["label"].as_str().unwrap().into(),
                r["text"].as_str().unwrap().into(),
            )
        })
        .collect();
    let want = [
        ("footnote", "x "),
        ("formula", "a^2"),
        ("formula", "b^2"),
        ("footnote", " y"),
    ];
    assert_eq!(runs, want.map(|(l, t)| (l.to_string(), t.to_string())));
}

#[test]
fn note_with_only_empty_paragraphs_is_omitted() {
    let foot = "<fn id=\"e1\"><p> </p><p><!-- c --></p><p>\n</p></fn>";
    let doc = convert(&table_wrap("t1", "Table 1", "Empty.", "Alpha", Some(foot)));
    let j = Json::of(&doc);
    assert!(j.table(0)["footnotes"].as_array().unwrap().is_empty());
    assert!(j.footnote_items().is_empty());
}
