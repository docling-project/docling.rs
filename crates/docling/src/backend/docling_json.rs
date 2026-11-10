//! docling JSON backend — reads docling's native `DoclingDocument` JSON
//! serialization and re-exports it. docling just pydantic-loads the model; here
//! we walk the `body` tree (children resolved through `$ref` into the
//! `texts`/`groups`/`tables`/`pictures` arrays, skipping `furniture`) and map
//! each item onto the crate's [`Node`] model so the shared Markdown serializer
//! reproduces docling-core's output. Each item's `prov` and the document's
//! `pages` ride along (#899): the JSON export of a docling-JSON input keeps
//! the page and box provenance docling gave the original document, and the
//! DocLang export carries the `<location>` tokens and `<page_break/>`s
//! docling-core's own serializer derives from them.

use serde_json::Value;

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{CaptionParent, DoclingDocument, Node, PictureImage, Table};

pub struct DoclingJsonBackend;

impl DeclarativeBackend for DoclingJsonBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let root: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            // Not JSON at all — but not a docling document either (its
            // schema name is nowhere in the text): the bytes as a json
            // code block, as MarkItDown reads any `.json` (#671). A broken
            // DoclingDocument keeps its parse error.
            Err(_) if !text.contains("DoclingDocument") => {
                return crate::backend::text::convert_json_text(source);
            }
            Err(e) => return Err(ConversionError::with_source("docling-json", e)),
        };
        // Generic JSON (#671) — no `schema_name`, no `body` to walk — is one
        // `json` code block, not an empty document.
        if !crate::backend::text::is_docling_document(&root) {
            return crate::backend::text::convert_json_text(source);
        }
        read_document(
            root,
            &source.name,
            source.path.as_deref().and_then(std::path::Path::parent),
        )
    }
}

/// Read a parsed `DoclingDocument` JSON value into the flat [`Node`] model —
/// the walk behind [`DoclingJsonBackend`], shared with backends that build
/// docling's item tree and take their Markdown nodes from that tree's own
/// JSON (the Box Note backend, #1033), so both read the same structure the
/// way docling-core's serializers do. `base` resolves referenced images;
/// `fallback_name` names a document whose JSON has no `name`.
pub(crate) fn read_document(
    mut root: Value,
    fallback_name: &str,
    base: Option<&std::path::Path>,
) -> Result<DoclingDocument, ConversionError> {
    // docling-core's `RefItem` reads a reference under either key
    // (`populate_by_name`): upstream fixtures written with `cref`
    // (`table_footnotes_496.json`) read like `$ref` ones (#1014).
    normalize_crefs(&mut root);
    check_table_budget(&root)?;
    // A document saved before schema 1.11.0 has trimmed inline runs its
    // producer expected joined with `" "`; put that join back in the
    // runs, as docling-core does on load (#997), before the faithful join.
    docling_core::inline::migrate_legacy_json(&mut root);
    // Referenced images resolve against the JSON file's directory (#403).
    inline_referenced_images(&mut root, base);
    let root = root;
    let name = root["name"].as_str().unwrap_or(fallback_name).to_string();
    let mut doc = DoclingDocument::new(name);
    // The `metadata` object (#679) a docling.rs export carried.
    if root["metadata"].is_object() {
        doc.metadata = docling_core::DocumentMetadata::from_json(&root["metadata"]);
    }
    let mut guard = WalkGuard {
        pages: page_sizes(&root),
        claimed_footnotes: claimed_footnotes(&root),
        ..WalkGuard::default()
    };
    if let Some(children) = root["body"]["children"].as_array() {
        for c in children {
            walk(c, &root, 0, &mut doc, &mut guard);
        }
    }
    // A page no body item sits on (a blank page, a furniture-only one)
    // is still a page of the document: its marker keeps it in the
    // exported `pages` map. Trailing markers render nothing — Markdown's
    // page break goes only between two rendered blocks.
    for (&page_no, &(width, height)) in &guard.pages {
        if !guard.seen_pages.contains(&page_no) {
            doc.push(Node::PageInfo {
                page_no,
                width,
                height,
            });
        }
    }
    Ok(doc)
}

/// Every table of the file against the cell budget (#861; docling-core
/// 2.103.1's load-time validators): the declared `num_rows × num_cols` and
/// the `grid` the file carries, whichever is larger, per table and summed
/// over `tables` — all of them, reachable or not, as upstream counts them,
/// since the dense `rows` of each is what the reader materializes.
fn check_table_budget(root: &Value) -> Result<(), ConversionError> {
    let mut budget = docling_core::limits::TableCellBudget::new();
    for (i, table) in root["tables"].as_array().into_iter().flatten().enumerate() {
        let data = &table["data"];
        let declared = |key: &str| data[key].as_u64().unwrap_or(0).min(usize::MAX as u64) as usize;
        let grid = data["grid"].as_array();
        let rows = declared("num_rows").max(grid.map_or(0, Vec::len));
        let cols = declared("num_cols").max(
            grid.into_iter()
                .flatten()
                .filter_map(Value::as_array)
                .map(Vec::len)
                .max()
                .unwrap_or(0),
        );
        budget
            .table(&format!("docling-json #/tables/{i}"), rows, cols)
            .map_err(|e| ConversionError::with_source("docling-json", e))?;
    }
    Ok(())
}

/// The JSON pointer of a reference: `$ref`, or `cref` — docling-core's
/// `RefItem` is `cref` with the `$ref` alias and `populate_by_name`, so a
/// document written by field name (`{"cref": "#/texts/3"}`, upstream's
/// `table_footnotes_496.json` and docling-core's own test files) is the
/// same document (#1014).
fn ref_str(reference: &Value) -> Option<&str> {
    reference["$ref"]
        .as_str()
        .or_else(|| reference["cref"].as_str())
}

/// Resolve a `{"$ref": "#/texts/3"}` reference into its array element.
fn resolve<'a>(reference: &Value, root: &'a Value) -> Option<&'a Value> {
    let path = ref_str(reference)?.strip_prefix("#/")?;
    let (kind, idx) = path.rsplit_once('/')?;
    root.get(kind)?.get(idx.parse::<usize>().ok()?)
}

fn ref_kind(reference: &Value) -> &str {
    ref_str(reference).unwrap_or("")
}

/// An inline group child's Markdown: a run's formatted text, a nested
/// inline group's runs concatenated (docling-core's `_collect_subtree_text`
/// joins an inline group with `""`), anything else empty.
fn text_of(reference: &Value, root: &Value) -> String {
    inline_text(reference, root, 0)
}

fn inline_text(reference: &Value, root: &Value, depth: usize) -> String {
    let Some(item) = resolve(reference, root) else {
        return String::new();
    };
    if ref_kind(reference).starts_with("#/groups/") {
        if item["label"].as_str() != Some("inline") || depth >= MAX_DEPTH {
            return String::new();
        }
        let empty = Vec::new();
        return item["children"]
            .as_array()
            .unwrap_or(&empty)
            .iter()
            .map(|c| inline_text(c, root, depth + 1))
            .collect();
    }
    formatted_text(item)
}

/// Escaped item text with docling-core's inline markers applied in order:
/// bold → italic → strikethrough → hyperlink (underline/script are no-ops in
/// Markdown), the run's edge whitespace hoisted outside them all and a
/// whitespace-only run left undecorated (`hoist_decoration_whitespace`, #997).
fn formatted_text(item: &Value) -> String {
    docling_core::inline::hoist(item["text"].as_str().unwrap_or(""), |core| {
        decorate(core, item)
    })
}

fn decorate(text: &str, item: &Value) -> String {
    let mut res = escape_text(text);
    let fmt = &item["formatting"];
    if fmt["bold"].as_bool() == Some(true) {
        res = format!("**{res}**");
    }
    if fmt["italic"].as_bool() == Some(true) {
        res = format!("*{res}*");
    }
    if fmt["strikethrough"].as_bool() == Some(true) {
        res = format!("~~{res}~~");
    }
    if let Some(url) = item["hyperlink"].as_str() {
        res = docling_core::hyperlink::markdown_link(&res, url);
    }
    res
}

/// Dispatch one body/child reference by the array it points into.
/// Guards the tree walk against a document that is not a tree: a `children`
/// entry that points back at an ancestor recursed until the stack overflowed
/// (an abort, not an error), a chain of 50 000 nested groups did the same, and
/// one item referenced from many parents multiplied the output with every
/// level. Every `$ref` is visited once — a docling document gives each item
/// exactly one parent — and nesting stops at [`MAX_DEPTH`].
#[derive(Default)]
struct WalkGuard {
    seen: std::collections::HashSet<String>,
    depth: usize,
    /// The document's `pages` (#899): page number → size in points.
    pages: std::collections::BTreeMap<usize, (f32, f32)>,
    /// The page the last provenanced item sat on (0 before the first).
    cur_page: usize,
    /// Pages a [`Node::PageInfo`] marker was emitted for.
    seen_pages: std::collections::BTreeSet<usize>,
    /// Refs some table, picture or code item lists in its `footnotes`
    /// (docling-core#675): rendered by that item, skipped by the walk.
    claimed_footnotes: std::collections::HashSet<String>,
}

/// Rewrite every `{"cref": …}` reference object to `{"$ref": …}`.
/// serde_json's own recursion limit (128) bounds the depth.
fn normalize_crefs(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.len() == 1 {
                if let Some(r) = map.remove("cref") {
                    map.insert("$ref".into(), r);
                    return;
                }
            }
            for child in map.values_mut() {
                normalize_crefs(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_crefs),
        _ => {}
    }
}

/// Every ref a floating item (table, picture, code) lists as a footnote.
fn claimed_footnotes(root: &Value) -> std::collections::HashSet<String> {
    ["tables", "pictures", "texts"]
        .iter()
        .filter_map(|bucket| root[bucket].as_array())
        .flatten()
        .filter_map(|it| it["footnotes"].as_array())
        .flatten()
        .filter_map(|r| r["$ref"].as_str().map(String::from))
        .collect()
}

/// An item's `footnotes`, each footnote item's plain text escaped (the flat
/// nodes' convention) — docling-core#675's `serialize_footnotes` prints
/// `it.text` without the run's formatting. A note whose text lives in its
/// inline runs — an empty item holding an inline group, JATS's table notes
/// (docling#4818, #1010) — reads as those runs' Markdown, which docling's
/// walk prints after the table.
fn footnotes_of(item: &Value, root: &Value) -> Vec<String> {
    item["footnotes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| resolve(r, root))
        .filter(|it| it["content_layer"].as_str() != Some("furniture"))
        .filter_map(|it| match it["text"].as_str() {
            Some(t) if !t.is_empty() => Some(escape_text(t)),
            _ => {
                let empty = Vec::new();
                let runs: String = it["children"]
                    .as_array()
                    .unwrap_or(&empty)
                    .iter()
                    .map(|c| text_of(c, root))
                    .collect();
                let runs = runs.trim();
                (!runs.is_empty()).then(|| runs.to_string())
            }
        })
        .collect()
}

/// A picture's or code item's footnotes as standalone `footnote` items
/// after it: the flat [`Node::Picture`] / [`Node::Code`] carry no list of
/// their own, so the Markdown matches docling's (one paragraph each, after
/// the element) while the JSON re-export loses the item's `footnotes` link —
/// see `docs/MIGRATION.md`.
fn push_footnotes(item: &Value, root: &Value, doc: &mut DoclingDocument) {
    for text in footnotes_of(item, root) {
        doc.push(Node::LabeledText {
            label: "footnote".into(),
            text,
            href: None,
        });
    }
}

/// The document's `pages` map: `{"1": {"size": {"width", "height"}, …}}`.
fn page_sizes(root: &Value) -> std::collections::BTreeMap<usize, (f32, f32)> {
    let mut pages = std::collections::BTreeMap::new();
    if let Some(map) = root["pages"].as_object() {
        for (key, page) in map {
            let no = page["page_no"]
                .as_u64()
                .map(|n| n as usize)
                .or_else(|| key.parse().ok());
            let (w, h) = (
                page["size"]["width"].as_f64(),
                page["size"]["height"].as_f64(),
            );
            if let (Some(no), Some(w), Some(h)) = (no, w, h) {
                if no > 0 {
                    pages.insert(no, (w as f32, h as f32));
                }
            }
        }
    }
    pages
}

/// An item's first `prov` entry (#899): the page it sits on, its box in the
/// page's own coordinate origin, and its character span. docling keeps a
/// list, one entry per page an item spans; the JSON export writes one box
/// per item, so the first — the only one in practice — is what survives.
struct Prov {
    page_no: usize,
    bbox: [f32; 4],
    bottom_left: bool,
    charspan: [usize; 2],
}

fn prov_of(item: &Value) -> Option<Prov> {
    let p = item["prov"].as_array()?.first()?;
    let page_no = p["page_no"].as_u64()? as usize;
    let b = &p["bbox"];
    let coord = |k: &str| b[k].as_f64().map(|v| v as f32);
    let bbox = [coord("l")?, coord("t")?, coord("r")?, coord("b")?];
    let bottom_left = b["coord_origin"].as_str() != Some("TOPLEFT");
    let span = p["charspan"].as_array();
    let at = |i: usize| span.and_then(|s| s.get(i)?.as_u64()).unwrap_or(0) as usize;
    Some(Prov {
        page_no,
        bbox,
        bottom_left,
        charspan: [at(0), at(1)],
    })
}

/// Before an item on page `page_no`: the [`Node::PageInfo`] marker that
/// tells the JSON export which page (and page size) the following items are
/// on, once per change of page, as the PDF and XLSX paths emit it. `true`
/// when the walk is on that page afterwards. A marker between two list items
/// would split the list (the serializers group a list from adjacent items),
/// so a list that runs onto the next page keeps that page's items in the
/// list, without a location — docling's own PDF documents never do, their
/// layout groups a list per page.
fn enter_page(
    page_no: usize,
    list_item: bool,
    doc: &mut DoclingDocument,
    g: &mut WalkGuard,
) -> bool {
    if page_no == 0 {
        return false;
    }
    if page_no == g.cur_page {
        return true;
    }
    let Some(&(width, height)) = g.pages.get(&page_no) else {
        return false;
    };
    if list_item && matches!(doc.nodes.last(), Some(Node::ListItem { .. })) {
        return false;
    }
    // docling-core's `_iterate_items` yields a page break between two items
    // on different pages (DocLang's `<page_break/>`; Markdown's placeholder,
    // which absorbs the marker pair into one). Not before the first item.
    if g.cur_page != 0 {
        doc.push(Node::PageBreak);
    }
    g.cur_page = page_no;
    g.seen_pages.insert(page_no);
    doc.push(Node::PageInfo {
        page_no,
        width,
        height,
    });
    true
}

/// Wrap the node at `index` (the first node the item pushed) in the item's
/// provenance: the exact box for the JSON export (`Node::Prov`, outside —
/// the JSON export prefers it) around, when the walk is on the item's page,
/// the location grid for DocLang (`Node::Located`, inside — its serializer
/// renders the node it wraps directly).
fn wrap_prov(doc: &mut DoclingDocument, index: usize, prov: &Prov, grid: Option<[u16; 4]>) {
    let inner = std::mem::replace(&mut doc.nodes[index], Node::PageBreak);
    let inner = match grid {
        Some(location) => Node::Located {
            location,
            inner: Box::new(inner),
        },
        None => inner,
    };
    doc.nodes[index] = Node::Prov {
        page_no: prov.page_no,
        bbox: prov.bbox,
        charspan: prov.charspan,
        bottom_left: prov.bottom_left,
        seq: None,
        inner: Box::new(inner),
    };
}

/// An item's provenance on the DocLang location grid (`[x0, y0, x1, y1]` on
/// 0–511, top-left origin): docling-core's
/// `_create_location_tokens_for_bbox` at resolution 512, the same
/// quantization as the PDF pipeline's. The `<location>` tokens of every
/// item, and the whole provenance of a list item, which the `location`
/// field carries: the serializers group a list from *adjacent* `ListItem`
/// nodes, so a `Prov` wrapper around one would split the list. The JSON
/// export denormalizes the grid against the page size, so a list item's box
/// comes back to within a grid cell (page size / 512 — under a point on A4)
/// and its charspan as `[0, len]`, which is what docling writes for a PDF
/// item anyway.
fn grid_location(prov: &Prov, g: &WalkGuard) -> Option<[u16; 4]> {
    let &(w, h) = g.pages.get(&prov.page_no)?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let q =
        |v: f32, dim: f32| ((512.0 * v as f64 / dim as f64).round() as i64).clamp(0, 511) as u16;
    let [l, t, r, b] = prov.bbox;
    let (top, bottom) = if prov.bottom_left {
        (h - t, h - b)
    } else {
        (t, b)
    };
    Some([q(l, w), q(top, h), q(r, w), q(bottom, h)])
}

/// Deepest `children` nesting walked. docling's own JSON never goes past a
/// few dozen (sections in sections, lists in list items).
const MAX_DEPTH: usize = 256;

fn walk(reference: &Value, root: &Value, level: u8, doc: &mut DoclingDocument, g: &mut WalkGuard) {
    let key = ref_kind(reference);
    if g.depth >= MAX_DEPTH || !g.seen.insert(key.to_string()) {
        return;
    }
    g.depth += 1;
    walk_inner(reference, root, level, doc, g);
    g.depth -= 1;
}

fn walk_inner(
    reference: &Value,
    root: &Value,
    level: u8,
    doc: &mut DoclingDocument,
    g: &mut WalkGuard,
) {
    let Some(item) = resolve(reference, root) else {
        return;
    };
    if item["content_layer"].as_str() == Some("furniture") {
        return;
    }
    let kind = ref_kind(reference);
    if g.claimed_footnotes.contains(kind) {
        // A footnote renders with the floating item listing it.
        return;
    }
    if kind.starts_with("#/texts/") {
        text_item(item, root, level, doc, g);
        // docling nests a section's content under its heading (and a list
        // item's sub-list under the item), so a text item's `children` are
        // body content too — without this walk a document collapses to its
        // first heading. Tables and pictures are not recursed: their children
        // are rich-cell / caption items already rendered with the parent.
        if let Some(children) = item["children"].as_array() {
            for c in children {
                walk(c, root, level, doc, g);
            }
        }
    } else if kind.starts_with("#/groups/") {
        group_item(item, root, level, doc, g);
    } else if kind.starts_with("#/tables/") {
        with_prov(item, doc, g, |doc, _| table_item(item, root, doc));
    } else if kind.starts_with("#/pictures/") {
        with_prov(item, doc, g, |doc, _| picture_item(item, root, doc));
    }
}

/// Run `push` for an item and attach the item's provenance (#899) to the
/// first node it pushed: the page marker ahead of it, the exact box around
/// it — or, for a list item, in its `location` field (`list_location`).
fn with_prov(
    item: &Value,
    doc: &mut DoclingDocument,
    g: &mut WalkGuard,
    push: impl FnOnce(&mut DoclingDocument, Option<[u16; 4]>),
) {
    let Some(prov) = prov_of(item) else {
        push(doc, None);
        return;
    };
    let list_item = item["label"].as_str() == Some("list_item");
    let on_page = enter_page(prov.page_no, list_item, doc, g);
    let start = doc.nodes.len();
    let grid = on_page.then(|| grid_location(&prov, g)).flatten();
    push(doc, grid);
    if doc.nodes.len() > start && !matches!(doc.nodes[start], Node::ListItem { .. }) {
        wrap_prov(doc, start, &prov, grid);
    }
}

fn text_item(item: &Value, root: &Value, level: u8, doc: &mut DoclingDocument, g: &mut WalkGuard) {
    with_prov(item, doc, g, |doc, location| {
        text_node(item, root, level, doc, location)
    });
}

fn text_node(
    item: &Value,
    root: &Value,
    level: u8,
    doc: &mut DoclingDocument,
    location: Option<[u16; 4]>,
) {
    let label = item["label"].as_str().unwrap_or("text");
    // docling does not serialize empty text items (an undecoded formula is the
    // one exception — it becomes a placeholder comment).
    if item["text"].as_str().unwrap_or("").is_empty() {
        if label == "formula" {
            doc.push(Node::Paragraph {
                text: "<!-- formula-not-decoded -->".into(),
            });
        }
        return;
    }
    let text = formatted_text(item);
    // Code and formulas are the two items docling serializes *unescaped*
    // (`escape_html = False`, `escape_underscores = False`): a SQL body keeps
    // its `VERIFY_GROUP_FOR_USER`, not `VERIFY\_GROUP\_FOR\_USER`.
    let raw = || item["text"].as_str().unwrap_or("").to_string();
    match label {
        "title" => doc.push(Node::Heading { level: 1, text }),
        "section_header" => {
            let lvl = item["level"].as_u64().unwrap_or(1) as u8;
            doc.push(Node::Heading {
                level: lvl + 1,
                text,
            });
        }
        "code" => {
            doc.push(Node::Code {
                language: item["code_language"]
                    .as_str()
                    .filter(|s| !s.is_empty() && *s != "unknown")
                    .map(String::from),
                text: raw(),
                orig: None,
                pretty: None,
            });
            // docling-core#675: the footnotes come right after the code,
            // ahead of its trailing captions.
            push_footnotes(item, root, doc);
            // A `CodeItem` is docling's only *floating* text item, and the
            // text serializer appends a floating item's captions **after** its
            // own text — the opposite of a picture or a table, which lead with
            // theirs (`Listing 1: …` under the fence, not above it).
            if let Some(cap) = caption_of(item, root) {
                doc.push(Node::Paragraph { text: cap });
            }
        }
        "list_item" => doc.push(Node::ListItem {
            ordered: item["enumerated"].as_bool().unwrap_or(false),
            number: 1,
            first_in_list: true,
            text,
            level,
            marker: None,
            location,
            dclx: None,
            href: None,
            layer: None,
        }),
        // docling prefixes the task-list marker to the text of a checkbox item.
        "checkbox_selected" => doc.push(Node::CheckboxItem {
            checked: true,
            text,
        }),
        "checkbox_unselected" => doc.push(Node::CheckboxItem {
            checked: false,
            text,
        }),
        // A decoded formula renders as `$$…$$`, also unescaped.
        "formula" => doc.push(Node::Formula {
            latex: raw(),
            orig: item["orig"].as_str().unwrap_or("").to_string(),
            location: None,
        }),
        // A caption some table/picture/code claims renders with that element;
        // one nobody claims is an ordinary body item and renders where it sits
        // (docling's serializer only skips the refs a floating item consumed).
        "caption" => {
            if !caption_is_claimed(item, root) {
                doc.push(Node::Caption { text, href: None });
            }
        }
        // text, paragraph, footnote, … A linked one keeps its link as the
        // item's `hyperlink` (#1014), not as `[x](url)` baked into its text —
        // which the HTML export then printed literally. The Markdown is the
        // same `[x](url)` either way (`Node::labeled_as_paragraph`). A
        // formatted link stays a paragraph: the node has no formatting.
        _ => match item["hyperlink"].as_str() {
            Some(url) if !has_formatting(item) => doc.push(Node::LabeledText {
                label: label.to_string(),
                text: escape_text(item["text"].as_str().unwrap_or("")),
                href: Some(url.to_string()),
            }),
            _ => doc.push(Node::Paragraph { text }),
        },
    }
}

/// Whether bold, italic or strikethrough — what [`formatted_text`] marks
/// up — is set on the item.
fn has_formatting(item: &Value) -> bool {
    let fmt = &item["formatting"];
    ["bold", "italic", "strikethrough"]
        .iter()
        .any(|k| fmt[*k].as_bool() == Some(true))
}

fn group_item(item: &Value, root: &Value, level: u8, doc: &mut DoclingDocument, g: &mut WalkGuard) {
    let label = item["label"].as_str().unwrap_or("unspecified");
    let empty = Vec::new();
    let children = item["children"].as_array().unwrap_or(&empty);
    match label {
        "list" | "ordered_list" => list_group(children, root, level, doc, g),
        "inline" => {
            // An inline group is one line: its runs carry their own
            // whitespace (schema 1.11.0, #997) and are concatenated as they
            // are; an older document's were migrated on load.
            let joined: String = children.iter().map(|c| text_of(c, root)).collect();
            let joined = joined.trim().to_string();
            if !joined.is_empty() {
                doc.push(Node::Paragraph { text: joined });
            }
        }
        // section / chapter / unspecified / sheet / comment_section → transparent
        _ => {
            for c in children {
                walk(c, root, level, doc, g);
            }
        }
    }
}

/// How docling-core's Markdown serializer renders a list item's marker.
///
/// * A marker it already considers valid Markdown — a bullet, or `12.` — is
///   printed **verbatim** and nothing is computed. This is what carries a
///   split reference list's real numbering (`18.` on the group that continues
///   over a page break) instead of restarting at 1.
/// * Any *other* non-empty marker (`a.`, `[7]`, `(3)`) forces a **bullet**:
///   the computed marker is a number only when the item carries no marker at
///   all (`… and (mode != AUTO or not item.marker)`). The original marker is
///   then kept after it when it holds a letter or digit, so docling renders
///   `- (1) Human Annotation`. In this node model it rides in the item's text,
///   which is where it lands on the rendered line either way; a marker with
///   no alphanumerics (a stray bullet glyph) is dropped, as upstream drops it.
/// * No marker at all leaves the group to decide: `{position}.` when its first
///   child is an enumerated item, `-` otherwise.
enum Marker {
    /// Print verbatim: `Some(n)` numbers the item, `None` bullets it.
    Verbatim(Option<u64>),
    /// Bullet, with this text (if any) kept in front of the item's own.
    Bullet(Option<String>),
    /// Nothing of its own — the group's kind and the item's position decide.
    FromGroup,
}

fn marker_of(item: &Value) -> Marker {
    let raw = item["marker"].as_str().unwrap_or("");
    if raw.is_empty() {
        return Marker::FromGroup;
    }
    if matches!(raw, "-" | "*" | "+") {
        return Marker::Verbatim(None);
    }
    if let Some(digits) = raw.strip_suffix('.') {
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return Marker::Verbatim(digits.parse().ok());
        }
    }
    Marker::Bullet(
        raw.chars()
            .any(|c| c.is_ascii_alphanumeric())
            .then(|| raw.to_string()),
    )
}

/// Emit a list group's items, recursing into nested lists at the next level.
fn list_group(
    children: &[Value],
    root: &Value,
    level: u8,
    doc: &mut DoclingDocument,
    g: &mut WalkGuard,
) {
    // docling-core's `first_item_is_enumerated`: the group renders computed
    // markers as numbers only when its *first child* is an enumerated item.
    let enumerated_group = children
        .first()
        .and_then(|c| resolve(c, root))
        .is_some_and(|it| {
            it["label"].as_str() == Some("list_item") && it["enumerated"].as_bool() == Some(true)
        });
    let mut first = true;
    for (pos, c) in children.iter().enumerate() {
        let kind = ref_kind(c);
        if kind.starts_with("#/groups/") {
            // A bare nested list (no enclosing item). It still occupies a
            // position in this group, which is why a later item can be
            // numbered past it.
            walk(c, root, level + 1, doc, g);
            continue;
        }
        let Some(item) = resolve(c, root) else {
            continue;
        };
        let checkbox = match item["label"].as_str() {
            Some("checkbox_selected") => Some("[x]"),
            Some("checkbox_unselected") => Some("[ ]"),
            _ => None,
        };
        if let Some(mark) = checkbox {
            // A checkbox in a list group (a Box Note check list, #1033) is a
            // line of the list: `- [x] text`, the next item right below it.
            if !g.seen.insert(kind.to_string()) {
                continue;
            }
            let body = formatted_text(item);
            doc.push(Node::ListItem {
                ordered: false,
                number: pos as u64 + 1,
                first_in_list: first,
                text: format!("{mark} {body}"),
                level,
                marker: None,
                location: None,
                dclx: None,
                href: None,
                layer: None,
            });
            first = false;
            if let Some(sub) = item["children"].as_array() {
                for s in sub {
                    walk(s, root, level + 1, doc, g);
                }
            }
            continue;
        }
        if item["label"].as_str() != Some("list_item") {
            continue;
        }
        let position = pos as u64 + 1;
        let sub = item["children"].as_array().map_or(&[][..], Vec::as_slice);
        // The item's leading inline content — the inline groups opening its
        // children (a Box Note list item's second paragraph,
        // #1033) — continues the item's own line, as docling-core's list
        // serializer prints it (`- Intro more **x** y`).
        let lead = sub.iter().take_while(|s| leading_inline(s, root)).count();
        let mut body = formatted_text(item);
        for s in &sub[..lead] {
            if !g.seen.insert(ref_kind(s).to_string()) {
                continue;
            }
            let more = text_of(s, root);
            let more = more.trim();
            if !more.is_empty() {
                if !body.is_empty() {
                    body.push(' ');
                }
                body.push_str(more);
            }
        }
        let (ordered, number, text) = match marker_of(item) {
            Marker::Verbatim(Some(n)) => (true, n, body),
            Marker::Verbatim(None) => (false, position, body),
            Marker::Bullet(kept) => {
                let text = match kept {
                    Some(m) if !body.is_empty() => format!("{m} {body}"),
                    Some(m) => m,
                    None => body,
                };
                (false, position, text)
            }
            Marker::FromGroup => (enumerated_group, position, body),
        };
        let location = prov_of(item)
            .filter(|p| enter_page(p.page_no, true, doc, g))
            .and_then(|p| grid_location(&p, g));
        doc.push(Node::ListItem {
            ordered,
            number,
            first_in_list: first,
            text,
            level,
            marker: None,
            location,
            dclx: None,
            href: None,
            layer: None,
        });
        first = false;
        for s in &sub[lead..] {
            walk(s, root, level + 1, doc, g);
        }
    }
}

/// Whether a list item's child is inline content its line takes in: an
/// `inline` group (upstream's `test_list_item_text_keeps_boundary_before_rich_paragraph`).
fn leading_inline(reference: &Value, root: &Value) -> bool {
    ref_kind(reference).starts_with("#/groups/")
        && resolve(reference, root).is_some_and(|c| c["label"].as_str() == Some("inline"))
}

/// A `RichTableCell`'s Markdown: docling-core serializes the group its `ref`
/// points at — the cell's blocks, blank-line separated — and a table cell
/// holds one line, so each newline becomes a space (`1\n\n*primary*` →
/// `1  *primary*`, a Box Note cell, #1033). `None` for a plain cell, or a
/// rich one whose group renders nothing.
fn rich_cell_markdown(cell: &Value, root: &Value) -> Option<String> {
    let group = cell
        .get("ref")
        .filter(|r| ref_kind(r).starts_with("#/groups/"))?;
    let mut sub = DoclingDocument::new("cell");
    walk(group, root, 0, &mut sub, &mut WalkGuard::default());
    let md = sub.export_to_markdown();
    let md = md.trim();
    (!md.is_empty()).then(|| md.replace('\n', " "))
}

fn table_item(item: &Value, root: &Value, doc: &mut DoclingDocument) {
    let mut rows = Vec::new();
    // Per-cell flags carried through so DocTags export and the chunker's
    // triplet serialization see docling's header/span structure, not just the
    // flattened text grid.
    let mut structure = docling_core::TableStructure::default();
    // Whether the file states the header flags at all — docling's export
    // writes `column_header` on every cell, so all-`false` means "no header"
    // (#1042), not "unknown".
    let mut declares_headers = false;
    // A `RichTableCell`'s `ref` sits on `table_cells` only (the derived
    // `grid` shows plain cells): keyed by the cell's anchor.
    let rich: std::collections::HashMap<(u64, u64), &Value> = item["data"]["table_cells"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|c| c.get("ref").is_some())
        .filter_map(|c| {
            Some((
                (
                    c["start_row_offset_idx"].as_u64()?,
                    c["start_col_offset_idx"].as_u64()?,
                ),
                c,
            ))
        })
        .collect();
    if let Some(grid) = item["data"]["grid"].as_array() {
        for (r, row) in grid.iter().enumerate() {
            let Some(cells) = row.as_array() else {
                continue;
            };
            rows.push(
                cells
                    .iter()
                    .map(|cell| {
                        let anchor = (
                            cell["start_row_offset_idx"].as_u64().unwrap_or(u64::MAX),
                            cell["start_col_offset_idx"].as_u64().unwrap_or(u64::MAX),
                        );
                        rich.get(&anchor)
                            .and_then(|rc| rich_cell_markdown(rc, root))
                            .unwrap_or_else(|| cell["text"].as_str().unwrap_or("").to_string())
                    })
                    .collect::<Vec<_>>(),
            );
            // A spanning cell is repeated at each grid position it covers; its
            // start offsets mark the anchor, everything past it a continuation.
            let past = |cell: &Value, key: &str, idx: usize| {
                cell[key].as_u64().is_some_and(|v| (v as usize) < idx)
            };
            declares_headers |= cells.iter().any(|c| c["column_header"].is_boolean());
            structure
                .col_header
                .push(flags(cells, |c| c["column_header"].as_bool() == Some(true)));
            structure
                .row_header
                .push(flags(cells, |c| c["row_header"].as_bool() == Some(true)));
            structure.col_continuation.push(flags(cells.as_slice(), {
                let mut col = 0;
                move |c| {
                    let cont = past(c, "start_col_offset_idx", col);
                    col += 1;
                    cont
                }
            }));
            structure
                .row_continuation
                .push(flags(cells, |c| past(c, "start_row_offset_idx", r)));
        }
    }
    if !rows.is_empty() {
        let has_structure = structure.col_header.iter().flatten().any(|&b| b)
            || structure.row_header.iter().flatten().any(|&b| b)
            || structure.col_continuation.iter().flatten().any(|&b| b)
            || structure.row_continuation.iter().flatten().any(|&b| b);
        doc.push(Node::Table(Table {
            rows,
            location: None,
            // Stated flags are kept even when every one is `false`: `None`
            // makes `Table::derive_cells` promote row 0 to the header the
            // file says it is not, and the reloaded document chunks
            // differently from the one exported (#1042). A JSON without the
            // keys keeps `None` and the row-0 default.
            structure: (has_structure || declares_headers).then_some(structure),
            cell_blocks: None,
            cells: None,
            caption: caption_of(item, root),
            caption_parent: caption_parent_of(item, root),
            caption_location: None,
            footnotes: footnotes_of(item, root),
            // A table of contents keeps its label through the round trip.
            document_index: item["label"].as_str() == Some("document_index"),
        }));
    }
}

/// Map each grid cell to a flag.
fn flags(cells: &[Value], f: impl FnMut(&Value) -> bool) -> Vec<bool> {
    cells.iter().map(f).collect()
}

fn picture_item(item: &Value, root: &Value, doc: &mut DoclingDocument) {
    doc.push(Node::Picture {
        caption: caption_of(item, root),
        caption_parent: caption_parent_of(item, root),
        caption_location: None,
        caption_href: None,
        image: picture_image(item),
        classification: picture_classification(item),
        description: picture_description(item),
    });
    push_footnotes(item, root, doc);
}

/// The picture's description (#975): docling's `meta.description`
/// (`DescriptionMetaField`: `text`, `created_by`), else the deprecated
/// `PictureDescriptionData` annotation (`kind: "description"`, `text`,
/// `provenance`) older documents carry alone. A description a picture-
/// description or picture-OCR pass computed is part of the document, and a
/// docling-JSON fed back in — to chunk it, to re-export it — keeps it; it
/// used to come back as `None`, so a document whose only text was its
/// pictures' descriptions chunked to nothing.
fn picture_description(item: &Value) -> Option<docling_core::PictureDescription> {
    let text_of = |v: &Value| {
        v["text"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
    };
    let meta = &item["meta"]["description"];
    if let Some(text) = text_of(meta) {
        return Some(docling_core::PictureDescription {
            text,
            provenance: meta["created_by"].as_str().unwrap_or_default().to_string(),
        });
    }
    item["annotations"]
        .as_array()?
        .iter()
        .find(|a| a["kind"].as_str() == Some("description"))
        .and_then(|a| {
            Some(docling_core::PictureDescription {
                text: text_of(a)?,
                provenance: a["provenance"].as_str().unwrap_or_default().to_string(),
            })
        })
}

/// The classifier's predictions (#975): `meta.classification.predictions`,
/// else the deprecated `classification` annotation's `predicted_classes`
/// (`class_name`, `confidence` each).
fn picture_classification(item: &Value) -> Option<Vec<docling_core::PictureClass>> {
    let preds = item["meta"]["classification"]["predictions"]
        .as_array()
        .or_else(|| {
            item["annotations"]
                .as_array()?
                .iter()
                .find(|a| a["kind"].as_str() == Some("classification"))?["predicted_classes"]
                .as_array()
        })?;
    let classes: Vec<_> = preds
        .iter()
        .filter_map(|p| {
            Some(docling_core::PictureClass {
                class_name: p["class_name"].as_str()?.to_string(),
                confidence: p["confidence"].as_f64().unwrap_or(0.0) as f32,
            })
        })
        .collect();
    (!classes.is_empty()).then_some(classes)
}

/// Where the item's first caption hangs in the source tree, so a re-export
/// keeps it there (#390): on the item (a PDF layout caption), on the item's
/// own parent — ahead of the item or behind it, by the parent's `children`
/// order — or, anywhere else, docling's `add_text` default, the body.
fn caption_parent_of(item: &Value, root: &Value) -> CaptionParent {
    let cap = &item["captions"][0];
    let Some(cap_ref) = ref_str(cap) else {
        return CaptionParent::Body;
    };
    let cap_parent = resolve(cap, root)
        .and_then(|c| ref_str(&c["parent"]))
        .unwrap_or("#/body");
    let self_ref = item["self_ref"].as_str().unwrap_or("");
    let item_parent = &item["parent"];
    if cap_parent == self_ref {
        CaptionParent::Item
    } else if cap_parent == ref_kind(item_parent) && cap_parent != "#/body" {
        let pos = |r: &str| {
            resolve(item_parent, root)?
                .get("children")?
                .as_array()?
                .iter()
                .position(|c| ref_str(c) == Some(r))
        };
        match (pos(cap_ref), pos(self_ref)) {
            (Some(c), Some(i)) if c > i => CaptionParent::ContainerAfter,
            _ => CaptionParent::Container,
        }
    } else {
        CaptionParent::Body
    }
}

/// The picture's embedded image — docling's `ImageRef` with a `data:` URI
/// (`{"mimetype", "dpi", "size", "uri"}`), which is how docling and our own
/// JSON export carry a picture's pixels. Without it every picture read back
/// from JSON was an empty placeholder, and `--images embedded`/`referenced`
/// silently had nothing to embed or write (#403). The dimensions come from the
/// bytes themselves (PNG IHDR / JPEG SOF), the declared `size` as a fallback.
fn picture_image(item: &Value) -> Option<PictureImage> {
    let image = item.get("image")?;
    let uri = image["uri"].as_str()?;
    let (mimetype, data) = parse_data_uri(uri)?;
    let mimetype = image["mimetype"]
        .as_str()
        .filter(|m| !m.is_empty())
        .unwrap_or(&mimetype)
        .to_string();
    let declared = |k: &str| image["size"][k].as_f64().unwrap_or(0.0).round().max(0.0) as u32;
    let (width, height) = crate::backend::rtf::image_size(&mimetype, &data)
        .unwrap_or((declared("width"), declared("height")));
    // The declared dpi round-trips (a PDF crop's 144 stays 144, #519).
    let dpi = image["dpi"]
        .as_u64()
        .and_then(|d| u32::try_from(d).ok())
        .filter(|&d| d > 0)
        .unwrap_or(PictureImage::DEFAULT_DPI);
    Some(PictureImage {
        mimetype,
        width,
        height,
        data,
        dpi,
        display_size: None,
    })
}

/// Split a `data:<mimetype>;base64,<payload>` URI into its media type and
/// decoded bytes. Anything else — a plain path, an `http(s)` or `file` URL —
/// is `None`: [`DoclingJsonBackend::convert`] inlines the local files it can
/// read beforehand, so only a genuinely unresolvable reference reaches here.
fn parse_data_uri(uri: &str) -> Option<(String, Vec<u8>)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let (mimetype, encoding) = meta.split_once(';').unwrap_or((meta, ""));
    if !encoding.eq_ignore_ascii_case("base64") {
        return None;
    }
    let data = docling_core::base64::decode(payload.trim())?;
    Some((mimetype.to_string(), data))
}

/// Turn each picture's *referenced* image (`uri` a filesystem path or `file:`
/// URL, as `--images referenced` writes them) into a `data:` URI, reading the
/// file relative to the JSON's own directory — docling's `ImageRef.pil_image`
/// opens such a URI from disk the same way. A file that cannot be read keeps
/// its `uri`, so the picture degrades to a placeholder rather than failing.
fn inline_referenced_images(root: &mut Value, base: Option<&std::path::Path>) {
    let Some(pictures) = root.get_mut("pictures").and_then(Value::as_array_mut) else {
        return;
    };
    for pic in pictures {
        let Some(image) = pic.get_mut("image").filter(|i| i.is_object()) else {
            continue;
        };
        let Some(uri) = image["uri"].as_str().map(str::to_string) else {
            continue;
        };
        if uri.starts_with("data:") || (uri.contains("://") && !uri.starts_with("file://")) {
            continue;
        }
        let path = std::path::PathBuf::from(uri.strip_prefix("file://").unwrap_or(&uri));
        let path = match base {
            Some(dir) if path.is_relative() => dir.join(path),
            _ => path,
        };
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        let mimetype = image["mimetype"]
            .as_str()
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| mime_of_path(&path));
        image["uri"] = Value::String(format!(
            "data:{mimetype};base64,{}",
            docling_core::base64::encode(&data)
        ));
    }
}

/// A media type from a file extension, for a referenced image whose `ImageRef`
/// carries none.
fn mime_of_path(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("webp") => "image/webp",
        Some("tif") | Some("tiff") => "image/tiff",
        _ => "image/png",
    }
    .to_string()
}

/// Whether a floating item lists this caption in its own `captions` — tables,
/// pictures and code items are the three that can. Scanning per caption keeps
/// the walk's signature unchanged; a document has few of either.
fn caption_is_claimed(item: &Value, root: &Value) -> bool {
    let Some(me) = item["self_ref"].as_str() else {
        return false;
    };
    ["tables", "pictures", "texts"].iter().any(|bucket| {
        root[bucket].as_array().is_some_and(|items| {
            items.iter().any(|it| {
                it["captions"]
                    .as_array()
                    .is_some_and(|caps| caps.iter().any(|c| ref_str(c) == Some(me)))
            })
        })
    })
}

/// An item's `captions` (refs into `texts`), joined as docling-core joins them
/// (`caption_delim`, a space). It belongs *on* the table or picture, not after
/// it: every serializer renders a caption before its element, and emitting the
/// caption items as trailing paragraphs put them on the wrong side (#384).
fn caption_of(item: &Value, root: &Value) -> Option<String> {
    let caps = item["captions"].as_array()?;
    let joined = caps
        .iter()
        .map(|c| text_of(c, root))
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.trim().is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn md(json: &str) -> String {
        DoclingJsonBackend
            .convert(&SourceDocument::from_bytes(
                "t.json",
                InputFormat::JsonDocling,
                json.as_bytes().to_vec(),
            ))
            .unwrap()
            .export_to_markdown()
    }

    /// #1009 / #1014: upstream's `table_footnotes_496.json` shape — `cref`
    /// references, a table whose two `footnote` texts it lists in
    /// `footnotes`. Expected output is docling-core 2.106's
    /// `export_to_markdown()` / `export_to_html()` / `HierarchicalChunker`.
    #[test]
    fn table_footnotes_read_from_cref_references() {
        let json = r##"{"schema_name":"DoclingDocument","version":"1.10.0","name":"t",
          "body":{"self_ref":"#/body","children":[{"cref":"#/texts/0"},{"cref":"#/tables/0"},{"cref":"#/texts/3"}],
                  "content_layer":"body","name":"_root_","label":"unspecified"},
          "groups":[],"pictures":[],
          "texts":[
            {"self_ref":"#/texts/0","parent":{"cref":"#/body"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":"Before.","text":"Before."},
            {"self_ref":"#/texts/1","parent":{"cref":"#/tables/0"},"children":[],"content_layer":"body","label":"footnote","prov":[],"orig":"*Note: 'Pending' awaits review.","text":"*Note: 'Pending' awaits review."},
            {"self_ref":"#/texts/2","parent":{"cref":"#/tables/0"},"children":[],"content_layer":"body","label":"footnote","prov":[],"orig":"**Note: a_b.","text":"**Note: a_b."},
            {"self_ref":"#/texts/3","parent":{"cref":"#/body"},"children":[],"content_layer":"body","label":"text","prov":[],"orig":"After.","text":"After."}],
          "tables":[{"self_ref":"#/tables/0","parent":{"cref":"#/body"},"children":[{"cref":"#/texts/1"},{"cref":"#/texts/2"}],
            "content_layer":"body","label":"table","prov":[],"captions":[],"references":[],
            "footnotes":[{"cref":"#/texts/1"},{"cref":"#/texts/2"}],
            "data":{"num_rows":2,"num_cols":2,"table_cells":[
              {"text":"ID","row_span":1,"col_span":1,"start_row_offset_idx":0,"end_row_offset_idx":1,"start_col_offset_idx":0,"end_col_offset_idx":1,"column_header":true,"row_header":false},
              {"text":"Status","row_span":1,"col_span":1,"start_row_offset_idx":0,"end_row_offset_idx":1,"start_col_offset_idx":1,"end_col_offset_idx":2,"column_header":true,"row_header":false},
              {"text":"1","row_span":1,"col_span":1,"start_row_offset_idx":1,"end_row_offset_idx":2,"start_col_offset_idx":0,"end_col_offset_idx":1,"column_header":false,"row_header":false},
              {"text":"Pending*","row_span":1,"col_span":1,"start_row_offset_idx":1,"end_row_offset_idx":2,"start_col_offset_idx":1,"end_col_offset_idx":2,"column_header":false,"row_header":false}],"grid":[
              [{"text":"ID","start_row_offset_idx":0,"end_row_offset_idx":1,"start_col_offset_idx":0,"end_col_offset_idx":1,"column_header":true,"row_header":false},
               {"text":"Status","start_row_offset_idx":0,"end_row_offset_idx":1,"start_col_offset_idx":1,"end_col_offset_idx":2,"column_header":true,"row_header":false}],
              [{"text":"1","start_row_offset_idx":1,"end_row_offset_idx":2,"start_col_offset_idx":0,"end_col_offset_idx":1,"column_header":false,"row_header":false},
               {"text":"Pending*","start_row_offset_idx":1,"end_row_offset_idx":2,"start_col_offset_idx":1,"end_col_offset_idx":2,"column_header":false,"row_header":false}]]}}],
          "key_value_items":[],"form_items":[],"pages":{}}"##;
        let doc = DoclingJsonBackend
            .convert(&SourceDocument::from_bytes(
                "t.json",
                InputFormat::JsonDocling,
                json.as_bytes().to_vec(),
            ))
            .unwrap();
        assert_eq!(
            doc.export_to_markdown(),
            "Before.\n\n|   ID | Status   |\n|------|----------|\n|    1 | Pending* |\n\n\
             *Note: 'Pending' awaits review.\n\n**Note: a\\_b.\n\nAfter.\n"
        );
        let html = doc.export_to_html();
        assert!(
            html.contains(
                "</table><div class=\"footnote\">*Note: &#x27;Pending&#x27; awaits review.</div>\
                 <div class=\"footnote\">**Note: a_b.</div>\n<p>After.</p>"
            ),
            "{html}"
        );
        // The JSON re-export keeps the footnotes on the table.
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(v["tables"][0]["footnotes"][1]["$ref"], "#/texts/2");
        assert_eq!(v["texts"][2]["label"], "footnote");
        assert_eq!(v["texts"][2]["parent"]["$ref"], "#/tables/0");
        // The table's chunk ends with its notes and lists them.
        let chunks = docling_core::chunker::HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert_eq!(
            chunks[1].text,
            "1, Status = Pending*\n\n*Note: 'Pending' awaits review.\n\n**Note: a_b."
        );
        let refs: Vec<&str> = chunks[1]
            .doc_items
            .iter()
            .map(|i| i.self_ref.as_str())
            .collect();
        assert_eq!(refs, ["#/tables/0", "#/texts/1", "#/texts/2"]);
        assert_eq!(chunks[2].doc_items[0].self_ref, "#/texts/3");
    }

    /// #1009: a picture's and a code item's footnotes print after the
    /// element (and before the code's trailing caption) — docling-core
    /// 2.106's Markdown; a footnote some item lists is not printed again
    /// where the body walk meets it.
    #[test]
    fn picture_and_code_footnotes_follow_the_element() {
        let json = r##"{"schema_name":"DoclingDocument","name":"t",
          "body":{"self_ref":"#/body","children":[{"$ref":"#/pictures/0"},{"$ref":"#/texts/1"},{"$ref":"#/texts/2"},{"$ref":"#/texts/0"}],"label":"unspecified"},
          "groups":[],"tables":[],
          "texts":[
            {"self_ref":"#/texts/0","parent":{"$ref":"#/body"},"children":[],"content_layer":"body","label":"footnote","text":"Source: a & b"},
            {"self_ref":"#/texts/1","parent":{"$ref":"#/body"},"children":[],"content_layer":"body","label":"caption","text":"Figure 1"},
            {"self_ref":"#/texts/2","parent":{"$ref":"#/body"},"children":[{"$ref":"#/texts/3"},{"$ref":"#/texts/4"}],"content_layer":"body","label":"code","text":"x = 1",
             "captions":[{"$ref":"#/texts/3"}],"footnotes":[{"$ref":"#/texts/4"}],"code_language":"unknown"},
            {"self_ref":"#/texts/3","parent":{"$ref":"#/texts/2"},"children":[],"content_layer":"body","label":"caption","text":"Listing 1"},
            {"self_ref":"#/texts/4","parent":{"$ref":"#/texts/2"},"children":[],"content_layer":"body","label":"footnote","text":"Runs in O(1)."}],
          "pictures":[{"self_ref":"#/pictures/0","parent":{"$ref":"#/body"},"children":[],"content_layer":"body","label":"picture",
             "captions":[{"$ref":"#/texts/1"}],"footnotes":[{"$ref":"#/texts/0"}],"annotations":[]}]}"##;
        assert_eq!(
            md(json),
            "Figure 1\n\n<!-- image -->\n\nSource: a &amp; b\n\n```\nx = 1\n```\n\n\
             Runs in O(1).\n\nListing 1\n"
        );
    }

    /// A `children` reference back to an ancestor, a chain of thousands of
    /// nested groups, and an item listed under many parents used to recurse
    /// without bound (or multiply the output). Each reference is walked once
    /// and nesting stops at `MAX_DEPTH`, so the documents convert and the
    /// cyclic one reads like its acyclic twin.
    #[test]
    fn cycles_and_deep_chains_convert() {
        let doc = |groups_children: &str| {
            format!(
                r##"{{"schema_name":"DoclingDocument","version":"1.7.0","name":"c",
                "body":{{"self_ref":"#/body","children":[{{"$ref":"#/groups/0"}}],"name":"_root_","label":"unspecified"}},
                "groups":[{{"self_ref":"#/groups/0","parent":{{"$ref":"#/body"}},"children":[{groups_children}],"name":"g","label":"section"}}],
                "texts":[{{"self_ref":"#/texts/0","parent":{{"$ref":"#/groups/0"}},"children":[],"label":"text","text":"hello","prov":[]}}]}}"##
            )
        };
        let plain = md(&doc(r##"{"$ref":"#/texts/0"}"##));
        assert!(plain.contains("hello"));
        let cyclic = md(&doc(
            r##"{"$ref":"#/texts/0"},{"$ref":"#/groups/0"},{"$ref":"#/body"}"##,
        ));
        assert_eq!(cyclic, plain);
        // 20 000 groups, each the only child of the previous one.
        let n = 20_000;
        let groups: Vec<String> = (0..n)
            .map(|i| {
                let child = if i + 1 < n { format!(r##"{{"$ref":"#/groups/{}"}}"##, i + 1) } else { r##"{"$ref":"#/texts/0"}"##.to_string() };
                let parent = if i == 0 { "#/body".to_string() } else { format!("#/groups/{}", i - 1) };
                format!(r##"{{"self_ref":"#/groups/{i}","parent":{{"$ref":"{parent}"}},"children":[{child}],"name":"g","label":"section"}}"##)
            })
            .collect();
        let deep = format!(
            r##"{{"schema_name":"DoclingDocument","version":"1.7.0","name":"d",
            "body":{{"self_ref":"#/body","children":[{{"$ref":"#/groups/0"}}],"name":"_root_","label":"unspecified"}},
            "groups":[{}],
            "texts":[{{"self_ref":"#/texts/0","parent":{{"$ref":"#/groups/19999"}},"children":[],"label":"text","text":"bottom","prov":[]}}]}}"##,
            groups.join(",")
        );
        // Converts (the text sits past the depth cap and is dropped).
        let _ = md(&deep);
    }

    /// #861 (docling-core 2.103.1's `validate_tree`): an item reachable
    /// through two references is walked once — 30 levels of a group whose
    /// two children are the same next group would be 2^30 walks without the
    /// guard — on every export; the leaf renders once.
    #[test]
    fn dag_shaped_tree_converts_once_and_in_linear_time() {
        let n = 30;
        let groups: Vec<String> = (0..n)
            .map(|i| {
                let child = if i + 1 < n {
                    format!(r##"{{"$ref":"#/groups/{}"}}"##, i + 1)
                } else {
                    r##"{"$ref":"#/texts/0"}"##.to_string()
                };
                let parent = if i == 0 { "#/body".to_string() } else { format!("#/groups/{}", i - 1) };
                format!(
                    r##"{{"self_ref":"#/groups/{i}","parent":{{"$ref":"{parent}"}},"children":[{child},{child}],"name":"g","label":"section"}}"##
                )
            })
            .collect();
        let dag = format!(
            r##"{{"schema_name":"DoclingDocument","version":"1.7.0","name":"d",
            "body":{{"self_ref":"#/body","children":[{{"$ref":"#/groups/0"}},{{"$ref":"#/groups/0"}}],"name":"_root_","label":"unspecified"}},
            "groups":[{}],
            "texts":[{{"self_ref":"#/texts/0","parent":{{"$ref":"#/groups/29"}},"children":[],"label":"text","text":"leaf","prov":[]}}]}}"##,
            groups.join(",")
        );
        let doc = DoclingJsonBackend
            .convert(&SourceDocument::from_bytes(
                "dag.json",
                InputFormat::JsonDocling,
                dag.into_bytes(),
            ))
            .unwrap();
        assert_eq!(doc.export_to_markdown().matches("leaf").count(), 1);
        // The JSON writes the text as `text` and `orig`: one item, two hits.
        assert_eq!(doc.export_to_json().matches("leaf").count(), 2);
        assert_eq!(doc.export_to_html().matches("leaf").count(), 1);
        assert_eq!(doc.export_to_latex().matches("leaf").count(), 1);
        assert_eq!(doc.export_to_doclang().matches("leaf").count(), 1);
        let pandoc = doc.export_to_pandoc_json();
        assert_eq!(pandoc.matches("leaf").count(), 1);
        let chunks = docling_core::chunker::HierarchicalChunker.chunk(&doc);
        assert_eq!(chunks.len(), 1, "{chunks:?}");
    }

    /// #861 (docling-core 2.103.1's `max_table_cells` /
    /// `max_total_table_cells`): a table declaring more cells than the
    /// budget — around one real cell — is refused before any grid is
    /// built, with the typed limit on the error's source chain; so is a
    /// document whose tables together pass the total.
    #[test]
    fn oversized_tables_are_rejected_with_a_typed_limit() {
        let doc = |tables: &str| {
            format!(
                r##"{{"schema_name":"DoclingDocument","version":"1.7.0","name":"t",
                "body":{{"self_ref":"#/body","children":[{{"$ref":"#/tables/0"}}],"name":"_root_","label":"unspecified"}},
                "tables":[{tables}]}}"##
            )
        };
        let table = |i: usize, rows: usize, cols: usize| {
            format!(
                r##"{{"self_ref":"#/tables/{i}","parent":{{"$ref":"#/body"}},"children":[],"label":"table","prov":[],
                "data":{{"num_rows":{rows},"num_cols":{cols},"table_cells":[{{"text":"x","start_row_offset_idx":0,"end_row_offset_idx":1,"start_col_offset_idx":0,"end_col_offset_idx":1,"row_span":1,"col_span":1}}],
                "grid":[[{{"text":"x","start_row_offset_idx":0,"end_row_offset_idx":1,"start_col_offset_idx":0,"end_col_offset_idx":1,"row_span":1,"col_span":1}}]]}}}}"##
            )
        };
        let convert = |json: String| {
            DoclingJsonBackend.convert(&SourceDocument::from_bytes(
                "t.json",
                InputFormat::JsonDocling,
                json.into_bytes(),
            ))
        };
        let err = convert(doc(&table(0, 1_000_000, 1_000_000))).unwrap_err();
        let limit = err.limit().expect("typed limit");
        assert_eq!(limit.variable, "DOCLING_RS_MAX_TABLE_CELLS");
        assert_eq!(limit.actual, 1_000_000 * 1_000_000);
        assert!(
            err.to_string().contains("DOCLING_RS_MAX_TABLE_CELLS"),
            "{err}"
        );
        // Rows alone past the cap, with no columns at all.
        assert!(convert(doc(&table(0, 1_000_001, 0)))
            .unwrap_err()
            .limit()
            .is_some());
        // Six tables of 999 999 cells pass each on their own and break the
        // document's total — reachable or not, as upstream counts them.
        let six: Vec<String> = (0..6).map(|i| table(i, 999, 1001)).collect();
        let err = convert(doc(&six.join(","))).unwrap_err();
        assert_eq!(
            err.limit().unwrap().variable,
            "DOCLING_RS_MAX_TOTAL_TABLE_CELLS"
        );
        // Under the caps the one real cell converts.
        let md = convert(doc(&table(0, 1000, 1000)))
            .unwrap()
            .export_to_markdown();
        assert!(md.contains("| x"), "{md}");
    }

    fn item(self_ref: &str, parent: &str, label: &str, extra: &str) -> String {
        format!(
            r#"{{"self_ref":"{self_ref}","parent":{{"$ref":"{parent}"}},"children":[],"label":"{label}","prov":[]{extra}}}"#
        )
    }

    /// #390: where a caption hangs is read back from the source tree, so a
    /// re-export keeps a PDF caption on its picture, an office chart's on the
    /// sheet ahead of the picture, and a declarative backend's on the body.
    #[test]
    fn caption_parents_round_trip() {
        let cap = |i: usize, parent: &str| {
            item(
                &format!("#/texts/{i}"),
                parent,
                "caption",
                r#","orig":"c","text":"c""#,
            )
        };
        let pic = |i: usize, parent: &str, cap: usize| {
            item(
                &format!("#/pictures/{i}"),
                parent,
                "picture",
                &format!(r##","captions":[{{"$ref":"#/texts/{cap}"}}],"annotations":[]"##),
            )
        };
        let json = format!(
            r##"{{"schema_name":"DoclingDocument","version":"1.10.0","name":"t",
            "body":{{"self_ref":"#/body","children":[{{"$ref":"#/pictures/0"}},{{"$ref":"#/groups/0"}},{{"$ref":"#/texts/1"}},{{"$ref":"#/pictures/2"}}],"name":"_root_","label":"unspecified"}},
            "furniture":{{"self_ref":"#/furniture","children":[],"name":"_root_","label":"unspecified"}},
            "groups":[{{"self_ref":"#/groups/0","parent":{{"$ref":"#/body"}},"children":[{{"$ref":"#/texts/2"}},{{"$ref":"#/pictures/1"}}],"name":"sheet","label":"section"}}],
            "texts":[{},{},{}],
            "pictures":[{},{},{}],
            "tables":[],"key_value_items":[],"form_items":[],"pages":{{}}}}"##,
            cap(0, "#/pictures/0"),
            cap(1, "#/body"),
            cap(2, "#/groups/0"),
            pic(0, "#/body", 0),
            pic(1, "#/groups/0", 2),
            pic(2, "#/body", 1),
        );
        let doc = DoclingJsonBackend
            .convert(&SourceDocument::from_bytes(
                "t.json",
                InputFormat::JsonDocling,
                json.into_bytes(),
            ))
            .unwrap();
        let parents: Vec<CaptionParent> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Picture { caption_parent, .. } => Some(*caption_parent),
                _ => None,
            })
            .collect();
        assert_eq!(
            parents,
            [
                CaptionParent::Item,
                CaptionParent::Container,
                CaptionParent::Body
            ]
        );
        let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let parent = |i: usize| {
            v["texts"][i]["parent"]["$ref"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(parent(0), "#/pictures/0");
        // A `section` group is replayed transparently (only lists come back
        // as groups), so the container caption lands beside its picture on
        // the body — ahead of it, as a chart's title is.
        assert_eq!(parent(1), "#/body");
        assert_eq!(parent(2), "#/body");
        let body: Vec<&str> = v["body"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["$ref"].as_str().unwrap())
            .collect();
        assert_eq!(
            body,
            [
                "#/pictures/0",
                "#/texts/1",
                "#/pictures/1",
                "#/texts/2",
                "#/pictures/2"
            ]
        );
    }

    /// #975: a picture's description and classification come back from
    /// `meta` (docling 2.x) or from the deprecated annotations alone (an
    /// older document), so a described document fed back in chunks and
    /// re-exports with its descriptions; a picture carrying neither keeps
    /// `None`.
    #[test]
    fn a_pictures_description_and_classification_survive_the_round_trip() {
        let json_with = |pictures: &str| {
            format!(
                r##"{{"name":"t","body":{{"children":[{{"$ref":"#/pictures/0"}}]}},
                  "texts":[],"groups":[],"tables":[],"pictures":[{pictures}]}}"##
            )
        };
        let picture_of = |doc: &DoclingDocument| {
            doc.nodes
                .iter()
                .find_map(|n| match n {
                    Node::Picture {
                        description,
                        classification,
                        ..
                    } => Some((description.clone(), classification.clone())),
                    _ => None,
                })
                .expect("a picture")
        };
        let convert = |pictures: &str| {
            DoclingJsonBackend
                .convert(&SourceDocument::from_bytes(
                    "t.json",
                    InputFormat::JsonDocling,
                    json_with(pictures).into_bytes(),
                ))
                .unwrap()
        };
        // docling 2.x: `meta` carries both, the annotations repeat them.
        let doc = convert(
            r##"{"self_ref":"#/pictures/0","label":"picture","captions":[],"children":[],
                 "meta":{"description":{"created_by":"mock-vision","text":"A cat on a mat."},
                         "classification":{"predictions":[{"confidence":0.9,"created_by":"DocumentPictureClassifier","class_name":"natural_image"}]}},
                 "annotations":[{"kind":"description","text":"A cat on a mat.","provenance":"mock-vision"}]}"##,
        );
        let (desc, classes) = picture_of(&doc);
        let desc = desc.expect("the description");
        assert_eq!(
            (desc.text.as_str(), desc.provenance.as_str()),
            ("A cat on a mat.", "mock-vision")
        );
        let classes = classes.expect("the classification");
        assert_eq!(classes[0].class_name, "natural_image");
        assert!((classes[0].confidence - 0.9).abs() < 1e-6);
        // The description is text the chunker sees: a document whose only
        // text is a picture description chunks to that description.
        let chunks = docling_core::chunker::HierarchicalChunker.chunk(&doc);
        assert!(
            chunks.iter().any(|c| c.text.contains("A cat on a mat.")),
            "{chunks:?}"
        );
        // …and the re-export carries it the way the input did.
        let out: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(
            out["pictures"][0]["meta"]["description"]["text"],
            "A cat on a mat."
        );
        assert_eq!(
            out["pictures"][0]["meta"]["description"]["created_by"],
            "mock-vision"
        );

        // An older document: the deprecated annotations alone.
        let doc = convert(
            r##"{"self_ref":"#/pictures/0","label":"picture","captions":[],"children":[],
                 "annotations":[{"kind":"classification","provenance":"DocumentPictureClassifier",
                                 "predicted_classes":[{"class_name":"logo","confidence":0.75}]},
                                {"kind":"description","text":"Old style.","provenance":"old-model"}]}"##,
        );
        let (desc, classes) = picture_of(&doc);
        assert_eq!(
            desc.map(|d| (d.text, d.provenance)),
            Some(("Old style.".into(), "old-model".into()))
        );
        assert_eq!(
            classes.map(|c| c[0].class_name.clone()).as_deref(),
            Some("logo")
        );

        // Neither: nothing invented.
        let doc = convert(
            r##"{"self_ref":"#/pictures/0","label":"picture","captions":[],"children":[],"annotations":[]}"##,
        );
        assert_eq!(picture_of(&doc), (None, None));
    }

    /// #403: a picture's `image` (docling's `ImageRef`, a `data:` URI) comes
    /// back as the node's image — so `--images embedded`/`referenced` have
    /// pixels to work with — and a *referenced* image (a path relative to the
    /// JSON file, as `--images referenced` writes) is read from disk.
    #[test]
    fn a_pictures_image_survives_the_round_trip() {
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAADwAAAAoCAIAAAAt2Q6oAAAASUlEQVR4nO3OQQ3AIAAAMUAI/qUgCw97HFnSKug8e4+/Wa8DX0hXpCvSFemKdEW6Il2RrkhXpCvSFemKdEW6Il2RrkhXpCvSlQtBOQFUzKPJpQAAAABJRU5ErkJggg==";
        let json_with = |uri: &str| {
            format!(
                r##"{{"name":"t","body":{{"children":[{{"$ref":"#/pictures/0"}}]}},
                  "texts":[],"groups":[],"tables":[],
                  "pictures":[{{"self_ref":"#/pictures/0","label":"picture","captions":[],"children":[],
                    "image":{{"mimetype":"image/png","dpi":72,"size":{{"width":60.0,"height":40.0}},"uri":"{uri}"}}}}]}}"##
            )
        };
        let image_of = |doc: &DoclingDocument| {
            doc.nodes
                .iter()
                .find_map(|n| match n {
                    Node::Picture { image, .. } => image.clone(),
                    _ => None,
                })
                .expect("the picture keeps its image")
        };
        let doc = DoclingJsonBackend
            .convert(&SourceDocument::from_bytes(
                "t.json",
                InputFormat::JsonDocling,
                json_with(&format!("data:image/png;base64,{PNG}")).into_bytes(),
            ))
            .unwrap();
        let img = image_of(&doc);
        assert_eq!(
            (img.mimetype.as_str(), img.width, img.height),
            ("image/png", 60, 40)
        );
        assert_eq!(img.data_uri(), format!("data:image/png;base64,{PNG}"));
        assert!(doc
            .export_to_markdown_with_images(docling_core::ImageMode::Embedded, "artifacts")
            .0
            .contains(&format!("![Image](data:image/png;base64,{PNG})")));

        // Referenced: the JSON sits next to its `artifacts/` directory.
        let dir = std::env::temp_dir().join(format!(
            "docling-rs-json-pic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("artifacts")).unwrap();
        std::fs::write(
            dir.join("artifacts/image_000000.png"),
            docling_core::base64::decode(PNG).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("t.json"), json_with("artifacts/image_000000.png")).unwrap();
        let doc = DoclingJsonBackend
            .convert(&SourceDocument::from_file(dir.join("t.json")).unwrap())
            .unwrap();
        let img = image_of(&doc);
        assert_eq!((img.width, img.height), (60, 40));
        assert_eq!(img.data_uri(), format!("data:image/png;base64,{PNG}"));
        // An unreadable reference degrades to a placeholder, never an error.
        std::fs::write(dir.join("t.json"), json_with("artifacts/missing.png")).unwrap();
        let doc = DoclingJsonBackend
            .convert(&SourceDocument::from_file(dir.join("t.json")).unwrap())
            .unwrap();
        assert!(doc
            .nodes
            .iter()
            .any(|n| matches!(n, Node::Picture { image: None, .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #384: a caption leads its picture and its table, and trails its *code*
    /// block — a `CodeItem` is docling's only floating text item, and the text
    /// serializer appends a floating item's captions after its own text.
    #[test]
    fn captions_lead_a_picture_and_a_table_but_trail_code() {
        let json = r##"{
          "name":"n","body":{"children":[{"$ref":"#/pictures/0"},{"$ref":"#/tables/0"},{"$ref":"#/texts/2"}]},
          "texts":[
            {"self_ref":"#/texts/0","label":"caption","text":"Figure 1: a duck","children":[]},
            {"self_ref":"#/texts/1","label":"caption","text":"Table 1: the counts","children":[]},
            {"self_ref":"#/texts/2","label":"code","text":"let x = 1;","children":[],
             "captions":[{"$ref":"#/texts/3"}]},
            {"self_ref":"#/texts/3","label":"caption","text":"Listing 1: a binding","children":[]}
          ],
          "groups":[],
          "tables":[{"self_ref":"#/tables/0","label":"table","captions":[{"$ref":"#/texts/1"}],
                     "data":{"grid":[[{"text":"a"}]]},"children":[]}],
          "pictures":[{"self_ref":"#/pictures/0","label":"picture","captions":[{"$ref":"#/texts/0"}],"children":[]}]
        }"##;
        assert_eq!(
            md(json),
            "Figure 1: a duck\n\n<!-- image -->\n\nTable 1: the counts\n\n| a   |\n|-----|\n\n```\nlet x = 1;\n```\n\nListing 1: a binding\n"
        );
    }

    /// A caption no floating item claims is an ordinary body item and renders
    /// where it sits; one that is claimed renders only with its element.
    #[test]
    fn an_unclaimed_caption_still_renders() {
        let json = r##"{
          "name":"n","body":{"children":[{"$ref":"#/texts/0"},{"$ref":"#/pictures/0"},{"$ref":"#/texts/1"}]},
          "texts":[
            {"self_ref":"#/texts/0","label":"caption","text":"Figure 6: nobody claims me","children":[]},
            {"self_ref":"#/texts/1","label":"caption","text":"Figure 7: claimed","children":[]}
          ],
          "groups":[],"tables":[],
          "pictures":[{"self_ref":"#/pictures/0","label":"picture","captions":[{"$ref":"#/texts/1"}],"children":[]}]
        }"##;
        assert_eq!(
            md(json),
            "Figure 6: nobody claims me\n\nFigure 7: claimed\n\n<!-- image -->\n"
        );
    }

    /// docling-core prints a marker it already considers valid Markdown
    /// verbatim, so a reference list that continues over a page break keeps its
    /// real numbering instead of restarting at 1.
    #[test]
    fn a_numeric_marker_is_printed_verbatim() {
        let json = r##"{
          "name":"n","body":{"children":[{"$ref":"#/groups/0"}]},
          "texts":[
            {"self_ref":"#/texts/0","label":"list_item","text":"Xue, W.","marker":"18.","enumerated":true,"children":[]},
            {"self_ref":"#/texts/1","label":"list_item","text":"Ye, J.","marker":"19.","enumerated":true,"children":[]}
          ],
          "groups":[{"self_ref":"#/groups/0","label":"list","name":"list",
                     "children":[{"$ref":"#/texts/0"},{"$ref":"#/texts/1"}]}],
          "tables":[],"pictures":[]
        }"##;
        assert_eq!(md(json), "18. Xue, W.\n19. Ye, J.\n");
    }

    /// Any *other* non-empty marker forces a bullet and is kept in front of the
    /// text when it holds a letter or digit (docling: `- (1) Human Annotation`),
    /// because a number is computed only for an item with no marker at all.
    #[test]
    fn a_non_markdown_marker_forces_a_bullet_and_is_kept() {
        let json = r##"{
          "name":"n","body":{"children":[{"$ref":"#/groups/0"}]},
          "texts":[
            {"self_ref":"#/texts/0","label":"list_item","text":"Human Annotation","marker":"(1)","enumerated":true,"children":[]},
            {"self_ref":"#/texts/1","label":"list_item","text":"Red - PDF cells","marker":"a.","enumerated":true,"children":[]},
            {"self_ref":"#/texts/2","label":"list_item","text":"a stray glyph","marker":"\u0084","enumerated":false,"children":[]}
          ],
          "groups":[{"self_ref":"#/groups/0","label":"list","name":"list",
                     "children":[{"$ref":"#/texts/0"},{"$ref":"#/texts/1"},{"$ref":"#/texts/2"}]}],
          "tables":[],"pictures":[]
        }"##;
        assert_eq!(
            md(json),
            "- (1) Human Annotation\n- a. Red - PDF cells\n- a stray glyph\n"
        );
    }

    /// With no marker at all the group decides: its first child being an
    /// enumerated item numbers every item by its *position among the children*,
    /// nested groups included — so an item after a sublist is numbered past it.
    #[test]
    fn an_unmarked_item_is_numbered_by_its_position_in_the_group() {
        let json = r##"{
          "name":"n","body":{"children":[{"$ref":"#/groups/0"}]},
          "texts":[
            {"self_ref":"#/texts/0","label":"list_item","text":"one","marker":"","enumerated":true,"children":[]},
            {"self_ref":"#/texts/1","label":"list_item","text":"nested","marker":"","enumerated":true,"children":[]},
            {"self_ref":"#/texts/2","label":"list_item","text":"after","marker":"","enumerated":true,"children":[]}
          ],
          "groups":[
            {"self_ref":"#/groups/0","label":"list","name":"list",
             "children":[{"$ref":"#/texts/0"},{"$ref":"#/groups/1"},{"$ref":"#/texts/2"}]},
            {"self_ref":"#/groups/1","label":"list","name":"list","children":[{"$ref":"#/texts/1"}]}
          ],
          "tables":[],"pictures":[]
        }"##;
        // The nested group takes position 2, so "after" is the third child.
        assert_eq!(md(json), "1. one\n    1. nested\n3. after\n");
    }

    /// A group whose first child is not an enumerated item bullets every item,
    /// whatever each one's own flag says.
    #[test]
    fn a_group_starting_on_a_bullet_stays_bulleted() {
        let json = r##"{
          "name":"n","body":{"children":[{"$ref":"#/groups/0"}]},
          "texts":[
            {"self_ref":"#/texts/0","label":"list_item","text":"bullet first","marker":"","enumerated":false,"children":[]},
            {"self_ref":"#/texts/1","label":"list_item","text":"still a bullet","marker":"","enumerated":true,"children":[]}
          ],
          "groups":[{"self_ref":"#/groups/0","label":"list","name":"list",
                     "children":[{"$ref":"#/texts/0"},{"$ref":"#/texts/1"}]}],
          "tables":[],"pictures":[]
        }"##;
        assert_eq!(md(json), "- bullet first\n- still a bullet\n");
    }

    /// Checkbox items carry docling's task-list marker, and code and formulas
    /// are the two items it serializes unescaped.
    #[test]
    fn checkboxes_render_and_code_is_not_escaped() {
        let json = r##"{
          "name":"n","body":{"children":[{"$ref":"#/texts/0"},{"$ref":"#/texts/1"},{"$ref":"#/texts/2"},{"$ref":"#/texts/3"}]},
          "texts":[
            {"self_ref":"#/texts/0","label":"checkbox_selected","text":"done","children":[]},
            {"self_ref":"#/texts/1","label":"checkbox_unselected","text":"todo","children":[]},
            {"self_ref":"#/texts/2","label":"code","text":"VERIFY_GROUP_FOR_USER ( SESSION_USER )","children":[]},
            {"self_ref":"#/texts/3","label":"formula","text":"a_1 + b_2","orig":"a_1 + b_2","children":[]}
          ],
          "groups":[],"tables":[],"pictures":[]
        }"##;
        assert_eq!(
            md(json),
            "- [x] done\n\n- [ ] todo\n\n```\nVERIFY_GROUP_FOR_USER ( SESSION_USER )\n```\n\n$$a_1 + b_2$$\n"
        );
    }

    /// docling ≥ 2.5x nests body content under its `section_header`; the
    /// nested items must be walked, not dropped with the heading's subtree.
    #[test]
    fn walks_children_nested_under_headings() {
        let json = r##"{
          "name": "n", "body": {"children": [{"$ref":"#/texts/0"}]},
          "texts": [
            {"self_ref":"#/texts/0","label":"section_header","level":1,"text":"Intro",
             "children":[{"$ref":"#/texts/1"},{"$ref":"#/texts/2"}]},
            {"self_ref":"#/texts/1","label":"text","text":"First para","children":[]},
            {"self_ref":"#/texts/2","label":"section_header","level":2,"text":"Sub",
             "children":[{"$ref":"#/texts/3"}]},
            {"self_ref":"#/texts/3","label":"text","text":"Deep para","children":[]}
          ],
          "groups": [], "tables": [], "pictures": []
        }"##;
        let doc = DoclingJsonBackend
            .convert(&SourceDocument::from_bytes(
                "t.json",
                InputFormat::JsonDocling,
                json.as_bytes().to_vec(),
            ))
            .unwrap();
        assert_eq!(
            doc.export_to_markdown(),
            "## Intro\n\nFirst para\n\n### Sub\n\nDeep para\n"
        );
    }

    #[test]
    fn walks_body_tree_with_formatting_and_lists() {
        let json = r##"{
          "schema_name": "DoclingDocument", "name": "t",
          "body": {"children": [{"$ref":"#/texts/0"},{"$ref":"#/texts/1"},{"$ref":"#/groups/0"}]},
          "texts": [
            {"self_ref":"#/texts/0","label":"title","text":"Doc"},
            {"self_ref":"#/texts/1","label":"section_header","level":1,"text":"Sec","hyperlink":"http://x"},
            {"self_ref":"#/texts/2","label":"list_item","text":"one","enumerated":false},
            {"self_ref":"#/texts/3","label":"list_item","text":"two","enumerated":false,
             "formatting":{"bold":true,"italic":false,"strikethrough":false}}
          ],
          "groups": [{"self_ref":"#/groups/0","label":"list",
                      "children":[{"$ref":"#/texts/2"},{"$ref":"#/texts/3"}]}],
          "tables": [], "pictures": []
        }"##;
        let src =
            SourceDocument::from_bytes("t", InputFormat::JsonDocling, json.as_bytes().to_vec());
        let md = DoclingJsonBackend
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert!(
            md.starts_with("# Doc\n\n## [Sec](http://x)\n\n- one\n- **two**"),
            "got:\n{md}"
        );
    }

    fn convert(json: &str) -> DoclingDocument {
        DoclingJsonBackend
            .convert(&SourceDocument::from_bytes(
                "t.json",
                InputFormat::JsonDocling,
                json.as_bytes().to_vec(),
            ))
            .unwrap()
    }

    /// #1014: docling-core's `RefItem` is `cref` with the `$ref` alias, so a
    /// file written by field name reads like a `$ref` one — it converted to
    /// an empty document.
    #[test]
    fn cref_references_read_like_dollar_ref() {
        let doc = |key: &str| {
            format!(
                r##"{{"schema_name":"DoclingDocument","name":"n",
                "body":{{"children":[{{"{key}":"#/texts/0"}},{{"{key}":"#/tables/0"}},{{"{key}":"#/groups/0"}}]}},
                "texts":[
                  {{"self_ref":"#/texts/0","label":"section_header","level":1,"text":"Head","children":[{{"{key}":"#/texts/1"}}]}},
                  {{"self_ref":"#/texts/1","label":"text","text":"Under the head","parent":{{"{key}":"#/texts/0"}}}},
                  {{"self_ref":"#/texts/2","label":"caption","text":"Table 1","parent":{{"{key}":"#/tables/0"}}}},
                  {{"self_ref":"#/texts/3","label":"list_item","text":"item","marker":"-"}}],
                "groups":[{{"self_ref":"#/groups/0","label":"list","name":"list","children":[{{"{key}":"#/texts/3"}}]}}],
                "tables":[{{"self_ref":"#/tables/0","label":"table","captions":[{{"{key}":"#/texts/2"}}],
                  "data":{{"num_rows":1,"num_cols":1,"grid":[[{{"text":"c","column_header":false}}]]}}}}]}}"##
            )
        };
        let with_dollar = md(&doc("$ref"));
        assert!(with_dollar.contains("Under the head") && with_dollar.contains("Table 1"));
        assert!(with_dollar.contains("- item"), "{with_dollar}");
        assert_eq!(md(&doc("cref")), with_dollar);
        assert_eq!(
            convert(&doc("cref")).export_to_json(),
            convert(&doc("$ref")).export_to_json()
        );
    }

    /// #1014: a linked text item's HTML export is an `<a href>`, not the
    /// Markdown link spelled out; the JSON keeps the `hyperlink` and the
    /// Markdown is unchanged.
    #[test]
    fn hyperlinked_text_exports_an_anchor() {
        let d = convert(
            r##"{"schema_name":"DoclingDocument","name":"n",
            "body":{"children":[{"$ref":"#/texts/0"},{"$ref":"#/texts/1"}]},
            "texts":[{"self_ref":"#/texts/0","label":"text","text":"x","hyperlink":"https://x.org/plain"},
                     {"self_ref":"#/texts/1","label":"text","text":"b","hyperlink":"https://x.org/b",
                      "formatting":{"bold":true}}]}"##,
        );
        let html = d.export_to_html();
        assert!(
            html.contains("<p><a href=\"https://x.org/plain\">x</a></p>"),
            "{html}"
        );
        assert!(!html.contains("[x](https://x.org/plain)"), "{html}");
        let json: Value = serde_json::from_str(&d.export_to_json()).unwrap();
        assert_eq!(json["texts"][0]["text"], "x");
        assert_eq!(json["texts"][0]["hyperlink"], "https://x.org/plain");
        assert_eq!(
            d.export_to_markdown(),
            "[x](https://x.org/plain)\n\n[**b**](https://x.org/b)\n"
        );
    }

    /// #1042: a table whose cells all say `column_header: false` reloads
    /// without a header — row 0 used to be promoted to one, changing the
    /// triplet chunks and the re-export. Without the keys the row-0
    /// default stays.
    #[test]
    fn all_false_header_flags_survive_a_round_trip() {
        use crate::chunker::HierarchicalChunker;
        let html = "<table><tr><td>Year</td><td>Revenue</td></tr><tr><td>2024</td><td>10</td></tr></table>";
        let original = crate::DocumentConverter::new()
            .convert(SourceDocument::from_bytes(
                "t.html",
                InputFormat::Html,
                html.as_bytes().to_vec(),
            ))
            .unwrap()
            .document;
        let json = original.export_to_json();
        let reloaded = convert(&json);
        let row0 = |d: &DoclingDocument| -> Vec<bool> {
            let v: Value = serde_json::from_str(&d.export_to_json()).unwrap();
            v["tables"][0]["data"]["grid"][0]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["column_header"].as_bool().unwrap())
                .collect()
        };
        assert_eq!(row0(&original), [false, false]);
        assert_eq!(row0(&reloaded), [false, false]);
        let chunk = |d: &DoclingDocument| HierarchicalChunker.chunk(d)[0].text.clone();
        assert_eq!(chunk(&original), "Year, 1 = Revenue. 2024, 1 = 10");
        assert_eq!(chunk(&reloaded), chunk(&original));

        // The same JSON without any `column_header` key: row 0 is the header.
        let mut v: Value = serde_json::from_str(&json).unwrap();
        for row in v["tables"][0]["data"]["grid"].as_array_mut().unwrap() {
            for cell in row.as_array_mut().unwrap() {
                cell.as_object_mut().unwrap().remove("column_header");
            }
        }
        let keyless = convert(&v.to_string());
        assert_eq!(chunk(&keyless), "2024, Revenue = 10");
    }

    /// #1091: a `document_index` table (docling's TOC label) reads back as
    /// one and is written out with the same label; a plain table stays
    /// `table`.
    #[test]
    fn a_document_index_table_keeps_its_label_through_the_round_trip() {
        let html = "<table><tr><td>Intro</td><td>1</td></tr></table>";
        let original = crate::DocumentConverter::new()
            .convert(SourceDocument::from_bytes(
                "t.html",
                InputFormat::Html,
                html.as_bytes().to_vec(),
            ))
            .unwrap()
            .document;
        let mut v: Value = serde_json::from_str(&original.export_to_json()).unwrap();
        assert_eq!(v["tables"][0]["label"], "table");
        v["tables"][0]["label"] = "document_index".into();
        let reloaded = convert(&v.to_string());
        assert!(matches!(
            reloaded.nodes.iter().find(|n| matches!(n, Node::Table(_))),
            Some(Node::Table(t)) if t.document_index
        ));
        let out: Value = serde_json::from_str(&reloaded.export_to_json()).unwrap();
        assert_eq!(out["tables"][0]["label"], "document_index");
        assert_eq!(
            reloaded.export_to_markdown(),
            original.export_to_markdown(),
            "a table of contents renders as a table"
        );
    }
}
