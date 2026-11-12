//! DOCX (Word) backend.
//!
//! A core port of docling's `MsWordDocumentBackend`: it walks `word/document.xml`
//! in order, mapping paragraphs to headings (by style), list items (by
//! numbering), or body paragraphs with inline formatting (bold/italic/strike →
//! Markdown markers, hyperlinks → links), and tables (with `gridSpan`/`vMerge`
//! merges duplicated). Images become `<!-- image -->`.
//!
//! Rich table cells (multiple paragraphs, nested tables, formatting) render
//! their full block content, flattened into the cell.
//!
//! Inline equations reproduce docling's inline-group spacing and stay attached to
//! their list item (`_handle_equations_in_text`); the OMML → LaTeX port is in
//! `omml.rs`. Blip-less DrawingML shapes (grouped drawings, charts, floating
//! text frames) yield one placeholder picture per paragraph — docling renders
//! them through LibreOffice into a single image, which serializes as the same
//! `<!-- image -->` placeholder.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use docling_core::{
    DoclingDocument, InlineRun, ListItemDclx, Node, PictureDescription, PictureImage, Revision,
    RevisionKind, Script, Table, TrackChanges,
};
use roxmltree::{Document, Node as XmlNode};

use crate::backend::markdown::escape_text;
use crate::backend::ooxml::{resolve, Package};
use crate::backend::symbol_fonts::{self, SymbolFont};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

#[derive(Debug, Clone, Copy)]
pub struct DocxBackend {
    /// How tracked changes are read (#851); `None` = docling's reading
    /// (insertions shown, deletions left out, every table row kept).
    pub track_changes: Option<TrackChanges>,
    /// Structured diagrams from the DrawingML drawings (#858): a code item
    /// in that format after a drawing's picture when its connectors name
    /// their end shapes; Mermaid by default.
    pub extract_diagrams: crate::DiagramFormat,
    /// A style's `w:outlineLvl` alone makes it a heading (docling#4480's
    /// `use_outline_level_for_headings`; on by default).
    pub use_outline_level_for_headings: bool,
}

impl Default for DocxBackend {
    fn default() -> Self {
        DocxBackend {
            track_changes: None,
            extract_diagrams: crate::DiagramFormat::default(),
            use_outline_level_for_headings: true,
        }
    }
}

impl DeclarativeBackend for DocxBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("docx: bad zip".into()))?;
        let document = pkg
            .read("word/document.xml")
            .ok_or_else(|| ConversionError::Parse("docx: no document.xml".into()))?;
        let styles = pkg.read("word/styles.xml").unwrap_or_default();
        let numbering = pkg.read("word/numbering.xml").unwrap_or_default();
        let even_and_odd_headers = even_and_odd_headers(&mut pkg);
        // Hyperlink relationship ids → target URLs.
        let rels = part_rels(&mut pkg, "word/document.xml");

        // Embedded images, by relationship id (for image export).
        let images = pkg.image_rels("word/document.xml", "word");
        let charts = chart_rels(&mut pkg, "word/document.xml");
        // The theme's colours and line widths, for the DrawingML shapes
        // (#858) whose fills and outlines reference them.
        let theme = pkg
            .read("word/theme/theme1.xml")
            .map(|xml| super::docx_drawingml::Theme::parse(&xml))
            .unwrap_or_default()
            .with_styles(&styles);

        let sm = parse_styles(&styles);
        let num_levels = parse_numbering(&numbering, &sm.nums);

        // Settle every `mc:AlternateContent` on the chosen branch first
        // (#572), so the walks below see one document without alternatives.
        let document = super::mc::resolve_alternate_content(&document);
        // Hidden text (`w:vanish`, docling#4958) settled the same way: a
        // fully hidden paragraph marked for the `invisible` layer, a hidden
        // run of a shown one cut out.
        let hidden = super::docx_hidden::HiddenStyles::parse(&styles, MAX_STYLE_INHERITANCE_DEPTH);
        let document = super::docx_hidden::resolve_hidden(&document, &hidden, false);
        let dom =
            Document::parse(&document).map_err(|e| ConversionError::with_source("docx", e))?;
        let ctx = Ctx {
            style_names: &sm.names,
            style_nums: &sm.nums,
            style_based: &sm.based,
            style_fonts: &sm.fonts,
            style_outline: &sm.outlines,
            style_bold: &sm.bolds,
            not_paragraph_styles: &sm.not_paragraph,
            num_levels: &num_levels,
            style_left_ind: &sm.left_inds,
            hidden: &hidden,
            rels: &rels,
            images: &images,
            charts: &charts,
            theme: &theme,
            even_and_odd_headers,
            table_depth: std::cell::Cell::new(0),
            orphan_inline_groups: std::cell::Cell::new(0),
            bookmarks: &HashMap::new(),
            track_changes: self.track_changes,
            extract_diagrams: self.extract_diagrams,
            outline_headings: self.use_outline_level_for_headings,
        };

        let mut doc = DoclingDocument::new(&source.name);
        let Some(body) = dom.descendants().find(|n| n.has_tag_name("body")) else {
            return Ok(doc);
        };
        // The bookmarks' destinations (#849), for both walks' internal links.
        let bookmarks = bookmark_targets(body, &ctx);
        let ctx = ctx.with_bookmarks(&bookmarks);
        let mut state = ListState::default();
        // Reviewer comments, by docx `w:id` in `word/comments.xml` order — the
        // order they become `comment_section` groups below, which is the order
        // `Node::Commented` indices refer to.
        let comments = parse_comments(&mut pkg);
        let comment_slot: HashMap<&str, usize> = comments
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();
        // Comment ranges opened by an earlier block and not yet closed: a
        // `w:commentRangeStart`/`End` pair may span paragraphs, and every
        // paragraph inside the range is annotated.
        let mut open: Vec<&str> = Vec::new();
        for node in body.children().filter(XmlNode::is_element) {
            let slots = block_comment_slots(node, &comment_slot, &mut open);
            let start = doc.nodes.len();
            process_block(node, &ctx, &mut state, &mut doc);
            if !slots.is_empty() {
                for item in doc.nodes[start..].iter_mut() {
                    let inner = std::mem::replace(
                        item,
                        Node::Paragraph {
                            text: String::new(),
                        },
                    );
                    *item = Node::Commented {
                        comments: slots.clone(),
                        inner: Box::new(inner),
                    };
                }
            }
        }
        drop_resumed_list_spacers(&mut doc.nodes);
        // Section headers/footers follow the body as furniture-layer content
        // (docling's `_add_header_footer`): the first section always
        // contributes, later sections only when they define a distinct first
        // page (`<w:titlePg/>`), which also switches both to the first-page
        // parts; the even-page parts come too when the document uses them
        // (`<w:evenAndOddHeaders/>`, #590 — docling never reads those).
        add_header_footer(&mut pkg, body, &ctx, &mut doc);
        // Footnote / endnote bodies follow as furniture too (docling#4374):
        // a `w:footnoteReference` in the body carries no text of its own, so
        // without reading `word/footnotes.xml` / `word/endnotes.xml` the
        // notes were silently dropped.
        for note in footnote_texts(&mut pkg, ctx.hidden) {
            doc.nodes.push(Node::Furniture {
                layer: docling_core::ContentLayer::Furniture,
                inner: Box::new(Node::Paragraph {
                    text: super::markdown::escape_html(&super::markdown::escape_underscores(
                        &note.text,
                    )),
                }),
            });
        }
        // Reviewer comments (docling's `notes` layer) are appended after the
        // body as `comment_section` groups: Markdown/LaTeX drop them, JSON
        // emits the group plus its notes text (and the `comments` back-refs on
        // the annotated items), DocLang the flat `<layer value="notes"/>` item.
        // The JSON takes docling's item tree, built by a call-for-call port of
        // upstream's walk ([`super::docx_tree`]) so the JSON structure —
        // heading nesting, inline groups of formatting runs, list groups,
        // rich-cell groups, textbox/header/footer sections, comment
        // back-refs — is upstream's; the flat nodes above stay the source for
        // Markdown / DocLang / LaTeX.
        doc.tree = Some(super::docx_tree::build_tree(
            &mut pkg, body, &ctx, &comments,
        ));
        for (id, text) in comments {
            doc.nodes.push(Node::CommentSection {
                name: format!("comment-{id}"),
                text,
                // The docx backend replaces `add_comment`'s text ref with the
                // group's, so replies to one comment group together.
                refs_note_text: false,
                grouped: true,
            });
        }
        Ok(doc)
    }
}

/// Append section headers/footers as furniture (docling's `_add_header_footer`,
/// docling#3843 shape). Sections are the `<w:sectPr>` elements in document
/// order; header/footer references inherit from earlier sections per type
/// (python-docx's linked-to-previous — the carry-forward map below). **Every**
/// section is visited; a part already emitted for an earlier section (the
/// inheritance case) is skipped by part name so it isn't duplicated. A section
/// with `<w:titlePg/>` contributes both its first-page **and** its regular
/// header/footer, since both are actually used — headers first, then footers,
/// first-page before regular within each; with `<w:evenAndOddHeaders/>` in
/// the settings, the even-page part follows the regular one (#590: Word
/// shows it on every even page, docling leaves it out — python-docx's
/// `Section` exposes `even_page_header`, `_add_header_footer` never asks).
fn add_header_footer(pkg: &mut Package, body: XmlNode, ctx: &Ctx, doc: &mut DoclingDocument) {
    for (_, part) in header_footer_parts(body, ctx) {
        emit_header_footer_part(pkg, &part, ctx, doc);
    }
}

/// The header/footer parts a document uses, in docling's emission order —
/// `(kind, part name)` with `kind` = `"page header"` / `"page footer"`, each
/// part once (see [`add_header_footer`]).
pub(super) fn header_footer_parts(body: XmlNode, ctx: &Ctx) -> Vec<(&'static str, String)> {
    let doc_rels = ctx.rels;
    let sect_prs: Vec<XmlNode> = body
        .descendants()
        .filter(|n| n.has_tag_name("sectPr"))
        .collect();
    let mut effective: HashMap<(&str, String), String> = HashMap::new();
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for sect in sect_prs.iter() {
        for r in sect.children().filter(XmlNode::is_element) {
            let kind = match r.tag_name().name() {
                "headerReference" => "hdr",
                "footerReference" => "ftr",
                _ => continue,
            };
            let ty = attr(r, "type").unwrap_or("default").to_string();
            if let Some(id) = attr(r, "id") {
                effective.insert((kind, ty), id.to_string());
            }
        }
        let title_pg = sect.children().any(|n| {
            n.has_tag_name("titlePg")
                && attr(n, "val") != Some("false")
                && attr(n, "val") != Some("0")
        });
        for kind in ["hdr", "ftr"] {
            // A part Word actually shows: the first page's with `titlePg`,
            // the regular (odd) one, and the even pages' when the document
            // distinguishes them — without `evenAndOddHeaders` an `even`
            // reference is a leftover Word ignores, so it is skipped here too.
            let types: &[&str] = match (title_pg, ctx.even_and_odd_headers) {
                (true, true) => &["first", "default", "even"],
                (true, false) => &["first", "default"],
                (false, true) => &["default", "even"],
                (false, false) => &["default"],
            };
            for ty in types {
                let Some(rid) = effective.get(&(kind, ty.to_string())) else {
                    continue;
                };
                let Some(part) = doc_rels.get(rid) else {
                    continue;
                };
                if !emitted.insert(part.clone()) {
                    continue;
                }
                let name = if kind == "hdr" {
                    "page header"
                } else {
                    "page footer"
                };
                out.push((name, part.clone()));
            }
        }
    }
    out
}

/// `word/settings.xml`'s `<w:evenAndOddHeaders/>` (#590): whether the
/// document shows different headers/footers on even pages — only then do the
/// sections' `w:type="even"` references mean anything to Word.
fn even_and_odd_headers(pkg: &mut Package) -> bool {
    let Some(xml) = pkg.read("word/settings.xml") else {
        return false;
    };
    let Ok(dom) = Document::parse(&xml) else {
        return false;
    };
    dom.descendants()
        .find(|n| n.has_tag_name("evenAndOddHeaders"))
        .is_some_and(|n| on_off(attr(n, "val")))
}

/// One footnote / endnote body ([`footnote_texts`]).
pub(super) struct DocxNote {
    /// `footnote` or `endnote` — which reference element calls it.
    pub(super) kind: &'static str,
    /// The note's `w:id`.
    pub(super) id: String,
    pub(super) text: String,
}

/// The body text of every footnote and endnote, in part order (footnotes,
/// then endnotes) and document order — docling's `_add_footnotes_and_endnotes`
/// (docling#4374): the parts are reached through the document's FOOTNOTES /
/// ENDNOTES relationships, the `separator` / `continuationSeparator` /
/// `continuationNotice` placeholders Word writes into every document are
/// skipped, and a note's non-blank paragraphs — at any depth, so a table in
/// the note counts, read through the inline wrappers (a tracked insertion,
/// a content control) like a body paragraph, docling#4735 — are joined with
/// one space. Each becomes a furniture-layer `footnote` item.
/// The note's kind (`footnote` / `endnote`) and `w:id` come along: a body
/// `w:footnoteReference` / `w:endnoteReference` names it by those (#538).
pub(super) fn footnote_texts(
    pkg: &mut Package,
    hidden: &super::docx_hidden::HiddenStyles,
) -> Vec<DocxNote> {
    const SKIP_TYPES: [&str; 3] = ["separator", "continuationSeparator", "continuationNotice"];
    let mut out = Vec::new();
    for (rel_suffix, tag) in [("/footnotes", "footnote"), ("/endnotes", "endnote")] {
        let Some(part) = pkg
            .rels_for("word/document.xml")
            .into_iter()
            .find(|r| r.rel_type.ends_with(rel_suffix))
            .map(|r| super::ooxml::resolve("word", &r.target))
        else {
            continue;
        };
        let Some(xml) = pkg.read(&part) else {
            continue;
        };
        let xml = super::mc::resolve_alternate_content(&xml);
        let xml = super::docx_hidden::resolve_hidden(&xml, hidden, true);
        let Ok(dom) = Document::parse(&xml) else {
            eprintln!("docling: failed to parse {tag}s part");
            continue;
        };
        for note in dom
            .root_element()
            .children()
            .filter(|n| n.has_tag_name(tag))
        {
            if attr(note, "type").is_some_and(|t| SKIP_TYPES.contains(&t)) {
                continue;
            }
            let texts: Vec<String> = note
                .descendants()
                .filter(|n| n.has_tag_name("p"))
                .map(|p| plain_paragraph_text(p).trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if !texts.is_empty() {
                out.push(DocxNote {
                    kind: tag,
                    id: attr(note, "id").unwrap_or_default().to_string(),
                    text: texts.join(" "),
                });
            }
        }
    }
    out
}

/// Walk one header/footer part and append its blocks wrapped in the furniture
/// layer. docling skips a part with no visible content (no non-blank paragraph
/// text, tables, images, or textboxes).
fn emit_header_footer_part(pkg: &mut Package, part: &str, ctx: &Ctx, doc: &mut DoclingDocument) {
    let Some(xml) = pkg.read(part) else {
        return;
    };
    let xml = super::mc::resolve_alternate_content(&xml);
    let xml = super::docx_hidden::resolve_hidden(&xml, ctx.hidden, false);
    let Ok(dom) = Document::parse(&xml) else {
        return;
    };
    let root = dom.root_element();
    let has_content = root
        .descendants()
        .any(|n| n.has_tag_name("t") && n.text().is_some_and(|t| !t.trim().is_empty()))
        || root.descendants().any(|n| {
            matches!(
                n.tag_name().name(),
                "tbl" | "blip" | "imagedata" | "txbxContent"
            )
        });
    if !has_content {
        return;
    }
    let rels = part_rels(pkg, part);
    let images = pkg.image_rels(part, "word");
    let charts = chart_rels(pkg, part);
    let part_ctx = ctx.for_part(&rels, &images, &charts);
    let mut sub = DoclingDocument::new("");
    let mut state = ListState::default();
    for node in root.children().filter(XmlNode::is_element) {
        process_block(node, &part_ctx, &mut state, &mut sub);
    }
    for n in sub.nodes {
        doc.nodes.push(Node::Furniture {
            layer: docling_core::ContentLayer::Furniture,
            inner: Box::new(n),
        });
    }
}

/// A part's relationship id → target map: hyperlink targets verbatim, every
/// other target resolved against `word/`. A fragment-only target
/// (`Target="#_Procédures_spéciales"`, an internal bookmark reference some
/// generators write as a relationship) is dropped, as docling 2.128 strips
/// those `Relationship` elements before python-docx opens the package
/// (docling#4243 — resolved as a part name they made it fail to load): the
/// hyperlink keeps its text and carries no target.
pub(super) fn part_rels(pkg: &mut Package, part: &str) -> HashMap<String, String> {
    pkg.rels_for(part)
        .iter()
        .filter(|r| !r.target.starts_with('#'))
        .map(|r| {
            let t = if r.rel_type.ends_with("/hyperlink") {
                r.target.clone()
            } else {
                resolve("word", &r.target)
            };
            (r.id.clone(), t)
        })
        .collect()
}

/// Parse `word/comments.xml` into `(w:id, note)` pairs, the note being
/// docling's `[author: {author} ({initials}), time: {iso}]: {text}` (the
/// author/initials parts drop out when absent). Empty when the part is missing.
pub(super) fn parse_comments(pkg: &mut Package) -> Vec<(String, String)> {
    let Some(xml) = pkg.read("word/comments.xml") else {
        return Vec::new();
    };
    let xml = super::mc::resolve_alternate_content(&xml);
    let Ok(dom) = Document::parse(&xml) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for c in dom.descendants().filter(|n| n.has_tag_name("comment")) {
        let author = attr(c, "author").unwrap_or("").trim();
        let initials = attr(c, "initials").unwrap_or("").trim();
        let date = attr(c, "date").map(format_comment_date).unwrap_or_default();
        let text: String = c.descendants().filter_map(flat_text).collect();
        let head = if author.is_empty() {
            format!("[time: {date}]")
        } else if initials.is_empty() {
            format!("[author: {author}, time: {date}]")
        } else {
            format!("[author: {author} ({initials}), time: {date}]")
        };
        let id = attr(c, "id").unwrap_or("").to_string();
        out.push((id, format!("{head}: {text}")));
    }
    out
}

/// The comment slots a body block is annotated by, in `comments.xml` order.
/// A block carries a comment when it opens one (`w:commentRangeStart`),
/// anchors one (`w:commentReference` — Word's single-run case, which needs no
/// range), or sits inside a range opened by an earlier block. `open` carries
/// that cross-block state and is updated for the ranges this block ends.
fn block_comment_slots<'a>(
    node: XmlNode<'a, 'a>,
    slot_of: &HashMap<&str, usize>,
    open: &mut Vec<&'a str>,
) -> Vec<usize> {
    let ids_with = |tag: &str| -> Vec<&'a str> {
        node.descendants()
            .filter(|n| n.has_tag_name(tag))
            .filter_map(|n| attr(n, "id"))
            .collect()
    };
    let started = ids_with("commentRangeStart");
    let ended = ids_with("commentRangeEnd");
    let referenced = ids_with("commentReference");

    let mut ids: Vec<&str> = open.clone();
    for id in started.iter().chain(referenced.iter()) {
        if !ids.contains(id) {
            ids.push(id);
        }
    }
    for id in started {
        if !ended.contains(&id) && !open.contains(&id) {
            open.push(id);
        }
    }
    open.retain(|id| !ended.contains(id));

    let mut slots: Vec<usize> = ids
        .iter()
        .filter_map(|id| slot_of.get(id).copied())
        .collect();
    slots.sort_unstable();
    slots.dedup();
    slots
}

/// OOXML comment dates use e.g. `2026-01-04T05:48:07Z`; docling normalizes them
/// to `2026-01-04T05:48:07.000+00:00` (millisecond precision, explicit offset).
fn format_comment_date(raw: &str) -> String {
    let base = raw.strip_suffix('Z').unwrap_or(raw);
    let with_ms = if base.contains('.') {
        base.to_string()
    } else {
        format!("{base}.000")
    };
    if raw.ends_with('Z') {
        format!("{with_ms}+00:00")
    } else {
        with_ms
    }
}

pub(super) struct Ctx<'a> {
    pub(super) style_names: &'a HashMap<String, String>,
    /// styleId -> the style's *own* `numPr` parts (`numId`, `ilvl`), each
    /// optional — resolved through `basedOn` by [`style_numbering`].
    pub(super) style_nums: &'a HashMap<String, (Option<String>, Option<i64>)>,
    pub(super) style_based: &'a HashMap<String, String>, // styleId -> basedOn styleId
    pub(super) style_fonts: &'a HashMap<String, String>, // styleId -> lowercased ascii font
    pub(super) style_outline: &'a HashMap<String, u8>,   // styleId -> 1-indexed outlineLvl
    /// styleId → the style's *own* `w:rPr/w:b` as python-docx's `font.bold`
    /// reads it (`true`/`false`; absent = not set) — the item tree's
    /// `_get_format_from_run` climbs the paragraph style chain for bold.
    pub(super) style_bold: &'a HashMap<String, bool>,
    /// The style ids a `w:pStyle` cannot name (#865): see
    /// [`paragraph_style_id`].
    pub(super) not_paragraph_styles: &'a HashSet<String>,
    pub(super) num_levels: &'a HashMap<(String, i64), NumLevel>, // (numId, ilvl) -> level props
    /// styleId → the first `w:pPr/w:ind` left indentation on the style's
    /// `basedOn` chain (twips; `None` = none sets one), and `""` → the
    /// default paragraph style's — see [`list_left_indent`].
    pub(super) style_left_ind: &'a HashMap<String, Option<i64>>,
    /// The styles' hidden-text settings (docling#4958) a header, footer or
    /// notes part is resolved with — see `docx_hidden`.
    pub(super) hidden: &'a super::docx_hidden::HiddenStyles,
    pub(super) rels: &'a HashMap<String, String>,
    pub(super) images: &'a HashMap<String, PictureImage>, // image relationship id -> extracted image
    /// Native charts by relationship id: `(classified kind, title, data grid)`
    /// parsed from the `word/charts/*.xml` parts (docling PR #3809).
    pub(super) charts: &'a HashMap<String, (String, Option<String>, docling_core::Table)>,
    /// `word/theme/theme1.xml` (#858): the scheme colours and line widths
    /// a DrawingML shape's style references resolve through.
    pub(super) theme: &'a super::docx_drawingml::Theme,
    /// `word/settings.xml` has `<w:evenAndOddHeaders/>`: the sections' even-page
    /// headers/footers are in use (#590).
    pub(super) even_and_odd_headers: bool,
    /// Nesting depth of the table being parsed (a table inside a cell inside a
    /// table …), bounded by [`MAX_TABLE_DEPTH`]: `parse_table_with` recurses
    /// per level, and a 35 KB file with 2 000 tables nested one inside the next
    /// overflowed the stack — an abort, not an error.
    pub(super) table_depth: std::cell::Cell<u32>,
    /// Empty inline groups the last top-level table's rich cells left on
    /// the body (see [`super::docx_tree::rich_cell_orphan_groups`]) — taken
    /// by the caller that pushes the table.
    pub(super) orphan_inline_groups: std::cell::Cell<usize>,
    /// Bookmark name → the link destination's identifier (#849), from
    /// [`bookmark_targets`]: what a `w:hyperlink w:anchor` resolves to.
    pub(super) bookmarks: &'a HashMap<String, String>,
    /// The tracked-changes policy (#851); `None` = docling's reading.
    pub(super) track_changes: Option<TrackChanges>,
    /// The diagram extraction (#858); `DiagramFormat::None` = pictures alone.
    pub(super) extract_diagrams: crate::DiagramFormat,
    /// `use_outline_level_for_headings` (docling#4480): an outline level
    /// alone promotes a style that is not named a heading.
    pub(super) outline_headings: bool,
}

impl<'a> Ctx<'a> {
    /// The context for a header/footer part: the document's style and
    /// numbering maps with the part's own relationships, images and charts.
    pub(super) fn for_part(
        &self,
        rels: &'a HashMap<String, String>,
        images: &'a HashMap<String, PictureImage>,
        charts: &'a HashMap<String, (String, Option<String>, docling_core::Table)>,
    ) -> Ctx<'a> {
        Ctx {
            style_names: self.style_names,
            style_nums: self.style_nums,
            style_based: self.style_based,
            style_fonts: self.style_fonts,
            style_outline: self.style_outline,
            style_bold: self.style_bold,
            not_paragraph_styles: self.not_paragraph_styles,
            num_levels: self.num_levels,
            style_left_ind: self.style_left_ind,
            hidden: self.hidden,
            rels,
            images,
            charts,
            theme: self.theme,
            even_and_odd_headers: self.even_and_odd_headers,
            table_depth: std::cell::Cell::new(0),
            orphan_inline_groups: std::cell::Cell::new(0),
            bookmarks: self.bookmarks,
            track_changes: self.track_changes,
            extract_diagrams: self.extract_diagrams,
            outline_headings: self.outline_headings,
        }
    }

    /// The view the runs are read in (#851): `accept` unless asked
    /// otherwise — docling's reading.
    pub(super) fn policy(&self) -> TrackChanges {
        self.track_changes.unwrap_or_default()
    }

    /// This context with the document's bookmarks resolved (#849) — they
    /// need the style maps to tell a heading's bookmark from a body one, so
    /// the map is built from a context without them first.
    pub(super) fn with_bookmarks(&self, bookmarks: &'a HashMap<String, String>) -> Ctx<'a> {
        Ctx {
            bookmarks,
            ..self.for_part(self.rels, self.images, self.charts)
        }
    }
}

/// Whether a table row stays under the tracked-changes policy (#851): a
/// row Word marks inserted (`w:trPr/w:ins`) goes under `reject`, one marked
/// deleted (`w:trPr/w:del`) under an explicit `accept`; without a policy
/// every row stays, as docling reads it.
pub(super) fn row_kept(tr: XmlNode, ctx: &Ctx) -> bool {
    let Some(policy) = ctx.track_changes else {
        return true;
    };
    let marked = |tag: &str| {
        tr.children()
            .find(|n| n.has_tag_name("trPr"))
            .is_some_and(|pr| pr.children().any(|n| n.has_tag_name(tag)))
    };
    match policy {
        TrackChanges::Accept => !marked("del"),
        TrackChanges::Reject => !marked("ins"),
        TrackChanges::All => true,
    }
}

/// The revision a `w:ins` / `w:del` / `w:moveTo` / `w:moveFrom` wrapper
/// marks its runs with (#851): the kind, the reviewer and the date.
pub(super) fn revision_of(wrapper: XmlNode, kind: RevisionKind) -> Revision {
    Revision {
        kind,
        author: attr(wrapper, "author").map(str::to_string),
        date: attr(wrapper, "date").map(str::to_string),
    }
}

/// Word's bookmark at the cursor (`_GoBack`): never a link target.
const DUMMY_BOOKMARKS: [&str; 1] = ["_GoBack"];

/// Is `p` a heading or title paragraph — the style checks both walks make,
/// without the numbering / text they add.
pub(super) fn is_heading_paragraph(p: XmlNode, ctx: &Ctx) -> bool {
    let style_id = paragraph_style_id(p, ctx);
    let style_name = ctx
        .style_names
        .get(style_id)
        .map(String::as_str)
        .unwrap_or(style_id);
    named_heading_level(style_id, style_name, ctx).is_some()
        || (ctx.outline_headings
            && inherited_outline(style_id, ctx).is_some_and(|l| (1..=9).contains(&l)))
        || is_title_style(style_id, style_name, ctx)
}

/// The paragraph a bookmark start belongs to (#849): the one it sits in,
/// else — Word writes a bookmark around a whole table or paragraph at the
/// body level — the next paragraph in document order.
fn bookmark_paragraph<'a, 'i>(start: XmlNode<'a, 'i>) -> Option<XmlNode<'a, 'i>> {
    if let Some(p) = start.ancestors().find(|n| n.has_tag_name("p")) {
        return Some(p);
    }
    start
        .next_siblings()
        .skip(1)
        .filter(XmlNode::is_element)
        .find_map(|n| {
            if n.has_tag_name("p") {
                Some(n)
            } else {
                n.descendants().find(|d| d.has_tag_name("p"))
            }
        })
}

/// Every bookmark of the document, name → the identifier its destination
/// is written with (#849). A bookmark in a heading takes the heading's
/// Pandoc auto-identifier (`Target section` → `target-section`), so
/// `pandoc -f json` and `pandoc -f docx` agree on the link; one in body
/// text or a table cell keeps its own name (Pandoc's choice, too), its
/// characters outside letters, digits, `_`, `-`, `.` replaced by `-`. Two
/// bookmarks on one heading share its identifier; an identifier a second
/// destination would repeat gets `-1`, `-2`, … (Pandoc's rule), so every
/// identifier names exactly one destination. `_GoBack` is skipped.
pub(super) fn bookmark_targets(body: XmlNode, ctx: &Ctx) -> HashMap<String, String> {
    let mut by_name = HashMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    let mut heading_ids: HashMap<roxmltree::NodeId, String> = HashMap::new();
    for start in body
        .descendants()
        .filter(|n| n.has_tag_name("bookmarkStart"))
    {
        let Some(name) = attr(start, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        if DUMMY_BOOKMARKS.contains(&name) || by_name.contains_key(name) {
            continue;
        }
        let Some(p) = bookmark_paragraph(start) else {
            continue;
        };
        if is_heading_paragraph(p, ctx) {
            if let Some(id) = heading_ids.get(&p.id()) {
                by_name.insert(name.to_string(), id.clone());
                continue;
            }
        }
        let base = if is_heading_paragraph(p, ctx) {
            docling_core::pandoc::auto_identifier(&plain_paragraph_text(p))
        } else {
            name.chars()
                .map(|c| {
                    if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect()
        };
        let mut id = base.clone();
        let mut n = 0;
        while taken.contains(&id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        taken.insert(id.clone());
        if is_heading_paragraph(p, ctx) {
            heading_ids.insert(p.id(), id.clone());
        }
        by_name.insert(name.to_string(), id);
    }
    by_name
}

/// The identifiers of the bookmarks that start in `p` — inside it, or just
/// before it at the body / cell level (see [`bookmark_paragraph`]) — in
/// document order, each once (#849).
pub(super) fn paragraph_anchors(p: XmlNode, ctx: &Ctx) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |start: XmlNode| {
        if let Some(id) = attr(start, "name").and_then(|n| ctx.bookmarks.get(n)) {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
    };
    // Body-level bookmarks between the previous paragraph and this one.
    let before: Vec<XmlNode> = p
        .prev_siblings()
        .filter(XmlNode::is_element)
        .take_while(|n| {
            matches!(
                n.tag_name().name(),
                "bookmarkStart" | "bookmarkEnd" | "proofErr" | "permStart" | "permEnd"
            )
        })
        .filter(|n| n.has_tag_name("bookmarkStart"))
        .collect();
    for start in before.into_iter().rev() {
        push(start);
    }
    for start in p.descendants().filter(|n| n.has_tag_name("bookmarkStart")) {
        push(start);
    }
    out
}

/// A `w:hyperlink`'s target (#849): the relationship `r:id` names when it
/// resolves; else the bookmark `w:anchor` names, as `#id`. A relationship
/// the part lacks and a bookmark the document lacks are diagnosed under
/// `DOCLING_RS_DEBUG` and leave the text unlinked — never a failed
/// conversion. `r:id` wins when both are present (Word writes both for a
/// link into another file's bookmark; the file is the destination).
pub(super) fn hyperlink_target(h: XmlNode, ctx: &Ctx) -> Option<String> {
    if let Some(id) = attr(h, "id") {
        match ctx.rels.get(id).filter(|t| !t.is_empty()) {
            Some(target) => return Some(target.clone()),
            None => docling_core::debug_log!("docx: hyperlink relationship {id} is missing"),
        }
    }
    let anchor = attr(h, "anchor").filter(|a| !a.is_empty())?;
    match ctx.bookmarks.get(anchor) {
        Some(id) => Some(format!("#{id}")),
        None => {
            docling_core::debug_log!("docx: hyperlink anchor {anchor:?} names no bookmark");
            None
        }
    }
}

/// Deepest table nesting parsed; anything deeper is dropped. Word itself
/// renders a handful of levels — real documents stop at two or three.
pub(super) const MAX_TABLE_DEPTH: u32 = 64;

/// Mutable list/heading numbering state carried across the body walk.
#[derive(Default)]
struct ListState {
    counters: HashMap<(String, i64), i64>, // (numId, ilvl) -> running number
    numbered_headers: HashMap<u8, u64>,    // heading level -> running number
    list_run_base: Option<i64>,            // base ilvl of the current contiguous list run
    /// The `numId` of the last list item emitted, while docling would still
    /// reuse its ListGroup: cleared by any non-empty body block (its
    /// `_end_list_on_body_text` / a parent change), kept across an empty
    /// spacer paragraph. A list item whose `numId` differs — or that follows
    /// such a block — opens a new list (`_manage_list_structure`'s "new list
    /// sequence"), which is what the item's `first_in_list` flags (#385).
    last_list_num_id: Option<String>,
    /// Whether a heading/title was emitted before the current paragraph. In
    /// docling's tree later content is parented under that heading `TextItem`,
    /// and the DocLang serializer leaves an InlineGroup whose parent is a
    /// `TextItem` *unwrapped* (no `<text>` element); under the body/a group it
    /// is wrapped.
    seen_heading: bool,
    /// Lists nested under the open list's item by left indentation,
    /// innermost last — the flat walk's half of docling#4465 (see
    /// `docx_tree`'s `NestedList`).
    nested_lists: Vec<FlatNestedList>,
    /// The last list item's effective left indentation in twips and its
    /// nesting level.
    last_list_left_indent: i64,
    last_list_level: u8,
}

/// A list nested under the item above it by left indentation (docling#4465):
/// its items start at `level`, offset by their `w:ilvl` past `start_ilvl`.
struct FlatNestedList {
    level: u8,
    start_ilvl: i64,
    outer_num_id: String,
    outer_left_indent: i64,
}

impl ListState {
    /// End the current list run (a non-list block). A list nested by left
    /// indentation closes with the outermost list, whose `numId` an item
    /// after an empty spacer paragraph resumes (docling#4465).
    fn end_list_run(&mut self) {
        self.list_run_base = None;
        if let Some(outer) = self.nested_lists.first() {
            if self.last_list_num_id.is_some() {
                self.last_list_num_id = Some(outer.outer_num_id.clone());
            }
        }
        self.nested_lists.clear();
    }

    /// An item's nesting level and whether it opens a list. `left_indent`
    /// nests an item of another `numId` than the open list's under that
    /// list's last item when it is indented deeper, and returns to the
    /// outer list when it is indented no deeper than the item a nested list
    /// hangs from — as `docx_tree`'s `manage_list_structure` places it.
    /// `open`: a list run was open before this item (an empty spacer
    /// paragraph closes it — the outer list then resumes by `numId` alone).
    fn list_item_position(
        &mut self,
        num_id: &str,
        ilvl: i64,
        left_indent: i64,
        open: bool,
    ) -> (u8, bool) {
        let base = *self.list_run_base.get_or_insert(ilvl);
        let mut prev = self.last_list_num_id.clone();
        if let Some(open) = prev.clone().filter(|p| open && p != num_id) {
            let mut closed = None;
            while self
                .nested_lists
                .last()
                .is_some_and(|n| left_indent <= n.outer_left_indent)
            {
                closed = self.nested_lists.pop();
            }
            if let Some(closed) = closed {
                prev = Some(closed.outer_num_id);
            } else if left_indent > self.last_list_left_indent {
                self.nested_lists.push(FlatNestedList {
                    level: self.last_list_level.saturating_add(1),
                    start_ilvl: ilvl,
                    outer_num_id: open,
                    outer_left_indent: self.last_list_left_indent,
                });
            }
        }
        let level = match self.nested_lists.last() {
            Some(n) => n.level as i64 + (ilvl - n.start_ilvl).max(0),
            None => (ilvl - base).max(0),
        }
        .min(u8::MAX as i64) as u8;
        self.last_list_left_indent = left_indent;
        self.last_list_level = level;
        self.last_list_num_id = Some(num_id.to_string());
        (level, prev.as_deref() != Some(num_id))
    }
}

/// Dispatch a body-level block: paragraph, table, or `<w:sdt>` (whose
/// `<w:sdtContent>` children are processed transparently).
fn process_block(node: XmlNode, ctx: &Ctx, state: &mut ListState, doc: &mut DoclingDocument) {
    match node.tag_name().name() {
        "p" => handle_paragraph(node, ctx, state, doc),
        "tbl" => {
            // A 1×1 table is treated as furniture: its single cell's content is
            // processed as document-body blocks (docling unwraps it).
            let rows: Vec<XmlNode> = node
                .children()
                .filter(|n| n.has_tag_name("tr") && row_kept(*n, ctx))
                .collect();
            let num_cols = rows
                .iter()
                .map(|r| row_cells(*r).into_iter().map(grid_span).sum::<usize>())
                .max()
                .unwrap_or(0);
            if rows.len() == 1 && num_cols == 1 {
                if let Some(cell) = row_cells(rows[0]).first() {
                    for child in child_elements(*cell) {
                        process_block(child, ctx, state, doc);
                    }
                }
            } else if let Some(table) = parse_table(node, ctx) {
                doc.push(Node::Table(table));
                // docling leaves an empty inline group at `parents[level-1]`
                // for every multi-run paragraph of a rich cell (its runs move
                // into the cell group). DocLang wraps it as `<text></text>`
                // under the body, but not under a heading `TextItem` — where
                // it sits once a heading was emitted (`seen_heading`); the
                // other serializers write nothing either way.
                let orphans = ctx.orphan_inline_groups.replace(0);
                for _ in 0..if state.seen_heading { 0 } else { orphans } {
                    doc.push(Node::DoclangOnly(Box::new(Node::Paragraph {
                        text: String::new(),
                    })));
                }
                state.end_list_run();
                state.last_list_num_id = None;
            }
        }
        "sdt" => {
            if let Some(content) = node.children().find(|n| n.has_tag_name("sdtContent")) {
                for child in child_elements(content) {
                    process_block(child, ctx, state, doc);
                }
            }
        }
        _ => {}
    }
}

fn handle_paragraph(p: XmlNode, ctx: &Ctx, state: &mut ListState, doc: &mut DoclingDocument) {
    handle_paragraph_inner(p, ctx, state, doc, false, false)
}

/// `rich` = inside a rich table cell, where a plain paragraph's formatted
/// segments each become a separate block (so they flatten to double spaces).
/// `skip_textbox` = this paragraph is itself textbox content, so its (nested)
/// textboxes are already covered by the enclosing paragraph's extraction.
fn handle_paragraph_inner(
    p: XmlNode,
    ctx: &Ctx,
    state: &mut ListState,
    doc: &mut DoclingDocument,
    rich: bool,
    skip_textbox: bool,
) {
    // A fully hidden paragraph (docling#4958, `docx_hidden`) is read as any
    // other, and what it yields goes to the `invisible` layer: kept, out of
    // the default exports.
    if super::docx_hidden::is_hidden_paragraph(p) {
        let start = doc.nodes.len();
        handle_paragraph_body(p, ctx, state, doc, rich, skip_textbox);
        for node in &mut doc.nodes[start..] {
            if !matches!(node, Node::Furniture { .. }) {
                let inner = std::mem::replace(node, Node::PageBreak);
                *node = Node::Furniture {
                    layer: docling_core::ContentLayer::Invisible,
                    inner: Box::new(inner),
                };
            }
        }
        return;
    }
    handle_paragraph_body(p, ctx, state, doc, rich, skip_textbox);
}

fn handle_paragraph_body(
    p: XmlNode,
    ctx: &Ctx,
    state: &mut ListState,
    doc: &mut DoclingDocument,
    rich: bool,
    skip_textbox: bool,
) {
    let style_id = paragraph_style_id(p, ctx);
    let style_name = ctx
        .style_names
        .get(style_id)
        .cloned()
        .unwrap_or_else(|| style_id.to_string());

    // Textbox content is emitted first, before the paragraph's own content — and
    // for *every* paragraph (a textbox can be anchored to a heading or list item,
    // not just a plain one). Each `<w:txbxContent>` yields its paragraphs' text
    // then any nested images, in document order. Every paragraph of every box
    // is emitted: the `mc:AlternateContent` alternatives were settled before
    // parsing (`mc.rs`, #572), so a box arrives once — docling's text-keyed
    // dedup in `_handle_textbox_content`, which existed for that duplicate,
    // also dropped a paragraph legitimately repeated in a box, and is not
    // reproduced.
    if !skip_textbox {
        for tc in p.descendants().filter(|n| n.has_tag_name("txbxContent")) {
            // Every paragraph of the box — docling's `.//w:p` — so a table
            // inside a text box reads as its cells' paragraphs (#532); one in
            // a box nested deeper belongs to that box's own pass.
            let paragraphs = tc.descendants().filter(|n| {
                n.has_tag_name("p")
                    && n.ancestors().find(|a| a.has_tag_name("txbxContent")) == Some(tc)
            });
            for tp in paragraphs {
                let trimmed = paragraph_markdown(tp, ctx).trim().to_string();
                // Process the paragraph fully (list items, formatting); `skip_textbox`
                // stops it re-extracting nested textboxes, which this loop covers.
                if !trimmed.is_empty() {
                    handle_paragraph_inner(tp, ctx, state, doc, false, true);
                } else {
                    // docling runs `_handle_text_elements` for *every* deduped
                    // textbox paragraph, so an empty one still yields an empty
                    // text item (DocLang `<text></text>`; Markdown drops it).
                    doc.push(Node::Paragraph {
                        text: String::new(),
                    });
                }
                for drawn in drawing_images(tp, ctx, false) {
                    push_drawn(doc, drawn);
                }
            }
        }
    }

    // docling emits a paragraph's images *before* its text for every paragraph
    // kind — the body loop's `drawing_blip`/`vml`/`drawingml` branches run
    // `_handle_pictures`/`_handle_drawingml` and only then the text handler, so
    // a heading, list item (even an empty one) or checkbox paragraph carries
    // its picture too. Images inside a textbox are skipped here — they're
    // extracted as textbox content above. Modern DrawingML (`<a:blip>`) wins
    // over legacy VML (`<v:imagedata>`), and blip-less shapes yield docling's
    // single rendered placeholder (see `drawing_images`).
    for drawn in drawing_images(p, ctx, true) {
        push_drawn(doc, drawn);
    }
    // Native charts anchored in this paragraph (docling PR #3809): classified
    // kind + cached data grid instead of a rendered-placeholder picture.
    for c in p.descendants().filter(|n| n.has_tag_name("chart")) {
        if let Some((kind, title, table)) = attr(c, "id").and_then(|id| ctx.charts.get(id)) {
            doc.push(Node::Chart {
                kind: kind.clone(),
                table: table.clone(),
                caption: title.clone(),
                location: None,
            });
        }
    }

    // Equations. A paragraph whose only content is OMML becomes one or more
    // standalone `$$…$$` formulas; otherwise its equations are woven inline
    // (`$…$`) into the surrounding text while the paragraph keeps its list /
    // heading role — mirroring docling's `_handle_equations_in_text` and the
    // inline-group serialization that puts a space between every child (so an
    // inline formula picks up a space on each side, doubling the space that the
    // preceding text run already carries).
    let eq_parts = collect_equation_parts(p);
    let has_equations = eq_parts.iter().any(|part| matches!(part, EqPart::Eq(_)));
    if has_equations && run_text(&eq_parts).trim().is_empty() {
        for part in &eq_parts {
            if let EqPart::Eq(eq) = part {
                if !eq.is_empty() {
                    doc.push(Node::Paragraph {
                        text: format!("$${eq}$$"),
                    });
                }
            }
        }
        state.end_list_run();
        state.last_list_num_id = None;
        return;
    }

    // A `<w14:checkbox>` paragraph becomes a task-list item (`- [x]` / `- [ ]`),
    // with the literal checkbox glyph stripped from the text.
    if p.descendants().any(|n| n.has_tag_name("checkbox")) {
        let checked = p
            .descendants()
            .find(|n| n.has_tag_name("checked"))
            .and_then(|n| attr(n, "val"))
            == Some("1");
        let text = clean_checkbox_symbols(&paragraph_markdown(p, ctx))
            .trim_end()
            .to_string();
        doc.push(Node::CheckboxItem { checked, text });
        state.end_list_run();
        state.last_list_num_id = None;
        return;
    }

    let text = if has_equations {
        serialize_inline_equations(&eq_parts)
    } else {
        paragraph_markdown(p, ctx)
    };

    // Numbering can come from the paragraph (a direct `numId` of 0 turns it off
    // and overrides the style) or be inherited from the paragraph's style.
    let numbering = if own_num_pr(p).is_some() {
        num_pr(p)
    } else {
        style_numbering(style_id, ctx)
    };

    // A heading style wins over a list: a numbered heading gets a computed
    // number prefix (`## 1 Section 1`) rather than becoming a list item.
    // Heading detection has two independent signals (docling#3961, #270): the
    // style *name* — id, display name, and their `basedOn` counterparts —
    // carries the level for heading-named styles, while `w:outlineLvl` is
    // OOXML's own language-independent marker — what Word's navigation pane
    // and generated TOCs read. The outline level is authoritative when the
    // style is a named heading, and it alone promotes a localized or custom
    // style ("Nadpis1", "Rubrik 1") the name check can't see. A Title style
    // keeps its own branch (level 1) either way, and outlineLvl 9 is OOXML's
    // "body text" sentinel, not a heading.
    let named = named_heading_level(style_id, &style_name, ctx);
    let outline = inherited_outline(style_id, ctx).filter(|l| (1..=9).contains(l));
    let level = match (named, outline) {
        (Some(1), _) => Some(1), // Title
        (Some(_), Some(out)) => Some(out + 1),
        (named, Some(out))
            if ctx.outline_headings && !is_title_style(style_id, &style_name, ctx) =>
        {
            debug_assert!(named.is_none());
            Some(out + 1)
        }
        (named, _) => named,
    };
    if let Some(level) = level {
        // A heading prints its plain text upstream, not the formatted runs.
        let text = if has_equations {
            text
        } else {
            heading_markdown(p, ctx)
        };
        if !text.is_empty() {
            // docling#3760 (`_is_numbered_heading`): the prefix is computed only
            // when numbering.xml gives the heading's level a *visible* format —
            // a `numFmt` of `none` (Word's "invisible numbering", used to keep
            // a heading in the outline without a number) leaves the text as is.
            let visibly_numbered = numbering.as_ref().is_some_and(|(num_id, ilvl)| {
                ctx.num_levels
                    .get(&(num_id.clone(), *ilvl))
                    .is_some_and(|l| l.visible)
            });
            let text = if visibly_numbered {
                let docling_level = level.saturating_sub(1).max(1);
                let text = numbered_heading_text(&mut state.numbered_headers, docling_level, &text);
                if let Some((num_id, ilvl)) = &numbering {
                    sync_heading_counter(
                        &mut state.counters,
                        ctx.num_levels,
                        state.numbered_headers[&docling_level] as i64,
                        num_id,
                        *ilvl,
                    );
                }
                text
            } else {
                text
            };
            doc.push(Node::Heading { level, text });
            state.seen_heading = true;
        }
        state.end_list_run();
        state.last_list_num_id = None;
        return;
    }

    // A code paragraph — either its style marks it as code, or it is set in a
    // monospaced font and reads like code (docling PR #3735). Merged into the
    // preceding fenced block when it directly follows one.
    let prev_is_code = matches!(doc.nodes.last(), Some(Node::Code { .. }));
    if !rich && (is_code_style(style_id, ctx) || is_code_by_font(p, style_id, ctx, prev_is_code)) {
        state.end_list_run();
        state.last_list_num_id = None;
        // Keep leading indentation (code blocks are verbatim); trailing space
        // is dropped, matching docling's `raw_paragraph_text.rstrip()`.
        let code_text = plain_paragraph_text(p).trim_end().to_string();
        if prev_is_code {
            if let Some(Node::Code { text: prev, .. }) = doc.nodes.last_mut() {
                if !code_text.is_empty() {
                    prev.push('\n');
                    prev.push_str(&code_text);
                }
                return;
            }
        }
        if !code_text.is_empty() {
            doc.push(Node::Code {
                language: detect_code_language(&code_text),
                text: code_text,
                orig: None,
                pretty: None,
            });
        }
        return;
    }

    // A caption numbered through a heading-relative template (`Tabell %1.%7`)
    // carries its number in its text and is no list item (#765).
    let caption_marker = numbering.as_ref().and_then(|(num_id, ilvl)| {
        caption_numbering_marker(
            &mut state.counters,
            ctx.num_levels,
            num_id,
            *ilvl,
            if style_id.is_empty() {
                "Normal"
            } else {
                style_id
            },
        )
    });
    let (numbering, text) = match caption_marker {
        Some(marker) if !has_equations => (None, paragraph_markdown_with_prefix(p, ctx, &marker)),
        Some(_) => (None, text),
        None => (numbering, text),
    };

    if let Some((num_id, ilvl)) = numbering {
        let numbered = ctx
            .num_levels
            .get(&(num_id.clone(), ilvl))
            .map(|l| l.numbered)
            .unwrap_or(false);
        // The base indent of the current contiguous list run (the first item's
        // `ilvl`); nesting spans numIds within the run.
        let open = state.list_run_base.is_some();
        state.list_run_base.get_or_insert(ilvl);
        if text.is_empty() {
            return;
        }
        // Word's list identity is the `numId`: the item continues the last
        // list when it carries the same one and no body content intervened;
        // anything else is a new list, so the serializers never have to infer
        // the boundary from the numbering (#385). An item of another `numId`
        // indented deeper nests under the item above it (docling#4465).
        let left_indent = list_left_indent(p, &num_id, ilvl, ctx);
        let (level, first_in_list) = state.list_item_position(&num_id, ilvl, left_indent, open);
        if numbered {
            get_list_counter(&mut state.counters, ctx.num_levels, &num_id, ilvl);
            let marker = build_enum_marker(&state.counters, ctx.num_levels, &num_id, ilvl);
            // `number` is the marker's last numeric component so the DocLang
            // serializer breaks a new ordered list when the sequence jumps
            // (e.g. `1.1.1.` → `2.3.1.`).
            let number = marker
                .trim_end_matches(['.', ')'])
                .rsplit(['.', ')'])
                .next()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(1);
            if cached_regex!(r"^\d+[.)]$").is_match(&marker) {
                // A plain `N.` marker is an ordered item in both Markdown and
                // DocLang.
                doc.push(Node::ListItem {
                    ordered: true,
                    number,
                    first_in_list,
                    text,
                    level,
                    // docling's DOCX backend passes the enumeration marker.
                    marker: Some(marker),
                    location: None,
                    split: None,
                    dclx: None,
                    href: None,
                    layer: None,
                });
            } else {
                // Any other marker is an ordered DocLang item with a clean-text
                // `<marker>` — carried in `dclx` — and renders in Markdown by
                // docling-core's rules (`ensure_valid_list_item_marker` with
                // `orig_list_item_marker_mode=AUTO`): a marker of Unicode
                // digits and a dot (`１.`, docling#4336) is already valid and
                // prints verbatim; one holding a letter or digit — any
                // Unicode one since docling-core 2.100 (`1.1.`, `a)`, `第九条`,
                // `甲.`, `①`) — is kept as a text prefix behind the bullet
                // (`- 1.1. text`); one without any (`•`) is dropped and the
                // item is a plain bullet.
                let dclx = Some(ListItemDclx {
                    ordered: true,
                    marker: Some(marker.clone()),
                    text: text.clone(),
                    runs: Vec::new(),
                });
                let already_valid = cached_regex!(r"^\d+\.$").is_match(&marker);
                let informative = marker.chars().any(char::is_alphanumeric);
                let (ordered, marker, text) = if already_valid {
                    (true, Some(marker), text)
                } else if informative {
                    (false, None, format!("{marker} {text}"))
                } else {
                    (false, None, text)
                };
                doc.push(Node::ListItem {
                    ordered,
                    number,
                    first_in_list,
                    text,
                    level,
                    marker,
                    location: None,
                    split: None,
                    dclx,
                    href: None,
                    layer: None,
                });
            }
        } else {
            // A bullet item; inline equations carry structured `<formula>` runs in
            // the DocLang overlay while Markdown keeps the flat `$…$` text. Plain
            // bold/italic formatting is left to the flat-text re-parse, but a
            // segment with Markdown-invisible formatting (underline, strike,
            // sub/superscript) needs reconstructed runs to survive into DocLang.
            let dclx = if has_equations {
                Some(ListItemDclx {
                    ordered: false,
                    marker: None,
                    text: text.clone(),
                    runs: inline_equation_runs(&eq_parts),
                })
            } else {
                let groups = run_groups(paragraph_run_tuples(p, ctx));
                groups
                    .iter()
                    .any(|(_, f, _)| f.underline || f.strike || f.script != 0)
                    .then(|| ListItemDclx {
                        ordered: false,
                        marker: None,
                        text: text.clone(),
                        runs: groups
                            .into_iter()
                            .filter(|(t, _, _)| !t.is_empty())
                            .map(|(t, f, _)| f.to_inline_run(&t))
                            .collect(),
                    })
            };
            doc.push(Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list,
                text,
                level,
                marker: None,
                location: None,
                split: None,
                dclx,
                href: None,
                layer: None,
            });
        }
        return;
    }

    // A plain (non-list) paragraph ends the current list run. Body *text*
    // also ends the list's identity; an empty spacer paragraph does not, so
    // the same Word list resumes after it as one list (docling#3902).
    state.end_list_run();
    if !text.is_empty() {
        state.last_list_num_id = None;
    }

    if !text.is_empty() {
        if has_equations {
            // A body paragraph with inline equations becomes an InlineGroup whose
            // equation fragments are `<formula>` runs; Markdown/JSON keep the flat
            // `$…$` text.
            let runs = inline_equation_runs(&eq_parts);
            doc.push(docling_core::inline_paragraph_node(text, runs, false));
        } else if rich {
            // In a rich cell each format segment is its own block (docling adds
            // one TextItem per element, keeping its formatting): a plain segment
            // is a paragraph, a formatted one an InlineGroup whose single run
            // carries the style (`<text><underline>…</underline></text>`).
            // docling#3891 keeps a cell paragraph's runs in their inline
            // group, which moves into the cell as a whole: several groups
            // are one inline item, concatenated with their own spacing.
            let tuples = paragraph_run_tuples(p, ctx);
            let groups = run_groups(tuples.clone());
            let multi = groups.len() > 1;
            if multi {
                let groups = groups.clone();
                let runs = groups
                    .into_iter()
                    .filter(|(t, _, _)| !t.is_empty())
                    .map(|(t, f, _)| f.to_inline_run(&t))
                    .collect();
                doc.push(Node::InlineGroup {
                    unwrapped: false,
                    runs,
                    md_text: group_runs(tuples),
                });
            }
            for (t, f, l) in groups.into_iter().filter(|_| !multi) {
                let seg = serialize_run(&t, f, l.as_deref());
                if seg.is_empty() {
                    continue;
                }
                if f == Fmt::default() {
                    doc.push(Node::Paragraph { text: seg });
                } else {
                    doc.push(Node::InlineGroup {
                        unwrapped: false,
                        runs: vec![f.to_inline_run(&t)],
                        md_text: seg,
                    });
                }
            }
        } else {
            // A body paragraph with inline formatting becomes an InlineGroup so
            // DocLang carries the structure; Markdown/JSON still see `text`.
            // docling parents it on the body group — wrapped in `<text>` —
            // until a heading appears, after which content hangs off the
            // heading `TextItem` and the group serializes unwrapped. A single
            // formatted hyperlink stays a text item so its `<href>` head
            // survives (the runs drop link targets).
            let groups = run_groups(paragraph_run_tuples(p, ctx));
            let lone_link = groups.len() == 1 && groups[0].2.is_some();
            if lone_link {
                doc.push(Node::Paragraph { text });
            } else {
                let runs = groups
                    .into_iter()
                    .filter(|(t, _, _)| !t.is_empty())
                    .map(|(t, f, _)| f.to_inline_run(&t))
                    .collect();
                doc.push(docling_core::inline_paragraph_node(
                    text,
                    runs,
                    state.seen_heading,
                ));
            }
        }
    } else if !has_equations && !has_drawing(p) {
        // docling emits an empty text item for a blank paragraph — in the body
        // (`skip_empty_text=False`) and inside rich cells alike; paragraphs
        // carrying a drawing skip it. This is DocLang/JSON-only — Markdown
        // drops empty paragraphs.
        doc.push(Node::Paragraph {
            text: String::new(),
        });
    }
}

/// A drawing's picture, then the diagram code item it extracted (#858).
fn push_drawn(doc: &mut DoclingDocument, drawn: Drawn) {
    doc.push(Node::Picture {
        caption: None,
        caption_href: None,
        image: drawn.image,
        classification: None,
        description: drawn.alt.map(PictureDescription::alt_text),
        caption_parent: Default::default(),
        caption_location: None,
    });
    if let Some((language, text)) = drawn.diagram {
        doc.push(Node::Code {
            language: Some(language),
            text,
            orig: None,
            pretty: None,
        });
    }
}

/// Whether a paragraph carries any drawing (image/shape/textbox) — docling
/// suppresses the blank-paragraph text item in that case.
fn has_drawing(p: XmlNode) -> bool {
    p.descendants()
        .any(|n| matches!(n.tag_name().name(), "drawing" | "pict" | "object"))
}

/// Whether a node is inside a textbox (`<w:txbxContent>` or `<v:textbox>`),
/// whose images/text are handled by the textbox path, not the paragraph body.
/// One entry per drawing in `node` (resolved to its extracted image when known):
/// modern `<a:blip r:embed>` win over legacy `<v:imagedata r:id>`, and a
/// paragraph whose drawings carry *neither* (pure DrawingML shapes, charts,
/// SmartArt — docling's `drawingml_els` branch) yields **one** placeholder
/// picture: docling renders all of the paragraph's shapes through LibreOffice
/// into a single image (`_handle_drawingml` → `_convert_elements_via_docx`),
/// which serializes as the same `<!-- image -->` placeholder we emit without
/// rendering. (docling suppresses a render that comes back as an invisible
/// spacer; no corpus fixture hits that, so no geometry heuristic here.) With
/// `skip_textbox`, drawings nested in a textbox are excluded (they're extracted
/// as textbox text instead). Each entry carries the picture's alt text (#676,
/// [`drawing_alt`]) when the author wrote one. The blip-less drawings are
/// rendered in-process into one SVG payload (#858, `docx_drawingml`), the
/// picture staying payload-less only when nothing in them can be drawn.
fn drawing_images(node: XmlNode, ctx: &Ctx, skip_textbox: bool) -> Vec<Drawn> {
    let keep = |n: XmlNode| !skip_textbox || !in_textbox(n);
    let blips: Vec<XmlNode> = node
        .descendants()
        .filter(|n| n.has_tag_name("blip") && keep(*n))
        .collect();
    if !blips.is_empty() {
        return blips
            .iter()
            .map(|b| Drawn {
                image: with_extent(
                    attr(*b, "embed").and_then(|id| ctx.images.get(id)).cloned(),
                    *b,
                ),
                alt: drawing_alt(*b),
                diagram: None,
            })
            .collect();
    }
    let vml: Vec<Drawn> = node
        .descendants()
        .filter(|n| n.has_tag_name("imagedata") && keep(*n))
        .map(|d| Drawn {
            image: attr(d, "id").and_then(|id| ctx.images.get(id)).cloned(),
            alt: drawing_alt(d),
            diagram: None,
        })
        .collect();
    if !vml.is_empty() {
        return vml;
    }
    let drawings: Vec<XmlNode> = node
        .descendants()
        .filter(|n| {
            n.has_tag_name("drawing")
                && keep(*n)
                // A drawing whose graphic is a parsed chart emits a Chart node
                // instead of the rendered-placeholder picture.
                && !n.descendants().any(|c| {
                    c.has_tag_name("chart")
                        && attr(c, "id").is_some_and(|id| ctx.charts.contains_key(id))
                })
        })
        .collect();
    if !drawings.is_empty() {
        let rendered = super::docx_drawingml::render(&drawings, ctx.theme);
        let diagram = rendered
            .as_ref()
            .and_then(|r| super::docx_drawingml::diagram(r, ctx.extract_diagrams));
        return vec![Drawn {
            image: rendered.map(|r| r.image),
            alt: None,
            diagram,
        }];
    }
    Vec::new()
}

/// One picture of a paragraph: its payload, its alt text, and the diagram
/// code item that follows it under `extract_diagrams` (#858).
pub(super) struct Drawn {
    pub(super) image: Option<PictureImage>,
    pub(super) alt: Option<String>,
    pub(super) diagram: Option<(String, String)>,
}

/// The alt text of the picture an `<a:blip>` or `<v:imagedata>` belongs to
/// (#676): a DrawingML picture's `wp:docPr` (the drawing's document
/// properties, where Word keeps "Alt Text"), else its `pic:cNvPr`; a VML
/// shape's `alt` attribute.
pub(super) fn drawing_alt(image_ref: XmlNode) -> Option<String> {
    fn props<'a, 'input>(n: XmlNode<'a, 'input>, name: &str) -> Option<XmlNode<'a, 'input>> {
        n.children().find(|c| c.has_tag_name(name))
    }
    for a in image_ref.ancestors() {
        if a.has_tag_name("inline") || a.has_tag_name("anchor") {
            if let Some(text) = props(a, "docPr").and_then(super::ooxml::alt_text_of) {
                return Some(text);
            }
        }
        if a.has_tag_name("pic") {
            if let Some(text) = props(a, "nvPicPr")
                .and_then(|n| props(n, "cNvPr"))
                .and_then(super::ooxml::alt_text_of)
            {
                return Some(text);
            }
        }
        if a.has_tag_name("shape") {
            return attr(a, "alt")
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string);
        }
        if a.has_tag_name("p") {
            break;
        }
    }
    None
}

/// The size a DrawingML picture is displayed at (#834): the `wp:extent` of
/// the `wp:inline` / `wp:anchor` an `<a:blip>` sits in, EMU → inches
/// (914 400 EMU to the inch). `None` for VML and for a drawing without one.
pub(super) fn drawing_extent(image_ref: XmlNode) -> Option<[f32; 2]> {
    let holder = image_ref
        .ancestors()
        .take_while(|a| !a.has_tag_name("p"))
        .find(|a| a.has_tag_name("inline") || a.has_tag_name("anchor"))?;
    let extent = holder.children().find(|c| c.has_tag_name("extent"))?;
    let emu = |name: &str| attr(extent, name)?.parse::<f64>().ok();
    let (cx, cy) = (emu("cx")?, emu("cy")?);
    (cx > 0.0 && cy > 0.0).then(|| [(cx / 914_400.0) as f32, (cy / 914_400.0) as f32])
}

/// `image` with the display size of the drawing `image_ref` belongs to.
pub(super) fn with_extent(image: Option<PictureImage>, image_ref: XmlNode) -> Option<PictureImage> {
    image.map(|mut img| {
        img.display_size = drawing_extent(image_ref);
        img
    })
}

/// Parse every chart part related to `part`: relationship id →
/// `(classified kind, title, data grid from the embedded caches)`.
pub(super) fn chart_rels(
    pkg: &mut Package,
    part: &str,
) -> HashMap<String, (String, Option<String>, docling_core::Table)> {
    let dir = part
        .rsplit_once('/')
        .map(|(d, _)| d)
        .unwrap_or("")
        .to_string();
    let rels: Vec<(String, String)> = pkg
        .rels_for(part)
        .iter()
        .filter(|r| r.rel_type.ends_with("/chart"))
        .map(|r| (r.id.clone(), resolve(&dir, &r.target)))
        .collect();
    rels.into_iter()
        .filter_map(|(id, path)| {
            let spec = pkg
                .read(&path)
                .as_deref()
                .and_then(crate::backend::xlsx_drawings::parse_chart)?;
            let table = crate::backend::xlsx_drawings::chart_table_from_caches(&spec)?;
            Some((id, (spec.kind.to_string(), spec.title, table)))
        })
        .collect()
}

pub(super) fn in_textbox(n: XmlNode) -> bool {
    n.ancestors()
        .any(|a| a.has_tag_name("txbxContent") || a.has_tag_name("textbox"))
}

/// An attribute by *local* name, ignoring its namespace (OOXML attributes are
/// namespaced, e.g. `w:val`, which roxmltree's bare `attribute()` won't match).
pub(super) fn attr<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

/// Prepend a multilevel heading number (`1`, `1.1`, `2`, …) to a numbered
/// heading at docling level `level` (1-indexed), maintaining per-level counters.
/// Mirrors docling's `_add_heading` numbering: bump this level, zero deeper
/// consecutive levels, then walk up prefixing each ancestor's counter (filling a
/// skipped `0` ancestor with `1`, the "no empty sublevels" rule).
pub(super) fn numbered_heading_text(
    headers: &mut HashMap<u8, u64>,
    level: u8,
    text: &str,
) -> String {
    *headers.entry(level).or_insert(0) += 1;
    let mut out = format!("{} {}", headers[&level], text);

    let mut next = level + 1;
    while headers.contains_key(&next) {
        headers.insert(next, 0);
        next += 1;
    }

    let mut prev = level.wrapping_sub(1);
    while prev >= 1 && headers.contains_key(&prev) {
        let c = headers.get_mut(&prev).unwrap();
        if *c == 0 {
            *c = 1;
        }
        out = format!("{}.{}", *c, out);
        prev = prev.wrapping_sub(1);
    }
    out
}

/// A paragraph style and its `basedOn` ancestors, nearest first — bounded
/// by [`MAX_STYLE_DEPTH`], a cycle or a dangling reference ending the walk.
pub(super) fn style_chain<'a>(style_id: &'a str, ctx: &'a Ctx) -> Vec<&'a str> {
    let mut chain = Vec::new();
    let mut sid = style_id;
    while !sid.is_empty() && chain.len() < MAX_STYLE_DEPTH && !chain.contains(&sid) {
        chain.push(sid);
        match ctx.style_based.get(sid) {
            Some(parent) => sid = parent,
            None => break,
        }
    }
    chain
}

/// The labels heading detection reads for a style (#841): its id and
/// display name, then each `basedOn` ancestor's, nearest first — docling's
/// `_get_label_and_level` stops after one hop, so a corporate
/// "SubSectionTitleL2" based on "SectionTitleL2" based on Heading 2 was body
/// text for it (the `.doc` backend walks the whole chain since #824; the
/// two formats now read the same document alike).
pub(super) fn heading_labels<'a>(
    style_id: &'a str,
    style_name: &'a str,
    ctx: &'a Ctx,
) -> Vec<&'a str> {
    let mut labels = Vec::new();
    for (i, sid) in style_chain(style_id, ctx).into_iter().enumerate() {
        // An explicit body-text outline (`w:outlineLvl` 9, stored as 10)
        // ends the climb: the style itself keeps docling's name check (a
        // built-in "heading 2" opted out of the outline is still named so),
        // but a style that opts out is not a heading through what it is
        // based on, and nothing above it is read — the `.doc` backend's rule.
        let body_text = ctx.style_outline.get(sid) == Some(&10);
        if i > 0 && body_text {
            break;
        }
        labels.push(sid);
        let name = if i == 0 {
            Some(style_name)
        } else {
            ctx.style_names.get(sid).map(String::as_str)
        };
        labels.extend(name);
        if body_text {
            break;
        }
    }
    labels
}

/// The 1-indexed `w:outlineLvl` a style resolves to: its own, else the
/// nearest `basedOn` ancestor's (#841 — docling reads the style's own only;
/// the `.doc` backend inherits it since #824). An explicit level on the way
/// up, OOXML's body-text 9 included (stored as 10 here, outside the heading
/// range the callers keep), ends the walk: a style that opts out of the
/// outline is body text whatever it is based on.
pub(super) fn inherited_outline(style_id: &str, ctx: &Ctx) -> Option<u8> {
    style_chain(style_id, ctx)
        .into_iter()
        .find_map(|sid| ctx.style_outline.get(sid).copied())
}

/// Markdown heading level from a paragraph style, by name — the name path of
/// docling's `_get_label_and_level` (#270 tail: custom styles without
/// `w:outlineLvl`). The style *id*, its display name, and their `basedOn`
/// counterparts up the chain ([`heading_labels`]; docling stops at one hop,
/// #841) are checked in that order for a "heading" substring (docling's
/// `is_heading`), and the FIRST label containing it decides alone —
/// upstream returns straight out of `_get_heading_and_level`, so later labels
/// never rescue an unparseable one. That is what promotes a custom style whose
/// only heading marker is its ancestry ("MyStyle" based on "Heading 2") or its
/// id ("Heading1" with a localized display name and no outline level).
///
/// docling renders a heading at level `N` as `#`×(N+1) and a Title as `#`; so
/// "heading 1" → `##`, an exact "title" name → `#`.
fn named_heading_level(style_id: &str, style_name: &str, ctx: &Ctx) -> Option<u8> {
    if style_name.eq_ignore_ascii_case("title") {
        return Some(1);
    }
    for label in heading_labels(style_id, style_name, ctx) {
        if label.to_ascii_lowercase().contains("heading") {
            return heading_label_level(label);
        }
    }
    None
}

/// docling's `_get_heading_and_level` + `_split_text_and_number` on one label,
/// as a Markdown level (docling level + 1):
///
/// - `<text><digits>` (the whole label) or `<digits><text>` (rest ignored)
///   where the text part trims to "heading" case-insensitively → that level,
///   clamped to ≥1 (upstream's custom-"Heading 0" rule);
/// - no such digit split → a heading only when the label carries a capital-H
///   "Heading" (upstream re-checks `"Heading" in p_style_id` on the returned
///   label, case-sensitively — so the raw id "HeadingCustom" qualifies but a
///   display name "my heading" does not), at docling's level-less default of 1
///   (`_add_heading(curr_level=None)` adds at `add_level = 1`);
/// - a split whose text part isn't "heading" (e.g. "My Heading 2") → not a
///   heading by name (upstream's `("", 0)` branch).
fn heading_label_level(label: &str) -> Option<u8> {
    let is_digit = |c: char| c.is_ascii_digit();
    let split = match label.find(is_digit) {
        Some(0) => {
            let digits_end = label.find(|c| !is_digit(c)).unwrap_or(label.len());
            let text_end = label[digits_end..]
                .find(is_digit)
                .map_or(label.len(), |i| digits_end + i);
            (text_end > digits_end).then(|| (&label[digits_end..text_end], &label[..digits_end]))
        }
        Some(first) if label[first..].chars().all(is_digit) => {
            Some((&label[..first], &label[first..]))
        }
        Some(_) => None,
        None => None,
    };
    match split {
        Some((text, digits)) if text.trim().eq_ignore_ascii_case("heading") => {
            // OOXML headings are 1–9: a custom "Heading 0" or "Heading 111"
            // clamps into that range (docling#4319).
            let level = digits.parse::<u8>().unwrap_or(u8::MAX).clamp(1, 9);
            Some(level + 1)
        }
        Some(_) => None,
        // No digit split: level-less heading iff the label itself says
        // "Heading" with a capital H.
        None => label.contains("Heading").then_some(2),
    }
}

/// The raw `numPr` parts of a style definition: its own `numId` and `ilvl`
/// values, each optional (a stock `heading 2` carries only `ilvl`). `None`
/// when the style has no `numPr` at all.
fn num_pr_parts(style: XmlNode) -> Option<(Option<String>, Option<i64>)> {
    let num_pr = style.descendants().find(|n| n.has_tag_name("numPr"))?;
    let num_id = num_pr
        .children()
        .find(|n| n.has_tag_name("numId"))
        .and_then(|n| attr(n, "val"))
        .map(str::to_string);
    let ilvl = num_pr
        .children()
        .find(|n| n.has_tag_name("ilvl"))
        .and_then(|n| attr(n, "val"))
        .and_then(|v| v.parse().ok());
    Some((num_id, ilvl))
}

/// A style's `(numId, ilvl)` resolved through its `basedOn` chain — docling's
/// `_style_numbering` (docling#3917). Word inherits numbering through the
/// style hierarchy, and `numId` and `ilvl` inherit *independently*: Word's
/// stock `heading 2` carries only `ilvl` and takes `numId` from `heading 1`,
/// so reading one style element left it unnumbered while its siblings at
/// other levels were numbered. The walk stops once both parts are known or
/// after ten ancestors (a malformed/cyclic chain); a `numId` of 0 found on
/// the way means "no list", like a paragraph's own `numId` 0; `ilvl`
/// defaults to 0 when only `numId` was found.
pub(super) fn style_numbering(style_id: &str, ctx: &Ctx) -> Option<(String, i64)> {
    let (mut num_id, mut ilvl): (Option<String>, Option<i64>) = (None, None);
    let mut cur = Some(style_id.to_string());
    let mut depth = 0;
    while let Some(id) = cur {
        if depth >= MAX_STYLE_INHERITANCE_DEPTH {
            break;
        }
        if let Some((n, l)) = ctx.style_nums.get(&id) {
            if num_id.is_none() {
                num_id = n.clone();
            }
            if ilvl.is_none() {
                ilvl = *l;
            }
        }
        if num_id.is_some() && ilvl.is_some() {
            break;
        }
        cur = ctx.style_based.get(&id).cloned();
        depth += 1;
    }
    let num_id = num_id?;
    if num_id == "0" {
        return None;
    }
    Some((num_id, ilvl.unwrap_or(0)))
}

/// `(numId, ilvl)` for an element carrying explicit list numbering. A `numId`
/// of 0 means "no list" in OOXML and yields `None`.
/// The paragraph's own `w:pPr/w:numPr` — the only place it is valid
/// (docling#4456): a descendant search reaches the paragraphs of a textbox
/// anchored in this one and takes their numbering as its own, which made an
/// empty list item and a stray list group after every numbered textbox.
pub(super) fn own_num_pr<'a, 'i>(p: XmlNode<'a, 'i>) -> Option<XmlNode<'a, 'i>> {
    p.children()
        .find(|n| n.has_tag_name("pPr"))?
        .children()
        .find(|n| n.has_tag_name("numPr"))
}

pub(super) fn num_pr(p: XmlNode) -> Option<(String, i64)> {
    let num_pr = own_num_pr(p)?;
    let num_id_node = num_pr.children().find(|n| n.has_tag_name("numId"))?;
    let num_id = attr(num_id_node, "val")?.to_string();
    if num_id == "0" {
        return None;
    }
    let ilvl = num_pr
        .children()
        .find(|n| n.has_tag_name("ilvl"))
        .and_then(|n| attr(n, "val"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Some((num_id, ilvl))
}

/// Build a paragraph's Markdown. Consecutive runs with the *same* formatting are
/// concatenated into one group (so "Paragraph 1" + ".1.1" → "Paragraph 1.1.1"),
/// each group is stripped and wrapped in its markers, and groups are joined with
/// single spaces — mirroring docling's `_get_paragraph_elements`.
fn paragraph_markdown(p: XmlNode, ctx: &Ctx) -> String {
    group_runs(paragraph_run_tuples(p, ctx))
}

/// A heading's or title's Markdown: its text without inline formatting or
/// link targets. docling's `_add_heading` / title branch take
/// `paragraph.text` — the runs' (hyperlinks' included) text concatenated
/// and stripped — not the formatted paragraph elements, so neither a bold
/// heading style (Word's Heading 1 sets `w:b`) nor a bold run puts `**` in
/// a heading.
fn heading_markdown(p: XmlNode, ctx: &Ctx) -> String {
    let mut runs: Vec<(String, Fmt, Option<String>)> = Vec::new();
    collect_run_tuples(p, Fmt::default(), None, ctx, &mut runs);
    let text: String = runs.iter().map(|(t, _, _)| t.as_str()).collect();
    escape_text(text.trim())
}

/// [`paragraph_markdown`] with `prefix` and a space put before the first
/// non-blank run, inside that run's formatting — docling's
/// `_apply_caption_numbering` (#765).
fn paragraph_markdown_with_prefix(p: XmlNode, ctx: &Ctx, prefix: &str) -> String {
    let mut runs = paragraph_run_tuples(p, ctx);
    if let Some(run) = runs.iter_mut().find(|r| !r.0.trim().is_empty()) {
        run.0 = format!("{prefix} {}", run.0.trim_start());
    }
    group_runs(runs)
}

/// All element children of a node.
pub(super) fn child_elements<'a, 'i>(n: XmlNode<'a, 'i>) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    n.children().filter(XmlNode::is_element)
}

/// Strip a leading checkbox glyph (matches docling's `_clean_checkbox_symbols`).
pub(super) fn clean_checkbox_symbols(text: &str) -> String {
    // Only the leading edge (docling#3891): the text may be the first of
    // several inline runs.
    let t = text.trim_start();
    for sym in ['☐', '☑', '☒', '□', '■', '▪', '▫'] {
        if let Some(rest) = t.strip_prefix(sym) {
            return rest.trim_start().to_string();
        }
    }
    t.to_string()
}

/// Group a paragraph's runs the docling way: consecutive runs with the *same*
/// formatting are concatenated (and stripped) into one `(text, format, link)`
/// group; a hyperlink always starts its own group. This is docling's
/// `_get_paragraph_elements` segmentation, shared by the Markdown and the
/// structured-run builders so both see the same text items.
fn run_groups(runs: Vec<(String, Fmt, Option<String>)>) -> Vec<(String, Fmt, Option<String>)> {
    group_by_format(runs, true)
}

/// docling#3891's `_get_paragraph_elements`: the same groups concatenated
/// as they are — in DOCX the boundary space very often lives inside a
/// formatted run, and it is that run's own text — then only the
/// paragraph's outer edges trimmed (`trim_run_edges`). The Markdown side
/// reads these; the DocLang runs keep [`run_groups`]' stripped ones.
fn faithful_groups(runs: Vec<(String, Fmt, Option<String>)>) -> Vec<(String, Fmt, Option<String>)> {
    docling_core::inline::trim_run_edges(
        group_by_format(runs, false),
        |g| g.0.as_str(),
        |g, t| g.0 = t,
        |_| true,
    )
}

/// The grouping behind [`run_groups`] (`strip`: each group stripped, blank
/// ones dropped — the pre-#997 shape) and [`faithful_groups`].
fn group_by_format(
    runs: Vec<(String, Fmt, Option<String>)>,
    strip: bool,
) -> Vec<(String, Fmt, Option<String>)> {
    let keep = |t: &str| {
        if strip {
            !t.trim().is_empty()
        } else {
            !t.is_empty()
        }
    };
    let cut = |t: &str| {
        if strip {
            t.trim().to_string()
        } else {
            t.to_string()
        }
    };
    let mut groups: Vec<(String, Fmt, Option<String>)> = Vec::new();
    let mut group_text = String::new();
    let mut previous_format: Option<Fmt> = None;
    let mut last_format = Fmt::default();
    for (text, fmt, link) in runs {
        last_format = fmt;
        if (!text.trim().is_empty() && Some(fmt) != previous_format) || link.is_some() {
            if keep(&group_text) {
                groups.push((cut(&group_text), previous_format.unwrap_or_default(), None));
            }
            group_text.clear();
            if link.is_some() {
                groups.push((cut(&text), fmt, link));
                continue;
            }
            previous_format = Some(fmt);
        }
        group_text.push_str(&text);
    }
    if keep(&group_text) {
        // The trailing group closes under the format that *opened* it, as in
        // docling — a whitespace-only run never opens a group (its text is
        // just appended), so it must not decide the group's format either.
        // Taking the last run's format instead lost the bold/italic of a
        // paragraph that ends with an unformatted space (#408).
        groups.push((
            cut(&group_text),
            previous_format.unwrap_or(last_format),
            None,
        ));
    }
    groups
}

/// A paragraph's Markdown: one serialized segment per format group
/// (docling's `_get_paragraph_elements`), concatenated as they are — each
/// carries its own boundary whitespace, hoisted outside its markers
/// (schema 1.11.0, #997).
fn group_runs(runs: Vec<(String, Fmt, Option<String>)>) -> String {
    faithful_groups(runs)
        .iter()
        .map(|(t, f, l)| {
            docling_core::inline::hoist(t, |core| serialize_run(core, *f, l.as_deref()))
        })
        .collect()
}

/// One ordered fragment of a paragraph that mixes text and OMML: a raw text run
/// (`<w:t>`), or a converted `<m:oMath>` LaTeX string. Text keeps its original
/// whitespace — docling reconstructs the paragraph verbatim before splitting it.
enum EqPart {
    Text(String),
    Eq(String),
}

/// Whether a node lives inside an OMML subtree (its `<m:t>` is math, not text).
fn in_math(n: XmlNode) -> bool {
    n.ancestors()
        .any(|a| a.has_tag_name("oMath") || a.has_tag_name("oMathPara"))
}

/// Split a paragraph into ordered text/equation fragments — a port of docling's
/// `_handle_equations_in_text`. Direct `<m:oMath>` children are preferred (to
/// keep sibling order); otherwise a deep walk picks up OMML wrapped in
/// `<m:oMathPara>` or other elements.
fn collect_equation_parts(p: XmlNode) -> Vec<EqPart> {
    let mut parts = Vec::new();
    let has_direct = child_elements(p).any(|c| c.has_tag_name("oMath"));
    if has_direct {
        for child in child_elements(p) {
            if child.has_tag_name("oMath") {
                let eq = crate::backend::omml::to_latex(child);
                if !eq.is_empty() {
                    parts.push(EqPart::Eq(eq));
                }
            } else {
                for t in child.descendants().filter(|n| !in_math(*n)) {
                    if let Some(txt) = flat_text(t) {
                        parts.push(EqPart::Text(txt.to_string()));
                    }
                }
            }
        }
    } else {
        for node in p.descendants() {
            if !in_math(node) {
                if let Some(txt) = flat_text(node) {
                    parts.push(EqPart::Text(txt.to_string()));
                }
            } else if node.has_tag_name("oMath") {
                let eq = crate::backend::omml::to_latex(node);
                if !eq.is_empty() {
                    parts.push(EqPart::Eq(eq));
                }
            }
        }
    }
    parts
}

/// The paragraph's plain (non-math) run text — empty means the paragraph holds
/// only equations, which are then emitted as standalone `$$…$$` blocks.
fn run_text(parts: &[EqPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            EqPart::Text(t) => Some(t.as_str()),
            EqPart::Eq(_) => None,
        })
        .collect()
}

/// Serialize a mixed text/equation paragraph the way docling's inline group does:
/// consecutive text runs are merged, the whole is stripped at its ends (and the
/// final text fragment fully stripped), then fragments — text escaped, equations
/// as `$…$` — are joined with single spaces.
fn serialize_inline_equations(parts: &[EqPart]) -> String {
    // Merge consecutive text fragments (docling splits the reconstructed text on
    // each equation marker, so text between two equations is a single element).
    let mut merged: Vec<EqPart> = Vec::new();
    for part in parts {
        match part {
            EqPart::Text(t) => {
                if let Some(EqPart::Text(last)) = merged.last_mut() {
                    last.push_str(t);
                } else {
                    merged.push(EqPart::Text(t.clone()));
                }
            }
            EqPart::Eq(e) => merged.push(EqPart::Eq(e.clone())),
        }
    }

    let n = merged.len();
    let mut out: Vec<String> = Vec::new();
    for (i, part) in merged.iter().enumerate() {
        match part {
            EqPart::Eq(e) => out.push(format!("${e}$")),
            EqPart::Text(t) => {
                // Only the paragraph's outer edges are trimmed: the
                // whitespace around an inline equation is a run boundary
                // (`$A=\pi r^{2}$ is the area formula`, docling#3891).
                let mut s = t.as_str();
                if i == 0 {
                    s = s.trim_start();
                }
                if i == n - 1 {
                    s = s.trim_end();
                }
                if !s.is_empty() {
                    out.push(escape_text(s));
                }
            }
        }
    }
    out.concat()
}

/// Structured [`InlineRun`]s for a mixed text/equation paragraph — the DocLang
/// side of [`serialize_inline_equations`]. Text fragments become plain runs
/// (with the same end-trimming: the whole is stripped at its ends, the final
/// fragment fully) and each `<m:oMath>` becomes a `formula` run carrying LaTeX.
/// docling parents these under an `InlineGroup`; the flat `$…$` `md_text` still
/// drives Markdown/JSON.
fn inline_equation_runs(parts: &[EqPart]) -> Vec<InlineRun> {
    let mut merged: Vec<EqPart> = Vec::new();
    for part in parts {
        match part {
            EqPart::Text(t) => {
                if let Some(EqPart::Text(last)) = merged.last_mut() {
                    last.push_str(t);
                } else {
                    merged.push(EqPart::Text(t.clone()));
                }
            }
            EqPart::Eq(e) => merged.push(EqPart::Eq(e.clone())),
        }
    }

    let n = merged.len();
    let mut runs = Vec::new();
    for (i, part) in merged.iter().enumerate() {
        match part {
            EqPart::Eq(e) => runs.push(InlineRun {
                text: e.clone(),
                formula: true,
                ..InlineRun::default()
            }),
            EqPart::Text(t) => {
                let s = if i == n - 1 {
                    t.trim()
                } else if i == 0 {
                    t.trim_start()
                } else {
                    t.as_str()
                };
                if !s.is_empty() {
                    runs.push(InlineRun {
                        text: s.to_string(),
                        ..InlineRun::default()
                    });
                }
            }
        }
    }
    runs
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
    /// No Markdown marker, but underline/script still split a run into its own
    /// format group (so neighbours aren't merged into one segment).
    underline: bool,
    /// 0 = baseline, 1 = subscript, 2 = superscript.
    script: u8,
}

impl Fmt {
    /// The structured [`InlineRun`] for a text segment under this formatting.
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

/// The bold a paragraph lends its runs beyond their own `rPr` — the two
/// paragraph-level steps of docling's `_get_format_from_run` (the item tree's
/// [`super::docx_tree`] `format_from_run`): the paragraph mark's
/// `w:pPr/w:rPr/w:b`, then the paragraph style's `basedOn` chain. docling ORs
/// them onto the run's own bold — an explicit `<w:b w:val="0"/>` on the run
/// does not switch a bold style off there — so a paragraph in Word's bold
/// Caption style prints `**…**`. A hyperlink's runs are built against the
/// paragraph too (`Run(r_el, paragraph)` in docling 2.140), so both steps
/// reach the link text.
fn paragraph_bold(p: XmlNode, ctx: &Ctx) -> bool {
    let p_pr = p.children().find(|n| n.has_tag_name("pPr"));
    let mark = p_pr
        .and_then(|pr| pr.children().find(|n| n.has_tag_name("rPr")))
        .is_some_and(|rp| {
            rp.children()
                .any(|n| n.has_tag_name("b") && !matches!(attr(n, "val"), Some("0" | "false")))
        });
    mark || style_chain_bold(paragraph_style_id(p, ctx), ctx)
}

/// Whether a paragraph style or one of its `basedOn` ancestors sets
/// `w:rPr/w:b` on — python-docx's `style.font.bold` climbed through
/// `base_style`, as docling's `_get_format_from_run` does (bounded, as a
/// `basedOn` cycle would otherwise loop).
pub(super) fn style_chain_bold(style_id: &str, ctx: &Ctx) -> bool {
    let mut sid = style_id;
    for _ in 0..10 {
        if sid.is_empty() {
            return false;
        }
        if ctx.style_bold.get(sid) == Some(&true) {
            return true;
        }
        match ctx.style_based.get(sid) {
            Some(b) => sid = b,
            None => return false,
        }
    }
    false
}

/// A paragraph's `(raw text, format, hyperlink url)` tuples, its runs
/// formatted the way docling's `_get_format_from_run` sees them (the run's
/// own `rPr` plus [`paragraph_bold`]).
fn paragraph_run_tuples(p: XmlNode, ctx: &Ctx) -> Vec<(String, Fmt, Option<String>)> {
    let mut out = Vec::new();
    let base = Fmt {
        bold: paragraph_bold(p, ctx),
        ..Fmt::default()
    };
    collect_run_tuples(p, base, None, ctx, &mut out);
    out
}

/// Flatten a node's runs to `(raw text, format, hyperlink url)` tuples. A
/// hyperlink's runs are merged into a single element so the whole link text is
/// wrapped once.
fn collect_run_tuples(
    node: XmlNode,
    fmt: Fmt,
    link: Option<&str>,
    ctx: &Ctx,
    out: &mut Vec<(String, Fmt, Option<String>)>,
) {
    for child in node.children().filter(XmlNode::is_element) {
        collect_one(child, fmt, link, ctx, out);
    }
}

/// Process a single inline child (run, hyperlink, or transparent wrapper),
/// appending `(text, format, link)` tuples.
fn collect_one(
    child: XmlNode,
    fmt: Fmt,
    link: Option<&str>,
    ctx: &Ctx,
    out: &mut Vec<(String, Fmt, Option<String>)>,
) {
    match child.tag_name().name() {
        "r" => {
            let run_fmt = run_format(child, fmt);
            // A run interleaves text (`<w:t>`) and line breaks (`<w:br>`/`<w:cr>`,
            // rendered as newlines that stay in the paragraph block).
            let text: String = child_elements(child).map(run_child_text).collect();
            if !text.is_empty() {
                out.push((text, run_fmt, link.map(str::to_string)));
            }
        }
        "hyperlink" => {
            let url = hyperlink_target(child, ctx);
            let mut inner = Vec::new();
            collect_run_tuples(child, fmt, None, ctx, &mut inner);
            let text: String = inner.iter().map(|(t, _, _)| t.as_str()).collect();
            let lfmt = inner.first().map(|(_, f, _)| *f).unwrap_or(fmt);
            if !text.trim().is_empty() {
                out.push((text, lfmt, url.or(link.map(str::to_string))));
            }
        }
        // Transparent inline wrappers — `customXml` included (#589): a run
        // inside a custom XML markup element is ordinary text to Word,
        // python-docx and the JSON walk alike, and the Markdown used to drop
        // it (`Prefix text.  Suffix text.`).
        "smartTag" | "customXml" | "fldSimple" | "sdt" | "sdtContent" => {
            collect_run_tuples(child, fmt, link, ctx, out)
        }
        // Tracked changes (#851): an insertion (or a move's destination)
        // shows unless the view rejects it; a deletion (or a move's
        // source) shows under `reject`, and under `all` as struck text —
        // the Markdown's only way to show what Pandoc's `Span .deletion`
        // types (the item tree carries the typed revision).
        "ins" | "moveTo" => {
            if ctx.policy() != TrackChanges::Reject {
                collect_run_tuples(child, fmt, link, ctx, out)
            }
        }
        "del" | "moveFrom" => match ctx.policy() {
            TrackChanges::Accept => {}
            TrackChanges::Reject => collect_run_tuples(child, fmt, link, ctx, out),
            TrackChanges::All => collect_run_tuples(
                child,
                Fmt {
                    strike: true,
                    ..fmt
                },
                link,
                ctx,
                out,
            ),
        },
        _ => {}
    }
}

fn run_format(r: XmlNode, base: Fmt) -> Fmt {
    let Some(r_pr) = r.children().find(|n| n.has_tag_name("rPr")) else {
        return base;
    };
    let on = |name: &str| -> bool {
        r_pr.children()
            .find(|n| n.has_tag_name(name))
            .map(|n| attr(n, "val") != Some("false") && attr(n, "val") != Some("0"))
            .unwrap_or(false)
    };
    let script = match r_pr
        .children()
        .find(|n| n.has_tag_name("vertAlign"))
        .and_then(|n| attr(n, "val"))
    {
        Some("subscript") => 1,
        Some("superscript") => 2,
        _ => base.script,
    };
    Fmt {
        bold: base.bold || on("b"),
        italic: base.italic || on("i"),
        strike: base.strike || on("strike"),
        underline: base.underline || on("u"),
        script,
    }
}

/// Wrap a run's text in its Markdown markers (bold inner, italic, strike, then
/// hyperlink outermost — so bold+italic collapses to `***…***`). Underline and
/// sub/superscript carry no marker.
fn serialize_run(text: &str, fmt: Fmt, link: Option<&str>) -> String {
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
    if let Some(url) = link {
        s = docling_core::hyperlink::markdown_link(&s, url);
    }
    s
}

fn parse_table(tbl: XmlNode, ctx: &Ctx) -> Option<Table> {
    parse_table_with(tbl, ctx, false)
}

/// `nested` = this table is being flattened inside a rich cell, so every cell is
/// rendered as plain text (docling's `_collect_subtree_text` drops formatting).
fn parse_table_with(tbl: XmlNode, ctx: &Ctx, nested: bool) -> Option<Table> {
    if ctx.table_depth.get() >= MAX_TABLE_DEPTH {
        return None;
    }
    ctx.table_depth.set(ctx.table_depth.get() + 1);
    let out = parse_table_inner(tbl, ctx, nested);
    ctx.table_depth.set(ctx.table_depth.get() - 1);
    out
}

fn parse_table_inner(tbl: XmlNode, ctx: &Ctx, nested: bool) -> Option<Table> {
    let rows: Vec<XmlNode> = tbl
        .children()
        .filter(|n| n.has_tag_name("tr") && row_kept(*n, ctx))
        .collect();
    // A row's width in grid columns includes the leading/trailing skipped
    // columns (`w:gridBefore`/`w:gridAfter`) — a row that starts late still
    // spans the full grid.
    let num_cols = rows
        .iter()
        .map(|r| {
            let (before, after) = row_grid_offsets(*r);
            before + after + row_cells(*r).into_iter().map(grid_span).sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    if rows.is_empty() || num_cols == 0 {
        return None;
    }

    let mut grid: Vec<Vec<String>> = vec![vec![String::new(); num_cols]; rows.len()];
    // Structured block content for rich cells (dclx-only; Markdown/JSON use the
    // flat `grid` text). Never built for a `nested` (flattened) table.
    let mut blocks: Vec<Vec<Vec<Node>>> = vec![vec![Vec::new(); num_cols]; rows.len()];
    let mut any_rich = false;
    // OTSL span continuations (dclx-only): `gridSpan` columns beyond the first
    // continue horizontally (`<lcel/>`), `vMerge` continuations vertically
    // (`<ucel/>`), a covered corner both (`<xcel/>`).
    let mut col_cont = vec![vec![false; num_cols]; rows.len()];
    let mut row_cont = vec![vec![false; num_cols]; rows.len()];
    let mut any_span = false;
    // A merged cell's text fills every grid position it covers, a rich cell's
    // as much as a plain one's (docling-core#857, 2.102, #777: the
    // serializers resolve a `RichTableCell` once per table and repeat it, where
    // they used to leave the rest of the merge empty).
    // Counted for the outermost table only: a nested table's rich cells are
    // part of the outer cell's walk.
    let top_level = !nested && ctx.table_depth.get() == 1;
    let mut orphans = 0usize;
    for (ri, row) in rows.iter().enumerate() {
        // The grid cursor starts past the row's skipped leading columns
        // (`w:gridBefore`) and advances by each cell's span — true grid
        // coordinates, never inferred from what an earlier row left behind
        // (upstream docling had the same positional-index conflation,
        // fixed in docling PR #3745: cells dropped and merges broken on
        // late-starting rows).
        let mut ci = row_grid_offsets(*row).0;
        for tc in row_cells(*row) {
            let span = grid_span(tc);
            // A continuation cell of a vertical merge repeats the cell above.
            let v_continue = tc
                .descendants()
                .find(|n| n.has_tag_name("vMerge"))
                .map(|n| attr(n, "val").unwrap_or("continue") != "restart")
                .unwrap_or(false);
            let text = if v_continue && ri > 0 {
                grid[ri - 1][ci].clone()
            } else if nested {
                tc.children()
                    .filter(|n| n.has_tag_name("p"))
                    .map(plain_paragraph_text)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                cell_markdown(tc, ctx)
            };
            if ci < num_cols {
                let cb = if v_continue && ri > 0 {
                    blocks[ri - 1][ci].clone()
                } else if nested {
                    Vec::new()
                } else {
                    cell_blocks_of(tc, ctx)
                };
                if !cb.is_empty() {
                    any_rich = true;
                    blocks[ri][ci] = cb;
                }
            }
            let col_end = (ci + span).min(num_cols);
            for cell in grid[ri].iter_mut().take(col_end).skip(ci) {
                *cell = text.clone();
            }
            if top_level && ci < col_end && !(v_continue && ri > 0) && is_rich_cell(tc) {
                orphans += super::docx_tree::rich_cell_orphan_groups(tc, ctx);
            }
            for c in col_cont[ri].iter_mut().take(col_end).skip(ci + 1) {
                *c = true;
                any_span = true;
            }
            if v_continue && ri > 0 {
                for c in row_cont[ri].iter_mut().take(col_end).skip(ci) {
                    *c = true;
                }
                any_span = true;
            }
            ci += span;
        }
    }
    if top_level {
        ctx.orphan_inline_groups.set(orphans);
    }
    let structure = any_span.then(|| {
        let mut header_row = vec![false; rows.len()];
        if let Some(h) = header_row.first_mut() {
            *h = true;
        }
        docling_core::TableStructure {
            header_row,
            col_continuation: col_cont,
            row_continuation: row_cont,
            row_header: Vec::new(),
            col_header: Vec::new(),
        }
    });
    Some(Table {
        footnotes: Vec::new(),
        rows: grid,
        location: None,
        structure,
        cell_blocks: any_rich.then_some(blocks),
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
        document_index: false,
    })
}

pub(super) fn grid_span(tc: XmlNode) -> usize {
    tc.descendants()
        .find(|n| n.has_tag_name("gridSpan"))
        .and_then(|n| attr(n, "val"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// The symbol-encoded fonts a run's `w:rFonts` names (#588): the `w:ascii`
/// font (characters below 0x80) and the `w:hAnsi` one (the rest — the
/// 0x80–0xFF byte codes and their Private Use Area images), each `None` for
/// an ordinary font or when the attribute is absent.
fn run_symbol_fonts(r: XmlNode) -> (Option<SymbolFont>, Option<SymbolFont>) {
    let Some(fonts) = r
        .children()
        .find(|n| n.has_tag_name("rPr"))
        .and_then(|pr| pr.children().find(|n| n.has_tag_name("rFonts")))
    else {
        return (None, None);
    };
    let font = |name: &str| attr(fonts, name).and_then(SymbolFont::from_name);
    (font("ascii"), font("hAnsi"))
}

/// The text of a `w:t` as Word shows it (#588): set in a symbol font
/// (`Symbol`, `Wingdings`, …), each glyph code — a byte or its PUA image —
/// becomes the Unicode character of that glyph; in any other font the text
/// is returned as it is. The font is the run's own `w:rFonts` (the run the
/// `w:t` sits in; a hyperlink's or content control's runs carry their own):
/// a symbol font inherited from a style is not resolved here.
pub(super) fn t_text<'a>(t: XmlNode<'a, '_>) -> Cow<'a, str> {
    let text = t.text().unwrap_or("");
    let Some(run) = t.parent().filter(|p| p.has_tag_name("r")) else {
        return Cow::Borrowed(text);
    };
    let (ascii, hansi) = run_symbol_fonts(run);
    if ascii.is_none() && hansi.is_none() || !text.chars().any(symbol_fonts::is_glyph_code) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .map(|c| {
                let font = if (c as u32) < 0x80 {
                    ascii.or(hansi)
                } else {
                    hansi.or(ascii)
                };
                font.and_then(|f| f.glyph(c as u32)).unwrap_or(c)
            })
            .collect(),
    )
}

/// A `w:sym` (Insert ▸ Symbol): the glyph `w:char` (hex, `F06C` or `6C`) of
/// the font `w:font` as Unicode (#588). A font this reader has no table for
/// yields the code point as Word stores it (the PUA image a reader with the
/// font would render), so the character is never silently dropped the way
/// python-docx's `CT_R.text` drops every `w:sym`.
pub(super) fn sym_text(n: XmlNode) -> String {
    let Some(code) = attr(n, "char").and_then(|c| u32::from_str_radix(c.trim(), 16).ok()) else {
        return String::new();
    };
    let mapped = attr(n, "font")
        .and_then(SymbolFont::from_name)
        .and_then(|f| f.glyph(code));
    mapped
        .or_else(|| char::from_u32(code).filter(|c| !c.is_control()))
        .map(String::from)
        .unwrap_or_default()
}

/// The plain text of one child of a `w:r`, python-docx's `CT_R.text` — which
/// is where docling's paragraph text comes from, so this is the parity target.
/// Anything not in its `w:br | w:cr | w:noBreakHyphen | w:ptab | w:t | w:tab`
/// set contributes nothing — except `w:sym`, which python-docx drops and this
/// reader maps to the glyph's Unicode character (#588).
pub(super) fn run_child_text(n: XmlNode) -> String {
    match n.tag_name().name() {
        "t" => t_text(n).into_owned(),
        // A deleted run's text (#851): read only when its `w:del` wrapper
        // is walked — under `reject` or `all`.
        "delText" => n.text().unwrap_or("").to_string(),
        "sym" => sym_text(n),
        // A *line* break is a newline; a page or column break has no text
        // equivalent at all.
        "br" => match attr(n, "type") {
            None | Some("textWrapping") => "\n".to_string(),
            Some(_) => String::new(),
        },
        "cr" => "\n".to_string(),
        // A hyphen Word marked as ineligible for a line wrap is still a hyphen
        // (#400): dropping it glued `In` and `Transit` into `InTransit`.
        "noBreakHyphen" => "-".to_string(),
        "tab" | "ptab" => "\t".to_string(),
        _ => String::new(),
    }
}

/// The plain text of a run inner-content element, for the places that flatten
/// a subtree with `descendants()` rather than walking a run's own children.
/// Only `w:t`, `w:sym` and `w:noBreakHyphen` are safe to pick up that way —
/// `w:tab` and `w:br` also appear in paragraph *properties*
/// (`w:pPr/w:tabs/w:tab`), which carry no text; `collect_run_tuples` handles
/// those from the run itself.
pub(super) fn flat_text<'a, 'i>(n: XmlNode<'a, 'i>) -> Option<Cow<'a, str>> {
    match n.tag_name().name() {
        "t" => Some(t_text(n)),
        "sym" => Some(Cow::Owned(sym_text(n))),
        "noBreakHyphen" => Some(Cow::Borrowed("-")),
        _ => None,
    }
}

/// A row's `w:tc` cells in document order, unwrapping any content control
/// (`w:sdt` → `w:sdtContent`) Word wrapped a cell in — its cover pages and
/// document-property fields are written that way, and the cell is then no
/// longer a direct child of the `w:tr` (docling#3946/#3951). Controls nest, so
/// the walk recurses. Without this a wrapped cell is skipped entirely: the
/// grid cursor advances only per emitted cell, so every later cell in the row
/// slides left under the wrong header, and a 1×1 layout table loses its only
/// cell — and with it all of its content.
pub(super) fn row_cells<'a, 'i>(tr: XmlNode<'a, 'i>) -> Vec<XmlNode<'a, 'i>> {
    let mut out = Vec::new();
    for child in tr.children().filter(XmlNode::is_element) {
        if child.has_tag_name("tc") {
            out.push(child);
        } else if child.has_tag_name("sdt") {
            if let Some(content) = child.children().find(|n| n.has_tag_name("sdtContent")) {
                out.extend(row_cells(content));
            }
        }
    }
    out
}

/// A row's skipped grid columns: `(w:gridBefore, w:gridAfter)` from its
/// `w:trPr`, 0 when absent.
pub(super) fn row_grid_offsets(tr: XmlNode) -> (usize, usize) {
    let read = |tag: &str| {
        tr.children()
            .find(|n| n.has_tag_name("trPr"))
            .and_then(|pr| pr.children().find(|n| n.has_tag_name(tag)))
            .and_then(|n| attr(n, "val"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (read("gridBefore"), read("gridAfter"))
}

/// A table cell's Markdown. A "plain" cell (one paragraph, unformatted runs, no
/// nested block/image) becomes its plain text; a "rich" cell renders its full
/// block content (paragraphs, lists, formatting), which the table serializer
/// then flattens (`\n` → space). Mirrors docling's `_is_rich_table_cell`,
/// plus [`renders_as_blocks`]' text boxes.
fn cell_markdown(tc: XmlNode, ctx: &Ctx) -> String {
    if renders_as_blocks(tc) {
        rich_cell_markdown(tc, ctx)
    } else {
        // No trim: the cell value is kept verbatim (docling uses `cell.text`);
        // the table serializer strips data cells but keeps the header as-is.
        tc.children()
            .filter(|n| n.has_tag_name("p"))
            .map(plain_paragraph_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Whether the flat outputs (Markdown, LaTeX, DocLang) render a cell from its
/// block content: docling's rich cells, and a cell holding a text box (#532).
/// A text box is not one of `_is_rich_table_cell`'s triggers, so docling
/// takes such a cell as plain — python-docx's `cell.text`, which skips the
/// box — and the box's paragraphs land *before the table* as a `textbox`
/// section: the cell prints empty and the text appears out of place (with a
/// second text box also inside the cell). Here the box's text stays in the
/// cell it is drawn in. The JSON (and the HTML rendered from it) keeps
/// docling's item tree — the text hoisted before the table — since that tree
/// mirrors upstream's structure call for call (`docx_tree.rs`), which is why
/// this is not folded into [`is_rich_cell`].
fn renders_as_blocks(tc: XmlNode) -> bool {
    is_rich_cell(tc) || tc.descendants().any(|n| n.has_tag_name("txbxContent"))
}

/// Whether a cell must be parsed as rich content rather than plain text
/// (docling's `_is_rich_table_cell`).
fn is_rich_cell(tc: XmlNode) -> bool {
    let paras: Vec<XmlNode> = child_elements(tc).filter(|c| c.has_tag_name("p")).collect();
    if paras.len() > 1 {
        return true;
    }
    // Any non-paragraph block (e.g. a nested table) makes the cell rich.
    if child_elements(tc).any(|c| !matches!(c.tag_name().name(), "p" | "tcPr")) {
        return true;
    }
    if tc.descendants().any(|n| n.has_tag_name("blip")) {
        return true;
    }
    paras
        .iter()
        .flat_map(|p| child_elements(*p).filter(|c| c.has_tag_name("r")))
        .any(run_has_format)
}

fn run_has_format(r: XmlNode) -> bool {
    let Some(rpr) = child_elements(r).find(|c| c.has_tag_name("rPr")) else {
        return false;
    };
    child_elements(rpr).any(|c| {
        matches!(
            c.tag_name().name(),
            "b" | "i" | "strike" | "u" | "vertAlign"
        ) && attr(c, "val") != Some("false")
            && attr(c, "val") != Some("0")
            && attr(c, "val") != Some("none")
    })
}

/// Render a rich cell's block content to Markdown (a nested table is flattened
/// to its space-joined cell text, as docling's nested-in-table serialization).
fn rich_cell_markdown(tc: XmlNode, ctx: &Ctx) -> String {
    let mut sub = DoclingDocument::new("");
    let mut state = ListState::default();
    for child in child_elements(tc) {
        cell_block(child, ctx, &mut state, &mut sub, true);
    }
    drop_resumed_list_spacers(&mut sub.nodes);
    // In-cell rendering: a heading inside the cell is plain text
    // (docling-core#540).
    sub.export_to_table_cell_markdown().trim().to_string()
}

/// One block of a rich cell: a paragraph, a nested table — flattened to its
/// space-joined cell text for Markdown (`flatten`, docling's
/// nested-in-table serialization), kept whole for DocLang — or a block
/// content control, whose `w:sdtContent` is walked like the cell itself, as
/// docling's `_walk_linear` does (#532: a cell holding only an `sdt` printed
/// empty).
fn cell_block(
    child: XmlNode,
    ctx: &Ctx,
    state: &mut ListState,
    sub: &mut DoclingDocument,
    flatten: bool,
) {
    match child.tag_name().name() {
        "p" => handle_paragraph_inner(child, ctx, state, sub, true, false),
        "tbl" => {
            let Some(table) = parse_table_with(child, ctx, flatten) else {
                return;
            };
            if !flatten {
                sub.push(Node::Table(table));
                return;
            }
            let text = table
                .rows
                .iter()
                .flatten()
                .filter(|c| !c.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                sub.push(Node::Paragraph { text });
            }
        }
        "sdt" => {
            if let Some(content) = child.children().find(|n| n.has_tag_name("sdtContent")) {
                for c in child_elements(content) {
                    cell_block(c, ctx, state, sub, flatten);
                }
            }
        }
        _ => {}
    }
}

/// docling's list reuse after blank spacers (docling#4305), which deletes the
/// blanks once the walk ends (docling#4674, #764): the empty paragraphs —
/// commented ones included — between a list item and the item that resumes
/// its list are dropped, so the list renders as one.
fn drop_resumed_list_spacers(nodes: &mut Vec<Node>) {
    fn bare(node: &Node) -> &Node {
        match node {
            Node::Commented { inner, .. } => bare(inner),
            other => other,
        }
    }
    let empty = |n: &Node| matches!(bare(n), Node::Paragraph { text } if text.trim().is_empty());
    let mut i = 0;
    while i < nodes.len() {
        if matches!(
            bare(&nodes[i]),
            Node::ListItem {
                first_in_list: false,
                ..
            }
        ) {
            let mut j = i;
            while j > 0 && empty(&nodes[j - 1]) {
                j -= 1;
            }
            if j < i && j > 0 && matches!(bare(&nodes[j - 1]), Node::ListItem { .. }) {
                nodes.drain(j..i);
                i = j;
            }
        }
        i += 1;
    }
}

/// A rich cell's DocLang block content — the structured counterpart of
/// [`rich_cell_markdown`]: the cell's paragraphs/lists plus *full* nested tables
/// (kept as `Node::Table`, not flattened), built by walking the cell like a
/// document body. Empty for a plain cell, whose flat text the serializer uses.
/// Markdown/JSON never consult this (they render the flat cell text).
fn cell_blocks_of(tc: XmlNode, ctx: &Ctx) -> Vec<Node> {
    if !renders_as_blocks(tc) {
        return Vec::new();
    }
    let mut sub = DoclingDocument::new("");
    let mut state = ListState::default();
    for child in child_elements(tc) {
        cell_block(child, ctx, &mut state, &mut sub, false);
    }
    drop_resumed_list_spacers(&mut sub.nodes);
    sub.nodes
}

/// `<m:oMath>` elements introduced by a child (the child itself, or those inside
/// an `<m:oMathPara>` wrapper).
fn omaths_of<'a, 'i>(child: XmlNode<'a, 'i>) -> Vec<XmlNode<'a, 'i>> {
    if child.has_tag_name("oMath") {
        vec![child]
    } else if child.has_tag_name("oMathPara") {
        child
            .descendants()
            .filter(|d| d.has_tag_name("oMath"))
            .collect()
    } else {
        vec![]
    }
}

/// A paragraph's plain run text and equations (`$…$`) in document order, no
/// formatting markers — python-docx's `Paragraph.text`, which is where
/// docling's code text (`raw_paragraph_text`) and plain-cell text come from.
fn plain_paragraph_text(p: XmlNode) -> String {
    let mut out = String::new();
    for child in child_elements(p) {
        let omaths = omaths_of(child);
        if omaths.is_empty() {
            push_inline_text(child, &mut out);
        } else {
            for m in omaths {
                let eq = crate::backend::omml::to_latex(m);
                if !eq.is_empty() {
                    out.push('$');
                    out.push_str(&eq);
                    out.push('$');
                }
            }
        }
    }
    out
}

/// Append the plain text of one paragraph child the way python-docx's
/// `CT_P.text` / docling's `_iter_paragraph_content` read it: a run's inner
/// content through [`run_child_text`] (so a `<w:br/>` is a newline and a tab a
/// tab — flattening the subtree to `w:t` alone glued the lines of a code
/// paragraph together, #409), a hyperlink's runs likewise, a content control's
/// `w:t` text only (docling's `.//w:sdtContent//w:t` xpath), and the
/// transparent wrappers recursed into. Anything else contributes nothing.
fn push_inline_text(node: XmlNode, out: &mut String) {
    match node.tag_name().name() {
        "r" => out.extend(child_elements(node).map(run_child_text)),
        // A link's runs, through the same wrappers as a paragraph's
        // (docling#4576: a tracked insertion inside the link text).
        "hyperlink" => {
            for c in child_elements(node).filter(|c| !c.has_tag_name("hyperlink")) {
                push_inline_text(c, out);
            }
        }
        "sdt" => {
            for t in node.descendants().filter(|n| n.has_tag_name("t")) {
                out.push_str(&t_text(t));
            }
        }
        "smartTag" | "customXml" | "ins" | "moveTo" | "fldSimple" => {
            for c in child_elements(node) {
                push_inline_text(c, out);
            }
        }
        _ => {}
    }
}

/// Case-folded paragraph *style names* that mark a paragraph as code (docling PR
/// #3735). Matched exactly — `"Listing"` and `"Source Reference"` must not match.
const CODE_STYLE_NAMES: &[&str] = &[
    "source code",
    "code",
    "code block",
    "code listing",
    "html preformatted",
    "preformatted text",
    "preformatted",
    "verbatim",
];

/// Case-folded paragraph *style IDs* that mark a paragraph as code (the XML
/// `w:styleId`, distinct from the human-readable name).
const CODE_STYLE_IDS: &[&str] = &[
    "sourcecode",
    "code",
    "codeblock",
    "codelisting",
    "htmlpreformatted",
    "preformattedtext",
    "preformatted",
    "verbatim",
];

/// Case-folded font families treated as monospaced for the font-fallback signal.
const MONOSPACE_FONTS: &[&str] = &[
    "consolas",
    "courier",
    "courier new",
    "lucida console",
    "menlo",
    "monaco",
    "dejavu sans mono",
    "andale mono",
    "liberation mono",
    "sf mono",
];

/// ASCII punctuation that distinguishes code from monospaced prose (docling's
/// `_CODE_INDICATIVE_CHARS`). Parentheses/brackets/lone semicolons are excluded.
const CODE_INDICATIVE_CHARS: &[char] = &['{', '}', ';', '=', '<', '>'];

/// Defensive cap against a malformed/cyclic `basedOn` chain.
const MAX_STYLE_DEPTH: usize = 10;

/// Whether a style marks its paragraphs as code: the style itself or any
/// ancestor in its `basedOn` chain carries a code style name/id (docling's
/// `_is_code_style`).
pub(super) fn is_code_style(style_id: &str, ctx: &Ctx) -> bool {
    let mut sid = style_id;
    for _ in 0..MAX_STYLE_DEPTH {
        if sid.is_empty() {
            break;
        }
        let name = ctx.style_names.get(sid).map(|s| s.to_ascii_lowercase());
        let id_l = sid.to_ascii_lowercase();
        if name
            .as_deref()
            .is_some_and(|n| CODE_STYLE_NAMES.contains(&n))
            || CODE_STYLE_IDS.contains(&id_l.as_str())
        {
            return true;
        }
        match ctx.style_based.get(sid) {
            Some(parent) => sid = parent,
            None => break,
        }
    }
    false
}

/// Whether the style reads as a *title* style — the substring check docling's
/// `_is_title_style` applies to the style id, the (possibly localized) display
/// name, and their `basedOn` counterparts (docling#3961, #270). A title-ish
/// style ("Title", "Subtitle") is never promoted to a heading by its
/// `w:outlineLvl`; it keeps reaching its own branch.
pub(super) fn is_title_style(style_id: &str, style_name: &str, ctx: &Ctx) -> bool {
    let has_title = |s: &str| s.to_ascii_lowercase().contains("title");
    if has_title(style_id) || has_title(style_name) {
        return true;
    }
    ctx.style_based.get(style_id).is_some_and(|base| {
        has_title(base) || ctx.style_names.get(base).is_some_and(|n| has_title(n))
    })
}

/// The lowercased font a style resolves to, walking the `basedOn` chain (used
/// when a run inherits its typeface). Empty when none applies.
fn style_font(style_id: &str, ctx: &Ctx) -> String {
    let mut sid = style_id;
    for _ in 0..MAX_STYLE_DEPTH {
        if sid.is_empty() {
            break;
        }
        if let Some(f) = ctx.style_fonts.get(sid) {
            return f.clone();
        }
        match ctx.style_based.get(sid) {
            Some(parent) => sid = parent,
            None => break,
        }
    }
    String::new()
}

/// docling's `_CODE_CALL_PATTERN`: a call whose arguments look like code
/// (`print(sys.argv)`, an empty `set()`), not a `party(ies)` shape —
/// `[A-Za-z_]\((?:\s*\)|[^)\d,._='"]{0,200}[\d,._='"][^)]{0,200}\))`
/// since 2.142, its argument runs bounded at 200 characters (docling#4973:
/// a linear-time guard for Python's backtracking engine). The `regex` crate
/// is linear without it, but its counted repetitions made that pattern
/// some 300× slower than the unbounded one on a long paragraph, so the
/// unbounded pattern — which every bounded match also matches — screens
/// first and the bounds are checked by a scan from each call opening.
pub(super) fn is_code_call(text: &str) -> bool {
    const RUN: usize = 200;
    let codeish = |c: char| matches!(c, ',' | '.' | '_' | '=' | '\'' | '"') || c.is_numeric();
    if !cached_regex!(r#"[A-Za-z_]\((?:\s*\)|[^)]*[\d,._='"][^)]*\))"#).is_match(text) {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    let bounded_call = |open: usize| {
        let args = &chars[open + 1..];
        // `\s*\)` — an empty call.
        if args.iter().find(|c| !c.is_whitespace()) == Some(&')') {
            return true;
        }
        // The first run stops at the first code-like character…
        let Some(first) = args
            .iter()
            .take(RUN + 1)
            .position(|&c| c == ')' || codeish(c))
        else {
            return false;
        };
        if args[first] == ')' {
            return false;
        }
        // …and the second runs to the closing parenthesis.
        args[first + 1..].iter().take(RUN + 1).any(|&c| c == ')')
    };
    (1..chars.len()).any(|i| {
        chars[i] == '('
            && (chars[i - 1].is_ascii_alphabetic() || chars[i - 1] == '_')
            && bounded_call(i)
    })
}

/// Whether a paragraph reads as code set in a monospaced font — the
/// lower-precision fallback used when the style name doesn't already mark it as
/// code (docling's `_is_code_by_font`): (nearly) every character must resolve to
/// a monospaced font and the text must carry a code signal, or be an indented
/// line continuing a code block.
pub(super) fn is_code_by_font(p: XmlNode, style_id: &str, ctx: &Ctx, prev_is_code: bool) -> bool {
    // A caption/figure/table/label style is never code.
    let style_lc = ctx
        .style_names
        .get(style_id)
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if ["caption", "figure", "table", "label"]
        .iter()
        .any(|kw| style_lc.contains(kw))
    {
        return false;
    }
    let raw = plain_paragraph_text(p);
    let stripped = raw.trim();
    if stripped.is_empty() || cached_regex!(r"(?i)^(figure|table|listing)\s+\d").is_match(stripped)
    {
        return false;
    }
    // A code signal: an indicative char other than a lone `;`, a call/def shape,
    // or an indented continuation of a preceding code block.
    let strong: Vec<char> = stripped
        .chars()
        .filter(|c| CODE_INDICATIVE_CHARS.contains(c))
        .collect();
    let has_strong = strong.iter().any(|&c| c != ';');
    let call = is_code_call(stripped);
    let def = cached_regex!(
        r"(?m)^[ \t]*(?:async\s+)?(?:def|class|if|elif|while|for|with|except|finally|try|catch|switch|function|func|fn|sub|proc)\s+\S[^\n]*:[ \t]*$"
    )
    .is_match(stripped);
    let has_code_char = has_strong || call || def || detect_code_language(stripped).is_some();
    let is_continuation = prev_is_code && raw.chars().next().is_some_and(|c| c.is_whitespace());
    if !has_code_char && !is_continuation {
        return false;
    }
    // Never reclassify a list item.
    if num_pr(p).is_some() {
        return false;
    }
    // (Nearly) every character must be monospaced.
    let sf = style_font(style_id, ctx);
    let (mono, total) = monospaced_char_counts(p, &sf);
    if total == 0 || (mono as f32) / (total as f32) < 0.9 {
        return false;
    }
    // A code-styled table cell is walked as rich content, not here.
    !p.ancestors().any(|n| n.has_tag_name("tc"))
}

/// Count monospaced vs total non-space characters across a paragraph's runs
/// (including runs nested in hyperlinks/insertions), resolving an inherited
/// typeface to `style_font` (docling's `_monospaced_char_counts`).
fn monospaced_char_counts(p: XmlNode, style_font: &str) -> (usize, usize) {
    let (mut mono, mut total) = (0usize, 0usize);
    for r in p.descendants().filter(|n| n.has_tag_name("r")) {
        let text: String = r.descendants().filter_map(flat_text).collect();
        let len = text.trim().chars().count();
        if len == 0 {
            continue;
        }
        total += len;
        let font = r
            .descendants()
            .find(|n| n.has_tag_name("rFonts"))
            .and_then(|n| attr(n, "ascii"))
            .map(|f| f.to_ascii_lowercase())
            .unwrap_or_else(|| style_font.to_string());
        if MONOSPACE_FONTS.contains(&font.as_str()) {
            mono += len;
        }
    }
    (mono, total)
}

/// docling's `detect_code_language(text)` with no hint.
pub(super) fn detect_code_language(text: &str) -> Option<String> {
    super::code_language::detect_code_language(text, None)
}

/// From `styles.xml`: `styleId` → display name / list numbering `(numId, ilvl)` /
/// basedOn parent styleId / lowercased ascii font — everything the block walk and
/// the code-block detector (docling PR #3735) read about a paragraph style.
struct StyleMaps {
    names: HashMap<String, String>,
    nums: HashMap<String, (Option<String>, Option<i64>)>,
    based: HashMap<String, String>,
    fonts: HashMap<String, String>,
    /// styleId → the style's own `w:pPr/w:outlineLvl` as a 1-indexed docling
    /// heading level (OOXML stores 0–8 for levels 1–9; 9 is the "body text"
    /// sentinel and maps to 10 here — filtered at the use site). The style's
    /// *own* definition only, no `basedOn` inheritance — mirroring docling's
    /// `_get_outline_level_from_style` (docling#3961, #270).
    outlines: HashMap<String, u8>,
    /// styleId → the style's own `w:rPr/w:b` (python-docx `font.bold`:
    /// `true`/`false` when the element is present). See [`Ctx::style_bold`].
    bolds: HashMap<String, bool>,
    /// The styles a paragraph cannot take (#865): every `w:type` but
    /// `paragraph` — see [`paragraph_style_id`].
    not_paragraph: HashSet<String>,
    /// See [`Ctx::style_left_ind`].
    left_inds: HashMap<String, Option<i64>>,
}
fn parse_styles(styles_xml: &str) -> StyleMaps {
    let mut names = HashMap::new();
    let mut nums = HashMap::new();
    let mut based = HashMap::new();
    let mut fonts = HashMap::new();
    let mut outlines = HashMap::new();
    let mut bolds = HashMap::new();
    let mut not_paragraph = HashSet::new();
    let mut own_lefts: HashMap<String, Option<i64>> = HashMap::new();
    let mut default_paragraph: Option<String> = None;
    let mut numbering_styles: HashSet<String> = HashSet::new();
    let Ok(dom) = Document::parse(styles_xml) else {
        return StyleMaps {
            names,
            nums,
            based,
            fonts,
            outlines,
            bolds,
            not_paragraph,
            left_inds: HashMap::new(),
        };
    };
    for style in dom.descendants().filter(|n| n.has_tag_name("style")) {
        let Some(id) = attr(style, "styleId") else {
            continue;
        };
        // `ST_StyleType`'s absent value is `paragraph`. Some generators
        // write the invalid `list` — docling#4831 (2.141) reads it as
        // `numbering`, the type it means — which is not a paragraph type
        // either. The style's other properties are still read: a numbering
        // style's `numPr` is what a `numStyleLink` resolves through.
        if attr(style, "type").is_some_and(|t| t != "paragraph") {
            not_paragraph.insert(id.to_string());
        }
        if let Some(name) = style
            .children()
            .find(|n| n.has_tag_name("name"))
            .and_then(|n| attr(n, "val"))
        {
            names.insert(id.to_string(), name.to_string());
        }
        if let Some(parts) = num_pr_parts(style) {
            nums.insert(id.to_string(), parts);
        }
        // basedOn chain and the style's own font — used by the code-block
        // detector (docling PR #3735) to walk a style's inheritance.
        if let Some(b) = style
            .children()
            .find(|n| n.has_tag_name("basedOn"))
            .and_then(|n| attr(n, "val"))
        {
            based.insert(id.to_string(), b.to_string());
        }
        if let Some(font) = style
            .descendants()
            .find(|n| n.has_tag_name("rFonts"))
            .and_then(|n| attr(n, "ascii"))
        {
            fonts.insert(id.to_string(), font.to_ascii_lowercase());
        }
        if let Some(lvl) = style
            .descendants()
            .find(|n| n.has_tag_name("outlineLvl"))
            .and_then(|n| attr(n, "val"))
            .and_then(|v| v.parse::<u8>().ok())
        {
            outlines.insert(id.to_string(), lvl.saturating_add(1));
        }
        if let Some(b) = style
            .children()
            .find(|n| n.has_tag_name("rPr"))
            .and_then(|pr| pr.children().find(|n| n.has_tag_name("b")))
        {
            bolds.insert(id.to_string(), on_off(attr(b, "val")));
        }
        own_lefts.insert(id.to_string(), own_left_indent(style));
        if attr(style, "type") == Some("paragraph")
            && attr(style, "default").is_some_and(|v| on_off(Some(v)))
        {
            // python-docx's `default_for`: the last default in document order.
            default_paragraph = Some(id.to_string());
        }
        if attr(style, "type").is_some_and(|t| t == "numbering" || t == "list") {
            numbering_styles.insert(id.to_string());
        }
    }
    // `_get_list_left_indent`'s style step: the first `w:ind` on the style's
    // `basedOn` chain (`_iter_style_chain` — stopping at a numbering style,
    // which has no `base_style`, at a missing one, and after
    // `MAX_STYLE_INHERITANCE_DEPTH` hops).
    let chain_left = |start: &str| -> Option<i64> {
        let mut id = start;
        for _ in 0..MAX_STYLE_INHERITANCE_DEPTH {
            if numbering_styles.contains(id) {
                return None;
            }
            if let Some(left) = *own_lefts.get(id)? {
                return Some(left);
            }
            id = based.get(id)?;
        }
        None
    };
    let mut left_inds: HashMap<String, Option<i64>> = own_lefts
        .keys()
        .map(|id| (id.clone(), chain_left(id)))
        .collect();
    left_inds.insert(
        String::new(),
        default_paragraph.as_deref().and_then(chain_left),
    );
    StyleMaps {
        names,
        nums,
        based,
        fonts,
        outlines,
        bolds,
        not_paragraph,
        left_inds,
    }
}

/// An element's own `w:pPr/w:ind` left indentation in twips — docling's
/// `_get_list_left_indent._left`: `w:left`, else its direction-neutral
/// spelling `w:start`; `None` when neither is set.
fn own_left_indent(el: XmlNode) -> Option<i64> {
    let ind = el
        .children()
        .find(|n| n.has_tag_name("pPr"))?
        .children()
        .find(|n| n.has_tag_name("ind"))?;
    ["left", "start"]
        .iter()
        .find_map(|a| attr(ind, a).and_then(|v| v.trim().parse::<i64>().ok()))
}

/// A list paragraph's effective left indentation in twips — docling's
/// `_get_list_left_indent` (docling#4465): the paragraph's own `w:ind`,
/// else its numbering level's, else its style's `basedOn` chain (an
/// unstyled paragraph takes the default paragraph style's); 0 when none
/// sets one. Both walks compare it to nest Word's multi-level list styles
/// ("List Bullet 2", "List Number 2", …), which each own a `numId` at
/// `w:ilvl` 0 and differ from "List Bullet" only by their indentation.
pub(super) fn list_left_indent(p: XmlNode, num_id: &str, ilvl: i64, ctx: &Ctx) -> i64 {
    own_left_indent(p)
        .or_else(|| {
            ctx.num_levels
                .get(&(num_id.to_string(), ilvl))
                .and_then(|l| l.left)
        })
        .or_else(|| {
            let id = paragraph_style_id(p, ctx);
            ctx.style_left_ind
                .get(id)
                .or_else(|| ctx.style_left_ind.get(""))
                .copied()
                .flatten()
        })
        .unwrap_or(0)
}

/// A paragraph's style id (`w:pStyle`), empty when unstyled — and empty
/// when the id names a style that is not a paragraph style (a numbering
/// style, a `w:type="list"` one, a character or table style): python-docx's
/// `paragraph.style` resolves such a reference to the default paragraph
/// style, so docling reads the paragraph as unstyled (#865, docling#4831).
pub(super) fn paragraph_style_id<'a>(p: XmlNode<'a, '_>, ctx: &Ctx) -> &'a str {
    let id = p
        .children()
        .find(|n| n.has_tag_name("pPr"))
        .and_then(|pr| pr.children().find(|n| n.has_tag_name("pStyle")))
        .and_then(|s| attr(s, "val"))
        .unwrap_or("");
    if ctx.not_paragraph_styles.contains(id) {
        ""
    } else {
        id
    }
}

/// python-docx's `ST_OnOff`: an absent `w:val` is on; `0`/`false`/`off` is off.
pub(super) fn on_off(val: Option<&str>) -> bool {
    !matches!(val, Some("0" | "false" | "off"))
}

/// One numbering level's properties (resolved `num` → `abstractNum` → `lvl`).
#[derive(Clone, Default)]
pub(super) struct NumLevel {
    pub(super) numbered: bool,
    /// Whether the level's `numFmt` renders a visible marker (docling's
    /// `_VISIBLE_NUMBERING_FORMATS`, docling#3760): `decimal`, roman, letter,
    /// `decimalZero`. `none` (and the exotic formats) is invisible — a heading
    /// on such a level carries a `numPr` for outline structure only, so it
    /// gets no computed `1.2` prefix.
    pub(super) visible: bool,
    /// The level's raw `numFmt` (`decimal`, `lowerLetter`, `upperRoman`, …);
    /// `None` when the level declares none.
    num_fmt: Option<String>,
    start: i64,
    lvl_text: String,
    /// The level's own `w:pPr/w:ind` left indentation in twips (docling#4465).
    pub(super) left: Option<i64>,
}

/// OOXML `numFmt` values that produce a visible list/heading marker
/// (docling's `_VISIBLE_NUMBERING_FORMATS`).
const VISIBLE_NUMBERING_FORMATS: &[&str] = &[
    "decimal",
    "lowerRoman",
    "upperRoman",
    "lowerLetter",
    "upperLetter",
    "decimalZero",
    // East Asian formats (docling#4336): rendered by `cjk_counter`.
    "chineseCounting",
    "chineseCountingThousand",
    "chineseLegalSimplified",
    "ideographDigital",
    "ideographTraditional",
    "ideographZodiac",
    "japaneseCounting",
    "decimalFullWidth",
    "decimalEnclosedCircle",
];

// East Asian numFmt rendering (docling#4336). Character sets follow
// ECMA-376-1:2016 §17.18.59 (ST_NumberFormat); where the standard's prose and
// its examples disagree, or are silent, the rules follow the markers Microsoft
// Word 16.112 renders, as noted on each helper.
const CJK_DIGITS: [char; 9] = ['一', '二', '三', '四', '五', '六', '七', '八', '九'];
const CHINESE_LEGAL_DIGITS: [char; 9] = ['壹', '贰', '叁', '肆', '伍', '陆', '柒', '捌', '玖'];
const HEAVENLY_STEMS: [char; 10] = ['甲', '乙', '丙', '丁', '戊', '己', '庚', '辛', '壬', '癸'];
const EARTHLY_BRANCHES: [char; 12] = [
    '子', '丑', '寅', '卯', '辰', '巳', '午', '未', '申', '酉', '戌', '亥',
];
/// Word renders an empty marker from this value on ([MS-OI29500] 2.1.548 j).
const CJK_GROUPED_NUMBER_LIMIT: i64 = 1_000_000;

/// Each decimal digit of `value` written with `zero` for 0 and `digits[d-1]`
/// otherwise (`_int_to_positional_marker`).
fn positional_marker(value: i64, zero: char, digits: &[char; 9]) -> String {
    if value < 0 {
        return value.to_string();
    }
    value
        .to_string()
        .bytes()
        .map(|b| match b - b'0' {
            0 => zero,
            d => digits[usize::from(d) - 1],
        })
        .collect()
}

/// 1..=len(symbols) map to one symbol each; other values stay decimal
/// (`_int_to_sequence_marker` — ECMA-376's fallback, which Word renders too).
fn sequence_marker(value: i64, symbols: &[char]) -> String {
    match usize::try_from(value) {
        Ok(v) if (1..=symbols.len()).contains(&v) => symbols[v - 1].to_string(),
        _ => value.to_string(),
    }
}

/// 1..999,999 with Chinese unit characters (`_int_to_chinese_grouped_marker`):
/// every non-zero digit is followed by its unit, the ten-thousands group is
/// closed by `myriad`, and one `zero` is written for each run of zero digits
/// that is followed by a non-zero digit: 101 → 一百〇一, 100010 → 一十万〇一十.
/// This matches Word ([MS-OI29500] 2.1.548 e says Word omits the zero for
/// 10,000–100,000; it does not).
fn chinese_grouped_marker(
    value: i64,
    digits: &[char; 9],
    units: [&str; 4],
    myriad: &str,
    zero: char,
) -> String {
    let text = value.to_string();
    let mut parts = String::new();
    let mut pending_zero = false;
    for (power, b) in (0..text.len()).rev().zip(text.bytes()) {
        let digit = usize::from(b - b'0');
        if digit == 0 {
            pending_zero = !parts.is_empty();
        } else {
            if pending_zero {
                parts.push(zero);
                pending_zero = false;
            }
            parts.push(digits[digit - 1]);
            parts.push_str(units[power % 4]);
        }
        if power == 4 {
            parts.push_str(myriad);
        }
    }
    parts
}

/// `chineseCounting`: 十 and 二十一 up to 99, then digit by digit with U+25CB as
/// zero (ECMA-376's pattern, which Word renders too).
fn chinese_counting_marker(value: i64) -> String {
    if value < 0 {
        return value.to_string();
    }
    if value >= 100 || value == 0 {
        return positional_marker(value, '\u{25cb}', &CJK_DIGITS);
    }
    let (tens, ones) = (value / 10, value % 10);
    let mut text = String::new();
    if tens > 0 {
        if tens > 1 {
            text.push(CJK_DIGITS[tens as usize - 1]);
        }
        text.push('十');
    }
    if ones > 0 {
        text.push(CJK_DIGITS[ones as usize - 1]);
    }
    text
}

/// `chineseCountingThousand` the way Word does: 10–19 are 十…十九, every other
/// ten keeps its digit (110 → 一百一十, 100000 → 一十万), U+3007 is the zero
/// (101 → 一百〇一); from 1,000,000 the marker is empty.
fn chinese_counting_thousand_marker(value: i64) -> String {
    match value {
        v if v < 0 => v.to_string(),
        0 => "\u{3007}".to_string(),
        v if v >= CJK_GROUPED_NUMBER_LIMIT => String::new(),
        10 => "十".to_string(),
        11..=19 => format!("十{}", CJK_DIGITS[value as usize - 11]),
        v => chinese_grouped_marker(v, &CJK_DIGITS, ["", "十", "百", "千"], "万", '\u{3007}'),
    }
}

/// `chineseLegalSimplified` (壹, 贰, …, 壹拾, 壹佰零壹): ECMA-376's digits and
/// units, 10 keeps its leading 壹, ten thousand is U+842C as Word renders it;
/// from 1,000,000 the marker is empty.
fn chinese_legal_marker(value: i64) -> String {
    match value {
        v if v < 0 => v.to_string(),
        0 => "零".to_string(),
        v if v >= CJK_GROUPED_NUMBER_LIMIT => String::new(),
        v => chinese_grouped_marker(v, &CHINESE_LEGAL_DIGITS, ["", "拾", "佰", "仟"], "萬", '零'),
    }
}

/// 1..9999 with 千, 百, 十; a digit 1 is omitted before its unit (before 千
/// only when `explicit_one_thousand` is off).
fn japanese_counting_group(mut value: i64, explicit_one_thousand: bool) -> String {
    let mut parts = String::new();
    for (unit_value, unit) in [(1000, '千'), (100, '百'), (10, '十')] {
        let digit = value / unit_value;
        value %= unit_value;
        if digit == 0 {
            continue;
        }
        if digit > 1 || (unit == '千' && explicit_one_thousand) {
            parts.push(CJK_DIGITS[digit as usize - 1]);
        }
        parts.push(unit);
    }
    if value > 0 {
        parts.push(CJK_DIGITS[value as usize - 1]);
    }
    parts
}

/// `japaneseCounting` (十, 百一, 千百, 一万一千, 十万): no zero character inside
/// a number, 百 and 十 without a leading 一, 千 without 一 below 10,000 and
/// 一千 after a 万 group — Word's rendering; from 1,000,000 the marker is empty.
fn japanese_counting_marker(value: i64) -> String {
    if value < 0 {
        return value.to_string();
    }
    if value == 0 {
        return "\u{3007}".to_string();
    }
    if value >= CJK_GROUPED_NUMBER_LIMIT {
        return String::new();
    }
    let (myriads, rest) = (value / 10_000, value % 10_000);
    let mut text = String::new();
    if myriads > 0 {
        if myriads == 1 {
            text.push(CJK_DIGITS[0]);
        } else {
            text.push_str(&japanese_counting_group(myriads, false));
        }
        text.push('万');
    }
    if rest > 0 {
        text.push_str(&japanese_counting_group(rest, myriads > 0));
    }
    text
}

/// docling's `_CJK_ENUM_FORMATTERS`: the marker for an East Asian `numFmt`,
/// `None` for any other format.
fn cjk_counter(counter: i64, num_fmt: &str) -> Option<String> {
    Some(match num_fmt {
        "chineseCounting" => chinese_counting_marker(counter),
        "chineseCountingThousand" => chinese_counting_thousand_marker(counter),
        "chineseLegalSimplified" => chinese_legal_marker(counter),
        "ideographDigital" => positional_marker(counter, '\u{3007}', &CJK_DIGITS),
        "ideographTraditional" => sequence_marker(counter, &HEAVENLY_STEMS),
        // ECMA-376 lists U+620C (戌), the eleventh Earthly Branch; Word 16.112
        // renders the adjacent U+620D (戍), which breaks the 子丑寅卯 sequence,
        // so the standard is kept here on purpose (as upstream does).
        "ideographZodiac" => sequence_marker(counter, &EARTHLY_BRANCHES),
        "japaneseCounting" => japanese_counting_marker(counter),
        "decimalFullWidth" => {
            if counter < 0 {
                counter.to_string()
            } else {
                counter
                    .to_string()
                    .bytes()
                    .map(|b| char::from_u32(0xFF10 + u32::from(b - b'0')).unwrap_or('?'))
                    .collect()
            }
        }
        "decimalEnclosedCircle" => {
            let circles: Vec<char> = (0x2460..0x2474).filter_map(char::from_u32).collect();
            sequence_marker(counter, &circles)
        }
        _ => return None,
    })
}

/// Map `(numId, ilvl)` → its level properties, resolved through `numbering.xml`'s
/// `num` → `abstractNum` → level (`numFmt`, `start`, `lvlText`).
///
/// An `abstractNum` without the level that carries a `w:numStyleLink` borrows
/// it from the numbering style it names: that style's `numId` → its `num` →
/// its `abstractNum`, followed up to [`MAX_STYLE_INHERITANCE_DEPTH`] hops and
/// never through the same `abstractNum` twice (docling's `_get_level_element`,
/// docling#4727, #765). `style_nums` is the styles' own `numPr`.
fn parse_numbering(
    numbering_xml: &str,
    style_nums: &HashMap<String, (Option<String>, Option<i64>)>,
) -> HashMap<(String, i64), NumLevel> {
    let mut out = HashMap::new();
    let Ok(dom) = Document::parse(numbering_xml) else {
        return out;
    };
    // numId -> abstractNumId
    let mut num_to_abstract: HashMap<String, String> = HashMap::new();
    for num in dom.descendants().filter(|n| n.has_tag_name("num")) {
        if let (Some(id), Some(abs)) = (
            attr(num, "numId"),
            num.descendants()
                .find(|n| n.has_tag_name("abstractNumId"))
                .and_then(|n| attr(n, "val")),
        ) {
            num_to_abstract.insert(id.to_string(), abs.to_string());
        }
    }
    // (numId, ilvl) -> the instance's `w:lvlOverride` (docling#5123, #1017):
    // its `w:lvl` replaces the abstract level, its `w:startOverride` sets the
    // start — ECMA-376 Part 1, 17.9.9 / 17.9.27. docling applies an override
    // only in 0..=32767 (`_MAX_START_OVERRIDE`, also PowerPoint's largest
    // `startAt`: letter and roman markers grow with the number); anything
    // else keeps the level's own `w:start`.
    let mut overrides: HashMap<(String, i64), (Option<NumLevel>, Option<i64>)> = HashMap::new();
    for num in dom.descendants().filter(|n| n.has_tag_name("num")) {
        let Some(id) = attr(num, "numId") else {
            continue;
        };
        for ov in num.children().filter(|n| n.has_tag_name("lvlOverride")) {
            let Some(ilvl) = attr(ov, "ilvl").and_then(|v| v.parse::<i64>().ok()) else {
                continue;
            };
            let level = ov
                .children()
                .find(|n| n.has_tag_name("lvl"))
                .map(|l| parse_num_level(l).1);
            let start = ov
                .children()
                .find(|n| n.has_tag_name("startOverride"))
                .and_then(|n| attr(n, "val"))
                .and_then(|v| v.trim().parse::<i64>().ok())
                .filter(|v| (0..=MAX_START_OVERRIDE).contains(v));
            // The first `w:lvlOverride` for a level wins, like docling's
            // `find`.
            overrides
                .entry((id.to_string(), ilvl))
                .or_insert((level, start));
        }
    }
    // abstractNumId -> { ilvl -> NumLevel }
    let mut abstract_levels: HashMap<String, HashMap<i64, NumLevel>> = HashMap::new();
    // abstractNumId -> the numbering style its `numStyleLink` names
    let mut style_links: HashMap<String, String> = HashMap::new();
    for abs in dom.descendants().filter(|n| n.has_tag_name("abstractNum")) {
        let Some(abs_id) = attr(abs, "abstractNumId") else {
            continue;
        };
        if let Some(style) = abs
            .children()
            .find(|n| n.has_tag_name("numStyleLink"))
            .and_then(|n| attr(n, "val"))
            .filter(|v| !v.is_empty())
        {
            style_links.insert(abs_id.to_string(), style.to_string());
        }
        let mut levels = HashMap::new();
        for lvl in abs.children().filter(|n| n.has_tag_name("lvl")) {
            let (ilvl, level) = parse_num_level(lvl);
            levels.insert(ilvl, level);
        }
        abstract_levels.insert(abs_id.to_string(), levels);
    }
    let ilvls: std::collections::BTreeSet<i64> = abstract_levels
        .values()
        .flat_map(|levels| levels.keys().copied())
        .chain(overrides.keys().map(|(_, ilvl)| *ilvl))
        .collect();
    let resolve = |abs_id: &str, ilvl: i64| -> Option<&NumLevel> {
        let mut abs_id = abs_id.to_string();
        let mut visited: HashSet<String> = HashSet::new();
        for _ in 0..MAX_STYLE_INHERITANCE_DEPTH {
            if !visited.insert(abs_id.clone()) {
                return None;
            }
            if let Some(level) = abstract_levels.get(&abs_id)?.get(&ilvl) {
                return Some(level);
            }
            let style = style_links.get(&abs_id)?;
            let linked_num = style_nums.get(style)?.0.as_ref()?;
            abs_id = num_to_abstract.get(linked_num)?.clone();
        }
        None
    };
    for (num_id, abs_id) in &num_to_abstract {
        for &ilvl in &ilvls {
            let key = (num_id.clone(), ilvl);
            let (override_level, start) = match overrides.get(&key) {
                Some((level, start)) => (level.as_ref(), *start),
                None => (None, None),
            };
            if let Some(level) = override_level.or_else(|| resolve(abs_id, ilvl)) {
                let mut level = level.clone();
                if let Some(start) = start {
                    level.start = start;
                }
                out.insert(key, level);
            }
        }
    }
    out
}

/// The largest `w:startOverride` applied — docling's `_MAX_START_OVERRIDE`.
const MAX_START_OVERRIDE: i64 = 32767;

/// One `w:lvl` of an `abstractNum` (or of a `w:lvlOverride`): its `ilvl`
/// and the level.
fn parse_num_level(lvl: XmlNode) -> (i64, NumLevel) {
    let ilvl: i64 = attr(lvl, "ilvl").and_then(|v| v.parse().ok()).unwrap_or(0);
    let num_fmt = lvl
        .children()
        .find(|n| n.has_tag_name("numFmt"))
        .and_then(|n| attr(n, "val"));
    let numbered = num_fmt.map(|v| v != "bullet").unwrap_or(true);
    let visible = num_fmt.is_some_and(|v| VISIBLE_NUMBERING_FORMATS.contains(&v));
    let start = lvl
        .children()
        .find(|n| n.has_tag_name("start"))
        .and_then(|n| attr(n, "val"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let lvl_text = lvl
        .children()
        .find(|n| n.has_tag_name("lvlText"))
        .and_then(|n| attr(n, "val"))
        .unwrap_or("")
        .to_string();
    (
        ilvl,
        NumLevel {
            numbered,
            visible,
            num_fmt: num_fmt.map(str::to_string),
            start,
            lvl_text,
            left: own_left_indent(lvl),
        },
    )
}

/// Cap on the `basedOn` walk when resolving a style's numbering — docling's
/// `_MAX_STYLE_INHERITANCE_DEPTH`.
const MAX_STYLE_INHERITANCE_DEPTH: usize = 10;

/// Whether a `numFmt` is a visible format other than plain `decimal` — the
/// letter / roman / `decimalZero` levels whose `lvlText` suffix (`%2)` → `a)`)
/// must be kept and whose counters cannot be rendered as raw decimals
/// (docling's `_NON_DECIMAL_NUMBERING_FORMATS`, docling#4087).
fn is_non_decimal_format(num_fmt: Option<&str>) -> bool {
    num_fmt.is_some_and(|f| f != "decimal" && VISIBLE_NUMBERING_FORMATS.contains(&f))
}

/// Render a list counter with an OOXML `numFmt` — docling's
/// `_format_enum_counter`: the East Asian formats through [`cjk_counter`]
/// (docling#4336), `lowerLetter`/`upperLetter` run a…z, aa…zz (the letter
/// repeated), roman numerals, `decimalZero` pads to two digits, and
/// everything else (including no format) is the plain decimal.
pub(super) fn format_enum_counter(counter: i64, num_fmt: Option<&str>) -> String {
    if counter >= 0 {
        if let Some(cjk) = num_fmt.and_then(|f| cjk_counter(counter, f)) {
            return cjk;
        }
    }
    let letter = |v: i64| -> String {
        if v <= 0 {
            return v.to_string();
        }
        let c = (b'a' + ((v - 1) % 26) as u8) as char;
        std::iter::repeat_n(c, ((v - 1) / 26 + 1) as usize).collect()
    };
    let roman = |v: i64| -> String {
        if v <= 0 {
            return v.to_string();
        }
        const NUMERALS: [(i64, &str); 13] = [
            (1000, "M"),
            (900, "CM"),
            (500, "D"),
            (400, "CD"),
            (100, "C"),
            (90, "XC"),
            (50, "L"),
            (40, "XL"),
            (10, "X"),
            (9, "IX"),
            (5, "V"),
            (4, "IV"),
            (1, "I"),
        ];
        let mut rest = v;
        let mut out = String::new();
        for (amount, numeral) in NUMERALS {
            while rest >= amount {
                out.push_str(numeral);
                rest -= amount;
            }
        }
        out
    };
    match num_fmt {
        Some("lowerLetter") => letter(counter),
        Some("upperLetter") => letter(counter).to_ascii_uppercase(),
        Some("lowerRoman") => roman(counter).to_ascii_lowercase(),
        Some("upperRoman") => roman(counter),
        Some("decimalZero") => format!("{counter:02}"),
        _ => counter.to_string(),
    }
}

/// The `start` value for `(numId, ilvl)`, defaulting to 1.
pub(super) fn level_start(
    num_levels: &HashMap<(String, i64), NumLevel>,
    num_id: &str,
    ilvl: i64,
) -> i64 {
    num_levels
        .get(&(num_id.to_string(), ilvl))
        .map(|l| l.start)
        .unwrap_or(1)
}

/// Increment the counter for `(numId, ilvl)` (seeding from its `start`) and reset
/// all deeper levels — docling's `_get_list_counter`.
pub(super) fn get_list_counter(
    counters: &mut HashMap<(String, i64), i64>,
    num_levels: &HashMap<(String, i64), NumLevel>,
    num_id: &str,
    ilvl: i64,
) {
    let key = (num_id.to_string(), ilvl);
    let c = counters
        .entry(key)
        .or_insert(level_start(num_levels, num_id, ilvl) - 1);
    *c += 1;
    for (k, v) in counters.iter_mut() {
        if k.0 == num_id && k.1 > ilvl {
            *v = 0;
        }
    }
}

/// Build a list item's marker from its `lvlText` template — docling's
/// `_build_enum_marker`. A template with literal text (e.g. `Proposal %1:`) has
/// its `%N` placeholders substituted; so does one on a letter / roman /
/// `decimalZero` level even when it holds only placeholders and punctuation
/// (`%2)` → `a)`, docling#4087 — before that guard such levels fell through
/// to `1.a.`), and — beyond docling, #835 — a decimal one whose punctuation
/// is a parenthesis (`(%1)` → `(3)`, `%1)` → `3)`): docling folds those into
/// `3.` too, and the Pandoc export then cannot tell `TwoParens` from
/// `Period`. A bare numeric template (`%1.%2.`) on a decimal level falls
/// back to the hierarchical `1.2.` form joining `counter[0..=ilvl]`; every
/// counter is rendered with its own level's `numFmt`.
pub(super) fn build_enum_marker(
    counters: &HashMap<(String, i64), i64>,
    num_levels: &HashMap<(String, i64), NumLevel>,
    num_id: &str,
    ilvl: i64,
) -> String {
    let counter_at = |lvl: i64| -> i64 {
        counters
            .get(&(num_id.to_string(), lvl))
            .copied()
            .unwrap_or_else(|| level_start(num_levels, num_id, lvl))
    };
    let fmt_at = |lvl: i64| -> Option<&str> {
        num_levels
            .get(&(num_id.to_string(), lvl))
            .and_then(|l| l.num_fmt.as_deref())
    };
    let lvl_text = num_levels
        .get(&(num_id.to_string(), ilvl))
        .map(|l| l.lvl_text.as_str())
        .unwrap_or("");
    let re_placeholder = cached_regex!(r"%(\d+)");
    if re_placeholder.is_match(lvl_text) {
        let stripped: String = re_placeholder.replace_all(lvl_text, "").into_owned();
        let stripped = stripped.trim_matches(|c: char| " .:[]".contains(c));
        if !stripped.is_empty() || is_non_decimal_format(fmt_at(ilvl)) {
            return re_placeholder
                .replace_all(lvl_text, |caps: &regex::Captures| {
                    let lvl_idx: i64 = caps[1].parse::<i64>().unwrap_or(1) - 1;
                    format_enum_counter(counter_at(lvl_idx), fmt_at(lvl_idx))
                })
                .into_owned();
        }
    }
    let parts: Vec<String> = (0..=ilvl)
        .map(|lvl| format_enum_counter(counter_at(lvl), fmt_at(lvl)))
        .collect();
    parts.join(".") + "."
}

/// The numbering marker of a caption-like paragraph (`Tabell 1.2`), which
/// belongs in its text rather than making it a list item — docling's
/// `_get_caption_numbering_marker` (docling#4727, #765). It applies to a
/// paragraph outside the Title / Heading / Code / `List…` / `Normal…`
/// styles whose level template references a shallower level (`%1` in
/// `Tabell %1.%7`, typically a heading counter) and has a letter in its
/// literal part. The counter is advanced and the marker built like a list
/// item's; `None` leaves the paragraph to the list handling.
pub(super) fn caption_numbering_marker(
    counters: &mut HashMap<(String, i64), i64>,
    num_levels: &HashMap<(String, i64), NumLevel>,
    num_id: &str,
    ilvl: i64,
    style_id: &str,
) -> Option<String> {
    let lower = style_id.to_lowercase();
    if matches!(style_id, "Title" | "Heading" | "Code")
        || lower.starts_with("list")
        || lower.starts_with("normal")
    {
        return None;
    }
    let lvl_text = num_levels
        .get(&(num_id.to_string(), ilvl))
        .map(|l| l.lvl_text.as_str())
        .filter(|t| !t.is_empty())?;
    let re_placeholder = cached_regex!(r"%(\d+)");
    let refers_up = re_placeholder
        .captures_iter(lvl_text)
        .filter_map(|c| c[1].parse::<i64>().ok())
        .any(|n| n - 1 < ilvl);
    let literal = re_placeholder.replace_all(lvl_text, "");
    if !refers_up || !literal.chars().any(char::is_alphabetic) {
        return None;
    }
    get_list_counter(counters, num_levels, num_id, ilvl);
    Some(build_enum_marker(counters, num_levels, num_id, ilvl))
}

/// Hand a numbered heading's number to the list counters of its `numId`, so
/// a template on the same numbering (`Tabell %1.%7`) counts within it —
/// docling's `_add_heading` (docling#4727, #765). The heading's level
/// counter takes the heading number; a deeper counter of the same `numId`
/// whose template references this level (`%{ilvl+1}`) restarts, while
/// unrelated deeper counters keep their state.
pub(super) fn sync_heading_counter(
    counters: &mut HashMap<(String, i64), i64>,
    num_levels: &HashMap<(String, i64), NumLevel>,
    heading_number: i64,
    num_id: &str,
    ilvl: i64,
) {
    counters.insert((num_id.to_string(), ilvl), heading_number);
    let placeholder = format!("%{}", ilvl + 1);
    let references_heading = |lvl_text: &str| {
        lvl_text
            .match_indices(&placeholder)
            .any(|(at, m)| !lvl_text[at + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
    };
    for (key, counter) in counters.iter_mut() {
        if key.0 == num_id
            && key.1 > ilvl
            && num_levels
                .get(key)
                .is_some_and(|l| references_heading(&l.lvl_text))
        {
            *counter = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        block_comment_slots, child_elements, flat_text, format_enum_counter, heading_label_level,
        plain_paragraph_text, row_cells, run_child_text, run_groups, Fmt,
    };
    use std::collections::HashMap;

    /// #400: `<w:noBreakHyphen/>` is a hyphen Word will not wrap at, and
    /// python-docx — where docling's paragraph text comes from — renders it as
    /// a plain `-`. Dropping it glued `In` and `Transit` into `InTransit`.
    /// A page or column break, by contrast, has *no* text equivalent there,
    /// while a line break and a carriage return are newlines.
    #[test]
    fn run_inner_content_matches_python_docx() {
        let xml = r#"<w:document xmlns:w="w"><w:body>
            <w:p><w:r><w:t>In</w:t><w:noBreakHyphen/><w:t>Transit</w:t></w:r></w:p>
            <w:p><w:r><w:t>a</w:t><w:tab/><w:t>b</w:t><w:ptab/><w:t>c</w:t></w:r></w:p>
            <w:p><w:r><w:t>line</w:t><w:br/><w:t>wrap</w:t></w:r></w:p>
            <w:p><w:r><w:t>soft</w:t><w:cr/><w:t>return</w:t></w:r></w:p>
            <w:p><w:r><w:t>page</w:t><w:br w:type="page"/><w:t>break</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let dom = roxmltree::Document::parse(xml).unwrap();
        let runs = |p: roxmltree::Node<'_, '_>| -> String {
            p.descendants()
                .filter(|n| n.has_tag_name("r"))
                .flat_map(child_elements)
                .map(run_child_text)
                .collect()
        };
        let texts: Vec<String> = dom
            .descendants()
            .filter(|n| n.has_tag_name("p"))
            .map(runs)
            .collect();
        assert_eq!(
            texts,
            vec![
                "In-Transit",
                "a\tb\tc",
                "line\nwrap",
                "soft\nreturn",
                "pagebreak",
            ]
        );
        // The flattening helper carries the hyphen too, and nothing else: a
        // `w:tab` under `w:pPr/w:tabs` is a tab *stop*, not text.
        let props = roxmltree::Document::parse(
            r#"<w:p xmlns:w="w"><w:pPr><w:tabs><w:tab w:val="left"/></w:tabs></w:pPr>
               <w:r><w:t>co</w:t><w:noBreakHyphen/><w:t>op</w:t></w:r></w:p>"#,
        )
        .unwrap();
        let flat: String = props
            .root_element()
            .descendants()
            .filter_map(flat_text)
            .collect();
        assert_eq!(flat, "co-op");
    }

    /// docling#3946/#3951: Word wraps a table cell in a content control for
    /// cover pages and document-property fields, so the `w:tc` is no longer a
    /// direct child of the `w:tr`. Collecting only direct children skipped it,
    /// which slid every later cell of the row one column to the left (the grid
    /// cursor advances per emitted cell) and emptied a 1×1 layout table.
    #[test]
    fn a_cell_in_a_content_control_is_still_a_cell_of_its_row() {
        let cell = |t: &str| format!("<w:tc><w:p><w:r><w:t>{t}</w:t></w:r></w:p></w:tc>");
        let wrapped = |t: &str| {
            format!(
                "<w:sdt><w:sdtPr/><w:sdtContent>{}</w:sdtContent></w:sdt>",
                cell(t)
            )
        };
        let row = |body: String| {
            format!(
                r#"<w:document xmlns:w="w"><w:body><w:tbl><w:tr>{body}</w:tr></w:tbl></w:body></w:document>"#
            )
        };
        let texts = |xml: &str| {
            let dom = roxmltree::Document::parse(xml).unwrap();
            let tr = dom.descendants().find(|n| n.has_tag_name("tr")).unwrap();
            row_cells(tr)
                .into_iter()
                .map(|tc| {
                    tc.descendants()
                        .filter(|n| n.has_tag_name("t"))
                        .filter_map(|n| n.text())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };
        let plain = row(format!("{}{}{}", cell("a"), cell("b"), cell("c")));
        let want = vec!["a".to_string(), "b".into(), "c".into()];
        assert_eq!(texts(&plain), want);
        // Wrapped in any position, the row keeps the same cells in the same order.
        for xml in [
            row(format!("{}{}{}", wrapped("a"), cell("b"), cell("c"))),
            row(format!("{}{}{}", cell("a"), wrapped("b"), cell("c"))),
            row(format!("{}{}{}", cell("a"), cell("b"), wrapped("c"))),
            row(format!("{}{}{}", wrapped("a"), cell("b"), wrapped("c"))),
        ] {
            assert_eq!(texts(&xml), want);
        }
        // Controls nest, and a control holding nothing contributes no cell.
        let nested = row(format!(
            "<w:sdt><w:sdtContent>{}</w:sdtContent></w:sdt>{}",
            wrapped("a"),
            cell("b")
        ));
        assert_eq!(texts(&nested), vec!["a".to_string(), "b".into()]);
        assert_eq!(
            texts(&row("<w:sdt><w:sdtContent/></w:sdt>".to_string())),
            Vec::<String>::new()
        );
    }

    /// #408: docling's `_get_paragraph_elements` closes the trailing run group
    /// under the format that *opened* it. A whitespace-only run never opens a
    /// group — its text is appended to the current one — so a paragraph whose
    /// bold or italic text is followed by an unformatted `" "` (Word writes
    /// that trailing space as its own run) is still one bold/italic element.
    /// We used the *last* run's format there, which reset it to plain.
    #[test]
    fn a_trailing_unformatted_space_keeps_the_paragraphs_format() {
        let bold = Fmt {
            bold: true,
            ..Fmt::default()
        };
        let italic = Fmt {
            italic: true,
            ..Fmt::default()
        };
        let plain = Fmt::default();
        let groups = |runs: Vec<(&str, Fmt)>| {
            run_groups(
                runs.into_iter()
                    .map(|(t, f)| (t.to_string(), f, None))
                    .collect(),
            )
            .into_iter()
            .map(|(t, f, _)| (t, f))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            groups(vec![("Bold text.", bold), (" ", plain)]),
            vec![("Bold text.".to_string(), bold)]
        );
        assert_eq!(
            groups(vec![("Italic text.", italic), (" ", plain)]),
            vec![("Italic text.".to_string(), italic)]
        );
        // A trailing run with real text still starts its own plain group …
        assert_eq!(
            groups(vec![("Bold text.", bold), (" Plain tail.", plain)]),
            vec![
                ("Bold text.".to_string(), bold),
                ("Plain tail.".to_string(), plain)
            ]
        );
        // … and whitespace *between* differently formatted runs joins the
        // group it follows, exactly as upstream concatenates it.
        assert_eq!(
            groups(vec![("a", bold), (" ", plain), ("b", italic), (" ", plain)]),
            vec![("a".to_string(), bold), ("b".to_string(), italic)]
        );
    }

    /// #409: a code paragraph's text is python-docx's `Paragraph.text`, so a
    /// `<w:br/>` — whether it sits in its own run or between the `w:t`s of one
    /// run — is a newline and a tab is a tab. Flattening the subtree to `w:t`
    /// alone joined `total = a + b` and `print(total)` into one line. A page
    /// break still has no text, a hyperlink's runs count, a content control
    /// contributes its `w:t` text (docling's `.//w:sdtContent//w:t` xpath).
    #[test]
    fn plain_paragraph_text_keeps_line_breaks_and_tabs() {
        let xml = r#"<w:document xmlns:w="w"><w:body>
            <w:p><w:pPr><w:pStyle w:val="Code"/></w:pPr>
                <w:r><w:t xml:space="preserve">total = a + b</w:t></w:r><w:r><w:br/></w:r>
                <w:r><w:t>print(total)</w:t></w:r><w:r><w:br/></w:r><w:r><w:t># done</w:t></w:r></w:p>
            <w:p><w:r><w:t>x = 1</w:t><w:br/><w:t>y = 2</w:t><w:br/><w:t>z = x + y</w:t></w:r></w:p>
            <w:p><w:r><w:t>if x:</w:t><w:br/><w:tab/><w:t>y</w:t><w:br w:type="page"/></w:r></w:p>
            <w:p><w:r><w:t>see </w:t></w:r><w:hyperlink r:id="rId1" xmlns:r="r"><w:r><w:t>docs</w:t><w:br/><w:t>here</w:t></w:r></w:hyperlink></w:p>
            <w:p><w:sdt><w:sdtContent><w:r><w:t>ctrl</w:t><w:br/><w:t>text</w:t></w:r></w:sdtContent></w:sdt></w:p>
        </w:body></w:document>"#;
        let dom = roxmltree::Document::parse(xml).unwrap();
        let texts: Vec<String> = dom
            .descendants()
            .filter(|n| n.has_tag_name("p"))
            .map(plain_paragraph_text)
            .collect();
        assert_eq!(
            texts,
            vec![
                "total = a + b\nprint(total)\n# done",
                "x = 1\ny = 2\nz = x + y",
                "if x:\n\ty",
                "see docs\nhere",
                "ctrltext",
            ]
        );
    }

    /// #385: a list item's `first_in_list` is Word's list identity — a new
    /// `numId`, or body text since the last item, opens a list; the items of
    /// one `numId` continue it, across nesting and across an empty spacer
    /// paragraph (docling's `_manage_list_structure` + its ListGroup cache).
    /// The serializers draw every list boundary from this flag alone.
    #[test]
    fn list_boundaries_follow_word_list_identity() {
        use crate::backend::DeclarativeBackend;
        use docling_core::Node;
        let convert = |name: &str| {
            let path = format!(
                "{}/../../tests/data/docx/sources/{name}",
                env!("CARGO_MANIFEST_DIR")
            );
            let bytes = std::fs::read(&path).expect("fixture exists");
            let src = crate::source::SourceDocument::from_bytes(
                name,
                crate::format::InputFormat::Docx,
                bytes,
            );
            super::DocxBackend::default()
                .convert(&src)
                .expect("converts")
        };
        let flags = |doc: &docling_core::DoclingDocument| -> Vec<(String, bool)> {
            doc.nodes
                .iter()
                .filter_map(|n| match n {
                    Node::ListItem {
                        text,
                        first_in_list,
                        ..
                    } => Some((text.clone(), *first_in_list)),
                    _ => None,
                })
                .collect()
        };
        let lists = flags(&convert("docx_lists.docx"));
        let flag = |t: &str| {
            lists
                .iter()
                .find(|(text, _)| text == t)
                .unwrap_or_else(|| panic!("item {t:?} in {lists:?}"))
                .1
        };
        // Test 1: one bullet list.
        assert!(flag("List item 1"));
        assert!(!flag("List item 2"));
        // Test 3: nested items share the parent's numId — no new list.
        assert!(!flag("List item 1.1"));
        // Test 7: four items of numId 2, then two single-item lists, each a
        // numId of its own, separated by empty paragraphs.
        assert!(flag("First item with numId 2"));
        assert!(!flag("Fourth item with numId 2"));
        let singles: Vec<bool> = lists
            .iter()
            .filter(|(t, _)| t == "Single item of a new list")
            .map(|(_, f)| *f)
            .collect();
        assert_eq!(singles, vec![true, true]);
        // docling#3902: prose between two items of one Word list ends it, an
        // empty spacer paragraph does not — `2. Second section` continues
        // the list that `- 1.2. Sub two` reopened after the prose.
        let spacer = flags(&convert("docx_list_blank_spacer.docx"));
        assert_eq!(
            spacer,
            vec![
                ("First section".to_string(), true),
                ("1.1. Sub one".to_string(), false),
                ("1.2. Sub two".to_string(), true),
                ("Second section".to_string(), false),
            ]
        );
    }

    /// docling's `_format_enum_counter`: letters wrap by repetition (aa, bb),
    /// roman numerals in either case, `decimalZero` pads to two digits.
    #[test]
    fn enum_counters_follow_num_fmt() {
        assert_eq!(format_enum_counter(1, Some("lowerLetter")), "a");
        assert_eq!(format_enum_counter(26, Some("lowerLetter")), "z");
        assert_eq!(format_enum_counter(27, Some("upperLetter")), "AA");
        assert_eq!(format_enum_counter(4, Some("lowerRoman")), "iv");
        assert_eq!(format_enum_counter(1994, Some("upperRoman")), "MCMXCIV");
        assert_eq!(format_enum_counter(7, Some("decimalZero")), "07");
        assert_eq!(format_enum_counter(7, Some("decimal")), "7");
        assert_eq!(format_enum_counter(7, None), "7");
        assert_eq!(format_enum_counter(0, Some("lowerLetter")), "0");
    }

    /// A comment range that opens in one paragraph and closes in a later one
    /// annotates every paragraph in between; `w:commentReference` alone (Word's
    /// range-less anchor) annotates just its own paragraph.
    #[test]
    fn comment_ranges_span_paragraphs() {
        let xml = r#"<w:document xmlns:w="w">
          <w:body>
            <w:p><w:commentRangeStart w:id="0"/><w:r><w:t>a</w:t></w:r></w:p>
            <w:p><w:r><w:t>b</w:t></w:r></w:p>
            <w:p><w:commentRangeEnd w:id="0"/><w:commentReference w:id="0"/></w:p>
            <w:p><w:r><w:t>d</w:t></w:r></w:p>
            <w:p><w:commentReference w:id="1"/></w:p>
          </w:body>
        </w:document>"#;
        let dom = roxmltree::Document::parse(xml).unwrap();
        let body = dom.descendants().find(|n| n.has_tag_name("body")).unwrap();
        let slot_of: HashMap<&str, usize> = [("0", 0), ("1", 1)].into_iter().collect();
        let mut open = Vec::new();
        let slots: Vec<Vec<usize>> = body
            .children()
            .filter(|n| n.is_element())
            .map(|p| block_comment_slots(p, &slot_of, &mut open))
            .collect();
        assert_eq!(
            slots,
            vec![vec![0], vec![0], vec![0], Vec::new(), vec![1]],
            "ranges must cover the paragraphs between start and end"
        );
        assert!(open.is_empty(), "the closed range must not stay open");
    }

    /// The `_get_heading_and_level` + `_split_text_and_number` lattice on a
    /// single label (#270 tail); levels are Markdown levels (docling + 1).
    #[test]
    fn heading_label_parsing_matches_docling() {
        // <text><digits> over the whole label, text trimming to "heading".
        assert_eq!(heading_label_level("Heading1"), Some(2));
        assert_eq!(heading_label_level("heading 3"), Some(4));
        // <digits><text>, the rest ignored (upstream's `^(\d+)(\D+)` alt).
        assert_eq!(heading_label_level("2Heading"), Some(3));
        // Custom "Heading 0" clamps to level 1 (md ##), "Heading 111" to 9
        // (docling#4319).
        assert_eq!(heading_label_level("Heading 0"), Some(2));
        assert_eq!(heading_label_level("Heading 111"), Some(10));
        assert_eq!(heading_label_level("Heading 99999"), Some(10));
        // A longer text part is not "heading": upstream's ("", 0) branch.
        assert_eq!(heading_label_level("My Heading 2"), None);
        // No digit split: heading only with a capital-H "Heading" in the raw
        // label (matches upstream's case-sensitive `"Heading" in p_style_id`).
        assert_eq!(heading_label_level("HeadingCustom"), Some(2));
        assert_eq!(heading_label_level("Heading 1a2"), Some(2));
        assert_eq!(heading_label_level("my heading"), None);
    }
}

#[cfg(test)]
mod inherited_heading_tests {
    use std::io::Write;

    use crate::backend::DeclarativeBackend;
    use crate::{InputFormat, SourceDocument};

    /// A DOCX of `styles` (`<w:style>` elements) and `body` (`<w:body>`
    /// children), with the numbering of a single decimal list.
    fn docx(styles: &str, body: &str) -> crate::DoclingDocument {
        const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
        let document = format!(r#"<w:document xmlns:w="{W}"><w:body>{body}</w:body></w:document>"#);
        let styles = format!(r#"<w:styles xmlns:w="{W}">{styles}</w:styles>"#);
        let numbering = format!(
            r#"<w:numbering xmlns:w="{W}"><w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num></w:numbering>"#
        );
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, content) in [
            ("word/document.xml", document.as_str()),
            ("word/styles.xml", styles.as_str()),
            ("word/numbering.xml", numbering.as_str()),
        ] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(content.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();
        let src = SourceDocument::from_bytes("t.docx", InputFormat::Docx, bytes);
        super::DocxBackend::default().convert(&src).unwrap()
    }

    fn style(id: &str, based_on: Option<&str>, outline: Option<u8>) -> String {
        let based = based_on
            .map(|b| format!(r#"<w:basedOn w:val="{b}"/>"#))
            .unwrap_or_default();
        let ppr = outline
            .map(|o| format!(r#"<w:pPr><w:outlineLvl w:val="{o}"/></w:pPr>"#))
            .unwrap_or_default();
        format!(
            r#"<w:style w:type="paragraph" w:styleId="{id}"><w:name w:val="{id}"/>{based}{ppr}</w:style>"#
        )
    }

    fn para(style: &str, text: &str, numbered: bool) -> String {
        let num = if numbered {
            r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr>"#
        } else {
            ""
        };
        format!(
            r#"<w:p><w:pPr><w:pStyle w:val="{style}"/>{num}</w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
        )
    }

    /// #841: a style two (or more) `basedOn` hops from Heading 2 is a level-2
    /// heading, a numbered one too (not a list item); a custom style's own
    /// `w:outlineLvl` promotes it through the chain as well; an explicit
    /// body-text outline (9) on the way up is not a heading whatever it is
    /// based on; a cyclic chain terminates as body text.
    #[test]
    fn heading_styles_inherit_through_the_whole_based_on_chain() {
        let styles = [
            style("Heading2", Some("Normal"), Some(1)),
            style("SectionTitleL2", Some("Heading2"), None),
            style("SubSectionTitleL2", Some("SectionTitleL2"), None),
            style("Deep", Some("SubSectionTitleL2"), None),
            style("Outlined", Some("Normal"), Some(2)),
            style("ViaOutline", Some("Outlined"), None),
            style("BodyOptOut", Some("SectionTitleL2"), Some(9)),
            style("LoopA", Some("LoopB"), None),
            style("LoopB", Some("LoopA"), None),
        ]
        .concat();
        let body = [
            para("SubSectionTitleL2", "Two hops", false),
            para("Deep", "Three hops numbered", true),
            para("ViaOutline", "Outline two hops", false),
            para("BodyOptOut", "Opted out", false),
            para("LoopA", "Cyclic", false),
            para("Normal", "Body.", false),
        ]
        .concat();
        let doc = docx(&styles, &body);
        assert_eq!(
            doc.export_to_markdown(),
            "### Two hops\n\n### 1 Three hops numbered\n\n#### Outline two hops\n\nOpted out\n\nCyclic\n\nBody.\n"
        );
        let json = doc.export_to_json();
        assert_eq!(json.matches("\"section_header\"").count(), 3, "{json}");
        assert!(!json.contains("\"list_item\""), "{json}");
    }
}

#[cfg(test)]
mod fragment_rels_tests {
    use crate::backend::DeclarativeBackend;
    use crate::{InputFormat, SourceDocument};

    /// docling#4243 (2.128): a relationship whose target is a bare fragment
    /// (`Target="#anchor"`, an internal bookmark some generators write as a
    /// relationship) is dropped before the package is read — the hyperlink
    /// keeps its text and carries no target, and a real link still resolves.
    #[test]
    fn fragment_only_relationship_targets_are_dropped() {
        use std::io::Write;
        let body = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>
<w:p><w:r><w:t xml:space="preserve">see </w:t></w:r><w:hyperlink r:id="rId1"><w:r><w:t>the annex</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> and </w:t></w:r><w:hyperlink r:id="rId2"><w:r><w:t>the site</w:t></w:r></w:hyperlink></w:p>
</w:body></w:document>"#;
        let rels = r##"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="#_Proc%C3%A9dures" TargetMode="External"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/></Relationships>"##;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, content) in [
            ("word/document.xml", body),
            ("word/_rels/document.xml.rels", rels),
        ] {
            zw.start_file(name, opts).unwrap();
            zw.write_all(content.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();
        let src = SourceDocument::from_bytes("t.docx", InputFormat::Docx, bytes);
        let md = super::DocxBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert_eq!(md, "see the annex and [the site](https://example.com)\n");
    }
}
