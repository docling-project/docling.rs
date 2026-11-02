//! Pandoc AST output (`--to pandoc`, #515): the document as the JSON
//! serialization of Pandoc's `Pandoc` type (`pandoc -f json`), which hands
//! docling.rs's parsing to every Pandoc writer — DOCX, ODT, EPUB, RST, Org,
//! Typst, AsciiDoc, … — through `docling-rs in.pdf --to pandoc | pandoc -f
//! json -t docx -o out.docx`.
//!
//! Like [`crate::html`] and [`crate::latex`] this walks the *JSON* document
//! model ([`export_to_json_value`](crate::DoclingDocument::export_to_json_value))
//! rather than the flat [`Node`](crate::Node) stream: the JSON export already
//! carries docling's item structure for every backend — the backend-built
//! item tree where one exists (HTML, DOCX: heading nesting, inline groups of
//! formatted runs, rich table cells), docling's generic grouping rules
//! otherwise — so the AST inherits those decisions instead of re-deriving
//! them. The walk is the HTML serializer's (`_iterate_items` with groups,
//! pictures not traversed, captions rendered by the item that owns them,
//! content layers filtered with their children still walked); only the
//! node it emits differs.
//!
//! Mapping (Pandoc constructor ← docling item):
//!
//! | docling | Pandoc |
//! |---|---|
//! | `title` | `Header 1` |
//! | `section_header` (level *n*) | `Header (n+1)`, capped at 6 — docling's title-reserving depth; under [`HeadingDepth::Source`] (#848) `Header n`, the depth the DOCX / ODT author wrote |
//! | `text`, `paragraph` | `Para` (`Plain` inside lists and table cells) |
//! | `inline` group | the runs' inlines joined by `Space` |
//! | formatting / hyperlink | `Strong`, `Emph`, `Underline`, `Strikeout`, `Subscript`, `Superscript`, then `Link` around it all — an internal link's target is `#id` (#849); a Markdown `[label](target)` inside an item's text, the flat node chain's hyperlink (`.doc`, AbiWord, …), is a `Link` too (#852) |
//! | a numbered heading (#850: a DOCX heading numbered through `w:numPr`, docling's `1.2 Title` text with `_number`) | the `Header` holds the authored text alone, `Title` — Pandoc numbers sections itself (`--number-sections`), and its docx reader writes no number, so a numbered AST would show `1.2 1.2 Title`; the Markdown and JSON keep docling's numbered text |
//! | a link destination (#849: a DOCX bookmark, `_anchors`) | on a heading, its `Header` identifier (Pandoc's own auto-identifier of the heading text, `target-section`, so `#target-section` resolves as it does from `pandoc -f docx`); anywhere else an empty `Span` with the identifier and the class `anchor`, Pandoc's docx reader's shape, at the start of the item's inlines |
//! | `list` group | `OrderedList` (first item enumerated; start, number style and delimiter from the items' markers — `a)` LowerAlpha OneParen, `iii.` LowerRoman from 3, `(3)` Decimal TwoParens, #835) / `BulletList`, nested lists inside their item |
//! | `code` | `CodeBlock` with the language as its class (`Code` inline) |
//! | `formula` | `Para [Math DisplayMath]` (`Math InlineMath` inline) |
//! | `checkbox_selected` / `_unselected` | the text after `☒` / `☐` — Pandoc's own task-list convention |
//! | `table` | `Table`: leading all-header rows as `TableHead`, `rowspan` / `colspan`, rich cells as their blocks, captions as the caption |
//! | `picture` | `Para [Image]` (Pandoc's readers' shape), or a `Figure` holding it when there is a caption or a tabular chart's data `Table`; the `Image`'s alt text is the picture's description — the alt text a DOCX / PPTX / XLSX / ODT author typed (#834) — else its caption; its `width` / `height` attributes the size the document displays it at; the target per [`ImageMode`] |
//! | footnotes of a table / picture | `Note` at the end of its caption (the float is the call site) |
//! | key-value / form graph | `Div .key-value-region` / `.form-container` holding a `DefinitionList` (or the nested `BulletList` of a hierarchical graph) |
//! | form `field_region` | `Div .field-region` holding a `DefinitionList`: `marker` + `field_key` as the term, each `field_value` a definition |
//! | any other text label (`caption` without an owner, `footnote` without one, `reference`, `handwritten_text`, `page_header`, …) | `Div .docling-<label> [Para]` |
//! | the document's metadata (#836) | `meta`: `title`, `subject`, `description`, `date` (the document's own, else its creation date), `publisher`, `rights`, `identifier` as `MetaInlines`, `author` and `keywords` as `MetaList`s of `MetaInlines`, `lang` as `MetaString` |
//! | a tracked change kept under `track_changes=all` (#851: a DOCX `w:ins` / `w:del` / `w:moveTo` / `w:moveFrom` run, `_revision`) | `Span .insertion` / `Span .deletion` with `author` and `date` around the run's inlines — Pandoc's docx reader's `--track-changes=all` shape; under an explicit `accept` / `reject` the document holds that view's text alone and the comments below are left out, as Pandoc leaves them |
//! | a reviewer comment on an item (#836) | Pandoc's docx reader's `--track-changes=all` shape: a `Span .comment-start` with `comment-id`, `author` and `date` holding the comment's text before the item's inlines, and an empty `Span .comment-end` with the `comment-id` after them; a thread's replies follow as further pairs |
//!
//! No Pandoc equivalent, so not written: provenance (pages, bounding boxes),
//! confidence and classification meta, form field geometry; furniture
//! (headers/footers) is off unless its [`ContentLayers`] are asked for,
//! exactly as in the HTML export, and the notes layer's comment sections
//! are written as the spans above, never as blocks. Images
//! are targets, not bytes, in Pandoc: the default [`ImageMode::Embedded`]
//! writes `data:` URIs, so the AST is self-contained and `pandoc -f json -t
//! docx` embeds every picture (#537); [`ImageMode::Referenced`] links files
//! under the artifacts directory (relative paths, resolved from where
//! `pandoc` runs). A picture without a target — [`ImageMode::Placeholder`],
//! or one whose payload docling cannot decode (EMF/WMF) — is still an
//! `Image`, classed `docling-placeholder` with an empty target: it survives
//! into every writer (which reports the missing resource and prints the alt
//! text) instead of vanishing as raw HTML.
//!
//! Version: the output is stamped [`PANDOC_API_VERSION`] — the latest
//! `pandoc-types` (Pandoc 3.x). Pandoc accepts a document whose
//! `pandoc-api-version` agrees on the first two components; a caller asking
//! for another API ([`PandocExportOptions::api_version`]) gets
//! [`PandocError::UnsupportedApiVersion`] rather than a document Pandoc would
//! reject. Every node is built through the [`ast`] constructors, the one
//! place a future API change touches.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use crate::document::{ContentLayers, DoclingDocument};
use crate::json::{TableData, TableView};
use crate::markdown::ImageMode;
use crate::TrackChanges;

/// The `pandoc-api-version` written — `pandoc-types` 1.23.1.1 (Pandoc 3.x).
pub const PANDOC_API_VERSION: [u32; 4] = [1, 23, 1, 1];

/// A Pandoc export: the JSON AST, and — for [`ImageMode::Referenced`] — the
/// image files it links to, as `(path under artifacts_dir, bytes)`.
pub type PandocOutput = (String, Vec<(String, Vec<u8>)>);

/// How a `section_header`'s `level` becomes a Pandoc `Header` depth (#848).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HeadingDepth {
    /// docling's convention, the default: `Header 1` is the document title
    /// and a `section_header` of level *n* is `Header (n+1)` (capped at 6),
    /// the depth docling's Markdown and HTML serializers give it (`##` for
    /// level 1). The flat `Node::Heading` round trip relies on it — a
    /// level-2 heading is stored as a level-1 `section_header` and comes
    /// back as `Header 2` — as do consumers of docling's JSON, so it is not
    /// changed under them.
    #[default]
    Docling,
    /// The source's own depth: a `section_header` of level *n* is `Header n`
    /// (clamped to 1–6; a missing or zero level is 1) and the `title` stays
    /// `Header 1` — what Pandoc's own docx / odt readers give (a DOCX
    /// `Heading 1` is `Header 1`), so `--number-sections`, TOC depths and
    /// `--shift-heading-level-by` behave as on `pandoc -f docx`. A document
    /// whose backend reserves level 1 for the title (HTML, Markdown: an
    /// `<h2>` is a level-1 `section_header`) comes out one depth shallower
    /// under it.
    Source,
}

impl HeadingDepth {
    /// The spellings every surface accepts (`docling` = the default).
    pub const NAMES: [&'static str; 2] = ["docling", "source"];

    pub fn as_str(self) -> &'static str {
        match self {
            HeadingDepth::Docling => "docling",
            HeadingDepth::Source => "source",
        }
    }
}

impl std::str::FromStr for HeadingDepth {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "docling" | "" => Ok(HeadingDepth::Docling),
            "source" => Ok(HeadingDepth::Source),
            other => Err(format!(
                "unknown heading depth {other:?} (expected docling or source)"
            )),
        }
    }
}

/// Options for [`to_pandoc`].
#[derive(Debug, Clone)]
pub struct PandocExportOptions {
    /// How pictures carry their image (see the module docs); `Embedded` by
    /// default, so the AST alone rebuilds a document with its pictures.
    pub image_mode: ImageMode,
    /// The directory referenced images are written under
    /// (`<artifacts_dir>/image_NNNNNN.<ext>`, the Markdown export's names).
    pub artifacts_dir: String,
    /// The content layers written; body only by default.
    pub layers: ContentLayers,
    /// The Pandoc API version the caller needs (`"1.23"`, `"1.23.1.1"`, …);
    /// `None` = [`PANDOC_API_VERSION`]. Only that API is supported.
    pub api_version: Option<String>,
    /// The `Header` depth of a `section_header` (#848): docling's
    /// title-reserving `n + 1` by default, or the source's own `n`.
    pub heading_depth: HeadingDepth,
    /// The tracked-changes policy the document was read under (#851).
    /// `None` (no policy asked for) and `Some(All)` write the reviewer
    /// comments as `Span .comment-start` / `.comment-end` pairs; an
    /// explicit `Accept` / `Reject` leaves them out, as Pandoc's
    /// `--track-changes=accept|reject` do.
    pub track_changes: Option<TrackChanges>,
}

impl Default for PandocExportOptions {
    fn default() -> Self {
        Self {
            image_mode: ImageMode::Embedded,
            artifacts_dir: "artifacts".to_string(),
            layers: ContentLayers::BODY,
            api_version: None,
            heading_depth: HeadingDepth::Docling,
            track_changes: None,
        }
    }
}

/// Why a Pandoc export was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PandocError {
    /// The requested `pandoc-api-version` is not the one supported.
    UnsupportedApiVersion { requested: String },
}

impl std::fmt::Display for PandocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PandocError::UnsupportedApiVersion { requested } => write!(
                f,
                "unsupported Pandoc API version '{requested}': only {} (pandoc-types {}, Pandoc 3.x) is supported",
                version_string(&PANDOC_API_VERSION[..2]),
                version_string(&PANDOC_API_VERSION),
            ),
        }
    }
}

impl std::error::Error for PandocError {}

fn version_string(parts: &[u32]) -> String {
    parts
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// Check a requested API version: Pandoc reads a document whose version
/// shares its first two components (major API, minor API), so `1.23`,
/// `1.23.1` and `1.23.1.1` are all this export's; anything else is refused.
pub fn check_api_version(requested: &str) -> Result<(), PandocError> {
    let err = || PandocError::UnsupportedApiVersion {
        requested: requested.to_string(),
    };
    let parts: Vec<u32> = requested
        .trim()
        .split('.')
        .map(|p| p.parse::<u32>().map_err(|_| err()))
        .collect::<Result<_, _>>()?;
    if parts.len() < 2 || parts[..2] != PANDOC_API_VERSION[..2] {
        return Err(err());
    }
    if parts.len() > PANDOC_API_VERSION.len()
        || parts
            .iter()
            .zip(PANDOC_API_VERSION.iter())
            .skip(2)
            .any(|(a, b)| a > b)
    {
        // A newer patch than this export knows.
        return Err(err());
    }
    Ok(())
}

/// The document as Pandoc JSON (compact, one line, like `pandoc -t json`),
/// and for [`ImageMode::Referenced`] the `(path, bytes)` image files to
/// write.
pub fn to_pandoc(
    doc: &DoclingDocument,
    options: &PandocExportOptions,
) -> Result<PandocOutput, PandocError> {
    // The JSON with the note calls docling's model has no field for (#538),
    // its tables typed (#875).
    let (json, tables) = crate::json::to_json_with_notes(doc);
    render_json(&json, &tables, options)
}

/// [`to_pandoc`] for a document already in docling's JSON wire format —
/// e.g. one produced by Python docling, or `DoclingDocument.export_to_dict()`
/// in the Python bindings.
pub fn from_docling_json(
    json: &Value,
    options: &PandocExportOptions,
) -> Result<PandocOutput, PandocError> {
    render_json(json, &[], options)
}

/// [`from_docling_json`] with the typed tables the JSON's table items name
/// ([`crate::json::to_json_typed_tables`]).
fn render_json(
    json: &Value,
    tables: &[TableData],
    options: &PandocExportOptions,
) -> Result<PandocOutput, PandocError> {
    if let Some(v) = &options.api_version {
        check_api_version(v)?;
    }
    // The walk recurses per nesting level; an XBRL instance nests thousands
    // deep, so it runs on a big-stack thread like the HTML serializer.
    let render = || {
        let mut ser = Serializer::new(json, options);
        ser.tables = tables;
        let blocks = ser.body();
        let doc = ast::document_with_meta(meta_of(json), blocks);
        (
            serde_json::to_string(&doc).expect("Pandoc JSON is always serializable"),
            ser.artifacts,
        )
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        Ok(std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("docling-pandoc".into())
                .stack_size(256 << 20)
                .spawn_scoped(scope, render)
                .expect("spawn the pandoc serializer thread")
                .join()
                .expect("pandoc serializer thread panicked")
        }))
    }
    #[cfg(target_arch = "wasm32")]
    {
        Ok(render())
    }
}

/// Pandoc's auto-identifier of a heading (`Text.Pandoc.Shared.
/// stringToIdentifier`, the default `auto_identifiers` rule, #849): the
/// text lower-cased, everything but letters, digits, `_`, `-`, `.` and
/// spaces removed, the words joined by `-`, anything up to the first letter
/// dropped; `section` when nothing is left. `Target section` →
/// `target-section`, `2026 Results` → `results`.
pub fn auto_identifier(text: &str) -> String {
    let kept: String = text
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace() || matches!(c, '_' | '-' | '.'))
        .flat_map(char::to_lowercase)
        .collect();
    let joined = kept.split_whitespace().collect::<Vec<_>>().join("-");
    let id = joined.trim_start_matches(|c: char| !c.is_alphabetic());
    if id.is_empty() {
        "section".to_string()
    } else {
        id.to_string()
    }
}

/// The Pandoc AST constructors (`Text.Pandoc.Definition`, JSON per
/// `Text.Pandoc.JSON`): every node this module writes is built here.
///
/// Every constructor moves the nodes it is given into the new one:
/// `json!` would deep-copy each interpolated `Value` (`to_value`), so a
/// table cell's content was copied again by its row, its table and the
/// document — on a 640,000-cell table most of the export's time and
/// twice its AST in peak memory (#875). Literal-only fragments still use
/// `json!`.
pub mod ast {
    use serde_json::{json, Value};

    use super::PANDOC_API_VERSION;

    /// A JSON array of the given nodes, moved in.
    fn arr<const N: usize>(items: [Value; N]) -> Value {
        Value::Array(items.into())
    }

    pub fn document(blocks: Vec<Value>) -> Value {
        document_with_meta(json!({}), blocks)
    }
    /// A document with its `Meta` (#836).
    pub fn document_with_meta(meta: Value, blocks: Vec<Value>) -> Value {
        let mut m = serde_json::Map::with_capacity(3);
        m.insert("pandoc-api-version".into(), json!(PANDOC_API_VERSION));
        m.insert("meta".into(), meta);
        m.insert("blocks".into(), Value::Array(blocks));
        Value::Object(m)
    }
    pub fn meta_inlines(inlines: Vec<Value>) -> Value {
        node("MetaInlines", Value::Array(inlines))
    }
    pub fn meta_string(s: &str) -> Value {
        node("MetaString", json!(s))
    }
    pub fn meta_list(items: Vec<Value>) -> Value {
        node("MetaList", Value::Array(items))
    }
    /// `Span Attr [Inline]` with key-value attributes.
    pub fn span(classes: &[&str], attrs: &[(String, String)], inlines: Vec<Value>) -> Value {
        let kv: Vec<Value> = attrs.iter().map(|(k, v)| json!([k, v])).collect();
        node(
            "Span",
            arr([json!(["", classes, kv]), Value::Array(inlines)]),
        )
    }

    /// `Attr`: identifier, classes, key-value pairs.
    pub fn attr(classes: &[&str]) -> Value {
        json!(["", classes, []])
    }

    fn node(t: &str, c: Value) -> Value {
        let mut m = serde_json::Map::with_capacity(2);
        m.insert("t".into(), Value::String(t.into()));
        m.insert("c".into(), c);
        Value::Object(m)
    }

    // --- blocks -------------------------------------------------------------

    pub fn para(inlines: Vec<Value>) -> Value {
        node("Para", Value::Array(inlines))
    }
    pub fn plain(inlines: Vec<Value>) -> Value {
        node("Plain", Value::Array(inlines))
    }
    pub fn header(level: usize, inlines: Vec<Value>) -> Value {
        node(
            "Header",
            arr([level.into(), attr(&[]), Value::Array(inlines)]),
        )
    }
    /// A `Header` with an identifier (#849: a bookmarked heading).
    pub fn header_with_id(level: usize, id: &str, inlines: Vec<Value>) -> Value {
        node(
            "Header",
            arr([level.into(), json!([id, [], []]), Value::Array(inlines)]),
        )
    }
    /// An empty `Span` carrying an identifier and the class `anchor` — the
    /// link destination Pandoc's docx reader writes for a bookmark (#849).
    pub fn anchor(id: &str) -> Value {
        node("Span", json!([[id, ["anchor"], []], []]))
    }
    pub fn code_block(language: Option<&str>, text: &str) -> Value {
        let classes: Vec<&str> = language.into_iter().collect();
        node("CodeBlock", arr([attr(&classes), text.into()]))
    }
    pub fn bullet_list(items: Vec<Vec<Value>>) -> Value {
        node(
            "BulletList",
            Value::Array(items.into_iter().map(Value::Array).collect()),
        )
    }
    /// `OrderedList ListAttributes [[Block]]`: `style` a `ListNumberStyle`
    /// (`Decimal`, `LowerAlpha`, `UpperRoman`, …), `delim` a
    /// `ListNumberDelim` (`Period`, `OneParen`, `TwoParens`, `DefaultDelim`).
    pub fn ordered_list(start: u64, style: &str, delim: &str, items: Vec<Vec<Value>>) -> Value {
        node(
            "OrderedList",
            arr([
                json!([start, { "t": style }, { "t": delim }]),
                Value::Array(items.into_iter().map(Value::Array).collect()),
            ]),
        )
    }
    pub fn definition_list(entries: Vec<(Vec<Value>, Vec<Vec<Value>>)>) -> Value {
        node(
            "DefinitionList",
            Value::Array(
                entries
                    .into_iter()
                    .map(|(term, defs)| {
                        arr([
                            Value::Array(term),
                            Value::Array(defs.into_iter().map(Value::Array).collect()),
                        ])
                    })
                    .collect(),
            ),
        )
    }
    /// `RawBlock Format Text` — kept by writers of that format, dropped by
    /// the others.
    pub fn raw_block(format: &str, text: &str) -> Value {
        node("RawBlock", json!([format, text]))
    }
    pub fn div(classes: &[&str], blocks: Vec<Value>) -> Value {
        node("Div", arr([attr(classes), Value::Array(blocks)]))
    }
    /// `Caption`: no short caption, the long one as blocks.
    pub fn caption(blocks: Vec<Value>) -> Value {
        arr([Value::Null, Value::Array(blocks)])
    }
    pub fn figure(classes: &[&str], caption: Value, blocks: Vec<Value>) -> Value {
        node(
            "Figure",
            arr([attr(classes), caption, Value::Array(blocks)]),
        )
    }
    /// A table cell: `Cell Attr Alignment RowSpan ColSpan [Block]`.
    pub fn cell(row_span: usize, col_span: usize, blocks: Vec<Value>) -> Value {
        arr([
            attr(&[]),
            json!({ "t": "AlignDefault" }),
            row_span.into(),
            col_span.into(),
            Value::Array(blocks),
        ])
    }
    pub fn row(cells: Vec<Value>) -> Value {
        arr([attr(&[]), Value::Array(cells)])
    }
    /// `Table Attr Caption [ColSpec] TableHead [TableBody] TableFoot`.
    pub fn table(caption: Value, num_cols: usize, head: Vec<Value>, body: Vec<Value>) -> Value {
        let colspecs: Vec<Value> = (0..num_cols)
            .map(|_| json!([{ "t": "AlignDefault" }, { "t": "ColWidthDefault" }]))
            .collect();
        node(
            "Table",
            arr([
                attr(&[]),
                caption,
                Value::Array(colspecs),
                arr([attr(&[]), Value::Array(head)]),
                arr([arr([attr(&[]), 0.into(), json!([]), Value::Array(body)])]),
                arr([attr(&[]), json!([])]),
            ]),
        )
    }

    // --- inlines ------------------------------------------------------------

    pub fn str_(s: &str) -> Value {
        node("Str", json!(s))
    }
    pub fn space() -> Value {
        json!({ "t": "Space" })
    }
    pub fn line_break() -> Value {
        json!({ "t": "LineBreak" })
    }
    /// `Strong`, `Emph`, `Underline`, `Strikeout`, `Subscript`, `Superscript`.
    pub fn wrap(t: &str, inlines: Vec<Value>) -> Value {
        node(t, Value::Array(inlines))
    }
    pub fn code(text: &str) -> Value {
        node("Code", arr([attr(&[]), text.into()]))
    }
    pub fn math(display: bool, tex: &str) -> Value {
        let kind = if display { "DisplayMath" } else { "InlineMath" };
        node("Math", json!([{ "t": kind }, tex]))
    }
    pub fn link(inlines: Vec<Value>, url: &str) -> Value {
        node(
            "Link",
            arr([attr(&[]), Value::Array(inlines), json!([url, ""])]),
        )
    }
    pub fn image(classes: &[&str], alt: Vec<Value>, src: &str) -> Value {
        image_sized(classes, &[], alt, src)
    }
    /// An `Image` with key-value attributes (`width` / `height`, #834).
    pub fn image_sized(
        classes: &[&str],
        attrs: &[(String, String)],
        alt: Vec<Value>,
        src: &str,
    ) -> Value {
        let kv: Vec<Value> = attrs.iter().map(|(k, v)| json!([k, v])).collect();
        node(
            "Image",
            arr([
                json!(["", classes, kv]),
                Value::Array(alt),
                json!([src, ""]),
            ]),
        )
    }
    pub fn note(blocks: Vec<Value>) -> Value {
        node("Note", Value::Array(blocks))
    }
}

/// Text as `Str` words and `Space`s, `\n` as `LineBreak`; surrounding
/// whitespace trimmed (a no-break space stays inside its word, as Pandoc's
/// readers keep it). A Markdown link `[label](target)` in the text — how
/// the flat node chain (the `.doc`, AbiWord, … readers) carries a
/// hyperlink, #852 — is a `Link`.
fn text_inlines(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, line) in text.trim().split('\n').enumerate() {
        if i > 0 {
            out.push(ast::line_break());
        }
        let is_ws = |c: char| c.is_whitespace() && c != '\u{a0}';
        let line_start = out.len();
        // Whitespace seen since the last inline of this line: one `Space`
        // before the next.
        let mut pending_space = false;
        let mut rest = line;
        while !rest.is_empty() {
            let (plain, link) = match markdown_link(rest) {
                Some((before, label, target, after)) => {
                    rest = after;
                    (before, Some((label, target)))
                }
                None => (std::mem::take(&mut rest), None),
            };
            let words: Vec<&str> = plain.split(is_ws).filter(|w| !w.is_empty()).collect();
            if words.is_empty() {
                pending_space |= !plain.is_empty();
            } else {
                if out.len() > line_start && (pending_space || plain.starts_with(is_ws)) {
                    out.push(ast::space());
                }
                for (k, word) in words.iter().enumerate() {
                    if k > 0 {
                        out.push(ast::space());
                    }
                    out.push(ast::str_(word));
                }
                pending_space = plain.ends_with(is_ws);
            }
            if let Some((label, target)) = link {
                if out.len() > line_start && pending_space {
                    out.push(ast::space());
                }
                let mut label_inlines = Vec::new();
                for (k, word) in label.split_whitespace().enumerate() {
                    if k > 0 {
                        label_inlines.push(ast::space());
                    }
                    label_inlines.push(ast::str_(word));
                }
                // An unsafe destination (`javascript:`, #860) keeps the
                // label's words alone.
                if crate::hyperlink::is_safe_hyperlink(target) {
                    out.push(ast::link(label_inlines, target));
                } else {
                    out.extend(label_inlines);
                }
                pending_space = false;
            }
        }
    }
    out
}

/// The first Markdown link of `text` as `(before, label, target, after)`:
/// `[label](target)` with a label free of brackets and newlines and a
/// target free of whitespace and parentheses — the shape the flat readers
/// write; an image's `![alt](src)` is not one.
fn markdown_link(text: &str) -> Option<(&str, &str, &str, &str)> {
    let mut from = 0;
    while let Some(open) = text[from..].find('[') {
        let open = from + open;
        from = open + 1;
        if text[..open].ends_with('!') {
            continue;
        }
        let close = text[open + 1..].find(']')?;
        let close = open + 1 + close;
        let label = &text[open + 1..close];
        if label.is_empty() || label.contains(['[', '\n']) || !text[close + 1..].starts_with('(') {
            continue;
        }
        let Some(end) = text[close + 2..].find(')') else {
            continue;
        };
        let end = close + 2 + end;
        let target = &text[close + 2..end];
        if target.is_empty() || target.contains(|c: char| c.is_whitespace() || c == '(') {
            continue;
        }
        return Some((&text[..open], label, target, &text[end + 1..]));
    }
    None
}

/// [`text_inlines`] with a `Note` spliced in at each call: `notes` is
/// `[[offset, text], …]` (chars into `text`), the JSON's internal `_notes`.
/// Whitespace around a call becomes a `Space`, as Pandoc's docx reader
/// writes `word¹ next` → `Str "word", Note, Space, Str "next"`.
fn text_with_notes(text: &str, notes: &[Value]) -> Vec<Value> {
    let mut calls: Vec<(usize, &str)> = notes
        .iter()
        .filter_map(|n| Some((n.get(0)?.as_u64()? as usize, n.get(1)?.as_str()?)))
        .collect();
    calls.sort_by_key(|&(offset, _)| offset);
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Value> = Vec::new();
    let push_segment = |out: &mut Vec<Value>, seg: &str| {
        let inlines = text_inlines(seg);
        if inlines.is_empty() {
            if !out.is_empty() && seg.chars().any(char::is_whitespace) {
                out.push(ast::space());
            }
            return;
        }
        if !out.is_empty() && seg.starts_with(char::is_whitespace) {
            out.push(ast::space());
        }
        out.extend(inlines);
        if seg.ends_with(char::is_whitespace) {
            out.push(ast::space());
        }
    };
    let mut prev = 0usize;
    for (offset, note) in calls {
        let at = offset.clamp(prev, chars.len());
        let seg: String = chars[prev..at].iter().collect();
        push_segment(&mut out, &seg);
        out.push(ast::note(vec![ast::para(text_inlines(note))]));
        prev = at;
    }
    let rest: String = chars[prev..].iter().collect();
    push_segment(&mut out, &rest);
    while out
        .last()
        .is_some_and(|v| v.get("t").and_then(Value::as_str) == Some("Space"))
    {
        out.pop();
    }
    out
}

/// Inline lists joined by single `Space`s (an inline group's runs) — none
/// before a run that opens with closing punctuation (`.`, `,`, `)`, …) or
/// after one that ends with opening punctuation: docling keeps each
/// formatting run as its own item and its serializers join them with a
/// space, which would put `link .` into every Pandoc output.
fn join_inlines(parts: Vec<Vec<Value>>) -> Vec<Value> {
    fn edge_char(inlines: &[Value], first: bool) -> Option<char> {
        let node = if first {
            inlines.first()
        } else {
            inlines.last()
        }?;
        match node.get("t").and_then(Value::as_str)? {
            "Str" => {
                let s = node.get("c")?.as_str()?;
                if first {
                    s.chars().next()
                } else {
                    s.chars().last()
                }
            }
            // Formatting / links: look inside.
            "Strong" | "Emph" | "Underline" | "Strikeout" | "Subscript" | "Superscript" => {
                edge_char(node.get("c")?.as_array()?, first)
            }
            "Link" => edge_char(node.get("c")?.get(1)?.as_array()?, first),
            _ => None,
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for part in parts.into_iter().filter(|p| !p.is_empty()) {
        if !out.is_empty() {
            let closes = edge_char(&part, true)
                .is_some_and(|c| ".,;:!?)]}%\u{bb}\u{201d}\u{2019}".contains(c));
            let opens = edge_char(&out, false).is_some_and(|c| "([{\u{ab}\u{201c}".contains(c));
            if !closes && !opens {
                out.push(ast::space());
            }
        }
        out.extend(part);
    }
    out
}

/// Pandoc's `Meta` from the document's `metadata` (#836, the JSON export's
/// `--metadata json` object; Python docling's JSON has none): `title`,
/// `subject`, `description`, `publisher`, `rights`, `identifier` and `date`
/// — the document's own date, else when it was created — as `MetaInlines`,
/// `author` and `keywords` as `MetaList`s of `MetaInlines` (Pandoc's own
/// shape for several authors), `lang` as `MetaString`, so `pandoc -t docx`
/// sets the title and author and the templates see them. Empty when the
/// document has none.
fn meta_of(json: &Value) -> Value {
    let mut meta = serde_json::Map::new();
    let Some(m) = json.get("metadata").and_then(Value::as_object) else {
        return Value::Object(meta);
    };
    let text = |key: &str| {
        m.get(key)
            .and_then(Value::as_str)
            .map(text_inlines)
            .filter(|t| !t.is_empty())
    };
    let list = |key: &str| -> Vec<Value> {
        m.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(text_inlines)
            .filter(|t| !t.is_empty())
            .map(ast::meta_inlines)
            .collect()
    };
    for (key, field) in [
        ("title", "title"),
        ("subject", "subject"),
        ("description", "description"),
        ("publisher", "publisher"),
        ("rights", "rights"),
        ("identifier", "identifier"),
    ] {
        if let Some(t) = text(field) {
            meta.insert(key.into(), ast::meta_inlines(t));
        }
    }
    let authors = list("authors");
    if !authors.is_empty() {
        meta.insert("author".into(), ast::meta_list(authors));
    }
    if let Some(date) = text("date").or_else(|| text("created")) {
        meta.insert("date".into(), ast::meta_inlines(date));
    }
    let keywords = list("keywords");
    if !keywords.is_empty() {
        meta.insert("keywords".into(), ast::meta_list(keywords));
    }
    if let Some(lang) = m.get("language").and_then(Value::as_str).map(str::trim) {
        if !lang.is_empty() {
            meta.insert("lang".into(), ast::meta_string(lang));
        }
    }
    Value::Object(meta)
}

/// A docling comment note — `[author: Name (initials), time: date]: text`,
/// the DOCX, `.doc`, ODT and XLSX backends' shape — split into its author,
/// date and text; a note without the head is all text.
fn split_comment(note: &str) -> (Option<String>, Option<String>, String) {
    let Some(rest) = note.strip_prefix('[') else {
        return (None, None, note.to_string());
    };
    let Some((head, body)) = rest.split_once(']') else {
        return (None, None, note.to_string());
    };
    let body = body.strip_prefix(':').unwrap_or(body).trim().to_string();
    let mut author = None;
    let mut date = None;
    for part in head.split(", ") {
        if let Some(a) = part.strip_prefix("author: ") {
            author = Some(a.trim().to_string());
        } else if let Some(t) = part.strip_prefix("time: ") {
            date = Some(t.trim().to_string());
        }
    }
    (author, date, body)
}

/// A length in inches as Pandoc's readers write it: `2.5in`, `1.25in` —
/// up to four decimals, trailing zeros dropped.
fn inches(v: f64) -> String {
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{}in", if s.is_empty() { "0" } else { s })
}

/// Pandoc's `ListAttributes` — start, number style, delimiter — from a
/// list's item markers (#835): `a)` → `(1, LowerAlpha, OneParen)`, `iii.` →
/// `(3, LowerRoman, Period)`, `(3)` → `(3, Decimal, TwoParens)`, a bare
/// `12` → `DefaultDelim`. Every list used to be `Decimal` / `Period`, and a
/// non-decimal start fell back to 1. A single letter reads as Roman only
/// when the list does — `i` / `I` (what Pandoc's Markdown reader assumes),
/// or another item's marker a multi-letter numeral — else as alpha, so a
/// list starting at `c)` is the third letter, not 100. Letters repeat past
/// `z` (`aa` = 27, Pandoc's own extension). A marker nothing parses
/// (`1.2.`) keeps the old `(1, Decimal, Period)`.
fn list_attributes(markers: &[&str]) -> (u64, &'static str, &'static str) {
    fn split(marker: &str) -> (&str, &'static str) {
        let m = marker.trim();
        if let Some(inner) = m.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
            return (inner.trim(), "TwoParens");
        }
        if let Some(inner) = m.strip_suffix(')') {
            return (inner.trim(), "OneParen");
        }
        if let Some(inner) = m.strip_suffix('.') {
            return (inner.trim(), "Period");
        }
        (m, "DefaultDelim")
    }
    fn roman(token: &str) -> Option<u64> {
        let value = |c: char| match c.to_ascii_lowercase() {
            'i' => Some(1),
            'v' => Some(5),
            'x' => Some(10),
            'l' => Some(50),
            'c' => Some(100),
            'd' => Some(500),
            'm' => Some(1000),
            _ => None,
        };
        let values: Vec<i64> = token.chars().map(value).collect::<Option<_>>()?;
        if values.is_empty() {
            return None;
        }
        let mut total = 0i64;
        for (i, &v) in values.iter().enumerate() {
            // Subtractive notation: `IV` is 5 − 1.
            if values.get(i + 1).is_some_and(|&next| next > v) {
                total -= v;
            } else {
                total += v;
            }
        }
        (total > 0).then_some(total as u64)
    }
    fn alpha(token: &str) -> Option<u64> {
        let first = token.chars().next()?;
        if !first.is_ascii_alphabetic() || !token.chars().all(|c| c == first) {
            return None;
        }
        let ordinal = (first.to_ascii_lowercase() as u64) - ('a' as u64) + 1;
        Some((token.chars().count() as u64 - 1) * 26 + ordinal)
    }
    const DEFAULT: (u64, &str, &str) = (1, "Decimal", "Period");
    let Some(first) = markers.first() else {
        return DEFAULT;
    };
    let (token, delim) = split(first);
    if token.is_empty() {
        return DEFAULT;
    }
    if token.chars().all(|c| c.is_ascii_digit()) {
        return token
            .parse()
            .map(|n| (n, "Decimal", delim))
            .unwrap_or(DEFAULT);
    }
    if !token.chars().all(|c| c.is_ascii_alphabetic()) {
        return DEFAULT;
    }
    let upper = token.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    let letters = |t: &str| t.chars().all(|c| c.is_ascii_alphabetic());
    let reads_roman = roman(token).is_some()
        && (token.len() > 1
            || token.eq_ignore_ascii_case("i")
            || markers[1..].iter().any(|m| {
                let (t, _) = split(m);
                t.len() > 1 && letters(t) && roman(t).is_some()
            }));
    if reads_roman {
        let style = if upper { "UpperRoman" } else { "LowerRoman" };
        return (roman(token).unwrap_or(1), style, delim);
    }
    match alpha(token) {
        Some(n) => (n, if upper { "UpperAlpha" } else { "LowerAlpha" }, delim),
        None => DEFAULT,
    }
}

struct GridCell {
    /// The covering cell's index in the [`TableView`].
    cell: usize,
    start_row: usize,
    start_col: usize,
    row_span: usize,
    col_span: usize,
}

struct Serializer<'a> {
    json: &'a Value,
    /// The document's typed tables, which its table items name; empty for
    /// a JSON read in ([`from_docling_json`]).
    tables: &'a [TableData],
    image_mode: ImageMode,
    artifacts_dir: String,
    layers: ContentLayers,
    heading_depth: HeadingDepth,
    track_changes: Option<TrackChanges>,
    artifacts: Vec<(String, Vec<u8>)>,
    pic_index: usize,
    visited: HashSet<String>,
    /// Items some table / picture / graph holds as a caption or footnote:
    /// rendered by their owner, skipped where the walk meets them.
    owned_captions: HashSet<String>,
    owned_footnotes: HashSet<String>,
}

impl<'a> Serializer<'a> {
    fn new(json: &'a Value, options: &PandocExportOptions) -> Self {
        let mut owned_captions = HashSet::new();
        let mut owned_footnotes = HashSet::new();
        for bucket in [
            "pictures",
            "tables",
            "key_value_items",
            "form_items",
            "texts",
        ] {
            for item in Self::array(json.get(bucket)) {
                for (key, set) in [
                    ("captions", &mut owned_captions),
                    ("footnotes", &mut owned_footnotes),
                ] {
                    for r in Self::array(item.get(key)) {
                        if let Some(cref) = r.get("$ref").and_then(Value::as_str) {
                            set.insert(cref.to_string());
                        }
                    }
                }
            }
        }
        Self {
            json,
            tables: &[],
            image_mode: options.image_mode,
            artifacts_dir: options.artifacts_dir.clone(),
            layers: options.layers,
            heading_depth: options.heading_depth,
            track_changes: options.track_changes,
            artifacts: Vec::new(),
            pic_index: 0,
            visited: HashSet::new(),
            owned_captions,
            owned_footnotes,
        }
    }

    fn array(v: Option<&'a Value>) -> impl Iterator<Item = &'a Value> {
        v.and_then(Value::as_array).into_iter().flatten()
    }

    fn resolve(&self, cref: &str) -> Option<&'a Value> {
        let rest = cref.strip_prefix("#/")?;
        if rest == "body" {
            return self.json.get("body");
        }
        let (bucket, idx) = rest.split_once('/')?;
        self.json.get(bucket)?.get(idx.parse::<usize>().ok()?)
    }

    fn refs(v: Option<&'a Value>) -> Vec<&'a str> {
        Self::array(v)
            .filter_map(|r| r.get("$ref").and_then(Value::as_str))
            .collect()
    }

    fn children(item: &'a Value) -> Vec<&'a str> {
        Self::refs(item.get("children"))
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

    /// The item's link destinations (`_anchors`, #849).
    fn anchors(item: &Value) -> Vec<String> {
        item.get("_anchors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_group(item: &Value) -> bool {
        Self::self_ref(item).starts_with("#/groups/")
    }

    fn excluded(&self, item: &Value) -> bool {
        let layer = item
            .get("content_layer")
            .and_then(Value::as_str)
            .unwrap_or("body");
        !self.layers.contains_name(layer)
    }

    /// docling's `_iterate_items(with_groups=True, traverse_pictures=False)`
    /// below `root`, depth-first pre-order; items off the chosen layers are
    /// not yielded but their subtrees are walked; under a picture only its
    /// captions are visited.
    fn descendants(&self, root: &'a Value, out: &mut Vec<&'a Value>) {
        let picture = Self::self_ref(root).starts_with("#/pictures/");
        let captions = if picture {
            Self::refs(root.get("captions"))
        } else {
            Vec::new()
        };
        for cref in Self::children(root) {
            if picture && !captions.contains(&cref) {
                continue;
            }
            // A caption or footnote some table / picture / graph owns is
            // rendered by that owner, inline group and all (#833): neither
            // it nor what nests under it is a block of the walk.
            if self.owned_captions.contains(cref) || self.owned_footnotes.contains(cref) {
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

    /// Every not-yet-visited descendant of `item`, serialized in order —
    /// a container marks what it renders as visited, so the flat walk
    /// skips it.
    fn blocks_of(&mut self, item: &'a Value) -> Vec<Value> {
        let mut nodes = Vec::new();
        self.descendants(item, &mut nodes);
        let mut out = Vec::new();
        for node in nodes {
            if self.visited.insert(Self::self_ref(node).to_string()) {
                out.extend(self.serialize(node));
            }
        }
        out
    }

    fn body(&mut self) -> Vec<Value> {
        let Some(body) = self.json.get("body") else {
            return Vec::new();
        };
        self.visited.insert("#/body".to_string());
        let mut blocks = self.blocks_of(body);
        let notes = self.unplaced_notes();
        blocks.extend(notes);
        blocks
    }

    /// Furniture `footnote` items no text item calls — docling's DOCX / ODT
    /// note bodies when the JSON carries no call sites (one exported by
    /// Python docling), or a call that could not be anchored — written while
    /// the furniture layer itself is off: each as a trailing `Note`, so a
    /// document rebuilt from the AST still has its notes (#538).
    fn unplaced_notes(&mut self) -> Vec<Value> {
        let notes: Vec<&'a Value> = Self::array(self.json.get("texts"))
            .filter(|t| Self::label(t) == "footnote" && !Self::is_note_body(t))
            .filter(|t| t.get("content_layer").and_then(Value::as_str) == Some("furniture"))
            .filter(|t| self.excluded(t) && !self.owned_footnotes.contains(Self::self_ref(t)))
            .collect();
        let mut out = Vec::new();
        for t in notes {
            // The note's formatting and links too (#833).
            let (text, _, _) = self.own_inlines(t);
            if !text.is_empty() {
                out.push(ast::para(vec![ast::note(vec![ast::para(text)])]));
            }
        }
        out
    }

    /// One item's blocks.
    fn serialize(&mut self, item: &'a Value) -> Vec<Value> {
        let sref = Self::self_ref(item);
        if Self::is_group(item) {
            return match Self::label(item) {
                // A comment thread is written as spans on the item it
                // annotates (#836), never as blocks; one no item refers to
                // (a plain cell's, an orphan's) has no place in the AST.
                "comment_section" => Vec::new(),
                "list" => self.list_group(item),
                "inline" => {
                    let (inlines, mut rest) = self.inline_group(item);
                    let mut out = Vec::new();
                    if !inlines.is_empty() {
                        out.push(ast::para(inlines));
                    }
                    out.append(&mut rest);
                    out
                }
                _ => self.blocks_of(item),
            };
        }
        if sref.starts_with("#/texts/") {
            if self.owned_captions.contains(sref) || self.owned_footnotes.contains(sref) {
                return Vec::new();
            }
            // A note body some text item calls is written there, as a `Note`.
            if self.excluded(item) || Self::is_note_body(item) {
                return Vec::new();
            }
            return self.text_item(item);
        }
        if sref.starts_with("#/tables/") {
            return self.table(item);
        }
        if sref.starts_with("#/pictures/") {
            return self.picture(item);
        }
        if sref.starts_with("#/key_value_items/") {
            return self.graph_item(item, "key-value-region");
        }
        if sref.starts_with("#/form_items/") {
            return self.graph_item(item, "form-container");
        }
        if sref.starts_with("#/field_regions/") {
            return self.field_region(item);
        }
        if sref.starts_with("#/field_items/") {
            let entries = vec![self.field_item(item)];
            return Self::field_blocks(entries);
        }
        Vec::new()
    }

    /// A form's field region (#515): its fields as one `DefinitionList` —
    /// key → value(s) — in a `Div .field-region`; a field without a key
    /// contributes its blocks in place.
    fn field_region(&mut self, region: &'a Value) -> Vec<Value> {
        let mut entries = Vec::new();
        for cref in Self::children(region) {
            let Some(child) = self.resolve(cref) else {
                continue;
            };
            if !self.visited.insert(cref.to_string()) {
                continue;
            }
            if cref.starts_with("#/field_items/") {
                entries.push(self.field_item(child));
            } else {
                let mut blocks = if self.excluded(child) {
                    Vec::new()
                } else {
                    self.serialize(child)
                };
                blocks.extend(self.blocks_of(child));
                entries.push((Vec::new(), blocks));
            }
        }
        let blocks = Self::field_blocks(entries);
        if blocks.is_empty() {
            Vec::new()
        } else {
            vec![ast::div(&["field-region"], blocks)]
        }
    }

    /// One field: its `marker` and `field_key` texts as the term, each
    /// `field_value` (and anything else under it) as a definition.
    fn field_item(&mut self, field: &'a Value) -> (Vec<Value>, Vec<Value>) {
        let mut nodes = Vec::new();
        self.descendants(field, &mut nodes);
        let (mut term, mut defs) = (Vec::new(), Vec::new());
        for node in nodes {
            if !self.visited.insert(Self::self_ref(node).to_string()) {
                continue;
            }
            match Self::label(node) {
                "marker" | "field_key" => {
                    let inlines = self.formatted(node);
                    if !inlines.is_empty() {
                        term = join_inlines(vec![term, inlines]);
                    }
                }
                "field_value" => {
                    let inlines = self.formatted(node);
                    if !inlines.is_empty() {
                        defs.push(ast::plain(inlines));
                    }
                }
                _ => defs.extend(self.serialize(node)),
            }
        }
        (term, defs)
    }

    /// Fields as blocks: runs of keyed fields become `DefinitionList`s, a
    /// key-less field's blocks stand on their own.
    fn field_blocks(entries: Vec<(Vec<Value>, Vec<Value>)>) -> Vec<Value> {
        let mut out = Vec::new();
        let mut run: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
        for (term, defs) in entries {
            if term.is_empty() {
                if !run.is_empty() {
                    out.push(ast::definition_list(std::mem::take(&mut run)));
                }
                out.extend(defs);
            } else {
                let defs = defs.into_iter().map(|d| vec![d]).collect();
                run.push((term, defs));
            }
        }
        if !run.is_empty() {
            out.push(ast::definition_list(run));
        }
        out
    }

    /// A text item's own inlines: its formatted text, or — for an empty
    /// item whose first child is an inline group (a list item, heading or
    /// caption whose content is a run of formatted spans; whatever else
    /// nests under it — a sub-list, a section's paragraphs — follows as
    /// blocks of its own, #833) — that group's, with any blocks the group
    /// held besides.
    fn own_inlines(&mut self, item: &'a Value) -> (Vec<Value>, Vec<Value>, bool) {
        let children = Self::children(item);
        if Self::text(item).is_empty() {
            if let Some(group) = children
                .first()
                .and_then(|c| self.resolve(c))
                .filter(|c| Self::is_group(c) && Self::label(c) == "inline")
            {
                self.visited.insert(Self::self_ref(group).to_string());
                let (inlines, rest) = self.inline_group(group);
                return (inlines, rest, true);
            }
        }
        (self.formatted(item), Vec::new(), false)
    }

    /// `inlines` between the comment spans of the item's reviewer comments
    /// (#836): for each `comment_section` group the item's `comments` refer
    /// to, one `Span .comment-start` per comment text — `comment-id` (the
    /// group's number, `-n` for a thread's replies), `author`, `date` from
    /// docling's note head, the comment's text inside — before the inlines
    /// and an empty `Span .comment-end` after them, Pandoc's docx reader's
    /// `--track-changes=all` shape. The groups are then visited, so a notes
    /// layer that is on does not write them again as blocks.
    fn with_comments(&mut self, item: &'a Value, inlines: Vec<Value>) -> Vec<Value> {
        let refs = Self::refs(item.get("comments"));
        if refs.is_empty() {
            return inlines;
        }
        // An explicit accept / reject view carries no comments (#851),
        // as Pandoc's `--track-changes=accept|reject` write none.
        if matches!(
            self.track_changes,
            Some(TrackChanges::Accept | TrackChanges::Reject)
        ) {
            return inlines;
        }
        let mut starts = Vec::new();
        let mut ends = Vec::new();
        for cref in refs {
            let Some(group) = self.resolve(cref) else {
                continue;
            };
            if !Self::is_group(group) || Self::label(group) != "comment_section" {
                continue;
            }
            self.visited.insert(cref.to_string());
            let id = group
                .get("name")
                .and_then(Value::as_str)
                .and_then(|n| n.strip_prefix("comment-"))
                .unwrap_or("0")
                .to_string();
            // The group's texts sit on the notes layer, which the layer
            // filter would drop: read the children directly.
            let mut k = 0;
            for r in Self::children(group) {
                self.visited.insert(r.to_string());
                let Some(node) = self.resolve(r).filter(|_| r.starts_with("#/texts/")) else {
                    continue;
                };
                let (author, date, text) = split_comment(Self::text(node));
                let comment_id = if k == 0 {
                    id.clone()
                } else {
                    format!("{id}-{k}")
                };
                k += 1;
                let mut attrs = vec![("comment-id".to_string(), comment_id.clone())];
                if let Some(a) = author {
                    attrs.push(("author".to_string(), a));
                }
                if let Some(d) = date {
                    attrs.push(("date".to_string(), d));
                }
                starts.push(ast::span(&["comment-start"], &attrs, text_inlines(&text)));
                ends.push(ast::span(
                    &["comment-end"],
                    &[("comment-id".to_string(), comment_id)],
                    Vec::new(),
                ));
            }
        }
        if starts.is_empty() {
            return inlines;
        }
        let mut out = starts;
        out.extend(inlines);
        out.extend(ends);
        out
    }

    /// The item's text with its formatting and hyperlink applied, and the
    /// notes it calls as `Note`s at their call sites (#538).
    fn formatted(&self, item: &Value) -> Vec<Value> {
        let base = match Self::label(item) {
            "code" => vec![ast::code(Self::text(item))],
            "formula" if !Self::text(item).is_empty() => {
                vec![ast::math(false, Self::text(item).trim())]
            }
            _ => {
                let (text, skipped) = Self::authored_text(item);
                match item.get("_notes").and_then(Value::as_array) {
                    Some(notes) if !notes.is_empty() => {
                        if skipped == 0 {
                            text_with_notes(text, notes)
                        } else {
                            // The note offsets count the numbered text.
                            let shifted: Vec<Value> = notes
                                .iter()
                                .map(|n| {
                                    let off = n[0].as_u64().unwrap_or(0).saturating_sub(skipped);
                                    serde_json::json!([off, n[1]])
                                })
                                .collect();
                            text_with_notes(text, &shifted)
                        }
                    }
                    _ => text_inlines(text),
                }
            }
        };
        Self::decorate(base, item)
    }

    /// The item's text without the generated numbering it opens with
    /// (`_number`, #850), and how many chars were dropped.
    fn authored_text(item: &Value) -> (&str, u64) {
        let text = Self::text(item);
        if let Some(number) = item.get("_number").and_then(Value::as_str) {
            if let Some(rest) = text
                .strip_prefix(number)
                .and_then(|rest| rest.strip_prefix(' '))
            {
                return (rest, (number.chars().count() + 1) as u64);
            }
        }
        (text, 0)
    }

    /// A furniture `footnote` item that is the body of a note a text item
    /// calls ([`crate::tree::TreeItem::note_body`]).
    fn is_note_body(item: &Value) -> bool {
        item.get("_note_body").and_then(Value::as_bool) == Some(true)
    }

    fn decorate(mut inlines: Vec<Value>, item: &Value) -> Vec<Value> {
        if inlines.is_empty() {
            return inlines;
        }
        if let Some(f) = item.get("formatting") {
            let on = |k: &str| f.get(k).and_then(Value::as_bool).unwrap_or(false);
            if on("bold") {
                inlines = vec![ast::wrap("Strong", inlines)];
            }
            if on("italic") {
                inlines = vec![ast::wrap("Emph", inlines)];
            }
            if on("underline") {
                inlines = vec![ast::wrap("Underline", inlines)];
            }
            if on("strikethrough") {
                inlines = vec![ast::wrap("Strikeout", inlines)];
            }
            match f.get("script").and_then(Value::as_str) {
                Some("sub") => inlines = vec![ast::wrap("Subscript", inlines)],
                Some("super") => inlines = vec![ast::wrap("Superscript", inlines)],
                _ => {}
            }
        }
        // An unsafe destination (`javascript:`, #860) keeps the text alone.
        if let Some(url) = item
            .get("hyperlink")
            .and_then(Value::as_str)
            .filter(|u| crate::hyperlink::is_safe_hyperlink(u))
        {
            inlines = vec![ast::link(inlines, url)];
        }
        // A tracked change kept under `all` (#851): Pandoc's docx reader's
        // span, outermost.
        if let Some(rev) = item.get("_revision") {
            if let Some(kind) = rev.get("kind").and_then(Value::as_str) {
                let mut attrs = Vec::new();
                for key in ["author", "date"] {
                    if let Some(v) = rev.get(key).and_then(Value::as_str) {
                        attrs.push((key.to_string(), v.to_string()));
                    }
                }
                inlines = vec![ast::span(&[kind], &attrs, inlines)];
            }
        }
        inlines
    }

    /// An inline group's runs as inlines joined by `Space`; anything in it
    /// that has no inline form (a nested list, a table) comes back as
    /// blocks for after the paragraph.
    fn inline_group(&mut self, group: &'a Value) -> (Vec<Value>, Vec<Value>) {
        let mut nodes = Vec::new();
        self.descendants(group, &mut nodes);
        let mut runs: Vec<Vec<Value>> = Vec::new();
        let mut rest: Vec<Value> = Vec::new();
        for node in nodes {
            let r = Self::self_ref(node).to_string();
            if self.visited.contains(&r) {
                continue;
            }
            self.visited.insert(r.clone());
            if r.starts_with("#/texts/") {
                if self.owned_captions.contains(&r)
                    || self.owned_footnotes.contains(&r)
                    || Self::is_note_body(node)
                {
                    continue;
                }
                let mut run: Vec<Value> = Self::anchors(node)
                    .iter()
                    .map(|id| ast::anchor(id))
                    .collect();
                run.append(&mut self.formatted(node));
                runs.push(run);
                continue;
            }
            if Self::is_group(node) && Self::label(node) == "inline" {
                let (inl, mut more) = self.inline_group(node);
                runs.push(inl);
                rest.append(&mut more);
                continue;
            }
            rest.extend(self.serialize(node));
        }
        (join_inlines(runs), rest)
    }

    fn text_item(&mut self, item: &'a Value) -> Vec<Value> {
        let label = Self::label(item);
        let mut out = Vec::new();
        match label {
            "list_item" => {
                // A list item met outside a list group: a one-item list.
                let blocks = self.list_item_blocks(item);
                if !blocks.is_empty() {
                    out.push(ast::bullet_list(vec![blocks]));
                }
                return out;
            }
            "code" => {
                let lang = item
                    .get("code_language")
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty() && *l != "unknown")
                    .map(str::to_ascii_lowercase);
                out.push(ast::code_block(lang.as_deref(), Self::text(item)));
            }
            "formula" => {
                let tex = Self::text(item).trim();
                if !tex.is_empty() {
                    out.push(ast::para(vec![ast::math(true, tex)]));
                }
            }
            _ => {
                let (inlines, rest, _) = self.own_inlines(item);
                let mut inlines = self.with_comments(item, inlines);
                // The item's link destinations (#849): a heading takes
                // the first as its identifier, the rest open its inlines
                // as anchor spans; any other item opens with them all.
                let mut anchors = Self::anchors(item);
                let header_id =
                    if matches!(label, "title" | "section_header") && !anchors.is_empty() {
                        Some(anchors.remove(0))
                    } else {
                        None
                    };
                if !anchors.is_empty() && (header_id.is_some() || !inlines.is_empty()) {
                    let mut with_anchors: Vec<Value> =
                        anchors.iter().map(|id| ast::anchor(id)).collect();
                    with_anchors.append(&mut inlines);
                    inlines = with_anchors;
                }
                match label {
                    "title" | "section_header" => {
                        let level = if label == "title" {
                            1
                        } else {
                            let n = item.get("level").and_then(Value::as_u64).unwrap_or(1) as usize;
                            match self.heading_depth {
                                // docling's depth: level 1 is `##`, under
                                // the title's `#`.
                                HeadingDepth::Docling => (n + 1).min(6),
                                // The source's depth (#848): a DOCX
                                // `Heading 1` is `Header 1`, as Pandoc's
                                // docx reader writes it.
                                HeadingDepth::Source => n.clamp(1, 6),
                            }
                        };
                        if !inlines.is_empty() {
                            out.push(match &header_id {
                                Some(id) => ast::header_with_id(level, id, inlines),
                                None => ast::header(level, inlines),
                            });
                        }
                    }
                    "text" | "paragraph" => {
                        if !inlines.is_empty() {
                            out.push(ast::para(inlines));
                        }
                    }
                    "checkbox_selected" | "checkbox_unselected" => {
                        let mark = if label == "checkbox_selected" {
                            "☒"
                        } else {
                            "☐"
                        };
                        let mut with_mark = vec![ast::str_(mark)];
                        if !inlines.is_empty() {
                            with_mark.push(ast::space());
                            with_mark.append(&mut inlines);
                        }
                        out.push(ast::para(with_mark));
                    }
                    other => {
                        if !inlines.is_empty() {
                            let class = format!("docling-{other}");
                            out.push(ast::div(&[&class], vec![ast::para(inlines)]));
                        }
                    }
                }
                out.extend(rest);
            }
        }
        // Content nested under the item (an HTML / DOCX heading's section).
        out.extend(self.blocks_of(item));
        out
    }

    /// A list item's blocks: its own text as `Plain`, then whatever is
    /// nested under it (sub-lists, paragraphs).
    fn list_item_blocks(&mut self, item: &'a Value) -> Vec<Value> {
        let (inlines, rest, _) = self.own_inlines(item);
        let inlines = self.with_comments(item, inlines);
        let mut blocks = Vec::new();
        if !inlines.is_empty() {
            blocks.push(ast::plain(inlines));
        }
        blocks.extend(rest);
        blocks.extend(self.blocks_of(item));
        blocks
    }

    fn list_group(&mut self, group: &'a Value) -> Vec<Value> {
        let first = Self::children(group)
            .first()
            .and_then(|r| self.resolve(r))
            .filter(|c| Self::label(c) == "list_item");
        let enumerated = first.is_some_and(|c| {
            c.get("enumerated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });
        // The items' markers decide the list's start, number style and
        // delimiter (#835).
        let markers: Vec<&str> = Self::children(group)
            .iter()
            .filter_map(|r| self.resolve(r))
            .filter(|c| Self::label(c) == "list_item")
            .filter_map(|c| c.get("marker").and_then(Value::as_str))
            .collect();
        let (start, style, delim) = list_attributes(&markers);

        let mut nodes = Vec::new();
        self.descendants(group, &mut nodes);
        let mut items: Vec<Vec<Value>> = Vec::new();
        for node in nodes {
            let r = Self::self_ref(node).to_string();
            if !self.visited.insert(r.clone()) {
                continue;
            }
            if Self::label(node) == "list_item"
                && r.starts_with("#/texts/")
                && !self.owned_captions.contains(&r)
            {
                let blocks = self.list_item_blocks(node);
                items.push(blocks);
                continue;
            }
            // Something that is not a list item directly in the list (a
            // paragraph between items): it belongs to the item before it.
            let blocks = self.serialize(node);
            if blocks.is_empty() {
                continue;
            }
            match items.last_mut() {
                Some(last) => last.extend(blocks),
                None => items.push(blocks),
            }
        }
        if items.is_empty() {
            return Vec::new();
        }
        if enumerated {
            vec![ast::ordered_list(start, style, delim, items)]
        } else {
            vec![ast::bullet_list(items)]
        }
    }

    /// The owner's caption texts as one `Caption`, with its footnotes as
    /// `Note`s at the end — the float is their call site. Each caption and
    /// note keeps its formatting, links and inline group (#833: a bold,
    /// linked `<caption>` used to come out as bare words, a multi-run one
    /// as an empty caption plus a stray `Para`).
    fn caption(&mut self, item: &'a Value) -> Value {
        let mut inlines = self.caption_text(item);
        for cref in Self::refs(item.get("footnotes")) {
            if let Some(note) = self.resolve(cref) {
                if cref.starts_with("#/texts/") && !self.excluded(note) {
                    let (text, _, _) = self.own_inlines(note);
                    if !text.is_empty() {
                        inlines.push(ast::note(vec![ast::para(text)]));
                    }
                }
            }
        }
        if inlines.is_empty() {
            ast::caption(Vec::new())
        } else {
            ast::caption(vec![ast::plain(inlines)])
        }
    }

    /// The owner's caption texts alone (no notes) — a picture's alt text,
    /// as Pandoc's readers give a captioned image.
    fn caption_text(&mut self, item: &'a Value) -> Vec<Value> {
        let caps: Vec<&'a Value> = Self::refs(item.get("captions"))
            .into_iter()
            .filter(|cref| cref.starts_with("#/texts/"))
            .filter_map(|cref| self.resolve(cref))
            .filter(|cap| !self.excluded(cap))
            .collect();
        let mut parts = Vec::new();
        for cap in caps {
            let (inlines, _, _) = self.own_inlines(cap);
            parts.push(inlines);
        }
        join_inlines(parts)
    }

    /// docling's `TableData.grid`: each position → the cell covering it.
    fn grid(table: &TableView<'_>) -> (usize, Vec<Vec<Option<GridCell>>>) {
        let (num_rows, num_cols) = (table.num_rows, table.num_cols);
        let mut grid: Vec<Vec<Option<GridCell>>> = (0..num_rows)
            .map(|_| (0..num_cols).map(|_| None).collect())
            .collect();
        for (i, cell) in table.cells.iter().enumerate() {
            let (r0, c0) = (cell.start_row, cell.start_col);
            let (r1, c1) = (cell.end_row, cell.end_col);
            if r0 >= num_rows || c0 >= num_cols {
                continue;
            }
            let (r1, c1) = (r1.clamp(r0 + 1, num_rows), c1.clamp(c0 + 1, num_cols));
            for row in grid.iter_mut().take(r1).skip(r0) {
                for slot in row.iter_mut().take(c1).skip(c0) {
                    *slot = Some(GridCell {
                        cell: i,
                        start_row: r0,
                        start_col: c0,
                        row_span: r1 - r0,
                        col_span: c1 - c0,
                    });
                }
            }
        }
        (num_cols, grid)
    }

    fn table_from_data(&mut self, data: &'a Value, caption: Value) -> Option<Value> {
        let table = crate::json::table_view(data, self.tables);
        let (num_cols, grid) = Self::grid(&table);
        if grid.is_empty() || num_cols == 0 {
            return None;
        }
        let mut rows: Vec<(bool, Value)> = Vec::new();
        for (i, row) in grid.iter().enumerate() {
            let mut cells = Vec::new();
            let mut all_header = true;
            let mut anchored = 0;
            for (j, slot) in row.iter().enumerate() {
                let Some(g) = slot else {
                    // A position no cell covers (a table's empty corner) is
                    // neutral to the header test.
                    cells.push(ast::cell(1, 1, Vec::new()));
                    continue;
                };
                if g.start_row != i || g.start_col != j {
                    continue;
                }
                anchored += 1;
                let cell = &table.cells[g.cell];
                all_header &= cell.column_header;
                let blocks = match cell.cell_ref {
                    Some(cref) => match self.resolve(cref) {
                        Some(target) => {
                            self.visited.insert(cref.to_string());
                            self.serialize(target)
                        }
                        None => Vec::new(),
                    },
                    None => {
                        let inl = text_inlines(cell.text);
                        if inl.is_empty() {
                            Vec::new()
                        } else {
                            vec![ast::plain(inl)]
                        }
                    }
                };
                cells.push(ast::cell(g.row_span, g.col_span, blocks));
            }
            rows.push((all_header && anchored > 0, ast::row(cells)));
        }
        // Leading all-header rows form the head; the rest is the body.
        let head_len = rows.iter().take_while(|(h, _)| *h).count();
        let head_len = if head_len == rows.len() { 0 } else { head_len };
        let mut head = Vec::new();
        let mut body = Vec::new();
        for (i, (_, row)) in rows.into_iter().enumerate() {
            if i < head_len {
                head.push(row);
            } else {
                body.push(row);
            }
        }
        Some(ast::table(caption, num_cols, head, body))
    }

    fn table(&mut self, item: &'a Value) -> Vec<Value> {
        let caption = self.caption(item);
        if self.excluded(item) {
            return Vec::new();
        }
        match item.get("data") {
            Some(data) => self.table_from_data(data, caption).into_iter().collect(),
            None => Vec::new(),
        }
    }

    fn picture(&mut self, item: &'a Value) -> Vec<Value> {
        if self.excluded(item) {
            return Vec::new();
        }
        let caption = self.caption(item);
        let has_caption = caption
            .get(1)
            .and_then(Value::as_array)
            .is_some_and(|b| !b.is_empty());
        let uri = item
            .get("image")
            .and_then(|i| i.get("uri"))
            .and_then(Value::as_str);
        let src = match (self.image_mode, uri) {
            (ImageMode::Embedded, Some(uri)) if uri.starts_with("data:") => Some(uri.to_string()),
            (ImageMode::Referenced, Some(uri)) => self
                .reference_image(uri)
                .or_else(|| (!uri.starts_with("data:")).then(|| uri.to_string())),
            _ => None,
        };
        // #537: every picture is an `Image`, so it survives into whatever
        // Pandoc writes and can be counted — one without a target
        // (placeholder mode, or a payload docling could not decode: EMF/WMF)
        // carries the `docling-placeholder` class and an empty target, which
        // a writer reports as a missing resource and renders as the alt text.
        // The alt text is the author's (`meta.description`, #834) — what
        // Pandoc's docx reader takes from `wp:docPr descr` — and only
        // without one the caption, as before.
        let description = item
            .get("meta")
            .and_then(|m| m.get("description"))
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
            .map(text_inlines)
            .filter(|t| !t.is_empty());
        let alt = match description {
            Some(alt) => alt,
            None => self.caption_text(item),
        };
        // The displayed size (`_display`, inches) as `width` / `height`
        // attributes, Pandoc's readers' shape for a sized image (#834).
        let attrs: Vec<(String, String)> = item
            .get("image")
            .and_then(|i| i.get("_display"))
            .and_then(Value::as_array)
            .and_then(|d| Some((d.first()?.as_f64()?, d.get(1)?.as_f64()?)))
            .map(|(w, h)| {
                vec![
                    ("width".to_string(), inches(w)),
                    ("height".to_string(), inches(h)),
                ]
            })
            .unwrap_or_default();
        let image = match &src {
            Some(src) => ast::image_sized(&[], &attrs, alt, src),
            None => ast::image_sized(&["docling-placeholder"], &attrs, alt, ""),
        };
        // A native chart's data grid (`meta.tabular_chart.chart_data`).
        let chart = item
            .get("meta")
            .and_then(|m| m.get("tabular_chart"))
            .and_then(|t| t.get("chart_data"))
            .and_then(|chart| self.table_from_data(chart, ast::caption(Vec::new())));
        if chart.is_none() && !has_caption {
            // Pandoc's own readers put a caption-less image in a paragraph.
            return vec![ast::para(vec![image])];
        }
        let mut blocks = vec![ast::plain(vec![image])];
        blocks.extend(chart);
        vec![ast::figure(&[], caption, blocks)]
    }

    /// Referenced mode: a `data:` URI becomes an artifact file named like
    /// the Markdown export's; the path is the image target.
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

    /// A key-value / form graph: a `DefinitionList` of keys and their
    /// values, or — when `to_child` links make it a hierarchy — the nested
    /// `BulletList` of it (the HTML export's two shapes), in a classed `Div`
    /// with the item's caption after it.
    fn graph_item(&mut self, item: &'a Value, class: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.excluded(item) {
            if let Some(graph) = item.get("graph") {
                if let Some(block) = Self::graph(graph) {
                    out.push(ast::div(&[class], vec![block]));
                }
            }
        }
        // The caption (and footnote notes) as a paragraph after the region.
        let caption = self.caption(item);
        for block in caption
            .get(1)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(inlines) = block.get("c").and_then(Value::as_array) {
                out.push(ast::para(inlines.clone()));
            }
        }
        out
    }

    fn graph(graph: &'a Value) -> Option<Value> {
        let cells: HashMap<u64, &Value> = Self::array(graph.get("cells"))
            .filter_map(|c| Some((c.get("cell_id")?.as_u64()?, c)))
            .collect();
        let order: Vec<u64> = Self::array(graph.get("cells"))
            .filter_map(|c| c.get("cell_id")?.as_u64())
            .collect();
        let mut child_links: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut value_links: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut value_order: Vec<u64> = Vec::new();
        let mut parents: HashSet<u64> = HashSet::new();
        for link in Self::array(graph.get("links")) {
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
        let cell_text =
            |id: u64| text_inlines(cells[&id].get("text").and_then(Value::as_str).unwrap_or(""));
        if child_links.is_empty() {
            if value_order.is_empty() {
                return None;
            }
            let entries = value_order
                .iter()
                .map(|&k| {
                    let defs = value_links[&k]
                        .iter()
                        .map(|&v| vec![ast::plain(cell_text(v))])
                        .collect();
                    (cell_text(k), defs)
                })
                .collect();
            return Some(ast::definition_list(entries));
        }
        // Hierarchical: a nested bullet list, cycles cut on the descent path.
        fn node(
            id: u64,
            cell_text: &dyn Fn(u64) -> Vec<Value>,
            child_links: &HashMap<u64, Vec<u64>>,
            value_links: &BTreeMap<u64, Vec<u64>>,
            path: &mut HashSet<u64>,
        ) -> Option<Vec<Value>> {
            if !path.insert(id) {
                return None;
            }
            let mut inlines = cell_text(id);
            if let Some(values) = value_links.get(&id) {
                let vals = join_inlines(values.iter().map(|&v| cell_text(v)).collect());
                inlines = vec![ast::wrap("Strong", inlines)];
                inlines.push(ast::str_(":"));
                if !vals.is_empty() {
                    inlines.push(ast::space());
                    inlines.extend(vals);
                }
            }
            let mut blocks = vec![ast::plain(inlines)];
            let kids: Vec<Vec<Value>> = child_links
                .get(&id)
                .into_iter()
                .flatten()
                .filter_map(|&c| node(c, cell_text, child_links, value_links, path))
                .collect();
            if !kids.is_empty() {
                blocks.push(ast::bullet_list(kids));
            }
            path.remove(&id);
            Some(blocks)
        }
        let mut path = HashSet::new();
        let items: Vec<Vec<Value>> = order
            .iter()
            .filter(|id| !parents.contains(id))
            .filter_map(|&id| node(id, &cell_text, &child_links, &value_links, &mut path))
            .collect();
        (!items.is_empty()).then(|| ast::bullet_list(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A docling JSON document from `body` children and the item buckets.
    fn doc(body: &[&str], texts: Value, groups: Value, tables: Value) -> Value {
        json!({
            "schema_name": "DoclingDocument",
            "name": "t",
            "body": {"self_ref": "#/body", "children": body.iter().map(|r| json!({"$ref": r})).collect::<Vec<_>>()},
            "texts": texts, "groups": groups, "tables": tables, "pictures": []
        })
    }

    fn text(i: usize, label: &str, text: &str, extra: Value) -> Value {
        let mut v = json!({"self_ref": format!("#/texts/{i}"), "children": [], "label": label, "text": text, "content_layer": "body"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    fn blocks(json: &Value) -> Vec<Value> {
        let (s, _) = from_docling_json(json, &PandocExportOptions::default()).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["pandoc-api-version"], json!(PANDOC_API_VERSION));
        v["blocks"].as_array().unwrap().clone()
    }

    /// #538: a text item's note calls become `Note`s at their offsets —
    /// glued to the word before, a `Space` where the text had one — and the
    /// note bodies they carry are not written again as blocks.
    #[test]
    fn note_calls_become_notes_at_their_call_sites() {
        let j = doc(
            &["#/texts/0", "#/texts/1"],
            json!([
                text(
                    0,
                    "text",
                    "debut suite end",
                    json!({"_notes": [[5, "first"], [15, "last"]]})
                ),
                text(
                    1,
                    "footnote",
                    "first",
                    json!({"content_layer": "furniture", "_note_body": true})
                ),
            ]),
            json!([]),
            json!([]),
        );
        let note = |t: &str| ast::note(vec![ast::para(vec![ast::str_(t)])]);
        assert_eq!(
            blocks(&j),
            vec![ast::para(vec![
                ast::str_("debut"),
                note("first"),
                ast::space(),
                ast::str_("suite"),
                ast::space(),
                ast::str_("end"),
                note("last"),
            ])]
        );
        // With the furniture layer on, a placed note body is still not a block.
        let opts = PandocExportOptions {
            layers: ContentLayers::ALL,
            ..Default::default()
        };
        let (s, _) = from_docling_json(&j, &opts).unwrap();
        assert!(!s.contains("docling-footnote"), "{s}");
    }

    /// #538: a furniture footnote nothing calls (Python docling's JSON has no
    /// call sites) still reaches the AST, as a trailing `Note`.
    #[test]
    fn uncalled_furniture_footnotes_trail_as_notes() {
        let j = doc(
            &["#/texts/0", "#/texts/1"],
            json!([
                text(0, "text", "body", json!({})),
                text(
                    1,
                    "footnote",
                    "MARKFN",
                    json!({"content_layer": "furniture"})
                ),
            ]),
            json!([]),
            json!([]),
        );
        assert_eq!(
            blocks(&j),
            vec![
                ast::para(vec![ast::str_("body")]),
                ast::para(vec![ast::note(vec![ast::para(vec![ast::str_("MARKFN")])])]),
            ]
        );
    }

    /// #537: every picture is an `Image` — embedded by default, in a `Para`
    /// without a caption, in a `Figure` (caption = alt text) with one; with
    /// no target (placeholder mode, an undecodable payload) it is classed
    /// `docling-placeholder` instead of vanishing as raw HTML.
    #[test]
    fn pictures_are_always_images() {
        let mut j = doc(
            &["#/pictures/0", "#/pictures/1", "#/pictures/2"],
            json!([text(0, "caption", "A duck", json!({})),]),
            json!([]),
            json!([]),
        );
        j["pictures"] = json!([
            {"self_ref": "#/pictures/0", "children": [], "label": "picture", "content_layer": "body",
             "image": {"uri": "data:image/png;base64,AAAA"}, "captions": []},
            {"self_ref": "#/pictures/1", "children": [], "label": "picture", "content_layer": "body",
             "image": {"uri": "data:image/png;base64,BBBB"}, "captions": [{"$ref": "#/texts/0"}]},
            {"self_ref": "#/pictures/2", "children": [], "label": "picture", "content_layer": "body",
             "captions": []},
        ]);
        let b = blocks(&j);
        assert_eq!(
            b[0],
            ast::para(vec![ast::image(&[], vec![], "data:image/png;base64,AAAA")])
        );
        assert_eq!(
            b[1],
            ast::figure(
                &[],
                ast::caption(vec![ast::plain(vec![
                    ast::str_("A"),
                    ast::space(),
                    ast::str_("duck")
                ])]),
                vec![ast::plain(vec![ast::image(
                    &[],
                    vec![ast::str_("A"), ast::space(), ast::str_("duck")],
                    "data:image/png;base64,BBBB"
                )])]
            )
        );
        assert_eq!(
            b[2],
            ast::para(vec![ast::image(&["docling-placeholder"], vec![], "")])
        );
        assert_eq!(b.len(), 3);
        // Placeholder mode keeps the node, without the pixels.
        let opts = PandocExportOptions {
            image_mode: ImageMode::Placeholder,
            ..Default::default()
        };
        let (s, _) = from_docling_json(&j, &opts).unwrap();
        assert!(!s.contains("base64") && !s.contains("RawBlock"), "{s}");
        assert_eq!(s.matches("docling-placeholder").count(), 3, "{s}");
    }

    /// #833: a caption of formatted runs (an inline group under the caption
    /// item) is the table's caption, its runs decorated, and nothing of it
    /// strays into the body; a multi-run heading with a section under it
    /// is a `Header` followed by the section's blocks; a multi-run list
    /// item with a nested list is `[Plain, BulletList]`; a formatted,
    /// linked single-run caption keeps `Strong` and `Link`.
    #[test]
    fn captions_headings_and_items_keep_their_inline_groups() {
        let mut j = doc(
            &["#/texts/0", "#/tables/0", "#/texts/3", "#/groups/2"],
            json!([
                text(
                    0,
                    "caption",
                    "",
                    json!({"children": [{"$ref": "#/groups/0"}]})
                ),
                text(1, "text", "Table 1:", json!({"formatting": {"bold": true}})),
                text(
                    2,
                    "text",
                    "Multi-run caption",
                    json!({"hyperlink": "https://x.org/c"})
                ),
                text(
                    3,
                    "section_header",
                    "",
                    json!({"level": 1, "children": [{"$ref": "#/groups/1"}, {"$ref": "#/texts/6"}]})
                ),
                text(4, "text", "1.1", json!({"formatting": {"bold": true}})),
                text(5, "text", "Formatted header", json!({})),
                text(6, "text", "Body under header.", json!({})),
                text(
                    7,
                    "list_item",
                    "",
                    json!({"children": [{"$ref": "#/groups/3"}, {"$ref": "#/groups/4"}]})
                ),
                text(8, "text", "Bold run", json!({"formatting": {"bold": true}})),
                text(9, "text", "and tail", json!({})),
                text(10, "list_item", "Nested child", json!({})),
                text(
                    11,
                    "caption",
                    "Linked bold",
                    json!({"formatting": {"bold": true}, "hyperlink": "https://x.org/spec"})
                ),
            ]),
            json!([
                {"self_ref": "#/groups/0", "children": [{"$ref": "#/texts/1"}, {"$ref": "#/texts/2"}], "label": "inline", "name": "group"},
                {"self_ref": "#/groups/1", "children": [{"$ref": "#/texts/4"}, {"$ref": "#/texts/5"}], "label": "inline", "name": "group"},
                {"self_ref": "#/groups/2", "children": [{"$ref": "#/texts/7"}], "label": "list", "name": "list"},
                {"self_ref": "#/groups/3", "children": [{"$ref": "#/texts/8"}, {"$ref": "#/texts/9"}], "label": "inline", "name": "group"},
                {"self_ref": "#/groups/4", "children": [{"$ref": "#/texts/10"}], "label": "list", "name": "list"},
            ]),
            json!([{"self_ref": "#/tables/0", "children": [], "label": "table",
                    "captions": [{"$ref": "#/texts/0"}, {"$ref": "#/texts/11"}],
                    "data": {"num_rows": 1, "num_cols": 1, "table_cells": [
                        {"text": "v", "row_span": 1, "col_span": 1, "start_row_offset_idx": 0, "end_row_offset_idx": 1,
                         "start_col_offset_idx": 0, "end_col_offset_idx": 1, "column_header": false}]}}]),
        );
        j["texts"][0]["children"] = json!([{"$ref": "#/groups/0"}]);
        let b = blocks(&j);
        assert_eq!(b.len(), 4, "{b:?}");
        assert_eq!(b[0]["t"], "Table");
        let caption = &b[0]["c"][1][1][0];
        assert_eq!(caption["t"], "Plain");
        assert_eq!(
            caption["c"][0],
            ast::wrap(
                "Strong",
                vec![ast::str_("Table"), ast::space(), ast::str_("1:")]
            )
        );
        assert_eq!(caption["c"][2]["t"], "Link");
        let linked = caption["c"].as_array().unwrap().last().unwrap();
        assert_eq!(linked["t"], "Link");
        assert_eq!(linked["c"][1][0]["t"], "Strong");
        assert_eq!(b[1]["t"], "Header");
        assert_eq!(b[1]["c"][2][0], ast::wrap("Strong", vec![ast::str_("1.1")]));
        assert_eq!(
            b[2],
            ast::para(vec![
                ast::str_("Body"),
                ast::space(),
                ast::str_("under"),
                ast::space(),
                ast::str_("header.")
            ])
        );
        assert_eq!(b[3]["t"], "BulletList");
        let item = b[3]["c"][0].as_array().unwrap();
        assert_eq!(item.len(), 2);
        assert_eq!(item[0]["t"], "Plain");
        assert_eq!(item[0]["c"][0]["t"], "Strong");
        assert_eq!(item[1]["t"], "BulletList");
    }

    /// #834: the `Image`'s alt text is the picture's description (the
    /// author's alt text) when it has one, the caption only without; the
    /// displayed size (`_display`, inches) becomes `width` / `height`.
    #[test]
    fn pictures_carry_alt_text_and_display_size() {
        let mut j = doc(
            &["#/pictures/0", "#/pictures/1"],
            json!([text(0, "caption", "A duck", json!({})),]),
            json!([]),
            json!([]),
        );
        j["pictures"] = json!([
            {"self_ref": "#/pictures/0", "children": [], "label": "picture", "content_layer": "body",
             "meta": {"description": {"text": "Architecture diagram", "created_by": "alt_text"}},
             "image": {"uri": "data:image/png;base64,AAAA", "_display": [2.5, 1.25]}, "captions": [{"$ref": "#/texts/0"}]},
            {"self_ref": "#/pictures/1", "children": [], "label": "picture", "content_layer": "body",
             "image": {"uri": "data:image/png;base64,BBBB"}, "captions": [{"$ref": "#/texts/0"}]},
        ]);
        let b = blocks(&j);
        let image = &b[0]["c"][2][0]["c"][0];
        assert_eq!(image["t"], "Image");
        assert_eq!(
            image["c"][0],
            json!(["", [], [["width", "2.5in"], ["height", "1.25in"]]])
        );
        assert_eq!(
            image["c"][1],
            json!([
                ast::str_("Architecture"),
                ast::space(),
                ast::str_("diagram")
            ])
        );
        let image = &b[1]["c"][2][0]["c"][0];
        assert_eq!(image["c"][0], json!(["", [], []]));
        assert_eq!(
            image["c"][1],
            json!([ast::str_("A"), ast::space(), ast::str_("duck")])
        );
        assert_eq!(inches(2.5), "2.5in");
        assert_eq!(inches(1.0), "1in");
        assert_eq!(inches(0.33333), "0.3333in");
    }

    /// #835: the markers' number style, delimiter and start.
    #[test]
    fn list_attributes_follow_the_markers() {
        assert_eq!(
            list_attributes(&["a)", "b)"]),
            (1, "LowerAlpha", "OneParen")
        );
        assert_eq!(
            list_attributes(&["iii.", "iv."]),
            (3, "LowerRoman", "Period")
        );
        assert_eq!(
            list_attributes(&["(3)", "(4)"]),
            (3, "Decimal", "TwoParens")
        );
        assert_eq!(
            list_attributes(&["c)", "d)"]),
            (3, "LowerAlpha", "OneParen")
        );
        assert_eq!(list_attributes(&["i."]), (1, "LowerRoman", "Period"));
        assert_eq!(list_attributes(&["v.", "vi."]), (5, "LowerRoman", "Period"));
        assert_eq!(list_attributes(&["x.", "y."]), (24, "LowerAlpha", "Period"));
        assert_eq!(list_attributes(&["B."]), (2, "UpperAlpha", "Period"));
        assert_eq!(list_attributes(&["IV)"]), (4, "UpperRoman", "OneParen"));
        assert_eq!(list_attributes(&["aa)"]), (27, "LowerAlpha", "OneParen"));
        assert_eq!(list_attributes(&["12"]), (12, "Decimal", "DefaultDelim"));
        assert_eq!(list_attributes(&["3."]), (3, "Decimal", "Period"));
        assert_eq!(list_attributes(&["1.2."]), (1, "Decimal", "Period"));
        assert_eq!(list_attributes(&[]), (1, "Decimal", "Period"));
        let j = doc(
            &["#/groups/0"],
            json!([
                text(
                    0,
                    "list_item",
                    "third",
                    json!({"enumerated": true, "marker": "(3)"})
                ),
                text(
                    1,
                    "list_item",
                    "fourth",
                    json!({"enumerated": true, "marker": "(4)"})
                ),
            ]),
            json!([{"self_ref": "#/groups/0", "children": [{"$ref": "#/texts/0"}, {"$ref": "#/texts/1"}], "label": "list", "name": "list"}]),
            json!([]),
        );
        let b = blocks(&j);
        assert_eq!(
            b[0]["c"][0],
            json!([3, {"t": "Decimal"}, {"t": "TwoParens"}])
        );
    }

    /// #836: the document's metadata is Pandoc's `meta`; a reviewer
    /// comment is a `comment-start` / `comment-end` span pair around the
    /// annotated item's inlines, with its author and date, a reply a
    /// further pair; the notes-layer groups never come out as blocks.
    #[test]
    fn metadata_and_comments_reach_the_ast() {
        let mut j = doc(
            &["#/texts/0", "#/groups/0"],
            json!([
                text(
                    0,
                    "text",
                    "The timeout is 30 s.",
                    json!({"comments": [{"$ref": "#/groups/0"}]})
                ),
                text(
                    1,
                    "text",
                    "[author: Reviewer (SR), time: 2026-01-15T12:00:00Z]: Verify it.",
                    json!({"content_layer": "notes"})
                ),
                text(2, "text", "Agreed", json!({"content_layer": "notes"})),
            ]),
            json!([{"self_ref": "#/groups/0", "children": [{"$ref": "#/texts/1"}, {"$ref": "#/texts/2"}], "label": "comment_section", "name": "comment-3", "content_layer": "notes"}]),
            json!([]),
        );
        j["metadata"] = json!({
            "title": "Synthetic Spec", "authors": ["A. Author", "B. Author"], "keywords": ["architecture", "protocol"],
            "language": "en-US", "created": "2026-01-15T10:00:00Z", "subject": "Gateways"
        });
        let (s, _) = from_docling_json(&j, &PandocExportOptions::default()).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["meta"]["title"],
            ast::meta_inlines(vec![
                ast::str_("Synthetic"),
                ast::space(),
                ast::str_("Spec")
            ])
        );
        assert_eq!(v["meta"]["author"]["t"], "MetaList");
        assert_eq!(v["meta"]["author"]["c"].as_array().unwrap().len(), 2);
        assert_eq!(
            v["meta"]["keywords"]["c"][1],
            ast::meta_inlines(vec![ast::str_("protocol")])
        );
        assert_eq!(v["meta"]["lang"], ast::meta_string("en-US"));
        assert_eq!(
            v["meta"]["date"],
            ast::meta_inlines(vec![ast::str_("2026-01-15T10:00:00Z")])
        );
        assert_eq!(v["meta"]["subject"]["t"], "MetaInlines");
        let b = v["blocks"].as_array().unwrap();
        assert_eq!(b.len(), 1, "{b:?}");
        let inl = b[0]["c"].as_array().unwrap();
        assert_eq!(inl[0]["t"], "Span");
        assert_eq!(
            inl[0]["c"][0],
            json!([
                "",
                ["comment-start"],
                [
                    ["comment-id", "3"],
                    ["author", "Reviewer (SR)"],
                    ["date", "2026-01-15T12:00:00Z"]
                ]
            ])
        );
        assert_eq!(
            inl[0]["c"][1],
            json!([ast::str_("Verify"), ast::space(), ast::str_("it.")])
        );
        assert_eq!(
            inl[1]["c"][0],
            json!(["", ["comment-start"], [["comment-id", "3-1"]]])
        );
        assert_eq!(inl[2], ast::str_("The"));
        let last = inl.last().unwrap();
        assert_eq!(
            last["c"][0],
            json!(["", ["comment-end"], [["comment-id", "3-1"]]])
        );
        assert_eq!(
            inl[inl.len() - 2]["c"][0],
            json!(["", ["comment-end"], [["comment-id", "3"]]])
        );
        // With the notes layer on, the comment texts are still not blocks.
        let opts = PandocExportOptions {
            layers: ContentLayers::ALL,
            ..Default::default()
        };
        let (s, _) = from_docling_json(&j, &opts).unwrap();
        assert_eq!(s.matches("Verify").count(), 1, "{s}");
        assert_eq!(
            split_comment("[time: t]: x"),
            (None, Some("t".into()), "x".into())
        );
        assert_eq!(split_comment("bare"), (None, None, "bare".into()));
        assert_eq!(meta_of(&json!({})), json!({}));
    }

    #[test]
    fn text_splits_into_words_spaces_and_line_breaks() {
        assert_eq!(
            text_inlines("  two  words\nnext\u{a0}line "),
            vec![
                ast::str_("two"),
                ast::space(),
                ast::str_("words"),
                ast::line_break(),
                ast::str_("next\u{a0}line"),
            ]
        );
        assert!(text_inlines("   ").is_empty());
    }

    #[test]
    fn headings_paragraphs_and_lists() {
        let j = doc(
            &["#/texts/0", "#/texts/1", "#/texts/2", "#/groups/0"],
            json!([
                text(0, "title", "Ducks", json!({})),
                text(1, "section_header", "Diet", json!({"level": 1})),
                text(
                    2,
                    "text",
                    "Ducks eat plants.",
                    json!({"formatting": {"bold": true}, "hyperlink": "https://x.org"})
                ),
                text(
                    3,
                    "list_item",
                    "seeds",
                    json!({"enumerated": true, "marker": "3."})
                ),
                text(
                    4,
                    "list_item",
                    "insects",
                    json!({"enumerated": true, "marker": "4."})
                ),
            ]),
            json!([{"self_ref": "#/groups/0", "children": [{"$ref": "#/texts/3"}, {"$ref": "#/texts/4"}], "label": "list", "name": "list"}]),
            json!([]),
        );
        let b = blocks(&j);
        assert_eq!(b[0], ast::header(1, vec![ast::str_("Ducks")]));
        assert_eq!(b[1], ast::header(2, vec![ast::str_("Diet")]));
        // formatting inside, the link around it all
        assert_eq!(b[2]["t"], "Para");
        assert_eq!(b[2]["c"][0]["t"], "Link");
        assert_eq!(b[2]["c"][0]["c"][1][0]["t"], "Strong");
        assert_eq!(b[3]["t"], "OrderedList");
        assert_eq!(b[3]["c"][0][0], 3, "start from the first marker");
        assert_eq!(
            b[3]["c"][1],
            json!([
                [ast::plain(vec![ast::str_("seeds")])],
                [ast::plain(vec![ast::str_("insects")])]
            ])
        );
    }

    /// #848: the two heading projections at their bounds — docling's
    /// title-reserving `n + 1` (missing → 2, 0 → 1, capped at 6) and the
    /// source's own `n` (missing / 0 → 1, 7 → 6); the title is `Header 1`
    /// under both.
    #[test]
    fn heading_depths_under_both_projections() {
        let j = doc(
            &[
                "#/texts/0",
                "#/texts/1",
                "#/texts/2",
                "#/texts/3",
                "#/texts/4",
                "#/texts/5",
                "#/texts/6",
                "#/texts/7",
            ],
            json!([
                text(0, "title", "T", json!({})),
                text(1, "section_header", "d1", json!({"level": 1})),
                text(2, "section_header", "d2", json!({"level": 2})),
                text(3, "section_header", "d5", json!({"level": 5})),
                text(4, "section_header", "d6", json!({"level": 6})),
                text(5, "section_header", "none", json!({})),
                text(6, "section_header", "zero", json!({"level": 0})),
                text(7, "section_header", "seven", json!({"level": 7})),
            ]),
            json!([]),
            json!([]),
        );
        let depths = |heading_depth: HeadingDepth| -> Vec<u64> {
            let options = PandocExportOptions {
                heading_depth,
                ..Default::default()
            };
            let (s, _) = from_docling_json(&j, &options).unwrap();
            let v: Value = serde_json::from_str(&s).unwrap();
            v["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["c"][0].as_u64().unwrap())
                .collect()
        };
        assert_eq!(depths(HeadingDepth::Docling), [1, 2, 3, 6, 6, 2, 1, 6]);
        assert_eq!(depths(HeadingDepth::Source), [1, 1, 2, 5, 6, 1, 1, 6]);
        assert_eq!("Source".parse::<HeadingDepth>(), Ok(HeadingDepth::Source));
        assert!("deep".parse::<HeadingDepth>().is_err());
    }

    /// #852: a Markdown link in an item's text (the flat chain's
    /// hyperlink) is a `Link`; brackets that are not one stay text.
    #[test]
    fn markdown_links_in_text_become_links() {
        assert_eq!(
            text_inlines("Jump to [target](#target-section) now [x] (y)"),
            vec![
                ast::str_("Jump"),
                ast::space(),
                ast::str_("to"),
                ast::space(),
                ast::link(vec![ast::str_("target")], "#target-section"),
                ast::space(),
                ast::str_("now"),
                ast::space(),
                ast::str_("[x]"),
                ast::space(),
                ast::str_("(y)"),
            ]
        );
        assert_eq!(
            text_inlines("[a](https://x.org/a)[b](https://x.org/b)"),
            vec![
                ast::link(vec![ast::str_("a")], "https://x.org/a"),
                ast::link(vec![ast::str_("b")], "https://x.org/b"),
            ]
        );
        assert_eq!(
            text_inlines("see ![alt](img.png) here"),
            vec![
                ast::str_("see"),
                ast::space(),
                ast::str_("![alt](img.png)"),
                ast::space(),
                ast::str_("here"),
            ]
        );
    }

    /// #851: a run's `_revision` is Pandoc's insertion / deletion span,
    /// and an explicit accept / reject view drops the comment spans.
    #[test]
    fn revisions_become_spans_and_views_drop_comments() {
        let j = doc(
            &["#/texts/0", "#/texts/1"],
            json!([
                text(
                    0,
                    "text",
                    "old",
                    json!({"_revision": {"kind": "deletion", "author": "Fixture", "date": "2025-01-01T00:00:00Z"}})
                ),
                text(
                    1,
                    "text",
                    "Commented",
                    json!({"comments": [{"$ref": "#/groups/0"}]})
                ),
                text(
                    2,
                    "text",
                    "[author: Fixture, time: 2025-01-01T00:00:00Z]: Review note",
                    json!({"content_layer": "notes"})
                ),
            ]),
            json!([{"self_ref": "#/groups/0", "children": [{"$ref": "#/texts/2"}], "label": "comment_section", "name": "comment-3", "content_layer": "notes"}]),
            json!([]),
        );
        let b = blocks(&j);
        assert_eq!(
            b[0],
            ast::para(vec![ast::span(
                &["deletion"],
                &[
                    ("author".into(), "Fixture".into()),
                    ("date".into(), "2025-01-01T00:00:00Z".into())
                ],
                vec![ast::str_("old")]
            )])
        );
        assert_eq!(
            b[1]["c"][0]["t"], "Span",
            "comments written without a policy"
        );
        let accept = PandocExportOptions {
            track_changes: Some(TrackChanges::Accept),
            ..Default::default()
        };
        let (s, _) = from_docling_json(&j, &accept).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["blocks"][1], ast::para(vec![ast::str_("Commented")]));
    }

    /// #850: a heading numbered by the backend (`_number`) writes its
    /// authored text alone; text that merely starts with digits is kept.
    #[test]
    fn numbered_headings_write_the_authored_text() {
        let j = doc(
            &["#/texts/0", "#/texts/1", "#/texts/2"],
            json!([
                text(
                    0,
                    "section_header",
                    "1.1 2026 Results",
                    json!({"level": 2, "_number": "1.1"})
                ),
                text(1, "section_header", "2026 Results", json!({"level": 1})),
                text(
                    2,
                    "section_header",
                    "3 Odd",
                    json!({"level": 1, "_number": "4"})
                ),
            ]),
            json!([]),
            json!([]),
        );
        let b = blocks(&j);
        assert_eq!(
            b[0],
            ast::header(
                3,
                vec![ast::str_("2026"), ast::space(), ast::str_("Results")]
            )
        );
        assert_eq!(
            b[1],
            ast::header(
                2,
                vec![ast::str_("2026"), ast::space(), ast::str_("Results")]
            )
        );
        // A `_number` the text does not open with is left alone.
        assert_eq!(
            b[2],
            ast::header(2, vec![ast::str_("3"), ast::space(), ast::str_("Odd")])
        );
    }

    /// #849: a bookmarked heading carries its identifier, a bookmark in
    /// body text an empty anchor span, and an internal link's `#id` target
    /// is written as is.
    #[test]
    fn anchors_become_header_ids_and_anchor_spans() {
        let j = doc(
            &["#/texts/0", "#/texts/1", "#/texts/2"],
            json!([
                text(
                    0,
                    "section_header",
                    "Target section",
                    json!({"level": 1, "_anchors": ["target-section", "Alias"]})
                ),
                text(1, "text", "Jump", json!({"hyperlink": "#target-section"})),
                text(2, "text", "Body mark", json!({"_anchors": ["BodyMark"]})),
            ]),
            json!([]),
            json!([]),
        );
        let b = blocks(&j);
        assert_eq!(
            b[0],
            ast::header_with_id(
                2,
                "target-section",
                vec![
                    ast::anchor("Alias"),
                    ast::str_("Target"),
                    ast::space(),
                    ast::str_("section")
                ]
            )
        );
        assert_eq!(
            b[1],
            ast::para(vec![ast::link(vec![ast::str_("Jump")], "#target-section")])
        );
        assert_eq!(
            b[2],
            ast::para(vec![
                ast::anchor("BodyMark"),
                ast::str_("Body"),
                ast::space(),
                ast::str_("mark")
            ])
        );
        assert_eq!(auto_identifier("Target section"), "target-section");
        assert_eq!(auto_identifier("2026 Results"), "results");
        assert_eq!(auto_identifier("1.1 Été & hiver!"), "été-hiver");
        assert_eq!(auto_identifier("***"), "section");
    }

    #[test]
    fn code_formulas_checkboxes_and_unmapped_labels() {
        let j = doc(
            &["#/texts/0", "#/texts/1", "#/texts/2", "#/texts/3"],
            json!([
                text(0, "code", "print(1)", json!({"code_language": "Python"})),
                text(1, "formula", "E = mc^2", json!({})),
                text(2, "checkbox_selected", "done", json!({})),
                text(3, "reference", "[1] Duck, D. (2020).", json!({})),
            ]),
            json!([]),
            json!([]),
        );
        let b = blocks(&j);
        assert_eq!(b[0], ast::code_block(Some("python"), "print(1)"));
        assert_eq!(b[1], ast::para(vec![ast::math(true, "E = mc^2")]));
        assert_eq!(b[2]["c"][0], ast::str_("☒"));
        assert_eq!(b[3]["t"], "Div");
        assert_eq!(b[3]["c"][0][1], json!(["docling-reference"]));
    }

    #[test]
    fn tables_keep_spans_header_rows_and_captions() {
        let cell = |r0: usize, c0: usize, rs: usize, cs: usize, t: &str, header: bool| {
            json!({"text": t, "row_span": rs, "col_span": cs,
                   "start_row_offset_idx": r0, "end_row_offset_idx": r0 + rs,
                   "start_col_offset_idx": c0, "end_col_offset_idx": c0 + cs,
                   "column_header": header})
        };
        let j = doc(
            &["#/tables/0"],
            json!([text(0, "caption", "Table 1: Ducks", json!({}))]),
            json!([]),
            json!([{"self_ref": "#/tables/0", "children": [{"$ref": "#/texts/0"}], "label": "table",
                    "captions": [{"$ref": "#/texts/0"}],
                    "data": {"num_rows": 2, "num_cols": 2, "table_cells": [
                        cell(0, 0, 1, 2, "Species", true),
                        cell(1, 0, 1, 1, "Mallard", false),
                        cell(1, 1, 1, 1, "Anas", false)]}}]),
        );
        let b = blocks(&j);
        assert_eq!(b.len(), 1, "the caption is rendered by its table only");
        let t = &b[0];
        assert_eq!(t["t"], "Table");
        assert_eq!(
            t["c"][1][1][0],
            ast::plain(vec![
                ast::str_("Table"),
                ast::space(),
                ast::str_("1:"),
                ast::space(),
                ast::str_("Ducks")
            ])
        );
        assert_eq!(t["c"][2].as_array().unwrap().len(), 2);
        let head = t["c"][3][1].as_array().unwrap();
        assert_eq!(head.len(), 1);
        assert_eq!(head[0][1][0][3], 2, "colspan");
        assert_eq!(t["c"][4][0][3].as_array().unwrap().len(), 1, "one body row");
    }

    #[test]
    fn api_version_is_checked() {
        for ok in ["1.23", "1.23.1", "1.23.1.1"] {
            assert!(check_api_version(ok).is_ok(), "{ok}");
        }
        for bad in ["1.22", "2.0", "1", "1.23.2", "1.23.1.2", "x.y", ""] {
            let e = check_api_version(bad).unwrap_err();
            assert!(e.to_string().contains("only 1.23"), "{bad}: {e}");
        }
        let err = to_pandoc(
            &DoclingDocument::new("t"),
            &PandocExportOptions {
                api_version: Some("1.22".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            PandocError::UnsupportedApiVersion {
                requested: "1.22".into()
            }
        );
    }

    #[test]
    #[cfg(feature = "preserve-order")] // field-order assertions (#970)
    fn empty_document_is_a_valid_pandoc_document() {
        let (s, artifacts) =
            to_pandoc(&DoclingDocument::new("e"), &PandocExportOptions::default()).unwrap();
        assert!(artifacts.is_empty());
        assert_eq!(
            s,
            r#"{"pandoc-api-version":[1,23,1,1],"meta":{},"blocks":[]}"#
        );
        assert_eq!(s, DoclingDocument::new("e").export_to_pandoc_json());
    }
}
