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

use crate::document::{DoclingDocument, Node, Table};

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

/// Build the docling-core JSON object for `doc`.
pub fn to_json(doc: &DoclingDocument) -> Value {
    let mut b = Builder::default();
    let body = b.walk_into(&doc.nodes, "#/body");

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
        "pages": {},
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

#[derive(Default)]
struct Builder {
    texts: Vec<Value>,
    groups: Vec<Value>,
    tables: Vec<Value>,
    pictures: Vec<Value>,
    field_regions: Vec<Value>,
    field_items: Vec<Value>,
}

impl Builder {
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
            Node::Paragraph { text } => {
                // A whole-paragraph display equation is a formula item (docling
                // wraps it in `$$…$$` and, unlike a text item, never escapes it).
                let t = text.trim();
                match t.strip_prefix("$$").and_then(|s| s.strip_suffix("$$")) {
                    Some(inner) if !inner.is_empty() => Some(self.add_formula(inner, parent)),
                    _ => Some(self.add_text("text", text, parent, json!({}))),
                }
            }
            Node::Code { language, text } => Some(self.add_code(text, language.as_deref(), parent)),
            Node::Table(t) => Some(self.add_table(t, parent)),
            Node::Picture { caption, image } => {
                Some(self.add_picture(caption.as_deref(), image.as_ref(), parent))
            }
            // A chart is a picture item in the JSON (its data table is
            // DocLang-only); no image payload.
            Node::Chart { .. } => Some(self.add_picture(None, None, parent)),
            // A DocLang-only node is omitted from the JSON body.
            Node::DoclangOnly(_) => None,
            Node::Group { label, children } => Some(self.add_group(label, children, parent)),
            Node::FieldRegion { items } => Some(self.add_field_region(items, parent)),
            // A rich inline group is a text item over its Markdown text; the
            // structured runs are DocLang-only, so the JSON matches a paragraph.
            Node::InlineGroup { md_text, .. } => {
                Some(self.add_text("text", md_text, parent, json!({})))
            }
            // Furniture is not emitted into the body/JSON (DocLang-only layer).
            Node::Furniture(_) => None,
            // Layout provenance is DocLang-only; emit the wrapped node.
            Node::Located { inner, .. } => self.add_node(inner, parent),
            // Page breaks are DocLang-only; docling omits them from the JSON body.
            Node::PageBreak => None,
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
                child_refs
                    .push(json!({ "$ref": self.add_text(label, text, &self_ref, json!({})) }));
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

    fn add_text(&mut self, label: &str, text: &str, parent: &str, extra: Value) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let raw = unescape_text(text);
        let mut item = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": label,
            "prov": [],
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
        self.texts.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "formula",
            "prov": [],
            "orig": latex,
            "text": latex,
        }));
        self_ref
    }

    fn add_code(&mut self, text: &str, language: Option<&str>, parent: &str) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let raw = unescape_text(text);
        self.texts.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "code",
            "prov": [],
            "orig": raw,
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
            ..
        } = node
        else {
            unreachable!()
        };
        let self_ref = format!("#/texts/{}", self.texts.len());
        let raw = unescape_text(text);
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
            "prov": [],
            "orig": raw,
            "text": raw,
            "enumerated": ordered,
            "marker": marker,
        }));
        self_ref
    }

    fn add_table(&mut self, t: &Table, parent: &str) -> String {
        let self_ref = format!("#/tables/{}", self.tables.len());
        let num_rows = t.rows.len();
        let num_cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut grid = Vec::with_capacity(num_rows);
        let mut cells = Vec::new();
        for (r, row) in t.rows.iter().enumerate() {
            let mut grid_row = Vec::with_capacity(num_cols);
            for c in 0..num_cols {
                let text = row.get(c).map(|s| unescape_text(s)).unwrap_or_default();
                let cell = json!({
                    "row_span": 1,
                    "col_span": 1,
                    "start_row_offset_idx": r,
                    "end_row_offset_idx": r + 1,
                    "start_col_offset_idx": c,
                    "end_col_offset_idx": c + 1,
                    "text": text,
                    "column_header": r == 0,
                    "row_header": false,
                    "row_section": false,
                    "fillable": false,
                });
                grid_row.push(cell.clone());
                cells.push(cell);
            }
            grid.push(grid_row);
        }
        self.tables.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "table",
            "prov": [],
            "captions": [],
            "references": [],
            "footnotes": [],
            "data": {
                "table_cells": cells,
                "num_rows": num_rows,
                "num_cols": num_cols,
                "grid": grid,
            },
            "annotations": [],
        }));
        self_ref
    }

    fn add_picture(
        &mut self,
        caption: Option<&str>,
        image: Option<&crate::PictureImage>,
        parent: &str,
    ) -> String {
        let self_ref = format!("#/pictures/{}", self.pictures.len());
        let mut captions = Vec::new();
        if let Some(cap) = caption.filter(|c| !c.is_empty()) {
            // Emit the caption as a text item that the picture references.
            let cap_ref = self.add_text("caption", cap, &self_ref, json!({}));
            captions.push(json!({ "$ref": cap_ref }));
        }
        let mut item = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "picture",
            "prov": [],
            "captions": captions,
            "references": [],
            "footnotes": [],
            "annotations": [],
        });
        // docling stores the extracted image as an `ImageRef` (data URI + size).
        if let Some(img) = image {
            item["image"] = json!({
                "mimetype": img.mimetype,
                "dpi": 72,
                "size": { "width": img.width, "height": img.height },
                "uri": img.data_uri(),
            });
        }
        self.pictures.push(item);
        self_ref
    }

    fn add_group(&mut self, label: &str, nodes: &[Node], parent: &str) -> String {
        let self_ref = format!("#/groups/{}", self.groups.len());
        self.groups.push(Value::Null);
        let children = self.walk_into(nodes, &self_ref);
        let name = if label == "inline" { "group" } else { label };
        self.groups[group_index(&self_ref)] = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": children,
            "content_layer": "body",
            "name": name,
            "label": label,
        });
        self_ref
    }

    /// Walk a slice of sibling nodes, returning each child's `$ref`; runs of
    /// list items are folded into list groups (one per sibling list).
    fn walk_into(&mut self, nodes: &[Node], parent: &str) -> Vec<Value> {
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
                    children.push(json!({ "$ref": r }));
                }
                i += 1;
            }
        }
        children
    }

    /// A run of list items may hold several *sibling* lists; emit one list group
    /// per sibling. The boundary mirrors the Markdown serializer: at the base
    /// level a new list starts on `first_in_list`, a kind flip (`ul`↔`ol`), or an
    /// ordered-number discontinuity.
    fn add_sibling_lists(&mut self, run: &[Node], parent: &str, out: &mut Vec<Value>) {
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
                continue; // nested item — handled inside add_list
            }
            if k > seg {
                if let Some((po, pn)) = prev {
                    if *first_in_list || po != *ordered || (*ordered && *number != pn + 1) {
                        out.push(json!({ "$ref": self.add_list(&run[seg..k], parent) }));
                        seg = k;
                    }
                }
            }
            prev = Some((*ordered, *number));
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
    use crate::{DoclingDocument, ImageMode, Node, PictureImage, Table};
    use serde_json::Value;

    fn doc_with_image() -> DoclingDocument {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Picture {
            caption: Some("Fig 1".into()),
            image: Some(PictureImage {
                mimetype: "image/png".into(),
                width: 4,
                height: 2,
                data: b"foobar".to_vec(),
            }),
        });
        doc
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
        // JSON carries the ImageRef (data URI + size)
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(v["pictures"][0]["image"]["mimetype"], "image/png");
        assert_eq!(v["pictures"][0]["image"]["size"]["width"], 4);
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
        });
        doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: false,
            text: "two".into(),
            level: 0,
            marker: None,
            location: None,
        });
        doc.push(Node::Table(Table {
            rows: vec![vec!["A".into(), "B".into()]],
            location: None,
            structure: None,
            cell_blocks: None,
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
}
