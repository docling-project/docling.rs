//! Export a [`DoclingDocument`] to docling-core's native JSON wire format
//! (`DoclingDocument` schema v1.11.0) — the same shape `export_to_dict()` /
//! `save_as_json()` produce in Python docling, and the inverse of the
//! JSON-docling reader.
//!
//! The crate's [`Node`] model bakes Markdown escaping (and inline markers) into
//! its text, whereas docling stores raw text and escapes at render time. We
//! therefore *un-escape* on the way out so a docling-core round-trip
//! (`load_from_json().export_to_markdown()`) reproduces the same Markdown.

use serde_json::{json, Map, Value};

use crate::document::{CaptionParent, ContentLayer, DoclingDocument, Node, Table};

const SCHEMA_VERSION: &str = "1.11.0";

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

/// An item's exact provenance, written verbatim (see [`Builder::prov_json`]):
/// a [`Node::Prov`] wrapper's top-left page box, or a tree item's
/// [`TreeProv`](crate::tree::TreeProv).
#[derive(Clone, Copy)]
struct ExactProv {
    page_no: usize,
    bbox: [f64; 4],
    bottom_left: bool,
    charspan: [usize; 2],
}

impl From<&crate::tree::TreeProv> for ExactProv {
    fn from(p: &crate::tree::TreeProv) -> Self {
        ExactProv {
            page_no: p.page_no,
            bbox: p.bbox,
            bottom_left: p.bottom_left,
            charspan: p.charspan,
        }
    }
}

/// Page sizes by page number; the first entry per page wins.
type PageSizes = std::collections::HashMap<usize, (f64, f64)>;

fn page_sizes(pages: &[(usize, f64, f64)]) -> PageSizes {
    let mut sizes = PageSizes::with_capacity(pages.len());
    for &(p, w, h) in pages {
        sizes.entry(p).or_insert((w, h));
    }
    sizes
}

/// docling-core's `_clamp_provenance_bboxes_to_pages`: each `prov` box of
/// every item on a known page is clamped to `[0, width] × [0, height]`
/// (whatever its `coord_origin` — docling clamps the stored numbers), and a
/// table whose provenance sits on one page has its cell boxes clamped too.
/// Runs over the finished `texts`, `pictures`, `tables` and
/// `key_value_items` (the form-field buckets are left as built).
fn clamp_boxes_to_pages(b: &mut Builder) {
    if b.pages.is_empty() {
        return;
    }
    // A map, not a scan per box: that was items × pages on long documents.
    let sizes = page_sizes(&b.pages);
    for item in &mut b.texts {
        match item {
            Item::Text(t) => t.prov.clamp(&sizes),
            Item::Json(v) => {
                clamp_item(v, &sizes);
            }
        }
    }
    for item in b.pictures.iter_mut().chain(b.key_value_items.iter_mut()) {
        clamp_item(item, &sizes);
    }
    for t in &mut b.tables {
        if let Some(size) = clamp_item(&mut t.item, &sizes) {
            t.data.clamp(size);
        }
    }
}

/// Clamp `item`'s `prov` boxes; returns the size of the page when the item
/// sits on exactly one known page — a table's cells are clamped to it.
fn clamp_item(item: &mut Value, sizes: &PageSizes) -> Option<(f64, f64)> {
    let size = |page_no: &Value| -> Option<(f64, f64)> {
        let n = page_no.as_u64()? as usize;
        sizes.get(&n).copied()
    };
    let clamp_bbox = |bbox: &mut Value, (w, h): (f64, f64)| {
        for (key, hi) in [("l", w), ("r", w), ("t", h), ("b", h)] {
            if let Some(v) = bbox.get(key).and_then(Value::as_f64) {
                bbox[key] = json!(round2(v.clamp(0.0, hi.max(0.0))));
            }
        }
    };
    let mut table_page: Option<Option<(f64, f64)>> = None;
    if let Some(provs) = item.get_mut("prov").and_then(Value::as_array_mut) {
        let mut page_nos: Vec<u64> = Vec::new();
        for prov in provs.iter_mut() {
            if let Some(n) = prov.get("page_no").and_then(Value::as_u64) {
                page_nos.push(n);
            }
            let Some(sz) = prov.get("page_no").and_then(size) else {
                continue;
            };
            if let Some(bbox) = prov.get_mut("bbox") {
                clamp_bbox(bbox, sz);
            }
        }
        page_nos.sort_unstable();
        page_nos.dedup();
        if let [only] = page_nos[..] {
            table_page = Some(size(&json!(only)));
        }
    }
    table_page.flatten()
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

/// docling's `TrackSource` entry: `kind`, the two offsets, then the optional
/// `identifier` / `voice` (dropped when `None`, as `exclude_none` does).
fn track_json(t: &crate::tree::TreeTrack) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("kind".into(), json!("track"));
    m.insert("start_time".into(), json!(t.start_time));
    m.insert("end_time".into(), json!(t.end_time));
    if let Some(id) = &t.identifier {
        m.insert("identifier".into(), json!(id));
    }
    if let Some(v) = &t.voice {
        m.insert("voice".into(), json!(v));
    }
    Value::Object(m)
}

/// docling-core's `ImageRef` (pydantic field order: mimetype, dpi, size —
/// as floats — uri), with the bytes inlined as a `data:` URI — a PNG, like
/// `ImageRef.from_pil`'s (`pixel_digest::docling_data_uri`).
fn image_ref_json(img: &crate::PictureImage) -> Value {
    let (mimetype, uri) = crate::pixel_digest::docling_data_uri(img);
    json!({
        "mimetype": mimetype,
        "dpi": img.dpi,
        "size": { "width": img.width as f64, "height": img.height as f64 },
        "uri": uri,
    })
}

/// Build the docling-core JSON object for `doc`.
pub fn to_json(doc: &DoclingDocument) -> Value {
    build_parts(doc, false).into_value(None)
}

/// The key a table's `data` is replaced by in [`to_json_typed_tables`]'s
/// `Value`: the index of its [`TableData`] beside it.
pub(crate) const TYPED_TABLE: &str = "_typed_table";

/// [`to_json`] for the serializers that walk the `Value` (HTML, Pandoc,
/// WebVTT), with each table's `data` left typed (#875): the item carries
/// `{"_typed_table": i}` and the cells are `tables[i]`, read through
/// [`table_view`]. As a `Value` a table cost two maps of eleven owned keys
/// per cell (its `table_cells` record and its `grid` copy) — 2.6 GB of an
/// HTML export's peak on a 640,000-cell CSV. `note_keys` adds the Pandoc
/// writer's keys ([`to_json_with_notes`]).
pub(crate) fn to_json_typed_tables(
    doc: &DoclingDocument,
    note_keys: bool,
) -> (Value, Vec<TableData>) {
    let mut tables = Vec::new();
    let value = build_parts(doc, note_keys).into_value(Some(&mut tables));
    (value, tables)
}

/// [`to_json`] plus the note calls docling's model cannot express: a text
/// item calling footnotes carries `"_notes": [[offset, text], …]`, a
/// footnote item that is such a note's body `"_note_body": true` (#538),
/// a text item that is a link destination `"_anchors": ["id", …]` (#849),
/// a heading whose text opens with generated numbering `"_number": "1.2"`
/// (#850), a run kept as a tracked change `"_revision": {"kind":
/// "insertion" | "deletion", "author", "date"}` (#851),
/// a picture's image its displayed size `"_display": [width, height]` in
/// inches (#834), and the document's `metadata` whether or not the JSON
/// export was asked for it (#836). The Pandoc writer reads these; nothing
/// is ever exported with them. The tables stay typed, as in
/// [`to_json_typed_tables`].
pub(crate) fn to_json_with_notes(doc: &DoclingDocument) -> (Value, Vec<TableData>) {
    to_json_typed_tables(doc, true)
}

fn build_parts(doc: &DoclingDocument, note_keys: bool) -> Parts {
    let mut b = Builder {
        note_keys,
        ..Builder::default()
    };
    // A backend that built docling's item tree ([`crate::tree`]) has already
    // decided every parent, child and creation index; serialize that. The
    // flat nodes are for the other serializers.
    let body = match &doc.tree {
        Some(tree) => {
            // The page map still comes from the flat stream's markers (a
            // PPTX slide's EMU size): the tree holds items, not pages.
            for n in &doc.nodes {
                if let Node::PageInfo {
                    page_no,
                    width,
                    height,
                } = n
                {
                    if *page_no > 0 {
                        b.pages.push((*page_no, *width as f64, *height as f64));
                    }
                }
            }
            // docling-core's `validate_misplaced_list_items` runs on every
            // document it serializes; apply it the same way (keeping the
            // items' children, which docling-core drops — #586).
            let mut tree = tree.clone();
            // Schema 1.11.0's inline runs own their whitespace (#997); a
            // backend still building trimmed runs gets the old join back in
            // the runs, as docling-core migrates a pre-1.11 document on load.
            if !tree.inline_whitespace {
                tree.separate_legacy_inline_runs();
            }
            tree.wrap_misplaced_list_items();
            b.write_tree(&tree).into_iter().map(Child::json).collect()
        }
        None => b.walk_into(&doc.nodes, "#/body"),
    };
    b.link_comments();

    // docling-core's `validate_document` — a pydantic `model_validator` every
    // `DoclingDocument` passes through when a `ConversionResult` (or a
    // serializer) is built around it — clamps every provenance box, and a
    // table's cell boxes, into its page's bounds in place
    // (`_clamp_provenance_bboxes_to_pages`). The JSON docling writes therefore
    // carries the clamped boxes: a spreadsheet region whose page was sized
    // `right − left` × `bottom − top` loses its offset, a slide shape hanging
    // off the slide edge is cut at it. Reproduce that on the finished items.
    clamp_boxes_to_pages(&mut b);

    let pages = b
        .pages
        .iter()
        .map(|(n, w, h)| {
            let r2 = |v: f64| (v * 100.0).round() / 100.0;
            // docling-core's `PageItem` field order: size, image, page_no
            // (#520 — the image only when page images were generated).
            let mut page = serde_json::Map::new();
            page.insert("size".into(), json!({ "width": r2(*w), "height": r2(*h) }));
            if let Some(img) = doc.page_images.get(n) {
                page.insert("image".into(), image_ref_json(img));
            }
            page.insert("page_no".into(), json!(n));
            (n.to_string(), Value::Object(page))
        })
        .collect::<serde_json::Map<String, Value>>();
    // docling only emits `field_regions` / `field_items` when a document has
    // form fields, and places them just before `pages`.
    let fields = (!b.field_regions.is_empty()).then_some((b.field_regions, b.field_items));
    Parts {
        name: doc.name.clone(),
        // The Pandoc writer (`note_keys`) always gets the metadata, for
        // its `meta` (#836); the JSON export only when asked.
        metadata: (doc.metadata_output.in_json() || note_keys)
            .then(|| doc.metadata.to_json())
            .filter(|m| m.as_object().is_some_and(|o| !o.is_empty())),
        origin: json!({
            "mimetype": "text/plain",
            "binary_hash": fnv1a(&doc.name),
            "filename": doc.name,
        }),
        furniture: json!({
            "self_ref": "#/furniture",
            "children": [],
            "content_layer": "furniture",
            "name": "_root_",
            "label": "unspecified",
        }),
        body,
        groups: b.groups,
        texts: b.texts,
        pictures: b.pictures,
        tables: b.tables,
        key_value_items: b.key_value_items,
        fields,
        pages,
    }
}

/// The finished document, before it is a `Value`: one representation
/// behind both [`to_json`] (a `Value`) and [`write_json`] (bytes, with no
/// `Value` ever built — the text items stay compact [`Item`]s throughout).
/// Both produce the same keys in the same order.
struct Parts {
    name: String,
    /// The document's metadata (#679), after `origin`, only when the
    /// document's output mode asks for it and there is any.
    metadata: Option<Value>,
    origin: Value,
    furniture: Value,
    /// The body's `children`; the rest of the body object is fixed.
    body: Vec<Child>,
    groups: Vec<Value>,
    texts: Vec<Item>,
    pictures: Vec<Value>,
    tables: Vec<TableItem>,
    key_value_items: Vec<Value>,
    /// `field_regions` and `field_items`, present only for a document with
    /// form fields (non-form documents' JSON has neither key).
    fields: Option<(Vec<Value>, Vec<Value>)>,
    pages: serde_json::Map<String, Value>,
}

impl Parts {
    /// The document `Value`; with `typed`, the tables' `data` goes there
    /// instead (see [`to_json_typed_tables`]).
    fn into_value(self, mut typed: Option<&mut Vec<TableData>>) -> Value {
        // Moved in, never through `json!`, which would deep-copy each array.
        let mut out = serde_json::Map::with_capacity(15);
        out.insert("schema_name".into(), "DoclingDocument".into());
        out.insert("version".into(), SCHEMA_VERSION.into());
        out.insert("name".into(), Value::String(self.name));
        out.insert("origin".into(), self.origin);
        if let Some(m) = self.metadata {
            out.insert("metadata".into(), m);
        }
        out.insert("furniture".into(), self.furniture);
        out.insert(
            "body".into(),
            object([
                ("self_ref", "#/body".into()),
                (
                    "children",
                    Value::Array(self.body.into_iter().map(Child::into_value).collect()),
                ),
                ("content_layer", "body".into()),
                ("name", "_root_".into()),
                ("label", "unspecified".into()),
            ]),
        );
        out.insert("groups".into(), Value::Array(self.groups));
        out.insert(
            "texts".into(),
            Value::Array(
                self.texts
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| item.into_value(i))
                    .collect(),
            ),
        );
        out.insert("pictures".into(), Value::Array(self.pictures));
        out.insert(
            "tables".into(),
            Value::Array(
                self.tables
                    .into_iter()
                    .map(|t| match typed.as_deref_mut() {
                        Some(store) => {
                            let TableItem { mut item, data } = t;
                            if let Some(slot) = item.get_mut("data") {
                                *slot = object([(TYPED_TABLE, store.len().into())]);
                            }
                            store.push(data);
                            item
                        }
                        None => t.into_value(),
                    })
                    .collect(),
            ),
        );
        out.insert("key_value_items".into(), Value::Array(self.key_value_items));
        out.insert("form_items".into(), Value::Array(Vec::new()));
        if let Some((regions, items)) = self.fields {
            out.insert("field_regions".into(), Value::Array(regions));
            out.insert("field_items".into(), Value::Array(items));
        }
        out.insert("pages".into(), Value::Object(self.pages));
        Value::Object(out)
    }
}

/// [`Parts`] with `extras` — top-level keys a caller appends after
/// `pages`, as inserting them into [`to_json`]'s `Value` would.
struct PartsWith<'a>(&'a Parts, &'a [(&'a str, &'a Value)]);

impl serde::Serialize for PartsWith<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let PartsWith(parts, extras) = *self;
        let mut m = s.serialize_map(Some(
            13 + 2 * usize::from(parts.fields.is_some())
                + usize::from(parts.metadata.is_some())
                + extras.len(),
        ))?;
        m.serialize_entry("schema_name", "DoclingDocument")?;
        m.serialize_entry("version", SCHEMA_VERSION)?;
        m.serialize_entry("name", &parts.name)?;
        m.serialize_entry("origin", &parts.origin)?;
        if let Some(meta) = &parts.metadata {
            m.serialize_entry("metadata", meta)?;
        }
        m.serialize_entry("furniture", &parts.furniture)?;
        m.serialize_entry("body", &Body(&parts.body))?;
        m.serialize_entry("groups", &parts.groups)?;
        m.serialize_entry("texts", &Texts(&parts.texts))?;
        m.serialize_entry("pictures", &parts.pictures)?;
        m.serialize_entry("tables", &parts.tables)?;
        m.serialize_entry("key_value_items", &parts.key_value_items)?;
        m.serialize_entry("form_items", &[(); 0])?;
        if let Some((regions, items)) = &parts.fields {
            m.serialize_entry("field_regions", regions)?;
            m.serialize_entry("field_items", items)?;
        }
        m.serialize_entry("pages", &parts.pages)?;
        for (k, v) in extras {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

/// The body object: fixed keys around its `children`.
struct Body<'a>(&'a [Child]);

impl serde::Serialize for Body<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("self_ref", "#/body")?;
        m.serialize_entry("children", self.0)?;
        m.serialize_entry("content_layer", "body")?;
        m.serialize_entry("name", "_root_")?;
        m.serialize_entry("label", "unspecified")?;
        m.end()
    }
}

/// Write `doc`'s docling JSON to `w` — byte for byte what serializing
/// [`to_json`]'s `Value` writes (compact, or `pretty` as
/// `serde_json::to_writer_pretty` indents it), without building that
/// `Value`. On a long PDF the `Value` is most of the export's memory: ~2.7 KB
/// per text item (an owned-key map per JSON object) against a few hundred
/// bytes here.
pub fn write_json<W: std::io::Write>(
    doc: &DoclingDocument,
    w: W,
    pretty: bool,
) -> serde_json::Result<()> {
    write_json_with(doc, w, pretty, &[])
}

/// [`write_json`] with `extras` appended after `pages` — byte for byte
/// serializing [`to_json`]'s `Value` with each `value[k] = v` inserted (the
/// keys must be new to it, as docling-serve's `confidence` is).
pub fn write_json_with<W: std::io::Write>(
    doc: &DoclingDocument,
    w: W,
    pretty: bool,
    extras: &[(&str, &Value)],
) -> serde_json::Result<()> {
    use serde::Serialize;
    let parts = build_parts(doc, false);
    let parts = PartsWith(&parts, extras);
    if pretty {
        parts.serialize(&mut serde_json::Serializer::pretty(w))
    } else {
        parts.serialize(&mut serde_json::Serializer::new(w))
    }
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

/// The picture-OCR enrichment's text (#645) as docling writes a picture
/// description: `meta.description` — a `DescriptionMetaField`, pydantic
/// field order `created_by` (the model, here the OCR engine) then `text` —
/// plus the deprecated-but-still-emitted `PictureDescriptionData`
/// annotation (`kind`, `text`, `provenance`), both exactly what
/// `PictureDescriptionBaseModel` attaches. Carried like
/// [`classification_meta`]'s: `annotations` is lifted onto the item.
fn description_meta(desc: &crate::PictureDescription) -> Value {
    json!({
        "description": {
            "created_by": desc.provenance,
            "text": desc.text,
        },
        "annotations": [{
            "kind": "description",
            "text": desc.text,
            "provenance": desc.provenance,
        }],
    })
}

/// A flat picture node's `meta`: the classifier's predictions and/or the
/// OCR description, merged — `description` precedes `classification` in
/// `PictureMeta`'s field order (it is inherited from the floating-item
/// base), the annotations run in the order docling's enrichment pipeline
/// appends them (classification, then description).
fn picture_meta(
    classes: Option<&[crate::PictureClass]>,
    desc: Option<&crate::PictureDescription>,
) -> Option<Value> {
    let mut meta = Map::new();
    let mut annotations = Vec::new();
    if let Some(desc) = desc {
        let mut m = description_meta(desc);
        annotations.push(m["annotations"][0].take());
        meta.insert("description".into(), m["description"].take());
    }
    if let Some(classes) = classes {
        let mut m = classification_meta(classes);
        annotations.insert(0, m["annotations"][0].take());
        meta.insert("classification".into(), m["classification"].take());
    }
    if meta.is_empty() {
        return None;
    }
    meta.insert("annotations".into(), Value::Array(annotations));
    Some(Value::Object(meta))
}

/// One table cell of docling's `TableCell`, typed (#875): the table's cells
/// stay these until the export writes them, rather than a `serde_json` map
/// of eleven owned keys each — and a second copy of that map in every grid
/// slot the cell covers. As maps a short numeric CSV cell cost ~7.5 KB of
/// export peak; typed it is this struct plus its text.
#[derive(Clone)]
pub(crate) struct GridCell {
    row_span: usize,
    col_span: usize,
    start_row: usize,
    end_row: usize,
    start_col: usize,
    end_col: usize,
    text: Box<str>,
    column_header: bool,
    row_header: bool,
    row_section: bool,
    /// `l t r b`, top-left origin — the backend's `f32` box widened to the
    /// `f64` a `Value` would hold it as, so both paths print the same digits.
    bbox: Option<[f64; 4]>,
    /// A `RichTableCell`'s `ref` to the group holding its content — written
    /// on `table_cells` only; the `grid` shows the plain cell.
    cell_ref: Option<Box<Value>>,
}

impl GridCell {
    /// docling's empty filler for a grid position no cell covers.
    fn filler(r: usize, c: usize) -> Self {
        GridCell {
            row_span: 1,
            col_span: 1,
            start_row: r,
            end_row: r + 1,
            start_col: c,
            end_col: c + 1,
            text: "".into(),
            column_header: false,
            row_header: false,
            row_section: false,
            bbox: None,
            cell_ref: None,
        }
    }

    /// The cell as a `Value`: the eleven fields in pydantic order, `bbox`
    /// appended when the cell carries one, then the rich cell's `ref`
    /// (`with_ref`, the `table_cells` copy). Built straight into a `Map`
    /// sized for its entries rather than through `json!`'s doubling map.
    fn to_value(&self, with_ref: bool) -> Value {
        let mut m = serde_json::Map::with_capacity(13);
        m.insert("row_span".into(), self.row_span.into());
        m.insert("col_span".into(), self.col_span.into());
        m.insert("start_row_offset_idx".into(), self.start_row.into());
        m.insert("end_row_offset_idx".into(), self.end_row.into());
        m.insert("start_col_offset_idx".into(), self.start_col.into());
        m.insert("end_col_offset_idx".into(), self.end_col.into());
        m.insert("text".into(), Value::String(self.text.to_string()));
        m.insert("column_header".into(), self.column_header.into());
        m.insert("row_header".into(), self.row_header.into());
        m.insert("row_section".into(), self.row_section.into());
        m.insert("fillable".into(), false.into());
        if let Some([l, t, r, b]) = self.bbox {
            m.insert(
                "bbox".into(),
                object([
                    ("l", l.into()),
                    ("t", t.into()),
                    ("r", r.into()),
                    ("b", b.into()),
                    ("coord_origin", "TOPLEFT".into()),
                ]),
            );
        }
        if let Some(r) = self.cell_ref.as_deref().filter(|_| with_ref) {
            m.insert("ref".into(), r.clone());
        }
        Value::Object(m)
    }
}

/// [`GridCell::to_value`] written in place: the same keys in the same order.
struct CellOut<'a>(&'a GridCell, bool);

impl serde::Serialize for CellOut<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let CellOut(c, with_ref) = *self;
        let cell_ref = c.cell_ref.as_deref().filter(|_| with_ref);
        let mut m = s.serialize_map(Some(
            11 + usize::from(c.bbox.is_some()) + usize::from(cell_ref.is_some()),
        ))?;
        m.serialize_entry("row_span", &c.row_span)?;
        m.serialize_entry("col_span", &c.col_span)?;
        m.serialize_entry("start_row_offset_idx", &c.start_row)?;
        m.serialize_entry("end_row_offset_idx", &c.end_row)?;
        m.serialize_entry("start_col_offset_idx", &c.start_col)?;
        m.serialize_entry("end_col_offset_idx", &c.end_col)?;
        m.serialize_entry("text", &c.text)?;
        m.serialize_entry("column_header", &c.column_header)?;
        m.serialize_entry("row_header", &c.row_header)?;
        m.serialize_entry("row_section", &c.row_section)?;
        m.serialize_entry("fillable", &false)?;
        if let Some(b) = c.bbox {
            struct Bbox([f64; 4]);
            impl serde::Serialize for Bbox {
                fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                    let mut m = s.serialize_map(Some(5))?;
                    for (k, v) in ["l", "t", "r", "b"].iter().zip(self.0) {
                        m.serialize_entry(k, &v)?;
                    }
                    m.serialize_entry("coord_origin", "TOPLEFT")?;
                    m.end()
                }
            }
            m.serialize_entry("bbox", &Bbox(b))?;
        }
        if let Some(r) = cell_ref {
            m.serialize_entry("ref", r)?;
        }
        m.end()
    }
}

/// What a `grid` position shows: the cell covering it (an index into
/// [`TableData::cells`] — docling repeats the whole cell at every position
/// it spans), docling's empty filler cell, or `null`.
#[derive(Clone, Copy)]
enum GridSlot {
    Cell(usize),
    Filler,
    Null,
}

/// docling's `TableData` for a table: `table_cells`, `num_rows`/`num_cols`
/// and the `grid` that repeats each cell at every position it covers —
/// typed (#875), each cell stored once and the grid as slots pointing at
/// it, so a table costs its cells rather than a map per cell and per slot.
/// Shared by table items and a chart picture's `meta.tabular_chart.chart_data`.
pub(crate) struct TableData {
    cells: Vec<GridCell>,
    num_rows: usize,
    num_cols: usize,
    /// Row-major, `num_rows × num_cols`.
    grid: Vec<GridSlot>,
}

/// One position of docling's `TableData.grid`, as the DocLang tree
/// serializer reads it (`doclang_tree.rs`): the covering cell's anchor,
/// text and header roles — an empty filler where no cell covers the
/// position, which is what docling's computed `grid` holds there.
pub(crate) struct GridPos<'a> {
    pub(crate) start_row: usize,
    pub(crate) start_col: usize,
    pub(crate) text: &'a str,
    pub(crate) column_header: bool,
    pub(crate) row_header: bool,
    pub(crate) row_section: bool,
}

/// [`TableData`] of a backend-built tree's table (raw cell text).
pub(crate) fn raw_table_data(t: &Table) -> TableData {
    table_data_with(t, true)
}

impl TableData {
    /// `(num_rows, num_cols)`.
    pub(crate) fn dims(&self) -> (usize, usize) {
        (self.num_rows, self.num_cols)
    }

    /// Whether `table_cells` holds any cell.
    pub(crate) fn has_cells(&self) -> bool {
        !self.cells.is_empty()
    }

    /// The grid position `(r, c)`.
    pub(crate) fn at(&self, r: usize, c: usize) -> GridPos<'_> {
        match self.grid[r * self.num_cols + c] {
            GridSlot::Cell(i) => {
                let cell = &self.cells[i];
                GridPos {
                    start_row: cell.start_row,
                    start_col: cell.start_col,
                    text: &cell.text,
                    column_header: cell.column_header,
                    row_header: cell.row_header,
                    row_section: cell.row_section,
                }
            }
            GridSlot::Filler | GridSlot::Null => GridPos {
                start_row: r,
                start_col: c,
                text: "",
                column_header: false,
                row_header: false,
                row_section: false,
            },
        }
    }
}

impl TableData {
    fn slot_cell(
        &self,
        slot: GridSlot,
        r: usize,
        c: usize,
    ) -> Option<std::borrow::Cow<'_, GridCell>> {
        match slot {
            GridSlot::Cell(i) => Some(std::borrow::Cow::Borrowed(&self.cells[i])),
            GridSlot::Filler => Some(std::borrow::Cow::Owned(GridCell::filler(r, c))),
            GridSlot::Null => None,
        }
    }

    fn into_value(self) -> Value {
        let cells = Value::Array(self.cells.iter().map(|c| c.to_value(true)).collect());
        let mut data = serde_json::Map::with_capacity(5);
        data.insert("table_cells".into(), cells);
        data.insert("num_rows".into(), self.num_rows.into());
        data.insert("num_cols".into(), self.num_cols.into());
        data.insert("orientation".into(), "rot_0".into());
        let mut rows = Vec::with_capacity(self.num_rows);
        for r in 0..self.num_rows {
            let mut row = Vec::with_capacity(self.num_cols);
            for c in 0..self.num_cols {
                let slot = self.grid[r * self.num_cols + c];
                row.push(match self.slot_cell(slot, r, c) {
                    Some(cell) => cell.to_value(false),
                    None => Value::Null,
                });
            }
            rows.push(Value::Array(row));
        }
        data.insert("grid".into(), Value::Array(rows));
        Value::Object(data)
    }

    /// docling's page clamp ([`clamp_boxes_to_pages`]) on every cell box —
    /// once per cell: the grid's copies are the same cells.
    fn clamp(&mut self, (w, h): (f64, f64)) {
        for bbox in self.cells.iter_mut().filter_map(|c| c.bbox.as_mut()) {
            for (v, hi) in bbox.iter_mut().zip([w, h, w, h]) {
                // A non-finite coordinate is `null` in the JSON, which the
                // clamp leaves alone.
                if v.is_finite() {
                    *v = round2(v.clamp(0.0, hi.max(0.0)));
                }
            }
        }
    }
}

impl serde::Serialize for TableData {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        struct Cells<'a>(&'a [GridCell]);
        impl serde::Serialize for Cells<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut seq = s.serialize_seq(Some(self.0.len()))?;
                for c in self.0 {
                    seq.serialize_element(&CellOut(c, true))?;
                }
                seq.end()
            }
        }
        struct Row<'a>(&'a TableData, usize);
        impl serde::Serialize for Row<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let Row(t, r) = *self;
                let mut seq = s.serialize_seq(Some(t.num_cols))?;
                for c in 0..t.num_cols {
                    match t.slot_cell(t.grid[r * t.num_cols + c], r, c) {
                        Some(cell) => seq.serialize_element(&CellOut(&cell, false))?,
                        None => seq.serialize_element(&())?,
                    }
                }
                seq.end()
            }
        }
        struct Grid<'a>(&'a TableData);
        impl serde::Serialize for Grid<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut seq = s.serialize_seq(Some(self.0.num_rows))?;
                for r in 0..self.0.num_rows {
                    seq.serialize_element(&Row(self.0, r))?;
                }
                seq.end()
            }
        }
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("table_cells", &Cells(&self.cells))?;
        m.serialize_entry("num_rows", &self.num_rows)?;
        m.serialize_entry("num_cols", &self.num_cols)?;
        m.serialize_entry("orientation", "rot_0")?;
        m.serialize_entry("grid", &Grid(self))?;
        m.end()
    }
}

/// A table cell as the `Value`-walking serializers read it — from a typed
/// [`GridCell`] or from a `table_cells` entry of docling JSON (a document
/// Python docling wrote, read by [`crate::pandoc::from_docling_json`]): a
/// missing field reads as `0` / `""` / `false`, like the `Value` getters
/// those serializers used.
pub(crate) struct CellView<'a> {
    pub start_row: usize,
    pub end_row: usize,
    pub start_col: usize,
    pub end_col: usize,
    pub row_span: usize,
    pub col_span: usize,
    pub text: &'a str,
    pub column_header: bool,
    pub row_header: bool,
    pub row_section: bool,
    /// A rich cell's `ref.$ref`.
    pub cell_ref: Option<&'a str>,
}

/// A table's `data` as [`CellView`]s: `num_rows`, `num_cols` and the
/// `table_cells` (the derived `grid` is not read).
pub(crate) struct TableView<'a> {
    pub num_rows: usize,
    pub num_cols: usize,
    pub cells: Vec<CellView<'a>>,
}

/// The [`TableView`] of a table item's `data`: the typed table it names
/// ([`TYPED_TABLE`], an index into `typed`), or the JSON's own fields.
pub(crate) fn table_view<'a>(data: &'a Value, typed: &'a [TableData]) -> TableView<'a> {
    let ref_of = |r: Option<&'a Value>| r.and_then(|r| r.get("$ref")).and_then(Value::as_str);
    if let Some(t) = data
        .get(TYPED_TABLE)
        .and_then(Value::as_u64)
        .and_then(|i| typed.get(i as usize))
    {
        return TableView {
            num_rows: t.num_rows,
            num_cols: t.num_cols,
            cells: t
                .cells
                .iter()
                .map(|c| CellView {
                    start_row: c.start_row,
                    end_row: c.end_row,
                    start_col: c.start_col,
                    end_col: c.end_col,
                    row_span: c.row_span,
                    col_span: c.col_span,
                    text: &c.text,
                    column_header: c.column_header,
                    row_header: c.row_header,
                    row_section: c.row_section,
                    cell_ref: ref_of(c.cell_ref.as_deref()),
                })
                .collect(),
        };
    }
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
    let flag = |v: &Value, k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    TableView {
        num_rows: num(data, "num_rows"),
        num_cols: num(data, "num_cols"),
        cells: data
            .get("table_cells")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| CellView {
                start_row: num(c, "start_row_offset_idx"),
                end_row: num(c, "end_row_offset_idx"),
                start_col: num(c, "start_col_offset_idx"),
                end_col: num(c, "end_col_offset_idx"),
                row_span: num(c, "row_span"),
                col_span: num(c, "col_span"),
                text: c.get("text").and_then(Value::as_str).unwrap_or(""),
                column_header: flag(c, "column_header"),
                row_header: flag(c, "row_header"),
                row_section: flag(c, "row_section"),
                cell_ref: ref_of(c.get("ref")),
            })
            .collect(),
    }
}

/// A `tables` entry: the item's JSON with a `null` stand-in under `data`
/// — so every edit of the item (children, content layer, comment
/// back-refs) works on the `Value` as before and keeps its key order —
/// and the typed [`TableData`] that takes the stand-in's place when the
/// item is written or turned into a `Value`.
struct TableItem {
    item: Value,
    data: TableData,
}

impl TableItem {
    fn into_value(self) -> Value {
        let TableItem { mut item, data } = self;
        if let Some(slot) = item.get_mut("data") {
            *slot = data.into_value();
        }
        item
    }
}

impl serde::Serialize for TableItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let Value::Object(obj) = &self.item else {
            return self.item.serialize(s);
        };
        let mut m = s.serialize_map(Some(obj.len()))?;
        for (k, v) in obj {
            if k == "data" {
                m.serialize_entry(k, &self.data)?;
            } else {
                m.serialize_entry(k, v)?;
            }
        }
        m.end()
    }
}

/// `TableData` from a table whose cell text is Markdown-flavoured (the flat
/// nodes: escapes to undo, GFM hard-break markers to strip).
fn table_data(t: &Table) -> TableData {
    table_data_with(t, false)
}

/// `TableData`; with `raw` the cell text is docling's own raw cell text (a
/// backend-built tree) and is written verbatim.
fn table_data_with(t: &Table, raw: bool) -> TableData {
    let cell_text = |s: &str| -> Box<str> {
        if raw {
            s.into()
        } else {
            unescape_text(&crate::markdown::strip_hard_breaks(s)).into()
        }
    };
    let widen = |b: [f32; 4]| b.map(f64::from);
    let mut num_rows = t.rows.len();
    let mut num_cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
    // Rectangular `rows` are the grid, and cells reaching past it are
    // clipped — docling's own `TableData.grid` on a USPTO table whose
    // replicated cells outrun `num_cols`, or an HTML rowspan past the last
    // row. Ragged rows are no grid at all: a backend that compacts them
    // (xlsx `skip_empty_cells`, #271) keeps the true offsets in first-class
    // cells, so there the grid spans every cell's extent and the omitted
    // positions come back as docling's empty filler cells.
    let ragged = t.rows.iter().any(|r| r.len() != num_cols);
    if let Some(first_class) = t.cells.as_ref().filter(|c| ragged && !c.is_empty()) {
        let (mut rows, mut cols) = (num_rows, num_cols);
        for c in first_class {
            rows = rows.max(c.start_row.saturating_add(c.row_span));
            cols = cols.max(c.start_col.saturating_add(c.col_span));
        }
        // A cell whose declared extent reaches past the cell budget (#861)
        // does not size the slot map; the grid stays the rows' own and the
        // cell is clipped to it, like any other cell reaching past the grid.
        if rows.saturating_mul(cols) <= crate::limits::max_table_cells() {
            num_rows = rows;
            num_cols = cols;
        }
    }
    let mut cells = Vec::new();
    // Grid slot → the cell covering it, row-major — no per-slot copy and
    // nothing hashed.
    let mut grid: Vec<GridSlot>;
    if let Some(first_class) = t.cells.as_ref().filter(|c| !c.is_empty()) {
        // A position no cell covers is docling's empty filler cell.
        grid = vec![GridSlot::Filler; num_rows * num_cols];
        cells.reserve(first_class.len());
        for c in first_class {
            let idx = cells.len();
            cells.push(GridCell {
                row_span: c.row_span,
                col_span: c.col_span,
                start_row: c.start_row,
                end_row: c.start_row + c.row_span,
                start_col: c.start_col,
                end_col: c.start_col + c.col_span,
                text: cell_text(&c.text),
                column_header: c.column_header,
                row_header: c.row_header,
                row_section: c.row_section,
                bbox: c.bbox.map(widen),
                cell_ref: None,
            });
            for r in c.start_row..(c.start_row + c.row_span).min(num_rows) {
                for k in c.start_col..(c.start_col + c.col_span).min(num_cols) {
                    grid[r * num_cols + k] = GridSlot::Cell(idx);
                }
            }
        }
    } else {
        grid = vec![GridSlot::Null; num_rows * num_cols];
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
        // An anchor's cell, by slot, once it is created (anchors precede
        // the slots they cover in row-major order).
        let mut anchor_cell: Vec<Option<usize>> = vec![None; num_rows * num_cols];
        for (r, row) in t.rows.iter().enumerate() {
            for c in 0..num_cols {
                let (ar, ac) = anchors[r * num_cols + c];
                if (ar, ac) == (r, c) {
                    let (er, ec) = extent[r * num_cols + c];
                    let text = row.get(c).map(|s| cell_text(s)).unwrap_or_default();
                    let column_header = match s.filter(|s| !s.col_header.is_empty()) {
                        Some(s) => flag(Some(&s.col_header), r, c),
                        None => r == 0,
                    };
                    anchor_cell[r * num_cols + c] = Some(cells.len());
                    cells.push(GridCell {
                        row_span: er - r + 1,
                        col_span: ec - c + 1,
                        start_row: r,
                        end_row: er + 1,
                        start_col: c,
                        end_col: ec + 1,
                        text,
                        column_header,
                        row_header: flag(s.map(|s| &s.row_header), r, c),
                        row_section: false,
                        bbox: None,
                        cell_ref: None,
                    });
                }
                if let Some(i) = anchor_cell[ar * num_cols + ac] {
                    grid[r * num_cols + c] = GridSlot::Cell(i);
                }
            }
        }
    }
    TableData {
        cells,
        num_rows,
        num_cols,
        grid,
    }
}

/// An item's `prov` array as the builder holds it: empty, or docling's one
/// `ProvenanceItem` — kept typed so a text item need not be a `Value` until
/// it is written (see [`Item`]).
#[derive(Clone, Copy)]
enum ProvSlot {
    None,
    One {
        page_no: usize,
        /// `l t r b`, already rounded to 2 decimals.
        bbox: [f64; 4],
        bottom_left: bool,
        charspan: [usize; 2],
    },
}

impl ProvSlot {
    fn origin(bottom_left: bool) -> &'static str {
        if bottom_left {
            "BOTTOMLEFT"
        } else {
            "TOPLEFT"
        }
    }

    /// The `prov` array: `page_no`, `bbox` (`l t r b` + `coord_origin`) and
    /// `charspan`, in pydantic field order.
    fn to_value(self) -> Value {
        let ProvSlot::One {
            page_no,
            bbox: [l, t, r, b],
            bottom_left,
            charspan,
        } = self
        else {
            return Value::Array(Vec::new());
        };
        let bbox = object([
            ("l", l.into()),
            ("t", t.into()),
            ("r", r.into()),
            ("b", b.into()),
            ("coord_origin", Self::origin(bottom_left).into()),
        ]);
        let span = Value::Array(vec![charspan[0].into(), charspan[1].into()]);
        Value::Array(vec![object([
            ("page_no", page_no.into()),
            ("bbox", bbox),
            ("charspan", span),
        ])])
    }

    /// docling's page clamp ([`clamp_boxes_to_pages`]) on the typed box.
    fn clamp(&mut self, sizes: &PageSizes) {
        if let ProvSlot::One { page_no, bbox, .. } = self {
            if let Some(&(w, h)) = sizes.get(page_no) {
                for (v, hi) in bbox.iter_mut().zip([w, h, w, h]) {
                    // A non-finite coordinate is `null` in the JSON, which the
                    // clamp leaves alone.
                    if v.is_finite() {
                        *v = round2(v.clamp(0.0, hi.max(0.0)));
                    }
                }
            }
        }
    }
}

impl serde::Serialize for ProvSlot {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        let ProvSlot::One {
            page_no,
            bbox: [l, t, r, b],
            bottom_left,
            charspan,
        } = *self
        else {
            return s.serialize_seq(Some(0))?.end();
        };
        struct Bbox([f64; 4], &'static str);
        impl serde::Serialize for Bbox {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut m = s.serialize_map(Some(5))?;
                for (k, v) in ["l", "t", "r", "b"].iter().zip(self.0) {
                    m.serialize_entry(k, &v)?;
                }
                m.serialize_entry("coord_origin", self.1)?;
                m.end()
            }
        }
        struct Entry(usize, Bbox, [usize; 2]);
        impl serde::Serialize for Entry {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("page_no", &self.0)?;
                m.serialize_entry("bbox", &self.1)?;
                m.serialize_entry("charspan", &self.2)?;
                m.end()
            }
        }
        let mut seq = s.serialize_seq(Some(1))?;
        seq.serialize_element(&Entry(
            page_no,
            Bbox([l, t, r, b], Self::origin(bottom_left)),
            charspan,
        ))?;
        seq.end()
    }
}

/// The plain text item — docling's `TextItem` with no children, on the body
/// layer, `orig` equal to `text` and no extra fields: what a PDF's text layer
/// is made of, millions of times over on a long document. As a `Value` each
/// cost ~2.7 KB (a map of owned keys per object); typed it is ~150 bytes, and
/// it serializes to exactly the same JSON. Its `self_ref` is its position
/// (`#/texts/{i}`), so it is not stored.
struct TextItem {
    parent: Box<str>,
    label: Box<str>,
    prov: ProvSlot,
    text: Box<str>,
}

impl TextItem {
    fn into_value(self, index: usize) -> Value {
        let text = String::from(self.text);
        object([
            ("self_ref", Value::String(format!("#/texts/{index}"))),
            ("parent", ref_value(self.parent)),
            ("children", Value::Array(Vec::new())),
            ("content_layer", "body".into()),
            ("label", Value::String(self.label.into())),
            ("prov", self.prov.to_value()),
            ("orig", Value::String(text.clone())),
            ("text", Value::String(text)),
        ])
    }
}

/// A `texts` entry: the compact [`TextItem`] until something needs to edit
/// it as JSON (a nested list's children, a content-layer move, a comment
/// back-ref), then the `Value` it stands for. Both boxed: a slot is 16 bytes.
enum Item {
    Text(Box<TextItem>),
    Json(Box<Value>),
}

impl Item {
    fn json(v: Value) -> Self {
        Item::Json(Box::new(v))
    }

    /// The item at `texts[index]` as an editable `Value`, converting a
    /// [`TextItem`] in place.
    fn value_mut(&mut self, index: usize) -> &mut Value {
        if let Item::Text(t) = self {
            let t = std::mem::replace(
                t,
                Box::new(TextItem {
                    parent: "".into(),
                    label: "".into(),
                    prov: ProvSlot::None,
                    text: "".into(),
                }),
            );
            *self = Item::json(t.into_value(index));
        }
        match self {
            Item::Json(v) => v,
            Item::Text(_) => unreachable!("converted above"),
        }
    }

    fn into_value(self, index: usize) -> Value {
        match self {
            Item::Text(t) => t.into_value(index),
            Item::Json(v) => *v,
        }
    }
}

/// The `texts` array, serialized in place.
struct Texts<'a>(&'a [Item]);

impl serde::Serialize for Texts<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        struct Text<'a>(usize, &'a TextItem);
        impl serde::Serialize for Text<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let Text(i, t) = *self;
                let mut m = s.serialize_map(Some(8))?;
                m.serialize_entry("self_ref", &format_args!("#/texts/{i}"))?;
                m.serialize_entry("parent", &Child::Ref(t.parent.clone()))?;
                m.serialize_entry("children", &[(); 0])?;
                m.serialize_entry("content_layer", "body")?;
                m.serialize_entry("label", &t.label)?;
                m.serialize_entry("prov", &t.prov)?;
                m.serialize_entry("orig", &t.text)?;
                m.serialize_entry("text", &t.text)?;
                m.end()
            }
        }
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for (i, item) in self.0.iter().enumerate() {
            match item {
                Item::Text(t) => seq.serialize_element(&Text(i, t))?,
                Item::Json(v) => seq.serialize_element(v)?,
            }
        }
        seq.end()
    }
}

/// An entry of a `children` list as the walk collects it: a `{"$ref": …}`
/// held as its target string (a body of millions of refs costs ~40 bytes
/// each, not a ~270-byte `Value` map), or any other JSON.
enum Child {
    Ref(Box<str>),
    Json(Box<Value>),
}

impl Child {
    fn json(v: Value) -> Self {
        Child::Json(Box::new(v))
    }

    fn into_value(self) -> Value {
        match self {
            Child::Ref(r) => ref_value(r),
            Child::Json(v) => *v,
        }
    }
}

impl serde::Serialize for Child {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Child::Ref(r) => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("$ref", r)?;
                m.end()
            }
            Child::Json(v) => v.serialize(s),
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[derive(Default)]
struct Builder {
    /// Write the internal `_notes` / `_note_body` keys of tree text items
    /// ([`crate::tree::TreeItem::notes`]) — for the Pandoc writer only
    /// ([`to_json_with_notes`]); docling's JSON has no such fields.
    note_keys: bool,
    texts: Vec<Item>,
    groups: Vec<Value>,
    tables: Vec<TableItem>,
    pictures: Vec<Value>,
    field_regions: Vec<Value>,
    field_items: Vec<Value>,
    key_value_items: Vec<Value>,
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
    /// The enclosing [`Node::Prov`] wrapper's (or the tree item's) exact
    /// provenance, which takes precedence over the grid box.
    pending_exact: Option<ExactProv>,
    /// The enclosing [`Node::Track`]'s timing, for the text item it wraps
    /// (#614).
    pending_track: Option<(crate::tree::TreeTrack, String)>,
    /// [`Node::Anchor`]s met since the last text item (#852): that item's
    /// `_anchors` on the Pandoc path.
    pending_anchors: Vec<String>,
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
    /// The last `comment_section` group emitted, as `(name, self_ref)`: a
    /// following section of the same name adds its note to that group rather
    /// than opening another — one `comment-{sheet}-{cell}` group holds every
    /// message of a threaded comment (docling#4353).
    last_comment_section: Option<(String, String)>,
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
        self.take_prov_slot(char_len).to_value()
    }

    /// [`Self::take_prov`], typed.
    fn take_prov_slot(&mut self, char_len: usize) -> ProvSlot {
        let prov = self.prov_slot(char_len, false);
        self.pending_exact = None;
        self.pending_loc = None;
        prov
    }

    /// The pending provenance without consuming it. An exact [`Node::Prov`]
    /// box wins over the grid; its own `charspan` is used unless
    /// `span_over_text` asks for `[0, char_len]` (a chart caption's span
    /// covers the caption text where the chart's is `[0, 0]`).
    fn prov_slot(&self, char_len: usize, span_over_text: bool) -> ProvSlot {
        let r2 = round2;
        if let Some(ExactProv {
            page_no,
            bbox: [l, t, r, b],
            bottom_left,
            charspan,
        }) = self.pending_exact
        {
            let charspan = if span_over_text {
                [0, char_len]
            } else {
                charspan
            };
            return ProvSlot::One {
                page_no,
                bbox: [r2(l), r2(t), r2(r), r2(b)],
                bottom_left,
                charspan,
            };
        }
        let Some([x0, y0, x1, y1]) = self.pending_loc else {
            return ProvSlot::None;
        };
        // An all-zero grid box is the sentinel for "this item has no geometry"
        // (a slide's speaker notes, say). docling writes a zero bbox for it,
        // not a box spanning the page, which is what denormalizing would give.
        if [x0, y0, x1, y1] == [0, 0, 0, 0] {
            return ProvSlot::One {
                page_no: self.cur_page,
                bbox: [0.0; 4],
                bottom_left: true,
                charspan: [0, char_len],
            };
        }
        ProvSlot::One {
            page_no: self.cur_page,
            bbox: [
                r2(x0 as f64 * self.cur_w / 512.0),
                r2(self.cur_h - y0 as f64 * self.cur_h / 512.0),
                r2(x1 as f64 * self.cur_w / 512.0),
                r2(self.cur_h - y1 as f64 * self.cur_h / 512.0),
            ],
            bottom_left: true,
            charspan: [0, char_len],
        }
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
                    .map(ref_value)
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
                // Without `preserve-order` the map is sorted and the position
                // is moot (#970); `remove` is the only removal it has.
                #[cfg(feature = "preserve-order")]
                obj.shift_remove(k);
                #[cfg(not(feature = "preserve-order"))]
                obj.remove(k);
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
        if self_ref.starts_with("#/texts/") {
            return self.texts.get_mut(idx).map(|item| item.value_mut(idx));
        }
        if self_ref.starts_with("#/tables/") {
            return self.tables.get_mut(idx).map(|t| &mut t.item);
        }
        let bucket = if self_ref.starts_with("#/pictures/") {
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
        let (mut nt, mut ng, mut ntb, mut np, mut nf, mut nk) = (0, 0, 0, 0, 0, 0);
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
                TreeKind::KeyValueGraph { .. } => {
                    nk += 1;
                    format!("#/key_value_items/{}", nk - 1)
                }
            };
            refs.push(r);
        }
        let ref_of = |id: usize| ref_value(refs[id].as_str());
        for (id, item) in tree.items.iter().enumerate() {
            if item.deleted {
                continue;
            }
            let parent = item.parent.map_or("#/body", |p| refs[p].as_str());
            // A deleted item has no ref; one still listed as a child (a stale
            // backend link) is left out rather than written as `"$ref": ""`,
            // which docling-core rejects on load (#527).
            let children: Vec<Value> = item
                .children
                .iter()
                .filter(|&&c| !tree.items[c].deleted)
                .map(|&c| ref_of(c))
                .collect();
            let layer = item.layer.map_or("body", |l| l.value());
            // The item's own provenance, consumed by the writer below
            // (`take_prov`); an item without one writes `prov: []`.
            self.pending_exact = item.prov.as_ref().map(ExactProv::from);
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
                    let prov = self.take_prov(text.chars().count());
                    let mut item_json = json!({
                        "self_ref": r,
                        "parent": { "$ref": parent },
                        "children": children,
                        "content_layer": layer,
                        "label": label,
                        "prov": prov,
                    });
                    // docling's `comments` back-refs sit between `prov` and
                    // `orig`, and are written only when set.
                    if !item.comments.is_empty() {
                        item_json["comments"] =
                            Value::Array(item.comments.iter().map(|&c| ref_of(c)).collect());
                    }
                    // `DocItem.source`, likewise only when set: a WebVTT
                    // cue's `TrackSource`, whose `None` fields are omitted.
                    if let Some(track) = &item.source {
                        item_json["source"] = json!([track_json(track)]);
                    }
                    merge(
                        &mut item_json,
                        json!({
                            "orig": orig.as_deref().unwrap_or(text),
                            "text": text,
                        }),
                    );
                    merge(&mut item_json, Value::Object(tail));
                    if self.note_keys {
                        if !item.notes.is_empty() {
                            item_json["_notes"] = Value::Array(
                                item.notes
                                    .iter()
                                    .map(|n| json!([n.offset, n.text]))
                                    .collect(),
                            );
                        }
                        if item.note_body {
                            item_json["_note_body"] = json!(true);
                        }
                        if !item.anchors.is_empty() {
                            item_json["_anchors"] = json!(item.anchors);
                        }
                        if let Some(number) = &item.number {
                            item_json["_number"] = json!(number);
                        }
                        if let Some(rev) = &item.revision {
                            let mut r = serde_json::Map::new();
                            r.insert("kind".into(), json!(rev.kind.as_str()));
                            if let Some(a) = &rev.author {
                                r.insert("author".into(), json!(a));
                            }
                            if let Some(d) = &rev.date {
                                r.insert("date".into(), json!(d));
                            }
                            item_json["_revision"] = Value::Object(r);
                        }
                    }
                    self.texts.push(Item::json(item_json));
                    r
                }
                TreeKind::Code {
                    text,
                    orig,
                    language,
                    formatting,
                    hyperlink,
                    captions,
                } => {
                    let r = format!("#/texts/{}", self.texts.len());
                    let prov = self.take_prov(text.chars().count());
                    let mut item_json = json!({
                        "self_ref": r,
                        "parent": { "$ref": parent },
                        "children": children,
                        "content_layer": layer,
                        "label": "code",
                        "prov": prov,
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
                            "captions": captions.iter().map(|&c| ref_of(c)).collect::<Vec<_>>(),
                            "references": [],
                            "footnotes": [],
                            "code_language": code_language(language.as_deref()),
                        }),
                    );
                    self.texts.push(Item::json(item_json));
                    r
                }
                TreeKind::Group { label, name } => {
                    self.pending_exact = None;
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
                    t.item["children"] = Value::Array(children);
                    t.item["content_layer"] = json!(layer);
                    t.item["captions"] =
                        Value::Array(captions.iter().map(|&c| ref_of(c)).collect());
                    // A `RichTableCell` is the plain cell plus a `ref` to the
                    // group holding its content — on `table_cells` only; the
                    // derived `grid` shows plain cells.
                    for &(row, col, group) in rich_cells {
                        let cell_ref = ref_of(group);
                        for c in t.data.cells.iter_mut() {
                            if c.start_row == row && c.start_col == col {
                                c.cell_ref = Some(Box::new(cell_ref.clone()));
                            }
                        }
                    }
                    r
                }
                TreeKind::Picture {
                    captions,
                    image,
                    classification,
                    extra_classes,
                    description,
                    confidence,
                    chart,
                    dpi,
                } => {
                    // A chart's meta: the kind as the one classification
                    // prediction (pydantic's field order puts `confidence`
                    // first when present), then the reconstructed data grid
                    // (#405).
                    let mut meta = classification.as_ref().map(|c| {
                        let mut pred = serde_json::Map::new();
                        if let Some(conf) = confidence {
                            pred.insert("confidence".into(), json!(conf));
                        }
                        pred.insert("class_name".into(), json!(c));
                        let mut preds = vec![Value::Object(pred)];
                        preds.extend(extra_classes.iter().map(|c| json!({ "class_name": c })));
                        json!({ "classification": { "predictions": preds } })
                    });
                    if let (Some(m), Some(t)) = (meta.as_mut(), chart) {
                        if !t.rows.is_empty() {
                            m["tabular_chart"] =
                                object([("chart_data", table_data(t).into_value())]);
                        }
                    }
                    // The OCR description (#645) goes first in the meta —
                    // `PictureMeta` field order — and as the annotation the
                    // flat node writes (see `description_meta`).
                    if let Some(desc) = description {
                        let mut d = description_meta(desc);
                        let mut merged = Map::new();
                        merged.insert("description".into(), d["description"].take());
                        if let Some(Value::Object(rest)) = meta {
                            merged.extend(rest);
                        }
                        merged.insert("annotations".into(), d["annotations"].take());
                        meta = Some(Value::Object(merged));
                    }
                    // The source of a picture no engine rendered (a LaTeX
                    // `tikzpicture`): `PictureMeta.code`.
                    if let Some((text, language)) = &item.code_meta {
                        let code = json!({ "text": text, "language": language });
                        match meta.as_mut() {
                            Some(Value::Object(m)) => {
                                m.insert("code".into(), code);
                            }
                            _ => meta = Some(json!({ "code": code })),
                        }
                    }
                    let prov = self.take_prov(0);
                    let r = self.push_picture(
                        prov,
                        captions.iter().map(|&c| ref_of(c)).collect(),
                        children,
                        image.as_ref(),
                        meta,
                        parent,
                    );
                    if let Some(idx) = ref_index(&r) {
                        self.pictures[idx]["content_layer"] = json!(layer);
                        // The image's dpi is the file's when the backend read
                        // it (python-pptx does); the default 72 otherwise.
                        if let (Some(dpi), Some(img)) = (dpi, self.pictures[idx].get_mut("image")) {
                            img["dpi"] = json!(dpi);
                        }
                    }
                    r
                }
                TreeKind::FieldRegion { items } => {
                    self.pending_exact = None;
                    let r = self.add_field_region(items, parent);
                    if let Some(region) = self.field_regions.last_mut() {
                        region["content_layer"] = json!(layer);
                    }
                    r
                }
                TreeKind::KeyValueGraph { cells, links } => {
                    self.pending_exact = None;
                    let r = self.add_key_value_graph(cells, links, parent);
                    if let Some(item) = self.key_value_items.last_mut() {
                        item["content_layer"] = json!(layer);
                    }
                    r
                }
            };
            debug_assert_eq!(self_ref, refs[id], "tree item {id} numbered out of order");
            // A float's own footnotes (`FloatingItem.footnotes`).
            if !item.footnotes.is_empty() {
                let notes: Vec<Value> = item
                    .footnotes
                    .iter()
                    .filter(|&&f| !tree.items[f].deleted)
                    .map(|&f| ref_of(f))
                    .collect();
                if let Some(v) = self.item_mut(&self_ref) {
                    v["footnotes"] = Value::Array(notes);
                }
            }
        }
        tree.body
            .iter()
            .filter(|&&c| !tree.items[c].deleted)
            .map(|&c| ref_of(c))
            .collect()
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
            // A PDF footnote: docling's own label, and its link as the item's
            // `hyperlink` rather than Markdown baked into the text.
            Node::LabeledText { label, text, href } => {
                let extra = match href {
                    Some(url) => json!({ "hyperlink": url }),
                    None => json!({}),
                };
                Some(self.add_text(label, text, parent, extra))
            }
            Node::Paragraph { text } | Node::ListText { text, .. } => {
                // A whole-paragraph display equation is a formula item (docling
                // wraps it in `$$…$$` and, unlike a text item, never escapes it).
                let t = text.trim();
                match t.strip_prefix("$$").and_then(|s| s.strip_suffix("$$")) {
                    Some(inner) if !inner.is_empty() => Some(self.add_formula(inner, parent)),
                    _ => Some(self.add_text("text", text, parent, json!({}))),
                }
            }
            Node::CheckboxItem { checked, text } => {
                // docling's checkbox item: the `checkbox_selected` /
                // `checkbox_unselected` label carries the state and the text
                // is the bare option label (right_to_left_03's `خير`,
                // docx_checkboxes' `Orange juice`). The `- [x]` task-list
                // marker is Markdown's rendering of it, not item text (#609).
                let label = if *checked {
                    "checkbox_selected"
                } else {
                    "checkbox_unselected"
                };
                Some(self.add_text(label, text, parent, json!({})))
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
                if let Some((_, self_ref)) = self
                    .last_comment_section
                    .as_ref()
                    .filter(|(n, _)| n == name)
                    .cloned()
                {
                    // Another message of the same thread: a further note in
                    // the group already open for this cell.
                    let child =
                        self.add_text("text", text, &self_ref, json!({ "content_layer": "notes" }));
                    if let Some(children) =
                        self.groups[group_index(&self_ref)]["children"].as_array_mut()
                    {
                        children.push(ref_value(child.as_str()));
                    }
                    self.comment_groups.push(if *refs_note_text {
                        child
                    } else {
                        self_ref.clone()
                    });
                    // The group is already its parent's child: listing it
                    // again put a thread's group in the body once per message.
                    return None;
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
                self.last_comment_section = Some((name.clone(), self_ref.clone()));
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
                description,
                caption_parent,
                caption_location,
            } => Some(self.add_picture(
                caption.as_deref(),
                caption_href.as_deref(),
                image.as_ref(),
                picture_meta(classification.as_deref(), description.as_ref()),
                parent,
                (*caption_parent, *caption_location),
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
                    meta["tabular_chart"] =
                        object([("chart_data", table_data(table).into_value())]);
                }
                // docling's office backends add the chart's title as a caption
                // item of the *container* (the sheet group, the slide), listed
                // before the picture that references it, with the chart's own
                // box and a charspan over the caption text — not as a child
                // of the picture, which is where a PDF caption lives.
                let mut captions = Vec::new();
                if let Some(cap) = caption.as_deref().filter(|c| !c.is_empty()) {
                    let prov = self.prov_slot(unescape_text(cap).chars().count(), true);
                    let cap_ref = self.add_text_with("caption", cap, parent, json!({}), prov);
                    self.pending_siblings.push(ref_value(cap_ref.as_str()));
                    captions.push(ref_value(cap_ref));
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
            Node::KeyValueGraph { cells, links } => {
                Some(self.add_key_value_graph(cells, links, parent))
            }
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
            // PDF page headers and footers: docling writes them as body-parented
            // `page_header`/`page_footer` text items on the furniture layer, so a
            // JSON consumer can read running headers (printed page ids, dates).
            Node::PageFurniture {
                footer,
                location,
                text,
            } => {
                if self.cur_page > 0 {
                    self.pending_loc = Some(*location);
                }
                let label = if *footer {
                    "page_footer"
                } else {
                    "page_header"
                };
                let item = self.add_text(label, text, parent, json!({}));
                self.pending_loc = None;
                self.set_layer(&item, "furniture");
                Some(item)
            }
            // A labelled furniture paragraph (a `.doc` header / footer or
            // note body, #535): the item docling's office backends write.
            Node::FurnitureText { label, text } => {
                let item = self.add_text(label, text, parent, json!({}));
                self.set_layer(&item, "furniture");
                Some(item)
            }
            // A PDF picture's contained text (docling's `_add_child_elements`):
            // items parented to the picture just written, listed after its
            // caption in that picture's `children`.
            Node::PictureChildren(children) => {
                let pic = self.pictures.len().checked_sub(1)?;
                let pic_ref = format!("#/pictures/{pic}");
                let refs = self.walk_into(children, &pic_ref);
                if let Some(list) = self.pictures[pic]["children"].as_array_mut() {
                    list.extend(refs.into_iter().map(Child::into_value));
                }
                None
            }
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
            Node::Anchor { id } => {
                self.pending_anchors.push(id.clone());
                None
            }
            Node::Track { track, cue, inner } => {
                self.pending_track = Some((track.clone(), cue.clone()));
                let r = self.add_node(inner, parent);
                self.pending_track = None;
                r
            }
            Node::Prov {
                page_no,
                bbox,
                charspan,
                bottom_left,
                inner,
                ..
            } => {
                self.pending_exact = Some(ExactProv {
                    page_no: *page_no,
                    bbox: bbox.map(f64::from),
                    bottom_left: *bottom_left,
                    charspan: *charspan,
                });
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
            item_refs.push(ref_value(self.add_field_item(item, &self_ref)));
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

    /// A `KeyValueItem`: docling's `GraphData` written cell for cell and link
    /// for link (`key_value_items/N`), the item itself childless and without
    /// provenance, the way the XBRL backend creates it.
    fn add_key_value_graph(
        &mut self,
        cells: &[crate::GraphCell],
        links: &[crate::GraphLink],
        parent: &str,
    ) -> String {
        let self_ref = format!("#/key_value_items/{}", self.key_value_items.len());
        let cells: Vec<Value> = cells
            .iter()
            .map(|c| {
                json!({
                    "label": c.label,
                    "cell_id": c.cell_id,
                    "text": c.text,
                    "orig": c.orig,
                })
            })
            .collect();
        let links: Vec<Value> = links
            .iter()
            .map(|l| {
                json!({
                    "label": l.label,
                    "source_cell_id": l.source_cell_id,
                    "target_cell_id": l.target_cell_id,
                })
            })
            .collect();
        self.key_value_items.push(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "key_value_region",
            "prov": [],
            "captions": [],
            "references": [],
            "footnotes": [],
            "graph": { "cells": cells, "links": links },
        }));
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
                child_refs.push(ref_value(self.add_text(label, text, &self_ref, extra)));
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
        if let Some(item) = self.item_mut(self_ref) {
            item["content_layer"] = json!(layer);
        }
    }

    fn add_text(&mut self, label: &str, text: &str, parent: &str, extra: Value) -> String {
        let prov = self.take_prov_slot(unescape_text(text).chars().count());
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
        prov: ProvSlot,
    ) -> String {
        let index = self.texts.len();
        let self_ref = format!("#/texts/{index}");
        // A [`Node::Track`] item is docling's ASR text item (#614): the
        // segment's words as `text`, its timing as `source` — the `[time: …]`
        // prefix the wrapped paragraph shows is Markdown's alone. It carries a
        // field the compact [`TextItem`] doesn't, so it is built as JSON.
        let mut item = match self.pending_track.take() {
            None => Item::Text(Box::new(TextItem {
                parent: parent.into(),
                label: label.into(),
                prov,
                text: unescape_text(text).into(),
            })),
            Some((track, cue)) => {
                let mut m = serde_json::Map::with_capacity(9);
                m.insert("self_ref".into(), Value::String(self_ref.clone()));
                m.insert("parent".into(), ref_value(parent));
                m.insert("children".into(), Value::Array(Vec::new()));
                m.insert("content_layer".into(), "body".into());
                m.insert("label".into(), label.into());
                m.insert("prov".into(), prov.to_value());
                // `DocItem.source` follows `prov` (docling's field order).
                m.insert("source".into(), Value::Array(vec![track_json(&track)]));
                m.insert("orig".into(), Value::String(cue.clone()));
                m.insert("text".into(), Value::String(cue));
                Item::json(Value::Object(m))
            }
        };
        if extra.as_object().is_some_and(|e| !e.is_empty()) {
            merge(item.value_mut(index), extra);
        }
        // The link destinations met before this item (#852), on the Pandoc
        // path only — docling's model has no field for them.
        let anchors = std::mem::take(&mut self.pending_anchors);
        if self.note_keys && !anchors.is_empty() {
            merge(item.value_mut(index), json!({ "_anchors": anchors }));
        }
        self.texts.push(item);
        self_ref
    }

    /// A display-math formula item. `latex` is the raw content (no `$$`); docling
    /// re-wraps it and never escapes it.
    fn add_formula(&mut self, latex: &str, parent: &str) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let prov = self.take_prov(latex.chars().count());
        self.texts.push(Item::json(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "formula",
            "prov": prov,
            "orig": latex,
            "text": latex,
        })));
        self_ref
    }

    /// A CodeFormula-enriched display formula: `text` is the model's LaTeX
    /// while `orig` keeps the raw glyph extraction (docling's enriched shape;
    /// the plain [`Self::add_formula`] above sets both to the same string).
    fn add_formula_item(&mut self, latex: &str, orig: &str, parent: &str) -> String {
        let self_ref = format!("#/texts/{}", self.texts.len());
        let prov = self.take_prov(latex.chars().count());
        self.texts.push(Item::json(json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": [],
            "content_layer": "body",
            "label": "formula",
            "prov": prov,
            "orig": orig,
            "text": latex,
        })));
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
        self.texts.push(Item::json(json!({
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
        })));
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
                    self.texts[idx].value_mut(idx)["children"]
                        .as_array_mut()
                        .unwrap()
                        .extend(nested);
                }
            }
            children.push(ref_value(item_ref));
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
        self.texts.push(Item::json(json!({
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
        })));
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
            Some(cap) => self.add_caption(
                cap,
                json!({}),
                &self_ref,
                parent,
                (t.caption_parent, t.caption_location),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let data = table_data_with(t, raw);
        let item = json!({
            "self_ref": self_ref,
            "parent": { "$ref": parent },
            "children": children,
            "content_layer": "body",
            "label": "table",
            "prov": prov,
            "captions": captions,
            "references": [],
            "footnotes": [],
            // The typed `data` takes this stand-in's place on output.
            "data": null,
            "annotations": [],
        });
        self.tables.push(TableItem { item, data });
        self_ref
    }

    /// Add a picture's or table's caption text item where `choice` says it
    /// hangs (#390), returning the `captions` entry for the item and the
    /// item's own `children` (the caption, when it is the item's child).
    /// The caption never consumes the item's pending provenance — the item
    /// takes its box first. Its own box, when the backend kept one (the PDF
    /// pipeline's caption region, #609), becomes the caption's `prov` on the
    /// item's page, as docling's reading-order model gives every caption the
    /// cluster it came from; without one the caption has no `prov`.
    fn add_caption(
        &mut self,
        text: &str,
        extra: Value,
        self_ref: &str,
        parent: &str,
        (choice, location): (CaptionParent, Option<[u16; 4]>),
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
        if self.cur_page > 0 && location.is_some() {
            self.pending_loc = location;
        }
        let cap_ref = ref_value(self.add_text("caption", text, cap_parent, extra));
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
        caption_at: (CaptionParent, Option<[u16; 4]>),
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
                self.add_caption(cap, extra, &self_ref, parent, caption_at)
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
            let mut image = image_ref_json(img);
            // The displayed size, for the Pandoc writer only (#834):
            // docling's `ImageRef` has no field for it.
            if self.note_keys {
                if let Some([w, h]) = img.display_size {
                    image["_display"] = json!([w, h]);
                }
            }
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
        let children: Vec<Value> = self
            .walk_into(nodes, &self_ref)
            .into_iter()
            .map(Child::into_value)
            .collect();
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
                .enumerate()
                .map(|(k, item)| item.value_mut(t + k))
                .chain(self.tables[tb..].iter_mut().map(|t| &mut t.item))
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
    fn walk_into(&mut self, nodes: &[Node], parent: &str) -> Vec<Child> {
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
            let mut slots: Vec<Vec<Child>> = (0..nodes.len()).map(|_| Vec::new()).collect();
            for i in order {
                if let Some(r) = self.add_node(&nodes[i], parent) {
                    slots[i].extend(self.pending_siblings.drain(..).map(Child::json));
                    slots[i].push(Child::Ref(r.into()));
                    slots[i].extend(self.pending_after.drain(..).map(Child::json));
                }
                if parent == "#/body" {
                    slots[i].extend(self.pending_body.drain(..).map(Child::json));
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
                let mut lists = Vec::new();
                self.add_sibling_lists(&nodes[start..i], parent, &mut lists);
                children.extend(lists.into_iter().map(Child::json));
            } else {
                if let Some(r) = self.add_node(&nodes[i], parent) {
                    children.extend(self.pending_siblings.drain(..).map(Child::json));
                    children.push(Child::Ref(r.into()));
                    children.extend(self.pending_after.drain(..).map(Child::json));
                }
                i += 1;
            }
            // Body-parented captions of items deeper in the tree follow the
            // top-level item they were created under (#390).
            if parent == "#/body" {
                children.extend(self.pending_body.drain(..).map(Child::json));
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
                out.push(ref_value(self.add_list(&run[seg..k], parent)));
                seg = k;
            }
        }
        out.push(ref_value(self.add_list(&run[seg..], parent)));
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
    if let (Some(t), Value::Object(e)) = (target.as_object_mut(), extra) {
        t.extend(e);
    }
}

/// A JSON object of exactly these entries, in this order. The hot per-item
/// constructors use this instead of `json!`: the macro grows its map by
/// doubling (an 8-key item kept 14 slots) and deep-copies every interpolated
/// `Value` through `to_value`. On a long text-heavy PDF those item objects
/// *are* the export's footprint (the same reasoning as [`cell_value`]).
fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    let mut m = serde_json::Map::with_capacity(N);
    for (k, v) in entries {
        m.insert(k.to_owned(), v);
    }
    Value::Object(m)
}

/// A `{"$ref": r}` reference object.
fn ref_value(r: impl Into<String>) -> Value {
    object([("$ref", Value::String(r.into()))])
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
    #[cfg(feature = "preserve-order")]
    use crate::ImageMode;
    #[cfg(feature = "preserve-order")]
    use crate::PictureImage;
    use crate::{CaptionParent, ContentLayer, DoclingDocument, Node, Table};
    use serde_json::Value;

    fn cell_at(text: &str, row: usize, col: usize, col_span: usize) -> crate::TableCell {
        crate::TableCell {
            text: text.into(),
            bbox: None,
            start_row: row,
            start_col: col,
            row_span: 1,
            col_span,
            column_header: row == 0,
            row_header: false,
            row_section: false,
        }
    }

    /// First-class cells past the rows: rectangular rows are the grid and
    /// clip them (docling's `TableData.grid` on USPTO's overrun replicas);
    /// ragged rows — an xlsx table compacted by `skip_empty_cells` (#271) —
    /// are no grid, so the cells' extent sizes it and the omitted positions
    /// come back as empty filler cells in `grid` only.
    #[test]
    fn ragged_rows_take_their_grid_from_first_class_cells() {
        let cells = vec![
            cell_at("h0", 0, 0, 1),
            cell_at("h2", 0, 2, 1),
            cell_at("wide", 1, 0, 3),
        ];
        let export = |rows: Vec<Vec<String>>| -> Value {
            let mut doc = DoclingDocument::new("t");
            doc.push(Node::Table(Table {
                rows,
                cells: Some(cells.clone()),
                ..Table::default()
            }));
            let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
            v["tables"][0]["data"].clone()
        };
        // Compacted rows: 2 and 3 wide, the cells say the grid is 3 wide.
        let data = export(vec![
            vec!["h0".into(), "h2".into()],
            vec!["wide".into(), "wide".into(), "wide".into()],
        ]);
        assert_eq!(data["num_rows"], 2);
        assert_eq!(data["num_cols"], 3);
        assert_eq!(data["table_cells"].as_array().unwrap().len(), 3);
        let row0: Vec<&str> = data["grid"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["text"].as_str().unwrap())
            .collect();
        assert_eq!(row0, ["h0", "", "h2"]);
        assert_eq!(data["table_cells"][2]["col_span"], 3);
        // Rectangular rows narrower than the cells: the rows win, the cells
        // are clipped to them exactly as before.
        let data = export(vec![
            vec!["h0".into(), "h2".into()],
            vec!["wide".into(), "wide".into()],
        ]);
        assert_eq!(data["num_cols"], 2);
        assert_eq!(data["grid"][0].as_array().unwrap().len(), 2);
    }

    #[cfg(feature = "preserve-order")]
    fn doc_with_image() -> DoclingDocument {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Picture {
            caption: Some("Fig 1".into()),
            caption_href: None,
            image: Some(PictureImage {
                dpi: crate::PictureImage::DEFAULT_DPI,
                mimetype: "image/png".into(),
                width: 4,
                height: 2,
                data: b"foobar".to_vec(),
                display_size: None,
            }),
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
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
            caption_location: None,
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
                    bottom_left: false,
                    seq: Some(1),
                    inner: Box::new(Node::Table(table.clone())),
                },
                Node::Prov {
                    page_no: 1,
                    bbox: [0.0, 1.0, 1.0, 1.0],
                    charspan: [0, 0],
                    bottom_left: false,
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
    /// #609: a PDF caption and a checkbox item carry their own page and box.
    /// docling's reading-order model gives every caption the cluster it came
    /// from (`_add_caption_or_footnote`), not its picture's or table's, and
    /// a checkbox is a text item like any other.
    #[test]
    fn captions_and_checkboxes_carry_their_own_prov() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 512.0,
            height: 512.0,
        });
        doc.push(Node::Located {
            location: [64, 64, 448, 256],
            inner: Box::new(Node::Picture {
                caption: Some("Figure 1".into()),
                caption_href: None,
                image: None,
                classification: None,
                description: None,
                caption_parent: CaptionParent::Item,
                caption_location: Some([64, 264, 448, 280]),
            }),
        });
        doc.push(Node::PageInfo {
            page_no: 2,
            width: 512.0,
            height: 512.0,
        });
        doc.push(Node::Located {
            location: [64, 120, 448, 300],
            inner: Box::new(Node::Table(Table {
                rows: vec![vec!["a".into()]],
                caption: Some("Table 1".into()),
                caption_parent: CaptionParent::Item,
                caption_location: Some([64, 100, 300, 112]),
                ..Table::default()
            })),
        });
        doc.push(Node::Located {
            location: [64, 320, 200, 332],
            inner: Box::new(Node::CheckboxItem {
                checked: false,
                text: "First option".into(),
            }),
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = v["texts"].as_array().unwrap();
        let by_text = |t: &str| texts.iter().find(|x| x["text"] == t).expect(t).clone();
        let bbox = |p: &Value| {
            let b = &p["prov"][0]["bbox"];
            [&b["l"], &b["t"], &b["r"], &b["b"]].map(|x| x.as_f64().unwrap())
        };
        // Figure 1: its own box (grid == points on a 512 page, flipped to
        // BOTTOMLEFT), not the picture's.
        let fig = by_text("Figure 1");
        assert_eq!(fig["prov"][0]["page_no"], 1);
        assert_eq!(bbox(&fig), [64.0, 248.0, 448.0, 232.0]);
        assert_eq!(fig["prov"][0]["charspan"], serde_json::json!([0, 8]));
        assert_eq!(fig["parent"]["$ref"], "#/pictures/0");
        assert_eq!(v["pictures"][0]["prov"][0]["bbox"]["t"], 448.0);
        // Table 1 on page 2, its own box; the table keeps its own.
        let tab = by_text("Table 1");
        assert_eq!(tab["prov"][0]["page_no"], 2);
        assert_eq!(bbox(&tab), [64.0, 412.0, 300.0, 400.0]);
        assert_eq!(v["tables"][0]["prov"][0]["bbox"]["t"], 392.0);
        // The checkbox item.
        let cb = by_text("First option");
        assert_eq!(cb["label"], "checkbox_unselected");
        assert_eq!(cb["prov"][0]["page_no"], 2);
        assert_eq!(bbox(&cb), [64.0, 192.0, 200.0, 180.0]);
        assert!(texts
            .iter()
            .all(|t| !t["prov"].as_array().unwrap().is_empty()));

        // Without a caption box (every declarative backend) the caption
        // still has no prov, and nothing else moves.
        let mut plain = DoclingDocument::new("t");
        plain.push(Node::PageInfo {
            page_no: 1,
            width: 512.0,
            height: 512.0,
        });
        plain.push(Node::Located {
            location: [64, 64, 448, 256],
            inner: Box::new(Node::Picture {
                caption: Some("Figure 1".into()),
                caption_href: None,
                image: None,
                classification: None,
                description: None,
                caption_parent: CaptionParent::Item,
                caption_location: None,
            }),
        });
        let v: Value = serde_json::from_str(&plain.export_to_json()).unwrap();
        assert_eq!(v["texts"][0]["prov"], serde_json::json!([]));
        assert_eq!(v["pictures"][0]["prov"][0]["bbox"]["t"], 448.0);
    }

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

    /// PDF page headers/footers reach the JSON as docling writes them:
    /// body-parented text items on the furniture layer, with their box.
    #[test]
    fn page_furniture_becomes_furniture_layer_text() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 512.0,
            height: 512.0,
        });
        doc.push(Node::PageFurniture {
            footer: false,
            location: [10, 0, 100, 20],
            text: "Chapter 1".into(),
        });
        doc.push(Node::Paragraph {
            text: "body".into(),
        });
        doc.push(Node::PageFurniture {
            footer: true,
            location: [400, 490, 500, 512],
            text: "1.10.2".into(),
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = v["texts"].as_array().unwrap();
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[0]["label"], "page_header");
        assert_eq!(texts[0]["content_layer"], "furniture");
        assert_eq!(texts[0]["parent"]["$ref"], "#/body");
        assert_eq!(texts[0]["prov"][0]["bbox"]["l"], 10.0);
        assert_eq!(texts[1]["content_layer"], "body");
        assert_eq!(texts[1]["prov"], serde_json::json!([]));
        assert_eq!(texts[2]["label"], "page_footer");
        assert_eq!(texts[2]["text"], "1.10.2");
        assert_eq!(texts[2]["content_layer"], "furniture");
    }

    #[test]
    #[cfg(feature = "preserve-order")] // field-order assertions (#970)
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
            caption_location: None,
        }));

        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(v["schema_name"], "DoclingDocument");
        assert_eq!(v["version"], "1.11.0");
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
    #[cfg(feature = "preserve-order")] // field-order assertions (#970)
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
                extra_classes: Vec::new(),
                classification: Some("bar_chart".into()),
                description: None,
                confidence: None,
                chart: Some(Table {
                    rows: vec![vec!["".into(), "s".into()], vec!["c".into(), "1".into()]],
                    ..Table::default()
                }),
                dpi: None,
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

    /// A tree item's `TreeProv` is written verbatim — a raw EMU box with
    /// whatever origin tag the backend set (`BOTTOMLEFT` here) and a per-item
    /// charspan, a note's zero `TOPLEFT` box — a picture's `image.dpi` is the file's when the backend
    /// read one, an item without provenance writes `prov: []`, and the page
    /// map still comes from the flat stream's markers.
    #[test]
    fn tree_items_carry_exact_provenance_and_dpi() {
        use crate::tree::{ItemTree, TreeKind, TreeProv};
        let text = |label: &str, t: &str| TreeKind::Text {
            label: label.into(),
            text: t.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: None,
        };
        let mut t = ItemTree::default();
        let slide = t.add(
            None,
            None,
            TreeKind::Group {
                label: "chapter".into(),
                name: "slide-0".into(),
            },
        );
        t.add_with_prov(
            Some(slide),
            None,
            text("paragraph", "héllo"),
            TreeProv {
                page_no: 1,
                bbox: [914400.0, 1828800.0, 2743200.0, 457200.0],
                bottom_left: true,
                charspan: [0, 5],
            },
        );
        t.add_with_prov(
            Some(slide),
            None,
            TreeKind::Picture {
                captions: Vec::new(),
                image: Some(crate::PictureImage {
                    dpi: crate::PictureImage::DEFAULT_DPI,
                    mimetype: "image/png".into(),
                    width: 2,
                    height: 2,
                    data: vec![0],
                    display_size: None,
                }),
                extra_classes: Vec::new(),
                classification: None,
                description: None,
                confidence: None,
                chart: None,
                dpi: Some(300),
            },
            TreeProv {
                page_no: 1,
                bbox: [0.0; 4],
                bottom_left: false,
                charspan: [0, 0],
            },
        );
        t.add(
            Some(slide),
            Some(ContentLayer::Notes),
            text("text", "no geometry"),
        );
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 9144000.0,
            height: 6858000.0,
        });
        doc.tree = Some(t);
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(
            v["texts"][0]["prov"],
            serde_json::json!([{
                "page_no": 1,
                "bbox": { "l": 914400.0, "t": 1828800.0, "r": 2743200.0, "b": 457200.0, "coord_origin": "BOTTOMLEFT" },
                "charspan": [0, 5],
            }])
        );
        assert_eq!(v["texts"][0]["label"], "paragraph");
        assert_eq!(
            v["pictures"][0]["prov"][0]["bbox"]["coord_origin"],
            "TOPLEFT"
        );
        assert_eq!(v["pictures"][0]["image"]["dpi"], 300);
        assert_eq!(v["texts"][1]["prov"], serde_json::json!([]));
        assert_eq!(v["texts"][1]["content_layer"], "notes");
        assert_eq!(v["pages"]["1"]["size"]["width"], 9144000.0);
        assert_eq!(v["pages"]["1"]["page_no"], 1);
    }

    /// docling-core's `validate_document` clamps every provenance box (and a
    /// one-page table's cell boxes) into its page — the state every
    /// `ConversionResult` leaves a document in, so the state docling's JSON
    /// shows. A box on a page the document does not describe is left alone.
    #[test]
    fn provenance_boxes_are_clamped_to_their_page() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::PageInfo {
            page_no: 1,
            width: 10.0,
            height: 8.0,
        });
        doc.push(Node::Prov {
            page_no: 1,
            bbox: [-1.0, 2.0, 12.0, 9.5],
            charspan: [0, 1],
            bottom_left: false,
            seq: None,
            inner: Box::new(Node::Paragraph { text: "x".into() }),
        });
        let mut table = Table {
            rows: vec![vec!["a".into()]],
            ..Table::default()
        };
        table.cells = Some(vec![crate::TableCell {
            text: "a".into(),
            bbox: Some([1.0, 1.0, 11.0, 9.0]),
            start_row: 0,
            start_col: 0,
            row_span: 1,
            col_span: 1,
            column_header: false,
            row_header: false,
            row_section: false,
        }]);
        doc.push(Node::Prov {
            page_no: 1,
            bbox: [0.0, 0.0, 10.0, 8.0],
            charspan: [0, 0],
            bottom_left: false,
            seq: None,
            inner: Box::new(Node::Table(table)),
        });
        doc.push(Node::Prov {
            page_no: 7,
            bbox: [-5.0, 0.0, 50.0, 50.0],
            charspan: [0, 1],
            bottom_left: false,
            seq: None,
            inner: Box::new(Node::Paragraph { text: "y".into() }),
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(
            v["texts"][0]["prov"][0]["bbox"],
            serde_json::json!({ "l": 0.0, "t": 2.0, "r": 10.0, "b": 8.0, "coord_origin": "TOPLEFT" })
        );
        let cell = &v["tables"][0]["data"]["table_cells"][0]["bbox"];
        assert_eq!(
            (cell["l"].as_f64(), cell["r"].as_f64(), cell["b"].as_f64()),
            (Some(1.0), Some(10.0), Some(8.0))
        );
        assert_eq!(v["tables"][0]["data"]["grid"][0][0]["bbox"]["r"], 10.0);
        assert_eq!(
            v["texts"][1]["prov"][0]["bbox"]["r"], 50.0,
            "page 7 is not described"
        );
    }

    /// docling-core's `validate_misplaced_list_items`, as it re-homes list
    /// items outside a `list` group (#527) — the refs below are what
    /// docling-core 2.99 writes for the same document: the two consecutive
    /// items on the body share a group, each item in another group gets its
    /// own, the runs are handled last-first (new groups and re-added texts
    /// numbered in that order), and a well-placed item is left alone.
    #[test]
    fn misplaced_list_items_are_wrapped_like_docling_core() {
        use crate::tree::{ItemTree, ListMeta, TreeKind};
        let mut t = ItemTree::default();
        let li = |txt: &str| TreeKind::Text {
            label: "list_item".into(),
            text: txt.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: Some(ListMeta {
                enumerated: false,
                marker: "-".into(),
            }),
        };
        let group = |label: &str, name: &str| TreeKind::Group {
            label: label.into(),
            name: name.into(),
        };
        t.add(None, None, li("A"));
        t.add(None, None, li("B"));
        t.add(
            None,
            None,
            TreeKind::Text {
                label: "text".into(),
                text: "plain".into(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: None,
            },
        );
        let cell = t.add(None, None, group("unspecified", "cell"));
        t.add(Some(cell), None, li("C"));
        t.add(Some(cell), None, li("D"));
        let list = t.add(None, None, group("list", "list"));
        t.add(Some(list), None, li("E"));
        let doc = DoclingDocument {
            tree: Some(t),
            ..DoclingDocument::new("t")
        };
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let refs = |x: &Value| -> Vec<String> {
            x.as_array()
                .unwrap()
                .iter()
                .map(|r| r["$ref"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            refs(&v["body"]["children"]),
            ["#/groups/4", "#/texts/0", "#/groups/0", "#/groups/1"]
        );
        assert_eq!(
            refs(&v["groups"][0]["children"]),
            ["#/groups/3", "#/groups/2"]
        );
        for (g, name, kids) in [
            (2, "group", vec!["#/texts/2"]),
            (3, "group", vec!["#/texts/3"]),
            (4, "group", vec!["#/texts/4", "#/texts/5"]),
            (1, "list", vec!["#/texts/1"]),
        ] {
            assert_eq!(v["groups"][g]["label"], "list");
            assert_eq!(v["groups"][g]["name"], name);
            assert_eq!(refs(&v["groups"][g]["children"]), kids);
        }
        let texts: Vec<&str> = v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["plain", "E", "D", "C", "A", "B"]);
        assert_eq!(v["texts"][2]["parent"]["$ref"], "#/groups/2");
    }

    /// A child the tree still lists after deleting it is left out instead
    /// of written as `"$ref": ""`, which docling-core rejects (#527).
    #[test]
    fn a_deleted_child_is_not_written_as_an_empty_ref() {
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
        let g = t.add(
            None,
            None,
            TreeKind::Group {
                label: "unspecified".into(),
                name: "g".into(),
            },
        );
        let gone = t.add(Some(g), None, text(""));
        t.add(Some(g), None, text("kept"));
        t.items[gone].deleted = true; // still in `g`'s children
        let doc = DoclingDocument {
            tree: Some(t),
            ..DoclingDocument::new("t")
        };
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(
            v["groups"][0]["children"],
            serde_json::json!([{"$ref": "#/texts/0"}])
        );
    }

    #[test]
    fn a_trimmed_run_tree_gets_the_legacy_join_in_its_runs() {
        use crate::tree::{ItemTree, TreeKind, TreeProv};
        let text = |label: &str, txt: &str| TreeKind::Text {
            label: label.into(),
            text: txt.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: None,
        };
        let mut t = ItemTree::default();
        let g = t.add(
            None,
            None,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        );
        t.add(Some(g), None, text("text", "left"));
        let prov = TreeProv {
            page_no: 1,
            bbox: [0.0, 0.0, 1.0, 1.0],
            bottom_left: false,
            charspan: [0, 1],
        };
        t.add_with_prov(Some(g), None, text("formula", "a"), prov.clone());
        t.add_with_prov(Some(g), None, text("formula", "b"), prov);
        t.add(Some(g), None, text("text", "right"));
        let doc = DoclingDocument {
            tree: Some(t),
            ..DoclingDocument::new("t")
        };
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts: Vec<&str> = v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["text"].as_str().unwrap())
            .collect();
        // `left ` · `a` · ` ` (standalone, appended) · `b` · ` right`.
        assert_eq!(texts, ["left ", "a", "b", " right", " "]);
        assert_eq!(v["texts"][0]["orig"], "left");
        assert_eq!(v["groups"][0]["children"][2]["$ref"], "#/texts/4");
        assert_eq!(
            v["texts"][4]["prov"][0]["charspan"],
            serde_json::json!([0, 1])
        );
    }

    #[test]
    #[cfg(feature = "preserve-order")] // field-order assertions (#970)
    fn a_backend_item_tree_is_written_verbatim() {
        use crate::tree::{Formatting, ItemTree, ListMeta, TreeKind};
        let mut t = ItemTree {
            // Runs as written: no legacy separator injection (#997).
            inline_whitespace: true,
            ..ItemTree::default()
        };
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
                captions: Vec::new(),
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
        let item = t.add(
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
        // A list item belongs in a `list` group — one outside it would be
        // re-homed by `wrap_misplaced_list_items` (created last, so the
        // other groups keep their numbers).
        let list = t.add(
            Some(sub),
            None,
            TreeKind::Group {
                label: "list".into(),
                name: "list".into(),
            },
        );
        t.reparent(item, Some(list));

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

    /// A thread's messages are notes of one `comment_section` group, which is
    /// listed once among its parent's children however many notes it holds.
    #[test]
    fn a_comment_thread_is_one_child_of_its_parent() {
        let note = |text: &str| Node::CommentSection {
            name: "comment-Sheet1-F7".into(),
            text: text.into(),
            refs_note_text: true,
            grouped: true,
        };
        let doc = DoclingDocument {
            name: "c".into(),
            nodes: vec![note("[author: A]: first"), note("[author: B]: reply")],
            ..DoclingDocument::new("c")
        };
        let v = crate::json::to_json(&doc);
        assert_eq!(v["body"]["children"].as_array().unwrap().len(), 1);
        assert_eq!(v["groups"][0]["children"].as_array().unwrap().len(), 2);
    }

    /// docx reviewer comments: a `comment_section` group on the notes layer
    /// holding the note text, and a `comments` back-ref on the annotated item —
    /// keyed between `prov` and `orig`, the slot docling emits it in.
    #[test]
    #[cfg(feature = "preserve-order")] // field-order assertions (#970)
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
            description: None,
            caption_parent,
            caption_location: None,
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

    /// A PDF picture's contained text (`Node::PictureChildren`) is written as
    /// docling's `_add_child_elements` writes it: items parented to the
    /// picture, after its caption in the picture's `children`, never in the
    /// body; a list item opens its own list group under the picture. The
    /// Markdown and the chunker's refs are unaffected.
    #[test]
    fn picture_children_hang_off_the_picture_after_its_caption() {
        let mut doc = DoclingDocument::new("t");
        doc.push(picture("fig", CaptionParent::Item));
        doc.push(Node::PictureChildren(vec![
            Node::Heading {
                level: 2,
                text: "in-figure title".into(),
            },
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
        ]));
        doc.push(Node::Paragraph {
            text: "after".into(),
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(refs(&v["body"]["children"]), ["#/pictures/0", "#/texts/4"]);
        assert_eq!(
            refs(&v["pictures"][0]["children"]),
            ["#/texts/0", "#/texts/1", "#/texts/2", "#/groups/0"]
        );
        assert_eq!(refs(&v["pictures"][0]["captions"]), ["#/texts/0"]);
        assert_eq!(v["texts"][1]["label"], "section_header");
        assert_eq!(v["texts"][2]["label"], "text");
        for t in 1..=2 {
            assert_eq!(v["texts"][t]["parent"]["$ref"], "#/pictures/0");
            assert_eq!(v["texts"][t]["content_layer"], "body");
        }
        assert_eq!(v["groups"][0]["parent"]["$ref"], "#/pictures/0");
        assert_eq!(v["texts"][3]["parent"]["$ref"], "#/groups/0");
        let md = doc.export_to_markdown();
        assert!(
            !md.contains("axis label") && !md.contains("callout"),
            "{md}"
        );
        assert!(md.contains("after"), "{md}");
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

    /// A PDF footnote (#620 follow-up): the JSON keeps docling's
    /// `footnote` label, the raw text and the link as `hyperlink`; the
    /// Markdown still wraps the whole item as `[text](uri)`, and the plain
    /// text drops the link like docling-core's `export_to_text`.
    #[test]
    fn a_labeled_text_keeps_its_label_and_hyperlink_out_of_the_text() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::LabeledText {
            label: "footnote".into(),
            text: "1 https://example.com/a\\_b".into(),
            href: Some("https://example.com/a_b".into()),
        });
        doc.push(Node::LabeledText {
            label: "footnote".into(),
            text: "© 2022 the authors".into(),
            href: None,
        });
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let t = &v["texts"][0];
        assert_eq!(t["label"], "footnote");
        assert_eq!(t["text"], "1 https://example.com/a_b");
        assert_eq!(t["orig"], "1 https://example.com/a_b");
        assert_eq!(t["hyperlink"], "https://example.com/a_b");
        assert_eq!(v["texts"][1]["label"], "footnote");
        assert!(v["texts"][1].get("hyperlink").is_none());
        assert_eq!(
            doc.export_to_markdown(),
            "[1 https://example.com/a\\_b](https://example.com/a_b)\n\n© 2022 the authors\n"
        );
        assert_eq!(
            doc.export_to_text(),
            "1 https://example.com/a_b\n\n© 2022 the authors"
        );
    }
}
