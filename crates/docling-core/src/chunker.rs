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

use std::collections::{BTreeMap, VecDeque};

use crate::document::{DoclingDocument, Node, Table};

/// What kind of document item a [`ChunkItem`] points at — only what the
/// hybrid splitting logic needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    ///
    /// Each chunk's [`ChunkItem::self_ref`] is the item's ref in the
    /// document's [JSON export](DoclingDocument::export_to_json_value)
    /// (#1041): the walk numbers the flat nodes, which is the JSON's
    /// numbering only for a document without an item tree, so the refs are
    /// mapped onto the export's items ([`RefMapper`]).
    pub fn chunk_with(&self, doc: &DoclingDocument, sink: &mut dyn FnMut(DocChunk) -> bool) {
        let mut mapper = RefMapper::new(&doc.export_to_json_value());
        Self::walk_nodes(doc, &mut |c| sink(mapper.remap(c)));
    }

    /// The walk itself, its refs in the flat nodes' numbering — what
    /// [`HybridChunker`] splits and merges before the refs are mapped.
    fn walk_nodes(doc: &DoclingDocument, sink: &mut dyn FnMut(DocChunk) -> bool) {
        let mut w = Walker {
            alloc: Alloc::default(),
            headings: BTreeMap::new(),
            stopped: false,
            sink,
        };
        w.walk(&doc.nodes);
    }
}

/// Maps the walk's chunk items onto the document's JSON export — a port of
/// `docling_rs.chunking._RefMapper`, which the Python binding has applied to
/// the caller's document since #1012.
///
/// The walk numbers the flat nodes the way the JSON export numbers a
/// document *without* an item tree; a backend's tree (HTML, DOCX, ODF, JATS,
/// LaTeX, …) is numbered by its own structure — a furniture title first, an
/// inline paragraph as several runs, empty items kept — so the refs drift
/// (#1041: on the corpus half of the text refs pointed at another item). The
/// items still arrive in (nearly) reading order with their kind and Markdown
/// text, so each is matched against the export's items walked in the same
/// order (the `body` tree, pictures and tables not descended): a table or
/// picture to the next untaken one of its kind, a text to an item it renders
/// exactly (`Section 2` for `[Section 2](#s2)`, `x` for `- x`), else by
/// containment close to the last match — an inline paragraph the walk
/// reports as one item then maps to the run of items it contains. Candidates
/// lie in a window around the last match, so an item emitted out of tree
/// order (a caption next to its float) neither strands the rest nor goes
/// unmatched; an item repeated across the chunks it is split into maps to
/// the same items again. An item that matches nothing is left out rather
/// than pointed at the wrong item.
struct RefMapper {
    /// The export's items in reading order: `(ref, kind, normalized text)`.
    order: Vec<(String, ChunkItemKind, String)>,
    /// Per item, a list item's marker and text as its Markdown prints them
    /// (`甲. 甲项`), the marker being a field of its own in the JSON.
    marked: Vec<Option<String>>,
    taken: Vec<bool>,
    /// The earliest untaken item.
    low: usize,
    /// One past the latest match.
    last: usize,
    seen: std::collections::HashMap<(String, ChunkItemKind, String), Vec<usize>>,
}

impl RefMapper {
    const WINDOW: usize = 64;
    const NEAR: usize = 16;
    const MIN_CONTAINED: usize = 4;

    fn new(doc: &serde_json::Value) -> Self {
        use serde_json::Value;
        let mut index: std::collections::HashMap<&str, &Value> = std::collections::HashMap::new();
        for key in [
            "texts",
            "tables",
            "pictures",
            "groups",
            "field_regions",
            "field_items",
            "key_value_items",
            "form_items",
        ] {
            for it in doc.get(key).and_then(Value::as_array).into_iter().flatten() {
                if let Some(r) = it.get("self_ref").and_then(Value::as_str) {
                    index.insert(r, it);
                }
            }
        }
        let children = |v: &Value| -> Vec<String> {
            v.get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| c.get("$ref").or_else(|| c.get("cref")))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        let mut order = Vec::new();
        let mut marked: Vec<Option<String>> = Vec::new();
        let mut stack: Vec<std::vec::IntoIter<String>> = doc
            .get("body")
            .map(|b| vec![children(b).into_iter()])
            .unwrap_or_default();
        // A crafted document can list an item twice or cyclically: each is
        // walked once.
        let mut visited = std::collections::HashSet::new();
        while let Some(top) = stack.last_mut() {
            let Some(r) = top.next() else {
                stack.pop();
                continue;
            };
            let Some(it) = index.get(r.as_str()).copied() else {
                continue;
            };
            if !visited.insert(r.clone()) {
                continue;
            }
            let text_of = |it: &Value| norm(it.get("text").and_then(Value::as_str).unwrap_or(""));
            if r.starts_with("#/texts/") {
                let marker = it.get("marker").and_then(Value::as_str).unwrap_or("");
                while marked.len() < order.len() {
                    marked.push(None);
                }
                marked
                    .push((!marker.is_empty()).then(|| norm(&format!("{marker} {}", text_of(it)))));
                order.push((r, ChunkItemKind::Text, text_of(it)));
            } else if r.starts_with("#/tables/") || r.starts_with("#/pictures/") {
                // A float is not descended, but its captions and footnotes are
                // chunked with it — wherever they hang (a PDF caption is the
                // float's child, a declarative one the body's).
                let refs = |key: &str| -> Vec<String> {
                    it.get(key)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|c| c.get("$ref").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                };
                let mut sat = |order: &mut Vec<(String, ChunkItemKind, String)>, key: &str| {
                    for c in refs(key) {
                        if let Some(t) = index.get(c.as_str()) {
                            if visited.insert(c.clone()) {
                                order.push((c, ChunkItemKind::Text, text_of(t)));
                            }
                        }
                    }
                };
                sat(&mut order, "captions");
                let kind = if r.starts_with("#/tables/") {
                    ChunkItemKind::Table
                } else {
                    ChunkItemKind::Picture
                };
                order.push((r, kind, String::new()));
                sat(&mut order, "footnotes");
                continue;
            }
            stack.push(children(it).into_iter());
        }
        let n = order.len();
        marked.resize(n, None);
        RefMapper {
            order,
            marked,
            taken: vec![false; n],
            low: 0,
            last: 0,
            seen: std::collections::HashMap::new(),
        }
    }

    /// Untaken items of `kind` in `[lo, hi)`, nearest-first: forward from
    /// the last match, then backward.
    fn near(&self, lo: usize, hi: usize, used: &[usize], kind: ChunkItemKind) -> Vec<usize> {
        let last = self.last.max(lo).min(hi);
        (last..hi)
            .chain((lo..last).rev())
            .filter(|&i| !self.taken[i] && !used.contains(&i) && self.order[i].1 == kind)
            .collect()
    }

    fn find(
        &self,
        (lo, hi): (usize, usize),
        used: &[usize],
        kind: ChunkItemKind,
        text: &str,
    ) -> Option<usize> {
        if kind != ChunkItemKind::Text {
            return self.near(lo, hi, used, kind).first().copied();
        }
        let plain = plain(text);
        if let Some(i) = self
            .near(lo, hi, used, kind)
            .into_iter()
            .find(|&i| self.matches(i, text, &plain))
        {
            return Some(i);
        }
        // Containment only close to the last match and for texts long enough
        // to mean something: a short one ("duck", ")") is contained in many
        // renderings and would pull the mapping out of step.
        let near_lo = lo.max(self.last.saturating_sub(Self::NEAR));
        let near_hi = hi.min(self.last + Self::NEAR);
        if near_lo >= near_hi {
            return None;
        }
        self.near(near_lo, near_hi, used, kind)
            .into_iter()
            .find(|&i| {
                let t = &self.order[i].2;
                t.chars().count() >= Self::MIN_CONTAINED && text.contains(t.as_str())
            })
    }

    /// Whether item `i` is what `text` (or its plain form) renders exactly.
    fn matches(&self, i: usize, text: &str, plain: &str) -> bool {
        let t = &self.order[i].2;
        t == text || t == plain || self.marked[i].as_deref().is_some_and(|m| m == plain)
    }

    /// Past the window, forward from the last match: an item matched
    /// exactly, or a long one the rendering contains — what is left when the
    /// walk skipped more of the export than the window spans (a large
    /// region it does not chunk). Short texts never match this far.
    fn find_far(&self, used: &[usize], kind: ChunkItemKind, text: &str) -> Option<usize> {
        const FAR_CONTAINED: usize = 16;
        // Bounded, so a document whose walk emits many items the export
        // lacks stays linear.
        const FAR: usize = 4096;
        if kind != ChunkItemKind::Text || text.chars().count() < FAR_CONTAINED {
            return None;
        }
        let plain = plain(text);
        let free = |i: &usize| !self.taken[*i] && !used.contains(i) && self.order[*i].1 == kind;
        let end = self.order.len().min(self.last + FAR);
        let far = || (self.last..end).filter(free);
        far().find(|&i| self.matches(i, text, &plain)).or_else(|| {
            far().find(|&i| {
                let t = &self.order[i].2;
                t.chars().count() >= FAR_CONTAINED && text.contains(t.as_str())
            })
        })
    }

    /// A text the walk split off one of the export's items (a paragraph cut
    /// at what its inline parser took for markup): the item that contains
    /// it — the latest match first, though already taken, then an untaken
    /// one close to it. Only for texts long enough to place.
    fn part_of(&self, lo: usize, hi: usize, text: &str) -> Option<usize> {
        let piece = plain(text);
        if piece.chars().count() < 2 * Self::MIN_CONTAINED {
            return None;
        }
        let contains = |i: usize| {
            self.order[i].1 == ChunkItemKind::Text && self.order[i].2.contains(piece.as_str())
        };
        if let Some(prev) = self.last.checked_sub(1).filter(|&p| contains(p)) {
            return Some(prev);
        }
        let near_lo = lo.max(self.last.saturating_sub(Self::NEAR));
        let near_hi = hi.min(self.last + Self::NEAR);
        (near_lo..near_hi).find(|&i| !self.taken[i] && contains(i))
    }

    /// The text items after a containment match that the same rendering
    /// also contains, in order — the runs of an inline paragraph.
    fn runs_after(&self, hit: usize, hi: usize, used: &[usize], text: &str) -> Vec<usize> {
        let first = &self.order[hit].2;
        let mut pos = text.find(first.as_str()).map_or(0, |p| p + first.len());
        let mut extra = Vec::new();
        let mut j = hit + 1;
        while j < hi
            && !self.taken[j]
            && !used.contains(&j)
            && self.order[j].1 == ChunkItemKind::Text
        {
            let t = &self.order[j].2;
            let Some(at) = (!t.is_empty())
                .then(|| text.get(pos..).and_then(|rest| rest.find(t.as_str())))
                .flatten()
            else {
                break;
            };
            extra.push(j);
            pos += at + t.len();
            j += 1;
        }
        extra
    }

    fn remap(&mut self, mut chunk: DocChunk) -> DocChunk {
        let n = chunk.doc_items.len();
        let lo = self.low.max(self.last.saturating_sub(Self::WINDOW));
        let hi = self.order.len().min(self.last + Self::WINDOW + n);
        let mut used: Vec<usize> = Vec::new();
        let mut items = Vec::with_capacity(n);
        for item in std::mem::take(&mut chunk.doc_items) {
            let text = norm(&item.text);
            let key = (item.self_ref.clone(), item.kind, text.clone());
            let mut runs = self
                .seen
                .get(&key)
                .filter(|r| !r.iter().any(|h| used.contains(h)))
                .cloned();
            if runs.is_none() && lo < hi {
                let hit = self
                    .find((lo, hi), &used, item.kind, &text)
                    .or_else(|| self.find_far(&used, item.kind, &text));
                if let Some(hit) = hit {
                    let mut r = vec![hit];
                    if item.kind == ChunkItemKind::Text && !self.matches(hit, &text, &plain(&text))
                    {
                        r.extend(self.runs_after(hit, hi, &used, &text));
                    }
                    self.seen.insert(key, r.clone());
                    runs = Some(r);
                } else if item.kind == ChunkItemKind::Text {
                    runs = self.part_of(lo, hi, &text).map(|h| vec![h]);
                }
            }
            let Some(runs) = runs else {
                crate::debug_log!(
                    "chunker: no JSON item for {} ({:?}) in [{lo}, {hi}), last {}",
                    item.self_ref,
                    text.chars().take(60).collect::<String>(),
                    self.last
                );
                continue;
            };
            for (k, &h) in runs.iter().enumerate() {
                if items
                    .iter()
                    .any(|i: &ChunkItem| i.self_ref == self.order[h].0)
                {
                    continue; // another piece of the same item
                }
                used.push(h);
                self.taken[h] = true;
                self.last = self.last.max(h + 1);
                // The walk's item keeps its own text (what the hybrid splitter
                // re-serializes); the runs after it are the export's items.
                items.push(ChunkItem {
                    self_ref: self.order[h].0.clone(),
                    kind: item.kind,
                    text: if k == 0 {
                        item.text.clone()
                    } else {
                        self.order[h].2.clone()
                    },
                });
            }
        }
        while self.low < self.taken.len() && self.taken[self.low] {
            self.low += 1;
        }
        chunk.doc_items = items;
        chunk
    }
}

/// `_norm`: an item text comparable to its Markdown rendering — entities
/// decoded, escapes dropped, whitespace collapsed.
fn norm(text: &str) -> String {
    crate::doclang_xml::decode_entities(text)
        .replace('\\', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `_plain`: a Markdown item rendering reduced toward the item's plain text —
/// link targets, emphasis and code markers and a heading or list prefix
/// dropped.
fn plain(md: &str) -> String {
    use std::sync::OnceLock;
    static LINK: OnceLock<regex::Regex> = OnceLock::new();
    static PREFIX: OnceLock<regex::Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| regex::Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").expect("static"));
    let prefix = PREFIX.get_or_init(|| {
        regex::Regex::new(r"^(#{1,6}(\s+|$)|[-*+](\s+\[[ xX]\])?(\s+|$)|\d+[.)](\s+|$))")
            .expect("static")
    });
    let mut text = link.replace_all(md, "$1").into_owned();
    for marker in ["**", "~~", "`", "*"] {
        text = text.replace(marker, "");
    }
    let text = prefix.replace(&text, "");
    // A formula renders in `$$…$$` / `$…$`.
    let text = text.trim();
    let text = text
        .strip_prefix("$$")
        .and_then(|t| t.strip_suffix("$$"))
        .or_else(|| text.strip_prefix('$').and_then(|t| t.strip_suffix('$')))
        .unwrap_or(text);
    text.trim().to_string()
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
        for k in 0..run.len() {
            let Node::ListItem {
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
            // The backend's list boundary, as in `json.rs::add_sibling_lists`.
            if k > seg && *first_in_list {
                self.list(&run[seg..k]);
                seg = k;
            }
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
                // docling represents a list item whose content is not pure
                // inline text (it carries a nested list or child pictures) as
                // an empty item wrapping an inline group: its marker and each
                // inline span become separate doc items.
                // docling represents a list item whose content is not pure
                // inline text (it carries a nested list or its own images) as
                // an empty item wrapping an inline group: its marker and each
                // inline span become separate doc items. The chunker's
                // image placeholder is empty, so the markers are stripped from
                // the text before segmentation.
                let has_pics = text.contains("<!-- image -->");
                let text = strip_image_markers(text);
                let text = text.as_str();
                let segments = inline_segments(text);
                if (has_nested || has_pics) && segments.len() > 1 && text.contains("](") {
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
        for k in 0..run.len() {
            let Node::ListItem {
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
            if k > seg && *first_in_list {
                self.alloc.group();
                self.list_refs(&run[seg..k], out);
                seg = k;
            }
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
                self.alloc.text();
                // docling stores heading text unformatted: a heading that is one
                // uniformly formatted span keeps its plain text; a *partially*
                // formatted heading becomes an empty heading whose only child is
                // an inline group. docling-core 2.106 (docling-core#924, #1012)
                // serializes that child as the heading's context and consumes
                // it, so it is no longer a body chunk under an empty heading.
                let runs = crate::inline_runs_from_markdown(text);
                let heading = if runs.len() <= 1 {
                    runs.first()
                        .map(|r| r.text.clone())
                        .unwrap_or_else(|| text.clone())
                } else {
                    text.clone()
                };
                self.set_heading(doc_level, unescape_text(&heading));
            }
            Node::Paragraph { text } | Node::ListText { text, .. } => {
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
            // A standalone caption chunks like any text item (docling's chunker
            // does not treat `caption` specially outside a table/picture).
            Node::Caption { text, .. } => {
                let self_ref = self.alloc.text();
                self.emit_inline(text, self_ref);
            }
            Node::LabeledText { .. } => self.one(&node.labeled_as_paragraph()),
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
            // An enriched display formula chunks like docling's formula item:
            // the LaTeX is the chunk text.
            Node::Formula { latex, .. } => {
                let self_ref = self.alloc.text();
                let body = format!("$${}$$", latex);
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
                let body = crate::markdown::code_fence("", &unescape_text(text));
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
                // `TripletTableSerializer`: the captions open the table's
                // chunk and are among its doc items — the caption is the
                // `#/texts/N` item the JSON writes ahead of the table (#1041:
                // it took no ref here, so every later text ref pointed one
                // item too early).
                let mut items = Vec::new();
                let mut parts = Vec::new();
                if let Some(cap) = t.caption.as_deref().filter(|c| !c.is_empty()) {
                    let text = unescape_text(cap);
                    parts.push(text.clone());
                    items.push(ChunkItem {
                        self_ref: self.alloc.text(),
                        kind: ChunkItemKind::Text,
                        text,
                    });
                }
                let self_ref = self.alloc.table();
                let body = triplet_table_text(t);
                items.push(ChunkItem {
                    self_ref,
                    kind: ChunkItemKind::Table,
                    text: body.clone(),
                });
                // docling-core#675: the table's footnotes (the `footnote`
                // texts the JSON writes after it) close its chunk, one
                // paragraph each, and are among its doc items.
                parts.push(body);
                for note in t.footnotes.iter().filter(|n| !n.is_empty()) {
                    let text = unescape_text(note);
                    parts.push(text.clone());
                    items.push(ChunkItem {
                        self_ref: self.alloc.text(),
                        kind: ChunkItemKind::Text,
                        text,
                    });
                }
                let body = parts
                    .into_iter()
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                self.emit(body, items);
            }
            Node::Picture {
                caption,
                description,
                ..
            } => {
                let cap = caption.as_deref().filter(|c| !c.is_empty());
                let cap_item = cap.map(|c| ChunkItem {
                    self_ref: self.alloc.text(),
                    kind: ChunkItemKind::Text,
                    text: unescape_text(c),
                });
                let pic_ref = self.alloc.picture();
                // The picture itself serializes to the (empty) chunking image
                // placeholder, and its caption is already consumed by the
                // caption chunk — so only the caption text is emitted. A
                // description (the picture-OCR text, #645) is the picture's
                // own annotation text — docling's chunker serializes the
                // picture through the Markdown picture serializer, so the
                // description lands in the chunk after the caption.
                let desc = description
                    .as_ref()
                    .map(|d| d.text.as_str())
                    .filter(|t| !t.is_empty());
                match (cap_item, desc) {
                    (Some(cap_item), None) => {
                        let body = cap_item.text.clone();
                        self.emit(body, vec![cap_item]);
                    }
                    (Some(cap_item), Some(desc)) => {
                        let body = format!("{}\n\n{desc}", cap_item.text);
                        let pic_item = ChunkItem {
                            self_ref: pic_ref,
                            kind: ChunkItemKind::Picture,
                            text: desc.to_string(),
                        };
                        self.emit(body, vec![cap_item, pic_item]);
                    }
                    (None, Some(desc)) => {
                        let pic_item = ChunkItem {
                            self_ref: pic_ref,
                            kind: ChunkItemKind::Picture,
                            text: desc.to_string(),
                        };
                        self.emit(desc.to_string(), vec![pic_item]);
                    }
                    (None, None) => {}
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
            // A group on a non-body layer (a hidden spreadsheet sheet) carries
            // no chunkable content, like every other non-body item.
            Node::Group { layer: Some(_), .. } => {}
            Node::Group { children, .. } => {
                // A generic group is a structural container: docling recurses
                // into it rather than chunking it whole.
                self.alloc.group();
                self.walk(children);
            }
            // A key-value graph holds no text items, so no chunk comes of it.
            Node::KeyValueGraph { .. } => {}
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
            // Layout provenance and comment annotations are transparent.
            Node::Located { inner, .. }
            | Node::Prov { inner, .. }
            | Node::Track { inner, .. }
            | Node::Commented { inner, .. } => self.one(inner),
            // A PDF page header/footer is a furniture-layer JSON text item:
            // never chunked (docling's chunker reads the body layer), but it
            // takes a `#/texts/N` number, so later refs keep matching the JSON.
            Node::PageFurniture { .. } | Node::FurnitureText { .. } => {
                self.alloc.text();
            }
            // A picture's children are JSON items docling's chunker never
            // reaches (it iterates without `traverse_pictures`): allocate
            // their refs only — one text each, plus the list group `json.rs`
            // opens for each list item (each starts its own list) — so later
            // refs keep matching the JSON.
            Node::PictureChildren(children) => {
                for child in children {
                    match child {
                        // A header/footer child is numbered exactly as a
                        // top-level one is, whatever the JSON does with it.
                        Node::PageFurniture { .. } => self.one(child),
                        Node::ListItem { .. } => {
                            self.alloc.group();
                            self.alloc.text();
                        }
                        _ => {
                            self.alloc.text();
                        }
                    }
                }
            }
            // Non-body layers and doclang-only nodes don't reach the chunker
            // (nor the JSON body).
            Node::CommentSection { .. }
            | Node::Furniture { .. }
            | Node::PageBreak
            | Node::PageInfo { .. }
            | Node::Anchor { .. }
            | Node::DoclangOnly(_) => {}
            // Runs are grouped by `walk`; a stray single item (hand-built
            // document, `Located` wrapper) still chunks instead of panicking.
            Node::ListItem { .. } => self.sibling_lists(std::slice::from_ref(node)),
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
        lines.push(format!(
            "{indent}{marker} {}",
            unescape_text(&strip_image_markers(text))
        ));
    }
    lines.join("\n")
}

/// Strip the Markdown image placeholders a list item's own images fold into
/// its text (the chunking serializer's `image_placeholder` is empty, so
/// docling's chunk text carries no marker), collapsing the newlines that
/// carried them.
fn strip_image_markers(text: &str) -> String {
    if !text.contains("<!-- image -->") {
        return text.to_string();
    }
    let cleaned: Vec<&str> = text
        .split('\n')
        .map(str::trim_end)
        .filter(|l| *l != "<!-- image -->")
        .collect();
    cleaned.join("\n").trim_end().to_string()
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
    let anchors = t.span_anchors();
    let rows: Vec<Vec<String>> = t
        .rows
        .iter()
        .enumerate()
        .map(|(ri, r)| {
            (0..r.len())
                .map(|ci| cell_chunk_text(t, &anchors, ri, ci))
                .collect()
        })
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

    // The header block is the one the Markdown serializer uses
    // (`Table::column_header_row_count`, docling-core#767), without its
    // "no flags -> row 0" fallback: pandas then gets integer column names.
    let num_headers = t.column_header_row_count().min(num_rows);

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
        // The URL may itself contain balanced parentheses (`/Duck_(film)`),
        // so the closing `)` is found by paren depth, not first match.
        if chars[i] == '[' && !rest.starts_with("[](") {
            // The label may itself contain balanced brackets (`[[ 1 ]](#ref)`
            // has label `[ 1 ]`), but an *unbalanced* `[` means this bracket is
            // plain text preceding a real link (`[ [*note*](url) ]`).
            let balanced = |c: usize| {
                let mut d = 0i32;
                for &ch in &chars[i + 1..c] {
                    match ch {
                        '[' => d += 1,
                        ']' => d -= 1,
                        _ => {}
                    }
                }
                d == 0
            };
            if let Some(close) = find(i + 1, "](").filter(|&c| balanced(c)) {
                let mut depth = 0usize;
                let mut url_end = None;
                for (k, &c) in chars.iter().enumerate().skip(close + 2) {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            if depth == 0 {
                                url_end = Some(k);
                                break;
                            }
                            depth -= 1;
                        }
                        _ => {}
                    }
                }
                if let Some(endp) = url_end {
                    // An unsafe destination (`javascript:`, #860) is plain
                    // text, not a hyperlink item.
                    let url: String = chars[close + 2..endp].iter().collect();
                    if !crate::hyperlink::is_safe_hyperlink(&url) {
                        plain.extend(&chars[i + 1..close]);
                        i = endp + 1;
                        continue;
                    }
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
        // An inline code span is a code item in docling's model, whose
        // standalone form is a fenced block; its backticks may run longer
        // than one (#859).
        if chars[i] == '`' {
            if let Some((s, e, after)) = crate::markdown::code_span_bounds(&chars, i) {
                if e > s && !chars[s..e].iter().all(|c| c.is_whitespace()) {
                    flush(&mut out, &mut plain, true, after_span);
                    let inner: String = chars[s..e].iter().collect();
                    out.push((
                        crate::markdown::code_fence("", &unescape_text(&inner)),
                        false,
                    ));
                    i = after;
                    after_span = true;
                    continue;
                }
            }
        }
        // A formatted span; longest markers first.
        let mut matched = false;
        for marker in ["***", "**", "*", "~~"] {
            if rest.starts_with(marker) {
                let mlen = marker.chars().count();
                if let Some(end) = find(i + mlen, marker) {
                    // A whitespace-only span (`* *` from a literal asterisk in
                    // running text next to a real italic) is not a docling run:
                    // treat the marker as plain text.
                    let inner_blank = chars[i + mlen..end].iter().all(|c| c.is_whitespace());
                    if end > i + mlen && !inner_blank {
                        flush(&mut out, &mut plain, true, after_span);
                        out.push((
                            unescape_text(&chars[i..end + mlen].iter().collect::<String>()),
                            false,
                        ));
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
    let plainish =
        |r: &crate::InlineRun| !r.bold && !r.italic && !r.strike && !r.code && !r.formula;
    let fully_plain =
        |r: &crate::InlineRun| plainish(r) && !r.underline && r.script == crate::Script::Baseline;
    let unmarked: Vec<(&str, bool)> = runs
        .iter()
        .filter(|r| plainish(r))
        .map(|r| (r.text.as_str(), fully_plain(r)))
        .collect();
    for start in 0..unmarked.len() {
        let mut rest = target;
        let mut taken: Vec<(String, bool)> = Vec::new();
        for (t, fully) in &unmarked[start..] {
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            match rest.strip_prefix(t) {
                Some(r) => {
                    taken.push((unescape_text(t), *fully));
                    rest = r.trim_start();
                    if rest.is_empty() {
                        break;
                    }
                }
                None => break,
            }
        }
        if rest.is_empty() && taken.len() >= 2 {
            // Consecutive fully-plain pieces (no underline / sub / sup) are one
            // annotation in docling's model — `simplify_text_elements` merges
            // them joined with a space, so they never split between themselves.
            // Only an underline / sub / sup run is a genuine boundary.
            let mut merged: Vec<(String, bool)> = Vec::new();
            for (t, fully) in taken {
                match merged.last_mut() {
                    Some((last, true)) if fully => {
                        last.push(' ');
                        last.push_str(&t);
                    }
                    _ => merged.push((t, fully)),
                }
            }
            if merged.len() >= 2 {
                return Some(merged.into_iter().map(|(t, _)| t).collect());
            }
            return None;
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
fn cell_chunk_text(t: &Table, anchors: &[Vec<(usize, usize)>], r: usize, c: usize) -> String {
    // A rich cell's blocks live on its anchor; every position it covers
    // reads them (docling-core#857, #777).
    let (ar, ac) = anchors
        .get(r)
        .and_then(|row| row.get(c))
        .copied()
        .unwrap_or((r, c));
    if let Some(blocks) = t
        .cell_blocks
        .as_ref()
        .and_then(|b| b.get(ar))
        .and_then(|row| row.get(ac))
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
        Node::Paragraph { text } | Node::ListText { text, .. } => unescape_text(text),
        Node::LabeledText { .. } => block_chunk_text(&node.labeled_as_paragraph()),
        Node::InlineGroup { md_text, .. } => unescape_text(md_text),
        Node::Code { text, .. } => crate::markdown::code_fence("", &unescape_text(text)),
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
        Node::Located { inner, .. }
        | Node::Prov { inner, .. }
        | Node::Track { inner, .. }
        | Node::Commented { inner, .. } => block_chunk_text(inner),
        Node::Group { layer: Some(_), .. } => String::new(),
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
        // The refs are mapped onto the JSON export last, on the chunks as
        // emitted: the splitting re-serializes the walk's items (#1041).
        let mut mapper = RefMapper::new(&doc.export_to_json_value());
        let sink = &mut |c: DocChunk| sink(mapper.remap(c));
        let mut merger = PeerMerger::default();
        let mut alive = true;
        HierarchicalChunker::walk_nodes(doc, &mut |c| {
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

        let make = |segments: Vec<String>| -> Vec<DocChunk> {
            segments
                .into_iter()
                .map(|s| DocChunk {
                    text: s,
                    headings: chunk.headings.clone(),
                    doc_items: chunk.doc_items.clone(),
                })
                .collect()
        };
        if chunk.doc_items.len() == 1 && chunk.doc_items[0].kind == ChunkItemKind::Table {
            // Table: split line-based. The triplet serializer has no header
            // lines, so this is a line-preserving split of the table text.
            // docling-core ≥ 2.103 (docling-core#690, docling.rs#987) budgets
            // the pieces at `available` (the window minus the heading path),
            // like text, and re-splits with a smaller budget when a piece's
            // `contextualize()` still overshoots — token counts are not
            // additive, so the subtraction alone can leave a piece a token
            // or two over. (≤ 2.102 packed the full window, so every piece
            // overran by the heading length — a deliberate fix here.)
            let lines: Vec<String> = chunk
                .text
                .split('\n')
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect();
            let mut budget = available;
            loop {
                let pieces = make(line_chunk_text(&lines, &self.tokenizer, budget));
                let overflow = pieces
                    .iter()
                    .map(|p| self.count_chunk_tokens(p).saturating_sub(max))
                    .max()
                    .unwrap_or(0);
                if overflow == 0 || budget <= overflow {
                    return pieces;
                }
                budget -= overflow;
            }
        }
        make(semchunk(&chunk.text, available, &self.tokenizer))
    }

    /// One step of docling's `_merge_chunks_with_matching_metadata`, streamed:
    /// extend the window with `chunk` when its headings match the window's and
    /// the merged window stays within budget, otherwise flush the window to
    /// `sink` and start a new one at `chunk`. Returns `false` once the sink
    /// cancels.
    ///
    /// Linear, as docling-core 2.102 has it (docling-core#823, #782): each
    /// chunk's text is counted once, the window grows on a running estimate
    /// (text tokens, the delimiter's cost, the headings' overhead), an exact
    /// count of the merged window runs only where the estimate overflows, and
    /// the final window is verified exactly — shrunk back to the largest one
    /// that fits when the estimate under-counted, the chunks it gives up
    /// starting the next window. Rebuilding and re-tokenizing the whole window
    /// for every chunk was quadratic in the window's length.
    fn merge_push(
        &self,
        m: &mut PeerMerger,
        chunk: DocChunk,
        sink: &mut dyn FnMut(DocChunk) -> bool,
    ) -> bool {
        self.merge_feed(m, VecDeque::from([(chunk, None)]), sink)
    }

    /// Feed chunks (with their text token count when already known) to the
    /// merge window, closing it whenever a chunk cannot join.
    fn merge_feed(
        &self,
        m: &mut PeerMerger,
        mut pending: VecDeque<(DocChunk, Option<usize>)>,
        sink: &mut dyn FnMut(DocChunk) -> bool,
    ) -> bool {
        while let Some((chunk, tokens)) = pending.pop_front() {
            let tokens = tokens.unwrap_or_else(|| self.tokenizer.count_tokens(&chunk.text));
            if let Err(chunk) = self.merge_extend(m, chunk, tokens) {
                let (alive, rest) = self.merge_close(m, sink);
                if !alive {
                    return false;
                }
                pending.push_front((chunk, Some(tokens)));
                for item in rest.into_iter().rev() {
                    pending.push_front(item);
                }
            }
        }
        true
    }

    /// Add `chunk` (of `tokens` text tokens) to the window, or hand it back
    /// when its headings differ or the window would overflow.
    fn merge_extend(
        &self,
        m: &mut PeerMerger,
        chunk: DocChunk,
        tokens: usize,
    ) -> Result<(), DocChunk> {
        let max = self.max_tokens() as i64;
        if m.window.is_empty() {
            // The headings' overhead around the joined text is shared by every
            // candidate of this window (it may be negative).
            m.overhead = self.count_chunk_tokens(&chunk) as i64 - tokens as i64;
            m.running = tokens as i64;
            m.window.push((chunk, tokens));
            return Ok(());
        }
        if chunk.headings != m.window[0].0.headings {
            return Err(chunk);
        }
        let delim = *m.delim.get_or_insert_with(|| {
            // `_estimate_delimiter_tokens`: what joining two texts adds.
            let t = &self.tokenizer;
            t.count_tokens("a\nb") as i64 - t.count_tokens("a") as i64 - t.count_tokens("b") as i64
        });
        m.running += tokens as i64 + delim;
        if m.running + m.overhead > max {
            let exact = self.count_chunk_tokens(&merged_chunk(
                m.window.iter().map(|(c, _)| c).chain([&chunk]),
            )) as i64;
            if exact > max {
                return Err(chunk);
            }
            // The estimate overshot; re-anchor it on the exact count.
            m.running = exact - m.overhead;
        }
        m.window.push((chunk, tokens));
        Ok(())
    }

    /// Close the window: verify it exactly, shrinking it back to the largest
    /// prefix that fits, emit that (a single chunk unchanged), and return the
    /// chunks given up, which start the next window.
    fn merge_close(
        &self,
        m: &mut PeerMerger,
        sink: &mut dyn FnMut(DocChunk) -> bool,
    ) -> (bool, Vec<(DocChunk, Option<usize>)>) {
        if m.window.is_empty() {
            return (true, Vec::new());
        }
        let max = self.max_tokens();
        let mut fit = m.window.len() - 1;
        while fit > 0
            && self.count_chunk_tokens(&merged_chunk(m.window[..=fit].iter().map(|(c, _)| c))) > max
        {
            fit -= 1;
        }
        let rest = m
            .window
            .split_off(fit + 1)
            .into_iter()
            .map(|(c, t)| (c, Some(t)))
            .collect();
        let out = if m.window.len() == 1 {
            m.window.pop().expect("single-chunk window").0
        } else {
            let merged = merged_chunk(m.window.iter().map(|(c, _)| c));
            m.window.clear();
            merged
        };
        (sink(out), rest)
    }

    /// Flush the merge window at the end of the stream (feeding back what a
    /// shrunk window gave up). Returns `false` once the sink cancels.
    fn merge_flush(&self, m: &mut PeerMerger, sink: &mut dyn FnMut(DocChunk) -> bool) -> bool {
        while !m.window.is_empty() {
            let (alive, rest) = self.merge_close(m, sink);
            if !alive || !self.merge_feed(m, rest.into(), sink) {
                return false;
            }
        }
        true
    }
}

/// docling's merged peer chunk: the texts joined by `\n`, every chunk's doc
/// items, the first chunk's headings.
fn merged_chunk<'a>(chunks: impl Iterator<Item = &'a DocChunk> + Clone) -> DocChunk {
    let mut first = chunks.clone();
    DocChunk {
        text: chunks
            .clone()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        headings: first.next().and_then(|c| c.headings.clone()),
        doc_items: chunks.flat_map(|c| c.doc_items.iter().cloned()).collect(),
    }
}

/// The in-flight peer-merge window of [`HybridChunker::chunk_with`]: the
/// window's chunks with their text token counts, the running token estimate
/// of the joined text, the window's headings overhead, and the delimiter's
/// token cost (measured once).
#[derive(Default)]
struct PeerMerger {
    window: Vec<(DocChunk, usize)>,
    running: i64,
    overhead: i64,
    delim: Option<i64>,
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
    /// the `.models/` ONNX files.
    pub const DEFAULT_TOKENIZER_PATH: &str = ".models/chunk/tokenizer.json";

    /// Resolve the tokenizer path for the hybrid chunker: an explicit path
    /// wins; otherwise fall back to [`DEFAULT_TOKENIZER_PATH`] when it exists
    /// on disk. Errors with the download instructions when neither is
    /// available.
    pub fn resolve_tokenizer_path(explicit: Option<&str>) -> Result<String, String> {
        if let Some(p) = explicit {
            return Ok(p.to_string());
        }
        let resolved = crate::assets::resolve(DEFAULT_TOKENIZER_PATH);
        if std::path::Path::new(&resolved).exists() {
            return Ok(resolved);
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

// ---------------------------------------------------------------------------
// Window chunker (feature `chunking`) — docling-rag's Markdown window chunker
// ---------------------------------------------------------------------------

#[cfg(feature = "chunking")]
mod window {
    use super::DocChunk;
    use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

    /// A contiguous run of body text under a heading path.
    #[derive(Debug, Clone, Default)]
    pub struct Section {
        /// The heading stack in effect for this section, outermost first
        /// (e.g. `["Guide", "Setup"]`). Empty for pre-heading / body-only text.
        pub heading_path: Vec<String>,
        /// The plain words of the section body, markup stripped.
        pub words: Vec<String>,
    }

    impl Section {
        /// The heading path rendered as a single context line, e.g.
        /// `# Guide > Setup`. Empty string when there is no heading.
        pub fn heading_context(&self) -> String {
            if self.heading_path.is_empty() {
                String::new()
            } else {
                format!("# {}", self.heading_path.join(" > "))
            }
        }
    }

    fn level_index(level: HeadingLevel) -> usize {
        match level {
            HeadingLevel::H1 => 1,
            HeadingLevel::H2 => 2,
            HeadingLevel::H3 => 3,
            HeadingLevel::H4 => 4,
            HeadingLevel::H5 => 5,
            HeadingLevel::H6 => 6,
        }
    }

    /// Parse Markdown into heading-bounded sections. A new section starts at
    /// every heading; the heading path is maintained as a stack keyed by
    /// heading level.
    pub fn parse_sections(markdown: &str) -> Vec<Section> {
        parse_sections_with_stack(markdown, Vec::new()).0
    }

    /// [`parse_sections`] with an explicit initial heading stack, returning the
    /// final stack — lets a streaming caller carry heading context across
    /// pieces. `heading_stack[i]` holds the current heading text at level `i+1`
    /// (may be empty when a level was skipped).
    pub fn parse_sections_with_stack(
        markdown: &str,
        initial_stack: Vec<String>,
    ) -> (Vec<Section>, Vec<String>) {
        let mut heading_stack: Vec<String> = initial_stack;
        let mut sections: Vec<Section> = Vec::new();
        // Text before the first heading of this piece continues the carried-over
        // section, so it keeps the heading path in effect at the split point.
        let mut current = Section {
            heading_path: heading_stack
                .iter()
                .filter(|h| !h.is_empty())
                .cloned()
                .collect(),
            words: Vec::new(),
        };

        let mut in_heading = false;
        let mut heading_level = 0usize;
        let mut heading_buf = String::new();

        let push_words = |section: &mut Section, text: &str| {
            for w in text.split_whitespace() {
                section.words.push(w.to_string());
            }
        };

        let flush = |sections: &mut Vec<Section>, section: &mut Section| {
            if !section.words.is_empty() {
                sections.push(std::mem::take(section));
            } else {
                *section = Section::default();
            }
        };

        for event in Parser::new(markdown) {
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    in_heading = true;
                    heading_level = level_index(level);
                    heading_buf.clear();
                }
                Event::End(TagEnd::Heading(_)) => {
                    in_heading = false;
                    // Update the heading stack: set this level, drop anything deeper.
                    let idx = heading_level.saturating_sub(1);
                    if heading_stack.len() <= idx {
                        heading_stack.resize(idx + 1, String::new());
                    } else {
                        heading_stack.truncate(idx + 1);
                    }
                    heading_stack[idx] = heading_buf.trim().to_string();
                    // A heading begins a new section.
                    flush(&mut sections, &mut current);
                    current.heading_path = heading_stack
                        .iter()
                        .filter(|h| !h.is_empty())
                        .cloned()
                        .collect();
                }
                Event::Text(t) | Event::Code(t) => {
                    if in_heading {
                        if !heading_buf.is_empty() {
                            heading_buf.push(' ');
                        }
                        heading_buf.push_str(&t);
                    } else {
                        push_words(&mut current, &t);
                    }
                }
                // Treat hard/soft breaks and rules as whitespace (words already split).
                Event::SoftBreak | Event::HardBreak | Event::Rule => {}
                _ => {}
            }
        }
        flush(&mut sections, &mut current);
        (sections, heading_stack)
    }

    /// docling-rag's Markdown **window chunker**: the document is split into
    /// heading-bounded [`Section`]s of plain words (markup stripped), and a
    /// fixed-size window of [`Self::max_words`] words slides over each section
    /// with [`Self::overlap`] fractional overlap. A chunk never crosses a
    /// heading boundary; [`Self::contextualize`] prefixes the heading path.
    #[derive(Debug, Clone)]
    pub struct WindowChunker {
        /// Window size in words (docling-rag's default 300).
        pub max_words: usize,
        /// Fractional overlap between consecutive windows (default 0.05 = 5%).
        pub overlap: f32,
    }

    impl Default for WindowChunker {
        fn default() -> Self {
            WindowChunker {
                max_words: 300,
                overlap: 0.05,
            }
        }
    }

    impl WindowChunker {
        pub fn new(max_words: usize, overlap: f32) -> Self {
            WindowChunker { max_words, overlap }
        }

        /// The window size, kept ≥ 1.
        fn word_budget(&self) -> usize {
            self.max_words.max(1)
        }

        /// Number of words carried from one window into the next; capped so
        /// the window always advances.
        fn overlap_words(&self, budget: usize) -> usize {
            let o = (budget as f32 * self.overlap).round() as usize;
            o.min(budget.saturating_sub(1))
        }

        /// Chunk a Markdown document.
        pub fn chunk(&self, markdown: &str) -> Vec<DocChunk> {
            let mut chunks = Vec::new();
            self.chunk_with(markdown, &mut |c| {
                chunks.push(c);
                true
            });
            chunks
        }

        /// Stream the chunks: `sink` receives each window as it is cut, and a
        /// `false` return cancels. [`Self::chunk`] is this with a collecting
        /// sink — the chunks and their order are identical.
        pub fn chunk_with(&self, markdown: &str, sink: &mut dyn FnMut(DocChunk) -> bool) {
            let (sections, _) = parse_sections_with_stack(markdown, Vec::new());
            for section in &sections {
                if !self.pack_section(section, sink) {
                    return;
                }
            }
        }

        /// Slide the window over one completed section, feeding each chunk to
        /// `sink`. Returns `false` once the sink cancels — exposed so a
        /// streaming caller (docling-rag) can pack sections as they complete.
        pub fn pack_section(
            &self,
            section: &Section,
            sink: &mut dyn FnMut(DocChunk) -> bool,
        ) -> bool {
            let words = &section.words;
            if words.is_empty() {
                return true;
            }
            let budget = self.word_budget();
            let step = budget - self.overlap_words(budget); // ≥ 1 by construction
            let mut start = 0;
            loop {
                let end = (start + budget).min(words.len());
                let chunk = DocChunk {
                    text: words[start..end].join(" "),
                    headings: (!section.heading_path.is_empty())
                        .then(|| section.heading_path.clone()),
                    doc_items: Vec::new(),
                };
                if !sink(chunk) {
                    return false;
                }
                if end >= words.len() {
                    return true;
                }
                start += step;
            }
        }

        /// Render a window chunk for embedding — docling-rag's rendering: the
        /// heading path as a `# Outer > Inner` context line, a blank line,
        /// then the body (just the body above any heading). Note this differs
        /// from the docling chunkers' [`contextualize`](super::contextualize),
        /// matching docling-rag instead.
        pub fn contextualize(chunk: &DocChunk) -> String {
            match &chunk.headings {
                Some(h) if !h.is_empty() => format!("# {}\n\n{}", h.join(" > "), chunk.text),
                _ => chunk.text.clone(),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn splits_on_headings_and_tracks_path() {
            let md = "\
intro words
# Chapter 1
para one
## Section 1.1
para two
# Chapter 2
para three";
            let secs = parse_sections(md);
            // pre-heading intro, Chapter 1, Section 1.1, Chapter 2.
            assert_eq!(secs.len(), 4);
            assert!(secs[0].heading_path.is_empty());
            assert_eq!(secs[1].heading_path, vec!["Chapter 1"]);
            assert_eq!(secs[2].heading_path, vec!["Chapter 1", "Section 1.1"]);
            // A deeper heading is dropped when we return to H1.
            assert_eq!(secs[3].heading_path, vec!["Chapter 2"]);
        }

        #[test]
        fn strips_markup_to_plain_words() {
            let md = "# T\n\nSome **bold** and `code` and [a link](http://x).";
            let secs = parse_sections(md);
            let words = &secs[0].words;
            assert!(words.contains(&"bold".to_string()));
            assert!(words.contains(&"code".to_string()));
            assert!(words.contains(&"link".to_string()));
            // No markdown punctuation survives as its own token.
            assert!(!words.iter().any(|w| w.contains('*') || w.contains('`')));
        }

        #[test]
        fn windows_overlap_and_never_cross_headings() {
            let body: Vec<String> = (0..25).map(|i| format!("w{i}")).collect();
            let md = format!("# A\n\n{}\n\n# B\n\nshort tail\n", body.join(" "));
            let chunker = WindowChunker::new(10, 0.2); // step 8, overlap 2
            let chunks = chunker.chunk(&md);
            // Section A: 25 words → windows [0..10), [8..18), [16..25).
            let a: Vec<_> = chunks
                .iter()
                .filter(|c| c.headings.as_deref() == Some(&["A".to_string()][..]))
                .collect();
            assert_eq!(a.len(), 3);
            assert!(a[0].text.starts_with("w0 ") && a[0].text.ends_with(" w9"));
            assert!(a[1].text.starts_with("w8 "), "overlap carries 2 words");
            assert!(a[2].text.ends_with(" w24"));
            // Section B stays its own chunk; nothing crosses the heading.
            let b: Vec<_> = chunks
                .iter()
                .filter(|c| c.headings.as_deref() == Some(&["B".to_string()][..]))
                .collect();
            assert_eq!(b.len(), 1);
            assert_eq!(b[0].text, "short tail");
            assert_eq!(WindowChunker::contextualize(b[0]), "# B\n\nshort tail");
        }

        #[test]
        fn sink_false_cancels_the_window_walk() {
            let md = format!(
                "# A\n\n{}\n",
                (0..50)
                    .map(|i| format!("w{i}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let chunker = WindowChunker::new(10, 0.0);
            let mut n = 0;
            chunker.chunk_with(&md, &mut |_| {
                n += 1;
                false
            });
            assert_eq!(n, 1);
        }
    }
}

#[cfg(feature = "chunking")]
pub use window::{parse_sections, parse_sections_with_stack, Section, WindowChunker};

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

    /// A page footer is a JSON text item (furniture layer) the chunker never
    /// chunks; the refs of the items after it still match the JSON.
    #[test]
    fn page_furniture_keeps_refs_aligned_with_json() {
        let doc = doc_with(vec![
            Node::Paragraph {
                text: "Before".into(),
            },
            Node::PageFurniture {
                footer: true,
                location: [0, 490, 100, 512],
                text: "1.10.2".into(),
            },
            Node::Paragraph {
                text: "After".into(),
            },
        ]);
        let chunks = HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].text, "After");
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(json["texts"][2]["text"], "After");
        assert_eq!(chunks[1].doc_items[0].self_ref, "#/texts/2");
    }

    /// #1041: a table's caption is the `#/texts/N` item the JSON writes
    /// ahead of the table — it opens the table's chunk (docling's
    /// `TripletTableSerializer`), and the text refs after it stay aligned.
    #[test]
    fn captioned_table_keeps_the_text_refs_aligned() {
        let table = Table {
            rows: vec![
                vec!["Year".into(), "Revenue".into()],
                vec!["2024".into(), "10".into()],
            ],
            caption: Some("Table 1: Revenue".into()),
            ..Table::default()
        };
        let doc = doc_with(vec![
            Node::Table(table.clone()),
            Node::Paragraph {
                text: "Revenue grew in 2024.".into(),
            },
            Node::Table(table),
            Node::Paragraph {
                text: "Costs were flat.".into(),
            },
        ]);
        let chunks = HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks[0].text, "Table 1: Revenue\n\n2024, Revenue = 10");
        let refs: Vec<Vec<&str>> = chunks
            .iter()
            .map(|c| c.doc_items.iter().map(|i| i.self_ref.as_str()).collect())
            .collect();
        assert_eq!(
            refs,
            [
                vec!["#/texts/0", "#/tables/0"],
                vec!["#/texts/1"],
                vec!["#/texts/2", "#/tables/1"],
                vec!["#/texts/3"],
            ]
        );
        let json = doc.export_to_json_value();
        assert_eq!(json["texts"][1]["text"], "Revenue grew in 2024.");
        assert_eq!(json["texts"][3]["text"], "Costs were flat.");
    }

    /// #1041: an item tree numbers the export by its own structure (a
    /// furniture title first, an inline paragraph as runs); the refs are
    /// mapped onto it, so each names the item its chunk was built from.
    #[test]
    fn refs_follow_an_item_tree() {
        use crate::tree::{ItemTree, TreeKind};
        let text = |label: &str, t: &str| TreeKind::Text {
            label: label.into(),
            text: t.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: None,
        };
        let mut doc = doc_with(vec![
            Node::Paragraph {
                text: "Lead **bold** tail".into(),
            },
            Node::Paragraph {
                text: "Second.".into(),
            },
        ]);
        let mut t = ItemTree {
            inline_whitespace: true,
            ..Default::default()
        };
        t.add(
            None,
            Some(crate::ContentLayer::Furniture),
            text("title", "Page"),
        );
        let g = t.add(
            None,
            None,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        );
        t.add(Some(g), None, text("text", "Lead "));
        t.add(Some(g), None, text("text", "bold"));
        t.add(Some(g), None, text("text", " tail"));
        t.add(None, None, text("text", "Second."));
        doc.tree = Some(t);
        let refs: Vec<Vec<String>> = HierarchicalChunker
            .chunk(&doc)
            .iter()
            .map(|c| c.doc_items.iter().map(|i| i.self_ref.clone()).collect())
            .collect();
        assert_eq!(
            refs,
            [
                vec![
                    "#/texts/1".to_string(),
                    "#/texts/2".into(),
                    "#/texts/3".into()
                ],
                vec!["#/texts/4".to_string()],
            ]
        );
    }

    /// A picture's children are never chunked, but they take `#/texts/N`
    /// (and list-group) slots in the JSON — the refs after them must still
    /// point at the right items.
    #[test]
    fn picture_children_are_not_chunked_but_keep_refs_aligned() {
        let doc = doc_with(vec![
            Node::Picture {
                caption: None,
                caption_href: None,
                image: None,
                classification: None,
                description: None,
                caption_parent: crate::CaptionParent::Item,
                caption_location: None,
            },
            Node::PictureChildren(vec![
                Node::Paragraph {
                    text: "axis label".into(),
                },
                Node::ListItem {
                    ordered: false,
                    number: 0,
                    first_in_list: true,
                    text: "callout".into(),
                    level: 0,
                    marker: None,
                    location: None,
                    dclx: None,
                    href: None,
                    layer: None,
                },
            ]),
            Node::Paragraph {
                text: "Body".into(),
            },
        ]);
        let chunks = HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Body");
        let json = doc.export_to_json_value();
        assert_eq!(json["texts"][2]["text"], "Body");
        assert_eq!(chunks[0].doc_items[0].self_ref, "#/texts/2");
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

    /// #1012 (docling-core#924, `test_rich_heading_chunk_metadata`): a
    /// partially formatted heading — an empty heading over an inline group
    /// in docling — is the heading context of the body, not a chunk of its
    /// own under an empty heading.
    #[test]
    fn rich_heading_is_the_heading_context() {
        for body in [Some("Body."), None] {
            let mut nodes = vec![Node::Heading {
                level: 1,
                text: "Hello **bold** world".into(),
            }];
            if let Some(b) = body {
                nodes.push(Node::Paragraph { text: b.into() });
            }
            let doc = doc_with(nodes);
            for chunks in [
                HierarchicalChunker.chunk(&doc),
                HybridChunker::new(WordTok(64)).chunk(&doc),
            ] {
                let got: Vec<_> = chunks
                    .iter()
                    .map(|c| (c.text.as_str(), c.headings.clone()))
                    .collect();
                let want: Vec<_> = body
                    .map(|b| (b, Some(vec!["Hello **bold** world".to_string()])))
                    .into_iter()
                    .collect();
                assert_eq!(got, want);
            }
        }
    }

    /// #1012 (docling-core#924,
    /// `test_rich_heading_is_emitted_before_a_sibling_heading`): a rich
    /// heading shadowed by a sibling leaves no chunk, and the sibling's body
    /// carries only the sibling.
    #[test]
    fn rich_heading_shadowed_by_a_sibling() {
        let doc = doc_with(vec![
            Node::Heading {
                level: 1,
                text: "Energy *E* here".into(),
            },
            Node::Heading {
                level: 1,
                text: "Next".into(),
            },
            Node::Paragraph {
                text: "Body.".into(),
            },
        ]);
        let chunks = HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Body.");
        assert_eq!(chunks[0].headings.as_deref(), Some(&["Next".into()][..]));
        assert_eq!(chunks[0].doc_items[0].self_ref, "#/texts/2");
    }

    /// docling.rs#987 (docling-core#690): table pieces are budgeted at the
    /// window minus the heading path, so `contextualize()` stays in budget.
    #[test]
    fn split_table_pieces_fit_with_headings() {
        let rows: Vec<Vec<String>> = std::iter::once(vec!["".to_string(), "A".into(), "B".into()])
            .chain((0..40).map(|i| vec![format!("R{i}"), format!("{i}"), format!("v{i}")]))
            .collect();
        let doc = doc_with(vec![
            Node::Heading {
                level: 1,
                text: "one two three four five six".into(),
            },
            Node::Table(Table {
                rows,
                ..Default::default()
            }),
        ]);
        let h = HybridChunker::new(WordTok(20));
        let chunks = h.chunk(&doc);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(
                h.count_chunk_tokens(c) <= 20,
                "over budget: {}",
                h.count_chunk_tokens(c)
            );
        }
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

    /// The peer merge before docling-core#823 (#782): rebuild and re-count
    /// the whole window for every chunk. The linear merge must give the
    /// same chunks.
    fn quadratic_merge<T: ChunkTokenizer>(
        h: &HybridChunker<T>,
        chunks: Vec<DocChunk>,
    ) -> Vec<DocChunk> {
        let mut out = Vec::new();
        let mut window: Vec<DocChunk> = Vec::new();
        let flush = |window: &mut Vec<DocChunk>, out: &mut Vec<DocChunk>| match window.len() {
            0 => {}
            1 => out.push(window.pop().unwrap()),
            _ => {
                out.push(merged_chunk(window.iter()));
                window.clear();
            }
        };
        for chunk in chunks {
            if window.is_empty() {
                window.push(chunk);
                continue;
            }
            let candidate = merged_chunk(window.iter().chain([&chunk]));
            if chunk.headings == window[0].headings
                && h.count_chunk_tokens(&candidate) <= h.max_tokens()
            {
                window.push(chunk);
            } else {
                flush(&mut window, &mut out);
                window.push(chunk);
            }
        }
        flush(&mut window, &mut out);
        out
    }

    fn linear_merge<T: ChunkTokenizer>(
        h: &HybridChunker<T>,
        chunks: Vec<DocChunk>,
    ) -> Vec<DocChunk> {
        let mut out = Vec::new();
        let mut sink = |c| {
            out.push(c);
            true
        };
        let mut m = PeerMerger::default();
        for c in chunks {
            assert!(h.merge_push(&mut m, c, &mut sink));
        }
        assert!(h.merge_flush(&mut m, &mut sink));
        out
    }

    /// A non-additive "tokenizer": a token per started 4 characters, so the
    /// joined text counts differently from the sum of its parts and the
    /// delimiter's measured cost is negative — the running estimate is
    /// re-anchored and the final window shrunk back.
    struct QuadCharTok(usize);
    impl ChunkTokenizer for QuadCharTok {
        fn count_tokens(&self, text: &str) -> usize {
            text.chars().count().div_ceil(4)
        }
        fn max_tokens(&self) -> usize {
            self.0
        }
    }

    /// Pseudo-random chunks: short texts of 1–12 words under one of three
    /// heading paths (runs of equal headings, as the walk yields them).
    fn random_chunks(seed: u64, n: usize) -> Vec<DocChunk> {
        let mut state = seed;
        let mut next = move |m: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % m
        };
        let headings = [
            None,
            Some(vec!["A".to_string()]),
            Some(vec!["A".into(), "B".into()]),
        ];
        let mut current = 0;
        (0..n)
            .map(|i| {
                if next(4) == 0 {
                    current = next(3) as usize;
                }
                let words = 1 + next(12) as usize;
                let text = (0..words)
                    .map(|w| ["x", "lorem", "ipsum", "dolor", "sit"][(w + i) % 5])
                    .collect::<Vec<_>>()
                    .join(" ");
                DocChunk {
                    text,
                    headings: headings[current].clone(),
                    doc_items: vec![ChunkItem {
                        self_ref: format!("#/texts/{i}"),
                        kind: ChunkItemKind::Text,
                        text: String::new(),
                    }],
                }
            })
            .collect()
    }

    /// #782 (docling-core#823): the linear peer merge gives exactly the
    /// chunks of the quadratic one, with an additive and a non-additive
    /// tokenizer, over many budgets.
    #[test]
    fn linear_peer_merge_matches_the_quadratic_one() {
        for seed in 0..40 {
            let chunks = random_chunks(seed, 60);
            for max in [3, 8, 16, 40, 200] {
                let h = HybridChunker::new(WordTok(max));
                assert_eq!(
                    linear_merge(&h, chunks.clone()),
                    quadratic_merge(&h, chunks.clone()),
                    "words, seed {seed}, max {max}"
                );
                let h = HybridChunker::new(QuadCharTok(max));
                assert_eq!(
                    linear_merge(&h, chunks.clone()),
                    quadratic_merge(&h, chunks.clone()),
                    "chars/4, seed {seed}, max {max}"
                );
            }
        }
    }

    /// 10k tiny paragraphs at an 8192-token budget — docling-core#823's
    /// benchmark. `cargo test -p docling-core --release -- --ignored
    /// peer_merge_bench --nocapture` prints both timings.
    #[test]
    #[ignore]
    fn peer_merge_bench() {
        let chunks: Vec<DocChunk> = (0..10_000)
            .map(|i| DocChunk {
                text: format!("Paragraph number {i} is short."),
                headings: Some(vec!["Section".into()]),
                doc_items: Vec::new(),
            })
            .collect();
        let h = HybridChunker::new(WordTok(8192));
        let t = std::time::Instant::now();
        let fast = linear_merge(&h, chunks.clone());
        let linear = t.elapsed();
        let t = std::time::Instant::now();
        let slow = quadratic_merge(&h, chunks);
        let quadratic = t.elapsed();
        assert_eq!(fast, slow);
        println!(
            "linear {linear:?}, quadratic {quadratic:?}, {} chunks",
            fast.len()
        );
    }

    /// A table from first-class cells `(text, row, col, row_span,
    /// column_header)`, its grid repeating a spanning cell's text.
    fn cell_table(
        num_rows: usize,
        num_cols: usize,
        cells: &[(&str, usize, usize, usize, bool)],
    ) -> Table {
        let mut rows = vec![vec![String::new(); num_cols]; num_rows];
        for &(text, r, c, span, _) in cells {
            for row in rows.iter_mut().skip(r).take(span) {
                row[c] = text.into();
            }
        }
        Table {
            rows,
            cells: Some(
                cells
                    .iter()
                    .map(|&(text, r, c, span, ched)| crate::TableCell {
                        text: text.into(),
                        bbox: None,
                        start_row: r,
                        start_col: c,
                        row_span: span,
                        col_span: 1,
                        column_header: ched,
                        row_header: false,
                        row_section: false,
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    /// #778 (docling-core#767, 2.102): the Markdown table and the triplet
    /// chunk agree on the header rows — upstream's parametrized
    /// `test_table_header_rows_in_markdown_and_dataframe`, plus the issue's
    /// two tables. Expected texts are docling-core 2.102.0's (compact
    /// Markdown, `HierarchicalChunker` chunk text).
    #[test]
    fn markdown_and_triplets_share_the_header_rows() {
        type Case<'a> = (
            &'a str,
            usize,
            usize,
            Vec<(&'a str, usize, usize, usize, bool)>,
            &'a str,
            &'a str,
        );
        let cases: Vec<Case> = vec![
            (
                "rows under a row-spanning header stay in the body",
                2,
                2,
                vec![("Price", 0, 0, 1, true), ("Category", 0, 1, 2, true), ("10.00", 1, 0, 1, false)],
                "| Price | Category |\n| - | - |\n| 10.00 | Category |",
                "10.00, Category = Category",
            ),
            (
                "a row header flagged as column header stays in the body",
                3,
                3,
                vec![
                    ("Year", 0, 0, 1, true),
                    ("Month", 0, 1, 1, true),
                    ("Revenue", 0, 2, 1, true),
                    ("2025", 1, 0, 2, true),
                    ("January", 1, 1, 1, false),
                    ("$134", 1, 2, 1, false),
                    ("February", 2, 1, 1, false),
                    ("$150", 2, 2, 1, false),
                ],
                "| Year | Month | Revenue |\n| - | - | - |\n| 2025 | January | $134 |\n| 2025 | February | $150 |",
                "2025, Month = January. 2025, Revenue = $134. 2025, Month = February. 2025, Revenue = $150",
            ),
            (
                "stacked header with an unmarked row label",
                3,
                3,
                vec![
                    ("", 0, 0, 1, true),
                    ("% of Total", 0, 1, 1, true),
                    ("% of Total", 0, 2, 1, true),
                    ("class label", 1, 0, 1, false),
                    ("Train", 1, 1, 1, true),
                    ("Test", 1, 2, 1, true),
                    ("Caption", 2, 0, 1, false),
                    ("2.5", 2, 1, 1, false),
                    ("2.4", 2, 2, 1, false),
                ],
                "| class label | % of Total - Train | % of Total - Test |\n| - | - | - |\n| Caption | 2.5 | 2.4 |",
                "Caption, % of Total.Train = 2.5. Caption, % of Total.Test = 2.4",
            ),
            (
                "a partially flagged first row stays the header",
                2,
                2,
                vec![("Name", 0, 0, 1, true), ("Value", 0, 1, 1, false), ("A", 1, 0, 1, false), ("1", 1, 1, 1, false)],
                "| Name | Value |\n| - | - |\n| A | 1 |",
                "A, Value = 1",
            ),
            (
                "no flags: Markdown promotes row 0, triplets do not",
                2,
                2,
                vec![("Name", 0, 0, 1, false), ("Value", 0, 1, 1, false), ("A", 1, 0, 1, false), ("1", 1, 1, 1, false)],
                "| Name | Value |\n| - | - |\n| A | 1 |",
                "Name, 1 = Value. A, 1 = 1",
            ),
            (
                "row headers flagged as column headers stay in the body",
                2,
                2,
                vec![("2025", 0, 0, 1, true), ("$134", 0, 1, 1, false), ("2026", 1, 0, 1, true), ("$150", 1, 1, 1, false)],
                "|  |  |\n| - | - |\n| 2025 | $134 |\n| 2026 | $150 |",
                "2025, 1 = $134. 2026, 1 = $150",
            ),
            (
                "a form with flagged first-column labels is all data",
                3,
                2,
                vec![
                    ("Name", 0, 0, 1, true),
                    ("Bob", 0, 1, 1, false),
                    ("Age", 1, 0, 1, true),
                    ("42", 1, 1, 1, false),
                    ("City", 2, 0, 1, true),
                    ("Paris", 2, 1, 1, false),
                ],
                "|  |  |\n| - | - |\n| Name | Bob |\n| Age | 42 |\n| City | Paris |",
                "Name, 1 = Bob. Age, 1 = 42. City, 1 = Paris",
            ),
            (
                "a partly flagged row 0 above a stacked header row",
                3,
                3,
                vec![
                    ("Region", 0, 0, 1, true),
                    ("2025", 0, 1, 1, false),
                    ("", 0, 2, 1, false),
                    ("", 1, 0, 1, false),
                    ("Q1", 1, 1, 1, true),
                    ("Q2", 1, 2, 1, true),
                    ("North", 2, 0, 1, false),
                    ("1", 2, 1, 1, false),
                    ("2", 2, 2, 1, false),
                ],
                "| Region | 2025 - Q1 | Q2 |\n| - | - | - |\n| North | 1 | 2 |",
                "North, 2025.Q1 = 1. North, Q2 = 2",
            ),
        ];
        for (name, rows, cols, cells, md, triplets) in cases {
            let t = cell_table(rows, cols, &cells);
            assert_eq!(
                crate::markdown::render_table(&t, true),
                md,
                "{name}: markdown"
            );
            assert_eq!(triplet_table_text(&t), triplets, "{name}: triplets");
        }
    }

    /// #777 (docling-core#857, 2.102): a rich header cell spanning two
    /// columns repeats in both — upstream's `col_span_rich_table_doc`. Its
    /// blocks sit on the anchor only and the covered position's flat text is
    /// blank, so LaTeX and the triplets must resolve the anchor themselves.
    #[test]
    fn a_column_spanning_rich_cell_repeats_in_every_column() {
        let mut t = cell_table(
            2,
            3,
            &[
                ("", 0, 0, 1, true),
                ("Partner", 0, 1, 1, true),
                ("Status", 1, 0, 1, false),
                ("Done", 1, 1, 1, false),
                ("Open", 1, 2, 1, false),
            ],
        );
        if let Some(cells) = t.cells.as_mut() {
            cells[1].col_span = 2;
        }
        let mut blocks = vec![vec![Vec::new(); 3]; 2];
        blocks[0][1] = vec![Node::Paragraph {
            text: "Partner".into(),
        }];
        t.cell_blocks = Some(blocks);
        assert_eq!(
            t.rows[0][2], "",
            "the covered position carries no text of its own"
        );
        assert_eq!(
            triplet_table_text(&t),
            "Status, Partner = Done. Status, Partner = Open"
        );
        let latex = crate::latex::render_table(&t);
        assert!(
            latex.contains(" & Partner & Partner \\\\ \\hline\nStatus & Done & Open \\\\ \\hline"),
            "{latex}"
        );
        // Markdown reads the flat text, which the backends now fill.
        t.rows[0][2] = "Partner".into();
        assert_eq!(
            crate::markdown::render_table(&t, true),
            "|  | Partner | Partner |\n| - | - | - |\n| Status | Done | Open |"
        );
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
