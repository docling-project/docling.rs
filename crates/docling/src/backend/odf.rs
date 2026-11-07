//! OpenDocument backend (`.odt`/`.ods`/`.odp`) — a port of docling's
//! `OpenDocument*` backends. ODF is a ZIP whose `content.xml` holds the body;
//! `styles.xml` plus `content.xml`'s automatic styles define text/paragraph/list
//! styles. Paragraph styles map to Title/Subtitle/Heading; `<text:h>` maps to a
//! heading by outline level; runs (`<text:span>`) carry bold/italic/strike/sub-
//! superscript resolved through the style parent chain; lists nest by depth.
//!
//! Two sibling envelopes ride the same parser (#215, docling.rs extensions —
//! docling reaches either only through LibreOffice):
//!
//! - **Flat ODF** (`.fodt`/`.fods`/`.fodp`): the same document XML uncompressed
//!   in a single `<office:document>` file — styles live in the same DOM and
//!   embedded charts are inline `<draw:object>` children instead of package
//!   parts.
//! - **StarOffice / OpenOffice 1.x XML** (`.sxw`/`.sxi`/`.sxc`, templates
//!   `.stw`/`.sti`/`.stc`, master `.sxg`): the ODF predecessor schema. The
//!   element vocabulary is nearly identical under different namespace URIs —
//!   and this parser matches local names only — so support is a mapping layer:
//!   `<office:body>` is the content container itself (no `office:text` /
//!   `office:presentation` wrapper), lists are `<text:ordered-list>` /
//!   `<text:unordered-list>`, tabs are `<text:tab-stop>`, headings carry
//!   `text:level`, and strike/underline sit on `style:text-crossing-out` /
//!   `style:text-underline`.

use std::collections::{HashMap, HashSet, VecDeque};

use roxmltree::{Document, Node as XmlNode};

use crate::backend::images::ImagePolicy;
use crate::backend::markdown::escape_text;
use crate::backend::ooxml::Package;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{
    inline_paragraph_node, DoclingDocument, InlineRun, ListItemDclx, Node, PictureDescription,
    PictureImage, Script, Table, TableStructure, TrackChanges,
};

pub struct OdfBackend;

#[derive(Default, Clone, Copy, PartialEq)]
pub(super) struct Fmt {
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) strike: bool,
    pub(super) underline: bool,
    pub(super) script: u8, // 0 none, 1 sub, 2 super
}

impl Fmt {
    /// The structured [`InlineRun`] for a text segment under this formatting —
    /// the ODF analogue of the DOCX backend's `to_inline_run` (underline and
    /// sub/superscript have no Markdown marker, so they only survive here).
    fn to_inline_run(self, text: &str) -> InlineRun {
        InlineRun {
            text: text.to_string(),
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strike: self.strike,
            script: match self.script {
                1 => Script::Sub,
                2 => Script::Super,
                _ => Script::Baseline,
            },
            code: false,
            formula: false,
        }
    }
}

#[derive(Default, Clone)]
struct StyleInfo {
    parent: Option<String>,
    display_name: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    strike: Option<bool>,
    underline: Option<bool>,
    script: Option<u8>,
    /// A paragraph style's `style:list-style-name` (#826): the list its
    /// paragraphs are numbered with, no `<text:list>` around them; `Some("")`
    /// cancels an inherited one. `list_level` is ODF 1.3's `style:list-level`.
    list_style: Option<String>,
    list_level: Option<i64>,
}

/// One list level's rendering: bullet vs numbered, its `start-value`, and the
/// prefix/suffix wrapping an enumerated marker (`num-prefix`/`num-suffix`).
#[derive(Default, Clone)]
pub(super) struct OdfLevel {
    pub(super) numbered: bool,
    pub(super) start: i64,
    pub(super) prefix: String,
    pub(super) suffix: String,
    /// `style:num-format` (#835): `1`, `a`, `A`, `i`, `I` (or empty).
    pub(super) format: String,
}

/// An ODT list item's marker for the item tree — docling's
/// `_odf_list_marker` with the level's number format and prefix (#835):
/// `{num-prefix}{counter in num-format}{num-suffix}`, `.` after a bare
/// counter — `a)`, `iii.`, `(3)`. docling writes the decimal counter and
/// the suffix alone, so `iii.` was `3.` and `(3)` was `3)`, and the Pandoc
/// export could not tell a Roman list from a decimal one; the DOCX backend
/// renders the same lists through their `numFmt` and `lvlText`, so this is
/// the twin's marker. The flat Markdown keeps docling's counter.
pub(super) fn odf_marker(counter: i64, level: Option<&OdfLevel>) -> String {
    let (prefix, suffix, format) = level
        .map(|l| (l.prefix.as_str(), l.suffix.as_str(), l.format.as_str()))
        .unwrap_or_default();
    let num_fmt = match format {
        "a" => Some("lowerLetter"),
        "A" => Some("upperLetter"),
        "i" => Some("lowerRoman"),
        "I" => Some("upperRoman"),
        _ => None,
    };
    let n = super::docx::format_enum_counter(counter, num_fmt);
    if prefix.is_empty() && suffix.is_empty() {
        format!("{n}.")
    } else {
        format!("{prefix}{n}{suffix}")
    }
}

/// List style name → level (1-based) → level rendering.
type ListStyles = HashMap<String, HashMap<i64, OdfLevel>>;

/// An embedded chart object: its docling classification (`bar_chart`, …) and
/// data grid, keyed in [`Styles::charts`] by the object name a `<draw:object>`
/// references (e.g. `Object 1`).
#[derive(Clone)]
pub(super) struct ChartInfo {
    pub(super) kind: String,
    pub(super) table: Table,
}

pub(super) struct Styles {
    map: HashMap<String, StyleInfo>,
    lists: ListStyles,
    /// Embedded chart objects by name (`Object 1` → chart). Empty for documents
    /// with no charts.
    pub(super) charts: HashMap<String, ChartInfo>,
    /// The package's decodable bitmap parts by name (`Pictures/x.png` →
    /// pixels), read for the JSON item tree: docling's `_add_odf_images`
    /// adds a picture only when PIL can open the part, and writes its
    /// `image` payload; the flat nodes carry the same pixels.
    pub(super) images: HashMap<String, PictureImage>,
    /// The package's part names (`Pictures/1000.png`, …); `None` for flat ODF,
    /// where every image is inline `<office:binary-data>`. Decides whether a
    /// `draw:image` `xlink:href` is an embedded picture or an *external*
    /// reference (docling#4015, 2.120.3).
    parts: Option<HashSet<String>>,
    /// Whether external `http(s)` image references may be fetched — the
    /// converter's image policy at the `Remote` tier (docling's
    /// `enable_remote_fetch`; #646 adds the host allow-list and the size
    /// cap). Local filesystem paths are never read: a document-supplied
    /// path must not turn into an arbitrary local-file read (the advisory
    /// behind #4015).
    policy: ImagePolicy,
    /// The `drawing-page` styles that hide a slide
    /// (`presentation:visibility="hidden"` on their
    /// `style:drawing-page-properties`; #753, docling#4767).
    hidden_pages: HashSet<String>,
    /// The body's reviewer comments (#829) by their `<office:annotation>`
    /// node, each the index of its [`Node::CommentSection`] in the flat
    /// stream — what a [`Node::Commented`] wrapper on the annotated node
    /// points at. Set per `<office:text>` before its walk; empty otherwise.
    comment_slots: HashMap<roxmltree::NodeId, usize>,
}

impl DeclarativeBackend for OdfBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        convert_odf(source, &ImagePolicy::default(), None)
    }
}

/// Convert an ODF / OO1.x / flat-ODF document. `policy` at `Remote` allows external
/// `http(s)` `draw:image` references to be fetched (bounded, SSRF-guarded);
/// off, an external image reference yields no picture at all — exactly
/// docling's `ImageResourceLoader` with remote fetch disabled (#4015).
/// Whether the package manifest marks `content.xml` as encrypted.
fn manifest_encrypts_content(manifest: &str) -> bool {
    let Ok(dom) = Document::parse(manifest) else {
        return false;
    };
    dom.descendants().any(|e| {
        e.tag_name().name() == "file-entry"
            && e.attributes()
                .any(|a| a.name() == "full-path" && a.value() == "content.xml")
            && e.children()
                .any(|c| c.tag_name().name() == "encryption-data")
    })
}

pub(crate) fn convert_odf(
    source: &SourceDocument,
    policy: &ImagePolicy,
    track_changes: Option<TrackChanges>,
) -> Result<DoclingDocument, ConversionError> {
    // The ODF reader gives the accepted view (#851): the text as the
    // document shows it with its `text:tracked-changes` region skipped —
    // insertions in place, deletions (kept out of line in that region)
    // left out. The other views are not read, and are refused rather than
    // answered with this one.
    if let Some(mode @ (TrackChanges::Reject | TrackChanges::All)) = track_changes {
        return Err(ConversionError::UnsupportedOption(format!(
            "odf: track_changes={mode} is not implemented by the ODF reader (its text is the \
             accepted view); accept is"
        )));
    }
    {
        let mut pkg = Package::open(&source.bytes);
        // A password-protected ODF package keeps its manifest in the clear
        // and lists each encrypted part's `manifest:encryption-data` (ODF
        // 1.2 part 3, 3.8); its content.xml is ciphertext, which used to
        // read as "no content.xml" (#624).
        if let Some(manifest) = pkg.as_mut().and_then(|p| p.read("META-INF/manifest.xml")) {
            if manifest_encrypts_content(&manifest) {
                return Err(crate::backend::offcrypto::unsupported(
                    source.format.as_str(),
                    "ODF package encryption",
                ));
            }
        }
        let (content, styles_xml) = match pkg.as_mut() {
            Some(pkg) => (
                pkg.read("content.xml")
                    .ok_or_else(|| ConversionError::Parse("odf: no content.xml".into()))?,
                pkg.read("styles.xml").unwrap_or_default(),
            ),
            // Flat ODF (#215): the whole document is one uncompressed XML file;
            // `office:styles` lives in the same DOM, so there is no styles part.
            None => (source.text()?.to_string(), String::new()),
        };

        let content_dom =
            Document::parse(&content).map_err(|e| ConversionError::with_source("odf", e))?;
        if pkg.is_none() {
            let root = content_dom.root_element().tag_name().name();
            if root != "document" && root != "document-content" {
                return Err(ConversionError::Parse(
                    "odf: not a zip package or flat office XML".into(),
                ));
            }
        }
        let styles_dom = Document::parse(&styles_xml).ok();
        let mut styles = parse_styles(&content_dom, styles_dom.as_ref());
        styles.policy = policy.clone();
        if let Some(pkg) = pkg.as_mut() {
            styles.parts = Some(pkg.names().map(str::to_string).collect());
            styles.charts = load_charts(pkg, &content_dom, &styles);
            let names: Vec<String> = pkg
                .names()
                .filter(|n| n.starts_with("Pictures/"))
                .map(str::to_string)
                .collect();
            for name in names {
                if let Some(img) = pkg
                    .read_bytes(&name)
                    .and_then(|b| crate::backend::ooxml::picture_image(&name, b))
                {
                    styles.images.insert(name, img);
                }
            }
        }

        let mut doc = DoclingDocument::new(&source.name);
        // `<office:body>` is always a direct child of the root. A descendants()
        // scan would trip on flat ODF, where the in-DOM styles can contain a
        // `<table:body>` template element earlier in document order.
        let Some(body) = content_dom
            .root_element()
            .children()
            .find(|n| n.has_tag_name("body"))
        else {
            return Ok(doc);
        };
        // The JSON serializes docling's item tree, built by a call-for-call
        // port of upstream's walk ([`super::odf_tree`]) over the same
        // container the flat walk reads; the flat nodes stay the source for
        // Markdown / DocLang / LaTeX.
        let mut built: Option<super::odf_tree::Built> = None;
        let mut wrapped = false;
        for office in body.children().filter(XmlNode::is_element) {
            match office.tag_name().name() {
                "text" => {
                    let comments = annotations(office);
                    styles.comment_slots = comment_slots(&comments);
                    let furniture = master_page_parts(&content_dom, styles_dom.as_ref());
                    walk_text(office, &styles, &mut doc);
                    push_master_page_furniture(&furniture, &styles, &mut doc);
                    push_note_furniture(office, &mut doc);
                    push_comment_sections(&comments, &mut doc);
                    built = Some(super::odf_tree::build_text(
                        office, &styles, &comments, &furniture,
                    ));
                }
                "spreadsheet" => {
                    walk_spreadsheet(office, &styles, &mut doc);
                    built = Some(super::odf_tree::build_spreadsheet(office, &styles));
                }
                "presentation" => {
                    walk_presentation(office, &styles, &mut doc);
                    built = Some(super::odf_tree::build_presentation(office, &styles));
                }
                _ => continue,
            }
            wrapped = true;
        }
        if !wrapped {
            // OpenOffice 1.x (#215): `<office:body>` *is* the content container.
            // The document kind comes from the root's `office:class`; a body
            // holding `<draw:page>`s is a presentation regardless.
            let class = attr(content_dom.root_element(), "class");
            if class == Some("presentation") || body.children().any(|c| c.has_tag_name("page")) {
                walk_presentation(body, &styles, &mut doc);
                built = Some(super::odf_tree::build_presentation(body, &styles));
            } else if class == Some("spreadsheet") {
                walk_spreadsheet(body, &styles, &mut doc);
                built = Some(super::odf_tree::build_spreadsheet(body, &styles));
            } else {
                let comments = annotations(body);
                styles.comment_slots = comment_slots(&comments);
                let furniture = master_page_parts(&content_dom, styles_dom.as_ref());
                walk_text(body, &styles, &mut doc);
                push_master_page_furniture(&furniture, &styles, &mut doc);
                push_note_furniture(body, &mut doc);
                push_comment_sections(&comments, &mut doc);
                built = Some(super::odf_tree::build_text(
                    body, &styles, &comments, &furniture,
                ));
            }
        }
        if let Some(built) = built {
            // A spreadsheet's sheets are pages of the JSON, sized like
            // docling's `_find_page_size`; the markers feed the export's
            // `pages` map and nothing else (DocLang and Markdown ignore them).
            for (page_no, width, height) in built.pages {
                doc.push(Node::PageInfo {
                    page_no,
                    width: width as f32,
                    height: height as f32,
                });
            }
            doc.tree = Some(built.tree);
        }
        Ok(doc)
    }
}

/// Whether an element is a list container: ODF's `<text:list>` or OpenOffice
/// 1.x's `<text:ordered-list>` / `<text:unordered-list>` (#215).
pub(super) fn is_list_tag(name: &str) -> bool {
    matches!(name, "list" | "ordered-list" | "unordered-list")
}

/// Whether an element is a transparent block container the walkers descend
/// into as if its children were body blocks: a `<text:section>`, or (#823,
/// docling#4750) an index — `<text:table-of-content>`, the alphabetical /
/// illustration / table / object / user indexes and a bibliography — and
/// the `<text:index-body>` / `<text:index-title>` it holds. The index's
/// `*-source` child is configuration (its `index-title-template` is not
/// rendered text) and is not a block tag, so the walk never reads it. The
/// index body's entries are the TOC lines docling keeps for a `.docx`; and
/// LibreOffice, converting a Word file whose TOC field is never closed,
/// wraps the rest of the document — headings, paragraphs, tables — inside
/// the index body, where every one of them used to vanish.
pub(super) fn is_transparent_container(name: &str) -> bool {
    matches!(
        name,
        "section"
            | "table-of-content"
            | "alphabetical-index"
            | "illustration-index"
            | "table-index"
            | "object-index"
            | "user-index"
            | "bibliography"
            | "index-body"
            | "index-title"
    )
}

// ---------------------------------------------------------------- styles

fn parse_styles(content: &Document, styles: Option<&Document>) -> Styles {
    let mut map = HashMap::new();
    let mut lists = HashMap::new();
    let mut hidden_pages = HashSet::new();
    for dom in [Some(content), styles].into_iter().flatten() {
        for s in dom.descendants() {
            match s.tag_name().name() {
                "style" => {
                    if let Some(name) = attr(s, "name") {
                        map.insert(name.to_string(), style_info(s));
                        let hides_page = attr(s, "family") == Some("drawing-page")
                            && s.children().any(|c| {
                                c.has_tag_name("drawing-page-properties")
                                    && attr(c, "visibility") == Some("hidden")
                            });
                        if hides_page {
                            hidden_pages.insert(name.to_string());
                        }
                    }
                }
                "list-style" => {
                    if let Some(name) = attr(s, "name") {
                        let mut levels = HashMap::new();
                        for lv in s.children().filter(XmlNode::is_element) {
                            let level: i64 =
                                attr(lv, "level").and_then(|v| v.parse().ok()).unwrap_or(1);
                            let numbered = lv.tag_name().name() == "list-level-style-number";
                            let start = attr(lv, "start-value")
                                .and_then(|v| v.parse().ok())
                                .map(|n: i64| n.max(1))
                                .unwrap_or(1);
                            let prefix = attr(lv, "num-prefix").unwrap_or("").to_string();
                            let suffix = attr(lv, "num-suffix").unwrap_or("").to_string();
                            let format = attr(lv, "num-format").unwrap_or("").to_string();
                            levels.insert(
                                level,
                                OdfLevel {
                                    numbered,
                                    start,
                                    prefix,
                                    suffix,
                                    format,
                                },
                            );
                        }
                        lists.insert(name.to_string(), levels);
                    }
                }
                _ => {}
            }
        }
    }
    Styles {
        map,
        lists,
        charts: HashMap::new(),
        images: HashMap::new(),
        parts: None,
        policy: ImagePolicy::default(),
        hidden_pages,
        comment_slots: HashMap::new(),
    }
}

/// Whether a `<draw:page>` is a hidden slide: its `draw:style-name` names a
/// `drawing-page` style with `presentation:visibility="hidden"` — docling's
/// `_is_slide_hidden` (#753, docling#4767). Such a slide is not presented;
/// it is kept on the invisible layer, like a hidden ODS sheet.
pub(super) fn slide_is_hidden(page: XmlNode, styles: &Styles) -> bool {
    attr(page, "style-name").is_some_and(|name| styles.hidden_pages.contains(name))
}

/// Whether a `<text:section>` (or an index, #823) is hidden
/// (`text:display="none"`, Writer's Format > Sections > Hide): its content
/// is kept on the invisible layer (#753, docling#4860).
pub(super) fn section_is_hidden(section: XmlNode) -> bool {
    attr(section, "display") == Some("none")
}

/// Load every embedded chart object a `<draw:object>` references. Each object is
/// a sub-package part `{name}/content.xml` holding a `<chart:chart>`; we keep its
/// classification and data `<table:table>` (parsed like any ODF table). Objects
/// that are not charts (or fail to parse) are skipped.
fn load_charts(
    pkg: &mut Package,
    content: &Document,
    styles: &Styles,
) -> HashMap<String, ChartInfo> {
    let mut charts = HashMap::new();
    for obj in content.descendants().filter(|n| n.has_tag_name("object")) {
        let Some(href) = attr(obj, "href") else {
            continue;
        };
        let name = href.trim_start_matches("./");
        if charts.contains_key(name) {
            continue;
        }
        let Some(xml) = pkg.read(&format!("{name}/content.xml")) else {
            continue;
        };
        let Ok(dom) = Document::parse(&xml) else {
            continue;
        };
        if !dom.descendants().any(|n| n.has_tag_name("chart")) {
            continue;
        }
        // Classification per docling: the first mapped `chart:class` on a
        // `<chart:chart>` (never the `<office:chart>` wrapper, which carries no
        // class), else on a `<chart:series>`, else "other_chart".
        let kind = dom
            .descendants()
            .filter(|n| n.has_tag_name("chart"))
            .find_map(|n| attr(n, "class").and_then(chart_kind))
            .or_else(|| {
                dom.descendants()
                    .filter(|n| n.has_tag_name("series"))
                    .find_map(|n| attr(n, "class").and_then(chart_kind))
            })
            .unwrap_or("other_chart")
            .to_string();
        let Some(table_node) = dom.descendants().find(|n| n.has_tag_name("table")) else {
            continue;
        };
        if let Some(table) = parse_table(table_node, styles) {
            charts.insert(name.to_string(), ChartInfo { kind, table });
        }
    }
    charts
}

/// Map an ODF `chart:class` (`chart:bar`, `chart:line`, …) to docling's
/// `PictureClassificationLabel` chart kind — exactly docling's
/// `_ODF_CHART_CLASS_TO_PICTURE_CLASSIFICATION` (unmapped classes fall back
/// to `other_chart` at the call site).
pub(super) fn chart_kind(class: &str) -> Option<&'static str> {
    match class {
        "chart:bar" => Some("bar_chart"),
        "chart:line" => Some("line_chart"),
        "chart:circle" | "chart:pie" => Some("pie_chart"),
        "chart:scatter" => Some("scatter_plot"),
        _ => None,
    }
}

fn style_info(s: XmlNode) -> StyleInfo {
    let mut info = StyleInfo {
        parent: attr(s, "parent-style-name").map(str::to_string),
        display_name: attr(s, "display-name").map(str::to_string),
        list_style: attr(s, "list-style-name").map(str::to_string),
        list_level: attr(s, "list-level").and_then(|v| v.parse().ok()),
        ..Default::default()
    };
    // OO1.x (#215) folds everything into one `<style:properties>` element.
    if let Some(tp) = s
        .children()
        .find(|c| c.has_tag_name("text-properties") || c.has_tag_name("properties"))
    {
        info.bold = attr(tp, "font-weight").map(is_bold);
        info.italic = attr(tp, "font-style").map(|v| v == "italic" || v == "oblique");
        // OO1.x (#215) names these `style:text-crossing-out` /
        // `style:text-underline` (values like `single`; `none` clears).
        info.strike = attr(tp, "text-line-through-style")
            .or_else(|| attr(tp, "text-crossing-out"))
            .map(|v| v != "none");
        info.underline = attr(tp, "text-underline-style")
            .or_else(|| attr(tp, "text-underline"))
            .map(|v| v != "none");
        info.script = attr(tp, "text-position").map(|v| {
            if v.starts_with("super") {
                2
            } else if v.starts_with("sub") {
                1
            } else {
                0
            }
        });
    }
    info
}

fn is_bold(v: &str) -> bool {
    v == "bold" || v.parse::<i32>().map(|n| n >= 600).unwrap_or(false)
}

/// Resolve a text/paragraph style's formatting through its parent chain.
pub(super) fn resolve_fmt(styles: &Styles, name: Option<&str>, base: Fmt) -> Fmt {
    let mut fmt = base;
    let mut chain = Vec::new();
    let mut cur = name.map(str::to_string);
    let mut seen = std::collections::HashSet::new();
    while let Some(n) = cur {
        if !seen.insert(n.clone()) {
            break;
        }
        if let Some(info) = styles.map.get(&n) {
            chain.push(info.clone());
            cur = info.parent.clone();
        } else {
            break;
        }
    }
    // Apply parent-first so the most-derived style wins.
    for info in chain.into_iter().rev() {
        if let Some(b) = info.bold {
            fmt.bold = b;
        }
        if let Some(i) = info.italic {
            fmt.italic = i;
        }
        if let Some(s) = info.strike {
            fmt.strike = s;
        }
        if let Some(u) = info.underline {
            fmt.underline = u;
        }
        if let Some(sc) = info.script {
            fmt.script = sc;
        }
    }
    fmt
}

/// The list a paragraph is numbered with through its *paragraph style*
/// (#826): the `style:list-style-name` of the style, or of the nearest
/// ancestor style setting one (an empty value cancels the inheritance), and
/// the 1-based level (`style:list-level`, 1 without it). That is how Writer
/// numbers a "List Bullet" paragraph that no `<text:list>` wraps — the way
/// Word keeps a list in the style's `w:numPr`, which docling's DOCX backend
/// reads; the ODT one only knew `<text:list>` and printed such paragraphs
/// as body text (the reporter's corpus: two thirds of its ODT files, every
/// one of their 26 627 list items).
pub(super) fn paragraph_list_style(styles: &Styles, name: Option<&str>) -> Option<(String, i64)> {
    let mut cur = name.map(str::to_string);
    let mut seen = HashSet::new();
    while let Some(n) = cur {
        if !seen.insert(n.clone()) {
            break;
        }
        let info = styles.map.get(&n)?;
        if let Some(list) = &info.list_style {
            if list.is_empty() {
                return None;
            }
            return Some((list.clone(), info.list_level.unwrap_or(1).max(1)));
        }
        cur = info.parent.clone();
    }
    None
}

/// The set of style names a paragraph resolves to (own, parent, display).
pub(super) fn paragraph_style_names(styles: &Styles, name: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(n) = name {
        out.push(n.to_string());
        if let Some(info) = styles.map.get(n) {
            if let Some(p) = &info.parent {
                out.push(p.clone());
            }
            if let Some(d) = &info.display_name {
                out.push(d.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- text runs

/// One formatted run of text.
#[derive(Clone)]
pub(super) struct Run {
    pub(super) text: String,
    pub(super) fmt: Fmt,
    /// The `<text:a xlink:href>` target the run sits under (docling#3949,
    /// 2.120): rendered as a Markdown link around the run.
    pub(super) href: Option<String>,
}

/// docling's `_odf_hyperlink_from_href`: a parseable absolute URL is kept
/// (normalized like pydantic's `AnyUrl` — a bare `scheme://host` gains its
/// `/`), a relative target (`guide.odt#part`, `#part`) is kept as a path, and
/// something that *looks* absolute (has a scheme) but does not parse
/// (`http://[::1`, `http://`, `http:`) is dropped rather than guessed at.
pub(super) fn hyperlink_from_href(href: Option<&str>) -> Option<String> {
    let href = href?.trim();
    if href.is_empty() {
        return None;
    }
    // `scheme:` — an ASCII letter, then letters/digits/`+.-`, then a colon.
    let scheme_len = href.find(':').filter(|&k| {
        let (first, rest) = href[..k].split_at(1);
        k > 0
            && first.chars().all(|c| c.is_ascii_alphabetic())
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    });
    let Some(k) = scheme_len else {
        return Some(href.to_string()); // relative target: kept as a path
    };
    let scheme = href[..k].to_ascii_lowercase();
    let rest = &href[k + 1..];
    if rest.is_empty() {
        return None; // `http:` — nothing after the scheme
    }
    let Some(authority) = rest.strip_prefix("//") else {
        return Some(format!("{scheme}:{rest}")); // `mailto:…`, `tel:…`
    };
    let host_end = authority.find(['/', '?', '#']).unwrap_or(authority.len());
    let (host, tail) = authority.split_at(host_end);
    // An empty host (`http://`) or an unterminated IPv6 literal (`http://[::1`)
    // is a malformed absolute URL: dropped, never guessed at.
    if host.is_empty() || (host.starts_with('[') && !host.contains(']')) {
        return None;
    }
    // pydantic's `AnyUrl` normalization: lower-cased scheme/host, and a bare
    // authority gains its `/` path (`https://example.com` → `…com/`).
    let slash = if tail.starts_with('/') { "" } else { "/" };
    Some(format!(
        "{scheme}://{}{slash}{tail}",
        host.to_ascii_lowercase()
    ))
}

/// Collect runs from a paragraph/heading element (recursing spans).
pub(super) fn collect_runs(el: XmlNode, styles: &Styles, base: Fmt, out: &mut Vec<Run>) {
    collect_runs_linked(el, styles, base, None, out)
}

fn collect_runs_linked(
    el: XmlNode,
    styles: &Styles,
    base: Fmt,
    href: Option<&str>,
    out: &mut Vec<Run>,
) {
    // docling's `_odf_text_runs` since docling#3850 (2.115, ported for #255):
    // the lxml *head* text, then each child's runs, then the child's `tail`
    // text in the parent's formatting. Before that fix a tail after any
    // content-bearing child was dropped ("with <span>bold</span>, and
    // <span>italic</span> formatting" lost ", and" and " formatting"); the
    // groundtruth now keeps every fragment. In roxmltree the head and the
    // tails are simply the element's text-node children, so collecting every
    // text node in document order reproduces the fixed reading exactly.
    let run = |text: String, fmt: Fmt| Run {
        text,
        fmt,
        href: href.map(str::to_string),
    };
    for child in el.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                out.push(run(t.to_string(), base));
            }
        } else if child.is_element() {
            match child.tag_name().name() {
                "span" => {
                    let fmt = resolve_fmt(styles, attr(child, "style-name"), base);
                    collect_runs_linked(child, styles, fmt, href, out);
                }
                // A hyperlink: its runs carry the target (docling#3949); a
                // `<text:a>` without a usable href is transparent.
                "a" => {
                    let fmt = resolve_fmt(styles, attr(child, "style-name"), base);
                    let link = hyperlink_from_href(attr(child, "href"));
                    collect_runs_linked(child, styles, fmt, link.as_deref().or(href), out);
                }
                "line-break" => out.push(run("\n".into(), base)),
                // `tab-stop` is OO1.x's spelling of `<text:tab>` (#215).
                "tab" | "tab-stop" => out.push(run("\t".into(), base)),
                "s" => {
                    // <text:s text:c="n"> = n spaces (default 1)
                    let n: usize = attr(child, "c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    out.push(run(" ".repeat(n), base));
                }
                // A `<draw:object>` in flat ODF (#215) embeds a whole
                // `<office:document>` inline — its meta/settings/body text is
                // not this paragraph's text. The packaged form keeps objects in
                // separate parts (an empty `<draw:object xlink:href=…/>` here),
                // so skipping the subtree makes both envelopes read the same.
                // `<office:binary-data>` is an inline image's base64 payload.
                "object" | "binary-data" => {}
                // A footnote/endnote's citation marker and body live inside
                // this element, but neither belongs in the citing sentence's
                // own text (docling#4375): the body can be arbitrarily long,
                // and splicing it in here corrupted the reading order. The
                // body is recovered separately as a furniture item, see
                // [`note_texts`].
                "note" => {}
                // A reviewer comment sits inline in the element it annotates
                // (#829, docling#4436): its author, date and text are not the
                // sentence's; they become a notes-layer comment, see
                // [`annotations`]. `annotation-end` carries no text.
                "annotation" | "annotation-end" => {}
                // A `<draw:text-box>` (#831) holds blocks of its own —
                // paragraphs, lists, tables — not this paragraph's text;
                // the walkers emit them after it, see [`text_boxes`].
                "text-box" => {}
                // docling's `_odf_text_runs` recurses into every child, so an
                // image's `<svg:desc>`/`<svg:title>` text is picked up too —
                // kept for parity (docling's groundtruth has it in the
                // paragraph); the same text is the picture's description
                // since #834, see [`frame_alt`].
                _ => collect_runs_linked(child, styles, base, href, out),
            }
        }
    }
}

/// The body text of every `<text:note>` below `body`, in document order —
/// docling's `_add_footnotes` (docling#4375, ODT only): the note body's
/// `text_content` (its descendant text concatenated), stripped; blank notes
/// are skipped. Each becomes a furniture-layer `footnote` item, the layer
/// headers/footers use: available to callers, out of the reading order.
pub(super) fn note_texts(body: XmlNode) -> Vec<String> {
    note_bodies(body)
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

/// [`note_texts`] with each note's `<text:note>` element — the call site a
/// citing paragraph's Pandoc `Note` goes to (#538).
pub(super) fn note_bodies(body: XmlNode) -> Vec<(roxmltree::NodeId, String)> {
    body.descendants()
        .filter(|n| n.has_tag_name("note"))
        .filter_map(|note| {
            let note_body = note.children().find(|c| c.has_tag_name("note-body"))?;
            let text: String = note_body
                .descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>()
                .trim()
                .to_string();
            (!text.is_empty()).then_some((note.id(), text))
        })
        .collect()
}

/// The `<text:note>` calls of a paragraph-like element (`p`, `h`, or a list
/// item's direct `p` / `h` children) as `(offset, note)`, the offset in chars
/// of the text [`collect_runs`] reads from it — the same traversal, with a
/// note's own text left out as there (#538).
pub(super) fn note_calls(el: XmlNode) -> (String, Vec<(usize, roxmltree::NodeId)>) {
    fn walk(el: XmlNode, text: &mut String, out: &mut Vec<(usize, roxmltree::NodeId)>) {
        for child in el.children() {
            if child.is_text() {
                text.push_str(child.text().unwrap_or_default());
                continue;
            }
            if !child.is_element() {
                continue;
            }
            match child.tag_name().name() {
                "line-break" => text.push('\n'),
                "tab" | "tab-stop" => text.push('\t'),
                "s" => {
                    let n: usize = attr(child, "c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    text.push_str(&" ".repeat(n));
                }
                "object" | "binary-data" => {}
                "note" => out.push((text.chars().count(), child.id())),
                "annotation" | "annotation-end" | "text-box" => {}
                _ => walk(child, text, out),
            }
        }
    }
    let mut text = String::new();
    let mut out = Vec::new();
    if el.has_tag_name("list-item") || el.has_tag_name("list-header") {
        // An item's headings are their own text items (#827), anchored apart.
        for c in el.children().filter(|c| c.has_tag_name("p")) {
            walk(c, &mut text, &mut out);
        }
    } else {
        walk(el, &mut text, &mut out);
    }
    (text, out)
}

// ---------------------------------------------------------------- page furniture

/// A page header or footer (#830): one `<style:header>` / `<style:footer>`
/// (or the `-left` / `-first` variant) of a `<style:master-page>`.
pub(super) struct MasterPart<'a, 'i> {
    pub(super) footer: bool,
    pub(super) el: XmlNode<'a, 'i>,
}

/// The page headers and footers of every master page (#830): ODT keeps
/// them in `styles.xml`'s `<office:master-styles>` (a flat file's sits in
/// the one DOM), which the stylesheet reader only read text and list
/// styles from, so none reached the furniture layer where the DOCX and
/// `.doc` backends put theirs (the reporter's corpus: 6 223 paragraphs and
/// 1 557 tables in 623 files). Master page by master page in file order,
/// headers before footers, each variant that is shown (`style:display`
/// not `false` — LibreOffice writes the unused left/first variants hidden)
/// and has content. Python docling reads none of them (its DOCX backend
/// does); the shape is the DOCX one, so the twin documents match.
pub(super) fn master_page_parts<'a, 'i>(
    content: &'a Document<'i>,
    styles: Option<&'a Document<'i>>,
) -> Vec<MasterPart<'a, 'i>> {
    let dom = styles.unwrap_or(content);
    let Some(master) = dom
        .root_element()
        .children()
        .find(|c| c.has_tag_name("master-styles"))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for page in master.children().filter(|c| c.has_tag_name("master-page")) {
        for (footer, tags) in [
            (false, ["header", "header-left", "header-first"]),
            (true, ["footer", "footer-left", "footer-first"]),
        ] {
            for el in page
                .children()
                .filter(|c| tags.contains(&c.tag_name().name()))
            {
                let shown = attr(el, "display") != Some("false");
                let has_content = el.descendants().any(|n| {
                    n.has_tag_name("image")
                        || n.has_tag_name("object")
                        || n.is_text() && !n.text().unwrap_or("").trim().is_empty()
                });
                if shown && has_content {
                    out.push(MasterPart { footer, el });
                }
            }
        }
    }
    out
}

/// The page headers and footers as furniture after the body (#830): each
/// part's blocks — paragraphs, tables, lists — walked like body blocks and
/// wrapped on the furniture layer, the DOCX backend's flat shape (DocLang
/// stamps each with its `<layer>`; Markdown and LaTeX leave them out).
fn push_master_page_furniture(parts: &[MasterPart], styles: &Styles, doc: &mut DoclingDocument) {
    for part in parts {
        let mut sub = DoclingDocument::new("");
        walk_blocks(
            part.el.children().filter(XmlNode::is_element),
            styles,
            &mut sub,
        );
        for n in sub.nodes {
            doc.nodes.push(Node::Furniture {
                layer: docling_core::ContentLayer::Furniture,
                inner: Box::new(n),
            });
        }
    }
}

/// Footnote/endnote bodies as furniture paragraphs after the body walk.
fn push_note_furniture(body: XmlNode, doc: &mut DoclingDocument) {
    for text in note_texts(body) {
        doc.nodes.push(Node::Furniture {
            layer: docling_core::ContentLayer::Furniture,
            inner: Box::new(Node::Paragraph {
                text: super::markdown::escape_html(&super::markdown::escape_underscores(&text)),
            }),
        });
    }
}

// ---------------------------------------------------------------- comments

/// A reviewer comment — an `<office:annotation>` (#829, docling#4436). Writer
/// writes it *inline* in the paragraph, heading or list item it annotates,
/// with its `dc:creator`, `dc:date`, initials and `<text:p>` body as
/// children, so the run collectors used to splice all four into the body
/// text (`Project BudgetReviewer Alpha2026-01-15T10:00:00RARename this
/// heading`) and nothing reached the notes layer (the reporter's corpus:
/// 795 comments in 113 files). They are now skipped there and collected
/// here, docling's DOCX note shape: `[author: Name (initials), time: date]:
/// text` — the head without a part it lacks, the bare text without any
/// head, nothing when both are empty. A comment inside a tracked-change
/// deletion is dropped with the text, as docling drops it.
#[derive(Clone, Debug)]
pub(super) struct Annotation {
    id: roxmltree::NodeId,
    /// The `<text:p>` / `<text:h>` holding the comment — the item the
    /// thread hangs on; `None` for one outside any (a bare cell).
    pub(super) anchor: Option<roxmltree::NodeId>,
    /// `office:name`, what a reply's `loext:parent-name` refers to.
    name: Option<String>,
    parent: Option<String>,
    pub(super) text: String,
}

/// Every annotation below `body`, in document order.
pub(super) fn annotations(body: XmlNode) -> Vec<Annotation> {
    body.descendants()
        .filter(|n| n.has_tag_name("annotation"))
        .filter(|a| !a.ancestors().any(|x| x.has_tag_name("tracked-changes")))
        .filter_map(|a| {
            let child_text = |tag: &str| {
                a.children()
                    .find(|c| c.has_tag_name(tag))
                    .map(|c| {
                        c.descendants()
                            .filter(|n| n.is_text())
                            .filter_map(|n| n.text())
                            .collect::<String>()
                            .trim()
                            .to_string()
                    })
                    .filter(|t| !t.is_empty())
            };
            let mut head: Vec<String> = Vec::new();
            if let Some(creator) = child_text("creator") {
                // `meta:creator-initials` (ODF 1.3) or LibreOffice's
                // `loext:sender-initials`.
                match child_text("creator-initials").or_else(|| child_text("sender-initials")) {
                    Some(initials) => head.push(format!("author: {creator} ({initials})")),
                    None => head.push(format!("author: {creator}")),
                }
            }
            if let Some(date) = child_text("date") {
                head.push(format!("time: {date}"));
            }
            let body: String = a
                .children()
                .filter(|c| c.has_tag_name("p"))
                .map(|p| para_plain_text(p).trim().to_string())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            let text = match (head.is_empty(), body.is_empty()) {
                (true, true) => return None,
                (false, true) => format!("[{}]", head.join(", ")),
                (true, false) => body,
                (false, false) => format!("[{}]: {body}", head.join(", ")),
            };
            Some(Annotation {
                id: a.id(),
                anchor: a
                    .ancestors()
                    .skip(1)
                    .find(|x| x.has_tag_name("p") || x.has_tag_name("h"))
                    .map(|x| x.id()),
                name: attr(a, "name").map(str::to_string),
                parent: attr(a, "parent-name").map(str::to_string),
                text,
            })
        })
        .collect()
}

/// The comment threads (#829): each a root comment and, depth first, the
/// replies naming it through `loext:parent-name` — a reply joins its
/// parent's thread wherever the file puts it. A comment without a parent,
/// naming itself or an unknown one is a root; a cycle of replies, which no
/// root reaches, becomes a thread from its first comment. Threads are
/// numbered from 1, `comment-{n}`, in this order.
pub(super) fn comment_threads(annotations: &[Annotation]) -> Vec<Vec<usize>> {
    let by_name: HashMap<&str, usize> = annotations
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.name.as_deref().map(|n| (n, i)))
        .collect();
    let mut replies: Vec<Vec<usize>> = vec![Vec::new(); annotations.len()];
    let mut is_reply = vec![false; annotations.len()];
    for (i, a) in annotations.iter().enumerate() {
        if let Some(parent) = a.parent.as_deref().and_then(|p| by_name.get(p)) {
            if *parent != i {
                replies[*parent].push(i);
                is_reply[i] = true;
            }
        }
    }
    let mut seen = vec![false; annotations.len()];
    let mut threads = Vec::new();
    for start in (0..annotations.len())
        .filter(|&i| !is_reply[i])
        .chain(0..annotations.len())
    {
        if seen[start] {
            continue;
        }
        let mut thread = Vec::new();
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            thread.push(i);
            stack.extend(replies[i].iter().rev());
        }
        threads.push(thread);
    }
    threads
}

/// Each comment's index among the flat [`Node::CommentSection`]s, which
/// [`push_comment_sections`] writes in thread order.
fn comment_slots(annotations: &[Annotation]) -> HashMap<roxmltree::NodeId, usize> {
    comment_threads(annotations)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(slot, i)| (annotations[i].id, slot))
        .collect()
}

/// The comments after the body as notes-layer sections (#829): one per
/// comment, in thread order, named by thread as the item tree names its
/// groups — the DOCX backend's flat shape (DocLang writes each as a
/// `<layer value="notes"/>` text; Markdown and LaTeX leave them out).
fn push_comment_sections(annotations: &[Annotation], doc: &mut DoclingDocument) {
    for (t, thread) in comment_threads(annotations).iter().enumerate() {
        for &i in thread {
            doc.nodes.push(Node::CommentSection {
                name: format!("comment-{}", t + 1),
                text: annotations[i].text.clone(),
                refs_note_text: false,
                grouped: true,
            });
        }
    }
}

/// The flat comment slots of the comments the paragraph-like elements in
/// `els` hold directly (an annotation in a nested list item, text box or
/// note belongs to the paragraph there).
fn comment_slots_in<'a, 'i: 'a>(
    els: impl Iterator<Item = XmlNode<'a, 'i>>,
    styles: &Styles,
) -> Vec<usize> {
    let mut slots = Vec::new();
    for el in els {
        for a in el
            .descendants()
            .filter(|n| n.has_tag_name("annotation"))
            .filter(|a| {
                a.ancestors()
                    .skip(1)
                    .find(|x| x.has_tag_name("p") || x.has_tag_name("h"))
                    .is_some_and(|x| x.id() == el.id())
            })
        {
            if let Some(&slot) = styles.comment_slots.get(&a.id()) {
                slots.push(slot);
            }
        }
    }
    slots
}

/// Wrap the node just pushed — the text node of an annotated paragraph,
/// heading or list item (#829) — in [`Node::Commented`] with `slots`.
fn wrap_last_commented(doc: &mut DoclingDocument, slots: Vec<usize>) {
    if slots.is_empty() {
        return;
    }
    if let Some(last) = doc.nodes.last_mut() {
        let inner = std::mem::replace(last, Node::PageBreak);
        *last = Node::Commented {
            comments: slots,
            inner: Box::new(inner),
        };
    }
}

/// Merge adjacent same-format runs, trim the paragraph's two ends
/// (docling's `_normalize_odf_text_runs`, `trim_run_edges` since
/// docling#3891), serialize each with its edge whitespace hoisted outside
/// the markers, and concatenate — docling-core 2.106's inline group (#997).
/// A hyperlink boundary is not special: its space stays in the run that
/// owns it (`Watch [the example talk](…) for context.`).
fn runs_to_text(mut runs: Vec<Run>) -> String {
    // merge adjacent same-fmt (and same-link) runs
    let mut merged: Vec<Run> = Vec::new();
    for r in runs.drain(..) {
        if let Some(last) = merged.last_mut() {
            if last.fmt == r.fmt && last.href == r.href {
                last.text.push_str(&r.text);
                continue;
            }
        }
        merged.push(r);
    }
    docling_core::inline::trim_run_edges(merged, |r| r.text.as_str(), |r, t| r.text = t, |_| true)
        .iter()
        .map(|r| {
            docling_core::inline::hoist(&r.text, |core| {
                serialize_run(core, r.fmt, r.href.as_deref())
            })
        })
        .collect()
}

/// The structured [`InlineRun`]s for a paragraph — one per format group, the
/// DocLang-only counterpart of [`runs_to_text`]. Adjacent same-format runs are
/// merged (raw text kept, so inter-run spacing survives as docling emits it),
/// then the whole paragraph is stripped at its two ends (docling's
/// `_normalize_odf_text_runs`). Empty groups drop out.
fn runs_to_inline(runs: Vec<Run>) -> Vec<InlineRun> {
    let mut merged: Vec<Run> = Vec::new();
    for r in runs {
        if let Some(last) = merged.last_mut() {
            if last.fmt == r.fmt && last.href == r.href {
                last.text.push_str(&r.text);
                continue;
            }
        }
        merged.push(r);
    }
    if let Some(first) = merged.first_mut() {
        first.text = first.text.trim_start().to_string();
    }
    if let Some(last) = merged.last_mut() {
        last.text = last.text.trim_end().to_string();
    }
    merged
        .into_iter()
        .filter(|r| !r.text.is_empty())
        .map(|r| r.fmt.to_inline_run(&r.text))
        .collect()
}

fn serialize_run(text: &str, fmt: Fmt, href: Option<&str>) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut s = escape_text(text);
    if fmt.bold {
        s = format!("**{s}**");
    }
    if fmt.italic {
        s = format!("*{s}*");
    }
    if fmt.strike {
        s = format!("~~{s}~~");
    }
    // docling-core's Markdown `serialize_hyperlink`, applied after formatting.
    if let Some(href) = href {
        s = docling_core::hyperlink::markdown_link(&s, href);
    }
    s
}

/// Whether an element is SVG-namespaced (`svg:title`, `svg:desc`), not
/// ODF's own `text:title` field.
pub(super) fn is_svg(el: XmlNode) -> bool {
    el.tag_name()
        .namespace()
        .is_some_and(|ns| ns.contains("svg"))
}

/// A picture's alt text (#834): the `<svg:title>` (Writer's "Text
/// Alternative"), else the `<svg:desc>` (its "Description"), of the
/// `<draw:frame>` the image sits in — the OOXML `descr` of a DOCX / PPTX
/// picture, which the backends read since #676. The text also stays in
/// the anchoring paragraph's runs, as docling's `_odf_text_runs` reads it
/// (its groundtruth keeps it there); stripping it would cost that parity.
pub(super) fn frame_alt(img: XmlNode) -> Option<String> {
    let frame = img.parent().filter(|p| p.has_tag_name("frame"))?;
    ["title", "desc"].into_iter().find_map(|tag| {
        frame
            .children()
            .find(|c| c.has_tag_name(tag) && is_svg(*c))
            .map(|c| {
                c.descendants()
                    .filter(|n| n.is_text())
                    .filter_map(|n| n.text())
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .filter(|t| !t.is_empty())
    })
}

/// The size a picture is displayed at (#834): its `<draw:frame>`'s
/// `svg:width` / `svg:height`, in inches.
pub(super) fn frame_display(img: XmlNode) -> Option<[f32; 2]> {
    let frame = img.parent().filter(|p| p.has_tag_name("frame"))?;
    let w = odf_length_in(attr(frame, "width")?)?;
    let h = odf_length_in(attr(frame, "height")?)?;
    (w > 0.0 && h > 0.0).then_some([w, h])
}

/// An ODF length (`2.5in`, `6.35cm`, `180pt`, `96px`, …) in inches.
pub(super) fn odf_length_in(s: &str) -> Option<f32> {
    let s = s.trim();
    let split = s
        .find(|c: char| c.is_ascii_alphabetic() || c == '%')
        .unwrap_or(s.len());
    let value: f32 = s[..split].trim().parse().ok()?;
    let per_inch = match s[split..].trim() {
        "in" | "" => 1.0,
        "cm" => 2.54,
        "mm" => 25.4,
        "pt" => 72.0,
        "pc" => 6.0,
        "px" => 96.0,
        _ => return None,
    };
    Some(value / per_inch)
}

/// `image` with the display size of the frame `img` sits in.
pub(super) fn with_frame_display(
    image: Option<PictureImage>,
    img: XmlNode,
) -> Option<PictureImage> {
    image.map(|mut i| {
        i.display_size = frame_display(img);
        i
    })
}

/// The picture node for a `<draw:image>`, or `None` when docling emits no
/// `PictureItem` for it (docling#4015, 2.120.3):
///
/// * an embedded part (`Pictures/…`, or inline `<office:binary-data>` in flat
///   ODF) is a picture — carrying the part's pixels when the package holds a
///   decodable bitmap (`Styles::images`), the inline payload stays unloaded;
/// * an external `http(s)` reference is fetched (bounded, SSRF-guarded) only
///   under `fetch_images`, and is a picture only when the fetch yields one;
/// * anything else external — an absolute or relative filesystem path, an
///   extension-less name — is never read and yields no picture.
fn odf_picture(styles: &Styles, img: XmlNode) -> Option<Node> {
    let href = attr(img, "href").unwrap_or("");
    if !image_can_be_bitmap(img, href) {
        return None;
    }
    let picture = |image: Option<PictureImage>| {
        Some(Node::Picture {
            caption: None,
            caption_href: None,
            image: with_frame_display(image, img),
            classification: None,
            description: frame_alt(img).map(PictureDescription::alt_text),
            caption_parent: Default::default(),
            caption_location: None,
        })
    };
    if href.is_empty() {
        return picture(None); // flat ODF inline payload, magic-checked above
    }
    // `./Pictures/x.png` (ODF) and `#Pictures/x.png` (OO1.x's package-internal
    // marker, #215) both name a part.
    let name = href.trim_start_matches("./").trim_start_matches('#');
    match &styles.parts {
        // The decoded part when PIL-equivalent decoding reads it (the JSON
        // tree's payload too), so DocLang names and packages the asset.
        Some(parts) if parts.contains(name) => return picture(styles.images.get(name).cloned()),
        // Flat ODF has no parts: a non-empty href is external by definition.
        _ => {}
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        if !styles.policy.sources.remote() {
            return None;
        }
        #[cfg(feature = "fetch-images")]
        {
            return crate::backend::images::fetch_remote_with(href, &styles.policy)
                .and_then(|img| picture(Some(img)));
        }
        #[cfg(not(feature = "fetch-images"))]
        {
            return None;
        }
    }
    None
}

// ---------------------------------------------------------------- text doc

fn walk_text(text: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    walk_blocks(text.children().filter(XmlNode::is_element), styles, doc);
}

/// Walk a run of sibling blocks, threading list-continuation state. Numbering
/// continues across consecutive `<text:list>` siblings when the next list opens
/// with an empty nested item (docling's `_OdfListState`); any non-list block
/// resets the continuation.
fn walk_blocks<'a, 'i: 'a>(
    els: impl Iterator<Item = XmlNode<'a, 'i>>,
    styles: &Styles,
    doc: &mut DoclingDocument,
) {
    let mut prev_state: Option<ListCont> = None;
    let mut style_run: Option<StyleListRun> = None;
    for el in els {
        if is_list_tag(el.tag_name().name()) {
            style_run = None;
            prev_state = add_odf_list(el, styles, doc, 0, 1, false, prev_state.take());
        } else if let Some((style, level)) = el
            .has_tag_name("p")
            .then(|| paragraph_list_style(styles, attr(el, "style-name")))
            .flatten()
        {
            prev_state = None;
            add_style_list_paragraph(el, style, level, styles, doc, &mut style_run);
        } else {
            prev_state = None;
            style_run = None;
            handle_block(el, styles, doc, 0, &mut Vec::new());
        }
    }
}

/// The list a run of consecutive style-numbered paragraphs (#826) forms: a
/// change of list style opens a new one, as Writer starts a new list.
struct StyleListRun {
    style: String,
    enumerated: bool,
    counter: i64,
    prefix: String,
    suffix: String,
}

/// A `<text:p>` whose paragraph style names a list style (#826): one item of
/// the list the run of such paragraphs builds, numbered along it.
fn add_style_list_paragraph(
    el: XmlNode,
    style: String,
    level: i64,
    styles: &Styles,
    doc: &mut DoclingDocument,
    run: &mut Option<StyleListRun>,
) {
    emit_paragraph_images(el, styles, doc);
    let mut runs = Vec::new();
    collect_runs(el, styles, Fmt::default(), &mut runs);
    let text = clean_lines(&runs_to_text(runs));
    if text.is_empty() {
        return;
    }
    let fresh = run.as_ref().is_none_or(|r| r.style != style);
    if fresh {
        let lv = list_level(styles, Some(&style), level);
        *run = Some(StyleListRun {
            style,
            enumerated: lv.is_some_and(|l| l.numbered),
            counter: lv.map_or(0, |l| l.start - 1),
            prefix: lv.map(|l| l.prefix.clone()).unwrap_or_default(),
            suffix: lv.map(|l| l.suffix.clone()).unwrap_or_default(),
        });
    }
    let Some(r) = run.as_mut() else {
        return;
    };
    r.counter += 1;
    let depth = (level - 1).clamp(0, u8::MAX as i64) as u8;
    push_odf_list_item(
        doc,
        text,
        depth,
        r.enumerated,
        r.counter,
        &r.prefix,
        &r.suffix,
        fresh,
    );
    wrap_last_commented(doc, comment_slots_in(std::iter::once(el), styles));
    walk_text_boxes(el, styles, doc);
}

fn handle_block(
    el: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    list_level: u8,
    counters: &mut Vec<u64>,
) {
    let _ = counters;
    match el.tag_name().name() {
        "h" => {
            emit_paragraph_images(el, styles, doc);
            // OO1.x headings carry `text:level` instead of `text:outline-level`.
            let level = attr(el, "outline-level")
                .or_else(|| attr(el, "level"))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(1)
                .max(1);
            let mut runs = Vec::new();
            collect_runs(el, styles, Fmt::default(), &mut runs);
            let text = runs_to_text(runs);
            if !text.is_empty() {
                doc.push(Node::Heading {
                    level: (level + 1) as u8,
                    text,
                });
                wrap_last_commented(doc, comment_slots_in(std::iter::once(el), styles));
            }
            walk_text_boxes(el, styles, doc);
        }
        "p" => {
            // docling's `_add_odf_paragraph` emits a paragraph's pictures before
            // its text.
            emit_paragraph_images(el, styles, doc);
            let names = paragraph_style_names(styles, attr(el, "style-name"));
            let mut runs = Vec::new();
            collect_runs(el, styles, Fmt::default(), &mut runs);
            let text = runs_to_text(runs.clone());
            if text.is_empty() {
                // A paragraph that only anchors a text box still yields the
                // box's blocks (#831).
                walk_text_boxes(el, styles, doc);
                return;
            }
            if names.iter().any(|n| n == "Title") {
                doc.push(Node::Heading { level: 1, text });
            } else if names.iter().any(|n| n == "Subtitle") {
                doc.push(Node::Heading { level: 2, text });
            } else {
                // Styled `<text:span>` runs → a rich `InlineGroup` (Markdown/JSON
                // still render `text`, so their output is unchanged); a plain
                // paragraph collapses back to `Node::Paragraph`.
                doc.push(inline_paragraph_node(text, runs_to_inline(runs), false));
            }
            wrap_last_commented(doc, comment_slots_in(std::iter::once(el), styles));
            walk_text_boxes(el, styles, doc);
        }
        n if is_list_tag(n) => {
            add_odf_list(el, styles, doc, list_level, 1, false, None);
        }
        "table" => {
            if let Some(table) = parse_table(el, styles) {
                doc.push(Node::Table(table));
            }
        }
        n if is_transparent_container(n) => {
            // docling walks a `<text:section>`'s children as ordinary body
            // blocks (docling#3852 / #178) — headings, paragraphs, tables and
            // lists inside a section were silently dropped before — and,
            // since docling#4750 (#823), an index's too. Recursing through
            // `walk_blocks` keeps list numbering continuing across the
            // section's own consecutive lists (`_add_odf_children`) and
            // handles nested sections. A hidden section's blocks go to the
            // invisible layer (#753).
            let start = doc.nodes.len();
            walk_blocks(el.children().filter(XmlNode::is_element), styles, doc);
            if section_is_hidden(el) {
                hide_nodes_from(doc, start);
            }
        }
        // A frame outside a paragraph — a picture anchored straight in a
        // table cell (#758) — is its chart or picture, as the JSON tree's
        // `_add_odf_children` has it.
        "frame" => {
            emit_frame_graphic(el, styles, doc);
            walk_text_boxes(el, styles, doc);
        }
        _ => {}
    }
}

/// Whether `n` sits inside a `<draw:text-box>` below `root` (#831): such a
/// node belongs to the box's blocks, which walk on their own.
pub(super) fn in_text_box(n: XmlNode, root: XmlNode) -> bool {
    n.ancestors()
        .skip(1)
        .take_while(|a| *a != root)
        .any(|a| a.has_tag_name("text-box"))
}

/// The text boxes a paragraph, heading or frame anchors (#831): every
/// `<draw:text-box>` below `el` that is not inside another one — a box in
/// a box walks with the paragraph of the outer box. Writer anchors a text
/// box as `<draw:frame><draw:text-box>…</draw:text-box></draw:frame>`
/// inside the paragraph; the run collectors read the box's paragraphs,
/// list items and table cells as the anchoring paragraph's own text, run
/// together with no separator (`Anchor paragraph text.First textbox
/// paragraph.Second…`), and a block-level frame's box was dropped (the
/// reporter's corpus: 187 of 632 files). Its blocks now walk as blocks
/// after the anchoring paragraph, as the DOCX backend emits a
/// `w:txbxContent`'s — a table in a box stays a table here, where that
/// backend flattens it to its cells' paragraphs (docling's `.//w:p`).
pub(super) fn text_boxes<'a, 'i>(el: XmlNode<'a, 'i>) -> Vec<XmlNode<'a, 'i>> {
    el.descendants()
        .filter(|n| n.has_tag_name("text-box") && !in_text_box(*n, el))
        .collect()
}

/// The blocks of the text boxes `el` anchors (#831), after it.
fn walk_text_boxes(el: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for tb in text_boxes(el) {
        walk_blocks(tb.children().filter(XmlNode::is_element), styles, doc);
    }
}

/// Emit the graphics anchored in a paragraph: each `<draw:frame>` yields one
/// node. OO1.x anchors `<draw:image>` directly — without a frame wrapper —
/// so frameless bitmaps yield a picture each too (#215).
fn emit_paragraph_images(el: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    // A frame inside a text box belongs to the box's own blocks (#831).
    for frame in el
        .descendants()
        .filter(|n| n.has_tag_name("frame") && !in_text_box(*n, el))
    {
        emit_frame_graphic(frame, styles, doc);
    }
    for img in el
        .descendants()
        .filter(|n| n.has_tag_name("image") && !n.ancestors().any(|a| a.has_tag_name("frame")))
    {
        if let Some(node) = odf_picture(styles, img) {
            doc.push(node);
        }
    }
}

/// Emit one node for a `<draw:frame>`: a chart when it wraps a known embedded
/// chart object, else a single picture placeholder from its first real image
/// (an `ObjectReplacements/` preview and alternate encodings collapse into that
/// one picture — docling emits one `PictureItem` per frame, not per bitmap).
/// Returns whether a node was emitted.
fn emit_frame_graphic(frame: XmlNode, styles: &Styles, doc: &mut DoclingDocument) -> bool {
    if let Some(obj) = frame.children().find(|c| c.has_tag_name("object")) {
        let name = attr(obj, "href").unwrap_or("").trim_start_matches("./");
        let info = styles
            .charts
            .get(name)
            .cloned()
            .or_else(|| inline_chart(obj, styles));
        if let Some(info) = info {
            doc.push(Node::Chart {
                kind: info.kind,
                table: info.table,
                caption: None,
                location: None,
            });
            return true;
        }
    }
    // The frame's alternate encodings (a PDF next to its PNG fallback) collapse
    // into one picture: the first `draw:image` docling would accept.
    let picture = frame
        .children()
        .filter(|c| {
            c.has_tag_name("image")
                && !attr(*c, "href")
                    .unwrap_or("")
                    .trim_start_matches("./")
                    .starts_with("ObjectReplacements/")
        })
        .find_map(|img| odf_picture(styles, img));
    if let Some(node) = picture {
        doc.push(node);
        return true;
    }
    false
}

/// A chart embedded *inline* in a `<draw:object>` — how flat ODF (#215) ships
/// what a packaged file stores as a separate object part: the object wraps a
/// whole `<office:document>` whose `<chart:chart>` and data `<table:table>`
/// sit right in the content DOM.
pub(super) fn inline_chart(obj: XmlNode, styles: &Styles) -> Option<ChartInfo> {
    let chart = obj.descendants().find(|n| n.has_tag_name("chart"))?;
    let kind = attr(chart, "class")
        .and_then(chart_kind)
        .or_else(|| {
            obj.descendants()
                .filter(|n| n.has_tag_name("series"))
                .find_map(|n| attr(n, "class").and_then(chart_kind))
        })
        .unwrap_or("other_chart")
        .to_string();
    let table_node = obj.descendants().find(|n| n.has_tag_name("table"))?;
    parse_table(table_node, styles).map(|table| ChartInfo { kind, table })
}

/// Continuation state carried across sibling `<text:list>` elements — docling's
/// `_OdfListState`. Carries the marker affixes so a continued list keeps the
/// original level's `num-prefix`/`num-suffix` even when its own `<text:list>`
/// element resolves to a bare style.
#[derive(Clone)]
struct ListCont {
    enumerated: bool,
    counter: i64,
    has_last: bool,
    prefix: String,
    suffix: String,
}

/// A list's item elements (`<text:list-item>` / `<text:list-header>`).
pub(super) fn list_items<'a, 'i>(list: XmlNode<'a, 'i>) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    list.children()
        .filter(|c| c.has_tag_name("list-item") || c.has_tag_name("list-header"))
}

/// An item's rendered text (its direct paragraphs' runs, cleaned to single lines)
/// and its directly-nested `<text:list>` elements. Mirrors docling's
/// `_odf_list_item_content` with `flatten_nested_text=False` — bar its
/// `<text:h>` children, which are headings, not item text (#827, see
/// [`item_headings`]).
pub(super) fn odf_item_content<'a, 'i>(
    item: XmlNode<'a, 'i>,
    styles: &Styles,
) -> (String, Vec<XmlNode<'a, 'i>>) {
    let mut parts: Vec<String> = Vec::new();
    let mut nested: Vec<XmlNode> = Vec::new();
    for child in item.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            n if is_list_tag(n) => nested.push(child),
            "p" => {
                let mut runs = Vec::new();
                collect_runs(child, styles, Fmt::default(), &mut runs);
                let text = clean_lines(&runs_to_text(runs));
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            _ => {}
        }
    }
    (parts.join(" "), nested)
}

/// Whether a list item has content of its own: text, or — in one of its
/// direct paragraphs — an image, an embedded object (a chart) or a note.
/// docling decides on odfdo's `text_recursive`, which renders such an
/// element as `(href)` or the note's citation and body; the item is kept
/// with an empty text (docling 2.139 takes the text from the runs only,
/// docling#4693), so it keeps its number and owns its nested list (#759).
pub(super) fn item_has_content(item: XmlNode, styles: &Styles) -> bool {
    !odf_item_content(item, styles).0.is_empty()
        || item
            .children()
            .filter(|c| c.has_tag_name("p"))
            .flat_map(|p| p.descendants())
            .any(|n| n.has_tag_name("image") || n.has_tag_name("object") || n.has_tag_name("note"))
}

/// A list item's direct `<text:h>` children (#827). Writer's outline
/// numbering — and a Word document's numbered headings, once LibreOffice
/// has written them — wraps chapter headings in `<text:list>` /
/// `<text:list-item>`, one nested list per outline level; docling's
/// `_odf_list_item_content` reads the heading as the item's text, so every
/// such heading became a `list_item` (the reporter's corpus: 515 of them,
/// all demoted) and the section hierarchy was gone. The walkers emit these
/// as headings with their `outline-level`, outside the list, as the `.docx`
/// backend emits the same document's; an item made of headings alone is no
/// list item at all (a list of them opens no list group), and the number
/// the outline list would print is not added, as the DOCX backend prints
/// none for the twin.
pub(super) fn item_headings<'a, 'i>(
    item: XmlNode<'a, 'i>,
) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    item.children().filter(|c| c.has_tag_name("h"))
}

/// Split on newlines, strip each line, drop the blanks, re-join with spaces —
/// docling's `_clean_odf_text_lines` joined.
pub(super) fn clean_lines(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a list renders anything (any item with text, or a renderable nested list).
pub(super) fn list_has_renderable(list: XmlNode, styles: &Styles) -> bool {
    list_items(list).any(|item| {
        let (_, nested) = odf_item_content(item, styles);
        item_has_content(item, styles)
            || item_headings(item).next().is_some()
            || nested.iter().any(|n| list_has_renderable(*n, styles))
    })
}

/// Whether any item has content of its own (vs. only nested lists).
pub(super) fn list_has_direct_text(list: XmlNode, styles: &Styles) -> bool {
    list_items(list).any(|item| item_has_content(item, styles))
}

/// Whether the first item is empty but wraps a renderable nested list — the
/// signal that this list continues the previous one's numbering.
pub(super) fn list_starts_with_empty_nested(list: XmlNode, styles: &Styles) -> bool {
    if let Some(item) = list_items(list).next() {
        let (_, nested) = odf_item_content(item, styles);
        return !item_has_content(item, styles)
            && nested.iter().any(|n| list_has_renderable(*n, styles));
    }
    false
}

/// A list level's rendering (bullet vs numbered) from the list's own style, else
/// the inherited `fallback` — docling's `_odf_list_level_is_enumerated`. The
/// OO1.x tags decide for themselves (#215): `<text:ordered-list>` is numbered,
/// `<text:unordered-list>` is bulleted, whatever the inherited fallback says.
pub(super) fn level_is_enumerated(
    styles: &Styles,
    list: XmlNode,
    level: i64,
    fallback: bool,
) -> bool {
    list_level(styles, attr(list, "style-name"), level)
        .map(|lv| lv.numbered)
        .unwrap_or_else(|| match list.tag_name().name() {
            "ordered-list" => true,
            "unordered-list" => false,
            _ => fallback,
        })
}

/// A list level's `start-value` (default 1).
pub(super) fn level_start(styles: &Styles, list: XmlNode, level: i64) -> i64 {
    list_level(styles, attr(list, "style-name"), level)
        .map(|lv| lv.start)
        .unwrap_or(1)
}

/// One level of a named list style, when the style defines it.
pub(super) fn list_level<'a>(
    styles: &'a Styles,
    name: Option<&str>,
    level: i64,
) -> Option<&'a OdfLevel> {
    styles.lists.get(name?)?.get(&level)
}

/// A level's `num-prefix`/`num-suffix` — the affixes wrapping an enumerated
/// marker (e.g. `"" / "."` → `1.`).
pub(super) fn level_affixes(styles: &Styles, list: XmlNode, level: i64) -> (String, String) {
    list_level(styles, attr(list, "style-name"), level)
        .map(|lv| (lv.prefix.clone(), lv.suffix.clone()))
        .unwrap_or_default()
}

/// Emit an ODF list as flat [`Node::ListItem`]s — a port of docling's
/// `_add_odf_list`. `depth` is the Markdown nesting level for items of this list;
/// `style_level` (1-based) drives style lookups; empty items collapse (their
/// nested list attaches to the previous item) and a list that opens with an empty
/// nested item continues the previous list's numbering.
fn add_odf_list(
    list: XmlNode,
    styles: &Styles,
    doc: &mut DoclingDocument,
    depth: u8,
    style_level: i64,
    enumerated_fallback: bool,
    continued: Option<ListCont>,
) -> Option<ListCont> {
    if !list_has_renderable(list, styles) {
        return None;
    }
    let style_enum = level_is_enumerated(styles, list, style_level, enumerated_fallback);
    let should_continue = continued.as_ref().map(|c| c.has_last).unwrap_or(false)
        && list_starts_with_empty_nested(list, styles);

    // A list with no direct text of its own (and not continuing) is transparent:
    // its items' nested lists take its place at the same depth.
    if !should_continue && !list_has_direct_text(list, styles) {
        for item in list_items(list) {
            let (_text, nested) = odf_item_content(item, styles);
            emit_item_headings(item, styles, doc);
            for n in nested {
                add_odf_list(n, styles, doc, depth, style_level + 1, style_enum, None);
            }
        }
        return None;
    }

    let (mut counter, current_enum) = match (should_continue, &continued) {
        (true, Some(c)) => (c.counter, c.enumerated),
        _ => (level_start(styles, list, style_level) - 1, style_enum),
    };
    // Enumerated-marker affixes: reuse the continued list's so numbering that
    // spans sibling `<text:list>`s keeps one suffix; else this level's own.
    let (prefix, suffix) = match (should_continue, &continued) {
        (true, Some(c)) => (c.prefix.clone(), c.suffix.clone()),
        _ => level_affixes(styles, list, style_level),
    };
    let mut has_last = should_continue;
    // A non-continued `<text:list>` opens a fresh docling ListGroup: its first
    // rendered item carries the fresh-list flag (a following sibling group at
    // the same depth gets its own `<list>` / top-level Markdown blank line).
    let mut first = !should_continue;

    for item in list_items(list) {
        let (text, nested) = odf_item_content(item, styles);
        let has_content = item_has_content(item, styles);
        let nested: Vec<XmlNode> = nested
            .into_iter()
            .filter(|n| list_has_renderable(*n, styles))
            .collect();
        // The item's headings (#827) come out before its text, outside the list.
        emit_item_headings(item, styles, doc);
        if !has_content && nested.is_empty() {
            continue;
        }
        if !has_content {
            // Empty item: its nested list collapses under the previous item.
            for n in &nested {
                add_odf_list(
                    *n,
                    styles,
                    doc,
                    depth + 1,
                    style_level + 1,
                    style_enum,
                    None,
                );
            }
            continue;
        }
        counter += 1;
        push_odf_list_item(
            doc,
            text,
            depth,
            current_enum,
            counter,
            &prefix,
            &suffix,
            std::mem::take(&mut first),
        );
        wrap_last_commented(
            doc,
            comment_slots_in(item.children().filter(|c| c.has_tag_name("p")), styles),
        );
        for p in item.children().filter(|c| c.has_tag_name("p")) {
            walk_text_boxes(p, styles, doc);
        }
        has_last = true;
        for n in &nested {
            add_odf_list(
                *n,
                styles,
                doc,
                depth + 1,
                style_level + 1,
                style_enum,
                None,
            );
        }
    }

    Some(ListCont {
        enumerated: current_enum,
        counter,
        has_last,
        prefix,
        suffix,
    })
}

/// Push one flat list item: the `counter`-th of a list at Markdown `depth`,
/// `enumerated` with the level's `prefix` / `suffix` around its number (an
/// `<ldiv>` marker) or bulleted; `first` opens a fresh docling ListGroup.
#[allow(clippy::too_many_arguments)]
fn push_odf_list_item(
    doc: &mut DoclingDocument,
    text: String,
    depth: u8,
    enumerated: bool,
    counter: i64,
    prefix: &str,
    suffix: &str,
    first: bool,
) {
    let (ordered, number, marker) = if enumerated {
        let n = counter.max(0) as u64;
        // docling renders an enumerated marker inside the `<ldiv>`:
        // `<marker>{prefix}{n}{suffix}</marker>` (e.g. `1.`).
        (true, n, Some(format!("{prefix}{n}{suffix}")))
    } else {
        (false, 0, None)
    };
    // The item's marker in docling's model is `_odf_list_marker`: the
    // counter and the level's `num-suffix`, `.` without one. docling-core's
    // Markdown prints one of digits and a dot (`1.`) as a list number; any
    // other (`1)`) is a text prefix behind a bullet (`- 1) text`,
    // `orig_list_item_marker_mode=AUTO`, as the DOCX walk does). DocLang
    // keeps the ordered item and its `<marker>` through `dclx`.
    let item_marker = format!(
        "{n}{}",
        if suffix.is_empty() { "." } else { suffix },
        n = counter.max(0)
    );
    let (ordered, marker, text, dclx) = match marker {
        Some(m) if !is_plain_number_marker(&item_marker) => {
            let dclx = Some(ListItemDclx {
                ordered: true,
                marker: Some(m),
                text: text.clone(),
                runs: Vec::new(),
            });
            (false, None, format!("{item_marker} {text}"), dclx)
        }
        // The model's marker is `item_marker` (`1.`), what the Markdown
        // prints; DocLang's stays `{prefix}{n}{suffix}` (an OOo 1.x list has
        // no suffix: `1`), carried in `dclx` when the two differ.
        Some(m) if m != item_marker => {
            let dclx = Some(ListItemDclx {
                ordered: true,
                marker: Some(m),
                text: text.clone(),
                runs: Vec::new(),
            });
            (ordered, Some(item_marker), text, dclx)
        }
        marker => (ordered, marker, text, None),
    };
    doc.push(Node::ListItem {
        ordered,
        number,
        first_in_list: first,
        text,
        level: depth,
        marker,
        location: None,
        dclx,
        href: None,
        layer: None,
    });
}

/// A list item's `<text:h>` children as headings (#827), at the top level.
fn emit_item_headings(item: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for h in item_headings(item) {
        handle_block(h, styles, doc, 0, &mut Vec::new());
    }
}

/// A marker of digits and a dot (`12.`), which docling-core's Markdown
/// prints as it is (`case_already_valid`).
fn is_plain_number_marker(marker: &str) -> bool {
    marker
        .strip_suffix('.')
        .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

// ---------------------------------------------------------------- tables

fn parse_table(table: XmlNode, styles: &Styles) -> Option<Table> {
    // Only rows of *this* table — `descendants()` would also pull in the rows of
    // any nested table (which a rich cell renders on its own).
    let tr_rows: Vec<XmlNode> = table
        .descendants()
        .filter(|n| {
            n.has_tag_name("table-row")
                && n.ancestors().find(|a| a.has_tag_name("table")) == Some(table)
        })
        .collect();
    if tr_rows.is_empty() {
        return None;
    }

    // Each `<table:table-cell>`/`<table:covered-table-cell>` occupies one column
    // (times its `number-columns-repeated`). ODF emits an explicit
    // `covered-table-cell` for every spanned-over position, so col/row spans need
    // no extra bookkeeping: the anchor carries the text and the covered cells are
    // blank — exactly docling's grid.
    let cells_of = |tr: &XmlNode| -> Vec<(bool, usize)> {
        tr.children()
            .filter(|c| c.has_tag_name("table-cell") || c.has_tag_name("covered-table-cell"))
            .map(|c| {
                (
                    c.has_tag_name("covered-table-cell"),
                    repeat(c, "number-columns-repeated"),
                )
            })
            .collect()
    };
    let num_cols = tr_rows
        .iter()
        .map(|tr| cells_of(tr).iter().map(|(_, r)| r).sum::<usize>())
        .max()
        .unwrap_or(0);
    if num_cols == 0 {
        return None;
    }

    // The text grid plus the OTSL span overlay. ODF marks a span on its anchor
    // (`number-columns-spanned`/`number-rows-spanned`) and emits an explicit
    // `covered-table-cell` at every covered position; we mark the anchor's
    // rectangle so a covered cell becomes `<lcel/>` (horizontal), `<ucel/>`
    // (vertical) or `<xcel/>` (both). The anchor's text also fills every
    // covered position, as docling's grid does.
    let nrows = tr_rows.len();
    let mut grid = vec![vec![String::new(); num_cols]; nrows];
    let mut col_cont = vec![vec![false; num_cols]; nrows];
    let mut row_cont = vec![vec![false; num_cols]; nrows];
    // Parallel per-cell block content for rich cells (lists / multiple
    // paragraphs / nested tables). Empty for plain cells.
    let mut blocks: Vec<Vec<Vec<Node>>> = vec![vec![Vec::new(); num_cols]; nrows];
    let mut any_rich = false;
    for (ri, tr) in tr_rows.iter().enumerate() {
        let mut ci = 0usize;
        for tc in tr
            .children()
            .filter(|c| c.has_tag_name("table-cell") || c.has_tag_name("covered-table-cell"))
        {
            let reps = repeat(tc, "number-columns-repeated");
            if tc.has_tag_name("covered-table-cell") {
                // A covered position: its continuation kind is set by the anchor
                // whose rectangle reaches it; here we only advance the column.
                ci = (ci + reps).min(num_cols);
                continue;
            }
            let text = cell_text(tc, styles);
            let cell_nodes = cell_blocks_of(tc, styles);
            any_rich |= !cell_nodes.is_empty();
            let cspan = repeat(tc, "number-columns-spanned");
            let rspan = repeat(tc, "number-rows-spanned");
            for _ in 0..reps {
                if ci >= num_cols {
                    break;
                }
                grid[ri][ci] = text.clone();
                blocks[ri][ci] = cell_nodes.clone();
                let r_end = (ri + rspan).min(nrows);
                let c_end = (ci + cspan).min(num_cols);
                for rr in ri..r_end {
                    for cc in ci..c_end {
                        if rr == ri && cc == ci {
                            continue;
                        }
                        // docling's `TableData.grid` repeats the anchor's text
                        // into every covered position (visible in
                        // Markdown/JSON), a rich cell's too (docling-core#857,
                        // #777). DocLang ignores it — continuation cells are
                        // token-only.
                        grid[rr][cc] = text.clone();
                        col_cont[rr][cc] |= cc > ci;
                        row_cont[rr][cc] |= rr > ri;
                    }
                }
                ci += 1;
            }
        }
    }

    // An all-empty grid keeps docling's `(0, 0, 0, 0)` fallback bounds — the
    // table renders as a 1×1 empty cell (`|    |`), not nothing.
    let (min_r, max_r, min_c, max_c) = data_bounds(&grid).unwrap_or((0, 0, 0, 0));
    let slice = |g: Vec<Vec<bool>>| -> Vec<Vec<bool>> {
        g[min_r..=max_r]
            .iter()
            .map(|row| row[min_c..=max_c].to_vec())
            .collect()
    };
    let rows: Vec<Vec<String>> = grid[min_r..=max_r]
        .iter()
        .map(|row| row[min_c..=max_c].to_vec())
        .collect();
    // docling marks the first (trimmed) row of an ODF table as the header band.
    let header_row = (0..rows.len()).map(|r| r == 0).collect();
    let cell_blocks = any_rich.then(|| {
        blocks[min_r..=max_r]
            .iter()
            .map(|row| row[min_c..=max_c].to_vec())
            .collect()
    });
    Some(Table {
        rows,
        location: None,
        structure: Some(TableStructure {
            header_row,
            col_continuation: slice(col_cont),
            row_continuation: slice(row_cont),
            row_header: Vec::new(),
            col_header: Vec::new(),
        }),
        cell_blocks,
        caption: None,
        cells: None,
        caption_parent: Default::default(),
        caption_location: None,
    })
}

/// The smallest rectangle covering all non-empty cells as `(min_r, max_r,
/// min_c, max_c)` — docling's `_find_true_data_bounds`; `None` when empty.
fn data_bounds(grid: &[Vec<String>]) -> Option<(usize, usize, usize, usize)> {
    let non_empty = |r: &Vec<String>| r.iter().any(|c| !c.trim().is_empty());
    let (min_r, max_r) = (
        grid.iter().position(non_empty)?,
        grid.iter().rposition(non_empty)?,
    );
    let ncols = grid[0].len();
    let col_used = |c: usize| (min_r..=max_r).any(|r| !grid[r][c].trim().is_empty());
    let min_c = (0..ncols).find(|&c| col_used(c))?;
    let max_c = (0..ncols).rposition(col_used)?;
    Some((min_r, max_r, min_c, max_c))
}

/// A table cell's Markdown. A *rich* cell (lists, multiple paragraphs, nested
/// tables or images) renders its full block content — formatting kept — flattened
/// to a single line; a *plain* cell renders its unformatted text. Mirrors
/// docling's `_odf_cell_is_rich` / `_odf_cell_text` split.
fn cell_text(tc: XmlNode, styles: &Styles) -> String {
    if is_rich_cell(tc, styles) {
        rich_cell_markdown(tc, styles)
    } else {
        plain_cell_text(tc)
    }
}

/// A *rich* cell's DocLang block content — the structured counterpart of
/// [`rich_cell_markdown`], built by walking the cell's children exactly like a
/// document body (so lists, headings, paragraphs with inline runs and nested
/// tables all render through the same code). Empty for a plain cell, whose flat
/// [`cell_text`] the serializer uses instead. Markdown/JSON never consult this.
fn cell_blocks_of(tc: XmlNode, styles: &Styles) -> Vec<Node> {
    if !is_rich_cell(tc, styles) {
        return Vec::new();
    }
    let mut sub = DoclingDocument::new("");
    walk_blocks(tc.children().filter(XmlNode::is_element), styles, &mut sub);
    sub.nodes
}

/// Whether a cell must render as rich block content — docling's
/// `_odf_cell_has_rich_content`: it holds an image, a renderable list, a header,
/// a nested table with content, a paragraph with an image, more than one
/// non-empty paragraph, or any non-empty paragraph in a cell without a typed
/// value (`office:value-type`) — presentation tables never type their cells, so
/// their text cells are all rich (docling's `cell.value is None` clause).
pub(super) fn is_rich_cell(tc: XmlNode, styles: &Styles) -> bool {
    if cell_has_image(tc) {
        return true;
    }
    let mut non_empty_paragraphs = 0;
    for child in tc.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            n if is_list_tag(n) && list_has_renderable(child, styles) => return true,
            "h" if !clean_lines(&para_plain_text(child)).is_empty() => return true,
            "table" if table_has_content(child) => return true,
            "p" => {
                if !clean_lines(&para_plain_text(child)).is_empty() {
                    non_empty_paragraphs += 1;
                }
                if cell_has_image(child) {
                    return true;
                }
            }
            _ => {}
        }
    }
    non_empty_paragraphs > 1 || (attr(tc, "value-type").is_none() && non_empty_paragraphs > 0)
}

/// A cell/paragraph holding a bitmap image (`<draw:image>`).
pub(super) fn cell_has_image(n: XmlNode) -> bool {
    n.descendants().any(|d| d.has_tag_name("image"))
}

/// Whether a nested table has any non-empty cell.
pub(super) fn table_has_content(table: XmlNode) -> bool {
    table
        .descendants()
        .filter(|n| n.has_tag_name("table-cell"))
        .any(|c| !plain_cell_text(c).trim().is_empty() || cell_has_image(c))
}

/// A plain cell's text: its paragraphs' unformatted text, blank-line-joined
/// (docling's `str(cell.value)` — subscripts inline, no Markdown markers).
pub(super) fn plain_cell_text(tc: XmlNode) -> String {
    tc.children()
        .filter(|c| c.has_tag_name("p") || c.has_tag_name("h"))
        .map(para_plain_text)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A paragraph's unformatted text, expanding `<text:s>`/`<text:tab>`/
/// `<text:line-break>` but dropping span formatting.
pub(super) fn para_plain_text(el: XmlNode) -> String {
    let mut out = String::new();
    para_plain_into(el, &mut out);
    out
}

fn para_plain_into(el: XmlNode, out: &mut String) {
    for child in el.children() {
        if child.is_text() {
            out.push_str(child.text().unwrap_or(""));
        } else if child.is_element() {
            match child.tag_name().name() {
                "line-break" => out.push('\n'),
                "tab" | "tab-stop" => out.push('\t'),
                "s" => {
                    let n: usize = attr(child, "c").and_then(|v| v.parse().ok()).unwrap_or(1);
                    out.push_str(&" ".repeat(n));
                }
                // Inline embedded documents and image payloads (flat ODF) are
                // not paragraph text.
                "object" | "binary-data" => {}
                // A footnote/endnote's citation marker and body live inside
                // this element, but neither belongs in the citing sentence's
                // own text (docling#4375): the body can be arbitrarily long,
                // and splicing it in here corrupted the reading order. The
                // body is recovered separately as a furniture item, see
                // [`note_texts`].
                "note" => {}
                // A reviewer comment's metadata and text (#829), see
                // [`annotations`].
                "annotation" | "annotation-end" => {}
                // A text box's blocks (#831) are their own items.
                "text-box" => {}
                _ => para_plain_into(child, out),
            }
        }
    }
}

/// Render a rich cell's block content to Markdown, then flatten to a single line
/// (docling's `RichTableCell` group serialized into the cell). A nested table is
/// flattened to its space-joined cell texts.
fn rich_cell_markdown(tc: XmlNode, styles: &Styles) -> String {
    let mut sub = DoclingDocument::new("");
    let mut prev_state: Option<ListCont> = None;
    for child in tc.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            n if is_list_tag(n) => {
                prev_state = add_odf_list(child, styles, &mut sub, 0, 1, false, prev_state.take());
            }
            "table" => {
                prev_state = None;
                if let Some(table) = parse_table(child, styles) {
                    let text = table
                        .rows
                        .iter()
                        .flatten()
                        .filter(|c| !c.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" ");
                    // A table part, not a text item: exempt from the GFM
                    // hard-line-break rule (docling-core#721), hence verbatim.
                    if !text.is_empty() {
                        sub.push(Node::TextDump(text));
                    }
                }
            }
            "p" | "h" | "frame" => {
                prev_state = None;
                handle_block(child, styles, &mut sub, 0, &mut Vec::new());
            }
            _ => {}
        }
    }
    // In-cell rendering: a heading inside the cell is plain text
    // (docling-core#540). Line breaks stay: the Markdown table serializer
    // flattens them to spaces (a hard break to three, as docling), while JSON
    // keeps the cell's raw newlines.
    sub.export_to_table_cell_markdown().trim().to_string()
}

// ---------------------------------------------------------------- spreadsheet

fn walk_spreadsheet(sheet: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for table in sheet.children().filter(|c| c.has_tag_name("table")) {
        let start = doc.nodes.len();
        add_ods_sheet(table, styles, doc);
        // A hidden sheet (`table:display="false"`) is kept on the invisible
        // layer, as the JSON tree has it (#753).
        if attr(table, "display") == Some("false") {
            hide_nodes_from(doc, start);
        }
    }
}

/// Move the body nodes pushed since `start` to the invisible layer: a list
/// item through its own layer (so a list stays one list), any other body
/// node wrapped in [`Node::Furniture`]; nodes already on another layer
/// (footnote bodies) keep it. A hidden section, slide or sheet is kept,
/// out of the default exports (#753).
fn hide_nodes_from(doc: &mut DoclingDocument, start: usize) {
    for node in &mut doc.nodes[start..] {
        match node {
            Node::Furniture { .. } => {}
            Node::ListItem { layer, .. } => {
                if layer.is_none() {
                    *layer = Some(docling_core::ContentLayer::Invisible);
                }
            }
            _ => {
                let inner = std::mem::replace(node, Node::PageBreak);
                *node = Node::Furniture {
                    layer: docling_core::ContentLayer::Invisible,
                    inner: Box::new(inner),
                };
            }
        }
    }
}

/// Split an ODS sheet into its disconnected data regions and emit each as a
/// separate table — a port of docling's `_convert_sheet_table` /
/// `_find_data_tables_in_sheet` (strict `gap_tolerance = 0` flood fill, singleton
/// cells kept as 1×1 tables). Numeric columns right-align via the shared table
/// serializer.
fn add_ods_sheet(table: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    // Build a sparse content grid: (row, col) → cell text, expanding
    // `number-{rows,columns}-repeated` (empty repeats only advance the index, so a
    // sheet padded to millions of empty cells stays cheap).
    let mut cells: HashMap<(usize, usize), String> = HashMap::new();
    // docling#4992: the rows holding a value, and the blank merged ranges
    // (inclusive row/column bounds) — see below.
    let mut rows_with_values: HashSet<usize> = HashSet::new();
    let mut blank_spans: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut row_idx = 0usize;
    for row in table.children().filter(|c| c.has_tag_name("table-row")) {
        let rrep = repeat(row, "number-rows-repeated");
        let mut row_cells: Vec<(usize, String)> = Vec::new();
        let mut col_idx = 0usize;
        let mut row_has_content = false;
        let mut row_has_value = false;
        for cell in row
            .children()
            .filter(|c| c.has_tag_name("table-cell") || c.has_tag_name("covered-table-cell"))
        {
            let crep = repeat(cell, "number-columns-repeated");
            let covered = cell.has_tag_name("covered-table-cell");
            // A cell holding a picture is a rich cell: its blocks, picture
            // included, as in the JSON tree (#758).
            let text = if cell_has_image(cell) {
                rich_cell_markdown(cell, styles)
            } else {
                ods_cell_text(cell)
            };
            if !text.is_empty() {
                row_has_value = true;
            } else if !covered {
                let rows = repeat(cell, "number-rows-spanned");
                let cols = repeat(cell, "number-columns-spanned");
                if rows > 1 || cols > 1 {
                    for r in 0..rrep.min(1024) {
                        for c in 0..crep.min(1024) {
                            let (top, left) = (row_idx + r, col_idx + c * cols);
                            blank_spans.push((top, top + rows - 1, left, left + cols - 1));
                        }
                    }
                }
            }
            if !text.is_empty() || covered {
                row_has_content = true;
                for c in 0..crep.min(1024) {
                    row_cells.push((col_idx + c, text.clone()));
                }
            }
            col_idx += crep;
        }
        if row_has_content {
            for r in 0..rrep.min(1024) {
                for (c, text) in &row_cells {
                    cells.insert((row_idx + r, *c), text.clone());
                }
            }
        }
        if row_has_value {
            rows_with_values.extend(row_idx..row_idx + rrep.min(1024));
        }
        row_idx += rrep;
    }
    // A covered cell continues its merge and counts as content — but a blank
    // merged range none of whose rows holds a value is a separator row: it
    // does not join the tables above and below it (docling#4992). Only the
    // positions the grid holds are visited, so an oversized declared span
    // stays cheap.
    for (top, bottom, left, right) in blank_spans {
        if rows_with_values.iter().any(|r| (top..=bottom).contains(r)) {
            continue;
        }
        cells.retain(|&(r, c), text| {
            !(text.is_empty() && (top..=bottom).contains(&r) && (left..=right).contains(&c))
        });
    }
    emit_sheet_regions(&cells, doc);
    // `_find_images_in_sheet`: the pictures anchored to the sheet itself, in
    // `<table:shapes>`, after its tables (#758, docling#4678).
    for img in table
        .children()
        .filter(|n| n.has_tag_name("shapes"))
        .flat_map(|shapes| shapes.descendants())
        .filter(|n| n.has_tag_name("image"))
    {
        if let Some(node) = odf_picture(styles, img) {
            doc.push(node);
        }
    }
}

/// Split a sparse cell grid into its disconnected data regions and emit each
/// as a table — the flood-fill core of [`add_ods_sheet`], shared with the
/// spreadsheet-interchange backends (DIF/SYLK/dBase, #216) so a `.dif` saved
/// from a sheet converts to the same tables as the `.ods` it came from.
pub(crate) fn emit_sheet_regions(
    cells: &HashMap<(usize, usize), String>,
    doc: &mut DoclingDocument,
) {
    if cells.is_empty() {
        return;
    }

    // Flood-fill connected content cells (4-directional, immediate neighbours
    // only) in row-major scan order, so region order matches docling's. The
    // scan visits the *content* cells in that order, not every position of
    // their bounding box: a sheet with one value in the first cell and one a
    // million rows and 16 000 columns away (1.7 KB of ODS thanks to
    // `number-*-repeated`) spans 17 billion positions, and walking them pinned
    // a core for minutes.
    let mut order: Vec<(usize, usize)> = cells.keys().copied().collect();
    order.sort_unstable();
    let mut visited: HashSet<(usize, usize)> = HashSet::new();
    for (ri, ci) in order {
        {
            if visited.contains(&(ri, ci)) {
                continue;
            }
            let mut region: HashSet<(usize, usize)> = HashSet::new();
            let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
            queue.push_back((ri, ci));
            region.insert((ri, ci));
            let (mut rmin, mut rmax, mut cmin, mut cmax) = (ri, ri, ci, ci);
            while let Some((r, c)) = queue.pop_front() {
                rmin = rmin.min(r);
                rmax = rmax.max(r);
                cmin = cmin.min(c);
                cmax = cmax.max(c);
                for (dr, dc) in [(0i64, 1i64), (0, -1), (1, 0), (-1, 0)] {
                    let nr = r as i64 + dr;
                    let nc = c as i64 + dc;
                    if nr < 0 || nc < 0 {
                        continue;
                    }
                    let key = (nr as usize, nc as usize);
                    if !region.contains(&key) && cells.contains_key(&key) {
                        region.insert(key);
                        queue.push_back(key);
                    }
                }
            }
            visited.extend(region.iter().copied());

            let rows: Vec<Vec<String>> = (rmin..=rmax)
                .map(|r| {
                    (cmin..=cmax)
                        .map(|c| cells.get(&(r, c)).cloned().unwrap_or_default())
                        .collect()
                })
                .collect();
            doc.push(Node::Table(Table {
                rows,
                location: None,
                structure: None,
                cell_blocks: None,
                cells: None,
                caption: None,
                caption_parent: Default::default(),
                caption_location: None,
            }));
        }
    }
}

/// A `number-*-repeated` attribute, at least 1.
pub(super) fn repeat(node: XmlNode, name: &str) -> usize {
    attr(node, name)
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(1)
}

/// An ODS cell's plain text — its paragraphs' text, newline-joined (github tables
/// are unescaped, matching docling's `_odf_cell_text` display).
pub(super) fn ods_cell_text(cell: XmlNode) -> String {
    cell.children()
        .filter(|c| c.has_tag_name("p") || c.has_tag_name("h"))
        .map(|p| {
            p.descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- presentation

/// Port of docling's `OdpDocumentBackend`: each `<draw:page>` gets its slide
/// name as a title when no element on it is a visible title (a frame with
/// `presentation:class="title"`, or a `draw:custom-shape` carrying the slide's
/// first text); speaker notes (`<presentation:notes>`) go to the notes
/// layer (#862) and animations are dropped; a title element's paragraphs
/// become the slide title.
fn walk_presentation(pres: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    for (idx, page) in pres
        .children()
        .filter(|c| c.has_tag_name("page"))
        .enumerate()
    {
        let name = attr(page, "name")
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("slide-{}", idx + 1));
        let start = doc.nodes.len();
        if !slide_has_visible_title(page) {
            doc.push(Node::Heading {
                level: 1,
                text: name,
            });
        }
        walk_slide(page, styles, doc);
        if slide_is_hidden(page, styles) {
            hide_nodes_from(doc, start);
        }
    }
}

/// docling's `_odf_image_can_be_bitmap`: an explicit `draw:mime-type` decides
/// (raster `image/*` only); otherwise the href's suffix — vector/preview
/// formats (`.svm`, `.svg`, `.emf`, `.wmf`, `.pdf`) never yield a picture.
/// A flat-ODF image with no href at all (#215) carries its payload inline as
/// `<office:binary-data>`, so the decoded magic bytes decide instead — that is
/// how the SVM previews flat files keep alongside their real bitmaps stay out.
pub(super) fn image_can_be_bitmap(img: XmlNode, href: &str) -> bool {
    if let Some(mime) = attr(img, "mime-type") {
        return mime.starts_with("image/") && mime != "image/svg+xml";
    }
    if href.is_empty() {
        if let Some(data) = img
            .children()
            .find(|c| c.has_tag_name("binary-data"))
            .and_then(|d| d.text())
        {
            let head: String = data
                .chars()
                .filter(|c| !c.is_whitespace())
                .take(16)
                .collect();
            return docling_core::base64::decode(&head)
                .map(|bytes| is_raster_magic(&bytes))
                .unwrap_or(false);
        }
        return false;
    }
    let name = href.rsplit('/').next().unwrap_or(href);
    let suffix = match name.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => String::new(),
    };
    // No empty suffix (docling#4015): an extension-less path is what system
    // files look like (`/etc/passwd`), never a valid ODF image reference.
    matches!(
        suffix.as_str(),
        "bmp" | "gif" | "jpeg" | "jpg" | "png" | "tif" | "tiff" | "webp"
    )
}

/// The magic bytes of the raster formats a picture placeholder is worth:
/// PNG, JPEG, GIF, BMP, TIFF or WebP. Vector previews (SVM starts `VCLMTF`)
/// and anything unrecognized are not bitmaps.
fn is_raster_magic(b: &[u8]) -> bool {
    b.starts_with(&[0x89, b'P', b'N', b'G'])
        || b.starts_with(&[0xFF, 0xD8, 0xFF])
        || b.starts_with(b"GIF8")
        || b.starts_with(b"BM")
        || b.starts_with(&[b'I', b'I', 0x2A, 0x00])
        || b.starts_with(&[b'M', b'M', 0x00, 0x2A])
        || (b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"))
}

/// Any non-blank text anywhere under the element (docling's
/// `_clean_odf_text_lines(text_recursive)` non-emptiness).
pub(super) fn element_has_text(el: XmlNode) -> bool {
    el.descendants()
        .filter(|n| n.is_text())
        .any(|n| !n.text().unwrap_or("").trim().is_empty())
}

/// docling's `_is_slide_title_element`: an explicit `presentation:class="title"`,
/// or a `draw:custom-shape` holding the slide's first text content.
pub(super) fn is_slide_title_element(el: XmlNode, is_first_text_content: bool) -> bool {
    if attr(el, "class") == Some("title") {
        return true;
    }
    is_first_text_content && el.tag_name().name() == "custom-shape"
}

pub(super) fn slide_has_visible_title(page: XmlNode) -> bool {
    let mut seen_text = false;
    for el in page.children().filter(XmlNode::is_element) {
        let tag = el.tag_name().name();
        if tag == "notes" || tag == "par" {
            continue;
        }
        if is_slide_title_element(el, !seen_text) {
            return true;
        }
        if element_has_text(el) {
            seen_text = true;
        }
    }
    false
}

fn walk_slide(page: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    let mut seen_text = false;
    for el in page.children().filter(XmlNode::is_element) {
        let tag = el.tag_name().name();
        // Speaker notes go to the notes layer (#862, docling#4869): the
        // body walk of the notes' text boxes, each node wrapped on that
        // layer — exports leave it out by default. Animation trees never
        // reach the document.
        if tag == "notes" {
            walk_slide_notes(el, styles, doc);
            continue;
        }
        if tag == "par" {
            continue;
        }
        let has_text = element_has_text(el);
        let is_title = is_slide_title_element(el, !seen_text);
        if has_text {
            seen_text = true;
        }
        if tag == "frame" {
            walk_slide_frame(el, styles, doc, is_title);
        } else {
            walk_textbox_children(
                el.children().filter(XmlNode::is_element),
                styles,
                doc,
                is_title,
            );
        }
    }
}

/// `_walk_slide_notes` for the flat nodes: the notes page's text boxes —
/// not its thumbnail or field placeholders — walked like a slide's, each
/// resulting node on the notes layer.
fn walk_slide_notes(notes: XmlNode, styles: &Styles, doc: &mut DoclingDocument) {
    let mut sub = DoclingDocument::new("");
    for el in notes.children().filter(XmlNode::is_element) {
        if attr(el, "class").is_some_and(|c| super::odf_tree::NOTES_PAGE_FIELD_CLASSES.contains(&c))
        {
            continue;
        }
        if el.has_tag_name("frame") {
            for tb in el.descendants().filter(|n| n.has_tag_name("text-box")) {
                walk_textbox_children(
                    tb.children().filter(XmlNode::is_element),
                    styles,
                    &mut sub,
                    false,
                );
            }
        } else {
            walk_textbox_children(
                el.children().filter(XmlNode::is_element),
                styles,
                &mut sub,
                false,
            );
        }
    }
    for node in sub.nodes {
        doc.push(Node::Furniture {
            layer: docling_core::ContentLayer::Notes,
            inner: Box::new(node),
        });
    }
}

/// docling's `_walk_slide_frame` order: embedded charts, tables, images (the
/// chart's `ObjectReplacements/` preview is skipped once the chart itself is
/// emitted), then text boxes.
fn walk_slide_frame(frame: XmlNode, styles: &Styles, doc: &mut DoclingDocument, is_title: bool) {
    let mut chart_count = 0usize;
    if let Some(obj) = frame.children().find(|c| c.has_tag_name("object")) {
        let name = attr(obj, "href").unwrap_or("").trim_start_matches("./");
        let info = styles
            .charts
            .get(name)
            .cloned()
            .or_else(|| inline_chart(obj, styles));
        if let Some(info) = info {
            doc.push(Node::Chart {
                kind: info.kind,
                table: info.table,
                caption: None,
                location: None,
            });
            chart_count += 1;
        }
    }
    // An inline chart's data table lives under the `<draw:object>` (flat ODF)
    // — already rendered as the chart above, so skip it here.
    for tbl in frame
        .descendants()
        .filter(|n| n.has_tag_name("table") && !n.ancestors().any(|a| a.has_tag_name("object")))
    {
        if let Some(t) = parse_table(tbl, styles) {
            doc.push(Node::Table(t));
        }
    }
    for img in frame.descendants().filter(|n| n.has_tag_name("image")) {
        let href = attr(img, "href").unwrap_or("");
        // The chart's preview bitmap — an `ObjectReplacements/` part, or (flat
        // ODF) an hrefless sibling holding inline `<office:binary-data>` —
        // never doubles as a picture once the chart itself is emitted.
        if chart_count > 0
            && (href.is_empty()
                || href
                    .trim_start_matches("./")
                    .starts_with("ObjectReplacements/"))
        {
            continue;
        }
        if let Some(node) = odf_picture(styles, img) {
            doc.push(node);
        }
    }
    for tb in frame.descendants().filter(|n| n.has_tag_name("text-box")) {
        walk_textbox_children(
            tb.children().filter(XmlNode::is_element),
            styles,
            doc,
            is_title,
        );
    }
}

/// docling's `_walk_textbox_children`: headings keep their outline level, a
/// title element's paragraphs become the slide TITLE (a `#` heading), other
/// paragraphs plain text, and sibling lists continue their numbering.
fn walk_textbox_children<'a, 'i: 'a>(
    els: impl Iterator<Item = XmlNode<'a, 'i>>,
    styles: &Styles,
    doc: &mut DoclingDocument,
    is_title: bool,
) {
    let mut prev_state: Option<ListCont> = None;
    for el in els {
        match el.tag_name().name() {
            "h" => {
                prev_state = None;
                handle_block(el, styles, doc, 0, &mut Vec::new());
            }
            "p" => {
                prev_state = None;
                let mut runs = Vec::new();
                collect_runs(el, styles, Fmt::default(), &mut runs);
                let text = runs_to_text(runs.clone());
                if text.is_empty() {
                    continue;
                }
                if is_title {
                    doc.push(Node::Heading { level: 1, text });
                } else {
                    doc.push(inline_paragraph_node(text, runs_to_inline(runs), false));
                }
            }
            n if is_list_tag(n) => {
                prev_state = add_odf_list(el, styles, doc, 0, 1, false, prev_state.take());
            }
            _ => {}
        }
    }
}

/// Attribute by local name (ODF attributes are namespaced, e.g. `text:style-name`).
pub(super) fn attr<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two cells a million rows and 16 000 columns apart: the region scan
    /// visits the cells, not the 17-billion-position box between them.
    #[test]
    fn sheet_regions_scan_cells_not_the_bounding_box() {
        let mut cells: HashMap<(usize, usize), String> = HashMap::new();
        cells.insert((0, 0), "a".into());
        cells.insert((1_000_000, 16_000), "b".into());
        let mut doc = DoclingDocument::new("s");
        let t = std::time::Instant::now();
        emit_sheet_regions(&cells, &mut doc);
        assert!(t.elapsed().as_secs() < 5);
        let tables = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Table(_)))
            .count();
        assert_eq!(tables, 2);
    }

    #[test]
    fn formatting_resolves_through_parent_chain() {
        // P2 → P1 → Strong (bold). T1 adds italic directly.
        let content = r#"<root xmlns:style="s" xmlns:fo="f">
            <style:style style:name="Strong" style:family="text">
              <style:text-properties fo:font-weight="bold"/></style:style>
            <style:style style:name="P1" style:family="text" style:parent-style-name="Strong"/>
            <style:style style:name="P2" style:family="text" style:parent-style-name="P1"/>
            <style:style style:name="T1" style:family="text">
              <style:text-properties fo:font-style="italic"/></style:style>
          </root>"#;
        let dom = Document::parse(content).unwrap();
        let styles = parse_styles(&dom, None);
        let f = resolve_fmt(&styles, Some("P2"), Fmt::default());
        assert!(f.bold && !f.italic, "bold inherited through P2→P1→Strong");
        let t = resolve_fmt(&styles, Some("T1"), Fmt::default());
        assert!(t.italic && !t.bold);
    }

    /// docling PR #3850 / #255: text trailing an inline child (its lxml
    /// `tail`) is kept — "with <span>bold</span>, and <span>italic</span>
    /// formatting" used to lose ", and" and " formatting", and a tail after a
    /// `<text:tab>` was dropped entirely.
    #[test]
    fn text_after_inline_elements_survives() {
        let xml = r#"<root xmlns:text="x">
            <text:p>with <text:span>bold</text:span>, and <text:span>italic</text:span> formatting</text:p>
            <text:p>Fassung:<text:tab/>zweite  Durchsicht</text:p></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("t");
        walk_text(dom.root_element(), &styles, &mut doc);
        let md = doc.export_to_markdown();
        // The unstyled spans merge with their tails into one run, so no
        // space-join artifacts here; the styled corpus fixture keeps them
        // (`**bold** ,  underline , …`) and is pinned by the regression suite.
        assert!(
            md.contains("with bold, and italic formatting"),
            "tails around spans survive:\n{md}"
        );
        assert!(
            md.contains("zweite  Durchsicht"),
            "tail after a tab survives:\n{md}"
        );
    }

    /// Build a minimal packaged ODT around one `content.xml` body fragment
    /// (`<office:text>` children), optionally with extra parts.
    fn odt_bytes(body: &str, parts: &[(&str, &[u8])]) -> Vec<u8> {
        let content = format!(
            r#"<office:document-content xmlns:office="urn:o" xmlns:text="x"
              xmlns:draw="d" xmlns:xlink="l">
            <office:body><office:text>{body}</office:text></office:body></office:document-content>"#
        );
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        use std::io::Write;
        zip.start_file("mimetype", opts).unwrap();
        zip.write_all(b"application/vnd.oasis.opendocument.text")
            .unwrap();
        zip.start_file("content.xml", opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        for (name, data) in parts {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn odt_markdown(body: &str) -> String {
        let source = crate::SourceDocument::from_bytes(
            "t.odt",
            crate::InputFormat::Odt,
            odt_bytes(body, &[]),
        );
        OdfBackend
            .convert(&source)
            .expect("converts")
            .export_to_markdown()
            .trim()
            .to_string()
    }

    /// docling#3949 (2.120): `<text:a>` runs carry their target into the
    /// Markdown as a link; the whitespace at the link boundary is stripped so
    /// it doesn't double up with the inline separator.
    #[test]
    fn odt_hyperlinks_render_as_markdown_links() {
        assert_eq!(
            odt_markdown(
                r#"<text:p>Watch <text:a xlink:href="https://example.com/talk">the example talk</text:a><text:span> for context.</text:span></text:p>"#
            ),
            "Watch [the example talk](https://example.com/talk) for context."
        );
        // Headings and titles keep a linked run inside the block.
        assert_eq!(
            odt_markdown(
                r##"<text:h text:outline-level="2">Read <text:a xlink:href="#details">details</text:a> now</text:h>"##
            ),
            "### Read [details](#details) now"
        );
        // A linked list item.
        assert_eq!(
            odt_markdown(
                r#"<text:list><text:list-item><text:p><text:a xlink:href="list-target.odt#part">linked list item</text:a></text:p></text:list-item></text:list>"#
            ),
            "- [linked list item](list-target.odt#part)"
        );
    }

    /// docling's `_odf_hyperlink_from_href`: blank / scheme-looking-but-invalid
    /// targets yield no link, relative targets are kept, a missing `xlink:href`
    /// leaves the anchor transparent.
    #[test]
    fn odt_hyperlink_targets_are_classified_like_docling() {
        for (href, expected) in [
            ("   ", "linked text"),
            ("http://[::1", "linked text"),
            ("http://", "linked text"),
            ("http:", "linked text"),
            ("guide.odt#part", "[linked text](guide.odt#part)"),
            ("#part", "[linked text](#part)"),
            ("https://example.com", "[linked text](https://example.com/)"),
        ] {
            assert_eq!(
                odt_markdown(&format!(
                    r#"<text:p><text:a xlink:href="{href}">linked text</text:a></text:p>"#
                )),
                expected,
                "href {href:?}"
            );
        }
        assert_eq!(
            odt_markdown(r#"<text:p>Before <text:a>malformed link</text:a> after</text:p>"#),
            "Before malformed link after"
        );
    }

    /// docling#4015 (2.120.3): a `draw:image` whose `xlink:href` points outside
    /// the package is never read from the filesystem and yields no picture —
    /// with or without an extension — while an embedded part still does.
    #[test]
    fn odt_external_image_references_yield_no_picture() {
        let frame = |href: &str| {
            format!(
                r#"<text:p><draw:frame><draw:image xlink:href="{href}"/></draw:frame></text:p>"#
            )
        };
        assert_eq!(odt_markdown(&frame("/tmp/canary.png")), "");
        assert_eq!(odt_markdown(&frame("/etc/passwd")), "");
        assert_eq!(odt_markdown(&frame("../outside.png")), "");
        // An http(s) reference is a fetch — off by default, so no picture.
        assert_eq!(odt_markdown(&frame("https://example.com/x.png")), "");
        // An embedded part is a picture, as before.
        let source = crate::SourceDocument::from_bytes(
            "t.odt",
            crate::InputFormat::Odt,
            odt_bytes(&frame("Pictures/1.png"), &[("Pictures/1.png", b"\x89PNG")]),
        );
        let md = OdfBackend.convert(&source).unwrap().export_to_markdown();
        assert_eq!(md.trim(), "<!-- image -->");
    }

    /// docling PR #3876 / #255: a `<draw:object>` whose package part is
    /// missing must be skipped, never abort the conversion — text around it
    /// still converts, and no phantom chart/picture is emitted.
    #[test]
    fn dangling_embedded_object_is_skipped() {
        let content = r#"<office:document-content xmlns:office="urn:o" xmlns:text="x"
              xmlns:draw="d" xmlns:xlink="l">
            <office:body><office:text>
              <text:p>Before the dangling object.</text:p>
              <text:p><draw:frame draw:name="dangling">
                <draw:object xlink:href="./Object 1"/></draw:frame></text:p>
              <text:p>After the dangling object.</text:p>
            </office:text></office:body></office:document-content>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        use std::io::Write;
        zip.start_file("mimetype", opts).unwrap();
        zip.write_all(b"application/vnd.oasis.opendocument.text")
            .unwrap();
        zip.start_file("content.xml", opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let source =
            crate::SourceDocument::from_bytes("dangling.odt", crate::InputFormat::Odt, bytes);
        let doc = OdfBackend
            .convert(&source)
            .expect("conversion must not abort");
        let md = doc.export_to_markdown();
        assert!(md.contains("Before the dangling object."), "{md}");
        assert!(md.contains("After the dangling object."), "{md}");
        assert!(!md.contains("<!-- image -->"), "no phantom picture:\n{md}");
    }

    /// docling PR #3852 / #178: content inside a `<text:section>` (including a
    /// nested section) is walked as ordinary body blocks, not dropped.
    #[test]
    fn section_content_is_walked() {
        let xml = r#"<root xmlns:text="x" xmlns:table="t">
            <text:p>Outside.</text:p>
            <text:section text:name="S1">
              <text:h text:outline-level="1">Inside heading</text:h>
              <text:p>Inside paragraph.</text:p>
              <text:section text:name="Nested">
                <text:p>Nested paragraph.</text:p>
              </text:section>
            </text:section></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("s");
        walk_text(dom.root_element(), &styles, &mut doc);
        let md = doc.export_to_markdown();
        assert!(md.contains("Outside."), "pre-section text:\n{md}");
        assert!(md.contains("## Inside heading"), "section heading:\n{md}");
        assert!(md.contains("Inside paragraph."), "section paragraph:\n{md}");
        assert!(
            md.contains("Nested paragraph."),
            "nested section content:\n{md}"
        );
    }

    /// #823 (docling#4750): a table of contents — and every other index —
    /// is transparent like a section: its title and entries, and whatever
    /// LibreOffice put inside the index body (an unclosed Word TOC field
    /// swallows the following headings, paragraphs and tables), walk as
    /// body blocks; the `*-source` template is not content; a hidden index
    /// goes to the invisible layer.
    #[test]
    fn index_body_content_is_walked() {
        let xml = r#"<root xmlns:text="x" xmlns:table="t">
            <text:p>Intro.</text:p>
            <text:table-of-content text:name="TOC">
              <text:table-of-content-source text:outline-level="10">
                <text:index-title-template>Template title</text:index-title-template>
              </text:table-of-content-source>
              <text:index-body>
                <text:index-title text:name="TOC_Head"><text:p>Contents</text:p></text:index-title>
                <text:p>Chapter One<text:tab/>1</text:p>
                <text:h text:outline-level="1">Chapter One</text:h>
                <table:table><table:table-row>
                  <table:table-cell><text:p>Cell</text:p></table:table-cell>
                </table:table-row></table:table>
              </text:index-body>
            </text:table-of-content>
            <text:alphabetical-index text:name="Idx" text:display="none">
              <text:index-body><text:p>Hidden entry</text:p></text:index-body>
            </text:alphabetical-index>
            <text:p>Outro.</text:p></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("s");
        walk_text(dom.root_element(), &styles, &mut doc);
        assert_eq!(
            doc.export_to_markdown(),
            "Intro.\n\nContents\n\nChapter One\t1\n\n## Chapter One\n\n| Cell   |\n|--------|\n\nOutro.\n"
        );
        assert!(doc.nodes.iter().any(|n| matches!(
            n,
            Node::Furniture {
                layer: docling_core::ContentLayer::Invisible,
                ..
            }
        )));
        for container in [
            "table-of-content",
            "user-index",
            "bibliography",
            "index-body",
        ] {
            assert!(is_transparent_container(container));
        }
        assert!(!is_transparent_container("table-of-content-source"));
    }

    /// #826: paragraphs numbered through their paragraph style's
    /// `style:list-style-name` — no `<text:list>` — are list items: a run of
    /// them is one list, a change of list style a new one, the style
    /// inherited through `parent-style-name` and an automatic style; the
    /// numbered level's `start-value` and `num-suffix` apply.
    #[test]
    fn style_list_name_paragraphs_are_list_items() {
        let xml = r#"<root xmlns:text="x" xmlns:style="s">
            <office:styles xmlns:office="o">
              <text:list-style style:name="LB"><text:list-level-style-bullet text:level="1" text:bullet-char="•"/></text:list-style>
              <text:list-style style:name="LN"><text:list-level-style-number text:level="1" style:num-format="1" style:num-suffix=")" text:start-value="3"/></text:list-style>
              <style:style style:name="Bul" style:family="paragraph" style:list-style-name="LB"/>
              <style:style style:name="Inh" style:family="paragraph" style:parent-style-name="Bul"/>
              <style:style style:name="P1" style:family="paragraph" style:parent-style-name="Inh"/>
              <style:style style:name="Num" style:family="paragraph" style:list-style-name="LN"/>
              <style:style style:name="Off" style:family="paragraph" style:parent-style-name="Num" style:list-style-name=""/>
            </office:styles>
            <text:p>Intro:</text:p>
            <text:p text:style-name="Bul">one</text:p>
            <text:p text:style-name="Inh">two</text:p>
            <text:p text:style-name="P1">three</text:p>
            <text:p text:style-name="Num">four</text:p>
            <text:p text:style-name="Num">five</text:p>
            <text:p text:style-name="Off">plain again</text:p></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        assert_eq!(
            paragraph_list_style(&styles, Some("P1")),
            Some(("LB".to_string(), 1))
        );
        assert_eq!(paragraph_list_style(&styles, Some("Off")), None);
        assert_eq!(paragraph_list_style(&styles, Some("nope")), None);
        let mut doc = DoclingDocument::new("s");
        walk_text(dom.root_element(), &styles, &mut doc);
        assert_eq!(
            doc.export_to_markdown(),
            "Intro:\n\n- one\n- two\n- three\n\n- 3) four\n- 4) five\n\nplain again\n"
        );
    }

    /// #827: a `<text:h>` inside a list item is a heading of its outline
    /// level, not a list item — a list of headings alone prints no list,
    /// and a heading item among bullet items keeps the bullets a list.
    #[test]
    fn headings_inside_list_items_are_headings() {
        let xml = r#"<root xmlns:text="x">
            <text:list text:style-name="Outline">
              <text:list-item>
                <text:h text:outline-level="1">Context</text:h>
                <text:list><text:list-item><text:h text:outline-level="2">Background</text:h></text:list-item></text:list>
              </text:list-item>
            </text:list>
            <text:p>Body one.</text:p>
            <text:list text:style-name="Outline" text:continue-numbering="true">
              <text:list-item><text:list><text:list-item><text:h text:outline-level="2">Target</text:h></text:list-item></text:list></text:list-item>
            </text:list>
            <text:list>
              <text:list-item><text:h text:outline-level="3">Inside bullets</text:h></text:list-item>
              <text:list-item><text:p>bullet</text:p></text:list-item>
            </text:list></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("s");
        walk_text(dom.root_element(), &styles, &mut doc);
        assert_eq!(
            doc.export_to_markdown(),
            "## Context\n\n### Background\n\nBody one.\n\n### Target\n\n#### Inside bullets\n\n- bullet\n"
        );
    }

    /// #829 (docling#4436): a comment's author, date, initials and text
    /// stay out of the annotated heading, paragraph and list item; each
    /// becomes a notes-layer `comment_section` with docling's note text,
    /// a reply joining its parent's thread, and the annotated node carries
    /// the back-references.
    #[test]
    fn annotations_are_notes_not_body_text() {
        let xml = r#"<root xmlns:text="x" xmlns:office="o" xmlns:dc="d" xmlns:loext="l">
            <text:h text:outline-level="1">Budget<office:annotation office:name="c1"><dc:creator>Alpha</dc:creator><dc:date>2026-01-15T10:00:00</dc:date><loext:sender-initials>RA</loext:sender-initials><text:p>Rename</text:p></office:annotation></text:h>
            <text:p>Total is <office:annotation office:name="c2"><dc:creator>Alpha</dc:creator><text:p>Verify</text:p><text:p>twice</text:p></office:annotation>500 EUR.<office:annotation-end office:name="c2"/></text:p>
            <text:p>Reply first: <office:annotation loext:parent-name="c1"><dc:creator>Bravo</dc:creator><dc:date>2026-01-16T09:00:00</dc:date><text:p>Agreed</text:p></office:annotation>text.</text:p>
            <text:list><text:list-item><text:p>Item<office:annotation><text:p>Bare</text:p></office:annotation></text:p></text:list-item></text:list></root>"#;
        let dom = Document::parse(xml).unwrap();
        let comments = annotations(dom.root_element());
        assert_eq!(
            comments.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            [
                "[author: Alpha (RA), time: 2026-01-15T10:00:00]: Rename",
                "[author: Alpha]: Verify\ntwice",
                "[author: Bravo, time: 2026-01-16T09:00:00]: Agreed",
                "Bare",
            ]
        );
        assert_eq!(comment_threads(&comments), [vec![0, 2], vec![1], vec![3]]);
        let mut styles = parse_styles(&dom, None);
        styles.comment_slots = comment_slots(&comments);
        let mut doc = DoclingDocument::new("s");
        walk_text(dom.root_element(), &styles, &mut doc);
        push_comment_sections(&comments, &mut doc);
        assert_eq!(
            doc.export_to_markdown(),
            "## Budget\n\nTotal is 500 EUR.\n\nReply first: text.\n\n- Item\n"
        );
        let wrapped: Vec<Vec<usize>> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Commented { comments, .. } => Some(comments.clone()),
                _ => None,
            })
            .collect();
        // Slots are positions in thread order: c1 0, its reply 1, c2 2, Bare 3.
        assert_eq!(wrapped, [vec![0], vec![2], vec![1], vec![3]]);
        let sections: Vec<(String, String)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::CommentSection { name, text, .. } => Some((name.clone(), text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].0, "comment-1");
        assert_eq!(sections[1].0, "comment-1");
        assert_eq!(sections[2].0, "comment-2");
        assert_eq!(sections[3].0, "comment-3");
        assert!(sections[1].1.ends_with("Agreed"));
    }

    /// #830: the master pages' headers and footers are furniture after the
    /// body — their paragraphs and tables walked as blocks, headers before
    /// footers — out of the Markdown; a hidden variant and an empty one
    /// are not.
    #[test]
    fn master_page_headers_and_footers_are_furniture() {
        let xml = r#"<office:document xmlns:office="o" xmlns:text="x" xmlns:style="s" xmlns:table="t">
            <office:master-styles>
              <style:master-page style:name="Standard">
                <style:header><text:p>Head <text:page-number>1</text:page-number></text:p>
                  <table:table><table:table-row><table:table-cell><text:p>Version</text:p></table:table-cell></table:table-row></table:table>
                </style:header>
                <style:header-left style:display="false"><text:p>Unused</text:p></style:header-left>
                <style:footer><text:p>Foot</text:p></style:footer>
              </style:master-page>
              <style:master-page style:name="Endnote"><style:header/><style:footer><text:p> </text:p></style:footer></style:master-page>
            </office:master-styles>
            <office:body><office:text><text:p>Body.</text:p></office:text></office:body></office:document>"#;
        let dom = Document::parse(xml).unwrap();
        let parts = master_page_parts(&dom, None);
        assert_eq!(
            parts.iter().map(|p| p.footer).collect::<Vec<_>>(),
            [false, true]
        );
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("s");
        push_master_page_furniture(&parts, &styles, &mut doc);
        let furniture: Vec<String> = doc
            .nodes
            .iter()
            .map(|n| match n {
                Node::Furniture { inner, .. } => match &**inner {
                    Node::Paragraph { text } => text.clone(),
                    Node::Table(t) => format!("table {}", t.rows[0].join("|")),
                    other => format!("{other:?}"),
                },
                other => format!("body {other:?}"),
            })
            .collect();
        assert_eq!(furniture, ["Head 1", "table Version", "Foot"]);
        assert_eq!(doc.export_to_markdown(), "");
    }

    /// #831: a `<draw:text-box>`'s paragraphs, list and table are blocks
    /// after the anchoring paragraph, not its text; a paragraph that only
    /// anchors a box still yields the box; a box inside a box walks with
    /// the outer box's paragraph; a block-level frame's box walks too.
    #[test]
    fn text_box_blocks_are_separate_items() {
        let xml = r#"<root xmlns:text="x" xmlns:draw="d" xmlns:table="t">
            <text:p>Anchor.<draw:frame><draw:text-box><text:p>First.</text:p><text:list><text:list-item><text:p>item</text:p></text:list-item></text:list><table:table><table:table-row><table:table-cell><text:p>Cell</text:p></table:table-cell></table:table-row></table:table></draw:text-box></draw:frame> tail.</text:p>
            <text:p><draw:frame><draw:text-box><text:p>Only a box.<draw:frame><draw:text-box><text:p>Inner.</text:p></draw:text-box></draw:frame></text:p></draw:text-box></draw:frame></text:p>
            <draw:frame><draw:text-box><text:h text:outline-level="1">Boxed heading</text:h></draw:text-box></draw:frame>
            <text:p>After.</text:p></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("s");
        walk_text(dom.root_element(), &styles, &mut doc);
        assert_eq!(
            doc.export_to_markdown(),
            "Anchor. tail.\n\nFirst.\n\n- item\n\n| Cell   |\n|--------|\n\nOnly a box.\n\nInner.\n\n## Boxed heading\n\nAfter.\n"
        );
        let p = dom.root_element().first_element_child().unwrap();
        assert_eq!(text_boxes(p).len(), 1);
        assert_eq!(para_plain_text(p), "Anchor. tail.");
    }

    /// #834: a frame's `<svg:title>` / `<svg:desc>` is the picture's alt
    /// text (and, for parity with docling's reading, still paragraph
    /// text); its `svg:width` / `svg:height` the display size in inches.
    #[test]
    fn frame_title_is_the_pictures_alt_text() {
        let xml = r#"<root xmlns:text="x" xmlns:draw="d" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:xlink="l">
            <text:p>Before <draw:frame draw:name="I" svg:width="2.5in" svg:height="3.175cm"><draw:image xlink:href="Pictures/x.png"/><svg:title>A gateway</svg:title><svg:desc>Longer</svg:desc></draw:frame> after.<text:title>Doc title</text:title></text:p></root>"#;
        let dom = Document::parse(xml).unwrap();
        let p = dom.root_element().first_element_child().unwrap();
        assert_eq!(para_plain_text(p), "Before A gatewayLonger after.Doc title");
        assert!(is_svg(
            p.descendants()
                .find(|n| n.has_tag_name("title") && n.tag_name().namespace().is_some())
                .unwrap()
        ));
        let img = p.descendants().find(|n| n.has_tag_name("image")).unwrap();
        assert_eq!(frame_alt(img).as_deref(), Some("A gateway"));
        let [w, h] = frame_display(img).unwrap();
        assert!((w - 2.5).abs() < 1e-5 && (h - 1.25).abs() < 1e-5, "{w} {h}");
        assert_eq!(odf_length_in("72pt"), Some(1.0));
        assert_eq!(odf_length_in("96px"), Some(1.0));
        assert_eq!(odf_length_in("10%"), None);
    }

    /// #835: the tree marker carries the level's number format, prefix and
    /// suffix.
    #[test]
    fn odf_markers_follow_num_format_and_affixes() {
        let level = |format: &str, prefix: &str, suffix: &str| OdfLevel {
            numbered: true,
            start: 1,
            prefix: prefix.into(),
            suffix: suffix.into(),
            format: format.into(),
        };
        assert_eq!(odf_marker(1, Some(&level("a", "", ")"))), "a)");
        assert_eq!(odf_marker(3, Some(&level("i", "", "."))), "iii.");
        assert_eq!(odf_marker(4, Some(&level("I", "", ""))), "IV.");
        assert_eq!(odf_marker(3, Some(&level("1", "(", ")"))), "(3)");
        assert_eq!(odf_marker(27, Some(&level("A", "", "."))), "AA.");
        assert_eq!(odf_marker(2, Some(&level("", "", ""))), "2.");
        assert_eq!(odf_marker(2, None), "2.");
    }

    #[test]
    fn ods_sheet_splits_into_regions() {
        // A title cell (isolated by an empty row) and a 2×2 data block become two
        // separate tables (strict gap-tolerance flood fill).
        let xml = r#"<root xmlns:table="t" xmlns:text="x">
          <table:table>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>Title</text:p></table:table-cell></table:table-row>
            <table:table-row><table:table-cell/></table:table-row>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>H1</text:p></table:table-cell>
              <table:table-cell><text:p>H2</text:p></table:table-cell></table:table-row>
            <table:table-row><table:table-cell/>
              <table:table-cell><text:p>1</text:p></table:table-cell>
              <table:table-cell><text:p>2</text:p></table:table-cell></table:table-row>
          </table:table></root>"#;
        let dom = Document::parse(xml).unwrap();
        let table = dom.descendants().find(|n| n.has_tag_name("table")).unwrap();
        let mut doc = DoclingDocument::new("t");
        add_ods_sheet(table, &parse_styles(&dom, None), &mut doc);
        let tables: Vec<&Table> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 2, "title singleton + data region");
        assert_eq!(tables[0].rows, vec![vec!["Title".to_string()]]);
        assert_eq!(tables[1].rows.len(), 2, "header + one data row");
        assert_eq!(tables[1].rows[0], vec!["H1".to_string(), "H2".to_string()]);
    }

    /// docling#4992: a blank merged row spanning the sheet only separates
    /// the tables above and below it; a value in its row makes it part of
    /// one table again.
    #[test]
    fn blank_merged_separator_row_splits_tables() {
        let shapes = |note: &str| {
            let cell = |t: &str| {
                if t.is_empty() {
                    "<table:table-cell/>".to_string()
                } else {
                    format!("<table:table-cell><text:p>{t}</text:p></table:table-cell>")
                }
            };
            let row = |cells: &[&str]| {
                format!(
                    "<table:table-row>{}</table:table-row>",
                    cells.iter().map(|t| cell(t)).collect::<String>()
                )
            };
            let xml = format!(
                r#"<root xmlns:table="t" xmlns:text="x"><table:table>{}{}{}<table:table-row><table:table-cell table:number-columns-spanned="3"/><table:covered-table-cell/><table:covered-table-cell/>{}</table:table-row>{}{}</table:table></root>"#,
                row(&["", "2025", "2026"]),
                row(&["GDP", "1.5", "2"]),
                row(&["CPI", "3.1", "2.9"]),
                cell(note),
                row(&["", "Q1", "Q2"]),
                row(&["Profits", "384", "276.9"]),
            );
            let dom = Document::parse(&xml).unwrap();
            let table = dom.descendants().find(|n| n.has_tag_name("table")).unwrap();
            let mut doc = DoclingDocument::new("t");
            add_ods_sheet(table, &parse_styles(&dom, None), &mut doc);
            doc.nodes
                .iter()
                .filter_map(|n| match n {
                    Node::Table(t) => Some((t.rows.len(), t.rows[0].len())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shapes(""), [(3, 3), (2, 3)]);
        assert_eq!(shapes("note"), [(6, 4)]);
    }

    #[test]
    fn list_continues_across_empty_nested_item() {
        // A numbered `<text:list>` followed by a second list that opens with an
        // empty item wrapping a nested list continues the numbering (3.) while the
        // nested bullets collapse under the previous item (level 1).
        let xml = r#"<root xmlns:text="x" xmlns:style="s">
          <style:list-style style:name="L1">
            <text:list-level-style-number text:level="1"/></style:list-style>
          <style:list-style style:name="L2">
            <text:list-level-style-bullet text:level="1"/>
            <text:list-level-style-bullet text:level="2"/></style:list-style>
          <office:body xmlns:office="o"><office:text>
            <text:list text:style-name="L1">
              <text:list-item><text:p>one</text:p></text:list-item>
              <text:list-item><text:p>two</text:p></text:list-item>
            </text:list>
            <text:list text:style-name="L2">
              <text:list-item><text:list>
                <text:list-item><text:p>bullet</text:p></text:list-item>
              </text:list></text:list-item>
              <text:list-item><text:p>three</text:p></text:list-item>
            </text:list>
          </office:text></office:body></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let body = dom.descendants().find(|n| n.has_tag_name("text")).unwrap();
        let mut doc = DoclingDocument::new("t");
        walk_text(body, &styles, &mut doc);
        let items: Vec<(u64, u8, &str)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::ListItem {
                    number,
                    level,
                    text,
                    ..
                } => Some((*number, *level, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec![
                (1, 0, "one"),
                (2, 0, "two"),
                (0, 1, "bullet"),
                (3, 0, "three"),
            ]
        );
    }

    fn table_of(xml: &str) -> Table {
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let table = dom.descendants().find(|n| n.has_tag_name("table")).unwrap();
        parse_table(table, &styles).unwrap()
    }

    #[test]
    fn rich_cell_renders_list_plain_cell_drops_formatting() {
        let xml = r#"<root xmlns:table="t" xmlns:text="x">
          <table:table><table:table-row>
            <table:table-cell><text:p>List:</text:p><text:list>
              <text:list-item><text:p>a</text:p></text:list-item>
              <text:list-item><text:p>b</text:p></text:list-item></text:list></table:table-cell>
            <table:table-cell><text:p>plain <text:span>bold</text:span></text:p></table:table-cell>
          </table:table-row></table:table></root>"#;
        let t = table_of(xml);
        // Rich cell: the list is rendered with markers; the block newlines stay in
        // the cell text (the Markdown table serializer flattens them to spaces).
        assert_eq!(t.rows[0][0], "List:\n\n- a\n- b");
        // Plain cell: single paragraph, no Markdown markers.
        assert_eq!(t.rows[0][1], "plain bold");
    }

    /// OO1.x (#215): `<text:ordered-list>`/`<text:unordered-list>` map to list
    /// groups (the ordered tag numbering by itself, without a list style),
    /// `<text:tab-stop>` expands like `<text:tab>`, and `<text:h text:level>`
    /// maps like `text:outline-level`.
    #[test]
    fn oo1x_lists_tabs_and_heading_levels_map() {
        let xml = r#"<root xmlns:text="x">
            <text:h text:level="2">Section</text:h>
            <text:p>a<text:tab-stop/><text:span>b</text:span></text:p>
            <text:ordered-list>
              <text:list-item><text:p>one</text:p>
                <text:unordered-list>
                  <text:list-item><text:p>sub</text:p></text:list-item>
                </text:unordered-list></text:list-item>
              <text:list-item><text:p>two</text:p></text:list-item>
            </text:ordered-list></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("s");
        walk_text(dom.root_element(), &styles, &mut doc);
        let md = doc.export_to_markdown();
        assert!(md.contains("### Section"), "text:level heading:\n{md}");
        assert!(md.contains("a\tb"), "tab-stop expands:\n{md}");
        assert!(md.contains("1. one"), "ordered-list numbers:\n{md}");
        assert!(md.contains("- sub"), "nested unordered-list bullets:\n{md}");
        assert!(md.contains("2. two"), "numbering continues:\n{md}");
    }

    /// OO1.x styles fold formatting into `<style:properties>` and name strike /
    /// underline `text-crossing-out` / `text-underline`.
    #[test]
    fn oo1x_style_properties_element_and_attribute_names() {
        let xml = r#"<root xmlns:style="s" xmlns:fo="f">
            <style:style style:name="T1" style:family="text">
              <style:properties fo:font-weight="bold"
                 style:text-crossing-out="single-line"
                 style:text-underline="single"/></style:style></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let f = resolve_fmt(&styles, Some("T1"), Fmt::default());
        assert!(f.bold && f.strike && f.underline);
    }

    /// An OO1.x document has no `office:text` wrapper — `office:body` is the
    /// content container, dispatched by the root's `office:class` (#215). The
    /// flat (non-zip) path parses the same bytes directly.
    #[test]
    fn oo1x_body_without_wrapper_converts() {
        let xml = r#"<office:document-content xmlns:office="o" xmlns:text="x"
              office:class="text" office:version="1.0">
            <office:body><text:p>Aus der Registratur.</text:p></office:body>
            </office:document-content>"#;
        let source =
            SourceDocument::from_bytes("reg.sxw", crate::InputFormat::Odt, xml.as_bytes().to_vec());
        let doc = OdfBackend.convert(&source).unwrap();
        assert!(doc.export_to_markdown().contains("Aus der Registratur."));
    }

    /// Flat ODF (#215): a single-file `<office:document>` converts without a
    /// zip envelope, and the in-DOM `<table:body>` style template earlier in
    /// document order does not shadow `<office:body>`.
    #[test]
    fn flat_odf_single_file_converts() {
        let xml = r#"<office:document xmlns:office="o" xmlns:text="x" xmlns:table="t">
            <office:automatic-styles><table:body/></office:automatic-styles>
            <office:body><office:text>
              <text:h text:outline-level="1">Flat</text:h>
              <text:p>No zip here.</text:p>
            </office:text></office:body></office:document>"#;
        let source = SourceDocument::from_bytes(
            "flat.fodt",
            crate::InputFormat::Odt,
            xml.as_bytes().to_vec(),
        );
        let doc = OdfBackend.convert(&source).unwrap();
        let md = doc.export_to_markdown();
        assert!(md.contains("## Flat"), "{md}");
        assert!(md.contains("No zip here."), "{md}");
    }

    /// Flat ODF embeds chart objects inline: the `<draw:object>` wraps a whole
    /// chart document whose data table renders as the chart's grid — and the
    /// hrefless SVM preview image next to it stays out.
    #[test]
    fn flat_inline_chart_object_renders_as_chart() {
        let xml = r#"<root xmlns:draw="d" xmlns:chart="c" xmlns:table="t" xmlns:text="x" xmlns:office="o">
          <draw:frame><draw:object>
            <office:document>
              <chart:chart chart:class="chart:bar">
                <table:table>
                  <table:table-row>
                    <table:table-cell office:value-type="string"><text:p>Row 1</text:p></table:table-cell>
                    <table:table-cell office:value-type="float"><text:p>9.1</text:p></table:table-cell>
                  </table:table-row>
                </table:table>
              </chart:chart>
            </office:document>
          </draw:object>
          <draw:image><office:binary-data>VkNMTVRGAQAx</office:binary-data></draw:image>
          </draw:frame></root>"#;
        let dom = Document::parse(xml).unwrap();
        let styles = parse_styles(&dom, None);
        let mut doc = DoclingDocument::new("c");
        let frame = dom.descendants().find(|n| n.has_tag_name("frame")).unwrap();
        walk_slide_frame(frame, &styles, &mut doc, false);
        let charts: Vec<_> = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Chart { .. }))
            .collect();
        assert_eq!(charts.len(), 1, "inline object renders as one chart");
        assert!(
            matches!(charts[0], Node::Chart { kind, .. } if kind == "bar_chart"),
            "chart:class maps"
        );
        assert!(
            !doc.nodes.iter().any(|n| matches!(n, Node::Picture { .. })),
            "SVM preview is not a picture"
        );
        assert!(
            !doc.nodes.iter().any(|n| matches!(n, Node::Table(_))),
            "the chart's data table is not doubled as a standalone table"
        );
    }

    /// An hrefless inline image is a picture only when its base64 payload
    /// decodes to a raster magic (PNG here); SVM/unknown payloads are not.
    #[test]
    fn inline_binary_image_magic_gates_pictures() {
        let xml = r#"<root xmlns:draw="d" xmlns:office="o">
            <draw:image draw:name="png"><office:binary-data>iVBORw0KGgoA</office:binary-data></draw:image>
            <draw:image draw:name="svm"><office:binary-data>VkNMTVRGAQAx</office:binary-data></draw:image>
          </root>"#;
        let dom = Document::parse(xml).unwrap();
        let imgs: Vec<XmlNode> = dom
            .descendants()
            .filter(|n| n.has_tag_name("image"))
            .collect();
        assert!(image_can_be_bitmap(imgs[0], ""), "PNG magic");
        assert!(!image_can_be_bitmap(imgs[1], ""), "SVM magic");
    }

    /// A merged cell's text repeats into the positions it covers, a rich
    /// cell's too (docling-core#857, #777).
    #[test]
    fn merged_cells_repeat_into_covered_columns() {
        let xml = r#"<root xmlns:table="t" xmlns:text="x">
          <table:table>
            <table:table-row>
              <table:table-cell table:number-columns-spanned="2"><text:p>Wide</text:p></table:table-cell>
              <table:covered-table-cell/>
              <table:table-cell><text:p>C</text:p></table:table-cell>
            </table:table-row>
            <table:table-row>
              <table:table-cell><text:p>x</text:p></table:table-cell>
              <table:table-cell><text:p>y</text:p></table:table-cell>
              <table:table-cell><text:p>z</text:p></table:table-cell>
            </table:table-row>
          </table:table></root>"#;
        let t = table_of(xml);
        assert_eq!(t.rows[0], vec!["Wide", "Wide", "C"]);
        assert_eq!(t.rows[1], vec!["x", "y", "z"]);
    }
}
