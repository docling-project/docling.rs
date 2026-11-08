//! Box Notes (`.boxnote`, `application/vnd.box.boxnote`, #1033) — a port of
//! docling's `BoxNoteDocumentBackend` (docling#3722, with docling#5160's
//! list-item text from its leading paragraph and docling#5119's picture
//! caption parented to its picture).
//!
//! A Box Note is JSON: the current (post-August 2022) schema is a
//! ProseMirror-style `doc` node tree, which maps block for block onto
//! docling's items —
//!
//! * `heading` level 1 → `title`, level *n* → `section_header` level *n*−1,
//!   its inline content collapsed to one run (one styled span keeps its
//!   formatting and link; mixed spans fall back to the plain joined text);
//! * `paragraph` → one `text` item, or an `inline` group of one `text` per
//!   styled run (`strong`/`em`/`underline`/`strikethrough` marks, `link`
//!   marks whose `href` is an `http`/`https`/`mailto` URL — anything else,
//!   `javascript:` included, is dropped); a `hard_break` is a `" "` run;
//! * `bullet_list` / `ordered_list` / `check_list` → a `list` group of
//!   `list_item`s / `checkbox_selected|unselected` texts, the item's text from
//!   its leading paragraph, the rest of the item (nested lists, further
//!   paragraphs) as its children;
//! * `code_block` → a code item; `table` → a table whose cells are spans
//!   clamped to the table (docling's `table_width`), a cell holding more than
//!   one plain paragraph a rich cell (`table_cell` group under the table);
//!   `image` → a picture captioned with its `alt` (else `fileName`);
//! * every other wrapper (`blockquote`, `call_out_box`, …) is transparent.
//!
//! Notes saved before that release (the `atext`/`pool` model) are rejected
//! as unsupported, as upstream does, rather than mis-read.
//!
//! The backend builds docling's item tree call for call, so the JSON export
//! is upstream's; the flat nodes every other serializer reads are taken from
//! that tree's own JSON by the docling-JSON reader — the Markdown is what
//! docling-core serializes from the same document.

use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind};
use docling_core::{DoclingDocument, Table, TableCell};
use serde_json::Value;

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct BoxNoteBackend;

/// docling's `MAX_COLSPAN` / `MAX_ROWSPAN` (`backend/utils/table_spans.py`).
const MAX_COLSPAN: usize = 1000;
const MAX_ROWSPAN: usize = 65_534;

/// Block nesting past which deeper content is dropped: the note is a JSON
/// tree (serde_json already caps its depth at 128), and the walk recurses per
/// level.
const MAX_DEPTH: usize = 100;

/// One styled span of a block: text, formatting, hyperlink.
type Run = (String, Option<Formatting>, Option<String>);

/// Whether JSON text looks like a Box Note: an object with a `doc` node of
/// type `doc` — the check a `.json` or an unnamed stream takes. (docling
/// detects the format by its extension and media type; a docling document
/// never has a top-level `doc`.)
pub(crate) fn looks_like_boxnote(text: &str) -> bool {
    let head = text.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('{')
        && text.contains("\"doc\"")
        && serde_json::from_str::<Value>(head)
            .is_ok_and(|v| v.get("doc").and_then(|d| d.get("type")) == Some(&"doc".into()))
}

impl DeclarativeBackend for BoxNoteBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let fail = |what: &str| ConversionError::Parse(format!("boxnote: {what}"));
        let text = source.text()?;
        // A leading UTF-8 BOM would fail the JSON parse (docling decodes with
        // `utf-8-sig` for the same reason).
        let text = text.trim_start_matches('\u{feff}');
        if text.trim().is_empty() {
            return Err(fail("empty Box Note"));
        }
        let root: Value =
            serde_json::from_str(text).map_err(|e| ConversionError::with_source("boxnote", e))?;
        let Value::Object(root) = root else {
            return Err(fail("a Box Note is a JSON object"));
        };
        let Some(Value::Object(body)) = root.get("doc") else {
            if root.contains_key("atext") {
                return Err(fail(
                    "legacy Box Notes (the pre-August-2022 atext/pool format) are not \
                     supported yet; only the current Box Note format can be converted",
                ));
            }
            return Err(fail("no 'doc' node found"));
        };

        let mut b = Builder::default();
        b.blocks(children(body.get("content")), None, 0);
        let mut tree = b.tree;
        // Each run keeps its own whitespace (docling-core 1.11.0 schema).
        tree.inline_whitespace = true;

        let name = std::path::Path::new(&source.name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "file".into());
        let mut shell = DoclingDocument::new(name.clone());
        shell.tree = Some(tree);
        let json = shell.export_to_json_value();
        let mut doc = crate::backend::docling_json::read_document(json, &name, None)?;
        doc.tree = shell.tree.take();
        Ok(doc)
    }
}

/// A node's `content` array (empty when missing or not an array).
fn children(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn node_type(node: &Value) -> &str {
    node.get("type").and_then(Value::as_str).unwrap_or("")
}

fn attr<'a>(node: &'a Value, key: &str) -> Option<&'a Value> {
    node.get("attrs").and_then(|a| a.get(key))
}

fn text_kind(label: &str, run: Run) -> TreeKind {
    let (text, formatting, hyperlink) = run;
    TreeKind::Text {
        label: label.into(),
        text,
        orig: None,
        formatting,
        hyperlink,
        level: None,
        list: None,
    }
}

#[derive(Default)]
struct Builder {
    tree: ItemTree,
}

impl Builder {
    fn blocks(&mut self, nodes: &[Value], parent: Option<usize>, depth: usize) {
        if depth >= MAX_DEPTH {
            return;
        }
        for node in nodes {
            self.block(node, parent, depth);
        }
    }

    fn block(&mut self, node: &Value, parent: Option<usize>, depth: usize) {
        let content = children(node.get("content"));
        match node_type(node) {
            "heading" => {
                let (text, formatting, hyperlink) = collapse(content);
                if text.is_empty() {
                    return;
                }
                // `attrs.level or 1`: a missing, null or zero level is 1.
                let level = attr(node, "level")
                    .and_then(Value::as_u64)
                    .filter(|&l| l > 0)
                    .unwrap_or(1);
                let kind = if level <= 1 {
                    text_kind("title", (text, formatting, hyperlink))
                } else {
                    TreeKind::Text {
                        label: "section_header".into(),
                        text,
                        orig: None,
                        formatting,
                        hyperlink,
                        level: Some((level - 1).min(u8::MAX as u64) as u8),
                        list: None,
                    }
                };
                self.tree.add(parent, None, kind);
            }
            "paragraph" => self.paragraph(content, parent),
            t @ ("bullet_list" | "ordered_list" | "check_list") => {
                self.list(t == "ordered_list", content, parent, depth)
            }
            "code_block" => {
                let code = plain_text(content, 0);
                if !code.is_empty() {
                    self.tree.add(
                        parent,
                        None,
                        TreeKind::Code {
                            text: code,
                            orig: None,
                            // docling's `add_code` gets no language: the
                            // `attrs.language` hint is not passed on.
                            language: None,
                            formatting: None,
                            hyperlink: None,
                            captions: Vec::new(),
                        },
                    );
                }
            }
            "table" => self.table(content, parent, depth),
            "image" => self.image(node, parent),
            // blockquote, call_out_box and other wrappers have no item of
            // their own; their inner blocks are kept.
            _ => self.blocks(content, parent, depth + 1),
        }
    }

    fn paragraph(&mut self, content: &[Value], parent: Option<usize>) {
        let runs = runs(content);
        match runs.len() {
            0 => {}
            1 => {
                let run = runs.into_iter().next().unwrap();
                self.tree.add(parent, None, text_kind("text", run));
            }
            _ => {
                let group = self.tree.add(
                    parent,
                    None,
                    TreeKind::Group {
                        label: "inline".into(),
                        name: "group".into(),
                    },
                );
                for run in runs {
                    self.tree.add(Some(group), None, text_kind("text", run));
                }
            }
        }
    }

    fn list(&mut self, enumerated: bool, items: &[Value], parent: Option<usize>, depth: usize) {
        // docling-core writes an ordered list's group with the `list` label
        // too (upstream's groundtruth), named `list` either way.
        let group = self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: "list".into(),
                name: "list".into(),
            },
        );
        for item in items {
            let (run, nested) = split_item(item);
            let kind = if node_type(item) == "check_list_item" {
                let checked = attr(item, "checked").is_some_and(truthy);
                let label = if checked {
                    "checkbox_selected"
                } else {
                    "checkbox_unselected"
                };
                text_kind(label, run)
            } else {
                let (text, formatting, hyperlink) = run;
                TreeKind::Text {
                    label: "list_item".into(),
                    text,
                    orig: None,
                    formatting,
                    hyperlink,
                    level: None,
                    list: Some(ListMeta {
                        enumerated,
                        marker: String::new(),
                    }),
                }
            };
            let id = self.tree.add(Some(group), None, kind);
            self.blocks(nested, Some(id), depth + 1);
        }
    }

    fn table(&mut self, rows: &[Value], parent: Option<usize>, depth: usize) {
        let rows: Vec<&Value> = rows
            .iter()
            .filter(|r| node_type(r) == "table_row")
            .collect();
        if rows.is_empty() {
            return;
        }
        let table_id = self.tree.add(
            parent,
            None,
            TreeKind::Table {
                table: Table::default(),
                rich_cells: Vec::new(),
                captions: Vec::new(),
                footnotes: Vec::new(),
            },
        );
        let row_cells: Vec<Vec<&Value>> = rows
            .iter()
            .map(|row| {
                children(row.get("content"))
                    .iter()
                    .filter(|c| matches!(node_type(c), "table_cell" | "table_header"))
                    .collect()
            })
            .collect();
        let spans: Vec<Vec<(usize, usize)>> = row_cells
            .iter()
            .map(|cells| {
                cells
                    .iter()
                    .map(|c| {
                        (
                            declared_span(c, "colspan", MAX_COLSPAN),
                            declared_span(c, "rowspan", MAX_ROWSPAN),
                        )
                    })
                    .collect()
            })
            .collect();
        let width = super::html::table_width(&spans);

        let n_rows = rows.len();
        let mut occupied = std::collections::HashSet::new();
        let mut cells = Vec::new();
        let mut rich = Vec::new();
        let mut num_cols = 0usize;
        for (row_idx, row) in row_cells.iter().enumerate() {
            let mut col_idx = 0usize;
            for (cell, &(col_span, row_span)) in row.iter().zip(&spans[row_idx]) {
                while occupied.contains(&(row_idx, col_idx)) {
                    col_idx += 1;
                }
                // Kept within the table, so the work below is proportional to
                // the table, not to the declared spans.
                let row_span = row_span.min(n_rows - row_idx);
                let col_span = col_span.min(width.saturating_sub(col_idx).max(1));
                let blocks = children(cell.get("content"));
                if cell_is_rich(blocks) {
                    let group = self.tree.add(
                        Some(table_id),
                        None,
                        TreeKind::Group {
                            label: "unspecified".into(),
                            name: "table_cell".into(),
                        },
                    );
                    self.blocks(blocks, Some(group), depth + 1);
                    rich.push((row_idx, col_idx, group));
                }
                cells.push(TableCell {
                    text: cell_text(blocks),
                    bbox: None,
                    start_row: row_idx,
                    start_col: col_idx,
                    row_span,
                    col_span,
                    column_header: node_type(cell) == "table_header",
                    row_header: false,
                    row_section: false,
                });
                for r in row_idx..row_idx + row_span {
                    for c in col_idx..col_idx + col_span {
                        occupied.insert((r, c));
                    }
                }
                col_idx += col_span;
                num_cols = num_cols.max(col_idx);
            }
        }
        let mut grid = vec![vec![String::new(); num_cols]; n_rows];
        for cell in &cells {
            for row in grid.iter_mut().skip(cell.start_row).take(cell.row_span) {
                for slot in row.iter_mut().skip(cell.start_col).take(cell.col_span) {
                    *slot = cell.text.clone();
                }
            }
        }
        if let TreeKind::Table {
            table, rich_cells, ..
        } = &mut self.tree.items[table_id].kind
        {
            *table = Table {
                rows: grid,
                cells: Some(cells),
                ..Table::default()
            };
            *rich_cells = rich;
        }
    }

    fn image(&mut self, node: &Value, parent: Option<usize>) {
        let picture = self.tree.add(
            parent,
            None,
            TreeKind::Picture {
                captions: Vec::new(),
                image: None,
                classification: None,
                extra_classes: Vec::new(),
                description: None,
                confidence: None,
                chart: None,
                dpi: None,
            },
        );
        // `attrs.alt or attrs.fileName` — an empty alt falls through.
        let label = ["alt", "fileName"]
            .iter()
            .find_map(|k| attr(node, k).filter(|v| truthy(v)))
            .map(py_str);
        if let Some(label) = label {
            let caption = self.tree.add(
                Some(picture),
                None,
                text_kind("caption", (label, None, None)),
            );
            if let TreeKind::Picture { captions, .. } = &mut self.tree.items[picture].kind {
                captions.push(caption);
            }
        }
    }
}

/// Python truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// A JSON value as a Python `str` field receives it: strings verbatim,
/// anything else in its JSON spelling.
fn py_str(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

/// A cell's `rowspan` / `colspan`, clamped to `[1, limit]`; a non-integer is 1.
fn declared_span(cell: &Value, key: &str, limit: usize) -> usize {
    match attr(cell, key) {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {
            let v = n.as_i64().unwrap_or(i64::MAX);
            v.clamp(1, limit as i64) as usize
        }
        Some(Value::Bool(b)) => usize::from(*b).max(1),
        _ => 1,
    }
}

/// A list item's own text — its leading paragraph collapsed to one run — and
/// the rest of the item, added as the item's children (docling#5160).
fn split_item(item: &Value) -> (Run, &[Value]) {
    let content = children(item.get("content"));
    match content.first() {
        Some(first) if node_type(first) == "paragraph" => {
            (collapse(children(first.get("content"))), &content[1..])
        }
        _ => ((String::new(), None, None), content),
    }
}

/// Inline content reduced to one run: a single styled span keeps its style; a
/// mixed span falls back to the plain joined text.
fn collapse(content: &[Value]) -> Run {
    let mut runs = runs(content);
    if runs.len() == 1 {
        return runs.pop().unwrap();
    }
    (runs.into_iter().map(|r| r.0).collect(), None, None)
}

fn runs(content: &[Value]) -> Vec<Run> {
    let mut out = Vec::new();
    for node in content {
        match node_type(node) {
            "text" => {
                let text = node.get("text").map(py_str).unwrap_or_default();
                if !text.is_empty() && node.get("text").is_some_and(truthy) {
                    let (formatting, hyperlink) = marks(children(node.get("marks")));
                    out.push((text, formatting, hyperlink));
                }
            }
            "hard_break" => out.push((" ".into(), None, None)),
            _ => {}
        }
    }
    out
}

fn marks(marks: &[Value]) -> (Option<Formatting>, Option<String>) {
    let mut formatting: Option<Formatting> = None;
    let mut hyperlink = None;
    for mark in marks {
        match node_type(mark) {
            "strong" => formatting.get_or_insert_with(Formatting::default).bold = true,
            "em" => formatting.get_or_insert_with(Formatting::default).italic = true,
            "underline" => formatting.get_or_insert_with(Formatting::default).underline = true,
            "strikethrough" => {
                formatting
                    .get_or_insert_with(Formatting::default)
                    .strikethrough = true
            }
            "link" => {
                if let Some(href) = attr(mark, "href").and_then(Value::as_str) {
                    if !href.is_empty() {
                        // An unsafe or malformed later link clears an earlier
                        // one, as upstream's reassignment does.
                        hyperlink = safe_url(href);
                    }
                }
            }
            _ => {}
        }
    }
    (formatting, hyperlink)
}

/// The text of inline nodes, hard breaks as newlines, nested content
/// flattened — upstream's `_plain_text`.
fn plain_text(nodes: &[Value], depth: usize) -> String {
    let mut out = String::new();
    if depth >= MAX_DEPTH {
        return out;
    }
    for node in nodes {
        match node_type(node) {
            "text" => out.push_str(&node.get("text").map(py_str).unwrap_or_default()),
            "hard_break" => out.push('\n'),
            _ => out.push_str(&plain_text(children(node.get("content")), depth + 1)),
        }
    }
    out
}

/// A cell's plain text: each block's text, stripped, joined with spaces.
fn cell_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .map(|b| plain_text(children(b.get("content")), 0).trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A cell is rich when it cannot be one plain-text cell: several blocks, a
/// non-paragraph block, or a lone paragraph with formatting or a link (a
/// plain cell has no hyperlink field, so the link would be lost).
fn cell_is_rich(blocks: &[Value]) -> bool {
    let meaningful: Vec<&Value> = blocks
        .iter()
        .filter(|b| node_type(b) != "paragraph" || !runs(children(b.get("content"))).is_empty())
        .collect();
    if meaningful.len() > 1 || meaningful.iter().any(|b| node_type(b) != "paragraph") {
        return true;
    }
    meaningful.iter().any(|b| {
        runs(children(b.get("content")))
            .iter()
            .any(|(_, f, h)| f.is_some() || h.is_some())
    })
}

/// A link mark's `href` as docling keeps it: only `http`, `https` and
/// `mailto` URLs become hyperlinks (a `javascript:` or a bare string is
/// dropped), validated and normalized the way pydantic's `AnyUrl` stores
/// them — scheme and host lower-cased, an empty path written as `/`.
fn safe_url(href: &str) -> Option<String> {
    let (scheme, rest) = href.split_once(':')?;
    let scheme_ok = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !scheme_ok {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "mailto" => (!rest.is_empty()).then(|| format!("mailto:{rest}")),
        "http" | "https" => {
            let rest = rest.strip_prefix("//")?;
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let (authority, tail) = rest.split_at(end);
            let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            // urlparse rejects unbalanced IPv6 brackets; AnyUrl wants a host.
            if host_port.contains('[') != host_port.contains(']') {
                return None;
            }
            let host = if host_port.starts_with('[') {
                &host_port[..=host_port.find(']')?]
            } else {
                host_port.split(':').next().unwrap_or("")
            };
            if host.is_empty() || authority.chars().any(char::is_whitespace) {
                return None;
            }
            let authority = match authority.rsplit_once('@') {
                Some((user, h)) => format!("{user}@{}", h.to_ascii_lowercase()),
                None => authority.to_ascii_lowercase(),
            };
            let tail = if tail.starts_with('/') {
                tail.to_string()
            } else {
                format!("/{tail}")
            };
            Some(format!("{scheme}://{authority}{tail}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn convert(payload: &Value) -> DoclingDocument {
        try_convert(payload.to_string().as_bytes()).expect("box note converts")
    }

    fn try_convert(bytes: &[u8]) -> Result<DoclingDocument, ConversionError> {
        let source =
            SourceDocument::from_bytes("test.boxnote", crate::InputFormat::Boxnote, bytes.to_vec());
        BoxNoteBackend.convert(&source)
    }

    fn doc_of(content: Value) -> Value {
        json!({"doc": {"type": "doc", "content": content}})
    }

    fn json_of(doc: &DoclingDocument) -> Value {
        serde_json::from_str(&doc.export_to_json()).unwrap()
    }

    fn linked(href: Value) -> Value {
        doc_of(json!([{"type": "paragraph", "content": [
            {"type": "text", "marks": [{"type": "link", "attrs": {"href": href}}], "text": "click"}
        ]}]))
    }

    /// The upstream sample: Markdown and JSON structure match the mirrored
    /// groundtruth (the JSON up to the origin, which docling.rs writes
    /// generically).
    #[test]
    fn upstream_sample_matches_groundtruth() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/boxnote");
        let src = std::fs::read(root.join("sources/sample.boxnote")).unwrap();
        let source = SourceDocument::from_bytes("sample.boxnote", crate::InputFormat::Boxnote, src);
        let doc = BoxNoteBackend.convert(&source).unwrap();
        let md = std::fs::read_to_string(root.join("groundtruth/sample.boxnote.md")).unwrap();
        let mut compact = doc.clone();
        compact.compact_tables = true;
        assert_eq!(compact.export_to_markdown(), md.trim_end());
        let mut want: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("groundtruth/sample.boxnote.json")).unwrap(),
        )
        .unwrap();
        let mut got = json_of(&doc);
        for v in [&mut want, &mut got] {
            v.as_object_mut().unwrap().remove("origin");
        }
        assert_eq!(got, want);
    }

    #[test]
    fn legacy_box_note_reports_unsupported() {
        let legacy = json!({
            "atext": {"text": "Hello, World!\n", "attribs": "*0+d|1+1"},
            "pool": {"numToAttrib": {"0": ["author", "1"]}, "nextNum": 1},
        });
        let err = try_convert(legacy.to_string().as_bytes()).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("legacy"), "{err}");
    }

    #[test]
    fn empty_and_non_object_notes_fail_cleanly() {
        for bytes in [&b""[..], b"[1, 2, 3]", b"42", b"null", b"\"hello\"", b"{}"] {
            assert!(
                try_convert(bytes).is_err(),
                "{:?}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn utf8_bom_does_not_fail_the_load() {
        let payload =
            doc_of(json!([{"type": "paragraph", "content": [{"type": "text", "text": "Hi"}]}]));
        let mut bytes = b"\xef\xbb\xbf".to_vec();
        bytes.extend(payload.to_string().as_bytes());
        assert_eq!(try_convert(&bytes).unwrap().export_to_markdown(), "Hi");
    }

    #[test]
    fn list_item_text_keeps_boundary_before_rich_paragraph() {
        let run = |t: &str, m: Option<&str>| match m {
            Some(m) => json!({"type": "text", "text": t, "marks": [{"type": m}]}),
            None => json!({"type": "text", "text": t}),
        };
        let item = json!({"type": "list_item", "content": [
            {"type": "paragraph", "content": [run("Intro", None)]},
            {"type": "paragraph", "content": [run("more ", None), run("x", Some("strong")), run(" y", None)]},
        ]});
        let doc = convert(&doc_of(json!([{"type": "bullet_list", "content": [item]}])));
        assert_eq!(doc.export_to_markdown(), "- Intro more **x** y");
    }

    #[test]
    fn safe_hyperlink_is_kept_and_normalized() {
        let j = json_of(&convert(&linked(json!("https://x.com"))));
        assert_eq!(j["texts"][0]["hyperlink"], "https://x.com/");
    }

    #[test]
    fn unsafe_or_non_string_hyperlink_is_dropped() {
        for href in [
            json!("javascript:alert(1)"),
            json!("hello"),
            json!(""),
            json!("http://[::1"),
            json!("//cdn.example.com/x"),
            json!(42),
            json!(true),
            json!(["http://x.com"]),
            json!({"href": "http://x.com"}),
        ] {
            let j = json_of(&convert(&linked(href.clone())));
            assert!(j["texts"][0].get("hyperlink").is_none(), "{href}");
        }
    }

    #[test]
    fn image_caption_is_parented_with_its_picture() {
        let doc = convert(&doc_of(json!([{"type": "bullet_list", "content": [{
            "type": "list_item",
            "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "Item with picture"}]},
                {"type": "image", "attrs": {"alt": "A chart of revenue"}},
            ],
        }]}])));
        let j = json_of(&doc);
        let pictures = j["pictures"].as_array().unwrap();
        assert_eq!(pictures.len(), 1);
        let pic = &pictures[0];
        assert_ne!(pic["parent"]["$ref"], "#/body");
        let caption = j["texts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["label"] == "caption")
            .unwrap();
        assert_eq!(caption["parent"]["$ref"], pic["self_ref"]);
        assert_eq!(pic["captions"][0]["$ref"], caption["self_ref"]);
    }

    fn table_cell(text: &str, rowspan: u64, colspan: u64) -> Value {
        json!({"type": "table_cell", "attrs": {"colspan": colspan, "rowspan": rowspan},
               "content": [{"type": "paragraph", "content": [{"type": "text", "text": text}]}]})
    }

    #[test]
    fn table_oversized_spans_clamped_to_table_size() {
        let doc = convert(&doc_of(json!([{"type": "table", "content": [
            {"type": "table_row", "content": [table_cell("A", 100_000_000, 1), table_cell("B", 1, 3_000_000)]},
            {"type": "table_row", "content": [table_cell("C", 1, 1)]},
        ]}])));
        let j = json_of(&doc);
        let data = &j["tables"][0]["data"];
        assert_eq!(
            (data["num_rows"].as_u64(), data["num_cols"].as_u64()),
            (Some(2), Some(2))
        );
        let cells: Vec<(String, u64, u64)> = data["table_cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["text"].as_str().unwrap().to_string(),
                    c["row_span"].as_u64().unwrap(),
                    c["col_span"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            cells,
            [("A".into(), 2, 1), ("B".into(), 1, 1), ("C".into(), 1, 1)]
        );
        let grid: Vec<Vec<&str>> = data["grid"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["text"].as_str().unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(grid, [["A", "B"], ["A", "C"]]);
    }

    #[test]
    fn empty_leading_paragraph_keeps_the_following_paragraph_in_order() {
        let doc = convert(&doc_of(json!([{"type": "bullet_list", "content": [{
            "type": "list_item",
            "content": [
                {"type": "paragraph", "content": []},
                {"type": "bullet_list", "content": [{"type": "list_item", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "Inner"}]}
                ]}]},
                {"type": "paragraph", "content": [{"type": "text", "text": "After"}]},
            ],
        }]}])));
        let j = json_of(&doc);
        let texts: Vec<&str> = j["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["text"].as_str())
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(texts, ["Inner", "After"]);
    }

    #[test]
    fn detects_box_notes_by_content() {
        assert!(looks_like_boxnote(
            r#"{"doc": {"type": "doc", "content": []}}"#
        ));
        assert!(!looks_like_boxnote(
            r#"{"schema_name": "DoclingDocument", "body": {}}"#
        ));
        assert!(!looks_like_boxnote(r#"{"doc": "x"}"#));
    }
}
