//! Document chunking for RAG pipelines — the Rust port of docling-core's
//! `docling_core.transforms.chunker`.
//!
//! Two chunkers, matching docling's semantics output-for-output:
//!
//! * [`HierarchicalChunker`] walks the document tree and yields one chunk per
//!   top-level item (paragraph, whole list, table, picture caption, …), each
//!   carrying the heading path it sits under. Tables are serialized in
//!   docling's *triplet* form (`row, column = value`), pictures contribute
//!   their captions.
//! * [`HybridChunker`] refines the hierarchical chunks with a tokenizer:
//!   oversized chunks are split (at item boundaries first, then within the
//!   text by docling's `semchunk` algorithm), and undersized neighbours that
//!   share the same headings are merged back together.
//!
//! [`contextualize`] renders a chunk to the string an embedding model should
//! see: the heading path plus the chunk text.
//!
//! Anything tokenizer-related is abstracted behind [`ChunkTokenizer`]; a
//! HuggingFace `tokenizers` implementation ships behind the `chunking` cargo
//! feature as [`HuggingFaceTokenizer`].

use std::collections::BTreeMap;

use crate::document::{DoclingDocument, Node, Table};

/// What kind of document item a [`ChunkItem`] points at — only what the
/// hybrid splitting logic needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkItemKind {
    /// A text-ish item (paragraph, list item, code, formula, caption, …).
    Text,
    /// A table item (single-item oversized chunks re-split per line, repeating
    /// docling's header handling).
    Table,
    /// A picture item.
    Picture,
}

/// One document item contributing to a chunk — the analogue of an entry in
/// docling's `DocMeta.doc_items`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkItem {
    /// The item's ref in [`DoclingDocument::export_to_json`] output
    /// (`#/texts/12`, `#/tables/0`, `#/pictures/1`, …).
    pub self_ref: String,
    pub kind: ChunkItemKind,
    /// The item serialized *standalone* (docling re-serializes individual
    /// items when splitting an oversized multi-item chunk — e.g. a nested
    /// list item flattens to `- text` with no indentation).
    pub text: String,
}

/// One chunk — the analogue of docling's `DocChunk` (text + `DocMeta`).
#[derive(Debug, Clone, PartialEq)]
pub struct DocChunk {
    /// The chunk body (markdown-flavoured, unescaped — same text docling puts
    /// in `DocChunk.text`).
    pub text: String,
    /// The heading path above this chunk, outermost first (`DocMeta.headings`;
    /// `None` when the chunk sits above any heading).
    pub headings: Option<Vec<String>>,
    /// The document items the chunk was built from (`DocMeta.doc_items`).
    pub doc_items: Vec<ChunkItem>,
}

/// Render a chunk for embedding: the heading path, then the text, joined with
/// newlines — docling's `BaseChunker.contextualize()`.
pub fn contextualize(chunk: &DocChunk) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(h) = &chunk.headings {
        parts.extend(h.iter().map(String::as_str));
    }
    parts.push(&chunk.text);
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Hierarchical chunker
// ---------------------------------------------------------------------------

/// Structure-driven chunker: one chunk per document item, lists and inline
/// groups kept whole, heading path tracked as metadata — docling-core's
/// `HierarchicalChunker` with default parameters.
#[derive(Debug, Clone, Default)]
pub struct HierarchicalChunker;

impl HierarchicalChunker {
    /// Chunk the document.
    pub fn chunk(&self, doc: &DoclingDocument) -> Vec<DocChunk> {
        let mut chunks = Vec::new();
        self.chunk_with(doc, &mut |c| {
            chunks.push(c);
            true
        });
        chunks
    }

    /// Stream the chunks: `sink` is called with each chunk as the document
    /// walk produces it, so a consumer can process (embed, forward) chunks
    /// without materializing the whole `Vec` first. A `false` return from
    /// `sink` cancels the walk. [`Self::chunk`] is this with a collecting
    /// sink — the chunks and their order are identical.
    pub fn chunk_with(&self, doc: &DoclingDocument, sink: &mut dyn FnMut(DocChunk) -> bool) {
        let mut w = Walker {
            alloc: Alloc::default(),
            headings: BTreeMap::new(),
            stopped: false,
            sink,
        };
        w.walk(&doc.nodes);
    }
}

/// Ref allocator mirroring the numbering `json.rs` gives every item, so
/// `ChunkItem::self_ref` matches the document's JSON export.
#[derive(Debug, Default)]
struct Alloc {
    texts: usize,
    groups: usize,
    tables: usize,
    pictures: usize,
    field_regions: usize,
    field_items: usize,
}

impl Alloc {
    fn text(&mut self) -> String {
        let r = format!("#/texts/{}", self.texts);
        self.texts += 1;
        r
    }
    fn group(&mut self) -> String {
        let r = format!("#/groups/{}", self.groups);
        self.groups += 1;
        r
    }
    fn table(&mut self) -> String {
        let r = format!("#/tables/{}", self.tables);
        self.tables += 1;
        r
    }
    fn picture(&mut self) -> String {
        let r = format!("#/pictures/{}", self.pictures);
        self.pictures += 1;
        r
    }
    fn field_region(&mut self) -> String {
        let r = format!("#/field_regions/{}", self.field_regions);
        self.field_regions += 1;
        r
    }
    fn field_item(&mut self) -> String {
        let r = format!("#/field_items/{}", self.field_items);
        self.field_items += 1;
        r
    }
}

struct Walker<'s> {
    alloc: Alloc,
    /// Active heading per docling level (title = 0, `section_header` = its
    /// `level`), pruned like docling's `heading_by_level`.
    headings: BTreeMap<u8, String>,
    /// Set once the sink refuses a chunk; the walk unwinds without emitting.
    stopped: bool,
    sink: &'s mut dyn FnMut(DocChunk) -> bool,
}

impl Walker<'_> {
    fn emit(&mut self, text: String, doc_items: Vec<ChunkItem>) {
        if self.stopped || text.is_empty() {
            return;
        }
        let headings: Vec<String> = self.headings.values().cloned().collect();
        self.stopped = !(self.sink)(DocChunk {
            text,
            headings: (!headings.is_empty()).then_some(headings),
            doc_items,
        });
    }

    /// Emit a text chunk whose doc items follow docling's inline granularity:
    /// mixed inline content (a paragraph docling represents as an inline group)
    /// contributes one item per span, plain text one item.
    fn emit_inline(&mut self, md_text: &str, self_ref: String) {
        self.emit_inline_with_runs(md_text, self_ref, &[]);
    }

    fn emit_inline_with_runs(
        &mut self,
        md_text: &str,
        self_ref: String,
        runs: &[crate::InlineRun],
    ) {
        let body = unescape_text(md_text);
        if body.is_empty() {
            return;
        }
        let segments: Vec<String> = inline_segments_tagged(md_text)
            .into_iter()
            .flat_map(|(text, is_plain)| {
                if is_plain {
                    if let Some(split) = split_plain_by_runs(&text, runs) {
                        return split;
                    }
                }
                vec![text]
            })
            .collect();
        let items: Vec<ChunkItem> = if segments.len() <= 1 {
            vec![ChunkItem {
                self_ref,
                kind: ChunkItemKind::Text,
                text: body.clone(),
            }]
        } else {
            segments
                .into_iter()
                .map(|text| ChunkItem {
                    self_ref: self_ref.clone(),
                    kind: ChunkItemKind::Text,
                    text,
                })
                .collect()
        };
        self.emit(body, items);
    }

    fn set_heading(&mut self, doc_level: u8, text: String) {
        self.headings.retain(|k, _| *k < doc_level);
        self.headings.insert(doc_level, text);
    }

    fn walk(&mut self, nodes: &[Node]) {
        let mut i = 0;
        while i < nodes.len() {
            if self.stopped {
                return;
            }
            if matches!(nodes[i], Node::ListItem { .. }) {
                let start = i;
                i += 1;
                loop {
                    match nodes.get(i) {
                        Some(Node::ListItem { .. }) => i += 1,
                        // An empty paragraph between two list items is absorbed
                        // into the run (mirrors json.rs / markdown.rs).
                        Some(Node::Paragraph { text })
                            if text.is_empty()
                                && matches!(nodes.get(i + 1), Some(Node::ListItem { .. })) =>
                        {
                            i += 1
                        }
                        _ => break,
                    }
                }
                self.sibling_lists(&nodes[start..i]);
            } else {
                self.one(&nodes[i]);
                i += 1;
            }
        }
    }

    /// Split a run of list items into sibling lists exactly like
    /// `json.rs::add_sibling_lists`, chunking each list separately (docling
    /// yields one chunk per `ListGroup`).
    fn sibling_lists(&mut self, run: &[Node]) {
        let base = level_of(&run[0]);
        let mut seg = 0;
        let mut prev: Option<(bool, u64)> = None;
        for k in 0..run.len() {
            let Node::ListItem {
                ordered,
                number,
                first_in_list,
                level,
                ..
            } = &run[k]
            else {
                continue;
            };
            if *level != base {
                continue; // nested item — handled inside `list`
            }
            if k > seg {
                if let Some((po, pn)) = prev {
                    if *first_in_list || po != *ordered || (*ordered && *number != pn + 1) {
                        self.list(&run[seg..k]);
                        seg = k;
                    }
                }
            }
            prev = Some((*ordered, *number));
        }
        self.list(&run[seg..]);
    }

    /// One `ListGroup`: allocate refs in `json.rs::add_list` order (group,
    /// then per top item its text ref followed by any nested groups) and emit
    /// a single chunk whose text is the indented markdown list.
    fn list(&mut self, items: &[Node]) {
        self.alloc.group();
        let mut chunk_items = Vec::new();
        self.list_refs(items, &mut chunk_items);
        let text = render_list(items);
        self.emit(text, chunk_items);
    }

    /// Allocate refs for one list's items (and nested sibling lists), mirroring
    /// `json.rs::add_list` / `add_sibling_lists` recursion, collecting the
    /// non-furniture items in allocation (= document) order.
    fn list_refs(&mut self, items: &[Node], out: &mut Vec<ChunkItem>) {
        let base = level_of(&items[0]);
        let mut i = 0;
        while i < items.len() {
            let Node::ListItem {
                ordered,
                number,
                text,
                level,
                layer,
                ..
            } = &items[i]
            else {
                i += 1;
                continue;
            };
            if *level > base {
                i += 1;
                continue;
            }
            let item_ref = self.alloc.text();
            let mut j = i + 1;
            while j < items.len() && level_of(&items[j]) > base {
                j += 1;
            }
            let has_nested = j > i + 1;
            if layer.is_none() {
                let marker = if *ordered {
                    format!("{number}.")
                } else {
                    "-".to_string()
                };
                // An item that carries both inline spans and a nested list is an
                // empty list item wrapping an inline group in docling's model:
                // its marker and each inline span are separate doc items.
                let segments = inline_segments(text);
                if has_nested && segments.len() > 1 && text.contains("](") {
                    out.push(ChunkItem {
                        self_ref: item_ref.clone(),
                        kind: ChunkItemKind::Text,
                        text: format!("{marker} "),
                    });
                    for seg in segments {
                        out.push(ChunkItem {
                            self_ref: item_ref.clone(),
                            kind: ChunkItemKind::Text,
                            text: seg,
                        });
                    }
                } else {
                    out.push(ChunkItem {
                        self_ref: item_ref.clone(),
                        kind: ChunkItemKind::Text,
                        text: format!("{marker} {}", unescape_text(text)),
                    });
                }
            }
            // nested items group under this one; each nested sibling list is a
            // fresh group ref
            if j > i + 1 {
                self.nested_sibling_lists(&items[i + 1..j], out);
            }
            i = j;
        }
    }

    fn nested_sibling_lists(&mut self, run: &[Node], out: &mut Vec<ChunkItem>) {
        let base = level_of(&run[0]);
        let mut seg = 0;
        let mut prev: Option<(bool, u64)> = None;
        for k in 0..run.len() {
            let Node::ListItem {
                ordered,
                number,
                first_in_list,
                level,
                ..
            } = &run[k]
            else {
                continue;
            };
            if *level != base {
                continue;
            }
            if k > seg {
                if let Some((po, pn)) = prev {
                    if *first_in_list || po != *ordered || (*ordered && *number != pn + 1) {
                        self.alloc.group();
                        self.list_refs(&run[seg..k], out);
                        seg = k;
                    }
                }
            }
            prev = Some((*ordered, *number));
        }
        self.alloc.group();
        self.list_refs(&run[seg..], out);
    }

    fn one(&mut self, node: &Node) {
        match node {
            Node::Heading { level, text } => {
                let doc_level = if *level == 1 {
                    0
                } else {
                    level.saturating_sub(1)
                };
                let self_ref = self.alloc.text();
                // docling stores heading text unformatted: a heading that is one
                // uniformly formatted span keeps its plain text; a *partially*
                // formatted heading becomes an empty heading whose content is an
                // inline group — which the chunker then yields as a chunk of its
                // own (under the freshly-set empty heading).
                let runs = crate::inline_runs_from_markdown(text);
                if runs.len() <= 1 {
                    let plain = runs
                        .first()
                        .map(|r| r.text.clone())
                        .unwrap_or_else(|| text.clone());
                    self.set_heading(doc_level, unescape_text(&plain));
                } else {
                    self.set_heading(doc_level, String::new());
                    let body = unescape_text(text);
                    self.emit(
                        body.clone(),
                        vec![ChunkItem {
                            self_ref,
                            kind: ChunkItemKind::Text,
                            text: body,
                        }],
                    );
                }
            }
            Node::Paragraph { text } => {
                let t = text.trim();
                let self_ref = self.alloc.text();
                // A whole-paragraph display equation is a formula item; docling's
                // chunk serializer re-wraps the raw latex in `$$…$$`.
                if let Some(inner) = t
                    .strip_prefix("$$")
                    .and_then(|s| s.strip_suffix("$$"))
                    .filter(|s| !s.is_empty())
                {
                    let body = format!("$${inner}$$");
                    self.emit(
                        body.clone(),
                        vec![ChunkItem {
                            self_ref,
                            kind: ChunkItemKind::Text,
                            text: body,
                        }],
                    );
                    return;
                }
                self.emit_inline(text, self_ref);
            }
            Node::CheckboxItem { checked, text } => {
                let self_ref = self.alloc.text();
                let mark = if *checked { "- [x] " } else { "- [ ] " };
                let body = format!("{mark}{}", unescape_text(text));
                self.emit(
                    body.clone(),
                    vec![ChunkItem {
                        self_ref,
                        kind: ChunkItemKind::Text,
                        text: body,
                    }],
                );
            }
            Node::Code { text, .. } => {
                let self_ref = self.alloc.text();
                let body = format!("```\n{}\n```", unescape_text(text));
                self.emit(
                    body.clone(),
                    vec![ChunkItem {
                        self_ref,
                        kind: ChunkItemKind::Text,
                        text: body,
                    }],
                );
            }
            Node::Table(t) => {
                let self_ref = self.alloc.table();
                let body = triplet_table_text(t);
                self.emit(
                    body.clone(),
                    vec![ChunkItem {
                        self_ref,
                        kind: ChunkItemKind::Table,
                        text: body,
                    }],
                );
            }
            Node::Picture { caption, .. } => {
                let cap = caption.as_deref().filter(|c| !c.is_empty());
                let cap_item = cap.map(|c| ChunkItem {
                    self_ref: self.alloc.text(),
                    kind: ChunkItemKind::Text,
                    text: unescape_text(c),
                });
                self.alloc.picture();
                // The picture itself serializes to the (empty) chunking image
                // placeholder, and its caption is already consumed by the
                // caption chunk — so only the caption text is emitted.
                if let Some(cap_item) = cap_item {
                    let body = cap_item.text.clone();
                    self.emit(body, vec![cap_item]);
                }
            }
            Node::Chart {
                kind,
                table,
                caption,
                ..
            } => {
                let cap = caption.as_deref().filter(|c| !c.is_empty());
                let cap_item = cap.map(|c| ChunkItem {
                    self_ref: self.alloc.text(),
                    kind: ChunkItemKind::Text,
                    text: unescape_text(c),
                });
                let pic_ref = self.alloc.picture();
                // caption, humanized classification, then the chart's data grid
                // as a (padded) markdown table — docling's picture serializer
                // parts, joined with blank lines.
                let mut parts: Vec<String> = Vec::new();
                if let Some(ci) = &cap_item {
                    parts.push(ci.text.clone());
                }
                parts.push(humanize_label(kind));
                let grid = crate::markdown::render_table(table, false);
                if !grid.is_empty() {
                    parts.push(unescape_text(&grid));
                }
                let body = parts.join("\n\n");
                // Re-serialized standalone (the hybrid window join), the picture
                // carries its caption itself, while the caption *item* renders
                // empty — docling's markdown serializer emits caption-label text
                // only through the picture.
                let pic_item = ChunkItem {
                    self_ref: pic_ref,
                    kind: ChunkItemKind::Picture,
                    text: body.clone(),
                };
                let items = match cap_item {
                    Some(mut ci) => {
                        ci.text = String::new();
                        vec![ci, pic_item]
                    }
                    None => vec![pic_item],
                };
                self.emit(body, items);
            }
            Node::Group { children, .. } => {
                // A generic group is a structural container: docling recurses
                // into it rather than chunking it whole.
                self.alloc.group();
                self.walk(children);
            }
            Node::FieldRegion { items } => {
                // Each field part (marker / key / value) is its own text item,
                // and docling chunks each one individually.
                self.alloc.field_region();
                for item in items {
                    self.alloc.field_item();
                    for part in [&item.marker, &item.key, &item.value].into_iter().flatten() {
                        let self_ref = self.alloc.text();
                        let body = unescape_text(part);
                        self.emit(
                            body.clone(),
                            vec![ChunkItem {
                                self_ref,
                                kind: ChunkItemKind::Text,
                                text: body,
                            }],
                        );
                    }
                }
            }
            Node::InlineGroup { md_text, runs, .. } => {
                let self_ref = self.alloc.text();
                self.emit_inline_with_runs(md_text, self_ref, runs);
            }
            Node::TextDump(text) => {
                let self_ref = self.alloc.text();
                let body = unescape_text(text);
                self.emit(
                    body.clone(),
                    vec![ChunkItem {
                        self_ref,
                        kind: ChunkItemKind::Text,
                        text: body,
                    }],
                );
            }
            // Layout provenance is transparent.
            Node::Located { inner, .. } => self.one(inner),
            // Non-body layers and doclang-only nodes don't reach the chunker
            // (nor the JSON body).
            Node::Furniture { .. }
            | Node::PageFurniture { .. }
            | Node::PageBreak
            | Node::DoclangOnly(_) => {}
            Node::ListItem { .. } => unreachable!("list items are chunked in runs"),
        }
    }
}

fn level_of(node: &Node) -> u8 {
    match node {
        Node::ListItem { level, .. } => *level,
        _ => 0,
    }
}

/// Render one sibling list as its markdown chunk text (indented items, same
/// rules as the full markdown serializer's list rendering).
fn render_list(items: &[Node]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for item in items {
        let Node::ListItem {
            ordered,
            number,
            text,
            level,
            layer,
            ..
        } = item
        else {
            continue;
        };
        if layer.is_some() {
            continue;
        }
        let indent = "    ".repeat(*level as usize);
        let marker = if *ordered {
            format!("{number}.")
        } else {
            "-".to_string()
        };
        lines.push(format!("{indent}{marker} {}", unescape_text(text)));
    }
    lines.join("\n")
}

/// docling-core's `_humanize_text`: underscores to spaces, first letter
/// capitalized (`line_chart` → `Line chart`).
fn humanize_label(label: &str) -> String {
    let text = label.replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => text,
    }
}

/// docling's `TripletTableSerializer` over `export_to_dataframe` semantics: the
/// leading rows carrying column-header cells become the dataframe's column
/// names (multiple header rows join per column with `.`; no header rows at all
/// yield pandas' integer column names), the rest are data rows; the dataframe
/// is then rendered as `row, column = value` sentences (with the header-only /
/// single-column special cases and the plain-text flatten fallback).
fn triplet_table_text(t: &Table) -> String {
    let rows: Vec<Vec<String>> = t
        .rows
        .iter()
        .enumerate()
        .map(|(ri, r)| (0..r.len()).map(|ci| cell_chunk_text(t, ri, ci)).collect())
        .collect();
    let num_rows = rows.len();
    let num_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if num_rows == 0 || num_cols == 0 {
        return String::new();
    }
    let cell = |r: usize, c: usize| -> &str {
        rows.get(r)
            .and_then(|row| row.get(c))
            .map(String::as_str)
            .unwrap_or("")
    };

    // Whether a cell is a column-header cell, resolving span continuations to
    // their origin (docling's grid replicates the spanning cell, so a header
    // spilling into the next row makes that row a header row too).
    let cell_is_header = |r: usize, c: usize| -> bool {
        let (mut r, mut c) = (r, c);
        loop {
            match &t.structure {
                Some(s) if !s.col_header.is_empty() => {
                    return s
                        .col_header
                        .get(r)
                        .and_then(|row| row.get(c))
                        .copied()
                        .unwrap_or(false)
                }
                Some(s) => {
                    let cont = |g: &Vec<Vec<bool>>| {
                        g.get(r)
                            .and_then(|row| row.get(c))
                            .copied()
                            .unwrap_or(false)
                    };
                    if r > 0 && cont(&s.row_continuation) {
                        r -= 1;
                        continue;
                    }
                    if c > 0 && cont(&s.col_continuation) {
                        c -= 1;
                        continue;
                    }
                    return if s.header_row.is_empty() {
                        r == 0
                    } else {
                        s.header_row.get(r).copied().unwrap_or(false)
                    };
                }
                None => return r == 0,
            }
        }
    };
    let row_is_header = |r: usize| (0..num_cols).any(|c| cell_is_header(r, c));
    let num_headers = (0..num_rows).take_while(|r| row_is_header(*r)).count();

    // Column names: header-row texts joined per column with '.', or the integer
    // positions when there are no header rows.
    let columns: Vec<String> = if num_headers > 0 {
        (0..num_cols)
            .map(|c| {
                let mut name = String::new();
                for r in 0..num_headers {
                    if !name.is_empty() {
                        name.push('.');
                    }
                    name.push_str(cell(r, c));
                }
                name
            })
            .collect()
    } else {
        (0..num_cols).map(|c| c.to_string()).collect()
    };
    let data_rows = num_headers..num_rows;
    let n_data = data_rows.len();

    // Header-only table: emit the header texts directly.
    if n_data == 0 {
        return columns
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(". ");
    }

    let data = |r: usize, c: usize| -> &str { cell(num_headers + r, c) };
    let text = if num_cols == 1 {
        // Single-column: the first data row is the column name, the rest are
        // values (a single data row emits its cell text alone).
        let col_name = data(0, 0).trim().to_string();
        if n_data == 1 {
            col_name
        } else {
            (1..n_data)
                .map(|r| format!("{col_name} = {}", data(r, 0).trim()))
                .collect::<Vec<_>>()
                .join(". ")
        }
    } else {
        // Triplets over the dataframe with the column names copied as row 0.
        let mut parts = Vec::new();
        for r in 0..n_data {
            for (c, col_name) in columns.iter().enumerate().skip(1) {
                parts.push(format!(
                    "{}, {} = {}",
                    data(r, 0).trim(),
                    col_name.trim(),
                    data(r, c).trim()
                ));
            }
        }
        parts.join(". ")
    };
    if !text.is_empty() {
        return text;
    }

    // Last-resort flatten: the data rows' non-blank cells joined with '. '
    // (the header rows are the dataframe's columns, so they are not included).
    (0..n_data)
        .flat_map(|r| (0..num_cols).map(move |c| (r, c)))
        .map(|(r, c)| data(r, c).trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(". ")
}

/// Split a markdown-flavoured text into docling's inline-item granularity: a
/// hyperlink / formatted span / inline formula is its own document item in
/// docling's model, with plain text runs between them. Returns the standalone
/// serialization of each item (`[text](url)`, `**bold**`, a formula re-wrapped
/// as `$$latex$$`, plain text), or a single-element vector when the text is one
/// uniform item. The single space docling's serializer inserts between inline
/// items is stripped from the adjacent plain runs.
fn inline_segments(md: &str) -> Vec<String> {
    inline_segments_tagged(md)
        .into_iter()
        .map(|(t, _)| t)
        .collect()
}

/// Like [`inline_segments`], with each segment tagged `true` when it came from
/// plain (unmarked) text — those may still need splitting at run boundaries
/// invisible in markdown (underline, soft breaks).
fn inline_segments_tagged(md: &str) -> Vec<(String, bool)> {
    let chars: Vec<char> = md.chars().collect();
    let n = chars.len();
    let find = |from: usize, pat: &str| -> Option<usize> {
        let hay: String = chars[from..].iter().collect();
        hay.find(pat).map(|p| from + hay[..p].chars().count())
    };
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut plain = String::new();
    let mut after_span = false;

    fn flush(
        out: &mut Vec<(String, bool)>,
        plain: &mut String,
        before_span: bool,
        after_span: bool,
    ) {
        let mut p = std::mem::take(plain);
        if after_span {
            if let Some(rest) = p.strip_prefix(' ') {
                p = rest.to_string();
            }
        }
        if before_span {
            if let Some(rest) = p.strip_suffix(' ') {
                p = rest.to_string();
            }
        }
        if !p.is_empty() {
            out.push((unescape_text(&p), true));
        }
    }

    let mut i = 0;
    while i < n {
        let rest: String = chars[i..].iter().collect();
        // A hyperlink span (not an image): the whole `[text](url)` is one item.
        if chars[i] == '[' && !rest.starts_with("[](") {
            if let Some(close) = find(i + 1, "](") {
                if let Some(endp) = find(close + 2, ")") {
                    flush(&mut out, &mut plain, true, after_span);
                    out.push((
                        unescape_text(&chars[i..=endp].iter().collect::<String>()),
                        false,
                    ));
                    i = endp + 1;
                    after_span = true;
                    continue;
                }
            }
        }
        // A formatted span; longest markers first. An inline code span is a
        // code item in docling's model, whose standalone form is a fenced block.
        let mut matched = false;
        for marker in ["***", "**", "*", "~~", "`"] {
            if rest.starts_with(marker) {
                let mlen = marker.chars().count();
                if let Some(end) = find(i + mlen, marker) {
                    if end > i + mlen {
                        flush(&mut out, &mut plain, true, after_span);
                        if marker == "`" {
                            let inner: String = chars[i + 1..end].iter().collect();
                            out.push((format!("```\n{}\n```", unescape_text(&inner)), false));
                        } else {
                            out.push((
                                unescape_text(&chars[i..end + mlen].iter().collect::<String>()),
                                false,
                            ));
                        }
                        i = end + mlen;
                        after_span = true;
                        matched = true;
                    }
                }
                break;
            }
        }
        if matched {
            continue;
        }
        // A literal `$$` inside running text is not an inline formula: copy it
        // through as plain characters.
        if rest.starts_with("$$") {
            plain.push_str("$$");
            i += 2;
            continue;
        }
        // An inline formula: standalone it re-serializes in display form.
        if chars[i] == '$' {
            if let Some(end) = find(i + 1, "$") {
                if end > i + 1 {
                    flush(&mut out, &mut plain, true, after_span);
                    let latex: String = chars[i + 1..end].iter().collect();
                    out.push((format!("$${latex}$$"), false));
                    i = end + 1;
                    after_span = true;
                    continue;
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush(&mut out, &mut plain, false, after_span);
    if out.is_empty() {
        out.push((unescape_text(md), true));
    }
    out
}

/// Split a plain markdown segment at run boundaries the markdown cannot show
/// (an underlined run, a `<sub>`/`<sup>` run): when a consecutive window of
/// two or more unmarked runs exactly covers the segment, each run is its own
/// document item.
fn split_plain_by_runs(segment: &str, runs: &[crate::InlineRun]) -> Option<Vec<String>> {
    let target = segment.trim();
    if target.is_empty() {
        return None;
    }
    let unmarked: Vec<&str> = runs
        .iter()
        .filter(|r| !r.bold && !r.italic && !r.strike && !r.code && !r.formula)
        .map(|r| r.text.as_str())
        .collect();
    for start in 0..unmarked.len() {
        let mut rest = target;
        let mut taken: Vec<String> = Vec::new();
        for t in &unmarked[start..] {
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            match rest.strip_prefix(t) {
                Some(r) => {
                    taken.push(unescape_text(t));
                    rest = r.trim_start();
                    if rest.is_empty() {
                        break;
                    }
                }
                None => break,
            }
        }
        if rest.is_empty() && taken.len() >= 2 {
            return Some(taken);
        }
    }
    None
}

/// A table cell's text for the triplet serializer. A *rich* cell (one carrying
/// block content) is re-serialized the way docling's chunking serializer sees
/// it: paragraphs joined with blank lines, a nested table as its own triplet
/// sentences, pictures as an empty placeholder. Plain cells use the flat text
/// with the markdown image placeholder stripped (the chunking serializer's
/// `image_placeholder` is empty).
fn cell_chunk_text(t: &Table, r: usize, c: usize) -> String {
    if let Some(blocks) = t
        .cell_blocks
        .as_ref()
        .and_then(|b| b.get(r))
        .and_then(|row| row.get(c))
        .filter(|b| !b.is_empty())
    {
        let mut parts: Vec<String> = Vec::new();
        for node in blocks.iter() {
            let part = block_chunk_text(node);
            if !part.is_empty() {
                parts.push(part);
            }
        }
        return parts.join("\n\n");
    }
    let flat = t
        .rows
        .get(r)
        .and_then(|row| row.get(c))
        .map(String::as_str)
        .unwrap_or("");
    unescape_text(flat)
        .replace("<!-- image -->", "")
        .trim()
        .to_string()
}

/// One block of a rich cell, serialized for chunking.
fn block_chunk_text(node: &Node) -> String {
    match node {
        Node::Paragraph { text } => unescape_text(text),
        Node::InlineGroup { md_text, .. } => unescape_text(md_text),
        Node::Code { text, .. } => format!("```\n{}\n```", unescape_text(text)),
        Node::Table(inner) => triplet_table_text(inner),
        Node::Picture { caption, .. } => caption
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(unescape_text)
            .unwrap_or_default(),
        Node::ListItem {
            ordered,
            number,
            text,
            ..
        } => {
            let marker = if *ordered {
                format!("{number}.")
            } else {
                "-".to_string()
            };
            format!("{marker} {}", unescape_text(text))
        }
        Node::CheckboxItem { checked, text } => {
            let mark = if *checked { "- [x] " } else { "- [ ] " };
            format!("{mark}{}", unescape_text(text))
        }
        Node::Heading { text, .. } => unescape_text(text),
        Node::Located { inner, .. } => block_chunk_text(inner),
        Node::Group { children, .. } => children
            .iter()
            .map(block_chunk_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Reverse the model's baked markdown text escaping — same mapping as the JSON
/// exporter (docling chunks carry raw text).
fn unescape_text(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("\\_", "_")
}

// ---------------------------------------------------------------------------
// Hybrid chunker
// ---------------------------------------------------------------------------

/// Token counting for [`HybridChunker`] — docling's `BaseTokenizer`.
pub trait ChunkTokenizer {
    /// Number of tokens in `text` (no special tokens).
    fn count_tokens(&self, text: &str) -> usize;
    /// The chunk budget (docling's `max_tokens`, e.g. 256 for MiniLM).
    fn max_tokens(&self) -> usize;
}

/// Tokenization-aware chunker on top of [`HierarchicalChunker`] — docling's
/// `HybridChunker` with default parameters (`merge_peers`,
/// `repeat_table_header` on; `omit_header_on_overflow` off).
pub struct HybridChunker<T: ChunkTokenizer> {
    tokenizer: T,
    merge_peers: bool,
}

impl<T: ChunkTokenizer> HybridChunker<T> {
    pub fn new(tokenizer: T) -> Self {
        Self {
            tokenizer,
            merge_peers: true,
        }
    }

    /// Disable merging of undersized same-heading neighbours.
    pub fn with_merge_peers(mut self, merge_peers: bool) -> Self {
        self.merge_peers = merge_peers;
        self
    }

    pub fn max_tokens(&self) -> usize {
        self.tokenizer.max_tokens()
    }

    /// Chunk the document.
    pub fn chunk(&self, doc: &DoclingDocument) -> Vec<DocChunk> {
        let mut chunks = Vec::new();
        self.chunk_with(doc, &mut |c| {
            chunks.push(c);
            true
        });
        chunks
    }

    /// Stream the chunks: each hierarchical chunk is split against the token
    /// budget as the document walk produces it, and the peer merge flushes a
    /// merged chunk to `sink` as soon as its window closes (a chunk with
    /// different headings arrives, or the budget fills). A `false` return from
    /// `sink` cancels the chunking. [`Self::chunk`] is this with a collecting
    /// sink — the chunks and their order are identical.
    pub fn chunk_with(&self, doc: &DoclingDocument, sink: &mut dyn FnMut(DocChunk) -> bool) {
        let mut merger = PeerMerger::default();
        let mut alive = true;
        HierarchicalChunker.chunk_with(doc, &mut |c| {
            for split in self.split_by_doc_items(c) {
                for chunk in self.split_using_plain_text(split) {
                    if !alive {
                        return false;
                    }
                    alive = if self.merge_peers {
                        self.merge_push(&mut merger, chunk, sink)
                    } else {
                        sink(chunk)
                    };
                }
            }
            alive
        });
        if alive {
            self.merge_flush(&mut merger, sink);
        }
    }

    fn count_chunk_tokens(&self, chunk: &DocChunk) -> usize {
        self.tokenizer.count_tokens(&contextualize(chunk))
    }

    /// docling's `_make_chunk_from_doc_items`: single-item chunks keep their
    /// text; multi-item windows re-join the items' standalone serializations.
    fn window_chunk(&self, chunk: &DocChunk, start: usize, end: usize) -> DocChunk {
        let doc_items: Vec<ChunkItem> = chunk.doc_items[start..=end].to_vec();
        let text = if chunk.doc_items.len() == 1 {
            chunk.text.clone()
        } else {
            doc_items
                .iter()
                .filter(|it| !it.text.is_empty())
                .map(|it| it.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        DocChunk {
            text,
            headings: chunk.headings.clone(),
            doc_items,
        }
    }

    fn split_by_doc_items(&self, chunk: DocChunk) -> Vec<DocChunk> {
        if chunk.doc_items.is_empty() {
            return vec![chunk];
        }
        let max = self.max_tokens();
        let num_items = chunk.doc_items.len();
        let mut chunks = Vec::new();
        let mut window_start = 0usize;
        let mut window_end = 0usize; // inclusive
        while window_end < num_items {
            let mut new_chunk = self.window_chunk(&chunk, window_start, window_end);
            if self.count_chunk_tokens(&new_chunk) <= max {
                if window_end < num_items - 1 {
                    window_end += 1;
                    continue;
                } else {
                    window_end = num_items; // last loop
                }
            } else if window_start == window_end {
                // One item that doesn't fit: keep it; the plain-text splitter
                // takes over.
                window_end += 1;
                window_start = window_end;
            } else {
                // The window without its last item fit; flush that and start a
                // new window at the current item.
                new_chunk = self.window_chunk(&chunk, window_start, window_end - 1);
                window_start = window_end;
            }
            chunks.push(new_chunk);
        }
        chunks
    }

    fn split_using_plain_text(&self, chunk: DocChunk) -> Vec<DocChunk> {
        let total = self.count_chunk_tokens(&chunk);
        let max = self.max_tokens();
        if total <= max {
            return vec![chunk];
        }
        let text_len = self.tokenizer.count_tokens(&chunk.text);
        let other_len = total - text_len;
        if other_len >= max {
            // Headings alone exceed the budget: drop them and retry.
            let stripped = DocChunk {
                headings: None,
                ..chunk
            };
            return self.split_using_plain_text(stripped);
        }
        let available = max - other_len;

        let segments =
            if chunk.doc_items.len() == 1 && chunk.doc_items[0].kind == ChunkItemKind::Table {
                // Table: split line-based, repeating headers. The triplet
                // serializer has no header lines, so this is a line-preserving
                // split of the table text. (docling constructs the line chunker
                // with the *tokenizer's* max_tokens — the `max_tokens=available`
                // argument is silently dropped by pydantic — so the line budget is
                // the full window, not `available`.)
                let lines: Vec<String> = chunk
                    .text
                    .split('\n')
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| l.to_string())
                    .collect();
                line_chunk_text(&lines, &self.tokenizer, max)
            } else {
                semchunk(&chunk.text, available, &self.tokenizer)
            };
        segments
            .into_iter()
            .map(|s| DocChunk {
                text: s,
                headings: chunk.headings.clone(),
                doc_items: chunk.doc_items.clone(),
            })
            .collect()
    }

    /// One step of docling's `_merge_chunks_with_matching_metadata`, streamed:
    /// extend the window with `chunk` when its headings match the window's and
    /// the merged candidate stays within budget, otherwise flush the window to
    /// `sink` and start a new one at `chunk`. Returns `false` once the sink
    /// cancels.
    fn merge_push(
        &self,
        m: &mut PeerMerger,
        chunk: DocChunk,
        sink: &mut dyn FnMut(DocChunk) -> bool,
    ) -> bool {
        if m.window.is_empty() {
            m.window.push(chunk);
            return true;
        }
        let candidate = DocChunk {
            text: m
                .window
                .iter()
                .map(|c| c.text.as_str())
                .chain([chunk.text.as_str()])
                .collect::<Vec<_>>()
                .join("\n"),
            headings: m.window[0].headings.clone(),
            doc_items: m
                .window
                .iter()
                .flat_map(|c| c.doc_items.iter().cloned())
                .chain(chunk.doc_items.iter().cloned())
                .collect(),
        };
        if chunk.headings == m.window[0].headings
            && self.count_chunk_tokens(&candidate) <= self.max_tokens()
        {
            m.window.push(chunk);
            m.merged = Some(candidate);
            true
        } else {
            let alive = self.merge_flush(m, sink);
            m.window.push(chunk);
            alive
        }
    }

    /// Flush the merge window: a single chunk passes through unchanged, a
    /// multi-chunk window emits its precomputed merge. Returns `false` once
    /// the sink cancels.
    fn merge_flush(&self, m: &mut PeerMerger, sink: &mut dyn FnMut(DocChunk) -> bool) -> bool {
        let alive = if m.window.len() == 1 {
            sink(m.window.pop().expect("single-chunk window"))
        } else if !m.window.is_empty() {
            m.window.clear();
            sink(m.merged.take().expect("multi-chunk window has a merge"))
        } else {
            true
        };
        m.merged = None;
        alive
    }
}

/// The in-flight peer-merge window of [`HybridChunker::chunk_with`].
#[derive(Default)]
struct PeerMerger {
    window: Vec<DocChunk>,
    merged: Option<DocChunk>,
}

// ---------------------------------------------------------------------------
// Line-based token chunking (docling's LineBasedTokenChunker, empty prefix)
// ---------------------------------------------------------------------------

/// Pack lines into chunks of at most `max_tokens`, splitting a line only when
/// it exceeds the budget on its own — docling's `LineBasedTokenChunker
/// .chunk_text` with an empty prefix (which is what the triplet table
/// serializer yields). Reproduces its exact output, including the `\n` it
/// prepends to a carried-over segment of an oversized line.
fn line_chunk_text<T: ChunkTokenizer>(lines: &[String], tok: &T, max_tokens: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for line in lines {
        let mut remaining: Vec<char> = line.chars().collect();
        loop {
            let rem_str: String = remaining.iter().collect();
            let line_tokens = tok.count_tokens(&rem_str);
            let available = max_tokens.saturating_sub(current_len);

            if line_tokens <= available {
                current.push_str(&rem_str);
                current_len += line_tokens;
                break;
            }
            if line_tokens <= max_tokens {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
                continue;
            }
            // Too large even for an empty chunk: split off what fits.
            let (mut take, rest) = split_by_token_limit(&remaining, available, tok);
            let mut rest = rest;
            if take.is_empty() {
                if rest.is_empty() {
                    break;
                }
                take = rest[..1].iter().collect();
                rest = rest[1..].to_vec();
            }
            current.push('\n');
            current.push_str(&take);
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
            remaining = rest;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Binary-search the longest char-prefix of `text` within `token_limit`
/// tokens, preferring to break at the last ASCII space — docling's
/// `split_by_token_limit`.
fn split_by_token_limit<T: ChunkTokenizer>(
    text: &[char],
    token_limit: usize,
    tok: &T,
) -> (String, Vec<char>) {
    if token_limit == 0 || text.is_empty() {
        return (String::new(), text.to_vec());
    }
    let full: String = text.iter().collect();
    if tok.count_tokens(&full) <= token_limit {
        return (full, Vec::new());
    }
    let (mut lo, mut hi) = (0usize, text.len());
    let mut best: Option<usize> = None;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let head: String = text[..mid].iter().collect();
        if tok.count_tokens(&head) <= token_limit {
            best = Some(mid);
            lo = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        }
    }
    let mut best_idx = match best {
        Some(b) if b > 0 => b,
        _ => return (String::new(), text.to_vec()),
    };
    // Snap back to the last space, if that leaves a non-empty head.
    if let Some(pos) = text[..best_idx].iter().rposition(|c| *c == ' ') {
        if pos > 0 {
            best_idx = pos;
        }
    }
    (text[..best_idx].iter().collect(), text[best_idx..].to_vec())
}

// ---------------------------------------------------------------------------
// semchunk port (the plain-text splitter HybridChunker delegates to)
// ---------------------------------------------------------------------------

/// Semantically meaningful non-whitespace splitters, most desirable first.
const NON_WS_SPLITTERS: &[&str] = &[
    ".", "?", "!", "*", ";", ",", "(", ")", "[", "]", "\u{201c}", "\u{201d}", "\u{2018}",
    "\u{2019}", "'", "\"", "`", ":", "\u{2014}", "\u{2026}", "/", "\\", "\u{2013}", "&", "-",
];

/// Split `text` into chunks of at most `chunk_size` tokens using the most
/// semantically meaningful splitter available — the `semchunk` algorithm
/// docling's HybridChunker delegates plain-text splitting to.
pub fn semchunk<T: ChunkTokenizer>(text: &str, chunk_size: usize, tok: &T) -> Vec<String> {
    let mut cache: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut counter = |s: &str| -> usize {
        if let Some(n) = cache.get(s) {
            return *n;
        }
        let n = tok.count_tokens(s);
        cache.insert(s.to_string(), n);
        n
    };
    let chunks = semchunk_rec(text, chunk_size, &mut counter);
    // top-level: drop empty / all-whitespace chunks
    chunks
        .into_iter()
        .filter(|c| !c.is_empty() && !c.chars().all(char::is_whitespace))
        .collect()
}

/// One recursion level of semchunk: split, merge windows back up to size, and
/// recurse into oversized splits.
fn semchunk_rec(
    text: &str,
    chunk_size: usize,
    counter: &mut dyn FnMut(&str) -> usize,
) -> Vec<String> {
    let (splitter, splitter_is_ws, splits) = split_text(text);

    let split_lens: Vec<usize> = splits.iter().map(|s| s.chars().count()).collect();
    let mut cum_lens = Vec::with_capacity(splits.len() + 1);
    cum_lens.push(0usize);
    for l in &split_lens {
        cum_lens.push(cum_lens.last().unwrap() + l);
    }
    let num_splits_plus_one = splits.len() + 1;

    let mut chunks: Vec<String> = Vec::new();
    let mut skips: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for i in 0..splits.len() {
        if skips.contains(&i) {
            continue;
        }
        let split = &splits[i];
        if counter(split) > chunk_size {
            let inner = semchunk_rec(split, chunk_size, counter);
            chunks.extend(inner);
        } else {
            let (end, merged) = merge_splits(
                &splits,
                &cum_lens,
                chunk_size,
                &splitter,
                counter,
                i,
                num_splits_plus_one,
            );
            for j in (i + 1)..end {
                skips.insert(j);
            }
            chunks.push(merged);
        }
        // Re-attach a non-whitespace splitter to the last chunk (or emit it as
        // its own chunk if it doesn't fit).
        let is_last = i == splits.len() - 1 || ((i + 1)..splits.len()).all(|j| skips.contains(&j));
        if !splitter_is_ws && !is_last {
            let with_splitter = format!(
                "{}{}",
                chunks.last().map(String::as_str).unwrap_or(""),
                splitter
            );
            if counter(&with_splitter) <= chunk_size {
                if let Some(last) = chunks.last_mut() {
                    *last = with_splitter;
                } else {
                    chunks.push(with_splitter);
                }
            } else {
                chunks.push(splitter.clone());
            }
        }
    }
    chunks
}

/// docling/semchunk's `merge_splits`: extend the window with a cum-length-guided
/// binary search until the token budget is hit.
fn merge_splits(
    splits: &[String],
    cum_lens: &[usize],
    chunk_size: usize,
    splitter: &str,
    counter: &mut dyn FnMut(&str) -> usize,
    start: usize,
    high_init: usize,
) -> (usize, String) {
    let mut average = 0.2f64;
    let mut low = start;
    let mut high = high_init;
    let offset = cum_lens[start];
    let mut target = offset as f64 + (chunk_size as f64 * average);

    while low < high {
        let i = bisect_left(cum_lens, target, low, high);
        let midpoint = i.min(high - 1);
        let joined = splits[start..midpoint.max(start)].join(splitter);
        let tokens = counter(&joined);
        let local_cum = cum_lens[midpoint] - offset;
        if local_cum > 0 && tokens > 0 {
            average = local_cum as f64 / tokens as f64;
            target = offset as f64 + (chunk_size as f64 * average);
        }
        if tokens > chunk_size {
            high = midpoint;
        } else {
            low = midpoint + 1;
        }
    }
    let end = low - 1;
    (end, splits[start..end.max(start)].join(splitter))
}

fn bisect_left(sorted: &[usize], target: f64, mut low: usize, mut high: usize) -> usize {
    while low < high {
        let mid = (low + high) / 2;
        if (sorted[mid] as f64) < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

/// semchunk's `_split_text`: pick the most desirable splitter present.
fn split_text(text: &str) -> (String, bool, Vec<String>) {
    // Longest run of newlines/carriage returns.
    if text.contains('\n') || text.contains('\r') {
        let splitter = longest_run(text, |c| c == '\n' || c == '\r');
        return (splitter.clone(), true, split_on(text, &splitter));
    }
    // Longest run of tabs.
    if text.contains('\t') {
        let splitter = longest_run(text, |c| c == '\t');
        return (splitter.clone(), true, split_on(text, &splitter));
    }
    // Longest run of whitespace.
    if text.chars().any(char::is_whitespace) {
        let splitter = longest_run(text, char::is_whitespace);
        if splitter.chars().count() == 1 {
            // Prefer a whitespace char preceded by a semantic splitter.
            for preceder in NON_WS_SPLITTERS {
                if let Some((ws, parts)) = split_after_preceder(text, preceder) {
                    return (ws, true, parts);
                }
            }
        }
        return (splitter.clone(), true, split_on(text, &splitter));
    }
    // Most desirable semantic splitter present.
    for s in NON_WS_SPLITTERS {
        if text.contains(s) {
            return (s.to_string(), false, split_on(text, s));
        }
    }
    // No splitter at all: split into characters.
    (
        String::new(),
        true,
        text.chars().map(|c| c.to_string()).collect(),
    )
}

/// The longest maximal run of chars matching `pred` (first one wins ties).
fn longest_run(text: &str, pred: impl Fn(char) -> bool) -> String {
    let mut best = String::new();
    let mut cur = String::new();
    for c in text.chars() {
        if pred(c) {
            cur.push(c);
        } else {
            if cur.chars().count() > best.chars().count() {
                best = cur.clone();
            }
            cur.clear();
        }
    }
    if cur.chars().count() > best.chars().count() {
        best = cur;
    }
    best
}

fn split_on(text: &str, splitter: &str) -> Vec<String> {
    text.split(splitter).map(str::to_string).collect()
}

/// Python: `re.search(rf'{p}(\s)', text)` → the first whitespace char preceded
/// by `p`; then `re.split(rf'(?<={p}){s}', text)` — split at every occurrence
/// of that whitespace char immediately preceded by `p`.
fn split_after_preceder(text: &str, preceder: &str) -> Option<(String, Vec<String>)> {
    let chars: Vec<char> = text.chars().collect();
    let p: Vec<char> = preceder.chars().collect();
    let mut ws: Option<char> = None;
    for i in p.len()..chars.len() {
        if chars[i].is_whitespace() && chars[i - p.len()..i] == p[..] {
            ws = Some(chars[i]);
            break;
        }
    }
    let ws = ws?;
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == ws && i >= p.len() && chars[i - p.len()..i] == p[..] {
            parts.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(chars[i]);
        i += 1;
    }
    parts.push(cur);
    Some((ws.to_string(), parts))
}

// ---------------------------------------------------------------------------
// HuggingFace tokenizer (feature `chunking`)
// ---------------------------------------------------------------------------

#[cfg(feature = "chunking")]
mod hf {
    use super::ChunkTokenizer;

    /// Where `scripts/install/download_dependencies.sh` puts the hybrid
    /// chunker's default tokenizer (all-MiniLM-L6-v2's `tokenizer.json`),
    /// relative to the process's working directory — the same convention as
    /// the `models/` ONNX files.
    pub const DEFAULT_TOKENIZER_PATH: &str = "models/chunk/tokenizer.json";

    /// Resolve the tokenizer path for the hybrid chunker: an explicit path
    /// wins; otherwise fall back to [`DEFAULT_TOKENIZER_PATH`] when it exists
    /// on disk. Errors with the download instructions when neither is
    /// available.
    pub fn resolve_tokenizer_path(explicit: Option<&str>) -> Result<String, String> {
        if let Some(p) = explicit {
            return Ok(p.to_string());
        }
        if std::path::Path::new(DEFAULT_TOKENIZER_PATH).exists() {
            return Ok(DEFAULT_TOKENIZER_PATH.to_string());
        }
        Err(format!(
            "the hybrid chunker needs a HuggingFace tokenizer.json: none passed and \
             {DEFAULT_TOKENIZER_PATH} does not exist — run \
             scripts/install/download_dependencies.sh (or pass an explicit path)"
        ))
    }

    /// [`ChunkTokenizer`] backed by a HuggingFace `tokenizer.json` — the Rust
    /// analogue of docling's `HuggingFaceTokenizer` (whose default is
    /// `sentence-transformers/all-MiniLM-L6-v2` with `max_tokens` 256).
    pub struct HuggingFaceTokenizer {
        tok: tokenizers::Tokenizer,
        max_tokens: usize,
    }

    impl HuggingFaceTokenizer {
        /// Load the tokenizer from an explicit path, or from
        /// [`DEFAULT_TOKENIZER_PATH`] when `path` is `None` (see
        /// [`resolve_tokenizer_path`]).
        pub fn resolve(path: Option<&str>, max_tokens: usize) -> Result<Self, String> {
            Self::from_file(resolve_tokenizer_path(path)?, max_tokens)
        }

        /// Load a `tokenizer.json`. `max_tokens` is the chunk budget (docling
        /// resolves it from the model's `sentence_bert_config.json`; for the
        /// default MiniLM model that is 256).
        pub fn from_file(
            path: impl AsRef<std::path::Path>,
            max_tokens: usize,
        ) -> Result<Self, String> {
            let mut tok = tokenizers::Tokenizer::from_file(path.as_ref())
                .map_err(|e| format!("failed to load tokenizer: {e}"))?;
            // Counting must see the full text (docling tokenizes without
            // truncation, padding, or special tokens — MiniLM's tokenizer.json
            // ships with fixed-length padding enabled, which would make every
            // short string count as the padded length).
            let _ = tok.with_truncation(None);
            tok.with_padding(None);
            Ok(Self { tok, max_tokens })
        }
    }

    impl ChunkTokenizer for HuggingFaceTokenizer {
        fn count_tokens(&self, text: &str) -> usize {
            self.tok
                .encode(text, false)
                .map(|e| e.get_tokens().len())
                .unwrap_or(0)
        }
        fn max_tokens(&self) -> usize {
            self.max_tokens
        }
    }
}

#[cfg(feature = "chunking")]
pub use hf::{resolve_tokenizer_path, HuggingFaceTokenizer, DEFAULT_TOKENIZER_PATH};

#[cfg(test)]
mod tests {
    use super::*;

    /// A whitespace "tokenizer" for algorithm tests.
    struct WordTok(usize);
    impl ChunkTokenizer for WordTok {
        fn count_tokens(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
        fn max_tokens(&self) -> usize {
            self.0
        }
    }

    fn doc_with(nodes: Vec<Node>) -> DoclingDocument {
        let mut d = DoclingDocument::new("t");
        for n in nodes {
            d.push(n);
        }
        d
    }

    #[test]
    fn hierarchical_headings_and_items() {
        let doc = doc_with(vec![
            Node::Heading {
                level: 1,
                text: "Title".into(),
            },
            Node::Paragraph {
                text: "Intro".into(),
            },
            Node::Heading {
                level: 2,
                text: "Sec".into(),
            },
            Node::Paragraph {
                text: "Body".into(),
            },
        ]);
        let chunks = HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "Intro");
        assert_eq!(chunks[0].headings.as_deref(), Some(&["Title".into()][..]));
        assert_eq!(chunks[0].doc_items[0].self_ref, "#/texts/1");
        assert_eq!(
            chunks[1].headings.as_deref(),
            Some(&["Title".into(), "Sec".into()][..])
        );
        assert_eq!(contextualize(&chunks[1]), "Title\nSec\nBody");
    }

    #[test]
    fn heading_shadowing_prunes_deeper_levels() {
        let doc = doc_with(vec![
            Node::Heading {
                level: 2,
                text: "A".into(),
            },
            Node::Heading {
                level: 3,
                text: "A.1".into(),
            },
            Node::Heading {
                level: 2,
                text: "B".into(),
            },
            Node::Paragraph { text: "p".into() },
        ]);
        let chunks = HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks[0].headings.as_deref(), Some(&["B".into()][..]));
    }

    #[test]
    fn triplet_table() {
        let t = Table {
            rows: vec![
                vec!["".into(), "Col1".into()],
                vec!["Row1".into(), "v".into()],
            ],
            ..Default::default()
        };
        assert_eq!(triplet_table_text(&t), "Row1, Col1 = v");
        // Single-column: row 0 is the dataframe header, the first data row
        // becomes the column name, the rest the values.
        let single = Table {
            rows: vec![vec!["H".into()], vec!["a".into()], vec!["b".into()]],
            ..Default::default()
        };
        assert_eq!(triplet_table_text(&single), "a = b");
    }

    #[test]
    fn hybrid_merges_small_peers_and_splits_large() {
        let doc = doc_with(vec![
            Node::Heading {
                level: 2,
                text: "S".into(),
            },
            Node::Paragraph { text: "a b".into() },
            Node::Paragraph { text: "c d".into() },
        ]);
        let chunks = HybridChunker::new(WordTok(16)).chunk(&doc);
        assert_eq!(chunks.len(), 1, "peers under one heading merge");
        assert_eq!(chunks[0].text, "a b\nc d");

        let long = "w ".repeat(40).trim().to_string();
        let doc = doc_with(vec![Node::Paragraph { text: long }]);
        let chunks = HybridChunker::new(WordTok(16)).chunk(&doc);
        assert!(chunks.len() > 1, "oversized paragraph splits");
        for c in &chunks {
            assert!(WordTok(16).count_tokens(&contextualize(c)) <= 16);
        }
    }

    #[test]
    fn semchunk_prefers_newlines_then_sentences() {
        let tok = WordTok(4);
        let out = semchunk("one two three. four five six\nseven eight", 4, &tok);
        assert!(out.iter().all(|c| tok.count_tokens(c) <= 4), "{out:?}");
    }
}
