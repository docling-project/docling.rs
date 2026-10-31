//! #628: the DOCX backend walks a document twice — `docx.rs` builds the flat
//! [`Node`] stream (Markdown, DocLang, LaTeX), `docx_tree.rs` the item tree
//! (JSON, and HTML/Pandoc from it). The walks share the XML readers but not
//! the traversal, so text one keeps the other can lose: #532/#533/#534 (a
//! text box, a table in a text box, a block `w:sdt` in a table cell —
//! Markdown), #546 (`w:ins` in a cell — JSON), #589 (inline `w:customXml` —
//! Markdown) were each found by a user, because nothing compared the two.
//!
//! These tests do. Both chains come from **one** `convert()` (`doc.nodes`
//! and `doc.tree`), and the comparison is the one the issue's probe makes:
//! the *set* of words (two or more letters, lower-cased) each chain carries,
//! never their order or count — the chains legitimately place things
//! differently (#532 keeps a cell's text-box text in the cell for the flat
//! outputs and hoists it before the table in the JSON, as docling does) and
//! only a word present in one chain and absent from the other is a loss.
//! Formatting is out of scope (rs-24 / #593 was a bold divergence; a word
//! set cannot see it).
//!
//! Two nets: a generated matrix of single-feature packages — every inline /
//! block wrapper the walks special-case, in every container context, so a
//! failure names the feature and the context — and the whole DOCX fixture
//! corpus. Long term the flat chain is to be retired for the item tree
//! (docling has one walk, `_walk_linear`); until then this is the safety
//! net, and goes with it.

use super::DeclarativeBackend;
use crate::{InputFormat, SourceDocument};
use docling_core::tree::{ItemTree, TreeKind};
use docling_core::{DoclingDocument, Node, Table};
use std::collections::BTreeSet;
use std::io::Write;

type Words = BTreeSet<String>;

/// The issue's word definition: runs of two or more letters, lower-cased —
/// digits, markers, escapes and punctuation fall away, so `1.2.` prefixes,
/// `\_` escapes and `**` markers never register as differences. The flat
/// text is Markdown-ready — a link's target (the tree keeps it in
/// `hyperlink`), the `<!-- image -->` placeholder a cell prints for a
/// picture, and `&lt;`/`&gt;`/`&amp;` entities are undone before counting.
fn words_of(text: &str, into: &mut Words) {
    let stripped = cached_regex!(r"!?\[([^\]]*)\]\([^)]*\)").replace_all(text, "$1");
    let stripped = cached_regex!(r"<!--.*?-->").replace_all(&stripped, " ");
    let stripped = stripped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    for w in cached_regex!(r"\p{L}{2,}").find_iter(&stripped) {
        into.insert(w.as_str().to_lowercase());
    }
}

fn table_words(t: &Table, into: &mut Words) {
    for cell in t.rows.iter().flatten() {
        words_of(cell, into);
    }
    if let Some(cells) = &t.cells {
        for c in cells {
            words_of(&c.text, into);
        }
    }
    if let Some(blocks) = &t.cell_blocks {
        for node in blocks.iter().flatten().flatten() {
            node_words(node, into);
        }
    }
    if let Some(c) = &t.caption {
        words_of(c, into);
    }
}

/// Every word the flat chain carries, through all the wrappers a node can
/// sit in.
fn node_words(node: &Node, into: &mut Words) {
    match node {
        Node::Heading { text, .. }
        | Node::Paragraph { text }
        | Node::ListText { text, .. }
        | Node::CheckboxItem { text, .. }
        | Node::Caption { text, .. }
        | Node::LabeledText { text, .. }
        | Node::PageFurniture { text, .. }
        | Node::FurnitureText { text, .. }
        | Node::TextDump(text) => words_of(text, into),
        // A list marker is metadata on both sides — the tree's `ListMeta`,
        // the flat node's `marker` — and the flat text of a multilevel or
        // lettered item carries it as a prefix (`iv. text`); the DocLang
        // view holds the clean text there. Markers are left out of the
        // comparison on both sides (the flat Markdown drops a marker without
        // ASCII letters or digits on purpose — MIGRATION, docling#4336).
        Node::ListItem { text, dclx, .. } => match dclx {
            Some(d) => words_of(&d.text, into),
            None => words_of(text, into),
        },
        Node::Code { text, .. } => words_of(text, into),
        Node::Table(t) => table_words(t, into),
        Node::Picture { caption, .. } => {
            if let Some(c) = caption {
                words_of(c, into);
            }
        }
        Node::Formula { latex, orig, .. } => {
            words_of(latex, into);
            words_of(orig, into);
        }
        Node::Chart { table, caption, .. } => {
            table_words(table, into);
            if let Some(c) = caption {
                words_of(c, into);
            }
        }
        Node::Group { children, .. } | Node::PictureChildren(children) => {
            for c in children {
                node_words(c, into);
            }
        }
        Node::FieldRegion { items } => {
            for f in items {
                for s in [&f.marker, &f.key, &f.value].into_iter().flatten() {
                    words_of(s, into);
                }
            }
        }
        Node::KeyValueGraph { cells, .. } => {
            for c in cells {
                words_of(&c.text, into);
            }
        }
        Node::InlineGroup { md_text, runs, .. } => {
            words_of(md_text, into);
            for r in runs {
                words_of(&r.text, into);
            }
        }
        Node::CommentSection { text, .. } => words_of(text, into),
        Node::Furniture { inner, .. }
        | Node::Commented { inner, .. }
        | Node::Track { inner, .. }
        | Node::Located { inner, .. }
        | Node::Prov { inner, .. }
        | Node::DoclangOnly(inner) => node_words(inner, into),
        Node::PageBreak | Node::PageInfo { .. } | Node::Anchor { .. } => {}
    }
}

fn flat_words(doc: &DoclingDocument) -> Words {
    let mut out = Words::new();
    for n in &doc.nodes {
        node_words(n, &mut out);
    }
    out
}

/// Every word the item tree carries: text and code items, table grids and
/// chart data, form and graph cells, and the footnote calls placed on items.
fn tree_words(tree: &ItemTree) -> Words {
    let mut out = Words::new();
    for item in &tree.items {
        if item.deleted {
            continue;
        }
        match &item.kind {
            TreeKind::Text { text, .. } => words_of(text, &mut out),
            TreeKind::Code { text, .. } => words_of(text, &mut out),
            TreeKind::Table { table, .. } => table_words(table, &mut out),
            TreeKind::Picture { chart, .. } => {
                if let Some(t) = chart {
                    table_words(t, &mut out);
                }
            }
            TreeKind::FieldRegion { items } => {
                for f in items {
                    for s in [&f.marker, &f.key, &f.value].into_iter().flatten() {
                        words_of(s, &mut out);
                    }
                }
            }
            TreeKind::KeyValueGraph { cells, .. } => {
                for c in cells {
                    words_of(&c.text, &mut out);
                }
            }
            TreeKind::Group { .. } => {}
        }
        for note in &item.notes {
            words_of(&note.text, &mut out);
        }
    }
    out
}

/// Words one chain has and the other lacks: `(flat_only, tree_only)`.
fn chain_divergence(bytes: Vec<u8>, name: &str) -> (Words, Words) {
    let src = SourceDocument::from_bytes(name, InputFormat::Docx, bytes);
    let doc = super::DocxBackend::default()
        .convert(&src)
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    let flat = flat_words(&doc);
    let tree = tree_words(
        doc.tree
            .as_ref()
            .expect("the DOCX backend builds an item tree"),
    );
    (
        flat.difference(&tree).cloned().collect(),
        tree.difference(&flat).cloned().collect(),
    )
}

// ---------------------------------------------------------------------------
// The generated matrix.

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const V: &str = "urn:schemas-microsoft-com:vml";
const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const WPS: &str = "http://schemas.microsoft.com/office/word/2010/wordprocessingShape";

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

/// A complete package around `body` (the `w:body` content): styles with a
/// heading, one decimal list, a footnote, a comment, and the relationships
/// a payload may reference (`rId1` hyperlink, `rId9` footnotes).
fn docx_with_body(body: &str) -> Vec<u8> {
    let document = format!(
        r#"<w:document xmlns:w="{W}" xmlns:r="{R}" xmlns:v="{V}" xmlns:wp="{WP}" xmlns:a="{A}" xmlns:wps="{WPS}"><w:body>{body}<w:sectPr/></w:body></w:document>"#
    );
    let styles = format!(
        r#"<w:styles xmlns:w="{W}"><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style></w:styles>"#
    );
    let numbering = format!(
        r#"<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
    );
    let footnotes = format!(
        r#"<w:footnotes xmlns:w="{W}"><w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:id="1"><w:p><w:r><w:t>footnote body quokka</w:t></w:r></w:p></w:footnote></w:footnotes>"#
    );
    let comments = format!(
        r#"<w:comments xmlns:w="{W}"><w:comment w:id="0" w:author="Reviewer Name" w:date="2026-01-01T00:00:00Z"><w:p><w:r><w:t>comment body wombat</w:t></w:r></w:p></w:comment></w:comments>"#
    );
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/path" TargetMode="External"/><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="footnotes.xml"/></Relationships>"#.to_string();
    package(&[
        ("word/document.xml", document),
        ("word/styles.xml", styles),
        ("word/numbering.xml", numbering),
        ("word/footnotes.xml", footnotes),
        ("word/comments.xml", comments),
        ("word/_rels/document.xml.rels", rels),
    ])
}

fn run(text: &str) -> String {
    format!(r#"<w:r><w:t xml:space="preserve">{text}</w:t></w:r>"#)
}

fn para(inner: &str) -> String {
    format!("<w:p>{inner}</w:p>")
}

/// A 1×2 table whose first cell holds `payload` (block content).
fn table(payload: &str) -> String {
    format!(
        "<w:tbl><w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid><w:tr><w:tc>{payload}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl>",
        para(&run("sibling cell"))
    )
}

fn vml_textbox(payload: &str) -> String {
    para(&format!(
        "<w:r><w:pict><v:shape><v:textbox><w:txbxContent>{payload}</w:txbxContent></v:textbox></v:shape></w:pict></w:r>"
    ))
}

fn dml_textbox(payload: &str) -> String {
    para(&format!(
        r#"<w:r><w:drawing><wp:inline><wp:extent cx="914400" cy="914400"/><wp:docPr id="1" name="Box"/><a:graphic><a:graphicData uri="{WPS}"><wps:wsp><wps:txbx><w:txbxContent>{payload}</w:txbxContent></wps:txbx></wps:wsp></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#
    ))
}

fn block_sdt(payload: &str) -> String {
    format!("<w:sdt><w:sdtPr/><w:sdtContent>{payload}</w:sdtContent></w:sdt>")
}

/// The single-feature payloads: `(name, block XML)`. Each carries words of
/// its own, so a lost word names the feature.
fn payloads() -> Vec<(&'static str, String)> {
    vec![
        ("plain", para(&run("plain paragraph alpha"))),
        (
            "inline-sdt",
            para(&format!(
                "<w:sdt><w:sdtPr/><w:sdtContent>{}</w:sdtContent></w:sdt>",
                run("inline control bravo")
            )),
        ),
        ("block-sdt", block_sdt(&para(&run("block control charlie")))),
        (
            "ins",
            para(&format!(
                r#"<w:ins w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z">{}</w:ins>"#,
                run("inserted text delta")
            )),
        ),
        (
            "moveTo",
            para(&format!(
                r#"<w:moveTo w:id="2" w:author="a" w:date="2026-01-01T00:00:00Z">{}</w:moveTo>"#,
                run("moved text echo")
            )),
        ),
        (
            "del",
            para(&format!(
                r#"{}<w:del w:id="3" w:author="a" w:date="2026-01-01T00:00:00Z"><w:r><w:delText>deleted text foxtrot</w:delText></w:r></w:del>"#,
                run("kept before deletion")
            )),
        ),
        (
            "customXml",
            para(&format!(
                r#"<w:customXml w:element="tag">{}</w:customXml>"#,
                run("custom xml golf")
            )),
        ),
        (
            "smartTag",
            para(&format!(
                r#"<w:smartTag w:uri="urn:x" w:element="place">{}</w:smartTag>"#,
                run("smart tag hotel")
            )),
        ),
        (
            "fldSimple",
            para(&format!(
                r#"<w:fldSimple w:instr=" REF x ">{}</w:fldSimple>"#,
                run("field result india")
            )),
        ),
        (
            "hyperlink",
            para(&format!(
                r#"{}<w:hyperlink r:id="rId1">{}</w:hyperlink>"#,
                run("see "),
                run("linked text juliet")
            )),
        ),
        (
            "bold-runs",
            para(&format!(
                "{}<w:r><w:rPr><w:b/></w:rPr><w:t>bold kilo</w:t></w:r>{}",
                run("plain before "),
                run(" plain after lima")
            )),
        ),
        (
            "heading",
            para(&format!(
                r#"<w:pPr><w:pStyle w:val="Heading1"/></w:pPr>{}"#,
                run("heading text mike")
            )),
        ),
        (
            "list-item",
            para(&format!(
                r#"<w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr>{}"#,
                run("list item november")
            )),
        ),
        (
            "footnote-ref",
            para(&format!(
                r#"{}<w:r><w:footnoteReference w:id="1"/></w:r>"#,
                run("noted text oscar")
            )),
        ),
        (
            "comment-range",
            para(&format!(
                r#"<w:commentRangeStart w:id="0"/>{}<w:commentRangeEnd w:id="0"/><w:r><w:commentReference w:id="0"/></w:r>"#,
                run("commented text papa")
            )),
        ),
        (
            "vml-textbox",
            vml_textbox(&para(&run("vml box text quebec"))),
        ),
        (
            "dml-textbox",
            dml_textbox(&para(&run("dml box text romeo"))),
        ),
        ("table", table(&para(&run("inner table sierra")))),
        (
            "line-break",
            para(&format!(
                "{}<w:r><w:br/><w:t>after break tango</w:t></w:r>",
                run("before break")
            )),
        ),
    ]
}

/// A container: the payload's block XML placed inside it.
type Context = (&'static str, fn(&str) -> String);

/// The containers a payload is placed in: `(name, wrap)`.
fn contexts() -> Vec<Context> {
    vec![
        ("body", |p| p.to_string()),
        ("table-cell", table),
        ("nested-table-cell", |p| table(&table(p))),
        ("vml-textbox", vml_textbox),
        ("dml-textbox", dml_textbox),
        ("textbox-in-cell", |p| table(&vml_textbox(p))),
        ("block-sdt-in-cell", |p| table(&block_sdt(p))),
        ("block-sdt", block_sdt),
    ]
}

/// The matrix: every payload in every context, both chains carrying the
/// same words. A failure lists each `payload @ context` with the words
/// only one chain has.
#[test]
fn generated_matrix_both_chains_carry_the_same_words() {
    let mut failures = Vec::new();
    for (ctx_name, wrap) in contexts() {
        for (payload_name, payload) in payloads() {
            let body = format!("{}{}", para(&run("lead paragraph")), wrap(&payload));
            let name = format!("{payload_name} @ {ctx_name}");
            let (flat_only, tree_only) = chain_divergence(docx_with_body(&body), &name);
            if !flat_only.is_empty() || !tree_only.is_empty() {
                failures.push(format!(
                    "{name}: flat-only {flat_only:?}, tree-only {tree_only:?}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "the two DOCX walks disagree on:\n{}",
        failures.join("\n")
    );
}

/// The probe's own evidence (#589 / rs-23): inline `w:customXml` text was
/// in the JSON and missing from the Markdown on 1.97.0. Pinned on its own
/// so the matrix cannot hide it behind an unrelated failure.
#[test]
fn inline_custom_xml_is_in_both_chains() {
    let body = para(&format!(
        r#"{}<w:customXml w:element="tag">{}</w:customXml>{}"#,
        run("Prefix text. "),
        run("INSIDE_CUSTOM_XML"),
        run(" Suffix text.")
    ));
    let (flat_only, tree_only) = chain_divergence(docx_with_body(&body), "customXml");
    assert!(
        flat_only.is_empty() && tree_only.is_empty(),
        "{flat_only:?} / {tree_only:?}"
    );
}

/// A table without `w:tblGrid`, and one whose grid declares fewer columns
/// than its rows hold: upstream drops the first and cuts the second at the
/// grid (python-docx reads the column count from the grid, docling#5006); both chains
/// keep every cell — nothing the document holds is lost.
#[test]
fn tables_outside_their_grid_are_in_both_chains() {
    let row = |a: &str, b: &str, c: &str| {
        format!(
            "<w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr>",
            para(&run(a)),
            para(&run(b)),
            para(&run(c))
        )
    };
    for (name, grid) in [
        ("no grid", String::new()),
        (
            "short grid",
            "<w:tblGrid><w:gridCol/><w:gridCol/></w:tblGrid>".to_string(),
        ),
    ] {
        let body = format!(
            "<w:tbl>{grid}{}{}</w:tbl>",
            row("ant", "bee", "cat"),
            row("dog", "eel", "fox")
        );
        let (flat_only, tree_only) = chain_divergence(docx_with_body(&body), name);
        assert!(
            flat_only.is_empty() && tree_only.is_empty(),
            "{name}: {flat_only:?} / {tree_only:?}"
        );
        let src = SourceDocument::from_bytes(name, InputFormat::Docx, docx_with_body(&body));
        let doc = super::DocxBackend::default().convert(&src).unwrap();
        let json = doc.export_to_json_value();
        let data = &json["tables"][0]["data"];
        assert_eq!(data["num_cols"], 3, "{name}");
        assert_eq!(
            data["table_cells"].as_array().map(Vec::len),
            Some(6),
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// The fixture corpus.

/// Every DOCX fixture — upstream's mirrored corpus and our own regression
/// cases — through the same comparison. A fixture whose chains differ for
/// a documented reason is listed in `KNOWN` with that reason; the list is
/// kept honest: an entry that no longer diverges fails too.
#[test]
fn fixture_corpus_both_chains_carry_the_same_words() {
    const KNOWN: &[(&str, &str)] = &[];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut names = Vec::new();
    for dir in [
        root.join("tests/data/docx/sources"),
        root.join("crates/docling/tests/data/docx/sources"),
    ] {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "docx") {
                names.push(path);
            }
        }
    }
    names.sort();
    assert!(names.len() > 50, "fixture dirs moved? {}", names.len());
    // The two walks over every fixture were the crate's slowest unit test
    // (20 s serial); the fixtures are independent, so they convert in
    // parallel and the verdicts are read back in the sorted order.
    let divergences: Vec<(String, Words, Words)> = {
        use rayon::prelude::*;
        names
            .par_iter()
            .map(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let bytes = std::fs::read(path).unwrap();
                let (flat_only, tree_only) = chain_divergence(bytes, &name);
                (name, flat_only, tree_only)
            })
            .collect()
    };
    let mut failures = Vec::new();
    let mut stale: Vec<&str> = KNOWN.iter().map(|(n, _)| *n).collect();
    for (name, flat_only, tree_only) in divergences {
        let diverges = !flat_only.is_empty() || !tree_only.is_empty();
        if let Some((_, reason)) = KNOWN.iter().find(|(n, _)| *n == name) {
            stale.retain(|n| *n != name);
            if !diverges {
                failures.push(format!(
                    "{name}: listed as known ({reason}) but the chains agree now"
                ));
            }
        } else if diverges {
            failures.push(format!(
                "{name}: flat-only {flat_only:?}, tree-only {tree_only:?}"
            ));
        }
    }
    for name in stale {
        failures.push(format!("{name}: in KNOWN but no such fixture"));
    }
    assert!(
        failures.is_empty(),
        "the two DOCX walks disagree on:\n{}",
        failures.join("\n")
    );
}
