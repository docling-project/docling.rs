//! Export a [`DoclingDocument`] to docling-core's native JSON wire format
//! (`DoclingDocument` schema v1.10.0) — the same shape `export_to_dict()` /
//! `save_as_json()` produce in Python docling, and the inverse of the
//! JSON-docling reader.
//!
//! The crate's [`Node`] model bakes Markdown escaping (and inline markers) into
//! its text, whereas docling stores raw text and escapes at render time. We
//! therefore *un-escape* on the way out so a docling-core round-trip
//! (`load_from_json().export_to_markdown()`) reproduces the same Markdown.

use serde_json::{json, Value};

use crate::document::{CaptionParent, ContentLayer, DoclingDocument, Node, Table};

const SCHEMA_VERSION: &str = "1.10.0";

/// docling-core's `CodeLanguageLabel` values (anything else serializes as
/// `unknown`, which the model requires for code items).
const CODE_LANGUAGES: &[&str] = &[
    "Ada",
    "Awk",
    "Bash",
    "bc",
    "C",
    "C#",
    "C++",
    "CMake",
    "COBOL",
    "CSS",
    "Ceylon",
    "Clojure",
    "Crystal",
    "Cuda",
    "Cython",
    "D",
    "Dart",
    "dc",
    "Dockerfile",
    "DocLang",
    "Elixir",
    "Erlang",
    "FORTRAN",
    "Forth",
    "Go",
    "HTML",
    "Haskell",
    "Haxe",
    "Java",
    "JavaScript",
    "JSON",
    "Julia",
    "Kotlin",
    "Latex",
    "Lisp",
    "Lua",
    "Matlab",
    "MoonScript",
    "Nim",
    "OCaml",
    "ObjectiveC",
    "Octave",
    "PHP",
    "Pascal",
    "Perl",
    "Prolog",
    "Python",
    "Racket",
    "Ruby",
    "Rust",
    "SML",
    "SQL",
    "Scala",
    "Scheme",
    "Swift",
    "Tikz",
    "TypeScript",
    "VisualBasic",
    "XML",
    "YAML",
];

/// Map a fence language to docling's `CodeLanguageLabel` (case-insensitive), else
/// `unknown`.
/// docling's `CodeLanguageLabel` for a language name (`unknown` when it is
/// not one docling knows) — the mapping the JSON `code_language` field uses,
/// for a backend that wants to test a hint before storing it.
pub fn code_language_label(lang: &str) -> &'static str {
    code_language(Some(lang))
}

pub(crate) fn code_language(lang: Option<&str>) -> &'static str {
    match lang {
        Some(l) => CODE_LANGUAGES
            .iter()
            .find(|c| c.eq_ignore_ascii_case(l))
            .copied()
            .unwrap_or("unknown"),
        None => "unknown",
    }
}

/// docling-core's `Formatting` model, every field written.
fn formatting_json(f: &crate::tree::Formatting) -> Value {
    json!({
        "bold": f.bold,
        "italic": f.italic,
        "underline": f.underline,
        "strikethrough": f.strikethrough,
        "script": match f.script {
            crate::Script::Baseline => "baseline",
            crate::Script::Sub => "sub",
            crate::Script::Super => "super",
        },
    })
}

/// Build the docling-core JSON object for `doc`.
pub fn to_json(doc: &DoclingDocument) -> Value {
    let mut b = Builder::default();
    // A backend that built docling's item tree ([`crate::tree`]) has already
    // decided every parent, child and creation index; serialize that. The
    // flat nodes are for the other serializers.
    let body = match &doc.tree {
        Some(tree) => b.write_tree(tree),
        None => b.walk_into(&doc.nodes, "#/body"),
    };
    b.link_comments();

    let mut out = json!({
        "schema_name": "DoclingDocument",
        "version": SCHEMA_VERSION,
        "name": doc.name,
        "origin": {
            "mimetype": "text/plain",
            "binary_hash": fnv1a(&doc.name),
            "filename": doc.name,
        },
        "furniture": {
            "self_ref": "#/furniture",
            "children": [],
            "content_layer": "furniture",
            "name": "_root_",
            "label": "unspecified",
        },
        "body": {
            "self_ref": "#/body",
            "children": body,
            "content_layer": "body",
            "name": "_root_",
            "label": "unspecified",
        },
        "groups": b.groups,
        "texts": b.texts,
        "pictures": b.pictures,
        "tables": b.tables,
        "key_value_items": [],
        "form_items": [],
        "pages": b.pages.iter().map(|(n, w, h)| {
            let r2 = |v: f64| (v * 100.0).round() / 100.0;
            (n.to_string(), json!({
                "size": { "width": r2(*w), "height": r2(*h) },
                "page_no": n,
            }))
        }).collect::<serde_json::Map<String, Value>>(),
    });

    // docling only emits `field_regions` / `field_items` when a document has
    // form fields, and places them just before `pages`. Insert them in that slot
    // (re-appending `pages` afterwards, since `preserve_order` keeps insertion
    // order) so non-KVP documents' JSON is byte-identical to before.
    if !b.field_regions.is_empty() {
        if let Some(obj) = out.as_object_mut() {
            let pages = obj.remove("pages");
            obj.insert("field_regions".into(), Value::Array(b.field_regions));
            obj.insert("field_items".into(), Value::Array(b.field_items));
            if let Some(pages) = pages {
                obj.insert("pages".into(), pages);
            }
        }
    }
    out
}

/// A DocumentPictureClassifier's predictions as the picture's `meta` — they
/// land twice, exactly like docling 2.x writes them: the newer
/// `meta.classification` field (pydantic field order: confidence, created_by,
/// class_name) and the deprecated-but-still-emitted `classification`
/// annotation, carried here under an `annotations` key that `add_picture`
/// lifts onto the item.
fn classification_meta(classes: &[crate::PictureClass]) -> Value {
    json!({
        "classification": {
            "predictions": classes.iter().map(|c| json!({
                "confidence": c.confidence as f64,
                "created_by": "DocumentPictureClassifier",
                "class_name": c.class_name,
            })).collect::<Vec<_>>(),
        },
        "annotations": [{
            "kind": "classification",
            "provenance": "DocumentPictureClassifier",
            "predicted_classes": classes.iter().map(|c| json!({
                "class_name": c.class_name,
                "confidence": c.confidence as f64,
            })).collect::<Vec<_>>(),
        }],
    })
}

/// docling's `TableData` for a table: `table_cells`, `num_rows`/`num_cols`
/// and the `grid` that repeats each cell at every position it covers. Shared
/// by table items and a chart picture's `meta.tabular_chart.chart_data`.
/// One table cell as docling's `TableCell` JSON: the eleven fields in
/// pydantic order, `bbox` appended when the cell carries one. Built straight
/// into a `Map` sized for the entries — the table grid repeats every cell
/// object once per spanned slot, so on a table-heavy document (a patent's
/// claims tables, an EBCDIC dump) this constructor and its clones *are* the
/// JSON export's cost; the `json!` macro built the same object through a
/// growing map with a rehash per doubling.
#[allow(clippy::too_many_arguments)]
fn cell_value(
    row_span: usize,
    col_span: usize,
    start_row: usize,
    end_row: usize,
    start_col: usize,
    end_col: usize,
    text: String,
    column_header: bool,
    row_header: bool,
    row_section: bool,
    bbox: Option<[f32; 4]>,
) -> Value {
    let mut m = serde_json::Map::with_capacity(12);
    m.insert("row_span".into(), row_span.into());
    m.insert("col_span".into(), col_span.into());
    m.insert("start_row_offset_idx".into(), start_row.into());
    m.insert("end_row_offset_idx".into(), end_row.into());
    m.insert("start_col_offset_idx".into(), start_col.into());
    m.insert("end_col_offset_idx".into(), end_col.into());
    m.insert("text".into(), Value::String(text));
    m.insert("column_header".into(), column_header.into());
    m.insert("row_header".into(), row_header.into());
    m.insert("row_section".into(), row_section.into());
    m.insert("fillable".into(), false.into());
    if let Some(b) = bbox {
        m.insert(
            "bbox".into(),
            json!({
                "l": b[0], "t": b[1], "r": b[2], "b": b[3],
                "coord_origin": "TOPLEFT",
            }),
        );
    }
    Value::Object(m)
}

/// `TableData` from a table whose cell text is Markdown-flavoured (the flat
/// nodes: escapes to undo, GFM hard-break markers to strip).
fn table_data(t: &Table) -> Value {
    table_data_with(t, false)
}

/// `TableData`; with `raw` the cell text is docling's own raw cell text (a
/// backend-built tree) and is written verbatim.
fn table_data_with(t: &Table, raw: bool) -> Value {
    let cell_text = |s: &str| {
        if raw {
            s.to_string()
        } else {
            unescape_text(&crate::markdown::strip_hard_breaks(s))
        }
    };
    let num_rows = t.rows.len();
    let num_cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut grid = Vec::with_capacity(num_rows);
    let mut cells = Vec::new();
    // Grid slot → index into `cells` (the anchor cell covering it). A flat
    // row-major table instead of a `HashMap<(r, c), Value>` of clones: the
    // grid is filled from it with one clone per slot, and nothing is hashed.
    let mut slot: Vec<Option<usize>> = vec![None; num_rows * num_cols];
    if let Some(first_class) = t.cells.as_ref().filter(|c| !c.is_empty()) {
        for c in first_class {
            let idx = cells.len();
            cells.push(cell_value(
                c.row_span,
                c.col_span,
                c.start_row,
                c.start_row + c.row_span,
                c.start_col,
                c.start_col + c.col_span,
                cell_text(&c.text),
                c.column_header,
                c.row_header,
                c.row_section,
                c.bbox,
            ));
            for r in c.start_row..(c.start_row + c.row_span).min(num_rows) {
                for k in c.start_col..(c.start_col + c.col_span).min(num_cols) {
                    slot[r * num_cols + k] = Some(idx);
                }
            }
        }
        for r in 0..num_rows {
            let mut grid_row = Vec::with_capacity(num_cols);
            for c in 0..num_cols {
                grid_row.push(match slot[r * num_cols + c] {
                    Some(i) => cells[i].clone(),
                    None => cell_value(
                        1,
                        1,
                        r,
                        r + 1,
                        c,
                        c + 1,
                        String::new(),
                        false,
                        false,
                        false,
                        None,
                    ),
                });
            }
            grid.push(grid_row);
        }
    } else {
        let s = t.structure.as_ref();
        let flag = |grid: Option<&Vec<Vec<bool>>>, r: usize, c: usize| -> bool {
            grid.and_then(|g| g.get(r))
                .and_then(|row| row.get(c))
                .copied()
                .unwrap_or(false)
        };
        let anchor_of = |r: usize, c: usize| -> (usize, usize) {
            let (mut r0, mut c0) = (r, c);
            while c0 > 0 && flag(s.map(|s| &s.col_continuation), r, c0) {
                c0 -= 1;
            }
            while r0 > 0 && flag(s.map(|s| &s.row_continuation), r0, c0) {
                r0 -= 1;
            }
            (r0, c0)
        };
        // Each slot's anchor, computed once; the anchor's extent is the
        // farthest slot that resolves to it.
        let anchors: Vec<(usize, usize)> = (0..num_rows)
            .flat_map(|r| (0..num_cols).map(move |c| (r, c)))
            .map(|(r, c)| anchor_of(r, c))
            .collect();
        let mut extent: Vec<(usize, usize)> = (0..num_rows)
            .flat_map(|r| (0..num_cols).map(move |c| (r, c)))
            .collect();
        for (i, &(ar, ac)) in anchors.iter().enumerate() {
            let (r, c) = (i / num_cols.max(1), i % num_cols.max(1));
            let e = &mut extent[ar * num_cols + ac];
            e.0 = e.0.max(r);
            e.1 = e.1.max(c);
        }
        for (r, row) in t.rows.iter().enumerate() {
            let mut grid_row = Vec::with_capacity(num_cols);
            for c in 0..num_cols {
                let (ar, ac) = anchors[r * num_cols + c];
                if (ar, ac) == (r, c) {
                    let (er, ec) = extent[r * num_cols + c];
                    let text = row.get(c).map(|s| cell_text(s)).unwrap_or_default();
                    let column_header = match s.filter(|s| !s.col_header.is_empty()) {
                        Some(s) => flag(Some(&s.col_header), r, c),
                        None => r == 0,
                    };
                    slot[r * num_cols + c] = Some(cells.len());
                    cells.push(cell_value(
                        er - r + 1,
                        ec - c + 1,
                        r,
                        er + 1,
                        c,
                        ec + 1,
                        text,
                        column_header,
                        flag(s.map(|s| &s.row_header), r, c),
                        false,
                        None,
                    ));
                }
                grid_row.push(match slot[ar * num_cols + ac] {
                    Some(i) => cells[i].clone(),
                    None => Value::Null,
                });
            }
            grid.push(grid_row);
        }
    }
    json!({
        "table_cells": cells,
        "num_rows": num_rows,
        "num_cols": num_cols,
        "orientation": "rot_0",
        "grid": grid,
    })
}

#[derive(Default)]
struct Builder {
    texts: Vec<Value>,
    groups: Vec<Value>,
    tables: Vec<Value>,
    pictures: Vec<Value>,
    field_regions: Vec<Value>,
    field_items: Vec<Value>,
    /// Pages seen so far (`page_no`, width, height in points) — from the
    /// [`Node::PageInfo`] markers the PDF paths emit; empty for every other
    /// backend, which keeps their JSON byte-identical (`"pages": {}`, no prov).
    pages: Vec<(usize, f64, f64)>,
    /// The page the walk is currently on (0 before the first marker).
    cur_page: usize,
    cur_w: f64,
    cur_h: f64,
    /// The enclosing [`Node::Located`] wrapper's 0–511 grid box, waiting to be
    /// consumed as the next item's provenance.
    pending_loc: Option<[u16; 4]>,
    /// The enclosing [`Node::Prov`] wrapper's exact `(page_no, bbox,
    /// charspan)`, which takes precedence over the grid box.
    pending_exact: Option<(usize, [f32; 4], [usize; 2])>,
    /// `$ref`s an item wants placed in its parent's `children` *before* its
    /// own — a chart's caption item, which docling's office backends add to
    /// the container ahead of the picture that references it.
    pending_siblings: Vec<Value>,
    /// `$ref`s an item wants placed in its parent's `children` right *after*
    /// its own — an HTML `<figure>`-wrapped table's caption
    /// ([`CaptionParent::ContainerAfter`]).
    pending_after: Vec<Value>,
    /// Caption `$ref`s that hang off `#/body` while their item sits deeper
    /// ([`CaptionParent::Body`]): docling appends them to the body's children
    /// as they are created, so they follow the top-level item being walked.
    pending_body: Vec<Value>,
    /// What each [`Node::CommentSection`] is referenced by, in document order —
    /// its group `$ref`, or its note text's when the section says so. The index
    /// is what a [`Node::Commented`] annotation carries.
    comment_groups: Vec<String>,
    /// Annotated items awaiting their refs: comments are usually emitted
    /// *after* the body they annotate (docx appends them), so the link is
    /// patched in once the whole document has been walked.
    pending_comments: Vec<(String, Vec<usize>)>,
}

impl Builder {
    /// Consume the pending location (if any) into a docling `prov` array: the
    /// 0-511 grid denormalized against the current page into BOTTOMLEFT
    /// points, rounded to 2 decimals like docling's own export. `char_len` is
    /// the item's text length in characters (0 for tables and pictures, whose
    /// charspan docling emits as `[0, 0]`).
    fn take_prov(&mut self, char_len: usize) -> Value {
        let prov = self.prov_json(char_len, false);
        self.pending_exact = None;
        self.pending_loc = None;
        prov
    }

    /// The pending provenance without consuming it. An exact [`Node::Prov`]
    /// box wins over the grid; its own `charspan` is used unless
    /// `span_over_text` asks for `[0, char_len]` (a chart caption's span
    /// covers the caption text where the chart's is `[0, 0]`).
    fn prov_json(&self, char_len: usize, span_over_text: bool) -> Value {
        let r2 = |v: f64| (v * 100.0).round() / 100.0;
        if let Some((page_no, [l, t, r, b], charspan)) = self.pending_exact {
            let charspan = if span_over_text {
                [0, char_len]
            } else {
                charspan
            };
            return json!([{
                "page_no": page_no,
                "bbox": {
                    "l": r2(l as f64), "t": r2(t as f64), "r": r2(r as f64), "b": r2(b as f64),
                    "coord_origin": "TOPLEFT",
                },
                "charspan": charspan,
            }]);
        }
        let Some([x0, y0, x1, y1]) = self.pending_loc else {
            return json!([]);
        };
        // An all-zero grid box is the sentinel for "this item has no geometry"
        // (a slide's speaker notes, say). docling writes a zero bbox for it,
        // not a box spanning the page, which is what denormalizing would give.
        if [x0, y0, x1, y1] == [0, 0, 0, 0] {
            return json!([{
                "page_no": self.cur_page,
                "bbox": { "l": 0.0, "t": 0.0, "r": 0.0, "b": 0.0, "coord_origin": "BOTTOMLEFT" },
                "charspan": [0, char_len],
            }]);
        }
        json!([{
            "page_no": self.cur_page,
            "bbox": {
                "l": r2(x0 as f64 * self.cur_w / 512.0),
                "t": r2(self.cur_h - y0 as f64 * self.cur_h / 512.0),
                "r": r2(x1 as f64 * self.cur_w / 512.0),
                "b": r2(self.cur_h - y1 as f64 * self.cur_h / 512.0),
                "coord_origin": "BOTTOMLEFT",
            },
            "charspan": [0, char_len],
        }])
    }

    /// Adopt a node's own location field (tables, formulas, list items carry
    /// one instead of a [`Node::Located`] wrapper) when no wrapper is pending.
    fn adopt_loc(&mut self, loc: Option<[u16; 4]>) {
        if self.pending_loc.is_none() && self.cur_page > 0 {
            self.pending_loc = loc;
        }
    }

    /// Resolve the [`Node::Commented`] annotations collected during the walk
    /// into docling's `comments: [{"$ref": "#/groups/N"}]` key on the annotated
    /// item. It has to run after the whole walk: docx appends its comment
    /// bodies, so the groups they live in are usually allocated *later* than
    /// the paragraphs pointing at them. docling emits the key between `prov`
    /// and `orig`, so insert it in place rather than appending (serde_json runs
    /// with `preserve_order`, i.e. key order is output order).
    fn link_comments(&mut self) {
        let refs: Vec<(String, Vec<Value>)> = std::mem::take(&mut self.pending_comments)
            .into_iter()
            .map(|(item, comments)| {
                let refs = comments
                    .iter()
                    .filter_map(|i| self.comment_groups.get(*i))
                    .map(|r| json!({ "$ref": r }))
                    .collect();
                (item, refs)
            })
            .collect();
        for (item, comment_refs) in refs {
            if comment_refs.is_empty() {
                continue;
            }
            let Some(target) = self.item_mut(&item) else {
                continue;
            };
            let Some(obj) = target.as_object_mut() else {
                continue;
            };
            let tail: Vec<(String, Value)> = obj
                .iter()
                .skip_while(|(k, _)| k.as_str() != "prov")
                .skip(1)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (k, _) in &tail {
                obj.shift_remove(k);
            }
            obj.insert("comments".into(), Value::Array(comment_refs));
            for (k, v) in tail {
                obj.insert(k, v);
            }
        }
    }

    /// The stored JSON object a `#/texts/N`-style self-ref points at.
    fn item_mut(&mut self, self_ref: &str) -> Option<&mut Value> {
        let idx = ref_index(self_ref)?;
        let bucket = if self_ref.starts_with("#/texts/") {
            &mut self.texts
        } else if self_ref.starts_with("#/tables/") {
            &mut self.tables
        } else if self_ref.starts_with("#/pictures/") {
            &mut self.pictures
        } else if self_ref.starts_with("#/groups/") {
            &mut self.groups
        } else {
            return None;
        };
        bucket.get_mut(idx)
    }

    /// Serialize a backend-built [`ItemTree`](crate::tree::ItemTree): every
    /// item in creation order into its bucket, with the parent / children /
    /// layer the tree recorded. Returns the body's `children` refs.
    fn write_tree(&mut self, tree: &crate::tree::ItemTree) -> Vec<Value> {
        use crate::tree::TreeKind;
        // Every item's `self_ref` first: children may be listed before they
        // are written (a rich cell's group is created after its content).
        let mut refs: Vec<String> = Vec::with_capacity(tree.items.len());
        let (mut nt, mut ng, mut ntb, mut np, mut nf) = (0, 0, 0, 0, 0);
        for item in &tree.items {
            if item.deleted {
                refs.push(String::new());
                continue;
            }
            let r = match &item.kind {
                TreeKind::Text { .. } | TreeKind::Code { .. } => {
                    nt += 1;
                    format!("#/texts/{}", nt - 1)
                }
                TreeKind::Group { .. } => {
                    ng += 1;
                    format!("#/groups/{}", ng - 1)
                }
                TreeKind::Table { .. } => {
                    ntb += 1;
                    format!("#/tables/{}", ntb - 1)
                }
                TreeKind::Picture { .. } => {
                    np += 1;
                    format!("#/pictures/{}", np - 1)
                }
                TreeKind::FieldRegion { items } => {
                    // A region's marker / key / value parts are text items
                    // too, numbered where docling creates them.
                    nt += items
                        .iter()
                        .map(|i| {
                            [&i.marker, &i.key, &i.value]
                                .iter()
                                .filter(|p| p.is_some())
                                .count()
                        })
                        .sum::<usize>();
                    nf += 1;
                    format!("#/field_regions/{}", nf - 1)
                }
            };
            refs.push(r);
        }
        let ref_of = |id: usize| json!({ "$ref": refs[id] });
        for (id, item) in tree.items.iter().enumerate() {
            if item.deleted {
                continue;
            }
            let parent = item.parent.map_or("#/body", |p| refs[p].as_str());
            let children: Vec<Value> = item.children.iter().map(|&c| ref_of(c)).collect();
            let layer = item.layer.map_or("body", |l| l.value());
            let self_ref = match &item.kind {
                TreeKind::Text {
                    label,
                    text,
                    orig,
                    formatting,
                    hyperlink,
                    level,
                    list,
                } => {
                    // docling's field order: …, text, formatting, hyperlink,
                    // then the subclass fields (`level`; `enumerated`, `marker`).
                    let mut tail = serde_json::Map::new();
                    if let Some(f) = formatting {
                        tail.insert("formatting".into(), formatting_json(f));
                    }
                    if let Some(h) = hyperlink {
                        tail.insert("hyperlink".into(), json!(h));
                    }
                    if let Some(l) = level {
                        tail.insert("level".into(), json!(l));
                    }
                    if let Some(l) = list {
                        tail.insert("enumerated".into(), json!(l.enumerated));
                        tail.insert("marker".into(), json!(l.marker));
                    }
                    let r = format!("#/texts/{}", self.texts.len());
                    let mut item_json = json!({
                        "self_ref": r,
                        "parent": { "$ref": parent },
                        "children": children,
                        "content_layer": layer,
                        "label": label,
                        "prov": [],
                    });
                    // docling's `comments` back-refs sit between `prov` and
                    // `orig`, and are written only when set.
                    if !item.comments.is_empty() {
                        item_json["comments"] =
                            Value::Array(item.comments.iter().map(|&c| ref_of(c)).collect());
                    }
                    merge(
                        &mut item_json,
                        json!({
                            "orig": orig.as_deref().unwrap_or(text),
                            "text": text,
                        }),
                    );
                    merge(&mut item_json, Value::Object(tail));
                    self.texts.push(item_json);
                    r
                }
                TreeKind::Code {
                    text,
                    orig,
                    language,
                    formatting,
                    hyperlink,
                } => {
                    let r = format!("#/texts/{}", self.texts.len());
                    let mut item_json = json!({
                        "self_ref": r,
                        "parent": { "$ref": parent },
                        "children": children,
                        "content_layer": layer,
                        "label": "code",
                        "prov": [],
                    });
                    if !item.comments.is_empty() {
                        item_json["comments"] =
                            Value::Array(item.comments.iter().map(|&c| ref_of(c)).collect());
                    }
                    merge(
                        &mut item_json,
                        json!({
                            "orig": orig.as_deref().unwrap_or(text),
                            "text": text,
                        }),
                    );
                    if let Some(f) = formatting {
                        item_json["formatting"] = formatting_json(f);
                    }
                    if let Some(h) = hyperlink {
                        item_json["hyperlink"] = json!(h);
                    }
                    merge(
                        &mut item_json,
                        json!({
                            "captions": [],
                            "references": [],
                            "footnotes": [],
                            "code_language": code_language(language.as_deref()),
                        }),
                    );
                    self.texts.push(item_json);
                    r
                }
                TreeKind::Group { label, name } => {
                    let r = format!("#/groups/{}", self.groups.len());
                    self.groups.push(json!({
                        "self_ref": r,
                        "parent": { "$ref": parent },
                        "children": children,
                        "content_layer": layer,
                        "name": name,
                        "label": label,
                    }));
                    r
                }
                TreeKind::Table {
                    table,
                    rich_cells,
                    captions,
                } => {
                    // docling's raw cell text: nothing to unescape or strip.
                    let r = self.add_table_with(table, parent, true);
                    let idx = ref_index(&r).expect("table ref");
                    let t = &mut self.tables[idx];
                    t["children"] = Value::Array(children);
                    t["content_layer"] = json!(layer);
                    t["captions"] = Value::Array(captions.iter().map(|&c| ref_of(c)).collect());
                    // A `RichTableCell` is the plain cell plus a `ref` to the
                    // group holding its content — on `table_cells` only; the
                    // derived `grid` shows plain cells.
                    for &(row, col, group) in rich_cells {
                        let cell_ref = ref_of(group);
                        let hit = |c: &Value| {
                            c["start_row_offset_idx"] == json!(row)
                                && c["start_col_offset_idx"] == json!(col)
                        };
                        if let Some(cells) = t["data"]["table_cells"].as_array_mut() {
                            for c in cells.iter_mut().filter(|c| hit(c)) {
                                c["ref"] = cell_ref.clone();
                            }
                        }
                    }
                    r
                }
                TreeKind::Picture {
                    captions,
                    image,
                    classification,
                    chart,
                } => {
                    // A chart's meta: the kind as the one classification
                    // prediction, then the reconstructed data grid (#405).
                    let mut meta = classification.as_ref().map(
                        |c| json!({ "classification": { "predictions": [{ "class_name": c }] } }),
                    );
                    if let (Some(m), Some(t)) = (meta.as_mut(), chart) {
                        if !t.rows.is_empty() {
                            m["tabular_chart"] = json!({ "chart_data": table_data(t) });
                        }
                    }
                    let r = self.push_picture(
                        json!([]),
                        captions.iter().map(|&c| ref_of(c)).collect(),
                        children,
                        image.as_ref(),
                        meta,
                        parent,
                    );
                    if let Some(idx) = ref_index(&r) {
                        self.pictures[idx]["content_layer"] = json!(layer);
                    }
                    r
                }
                TreeKind::FieldRegion { items } => {
                    let r = self.add_field_region(items, parent);
                    if let Some(region) = self.field_regions.last_mut() {
                        region["content_layer"] = json!(layer);
                    }
                    r
                }
            };
            debug_assert_eq!(self_ref, refs[id], "tree item {id} numbered out of order");
        }
        tree.body.iter().map(|&c| ref_of(c)).collect()
    }

    fn add_node(&mut self, node: &Node, parent: &str) -> Option<String> {
        match node {
            Node::Heading { level: 1, text } => {
                Some(self.add_text("title", text, parent, json!({})))
            }
            Node::Heading { level, text } => Some(self.add_text(
                "section_header",
                text,
                parent,
                json!({ "level": level.saturating_sub(1) }),
            )),
            Node::Caption { text, href } => {
                let extra = match href {
                    Some(url) => json!({ "hyperlink": url }),
                    None => json!({}),
                };
                Some(self.add_text("caption", text, parent, extra))
            }
            Node::Paragraph { text } => {
                // A whole-paragraph display equation is a formula item (docling
                // wraps it in `$$…$$` and, unlike a text item, never escapes it).
                let t = text.trim();
                match t.strip_prefix("$$").and_then(|s| s.strip_suffix("$$")) {
                    Some(inner) if !inner.is_empty() => Some(self.add_formula(inner, parent)),
                    _ => Some(self.add_text("text", text, parent, json!({}))),
                }
            }
            Node::CheckboxItem { checked, text } => {
                // JSON keeps the task-list form as a plain text item (the
                // `checkbox_selected`/`checkbox_unselected` label is DocLang-only).
                let mark = if *checked { "- [x] " } else { "- [ ] " };
                Some(self.add_text("text", &format!("{mark}{text}"), parent, json!({})))
            }
            Node::Code {
                language,
                text,
                orig,
                ..
            } => Some(self.add_code(text, language.as_deref(), orig.as_deref(), parent)),
            // A CodeFormula-decoded display formula: `text` carries the LaTeX,
            // `orig` the raw glyph extraction (docling's enriched shape).
            Node::Formula {
                latex,
                orig,
                location,
            } => {
                self.adopt_loc(*location);
                Some(self.add_formula_item(latex, orig, parent))
            }
            // docling's notes-layer `comment_section` group holding the
            // comment's text item. What the annotated items point at differs
            // upstream: the docx backend links the group (so a comment's
            // replies group together), everything going through
            // docling-core's `add_comment` links the note text itself.
            Node::CommentSection {
                name,
                text,
                refs_note_text,
                grouped,
            } => {
                if !*grouped {
                    // docling-core's bare `add_comment`: the note text sits
                    // directly under the parent and is what the back-refs
                    // point at.
                    let child =
                        self.add_text("text", text, parent, json!({ "content_layer": "notes" }));
                    self.comment_groups.push(child.clone());
                    return Some(child);
                }
                let self_ref = format!("#/groups/{}", self.groups.len());
                self.groups.push(Value::Null);
                let child =
                    self.add_text("text", text, &self_ref, json!({ "content_layer": "notes" }));
                self.groups[group_index(&self_ref)] = json!({
                    "self_ref": self_ref,
                    "parent": { "$ref": parent },
                    "children": [{ "$ref": child }],
                    "content_layer": "notes",
                    "name": name,
                    "label": "comment_section",
                });
                self.comment_groups.push(if *refs_note_text {
                    child
                } else {
                    self_ref.clone()
                });
                Some(self_ref)
            }
            // The annotation itself is a cross-reference: emit the item, then
            // remember it so the group refs can be filled in at the end.
            Node::Commented { comments, inner } => {
                let item = self.add_node(inner, parent)?;
                if !comments.is_empty() {
                    self.pending_comments.push((item.clone(), comments.clone()));
                }
                Some(item)
            }
            Node::Table(t) => Some(self.add_table(t, parent)),
            Node::Picture {
                caption,
                caption_href,
                image,
                classification,
                caption_parent,
            } => Some(self.add_picture(
                caption.as_deref(),
                caption_href.as_deref(),
                image.as_ref(),
                classification.as_deref().map(classification_meta),
                parent,
                *caption_parent,
            )),
            // A chart is a picture item in the JSON with docling's chart
            // meta — `classification` (the chart kind, as the one prediction)
            // and `tabular_chart.chart_data`, the series reconstructed as a
            // `TableData` (#405) — and no image payload.
            Node::Chart {
                kind,
                table,
                caption,
                location,
            } => {
                self.adopt_loc(*location);
                let mut meta = json!({
                    "classification": { "predictions": [{ "class_name": kind }] },
                });
                if !table.rows.is_empty() {
                    meta["tabular_chart"] = json!({ "chart_data": table_data(table) });
                }
                // docling's office backends add the chart's title as a caption
                // item of the *container* (the sheet group, the slide), listed
                // before the picture that references it, with the chart's own
                // box and a charspan over the caption text — not as a child
                // of the picture, which is where a PDF caption lives.
                let mut captions = Vec::new();
                if let Some(cap) = caption.as_deref().filter(|c| !c.is_empty()) {
                    let prov = self.prov_json(unescape_text(cap).chars().count(), true);
                    let cap_ref = self.add_text_with("caption", cap, parent, json!({}), prov);
                    self.pending_siblings.push(json!({ "$ref": cap_ref }));
                    captions.push(json!({ "$ref": cap_ref }));
                }
                let prov = self.take_prov(0);
                Some(self.push_picture(prov, captions, Vec::new(), None, Some(meta), parent))
            }
            // A DocLang-only node is omitted from the JSON body.
            Node::DoclangOnly(_) => None,
            Node::Group {
                label,
                name,
                layer,
                children,
            } => Some(self.add_group(label, name.as_deref(), *layer, children, parent)),
            Node::FieldRegion { items } => Some(self.add_field_region(items, parent)),
            // A rich inline group is a text item over its Markdown text; the
            // structured runs are DocLang-only, so the JSON matches a paragraph.
            Node::InlineGroup { md_text, .. } => {
                Some(self.add_text("text", md_text, parent, json!({})))
            }
            // A plain-text backend dump is a single text item over the file body.
            Node::TextDump(text) => Some(self.add_text("text", text, parent, json!({}))),
            // Speaker notes are content a deck carries, and docling puts them
            // in the JSON on their own layer, so a consumer reading only JSON
            // can pick them (#402). Page furniture stays out of the flat
            // path; a backend that builds docling's item tree (HTML) puts its
            // furniture-layer items in the JSON through `write_tree`.
            Node::Furniture {
                layer: ContentLayer::Notes,
                inner,
            } => {
                let item = self.add_node(inner, parent)?;
                self.set_layer(&item, "notes");
                Some(item)
            }
            Node::Furniture { .. } => None,
            Node::PageFurniture { .. } => None,
            // A location wrapper turns into the wrapped item's `prov` entry —
            // but only on pages the PDF paths described with a PageInfo marker
            // (other geometry-bearing backends, e.g. PPTX shapes, keep their
            // pre-#171 provenance-less JSON until they emit markers too).
            Node::Located { location, inner } => {
                if self.cur_page > 0 {
                    self.pending_loc = Some(*location);
                }
                let r = self.add_node(inner, parent);
                self.pending_loc = None;
                r
            }
            Node::Prov {
                page_no,
                bbox,
                charspan,
                inner,
                ..
            } => {
                self.pending_exact = Some((*page_no, *bbox, *charspan));
                let r = self.add_node(inner, parent);
                self.pending_exact = None;
                r
            }
            // Page breaks are DocLang-only; docling omits them from the JSON body.
            Node::PageBreak => None,
            // The page marker: record the page's number and size for the
            // `pages` map, and denormalize every following location against it.
            Node::PageInfo {
                page_no,
                width,
                height,
            } => {
                self.cur_page = *page_no;
                self.cur_w = *width as f64;
                self.cur_h = *height as f64;
                if *page_no > 0 {
                    self.pages.push((*page_no, self.cur_w, self.cur_h));
                }
                None
            }
            // Handled by `add_list` in `walk`.
            Node::ListItem { .. } => None,
        }
    }

    /// A form key-value region: `field_regions/N` holds the region, each field is
    /// a `field_items/M` whose children are its `marker` / `field_key` /
    /// `field_value` texts (absent parts are simply omitted).
    fn add_field_region(&mut self, items: &[crate::FieldItem], parent: &str) -> String {
        let self_ref = format!("#/field_regions/{}", self.field_regions.len());
        self.field_regions.push(Value::Null);
        let region_index = self.field_regions.len() - 1;
        let mut item_refs = Vec::new();
        for item in items {
            item_refs.push(json!({ "$ref": self.add_field_item(item, &self_ref) }));
        }
        self.field_regions[region_index] = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": item_refs,
            "content_layer": "body",
            "label": "field_region",
            "prov": [],
        });
        self_ref
    }

    fn add_field_item(&mut self, item: &crate::FieldItem, parent: &str) -> String {
        let self_ref = format!("#/field_items/{}", self.field_items.len());
        self.field_items.push(Value::Null);
        let item_index = self.field_items.len() - 1;
        let mut child_refs = Vec::new();
        for (label, text) in [
            ("marker", &item.marker),
            ("field_key", &item.key),
            ("field_value", &item.value),
        ] {
            if let Some(text) = text {
                // A value's `kind` (docling's `read_only` / `fillable`)
                // follows its text.
                let extra = match (label, &item.value_kind) {
                    ("field_value", Some(kind)) => json!({ "kind": kind }),
                    _ => json!({}),
                };
                child_refs.push(json!({ "$ref": self.add_text(label, text, &self_ref, extra) }));
            }
        }
        self.field_items[item_index] = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": child_refs,
            "content_layer": "body",
            "label": "field_item",
            "prov": [],
        });
        self_ref
    }

    /// Move an already-emitted item onto a content layer. Notes are single
    /// text items today; a deeper notes subtree would need its children moved
    /// too, and no backend builds one.
    fn set_layer(&mut self, self_ref: &str, layer: &str) {
        let bucket = match self_ref.split('/').nth(1) {
            Some("texts") => &mut self.texts,
            Some("tables") => &mut self.tables,
            Some("pictures") => &mut self.pictures,
            Some("groups") => &mut self.groups,
            _ => return,
        };
        if let Some(item) = self_ref
            .rsplit('/')
            .next()
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| bucket.get_mut(i))
        {
            item["content_layer"] = json!(layer);
        }
    }

    fn add_text(&mut self, label: &str, text: &str, parent: &str, extra: Value) -> String {
        let prov = self.take_prov(unescape_text(text).chars().count());
        self.add_text_with(label, text, parent, extra, prov)
    }

    /// [`Self::add_text`] with an explicit `prov` (a chart caption shares the
    /// chart's box without consuming it).
    fn add_text_with(
        &mut self,
        label: &str,
        text: &str,
        parent: &str,
        extra: Value,
        prov: Value,
    ) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let raw = unescape_text(text);
        let mut item = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": label,
            "prov": prov,
            "orig": raw,
            "text": raw,
        });
        merge(&mut item, extra);
        self.texts.push(item);
        self_ref
    }

    /// A display-math formula item. `latex` is the raw content (no `$$`); docling
    /// re-wraps it and never escapes it.
    fn add_formula(&mut self, latex: &str, parent: &str) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let prov = self.take_prov(latex.chars().count());
        self.texts.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "formula",
            "prov": prov,
            "orig": latex,
            "text": latex,
        }));
        self_ref
    }

    /// A CodeFormula-enriched display formula: `text` is the model's LaTeX
    /// while `orig` keeps the raw glyph extraction (docling's enriched shape;
    /// the plain [`Self::add_formula`] above sets both to the same string).
    fn add_formula_item(&mut self, latex: &str, orig: &str, parent: &str) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let prov = self.take_prov(latex.chars().count());
        self.texts.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "formula",
            "prov": prov,
            "orig": orig,
            "text": latex,
        }));
        self_ref
    }

    fn add_code(
        &mut self,
        text: &str,
        language: Option<&str>,
        orig: Option<&str>,
        parent: &str,
    ) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let raw = unescape_text(text);
        let prov = self.take_prov(raw.chars().count());
        self.texts.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "code",
            "prov": prov,
            // With code enrichment, `text` is the model's rewrite while `orig`
            // keeps the raw extraction; otherwise both are the same string.
            "orig": orig.map(unescape_text).unwrap_or_else(|| raw.clone()),
            "text": raw,
            "captions": [],
            "references": [],
            "footnotes": [],
            "code_language": code_language(language),
        }));
        self_ref
    }

    /// Build a list group from a run of (possibly multi-level) list items. A
    /// deeper level starts a nested list under the preceding item.
    fn add_list(&mut self, items: &[Node], parent: &str) -> String {
        let self_ref = format!("#/groups/{}", self.groups.len());
        // reserve the slot so nested groups get later indices
        self.groups.push(Value::Null);
        let base = level_of(&items[0]);
        let mut children = Vec::new();
        let mut i = 0;
        while i < items.len() {
            // Empty paragraphs absorbed into the run (blank lines between items)
            // are not list items — skip them.
            if !matches!(items[i], Node::ListItem { .. }) {
                i += 1;
                continue;
            }
            let lvl = level_of(&items[i]);
            if lvl > base {
                // shouldn't happen at the head; skip defensively
                i += 1;
                continue;
            }
            let item_ref = self.add_list_item(&items[i], &self_ref);
            // collect any deeper items that nest under this one
            let mut j = i + 1;
            while j < items.len() && level_of(&items[j]) > base {
                j += 1;
            }
            if j > i + 1 {
                let mut nested = Vec::new();
                self.add_sibling_lists(&items[i + 1..j], &item_ref, &mut nested);
                // the nested list group(s) are children of this item
                if let Some(idx) = ref_index(&item_ref) {
                    self.texts[idx]["children"]
                        .as_array_mut()
                        .unwrap()
                        .extend(nested);
                }
            }
            children.push(json!({ "$ref": item_ref }));
            i = j;
        }
        self.groups[group_index(&self_ref)] = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": children,
            "content_layer": "body",
            "name": "list",
            "label": "list",
        });
        self_ref
    }

    fn add_list_item(&mut self, node: &Node, parent: &str) -> String {
        let Node::ListItem {
            ordered,
            number,
            text,
            location,
            ..
        } = node
        else {
            unreachable!()
        };
        self.adopt_loc(*location);
        let self_ref = format!("#/texts/{}", self.texts.len());
        let raw = unescape_text(text);
        let prov = self.take_prov(raw.chars().count());
        let marker = if *ordered {
            format!("{number}.")
        } else {
            "-".to_string()
        };
        self.texts.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "list_item",
            "prov": prov,
            "orig": raw,
            "text": raw,
            "enumerated": ordered,
            "marker": marker,
        }));
        self_ref
    }

    fn add_table(&mut self, t: &Table, parent: &str) -> String {
        self.add_table_with(t, parent, false)
    }

    /// [`Self::add_table`]; `raw` cell text is written verbatim (see
    /// [`table_data_with`]).
    fn add_table_with(&mut self, t: &Table, parent: &str, raw: bool) -> String {
        let self_ref = format!("#/tables/{}", self.tables.len());
        self.adopt_loc(t.location);
        let prov = self.take_prov(0);
        // The caption is a separate text item the table references (docling's
        // `TableItem.captions`), added before the grid so its box isn't
        // inherited by a later item.
        let (captions, children) = match t.caption.as_deref().filter(|c| !c.is_empty()) {
            Some(cap) => self.add_caption(cap, json!({}), &self_ref, parent, t.caption_parent),
            None => (Vec::new(), Vec::new()),
        };
        let data = table_data_with(t, raw);
        self.tables.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": children,
            "content_layer": "body",
            "label": "table",
            "prov": prov,
            "captions": captions,
            "references": [],
            "footnotes": [],
            "data": data,
            "annotations": [],
        }));
        self_ref
    }

    /// Add a picture's or table's caption text item where `choice` says it
    /// hangs (#390), returning the `captions` entry for the item and the
    /// item's own `children` (the caption, when it is the item's child).
    /// The caption never consumes the item's pending provenance — the item
    /// takes its box first.
    fn add_caption(
        &mut self,
        text: &str,
        extra: Value,
        self_ref: &str,
        parent: &str,
        choice: CaptionParent,
    ) -> (Vec<Value>, Vec<Value>) {
        // docling's PDF pipeline parents the caption to the item; every
        // declarative backend leaves `add_text`'s default — the body — even
        // for an item inside a group; the office backends and HTML's
        // `<figure>` hang it off the item's container.
        let cap_parent = match choice {
            CaptionParent::Item => self_ref,
            CaptionParent::Container | CaptionParent::ContainerAfter => parent,
            CaptionParent::Body => "#/body",
        };
        let cap_ref = json!({ "$ref": self.add_text("caption", text, cap_parent, extra) });
        match choice {
            CaptionParent::Item => return (vec![cap_ref.clone()], vec![cap_ref]),
            // Created ahead of the item, so it precedes the item in the
            // container's children — and, on the body, in the body's.
            CaptionParent::Container => self.pending_siblings.push(cap_ref.clone()),
            CaptionParent::Body if parent == "#/body" => {
                self.pending_siblings.push(cap_ref.clone())
            }
            CaptionParent::ContainerAfter => self.pending_after.push(cap_ref.clone()),
            // The item sits deeper: the body's children get the caption after
            // the top-level item under walk, where docling appended it.
            CaptionParent::Body => self.pending_body.push(cap_ref.clone()),
        }
        (vec![cap_ref], Vec::new())
    }

    /// `meta` is the picture's docling `PictureMeta` (a classifier's
    /// predictions, a chart's kind and data), `None` for a plain picture.
    fn add_picture(
        &mut self,
        caption: Option<&str>,
        caption_href: Option<&str>,
        image: Option<&crate::PictureImage>,
        meta: Option<Value>,
        parent: &str,
        caption_parent: CaptionParent,
    ) -> String {
        let self_ref = format!("#/pictures/{}", self.pictures.len());
        // Take the picture's own provenance before the caption text is added —
        // the caption is a separate item and must not inherit the crop's box.
        let prov = self.take_prov(0);
        let (captions, children) = match caption.filter(|c| !c.is_empty()) {
            Some(cap) => {
                // Emit the caption as a text item that the picture references. A
                // wrapping `<a href>`'s link rides as docling's `hyperlink` field
                // on the caption item (#328).
                let extra = match caption_href {
                    Some(href) => json!({ "hyperlink": href }),
                    None => json!({}),
                };
                self.add_caption(cap, extra, &self_ref, parent, caption_parent)
            }
            None => (Vec::new(), Vec::new()),
        };
        self.push_picture(prov, captions, children, image, meta, parent)
    }

    /// Append the picture item itself — `prov`, `captions` and `children`
    /// (a PDF caption is the picture's child) already settled.
    fn push_picture(
        &mut self,
        prov: Value,
        captions: Vec<Value>,
        children: Vec<Value>,
        image: Option<&crate::PictureImage>,
        meta: Option<Value>,
        parent: &str,
    ) -> String {
        let self_ref = format!("#/pictures/{}", self.pictures.len());
        // The legacy `classification` annotation rides along with a
        // classifier's `meta` (see `classification_meta`); a chart's meta has
        // none, like docling's.
        let annotations = meta
            .as_ref()
            .and_then(|m| m.get("annotations").cloned())
            .unwrap_or_else(|| json!([]));
        let meta = meta.map(|mut m| {
            if let Some(obj) = m.as_object_mut() {
                obj.remove("annotations");
            }
            m
        });
        // `meta` sits between `content_layer` and `label` in docling's field
        // order (and `preserve_order` keeps ours byte-compatible), so the item
        // is built in one shot per shape rather than patched afterwards.
        let mut item = match meta {
            Some(meta) => json!({
                "self_ref": self_ref,
                "parent": { "$ref": parent },
                "children": children,
                "content_layer": "body",
                "meta": meta,
                "label": "picture",
                "prov": prov,
                "captions": captions,
                "references": [],
                "footnotes": [],
                "annotations": annotations,
            }),
            None => json!({
                "self_ref": self_ref,
                "parent": { "$ref": parent },
                "children": children,
                "content_layer": "body",
                "label": "picture",
                "prov": prov,
                "captions": captions,
                "references": [],
                "footnotes": [],
                "annotations": annotations,
            }),
        };
        // docling stores the extracted image as an `ImageRef` (data URI + size,
        // the size as floats) between `footnotes` and `annotations` — pydantic
        // field order, which `preserve_order` lets us reproduce by rebuilding
        // the tail.
        if let Some(img) = image {
            let image = json!({
                "mimetype": img.mimetype,
                "dpi": 72,
                "size": { "width": img.width as f64, "height": img.height as f64 },
                "uri": img.data_uri(),
            });
            if let Some(obj) = item.as_object_mut() {
                let annotations = obj.remove("annotations").unwrap_or_else(|| json!([]));
                obj.insert("image".into(), image);
                obj.insert("annotations".into(), annotations);
            }
        }
        self.pictures.push(item);
        self_ref
    }

    fn add_group(
        &mut self,
        label: &str,
        name: Option<&str>,
        layer: Option<ContentLayer>,
        nodes: &[Node],
        parent: &str,
    ) -> String {
        let self_ref = format!("#/groups/{}", self.groups.len());
        self.groups.push(Value::Null);
        // Everything the walk creates belongs to this group, so a non-body
        // layer (a hidden sheet) is stamped on the whole subtree afterwards —
        // docling puts the layer on the group *and* on every item under it.
        let mark = (
            self.texts.len(),
            self.tables.len(),
            self.pictures.len(),
            self.groups.len(),
        );
        let children = self.walk_into(nodes, &self_ref);
        let name = name.unwrap_or(if label == "inline" { "group" } else { label });
        let content_layer = layer.map_or("body", |l| l.value());
        self.groups[group_index(&self_ref)] = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": children,
            "content_layer": content_layer,
            "name": name,
            "label": label,
        });
        if layer.is_some() {
            let (t, tb, p, g) = mark;
            for item in self.texts[t..]
                .iter_mut()
                .chain(self.tables[tb..].iter_mut())
                .chain(self.pictures[p..].iter_mut())
                .chain(self.groups[g..].iter_mut())
            {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("content_layer".into(), json!(content_layer));
                }
            }
        }
        self_ref
    }

    /// Walk a slice of sibling nodes, returning each child's `$ref`; runs of
    /// list items are folded into list groups (one per sibling list).
    fn walk_into(&mut self, nodes: &[Node], parent: &str) -> Vec<Value> {
        // Siblings that all carry a creation rank (an XLSX sheet's items) are
        // *added* in that order — so `#/tables/N` and friends are numbered as
        // docling numbers them — while their refs keep the node order, which
        // is docling's position-sorted `children`.
        let seqs: Option<Vec<usize>> = nodes
            .iter()
            .map(|n| match n {
                Node::Prov { seq: Some(s), .. } => Some(*s),
                _ => None,
            })
            .collect();
        if let Some(seqs) = seqs.filter(|s| !s.is_empty()) {
            let mut order: Vec<usize> = (0..nodes.len()).collect();
            order.sort_by_key(|&i| seqs[i]);
            let mut slots: Vec<Vec<Value>> = vec![Vec::new(); nodes.len()];
            for i in order {
                if let Some(r) = self.add_node(&nodes[i], parent) {
                    slots[i].append(&mut self.pending_siblings);
                    slots[i].push(json!({ "$ref": r }));
                    slots[i].append(&mut self.pending_after);
                }
                if parent == "#/body" {
                    slots[i].append(&mut self.pending_body);
                }
            }
            return slots.into_iter().flatten().collect();
        }
        let mut children = Vec::new();
        let mut i = 0;
        while i < nodes.len() {
            if matches!(nodes[i], Node::ListItem { .. }) {
                let start = i;
                i += 1;
                loop {
                    match nodes.get(i) {
                        Some(Node::ListItem { .. }) => i += 1,
                        // Absorb an empty paragraph sitting between two list
                        // items (docling keeps the ListGroup contiguous).
                        Some(Node::Paragraph { text })
                            if text.is_empty()
                                && matches!(nodes.get(i + 1), Some(Node::ListItem { .. })) =>
                        {
                            i += 1
                        }
                        _ => break,
                    }
                }
                self.add_sibling_lists(&nodes[start..i], parent, &mut children);
            } else {
                if let Some(r) = self.add_node(&nodes[i], parent) {
                    children.append(&mut self.pending_siblings);
                    children.push(json!({ "$ref": r }));
                    children.append(&mut self.pending_after);
                }
                i += 1;
            }
            // Body-parented captions of items deeper in the tree follow the
            // top-level item they were created under (#390).
            if parent == "#/body" {
                children.append(&mut self.pending_body);
            }
        }
        children
    }

    /// A run of list items may hold several *sibling* lists; emit one list group
    /// per sibling. The boundary is the backend's `first_in_list` flag on a
    /// base-level item — the same rule as the Markdown serializer's blank line
    /// (#385; the kind-flip and number-gap guesses are gone).
    fn add_sibling_lists(&mut self, run: &[Node], parent: &str, out: &mut Vec<Value>) {
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
                continue; // nested item — handled inside add_list
            }
            if k > seg && *first_in_list {
                out.push(json!({ "$ref": self.add_list(&run[seg..k], parent) }));
                seg = k;
            }
        }
        out.push(json!({ "$ref": self.add_list(&run[seg..], parent) }));
    }
}

fn level_of(node: &Node) -> u8 {
    match node {
        Node::ListItem { level, .. } => *level,
        _ => 0,
    }
}

fn group_index(self_ref: &str) -> usize {
    self_ref.rsplit('/').next().unwrap().parse().unwrap()
}

fn ref_index(self_ref: &str) -> Option<usize> {
    self_ref.rsplit('/').next()?.parse().ok()
}

/// Merge the key/values of `extra` (an object) into `target` (an object).
fn merge(target: &mut Value, extra: Value) {
    if let (Some(t), Some(e)) = (target.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            t.insert(k.clone(), v.clone());
        }
    }
}

/// Reverse [`crate`]'s Markdown text escaping (HTML entities + `\_`).
fn unescape_text(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("\\_", "_")
}

/// 64-bit FNV-1a, a stand-in for docling's `binary_hash` (we lack the source bytes
/// at export time; the value only needs to be a stable u64).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use crate::{
        CaptionParent, ContentLayer, DoclingDocument, ImageMode, Node, PictureImage, Table,
    };
    use serde_json::Value;

    fn doc_with_image() -> DoclingDocument {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Picture {
            caption: Some("Fig 1".into()),
            caption_href: None,
            image: Some(PictureImage {
                mimetype: "image/png".into(),
                width: 4,
                height: 2,
                data: b"foobar".to_vec(),
            }),
            classification: None,
            caption_parent: Default::default(),
        });
        doc
    }

    /// #402: a deck's speaker notes are content, and docling puts them in the
    /// JSON on the `notes` layer so a consumer reading only JSON can pick them
    /// out. Markdown still serializes the body layer alone, and page furniture
    /// stays out of the JSON, where docling does keep it.
    #[test]
    fn notes_layer_items_reach_the_json_but_furniture_does_not() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Heading {
            level: 1,
            text: "Slide One".into(),
        });
        doc.push(Node::Furniture {
            layer: ContentLayer::Notes,
            inner: Box::new(Node::Located {
                location: [0, 0, 0, 0],
                inner: Box::new(Node::Paragraph {
                    text: "Speaker note for slide 1.".into(),
                }),
            }),
        });
        doc.push(Node::Furniture {
            layer: ContentLayer::Furniture,
            inner: Box::new(Node::Paragraph {
                text: "page header".into(),
            }),
        });

        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = v["texts"].as_array().unwrap();
        assert_eq!(
            texts
                .iter()
                .map(|t| (
                    t["label"].as_str().unwrap(),
                    t["content_layer"].as_str().unwrap(),
                    t["text"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("title", "body", "Slide One"),
                ("text", "notes", "Speaker note for slide 1."),
            ],
            "the note is carried on its own layer; the furniture is not carried"
        );
        // The body layer is what Markdown serializes, so it does not change.
        assert_eq!(doc.export_to_markdown(), "# Slide One\n");
    }

    /// #410: a backend that describes merged ranges only as continuation
    /// flags (xlsx `<mergeCell>`, docx `gridSpan`/`vMerge`) gets docling's
    /// one-`TableCell`-per-range JSON: the anchor's offsets and spans, the
    /// entry repeated across the grid positions it covers — not a 1×1 cell
    /// per position with the text copied into each.
    #[test]
    fn continuation_flags_become_spanning_cells() {
        let mut doc = DoclingDocument::new("t");
        // A1:C2 merged ("merged"), then a plain row underneath.
        let rows = vec![
            vec!["merged".to_string(), "merged".into(), "merged".into()],
            vec!["merged".to_string(), "merged".into(), "merged".into()],
            vec!["a".to_string(), "b".into(), "c".into()],
        ];
        doc.push(Node::Table(crate::Table {
            rows,
            location: None,
            structure: Some(crate::TableStructure {
                header_row: vec![true, false, false],
                col_continuation: vec![
                    vec![false, true, true],
                    vec![false, true, true],
                    vec![false, false, false],
                ],
                row_continuation: vec![
                    vec![false, false, false],
                    vec![true, true, true],
                    vec![false, false, false],
                ],
                row_header: Vec::new(),
                col_header: Vec::new(),
            }),
            cell_blocks: None,
            cells: None,
            caption: None,
            caption_parent: Default::default(),
        }));
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let data = &v["tables"][0]["data"];
        assert_eq!(data["num_rows"], 3);
        assert_eq!(data["num_cols"], 3);
        let cells = data["table_cells"].as_array().unwrap();
        assert_eq!(
            cells.len(),
            4,
            "one cell for the range, three for the plain row"
        );
        assert_eq!(
            cells[0],
            serde_json::json!({
                "row_span": 2, "col_span": 3,
                "start_row_offset_idx": 0, "end_row_offset_idx": 2,
                "start_col_offset_idx": 0, "end_col_offset_idx": 3,
                "text": "merged", "column_header": true, "row_header": false,
                "row_section": false, "fillable": false,
            })
        );
        assert_eq!(cells[1]["text"], "a");
        assert_eq!(cells[1]["row_span"], 1);
        assert_eq!(cells[1]["column_header"], false);
        // The grid repeats the range's entry at every position it covers.
        let grid = data["grid"].as_array().unwrap();
        assert_eq!(grid.len(), 3);
        for (r, row) in grid.iter().take(2).enumerate() {
            for (c, cell) in row.as_array().unwrap().iter().enumerate() {
                assert_eq!(*cell, cells[0], "grid[{r}][{c}]");
            }
        }
        assert_eq!(grid[2][2]["text"], "c");
    }

    /// A [`Node::Prov`] wrapper is docling's provenance verbatim — the exact
    /// box in a top-left origin, the backend's charspan — and the page marker
    /// before it sizes the page; a chart's caption becomes a sibling of the
    /// picture in the container, listed first, sharing the chart's box with
    /// a charspan over its text. That is the JSON shape of an XLSX sheet.
    #[test]
    fn exact_provenance_pages_and_chart_captions_follow_docling() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 3.0,
            height: 4.0,
        });
        let table = crate::Table {
            rows: vec![vec!["a".to_string(), "b".into()]],
            ..Default::default()
        };
        doc.push(Node::Group {
            label: "sheet".into(),
            name: Some("Data".into()),
            layer: None,
            children: vec![
                // Node order is the position-sorted one; creation order (the
                // `seq`) had the chart first — so the chart is `#/pictures/0`
                // *and* its caption `#/texts/0`, while the table stays the
                // group's first child.
                Node::Prov {
                    page_no: 1,
                    bbox: [0.0, 0.0, 3.0, 4.0],
                    charspan: [0, 0],
                    seq: Some(1),
                    inner: Box::new(Node::Table(table.clone())),
                },
                Node::Prov {
                    page_no: 1,
                    bbox: [0.0, 1.0, 1.0, 1.0],
                    charspan: [0, 0],
                    seq: Some(0),
                    inner: Box::new(Node::Chart {
                        kind: "bar_chart".into(),
                        table,
                        caption: Some("Sales".into()),
                        location: Some([0, 128, 170, 128]),
                    }),
                },
            ],
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(
            v["pages"],
            serde_json::json!({"1": {"size": {"width": 3.0, "height": 4.0}, "page_no": 1}})
        );
        assert_eq!(
            v["tables"][0]["prov"],
            serde_json::json!([{
                "page_no": 1,
                "bbox": {"l": 0.0, "t": 0.0, "r": 3.0, "b": 4.0, "coord_origin": "TOPLEFT"},
                "charspan": [0, 0],
            }])
        );
        assert_eq!(v["tables"][0]["data"]["orientation"], "rot_0");
        // The caption is the group's child *before* the picture, parented to
        // the group, and referenced by the picture.
        let sheet = &v["groups"][0];
        assert_eq!(
            sheet["children"],
            serde_json::json!([
                {"$ref": "#/tables/0"}, {"$ref": "#/texts/0"}, {"$ref": "#/pictures/0"}
            ])
        );
        let cap = &v["texts"][0];
        assert_eq!(cap["label"], "caption");
        assert_eq!(cap["parent"], serde_json::json!({"$ref": "#/groups/0"}));
        assert_eq!(cap["prov"][0]["charspan"], serde_json::json!([0, 5]));
        assert_eq!(cap["prov"][0]["bbox"]["b"], 1.0);
        let pic = &v["pictures"][0];
        assert_eq!(pic["captions"], serde_json::json!([{"$ref": "#/texts/0"}]));
        assert_eq!(pic["prov"][0]["charspan"], serde_json::json!([0, 0]));
        assert_eq!(pic["prov"][0]["bbox"]["coord_origin"], "TOPLEFT");
        assert_eq!(
            pic["meta"]["classification"]["predictions"][0]["class_name"],
            "bar_chart"
        );
        assert_eq!(pic["meta"]["tabular_chart"]["chart_data"]["num_cols"], 2);
    }

    /// An all-zero location is the "no geometry" sentinel — a slide's speaker
    /// notes carry one — and docling writes it as a zero bbox, not as a box
    /// spanning the whole page, which is what denormalizing the grid gives.
    #[test]
    fn a_zero_location_is_a_zero_bbox_not_the_whole_page() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 12192000.0,
            height: 6858000.0,
        });
        doc.push(Node::Furniture {
            layer: ContentLayer::Notes,
            inner: Box::new(Node::Located {
                location: [0, 0, 0, 0],
                inner: Box::new(Node::Paragraph {
                    text: "a note".into(),
                }),
            }),
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let prov = &v["texts"][0]["prov"][0];
        assert_eq!(prov["page_no"], 1);
        assert_eq!(prov["charspan"], serde_json::json!([0, 6]));
        assert_eq!(
            prov["bbox"],
            serde_json::json!({"l": 0.0, "t": 0.0, "r": 0.0, "b": 0.0, "coord_origin": "BOTTOMLEFT"})
        );
        // The page itself is recorded at its true size.
        assert_eq!(
            v["pages"]["1"]["size"],
            serde_json::json!({"width": 12192000.0, "height": 6858000.0})
        );
    }

    /// #171: PageInfo markers become the `pages` map, and `Located` wrappers /
    /// node-level locations become per-item `prov` — the 0–511 grid
    /// denormalized against the page into BOTTOMLEFT points. Without markers
    /// (every declarative backend) the JSON stays exactly as before: empty
    /// `pages`, `prov: []` even for located nodes.
    #[test]
    fn page_markers_produce_pages_and_prov() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 512.0,
            height: 1024.0,
        });
        doc.push(Node::Located {
            location: [128, 64, 256, 128], // quarter/eighth points of the grid
            inner: Box::new(Node::Paragraph {
                text: "hello".into(),
            }),
        });
        doc.push(Node::Table(Table {
            rows: vec![vec!["a".into()]],
            location: Some([0, 0, 512, 512]),
            ..Table::default()
        }));
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(v["pages"]["1"]["page_no"], 1);
        assert_eq!(v["pages"]["1"]["size"]["width"], 512.0);
        assert_eq!(v["pages"]["1"]["size"]["height"], 1024.0);
        // 512-wide page: grid x scales 1:1; 1024-high: grid y doubles, then
        // flips to the BOTTOMLEFT origin (t from grid-top 64 → 1024-128=896).
        let prov = &v["texts"][0]["prov"][0];
        assert_eq!(prov["page_no"], 1);
        assert_eq!(prov["bbox"]["l"], 128.0);
        assert_eq!(prov["bbox"]["t"], 896.0);
        assert_eq!(prov["bbox"]["r"], 256.0);
        assert_eq!(prov["bbox"]["b"], 768.0);
        assert_eq!(prov["bbox"]["coord_origin"], "BOTTOMLEFT");
        assert_eq!(prov["charspan"][1], 5);
        // The table adopts its own location field; charspan is [0, 0].
        let tprov = &v["tables"][0]["prov"][0];
        assert_eq!(tprov["bbox"]["t"], 1024.0);
        assert_eq!(tprov["bbox"]["b"], 0.0);
        assert_eq!(tprov["charspan"][1], 0);

        // No markers → the pre-#171 shape, byte for byte.
        let mut plain = DoclingDocument::new("t");
        plain.push(Node::Located {
            location: [1, 2, 3, 4],
            inner: Box::new(Node::Paragraph { text: "x".into() }),
        });
        let v: Value = serde_json::from_str(&plain.export_to_json()).unwrap();
        assert_eq!(v["pages"], serde_json::json!({}));
        assert_eq!(v["texts"][0]["prov"], serde_json::json!([]));
    }

    #[test]
    fn picture_image_in_markdown_modes_and_json() {
        let doc = doc_with_image();
        // placeholder (default) ignores the image
        assert!(doc.export_to_markdown().contains("<!-- image -->"));
        // embedded → base64 data URI (b"foobar" → "Zm9vYmFy")
        let (md, files) = doc.export_to_markdown_with_images(ImageMode::Embedded, "artifacts");
        assert!(
            md.contains("![Image](data:image/png;base64,Zm9vYmFy)"),
            "got:\n{md}"
        );
        assert!(files.is_empty());
        // referenced → file link + collected bytes
        let (md, files) = doc.export_to_markdown_with_images(ImageMode::Referenced, "artifacts");
        assert!(
            md.contains("![Image](artifacts/image_000000.png)"),
            "got:\n{md}"
        );
        assert_eq!(
            files,
            vec![("artifacts/image_000000.png".to_string(), b"foobar".to_vec())]
        );
        // JSON carries the ImageRef (data URI + size — floats, as docling's
        // `Size` is — placed before `annotations`).
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(v["pictures"][0]["image"]["mimetype"], "image/png");
        assert_eq!(v["pictures"][0]["image"]["size"]["width"], 4.0);
        let keys: Vec<&str> = v["pictures"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(&keys[keys.len() - 2..], ["image", "annotations"]);
        assert_eq!(
            v["pictures"][0]["image"]["uri"],
            "data:image/png;base64,Zm9vYmFy"
        );
    }

    #[test]
    fn exports_docling_schema() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Heading {
            level: 1,
            text: "Title".into(),
        });
        doc.push(Node::Heading {
            level: 2,
            text: "Sec".into(),
        });
        doc.push(Node::Paragraph {
            text: "Body &amp; more".into(),
        }); // markdown-escaped
        doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: true,
            text: "one".into(),
            level: 0,
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: false,
            text: "two".into(),
            level: 0,
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        doc.push(Node::Table(Table {
            rows: vec![vec!["A".into(), "B".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
            caption_parent: Default::default(),
        }));

        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(v["schema_name"], "DoclingDocument");
        assert_eq!(v["version"], "1.10.0");
        assert_eq!(v["texts"][0]["label"], "title");
        assert_eq!(v["texts"][1]["label"], "section_header");
        assert_eq!(v["texts"][1]["level"], 1); // heading level 2 → docling level 1
        assert_eq!(v["texts"][2]["text"], "Body & more"); // un-escaped for the wire format
                                                          // consecutive list items fold into one list group, parented to it
        assert_eq!(v["groups"][0]["label"], "list");
        assert_eq!(v["groups"][0]["children"].as_array().unwrap().len(), 2);
        assert_eq!(v["texts"][3]["parent"]["$ref"], "#/groups/0");
        assert_eq!(v["texts"][3]["marker"], "-");
        // table grid + header flag
        assert_eq!(v["tables"][0]["data"]["num_cols"], 2);
        assert_eq!(v["tables"][0]["data"]["grid"][0][0]["column_header"], true);
    }
    /// A named group on a non-body layer — docling's hidden spreadsheet sheet:
    /// the group carries the sheet's name and the `invisible` layer, and every
    /// item inside it carries the layer too.
    #[test]
    fn a_layered_group_stamps_its_whole_subtree() {
        let doc = DoclingDocument {
            name: "s".into(),
            nodes: vec![
                Node::Group {
                    label: "sheet".into(),
                    name: Some("Sheet1".into()),
                    layer: None,
                    children: vec![Node::Paragraph {
                        text: "visible".into(),
                    }],
                },
                Node::Group {
                    label: "sheet".into(),
                    name: Some("Sheet2".into()),
                    layer: Some(ContentLayer::Invisible),
                    children: vec![Node::Paragraph {
                        text: "hidden".into(),
                    }],
                },
            ],
            ..DoclingDocument::new("s")
        };
        let v = crate::json::to_json(&doc);
        assert_eq!(v["groups"][0]["label"], "sheet");
        assert_eq!(v["groups"][0]["name"], "Sheet1");
        assert_eq!(v["groups"][0]["content_layer"], "body");
        assert_eq!(v["texts"][0]["content_layer"], "body");
        assert_eq!(v["groups"][1]["name"], "Sheet2");
        assert_eq!(v["groups"][1]["content_layer"], "invisible");
        assert_eq!(v["texts"][1]["content_layer"], "invisible");
        // The group's children are the items, and the body holds the groups.
        assert_eq!(v["groups"][1]["children"][0]["$ref"], "#/texts/1");
        assert_eq!(v["body"]["children"][1]["$ref"], "#/groups/1");
    }

    /// A backend-built item tree is serialized as it is: items numbered in
    /// creation order per bucket (a field region's part texts included), the
    /// tree's parents / children / layers, docling's field order for
    /// `formatting`, `hyperlink`, `level`, `enumerated`/`marker`, a rich
    /// cell's `ref` on `table_cells` only, raw cell text.
    /// The DOCX tree's extras: an item `delete`d (docling's `delete_items`,
    /// the spacer between two items of a resumed list) is neither written nor
    /// numbered, `comments` back-refs sit between `prov` and `orig`, and a
    /// chart picture carries `classification` plus `tabular_chart`.
    #[test]
    fn deleted_items_comment_refs_and_chart_meta_in_the_tree() {
        use crate::tree::{ItemTree, TreeKind};
        let mut t = ItemTree::default();
        let text = |txt: &str| TreeKind::Text {
            label: "text".into(),
            text: txt.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: None,
        };
        let a = t.add(None, None, text("a"));
        let blank = t.add(None, None, text(""));
        let b = t.add(None, None, text("b"));
        t.delete(blank);
        let group = t.add(
            None,
            Some(ContentLayer::Notes),
            TreeKind::Group {
                label: "comment_section".into(),
                name: "comment-0".into(),
            },
        );
        t.add(Some(group), Some(ContentLayer::Notes), text("note"));
        t.items[a].comments.push(group);
        t.add(
            None,
            None,
            TreeKind::Picture {
                captions: Vec::new(),
                image: None,
                classification: Some("bar_chart".into()),
                chart: Some(Table {
                    rows: vec![vec!["".into(), "s".into()], vec!["c".into(), "1".into()]],
                    ..Table::default()
                }),
            },
        );
        assert_eq!(t.last_text(), Some(4), "the note; the blank is skipped");
        assert_eq!(t.bucket_index(b), 1, "numbered past the deleted item");
        let mut doc = DoclingDocument::new("t");
        doc.tree = Some(t);
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = v["texts"].as_array().unwrap();
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[1]["text"], "b");
        assert_eq!(texts[1]["self_ref"], "#/texts/1");
        assert_eq!(
            v["body"]["children"],
            serde_json::json!([{"$ref": "#/texts/0"}, {"$ref": "#/texts/1"}, {"$ref": "#/groups/0"}, {"$ref": "#/pictures/0"}])
        );
        let keys: Vec<&str> = texts[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "self_ref",
                "parent",
                "children",
                "content_layer",
                "label",
                "prov",
                "comments",
                "orig",
                "text"
            ]
        );
        assert_eq!(
            texts[0]["comments"],
            serde_json::json!([{"$ref": "#/groups/0"}])
        );
        assert!(texts[1].get("comments").is_none());
        let meta = &v["pictures"][0]["meta"];
        assert_eq!(
            meta["classification"]["predictions"][0]["class_name"],
            "bar_chart"
        );
        assert_eq!(meta["tabular_chart"]["chart_data"]["num_rows"], 2);
    }

    #[test]
    fn a_backend_item_tree_is_written_verbatim() {
        use crate::tree::{Formatting, ItemTree, ListMeta, TreeKind};
        let mut t = ItemTree::default();
        let text = |label: &str, txt: &str| TreeKind::Text {
            label: label.into(),
            text: txt.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: None,
        };
        let title = t.add(None, Some(ContentLayer::Furniture), text("title", "Page"));
        let h = t.add(None, None, text("title", "Heading"));
        let group = t.add(
            Some(h),
            None,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        );
        t.add(
            Some(group),
            None,
            TreeKind::Text {
                label: "text".into(),
                text: "bold".into(),
                orig: None,
                formatting: Some(Formatting {
                    bold: true,
                    ..Formatting::default()
                }),
                hyperlink: Some("https://example.com/".into()),
                level: None,
                list: None,
            },
        );
        t.add(
            Some(group),
            None,
            TreeKind::Code {
                text: "x = 1".into(),
                orig: None,
                language: Some("python".into()),
                formatting: None,
                hyperlink: None,
            },
        );
        let sub = t.add(
            Some(h),
            None,
            TreeKind::Text {
                label: "section_header".into(),
                text: "Sub".into(),
                orig: Some("Sub\u{2019}".into()),
                formatting: None,
                hyperlink: None,
                level: Some(1),
                list: None,
            },
        );
        t.add(
            Some(sub),
            None,
            TreeKind::Text {
                label: "list_item".into(),
                text: "item".into(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: Some(ListMeta {
                    enumerated: true,
                    marker: "3.".into(),
                }),
            },
        );
        let _region = t.add(
            Some(sub),
            None,
            TreeKind::FieldRegion {
                items: vec![crate::FieldItem {
                    marker: None,
                    key: Some("Name".into()),
                    value: Some("Duck".into()),
                    value_kind: Some("read_only".into()),
                }],
            },
        );
        let table = t.add(
            Some(sub),
            None,
            TreeKind::Table {
                table: Table {
                    rows: vec![vec!["a  \n&lt;".into(), "b".into()]],
                    cells: Some(vec![
                        crate::TableCell {
                            text: "a  \n&lt;".into(),
                            bbox: None,
                            start_row: 0,
                            start_col: 0,
                            row_span: 3,
                            col_span: 1,
                            column_header: false,
                            row_header: true,
                            row_section: false,
                        },
                        crate::TableCell {
                            text: "b".into(),
                            bbox: None,
                            start_row: 0,
                            start_col: 1,
                            row_span: 1,
                            col_span: 1,
                            column_header: false,
                            row_header: false,
                            row_section: false,
                        },
                    ]),
                    ..Table::default()
                },
                rich_cells: vec![(0, 1, 0)], // patched below
                captions: Vec::new(),
            },
        );
        let cell_group = t.add(
            Some(table),
            None,
            TreeKind::Group {
                label: "unspecified".into(),
                name: "rich_cell_group_1_0_0".into(),
            },
        );
        if let TreeKind::Table { rich_cells, .. } = &mut t.items[table].kind {
            *rich_cells = vec![(0, 1, cell_group)];
        }
        let after = t.add(Some(sub), None, text("text", "after the region"));
        let _ = (title, after);

        let doc = DoclingDocument {
            tree: Some(t),
            ..DoclingDocument::new("t")
        };
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        // Creation order: Page, Heading, bold, x = 1, Sub, item, [Name, Duck], after.
        let texts: Vec<&str> = v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            [
                "Page",
                "Heading",
                "bold",
                "x = 1",
                "Sub",
                "item",
                "Name",
                "Duck",
                "after the region"
            ]
        );
        assert_eq!(
            v["body"]["children"],
            serde_json::json!([{"$ref": "#/texts/0"}, {"$ref": "#/texts/1"}])
        );
        assert_eq!(v["texts"][0]["content_layer"], "furniture");
        assert_eq!(
            v["texts"][1]["children"],
            serde_json::json!([{"$ref": "#/groups/0"}, {"$ref": "#/texts/4"}])
        );
        let bold = &v["texts"][2];
        assert_eq!(bold["parent"]["$ref"], "#/groups/0");
        let keys: Vec<&str> = bold
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "self_ref",
                "parent",
                "children",
                "content_layer",
                "label",
                "prov",
                "orig",
                "text",
                "formatting",
                "hyperlink"
            ]
        );
        assert_eq!(
            bold["formatting"],
            serde_json::json!({"bold": true, "italic": false, "underline": false, "strikethrough": false, "script": "baseline"})
        );
        let code = &v["texts"][3];
        assert_eq!(code["label"], "code");
        assert_eq!(code["code_language"], "Python");
        let sub = &v["texts"][4];
        assert_eq!(sub["orig"], "Sub\u{2019}");
        assert_eq!(sub["level"], 1);
        let item = &v["texts"][5];
        let keys: Vec<&str> = item
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "self_ref",
                "parent",
                "children",
                "content_layer",
                "label",
                "prov",
                "orig",
                "text",
                "enumerated",
                "marker"
            ]
        );
        assert_eq!(item["marker"], "3.");
        assert_eq!(v["texts"][7]["kind"], "read_only");
        assert_eq!(v["field_regions"][0]["parent"]["$ref"], "#/texts/4");
        let table = &v["tables"][0];
        assert_eq!(
            table["children"],
            serde_json::json!([{"$ref": "#/groups/1"}])
        );
        let cells = table["data"]["table_cells"].as_array().unwrap();
        assert_eq!(
            cells[0]["text"], "a  \n&lt;",
            "raw cell text is written verbatim"
        );
        assert_eq!(
            cells[0]["end_row_offset_idx"], 3,
            "declared spans are not clamped"
        );
        assert_eq!(cells[1]["ref"], serde_json::json!({"$ref": "#/groups/1"}));
        assert!(cells[0].get("ref").is_none());
        assert!(
            table["data"]["grid"][0][1].get("ref").is_none(),
            "the grid shows plain cells"
        );
        assert_eq!(v["groups"][1]["name"], "rich_cell_group_1_0_0");
    }

    /// A comment section that links its note text rather than its group — the
    /// spreadsheet shape, where docling-core's `add_comment` appends the text
    /// item's ref to each target.
    #[test]
    fn a_comment_section_can_be_referenced_by_its_note_text() {
        let doc = DoclingDocument {
            name: "c".into(),
            nodes: vec![
                Node::Commented {
                    comments: vec![0],
                    inner: Box::new(Node::Paragraph {
                        text: "annotated".into(),
                    }),
                },
                Node::CommentSection {
                    name: "comment-Sheet1-A1".into(),
                    text: "[author: A]: note".into(),
                    refs_note_text: true,
                    grouped: true,
                },
            ],
            ..DoclingDocument::new("c")
        };
        let v = crate::json::to_json(&doc);
        assert_eq!(v["groups"][0]["name"], "comment-Sheet1-A1");
        assert_eq!(v["texts"][0]["comments"][0]["$ref"], "#/texts/1");
    }

    /// docx reviewer comments: a `comment_section` group on the notes layer
    /// holding the note text, and a `comments` back-ref on the annotated item —
    /// keyed between `prov` and `orig`, the slot docling emits it in.
    #[test]
    fn comment_sections_link_back_to_their_items() {
        let doc = DoclingDocument {
            name: "c".into(),
            nodes: vec![
                Node::Commented {
                    comments: vec![0],
                    inner: Box::new(Node::Paragraph {
                        text: "annotated".into(),
                    }),
                },
                Node::Paragraph {
                    text: "plain".into(),
                },
                Node::CommentSection {
                    name: "comment-7".into(),
                    text: "[time: t]: note".into(),
                    refs_note_text: false,
                    grouped: true,
                },
            ],
            ..DoclingDocument::new("c")
        };
        let v = crate::json::to_json(&doc);
        // The group is the comment section; its only child is the notes text.
        assert_eq!(v["groups"][0]["label"], "comment_section");
        assert_eq!(v["groups"][0]["name"], "comment-7");
        assert_eq!(v["groups"][0]["content_layer"], "notes");
        assert_eq!(v["groups"][0]["children"][0]["$ref"], "#/texts/2");
        assert_eq!(v["texts"][2]["content_layer"], "notes");
        // The annotated item points back at the group; the plain one has no key.
        assert_eq!(v["texts"][0]["comments"][0]["$ref"], "#/groups/0");
        assert!(v["texts"][1].get("comments").is_none());
        // docling's key order: … prov, comments, orig, text.
        let keys: Vec<&str> = v["texts"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            &keys[keys.len() - 4..],
            &["prov", "comments", "orig", "text"]
        );
    }

    fn picture(caption: &str, caption_parent: CaptionParent) -> Node {
        Node::Picture {
            caption: Some(caption.into()),
            caption_href: None,
            image: None,
            classification: None,
            caption_parent,
        }
    }

    fn group(children: Vec<Node>) -> Node {
        Node::Group {
            label: "section".into(),
            name: None,
            layer: None,
            children,
        }
    }

    fn refs(v: &Value) -> Vec<&str> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|r| r["$ref"].as_str().unwrap())
            .collect()
    }

    /// #390: a declarative backend's caption is docling's `add_text` default —
    /// a body child, appended as it is created — wherever the picture sits:
    /// ahead of a top-level picture, behind the top-level item enclosing a
    /// nested one. The picture references it either way and has no children.
    #[test]
    fn a_body_caption_follows_the_enclosing_top_level_item() {
        let mut doc = DoclingDocument::new("t");
        doc.push(picture("top", CaptionParent::Body));
        doc.push(group(vec![
            Node::Paragraph { text: "p".into() },
            picture("nested", CaptionParent::Body),
        ]));
        doc.push(Node::Paragraph {
            text: "after".into(),
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(
            refs(&v["body"]["children"]),
            [
                "#/texts/0",
                "#/pictures/0",
                "#/groups/0",
                "#/texts/2",
                "#/texts/3"
            ]
        );
        assert_eq!(
            refs(&v["groups"][0]["children"]),
            ["#/texts/1", "#/pictures/1"]
        );
        for (cap, pic) in [(0, 0), (2, 1)] {
            assert_eq!(v["texts"][cap]["label"], "caption");
            assert_eq!(v["texts"][cap]["parent"]["$ref"], "#/body");
            assert_eq!(
                refs(&v["pictures"][pic]["captions"]),
                [format!("#/texts/{cap}")]
            );
            assert_eq!(v["pictures"][pic]["children"], serde_json::json!([]));
        }
    }

    /// The PDF pipeline's caption is the picture's (or table's) own child,
    /// as docling attaches a layout caption.
    #[test]
    fn an_item_caption_is_the_items_first_child() {
        let mut doc = DoclingDocument::new("t");
        doc.push(picture("fig", CaptionParent::Item));
        doc.push(Node::Table(Table {
            rows: vec![vec!["a".into()]],
            caption: Some("tab".into()),
            caption_parent: CaptionParent::Item,
            ..Table::default()
        }));
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(refs(&v["body"]["children"]), ["#/pictures/0", "#/tables/0"]);
        assert_eq!(v["texts"][0]["parent"]["$ref"], "#/pictures/0");
        assert_eq!(refs(&v["pictures"][0]["children"]), ["#/texts/0"]);
        assert_eq!(refs(&v["pictures"][0]["captions"]), ["#/texts/0"]);
        assert_eq!(v["texts"][1]["parent"]["$ref"], "#/tables/0");
        assert_eq!(refs(&v["tables"][0]["children"]), ["#/texts/1"]);
        assert_eq!(refs(&v["tables"][0]["captions"]), ["#/texts/1"]);
    }

    /// A container caption sits beside its item under the item's parent —
    /// ahead of it (an office chart's title) or behind it (an HTML
    /// `<figure>`'s table, whose figcaption docling adds after the table).
    #[test]
    fn a_container_caption_is_the_items_sibling() {
        let mut doc = DoclingDocument::new("t");
        doc.push(group(vec![
            picture("chart", CaptionParent::Container),
            Node::Table(Table {
                rows: vec![vec!["a".into()]],
                caption: Some("figcaption".into()),
                caption_parent: CaptionParent::ContainerAfter,
                ..Table::default()
            }),
        ]));
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(refs(&v["body"]["children"]), ["#/groups/0"]);
        assert_eq!(
            refs(&v["groups"][0]["children"]),
            ["#/texts/0", "#/pictures/0", "#/tables/0", "#/texts/1"]
        );
        assert_eq!(v["texts"][0]["parent"]["$ref"], "#/groups/0");
        assert_eq!(v["texts"][1]["parent"]["$ref"], "#/groups/0");
        assert_eq!(v["pictures"][0]["children"], serde_json::json!([]));
        assert_eq!(v["tables"][0]["children"], serde_json::json!([]));
    }
}
