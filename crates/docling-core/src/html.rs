//! HTML serializer — the Rust counterpart of docling-core's
//! `HTMLDocSerializer` with default `HTMLParams` (single-column style, body
//! layer, formulas as MathML), scored byte-for-byte against the HTML
//! groundtruth upstream ships for its ODF and DOCX fixtures (#492).
//!
//! Like [`crate::latex`] this walks the *JSON* document model rather than
//! the flat [`Node`](crate::Node) stream: upstream's serializer reads
//! `DoclingDocument`, and [`crate::json`] already reproduces that structure
//! item for item (the backend-built item tree where one exists, docling's
//! generic grouping rules otherwise), so rendering from
//! [`export_to_json_value`](crate::DoclingDocument::export_to_json_value)
//! inherits every heading-nesting, inline-group and rich-cell decision the
//! JSON export already gets right instead of re-deriving it.
//!
//! What is reproduced: the head (`<title>` = document name, generator meta,
//! the single-column stylesheet), `<div class='page'>` around the body,
//! `<h1>` for a title and `<h{level+1}>` for section headers, `<p>` for
//! text, `<li style="list-style-type: '{marker} ';">` with nested
//! `<ol>`/`<ul>` (ordered when the first item is enumerated), inline groups
//! as `<span class='inline-group'>` with parts joined by one space,
//! `<strong>`/`<em>`/`<u>`/`<del>`/`<sub>`/`<sup>`/`<a href>` applied in
//! upstream's `post_process` order *around* the element, `\n` → `<br>`,
//! tables with `<th>` for header cells, `rowspan`/`colspan`, rich cells
//! rendered as their block content, `<caption>`/`<figcaption>` holding
//! `<div class="caption">`, pictures as `<figure>` with the image per
//! [`ImageMode`] (a `data:` URI when embedded, a relative path when
//! referenced, nothing for the placeholder mode — upstream's default) and
//! the picture `meta` block (`<details class="docling-meta">` with the
//! classification and the tabular-chart table), key-value / form graphs as
//! `<div class="key-value-region">` / `<div class="form-container">`, and
//! `dir="rtl"` on captions and cells whose text reads right-to-left, and
//! formulas as MathML through [`crate::mathml`] — a port of the
//! `latex2mathml` library upstream runs, with the `<annotation
//! encoding="TeX">` source and upstream's `<pre>` fallback for LaTeX the
//! library rejects. Escaping follows Python's `html.escape`: `&<>`
//! everywhere, plus `"` and `'` where upstream calls it with `quote=True`
//! (captions, table cells, the title, meta names); code and formula source
//! is escaped too (docling-core 2.106, #894) — only the Markdown backend's
//! raw HTML blocks stay markup.

use serde_json::Value;

use crate::document::{ContentLayers, DoclingDocument, HtmlExportOptions};
use crate::json::{TableData, TableView};
use crate::markdown::ImageMode;

/// docling-core's `_get_css_for_single_column()`, verbatim.
const CSS_SINGLE_COLUMN: &str = r##"<style>
    html {
        background-color: #f5f5f5;
        font-family: Arial, sans-serif;
        line-height: 1.6;
    }
    body {
        max-width: 800px;
        margin: 0 auto;
        padding: 2rem;
        background-color: white;
        box-shadow: 0 0 10px rgba(0,0,0,0.1);
    }
    h1, h2, h3, h4, h5, h6 {
        color: #333;
        margin-top: 1.5em;
        margin-bottom: 0.5em;
    }
    h1 {
        font-size: 2em;
        border-bottom: 1px solid #eee;
        padding-bottom: 0.3em;
    }
    table {
        border-collapse: collapse;
        margin: 1em 0;
        width: 100%;
    }
    th, td {
        border: 1px solid #ddd;
        padding: 8px;
        text-align: left;
    }
    th {
        background-color: #f2f2f2;
        font-weight: bold;
    }
    figure {
        margin: 1.5em 0;
        text-align: center;
    }
    figcaption {
        color: #666;
        font-style: italic;
        margin-top: 0.5em;
    }
    img {
        max-width: 100%;
        height: auto;
    }
    pre {
        background-color: #f6f8fa;
        border-radius: 3px;
        padding: 1em;
        overflow: auto;
    }
    code {
        font-family: monospace;
        background-color: #f6f8fa;
        padding: 0.2em 0.4em;
        border-radius: 3px;
    }
    pre code {
        background-color: transparent;
        padding: 0;
    }
    .formula {
        text-align: center;
        padding: 0.5em;
        margin: 1em 0;
        background-color: #f9f9f9;
    }
    .formula-not-decoded {
        text-align: center;
        padding: 0.5em;
        margin: 1em 0;
        background: repeating-linear-gradient(
            45deg,
            #f0f0f0,
            #f0f0f0 10px,
            #f9f9f9 10px,
            #f9f9f9 20px
        );
    }
    .page-break {
        page-break-after: always;
        border-top: 1px dashed #ccc;
        margin: 2em 0;
    }
    .key-value-region {
        background-color: #f9f9f9;
        padding: 1em;
        border-radius: 4px;
        margin: 1em 0;
    }
    .key-value-region dt {
        font-weight: bold;
    }
    .key-value-region dd {
        margin-left: 1em;
        margin-bottom: 0.5em;
    }
    .form-container {
        border: 1px solid #ddd;
        padding: 1em;
        border-radius: 4px;
        margin: 1em 0;
    }
    .form-item {
        margin-bottom: 0.5em;
    }
    .image-classification {
        font-size: 0.9em;
        color: #666;
        margin-top: 0.5em;
    }
    details.docling-meta {
        margin: 0.5em 0;
        font-size: 0.9em;
        text-align: left;
    }
    figure details.docling-meta {
        text-align: left;
    }
    details.docling-meta > summary {
        cursor: pointer;
        color: #555;
        font-style: italic;
        padding: 2px 6px;
    }
    .docling-meta-field {
        background-color: #f0f0f0;
        border-left: 3px solid #ccc;
        padding: 6px 10px;
        margin: 4px 0 4px 1em;
        border-radius: 3px;
        text-align: left;
    }
    .docling-meta-field-label {
        font-weight: bold;
        color: #444;
    }
    pre.docling-meta-code {
        background-color: #1e1e1e;
        color: #d4d4d4;
        border-radius: 4px;
        padding: 10px 12px;
        margin: 6px 0;
        overflow-x: auto;
        font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace;
        font-size: 0.85em;
        line-height: 1.45;
        white-space: pre;
        tab-size: 4;
    }
    pre.docling-meta-code code {
        background: transparent;
        border: none;
        padding: 0;
        color: inherit;
        font-family: inherit;
        font-size: inherit;
        display: block;
        white-space: pre;
    }
</style>"##;

/// Serialize `doc` to a complete HTML document per `options`. Pictures
/// follow its `image_mode`; in [`ImageMode::Referenced`] the returned
/// artifacts are `(path, bytes)` pairs named
/// `<artifacts_dir>/image_NNNNNN.<ext>` exactly as the Markdown export names
/// them, and the `<img src>` points at them. Only items on the option's
/// content `layers` render (#499). No trailing newline — upstream returns
/// the joined parts as is.
pub fn to_html(
    doc: &DoclingDocument,
    options: &HtmlExportOptions,
) -> (String, Vec<(String, Vec<u8>)>) {
    render_html(doc, options, None)
}

/// docling's `serialize_markdown_for_html` (`backend/utils/markdown_html.py`,
/// docling#4777): the HTML a Markdown document with a raw HTML block is
/// re-converted from. Every picture is `<figure>[<figcaption>…]<img
/// src="{prefix}{self_ref}"></figure>` — a document-local key the HTML
/// backend resolves back to the picture's image, whatever the image mode —
/// a code item starting with `raw_marker` (a raw HTML block) is written raw
/// and any other code escaped (upstream writes all code raw — docling-core
/// 2.103 — so an inline `` `<img>` `` became a picture; or, with 2.104, all
/// escaped, so the blocks stayed text), and the rest is the default export.
#[doc(hidden)]
pub fn to_html_with_picture_refs(doc: &DoclingDocument, prefix: &str, raw_marker: &str) -> String {
    render_html(
        doc,
        &HtmlExportOptions::default(),
        Some((prefix, raw_marker)),
    )
    .0
}

fn render_html(
    doc: &DoclingDocument,
    options: &HtmlExportOptions,
    reparse: Option<(&str, &str)>,
) -> (String, Vec<(String, Vec<u8>)>) {
    // The tables stay typed (#875): a `Value` per cell was most of an HTML
    // export's peak on a large sheet.
    let (json, tables) = crate::json::to_json_typed_tables(doc, false);
    let alt_text_meta = doc.picture_alt_text;
    // The walk recurses per nesting level (an item renders its children
    // inside itself), and an XBRL instance nests thousands deep — more than
    // a 2 MB test-thread stack holds — so it runs on a thread of its own.
    let render = || {
        let mut ser = Serializer::new(&json, options);
        ser.tables = &tables;
        ser.alt_text_meta = alt_text_meta;
        ser.picture_ref_prefix = reparse.map(|(prefix, _)| prefix);
        ser.raw_code_marker = reparse.map(|(_, marker)| marker);
        let body = ser.serialize_body();
        let name = json.get("name").and_then(Value::as_str).unwrap_or_default();
        let title = if name.is_empty() {
            "Docling Document".to_string()
        } else {
            escape(name, true)
        };
        let html = format!(
            "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"UTF-8\"/>\n<title>{title}</title>\n\
             <meta name=\"generator\" content=\"Docling HTML Serializer\"/>\n{CSS_SINGLE_COLUMN}\n</head>\n\
             <body>\n<div class='page'>\n{body}\n</div>\n</body>\n</html>"
        );
        (html, ser.artifacts)
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("docling-html".into())
                .stack_size(256 << 20)
                .spawn_scoped(scope, render)
                .expect("spawn the html serializer thread")
                .join()
                .expect("html serializer thread panicked")
        })
    }
    // wasm32-unknown-unknown has no threads (`spawn` fails, and `expect`
    // would abort the `"html"` output kind): render on the caller's stack.
    #[cfg(target_arch = "wasm32")]
    {
        render()
    }
}

/// Python's `html.escape(s, quote)`.
fn escape(s: &str, quote: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote => out.push_str("&quot;"),
            '\'' if quote => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// docling-core's `_escape_attr_value`: `html.escape(quote=False)` plus
/// `"` → `&quot;`, for a double-quoted attribute (`'` stays, so a CSS
/// string keeps its quotes readable).
fn escape_attr(s: &str) -> String {
    escape(s, false).replace('"', "&quot;")
}

/// docling-core's `_escape_css_string`: a list marker kept inside the
/// single-quoted CSS string of `list-style-type`.
fn escape_css_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\r', "")
        .replace('\n', "\\A ")
}

/// docling-core's `get_text_direction`: `rtl` when the first character is a
/// right-to-left letter (Unicode bidi class R or AL) or more than half of the
/// characters are, `ltr` otherwise (and for empty text).
fn text_direction(text: &str) -> &'static str {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return "ltr";
    };
    if is_rtl_letter(first) {
        return "rtl";
    }
    let total = text.chars().count();
    let rtl = std::iter::once(first)
        .chain(chars)
        .filter(|&c| is_rtl_letter(c))
        .count();
    if rtl as f64 > total as f64 / 2.0 {
        "rtl"
    } else {
        "ltr"
    }
}

/// Bidi classes R and AL, by block: Hebrew, Arabic, Syriac, Thaana, NKo,
/// Samaritan, Mandaic and the Arabic/Hebrew presentation forms — minus the
/// Arabic-Indic digits (class AN) and the blocks' punctuation.
fn is_rtl_letter(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x05BE | 0x05C0 | 0x05C3 | 0x05C6 | 0x05D0..=0x05EA | 0x05EF..=0x05F4
        | 0x0608 | 0x060B | 0x060D | 0x061B..=0x064A | 0x066D..=0x066F | 0x0671..=0x06D5
        | 0x06E5 | 0x06E6 | 0x06EE | 0x06EF | 0x06FA..=0x070D | 0x070F | 0x0710 | 0x0712..=0x072F
        | 0x074D..=0x07A5 | 0x07B1 | 0x07C0..=0x07EA | 0x07F4 | 0x07F5 | 0x07FA | 0x07FE..=0x0815
        | 0x081A | 0x0824 | 0x0828 | 0x0830..=0x083E | 0x0840..=0x0858 | 0x085E | 0x0860..=0x086A
        | 0x0870..=0x088E | 0x08A0..=0x08C9 | 0x200F | 0xFB1D | 0xFB1F..=0xFB28 | 0xFB2A..=0xFB4F
        | 0xFB50..=0xFD3D | 0xFD50..=0xFDC7 | 0xFDF0..=0xFDFC | 0xFE70..=0xFEFC
        | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
}

/// A referenced image's `src`: docling-core's `html.escape(_escape_uri_path(uri))`
/// (docling-core#772, 2.102) — the destination encoded exactly as the Markdown
/// export encodes it (a URL keeps its scheme and delimiters, `%` escapes are
/// not encoded twice, `&`, `,`, `+` stay), then escaped for the attribute.
/// Python's plain `quote` used to turn `Q&A, v2+final` into `Q%26A%2C…` and
/// `https://` into `https%3A//`.
fn img_src(uri: &str) -> String {
    escape(&crate::markdown::escape_uri_path(uri), true)
}

/// Python's `str.capitalize()` after `_` → space: the first character
/// upper-cased, the rest lower-cased (`bar_chart` → `Bar chart`).
fn humanize(text: &str) -> String {
    let text = text.replace("__", "_").replace('_', " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => text,
    }
}

/// docling's `Formatting`.
#[derive(Default, Clone, Copy)]
struct Fmt {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    sub: bool,
    sup: bool,
}

impl Fmt {
    fn from_json(v: Option<&Value>) -> Self {
        let Some(v) = v else {
            return Self::default();
        };
        let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let script = v
            .get("script")
            .and_then(Value::as_str)
            .unwrap_or("baseline");
        Self {
            bold: flag("bold"),
            italic: flag("italic"),
            underline: flag("underline"),
            strikethrough: flag("strikethrough"),
            sub: script == "sub",
            sup: script == "super",
        }
    }
}

/// A grid position of a table: the cell covering it (its index in the
/// [`TableView`]) and where that cell starts, as docling's `TableData.grid`
/// exposes them.
struct GridCell {
    cell: usize,
    start_row: usize,
    start_col: usize,
    row_span: usize,
    col_span: usize,
}

struct Serializer<'a> {
    json: &'a Value,
    /// The document's typed tables, which its table items name
    /// ([`crate::json::to_json_typed_tables`]); empty for a JSON read in.
    tables: &'a [TableData],
    image_mode: ImageMode,
    /// Render pictures' alt text (#676: the document's `picture_alt_text`)
    /// — the `<img alt>` and the visible meta text. Off, the HTML is
    /// docling's byte for byte.
    alt_text_meta: bool,
    artifacts_dir: String,
    /// The content layers rendered (`HTMLParams.layers`).
    layers: ContentLayers,
    /// [`to_html_with_picture_refs`]'s key prefix: pictures render as
    /// docling's `_MarkdownPictureSerializer` writes them.
    picture_ref_prefix: Option<&'a str>,
    /// [`to_html_with_picture_refs`]'s raw-block marker: a code item whose
    /// text starts with it is written raw, any other escaped.
    raw_code_marker: Option<&'a str>,
    artifacts: Vec<(String, Vec<u8>)>,
    pic_index: usize,
    /// Refs already rendered (upstream's `visited`), so the body traversal
    /// skips what a container rendered inside itself.
    visited: std::collections::HashSet<String>,
    /// Refs of items some floating item holds as its caption or footnote:
    /// rendered by that item, empty when met on their own.
    captions_of_some_item: std::collections::HashSet<String>,
    footnotes_of_some_item: std::collections::HashSet<String>,
    /// Upstream's `_satellite_descendants` (docling-core#865): everything
    /// under such a caption — a mixed caption's inline group and runs —
    /// which the caption renders inside itself, so the outer walk skips it.
    satellite_descendants: std::collections::HashSet<String>,
}

impl<'a> Serializer<'a> {
    fn new(json: &'a Value, options: &HtmlExportOptions) -> Self {
        let mut captions = std::collections::HashSet::new();
        let mut footnotes = std::collections::HashSet::new();
        for bucket in [
            "pictures",
            "tables",
            "key_value_items",
            "form_items",
            "texts",
        ] {
            for item in json
                .get(bucket)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for (key, set) in [("captions", &mut captions), ("footnotes", &mut footnotes)] {
                    for r in item
                        .get(key)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(cref) = r.get("$ref").and_then(Value::as_str) {
                            set.insert(cref.to_string());
                        }
                    }
                }
            }
        }
        let mut satellite_descendants = std::collections::HashSet::new();
        let resolve = |cref: &str| -> Option<&'a Value> {
            let (bucket, idx) = cref.strip_prefix("#/")?.split_once('/')?;
            json.get(bucket)?.get(idx.parse::<usize>().ok()?)
        };
        for cap in &captions {
            let mut stack: Vec<&str> = resolve(cap).map(Self::children).unwrap_or_default();
            while let Some(r) = stack.pop() {
                if satellite_descendants.insert(r.to_string()) {
                    if let Some(child) = resolve(r) {
                        stack.extend(Self::children(child));
                    }
                }
            }
        }
        Self {
            json,
            tables: &[],
            image_mode: options.image_mode,
            alt_text_meta: false,
            artifacts_dir: options.artifacts_dir.clone(),
            layers: options.layers,
            picture_ref_prefix: None,
            raw_code_marker: None,
            artifacts: Vec::new(),
            pic_index: 0,
            visited: std::collections::HashSet::new(),
            captions_of_some_item: captions,
            footnotes_of_some_item: footnotes,
            satellite_descendants,
        }
    }

    /// Resolve `#/texts/3`-style refs against the JSON buckets.
    fn resolve(&self, cref: &str) -> Option<&'a Value> {
        let rest = cref.strip_prefix("#/")?;
        if rest == "body" {
            return self.json.get("body");
        }
        let (bucket, idx) = rest.split_once('/')?;
        let idx: usize = idx.parse().ok()?;
        self.json.get(bucket)?.get(idx)
    }

    fn children(item: &'a Value) -> Vec<&'a str> {
        item.get("children")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(|r| r.get("$ref").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn self_ref(item: &Value) -> &str {
        item.get("self_ref").and_then(Value::as_str).unwrap_or("")
    }

    fn label(item: &Value) -> &str {
        item.get("label").and_then(Value::as_str).unwrap_or("")
    }

    fn text(item: &Value) -> &str {
        item.get("text").and_then(Value::as_str).unwrap_or("")
    }

    fn is_group(item: &Value) -> bool {
        Self::self_ref(item).starts_with("#/groups/")
    }

    /// Upstream's `get_excluded_refs`: items off the chosen content layers
    /// are excluded (labels and pages are unrestricted). An item without a
    /// `content_layer` is on the body layer, as in docling's model default.
    fn excluded(&self, item: &Value) -> bool {
        let layer = item
            .get("content_layer")
            .and_then(Value::as_str)
            .unwrap_or("body");
        !self.layers.contains_name(layer)
    }

    /// Upstream's `_iterate_items(with_groups=True, traverse_pictures=False)`
    /// from `root`: the subtree in depth-first pre-order. Only items on the
    /// chosen layers are yielded, but the walk descends through the others;
    /// under a picture only its own caption items are visited. `root` itself
    /// is not yielded.
    fn descendants(&self, root: &'a Value, out: &mut Vec<&'a Value>) {
        let root_is_picture = Self::self_ref(root).starts_with("#/pictures/");
        let caption_refs: Vec<&str> = if root_is_picture {
            root.get("captions")
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .filter_map(|r| r.get("$ref").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        for cref in Self::children(root) {
            if root_is_picture && !caption_refs.contains(&cref) {
                continue;
            }
            let Some(child) = self.resolve(cref) else {
                continue;
            };
            if !self.excluded(child) {
                out.push(child);
            }
            self.descendants(child, out);
        }
    }

    /// Upstream's `get_parts(item)`: every not-yet-visited descendant,
    /// serialized in order; a container marks what it renders as visited, so
    /// the flat walk skips it.
    fn parts(&mut self, item: &'a Value, inline: bool) -> Vec<String> {
        let mut nodes = Vec::new();
        self.descendants(item, &mut nodes);
        // `_skip_in_walk`: a caption's content is skipped unless the walk
        // starts inside that caption.
        let root = Self::self_ref(item);
        let inside_satellite =
            self.satellite_descendants.contains(root) || self.captions_of_some_item.contains(root);
        let mut out = Vec::new();
        for node in nodes {
            let r = Self::self_ref(node).to_string();
            if self.visited.contains(&r)
                || (!inside_satellite && self.satellite_descendants.contains(&r))
            {
                continue;
            }
            self.visited.insert(r);
            let text = self.serialize(node, inline);
            if !text.is_empty() {
                out.push(text);
            }
        }
        out
    }

    fn serialize_body(&mut self) -> String {
        let body = self.json.get("body").cloned().unwrap_or(Value::Null);
        // `body` is borrowed for the whole walk; the clone is one small
        // object (its `children` refs), the items themselves stay in `json`.
        let body: &'a Value = Box::leak(Box::new(body));
        self.visited.insert("#/body".to_string());
        self.parts(body, false).join("\n")
    }

    /// Upstream's `DocSerializer.serialize` for one item (already marked
    /// visited by the caller): the per-kind serializer, then the item's meta
    /// block after it.
    fn serialize(&mut self, item: &'a Value, inline: bool) -> String {
        let sref = Self::self_ref(item);
        let mut parts: Vec<String> = Vec::new();
        let wraps_meta = sref.starts_with("#/pictures/");
        let meta = if wraps_meta {
            String::new()
        } else {
            self.meta(item)
        };
        let part = if Self::is_group(item) {
            match Self::label(item) {
                "list" => self.list_group(item, inline),
                "inline" => self.inline_group(item),
                _ => self.parts(item, inline).join("\n"),
            }
        } else if sref.starts_with("#/texts/") {
            if self.captions_of_some_item.contains(sref)
                || self.footnotes_of_some_item.contains(sref)
            {
                return String::new();
            }
            if self.excluded(item) {
                String::new()
            } else {
                self.text_item(item, inline)
            }
        } else if sref.starts_with("#/tables/") {
            self.table(item)
        } else if sref.starts_with("#/pictures/") {
            self.picture(item)
        } else if sref.starts_with("#/key_value_items/") {
            self.graph_item(item, "key-value-region")
        } else if sref.starts_with("#/form_items/") {
            self.graph_item(item, "form-container")
        } else if sref.starts_with("#/field_regions/") {
            // docling-core has no HTML serializer for the form field items
            // (`FieldRegionItem` / `FieldItem`) yet: its fallback leaves this
            // comment, and the flat walk still reaches their text children.
            "<!-- Unhandled item type: FieldRegionItem -->".to_string()
        } else if sref.starts_with("#/field_items/") {
            "<!-- Unhandled item type: FieldItem -->".to_string()
        } else {
            String::new()
        };
        if !part.is_empty() {
            parts.push(part);
        }
        if !meta.is_empty() {
            parts.push(meta);
        }
        parts.join("\n")
    }

    /// Upstream's `post_process`: formatting wrappers in a fixed order, then
    /// the hyperlink around it all.
    fn post_process(text: String, fmt: Fmt, hyperlink: Option<&str>) -> String {
        let mut res = text;
        if fmt.bold {
            res = format!("<strong>{res}</strong>");
        }
        if fmt.italic {
            res = format!("<em>{res}</em>");
        }
        if fmt.underline {
            res = format!("<u>{res}</u>");
        }
        if fmt.strikethrough {
            res = format!("<del>{res}</del>");
        }
        if fmt.sub {
            res = format!("<sub>{res}</sub>");
        } else if fmt.sup {
            res = format!("<sup>{res}</sup>");
        }
        // An unsafe destination (`javascript:`, #860) keeps the text alone.
        if let Some(url) = hyperlink.filter(|u| crate::hyperlink::is_safe_hyperlink(u)) {
            res = format!("<a href=\"{}\">{res}</a>", crate::hyperlink::html_href(url));
        }
        res
    }

    /// `HTMLTextSerializer.serialize`.
    fn text_item(&mut self, item: &'a Value, inline: bool) -> String {
        let label = Self::label(item);
        let fmt = Fmt::from_json(item.get("formatting"));
        let hyperlink = item.get("hyperlink").and_then(Value::as_str);
        let children = Self::children(item);
        let is_code = label == "code";
        let is_formula = label == "formula";
        let mut post_processed = false;

        // A text item that is only a wrapper around one inline group renders
        // as that group.
        let inline_child = if Self::text(item).is_empty() && children.len() == 1 {
            self.resolve(children[0])
                .filter(|c| Self::is_group(c) && Self::label(c) == "inline")
        } else {
            None
        };
        let has_inline_repr = inline_child.is_some();
        let mut text = match inline_child {
            Some(group) => {
                self.visited.insert(Self::self_ref(group).to_string());
                post_processed = true;
                self.inline_group(group)
            }
            None => {
                let raw = Self::text(item);
                if is_code {
                    match self.raw_code_marker {
                        // The Markdown backend's HTML detour: a raw HTML
                        // block (its markers around it) is meant as markup
                        // and stays raw.
                        Some(marker) if raw.starts_with(marker) => raw.to_string(),
                        // docling-core 2.106 (docling-core#894): code is
                        // `html.escape`d like any text, its newlines kept
                        // (no `<br>` inside `<pre>`).
                        _ => escape(raw, false),
                    }
                } else if is_formula {
                    // A formula's LaTeX feeds `latex2mathml` as is;
                    // [`Self::formula`] escapes it for the `<annotation>`
                    // and the fallbacks.
                    raw.to_string()
                } else {
                    escape(raw, false).replace('\n', "<br>")
                }
            }
        };

        // docling-core 2.103: a plain item's formatting and hyperlink wrap
        // its text inside the tag (`<p><strong>bold</strong></p>`, the
        // groundtruth of `docx_rich_cells`); 2.99 wrapped the tag. Code,
        // formulas and list items keep their own order below.
        if !has_inline_repr && !is_code && !is_formula && label != "list_item" {
            text = Self::post_process(text, fmt, hyperlink);
            post_processed = true;
        }

        if label == "title" {
            text = format!("<h1>{text}</h1>");
        } else if label == "section_header" {
            let level = item.get("level").and_then(Value::as_u64).unwrap_or(1) as usize;
            let level = (level + 1).min(6);
            text = format!("<h{level}>{text}</h{level}>");
        } else if is_formula {
            text = Self::formula(&text, inline, has_inline_repr);
        } else if is_code {
            text = if inline {
                format!("<code>{text}</code>")
            } else {
                format!("<pre><code>{text}</code></pre>")
            };
        } else if label == "list_item" {
            let mut text_parts: Vec<String> = Vec::new();
            if !text.is_empty() {
                if has_inline_repr {
                    text = format!("\n{text}\n");
                } else {
                    text = Self::post_process(text, fmt, hyperlink);
                    post_processed = true;
                }
                text_parts.push(text);
            }
            let nested = self.parts(item, inline);
            let had_nested = !nested.is_empty();
            text_parts.extend(nested);
            text = text_parts.join("\n");
            if had_nested {
                text = format!("\n{text}\n");
            }
            if !text.is_empty() {
                let marker = item.get("marker").and_then(Value::as_str).unwrap_or("");
                text = if marker.is_empty() {
                    format!("<li>{text}</li>")
                } else {
                    format!(
                        "<li style=\"{}\">{text}</li>",
                        escape_attr(&format!(
                            "list-style-type: '{} ';",
                            escape_css_string(marker)
                        ))
                    )
                };
            }
        } else if !inline {
            text = format!("<p>{text}</p>");
        }

        if !post_processed {
            text = Self::post_process(text, fmt, hyperlink);
        }

        if !has_inline_repr && label != "list_item" && !children.is_empty() {
            let nested = self.parts(item, inline).join("\n");
            if !nested.is_empty() {
                text = if text.is_empty() {
                    nested
                } else {
                    format!("{text}\n{nested}")
                };
            }
        }
        text
    }

    /// Upstream's `_process_formula`: `latex2mathml` (see [`crate::mathml`])
    /// with the `<annotation encoding="TeX">` child, `<div>`-wrapped for a
    /// block formula; its `except Exception` branch — `<pre>{text}</pre>` —
    /// for LaTeX the library rejects; the `formula-not-decoded` placeholders
    /// for an empty formula. The image fallbacks (a formula crop from a
    /// stored page image) are not reproduced: the documents this serializer
    /// is scored on carry none. The fallback text is HTML-escaped unless it
    /// already is HTML — a formula wrapping an inline group,
    /// `text_is_html` (docling-core#894).
    fn formula(text: &str, inline: bool, text_is_html: bool) -> String {
        if !text.is_empty() {
            match crate::mathml::formula_to_mathml(text, inline) {
                Ok(mathml) => mathml,
                Err(_) if text_is_html => format!("<pre>{text}</pre>"),
                Err(_) => format!("<pre>{}</pre>", escape(text, false)),
            }
        } else if inline {
            "<span class=\"formula-not-decoded\">Formula not decoded</span>".to_string()
        } else {
            "<div class=\"formula-not-decoded\">Formula not decoded</div>".to_string()
        }
    }

    /// `HTMLListSerializer`: `<ol>` when the first item is enumerated.
    fn list_group(&mut self, item: &'a Value, inline: bool) -> String {
        let parts = self.parts(item, inline);
        if parts.is_empty() {
            return String::new();
        }
        let enumerated = Self::children(item)
            .first()
            .and_then(|r| self.resolve(r))
            .is_some_and(|c| {
                Self::label(c) == "list_item"
                    && c.get("enumerated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            });
        let tag = if enumerated { "ol" } else { "ul" };
        format!("<{tag}>\n{}\n</{tag}>", parts.join("\n"))
    }

    /// `HTMLInlineSerializer`: the parts joined by one space.
    fn inline_group(&mut self, item: &'a Value) -> String {
        let parts = self.parts(item, true);
        if parts.is_empty() {
            return String::new();
        }
        format!("<span class='inline-group'>{}</span>", parts.join(" "))
    }

    /// Upstream's `serialize_captions`: each caption text item as
    /// `<div class="caption">` — its own text and, for a mixed caption
    /// (docling-core#865), its inline group's parts, joined by a space —
    /// the captions joined by a space, inside `tag`.
    fn captions(&mut self, item: &'a Value, tag: &str) -> String {
        let mut results: Vec<String> = Vec::new();
        for r in item
            .get("captions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(cap) = r
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|c| self.resolve(c))
            else {
                continue;
            };
            if !Self::self_ref(cap).starts_with("#/texts/") || self.excluded(cap) {
                continue;
            }
            let text = Self::text(cap);
            let mut parts = vec![escape(text, true)];
            parts.extend(self.parts(cap, true));
            let content = parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            // `_get_plain_text`: the item's text and its descendants'.
            let mut plain = text.to_string();
            let mut nodes = Vec::new();
            self.descendants(cap, &mut nodes);
            for n in nodes {
                plain.push_str(Self::text(n));
            }
            let dir = if text_direction(&plain) == "rtl" {
                " dir=\"rtl\""
            } else {
                ""
            };
            results.push(format!("<div class=\"caption\"{dir}>{content}</div>"));
        }
        if results.is_empty() {
            String::new()
        } else {
            format!("<{tag}>{}</{tag}>", results.join(" "))
        }
    }

    /// docling's `TableData.grid`: every position holds the cell covering it,
    /// and a position no cell covers holds an empty one-by-one placeholder
    /// (upstream fills the grid with empty `TableCell`s first), which the
    /// serializer writes as `<td></td>`.
    fn grid(table: &TableView<'_>) -> Vec<Vec<Option<GridCell>>> {
        let (num_rows, num_cols) = (table.num_rows, table.num_cols);
        let mut grid: Vec<Vec<Option<GridCell>>> = (0..num_rows)
            .map(|_| (0..num_cols).map(|_| None).collect())
            .collect();
        for (i, cell) in table.cells.iter().enumerate() {
            let (r0, c0) = (cell.start_row, cell.start_col);
            let (r1, c1) = (cell.end_row, cell.end_col);
            let (row_span, col_span) = (cell.row_span.max(1), cell.col_span.max(1));
            for row in grid.iter_mut().take(r1.min(num_rows)).skip(r0) {
                for slot in row.iter_mut().take(c1.min(num_cols)).skip(c0) {
                    *slot = Some(GridCell {
                        cell: i,
                        start_row: r0,
                        start_col: c0,
                        row_span,
                        col_span,
                    });
                }
            }
        }
        grid
    }

    /// `HTMLTableSerializer`.
    fn table(&mut self, item: &'a Value) -> String {
        let mut res_parts: Vec<String> = Vec::new();
        let cap = self.captions(item, "caption");
        if !cap.is_empty() {
            res_parts.push(cap);
        }
        if !self.excluded(item) {
            if let Some(data) = item.get("data") {
                let body = self.table_body(data);
                if !body.is_empty() {
                    res_parts.push(format!("<tbody>{body}</tbody>"));
                }
            }
        }
        let text = res_parts.concat();
        if text.is_empty() {
            String::new()
        } else {
            format!("<table>{text}</table>")
        }
    }

    /// The `<tr>` rows of a `TableData` (shared with the tabular-chart meta).
    fn table_body(&mut self, data: &'a Value) -> String {
        let mut body = String::new();
        let table = crate::json::table_view(data, self.tables);
        for (i, row) in Self::grid(&table).iter().enumerate() {
            body.push_str("<tr>");
            for (j, slot) in row.iter().enumerate() {
                let Some(g) = slot else {
                    body.push_str("<td></td>");
                    continue;
                };
                if g.start_row != i || g.start_col != j {
                    continue;
                }
                let cell = &table.cells[g.cell];
                let content = match cell.cell_ref {
                    Some(cref) => match self.resolve(cref) {
                        Some(target) => {
                            self.visited.insert(cref.to_string());
                            self.serialize(target, false)
                        }
                        None => String::new(),
                    },
                    None => escape(cell.text.trim(), true),
                };
                let tag = if cell.column_header || cell.row_header || cell.row_section {
                    "th"
                } else {
                    "td"
                };
                let mut open = tag.to_string();
                if g.row_span > 1 {
                    open.push_str(&format!(" rowspan=\"{}\"", g.row_span));
                }
                if g.col_span > 1 {
                    open.push_str(&format!(" colspan=\"{}\"", g.col_span));
                }
                if text_direction(&content) == "rtl" {
                    open.push_str(" dir=\"rtl\"");
                }
                body.push_str(&format!("<{open}>{content}</{tag}>"));
            }
            body.push_str("</tr>");
        }
        body
    }

    /// `HTMLPictureSerializer`: caption, image per mode, meta — in a
    /// `<figure>`.
    fn picture(&mut self, item: &'a Value) -> String {
        if let Some(prefix) = self.picture_ref_prefix {
            // `_MarkdownPictureSerializer`: the captions and the key, no
            // exclusion check and no meta.
            let cap = self.captions(item, "figcaption");
            let self_ref = item
                .get("self_ref")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let src = escape(&format!("{prefix}{self_ref}"), true);
            return format!("<figure>{cap}<img src=\"{src}\"></figure>");
        }
        let mut res_parts: Vec<String> = Vec::new();
        let cap = self.captions(item, "figcaption");
        if !cap.is_empty() {
            res_parts.push(cap);
        }
        if !self.excluded(item) {
            let uri = item
                .get("image")
                .and_then(|i| i.get("uri"))
                .and_then(Value::as_str);
            // The picture's alt text (#676) is the `<img>`'s alt — what the
            // attribute is for — under `picture_alt_text`; off, the tag is
            // docling's.
            let alt = Self::alt_text(item)
                .filter(|_| self.alt_text_meta)
                .map(|t| format!(" alt=\"{}\"", escape(t, true)))
                .unwrap_or_default();
            match (self.image_mode, uri) {
                (ImageMode::Embedded, Some(uri)) if uri.starts_with("data:") => {
                    // `html.escape(str(uri))` since docling-core#894: a
                    // quote in the URI cannot leave the attribute.
                    res_parts.push(format!("<img src=\"{}\"{alt}>", escape(uri, true)));
                }
                (ImageMode::Referenced, Some(uri)) => {
                    if let Some(path) = self.reference_image(uri) {
                        res_parts.push(format!("<img src=\"{}\"{alt}>", img_src(&path)));
                    } else if !uri.starts_with("data:") {
                        res_parts.push(format!("<img src=\"{}\"{alt}>", img_src(uri)));
                    }
                }
                _ => {}
            }
        }
        let meta = self.meta(item);
        if !meta.is_empty() {
            res_parts.push(meta);
        }
        let text = res_parts.concat();
        if text.is_empty() {
            String::new()
        } else {
            format!("<figure>{text}</figure>")
        }
    }

    /// Referenced mode: a `data:` URI becomes an artifact file named like the
    /// Markdown export's, and the path is what the page references.
    fn reference_image(&mut self, uri: &str) -> Option<String> {
        let rest = uri.strip_prefix("data:")?;
        let (mime, payload) = rest.split_once(";base64,")?;
        let bytes = crate::base64::decode(payload)?;
        let path = format!(
            "{}/image_{:06}.{}",
            self.artifacts_dir,
            self.pic_index,
            crate::markdown::ext_for(mime)
        );
        self.pic_index += 1;
        self.artifacts.push((path.clone(), bytes));
        Some(path)
    }

    /// `HTMLMetaSerializer`: the item's `meta` fields in docling's model
    /// order (`BaseMeta`, `FloatingMeta`, `PictureMeta`, then extras).
    /// A picture's alt text (#676): its `meta.description` when that was
    /// `created_by` the alt text, not an OCR or captioning model.
    fn alt_text(item: &'a Value) -> Option<&'a str> {
        let desc = item.get("meta")?.get("description")?;
        (desc.get("created_by").and_then(Value::as_str)
            == Some(crate::PictureDescription::ALT_TEXT))
        .then(|| desc.get("text").and_then(Value::as_str))
        .flatten()
        .filter(|t| !t.is_empty())
    }

    fn meta(&mut self, item: &'a Value) -> String {
        let Some(meta) = item.get("meta").and_then(Value::as_object) else {
            return String::new();
        };
        if self.excluded(item) {
            return String::new();
        }
        // Alt text (#676) renders on request only.
        let hide_alt = !self.alt_text_meta && Self::alt_text(item).is_some();
        const ORDER: [&str; 10] = [
            "summary",
            "language",
            "entities",
            "keywords",
            "topics",
            "description",
            "classification",
            "molecule",
            "tabular_chart",
            "code",
        ];
        let mut names: Vec<&str> = ORDER
            .iter()
            .copied()
            .filter(|k| meta.contains_key(*k) && !(hide_alt && *k == "description"))
            .collect();
        names.extend(
            meta.keys()
                .map(String::as_str)
                .filter(|k| !ORDER.contains(k)),
        );
        let mut fields = String::new();
        for name in names {
            let value = &meta[name];
            let (txt, markup) = match name {
                "summary" | "description" => (
                    value
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    false,
                ),
                "language" => (
                    value
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    false,
                ),
                "keywords" | "topics" => (
                    value
                        .get("values")
                        .and_then(Value::as_array)
                        .map(|v| {
                            v.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    false,
                ),
                "entities" => (
                    value
                        .get("mentions")
                        .and_then(Value::as_array)
                        .map(|v| {
                            v.iter()
                                .map(|m| {
                                    let text = m.get("text").and_then(Value::as_str).unwrap_or("");
                                    let label = m.get("label").and_then(Value::as_str);
                                    let span =
                                        m.get("charspan").and_then(Value::as_array).and_then(|s| {
                                            Some((s.first()?.as_u64()?, s.get(1)?.as_u64()?))
                                        });
                                    match (label, span) {
                                        (Some(l), Some((a, b))) => {
                                            format!("{text} ({l}, [{a},{b}])")
                                        }
                                        (Some(l), None) => format!("{text} ({l})"),
                                        (None, Some((a, b))) => format!("{text} ([{a},{b}])"),
                                        (None, None) => text.to_string(),
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default(),
                    false,
                ),
                "classification" => {
                    let preds = value.get("predictions").and_then(Value::as_array);
                    let main = preds.and_then(|p| {
                        let mut best: Option<(&Value, f64)> = None;
                        for pred in p {
                            if let Some(c) = pred.get("confidence").and_then(Value::as_f64) {
                                if best.is_none_or(|(_, b)| c > b) {
                                    best = Some((pred, c));
                                }
                            }
                        }
                        best.map(|(v, _)| v).or_else(|| p.first())
                    });
                    (
                        humanize(
                            main.and_then(|m| m.get("class_name"))
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        ),
                        false,
                    )
                }
                "molecule" => (
                    value
                        .get("smi")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    false,
                ),
                "tabular_chart" => {
                    let table = value
                        .get("chart_data")
                        .map(|d| self.table_body(d))
                        .unwrap_or_default();
                    if table.is_empty() {
                        continue;
                    }
                    (format!("<table><tbody>{table}</tbody></table>"), true)
                }
                "code" => {
                    let lang = value
                        .get("language")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_lowercase();
                    let class = if lang.is_empty() {
                        String::new()
                    } else {
                        format!(" class=\"language-{}\"", escape(&lang, true))
                    };
                    (
                        format!(
                            "<pre class=\"docling-meta-code\"><code{class}>{}</code></pre>",
                            escape(
                                value.get("text").and_then(Value::as_str).unwrap_or(""),
                                true
                            )
                        ),
                        true,
                    )
                }
                _ => {
                    let s = match value {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    if s.is_empty() {
                        continue;
                    }
                    (s, false)
                }
            };
            let txt = if markup { txt } else { escape(&txt, false) };
            let name_esc = escape(name, true);
            fields.push_str(&format!(
                "<div class=\"docling-meta-field\" data-meta-name=\"{name_esc}\">\
                 <span class=\"docling-meta-field-label\">{name_esc}:</span> \
                 <span class=\"docling-meta-field-value\">{txt}</span></div>"
            ));
        }
        if fields.is_empty() {
            String::new()
        } else {
            format!("<details class=\"docling-meta\"><summary>Meta</summary>{fields}</details>")
        }
    }

    /// `HTMLKeyValueSerializer` / `HTMLFormSerializer` over the item's
    /// `graph` (`_HTMLGraphDataSerializer`), then its captions.
    fn graph_item(&mut self, item: &'a Value, class: &str) -> String {
        let mut res_parts: Vec<String> = Vec::new();
        if !self.excluded(item) {
            if let Some(graph) = item.get("graph") {
                let g = Self::graph(graph, class);
                if !g.is_empty() {
                    res_parts.push(g);
                }
            }
        }
        let cap = self.captions(item, "figcaption");
        if !cap.is_empty() {
            res_parts.push(cap);
        }
        res_parts.join("\n")
    }

    fn graph(graph: &Value, class: &str) -> String {
        use std::collections::{BTreeMap, HashMap, HashSet};
        let cells: HashMap<u64, &Value> = graph
            .get("cells")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| Some((c.get("cell_id")?.as_u64()?, c)))
            .collect();
        let cell_order: Vec<u64> = graph
            .get("cells")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("cell_id")?.as_u64())
            .collect();
        let mut child_links: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut value_links: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut value_order: Vec<u64> = Vec::new();
        let mut parents: HashSet<u64> = HashSet::new();
        for link in graph
            .get("links")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(s), Some(t)) = (
                link.get("source_cell_id").and_then(Value::as_u64),
                link.get("target_cell_id").and_then(Value::as_u64),
            ) else {
                continue;
            };
            if !cells.contains_key(&s) || !cells.contains_key(&t) {
                continue;
            }
            match link.get("label").and_then(Value::as_str) {
                Some("to_child") => {
                    child_links.entry(s).or_default().push(t);
                    parents.insert(t);
                }
                Some("to_value") => {
                    if !value_links.contains_key(&s) {
                        value_order.push(s);
                    }
                    value_links.entry(s).or_default().push(t);
                }
                _ => {}
            }
        }
        let cell_text = |id: u64| {
            escape(
                cells[&id].get("text").and_then(Value::as_str).unwrap_or(""),
                true,
            )
        };
        let roots: Vec<u64> = cell_order
            .iter()
            .copied()
            .filter(|id| !parents.contains(id))
            .collect();
        // `to_child` links form a tree (or a DAG, whose shared cells upstream
        // renders once per parent) in a well-formed graph, but an XBRL
        // instance's key-value graph can close a cycle — a cell that is its
        // grandchild's child — and upstream's recursion then never returns.
        // The cells on the current descent are tracked; a link back onto one
        // of them contributes nothing.
        let mut path: HashSet<u64> = HashSet::new();
        let mut parts = vec![format!("<div class=\"{class}\">")];
        if !roots.is_empty() {
            parts.push(format!("<ul class=\"{class}\">"));
            fn render(
                id: u64,
                cell_text: &dyn Fn(u64) -> String,
                child_links: &HashMap<u64, Vec<u64>>,
                value_links: &BTreeMap<u64, Vec<u64>>,
                cells: &HashMap<u64, &Value>,
                path: &mut HashSet<u64>,
            ) -> String {
                if !path.insert(id) {
                    return String::new();
                }
                let mut text = cell_text(id);
                if let Some(values) = value_links.get(&id) {
                    let vals: Vec<String> = values
                        .iter()
                        .filter(|v| cells.contains_key(v))
                        .map(|&v| cell_text(v))
                        .collect();
                    text = format!("<strong>{text}</strong>: {}", vals.join(", "));
                }
                let out = match child_links.get(&id) {
                    Some(children) if !children.is_empty() => {
                        let mut out = vec![format!("<li>{text}</li>"), "<ul>".to_string()];
                        for &c in children {
                            out.push(render(c, cell_text, child_links, value_links, cells, path));
                        }
                        out.push("</ul>".to_string());
                        out.join("\n")
                    }
                    _ if value_links.contains_key(&id) => format!("<li>{text}</li>"),
                    _ => String::new(),
                };
                path.remove(&id);
                out
            }
            for root in roots {
                parts.push(render(
                    root,
                    &cell_text,
                    &child_links,
                    &value_links,
                    &cells,
                    &mut path,
                ));
            }
            parts.push("</ul>".to_string());
        } else {
            parts.push(format!("<dl class=\"{class}\">"));
            for key in value_order {
                parts.push(format!("<dt>{}</dt>", cell_text(key)));
                for &v in &value_links[&key] {
                    parts.push(format!("<dd>{}</dd>", cell_text(v)));
                }
            }
            parts.push("</dl>".to_string());
        }
        parts.push("</div>".to_string());
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_matches_python_html_escape() {
        assert_eq!(
            escape("a < b & c > d \"q\" 'x'", false),
            "a &lt; b &amp; c &gt; d \"q\" 'x'"
        );
        assert_eq!(escape("\"q\" 'x'", true), "&quot;q&quot; &#x27;x&#x27;");
    }

    #[test]
    fn text_direction_follows_the_first_letter_or_the_majority() {
        assert_eq!(text_direction(""), "ltr");
        assert_eq!(text_direction("hello"), "ltr");
        assert_eq!(text_direction("שלום world"), "rtl");
        assert_eq!(text_direction("1 مرحبا بالعالم"), "rtl");
        assert_eq!(text_direction("abc ש"), "ltr");
    }

    /// #781 (docling-core#772): a referenced `src` is encoded like the
    /// Markdown destination, then HTML-escaped — `&`, `,`, `+` and a URL's
    /// `:` stay, a `%` escape is not encoded twice.
    #[test]
    fn img_src_encodes_like_markdown() {
        assert_eq!(
            img_src("Q&A, v2+final_artifacts/image_000000.png"),
            "Q&amp;A,%20v2+final_artifacts/image_000000.png"
        );
        assert_eq!(
            img_src("https://e.x/a b.png?x=1&y=2"),
            "https://e.x/a%20b.png?x=1&amp;y=2"
        );
        assert_eq!(img_src("dir/a%20b.png"), "dir/a%20b.png");
        assert_eq!(img_src("ü.png"), "%C3%BC.png");
        assert_eq!(img_src("it's \"q\".png"), "it&#x27;s%20%22q%22.png");
    }

    #[test]
    fn humanize_is_pythons_capitalize_after_underscores() {
        assert_eq!(humanize("bar_chart"), "Bar chart");
        assert_eq!(humanize("LINE_CHART"), "Line chart");
        assert_eq!(humanize(""), "");
    }

    /// A document with no items still renders the full skeleton.
    #[test]
    fn empty_document_skeleton() {
        let doc = crate::DoclingDocument::new("empty");
        let (html, artifacts) = to_html(&doc, &HtmlExportOptions::default());
        assert!(artifacts.is_empty());
        assert!(html.starts_with(
            "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"UTF-8\"/>\n<title>empty</title>\n"
        ));
        assert!(html.ends_with("<body>\n<div class='page'>\n\n</div>\n</body>\n</html>"));
        assert_eq!(html, doc.export_to_html());
    }

    /// A body paragraph, a furniture-layer running header (a PDF
    /// `page_header`, body-parented like docling writes it) and a reviewer
    /// comment (notes layer): the default export is body only, each extra
    /// layer adds its items through the ordinary text serializer, and `NONE`
    /// renders nothing — the way docling-core reads `HTMLParams.layers`
    /// (#499).
    #[test]
    fn layers_select_what_renders() {
        use crate::document::{ContentLayer, ContentLayers, Node};
        let mut doc = crate::DoclingDocument::new("layers");
        doc.push(Node::PageFurniture {
            footer: false,
            location: [0, 0, 0, 0],
            text: "Running header".into(),
        });
        doc.push(Node::Paragraph {
            text: "Body text".into(),
        });
        doc.push(Node::CommentSection {
            name: "comment-1".into(),
            text: "A reviewer note".into(),
            refs_note_text: false,
            grouped: true,
        });
        let page = |html: &str| {
            let start = html.find("<div class='page'>\n").unwrap() + "<div class='page'>\n".len();
            let end = html.rfind("\n</div>\n</body>").unwrap();
            html[start..end].to_string()
        };

        let body_only = doc.export_to_html();
        assert_eq!(page(&body_only), "<p>Body text</p>");
        assert_eq!(
            body_only,
            doc.export_to_html_with_layers(ContentLayers::BODY),
            "the default export is the body-only set"
        );
        assert_eq!(
            body_only,
            doc.export_to_html_with(&HtmlExportOptions::default()).0
        );

        let with_furniture =
            doc.export_to_html_with_layers(ContentLayers::BODY.with(ContentLayer::Furniture));
        assert_eq!(
            page(&with_furniture),
            "<p>Running header</p>\n<p>Body text</p>"
        );
        let with_notes =
            doc.export_to_html_with_layers(ContentLayers::BODY.with(ContentLayer::Notes));
        assert_eq!(
            page(&with_notes),
            "<p>Body text</p>\n<p>A reviewer note</p>"
        );
        assert_eq!(
            page(&doc.export_to_html_with_layers(ContentLayers::ALL)),
            "<p>Running header</p>\n<p>Body text</p>\n<p>A reviewer note</p>"
        );
        assert_eq!(
            page(
                &doc.export_to_html_with_layers(ContentLayers::NONE.with(ContentLayer::Furniture))
            ),
            "<p>Running header</p>",
            "a set without `body` drops the main content, as upstream's does"
        );
        assert_eq!(
            page(&doc.export_to_html_with_layers(ContentLayers::NONE)),
            ""
        );
    }

    /// docling-core#894: code, formula source and list markers are escaped
    /// for the context they land in — `test_html_code_item_text_is_escaped`,
    /// `test_html_mathml_annotation_is_escaped`,
    /// `test_html_mathml_keeps_symbol_character_references`,
    /// `test_html_formula_mathml_failure_falls_back_to_escaped_pre` and
    /// `test_html_list_item_marker_cannot_break_out_of_style_attribute`.
    #[test]
    fn code_formula_and_markers_are_escaped() {
        use crate::document::Node;
        let body = |nodes: Vec<Node>| {
            let mut doc = crate::DoclingDocument::new("escape");
            for n in nodes {
                doc.push(n);
            }
            let html = doc.export_to_html();
            let start = html.find("<div class='page'>\n").unwrap() + "<div class='page'>\n".len();
            let end = html.rfind("\n</div>\n</body>").unwrap();
            html[start..end].to_string()
        };
        let code = |text: &str| Node::Code {
            language: None,
            text: text.into(),
            orig: None,
            pretty: None,
        };
        let formula = |latex: &str| Node::Formula {
            latex: latex.into(),
            orig: latex.into(),
            location: None,
        };

        assert_eq!(
            body(vec![code("conda create -n <CONDA_NAME>\nDict<str> x = \"<div>\";")]),
            "<pre><code>conda create -n &lt;CONDA_NAME&gt;\nDict&lt;str&gt; x = \"&lt;div&gt;\";</code></pre>"
        );
        assert_eq!(
            // A node's text keeps `&` entity-escaped (the JSON item's text
            // is `print('&amp; stays literal')`).
            body(vec![code(
                "if a && b:\n    print('&amp;amp; stays literal')"
            )]),
            "<pre><code>if a &amp;&amp; b:\n    print('&amp;amp; stays literal')</code></pre>"
        );

        for (latex, annotation) in [
            ("a<b && c>d", "a&lt;b &amp;&amp; c&gt;d"),
            (
                r"\text{<svg/onload=alert(1)>}",
                r"\text{&lt;svg/onload=alert(1)&gt;}",
            ),
        ] {
            let html = body(vec![formula(latex)]);
            assert!(!html.contains("<svg") && !html.contains("<img"), "{html}");
            assert!(
                html.contains(&format!(
                    "<annotation encoding=\"TeX\">{annotation}</annotation></math>"
                )),
                "{html}"
            );
        }
        let html = body(vec![formula(r"\alpha \leq \beta")]);
        assert!(
            html.contains("<mi>&#x003B1;</mi><mo>&#x02264;</mo><mi>&#x003B2;</mi>"),
            "{html}"
        );
        assert!(!html.contains("&amp;#x"), "{html}");
        // LaTeX latex2mathml rejects falls back to an escaped `<pre>`.
        assert_eq!(
            body(vec![formula(r"\left( a<b & c")]),
            "<pre>\\left( a&lt;b &amp; c</pre>"
        );

        // The flat node's marker is DocLang's; the JSON item's (what the
        // `<li style>` reads) goes through these two.
        for (marker, style) in [
            (
                "x\" onmouseover=\"alert(1)\" y=\"",
                "list-style-type: 'x&quot; onmouseover=&quot;alert(1)&quot; y=&quot; ';",
            ),
            ("it's", r"list-style-type: 'it\'s ';"),
            (
                "'; color: red; content: '",
                r"list-style-type: '\'; color: red; content: \' ';",
            ),
        ] {
            assert_eq!(
                escape_attr(&format!(
                    "list-style-type: '{} ';",
                    escape_css_string(marker)
                )),
                style
            );
        }
    }

    #[test]
    fn content_layers_parse_list() {
        use crate::document::{ContentLayer, ContentLayers};
        assert_eq!(ContentLayers::parse_list("body"), Ok(ContentLayers::BODY));
        assert_eq!(
            ContentLayers::parse_list(" body , furniture,notes "),
            Ok(ContentLayers::BODY
                .with(ContentLayer::Furniture)
                .with(ContentLayer::Notes))
        );
        assert_eq!(ContentLayers::parse_list("all"), Ok(ContentLayers::ALL));
        assert_eq!(ContentLayers::parse_list(""), Ok(ContentLayers::NONE));
        assert!(ContentLayers::parse_list("body,header")
            .unwrap_err()
            .contains("header"));
        assert!(ContentLayers::default().contains(None));
        assert!(!ContentLayers::default().contains(Some(ContentLayer::Notes)));
        assert!(ContentLayers::ALL.contains_name("invisible"));
        assert!(!ContentLayers::ALL.contains_name("other"));
    }
}
