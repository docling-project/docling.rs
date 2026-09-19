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

use std::collections::HashMap;

use docling_core::{DoclingDocument, InlineRun, ListItemDclx, Node, PictureImage, Script, Table};
use roxmltree::{Document, Node as XmlNode};

use crate::backend::markdown::escape_text;
use crate::backend::ooxml::{resolve, Package};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct DocxBackend;

impl DeclarativeBackend for DocxBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("docx: bad zip".into()))?;
        let document = pkg
            .read("word/document.xml")
            .ok_or_else(|| ConversionError::Parse("docx: no document.xml".into()))?;
        let styles = pkg.read("word/styles.xml").unwrap_or_default();
        let numbering = pkg.read("word/numbering.xml").unwrap_or_default();
        // Hyperlink relationship ids → target URLs.
        let rels: HashMap<String, String> = pkg
            .rels_for("word/document.xml")
            .iter()
            .map(|r| {
                let t = if r.rel_type.ends_with("/hyperlink") {
                    r.target.clone()
                } else {
                    resolve("word", &r.target)
                };
                (r.id.clone(), t)
            })
            .collect();

        // Embedded images, by relationship id (for image export).
        let images = pkg.image_rels("word/document.xml", "word");
        let charts = chart_rels(&mut pkg, "word/document.xml");

        let sm = parse_styles(&styles);
        let num_levels = parse_numbering(&numbering);

        let dom =
            Document::parse(&document).map_err(|e| ConversionError::with_source("docx", e))?;
        let ctx = Ctx {
            style_names: &sm.names,
            style_nums: &sm.nums,
            style_based: &sm.based,
            style_fonts: &sm.fonts,
            style_outline: &sm.outlines,
            style_bold: &sm.bolds,
            num_levels: &num_levels,
            rels: &rels,
            images: &images,
            charts: &charts,
            table_depth: std::cell::Cell::new(0),
        };

        let mut doc = DoclingDocument::new(&source.name);
        let Some(body) = dom.descendants().find(|n| n.has_tag_name("body")) else {
            return Ok(doc);
        };
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
        // Section headers/footers follow the body as furniture-layer content
        // (docling's `_add_header_footer`): the first section always
        // contributes, later sections only when they define a distinct first
        // page (`<w:titlePg/>`), which also switches both to the first-page
        // parts.
        add_header_footer(&mut pkg, body, &ctx, &mut doc);
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
/// first-page before regular within each.
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
            let types: &[&str] = if title_pg {
                &["first", "default"]
            } else {
                &["default"]
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

/// Walk one header/footer part and append its blocks wrapped in the furniture
/// layer. docling skips a part with no visible content (no non-blank paragraph
/// text, tables, images, or textboxes).
fn emit_header_footer_part(pkg: &mut Package, part: &str, ctx: &Ctx, doc: &mut DoclingDocument) {
    let Some(xml) = pkg.read(part) else {
        return;
    };
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
/// other target resolved against `word/`.
pub(super) fn part_rels(pkg: &mut Package, part: &str) -> HashMap<String, String> {
    pkg.rels_for(part)
        .iter()
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
    pub(super) num_levels: &'a HashMap<(String, i64), NumLevel>, // (numId, ilvl) -> level props
    pub(super) rels: &'a HashMap<String, String>,
    pub(super) images: &'a HashMap<String, PictureImage>, // image relationship id -> extracted image
    /// Native charts by relationship id: `(classified kind, title, data grid)`
    /// parsed from the `word/charts/*.xml` parts (docling PR #3809).
    pub(super) charts: &'a HashMap<String, (String, Option<String>, docling_core::Table)>,
    /// Nesting depth of the table being parsed (a table inside a cell inside a
    /// table …), bounded by [`MAX_TABLE_DEPTH`]: `parse_table_with` recurses
    /// per level, and a 35 KB file with 2 000 tables nested one inside the next
    /// overflowed the stack — an abort, not an error.
    pub(super) table_depth: std::cell::Cell<u32>,
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
            num_levels: self.num_levels,
            rels,
            images,
            charts,
            table_depth: std::cell::Cell::new(0),
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
}

/// Dispatch a body-level block: paragraph, table, or `<w:sdt>` (whose
/// `<w:sdtContent>` children are processed transparently).
fn process_block(node: XmlNode, ctx: &Ctx, state: &mut ListState, doc: &mut DoclingDocument) {
    match node.tag_name().name() {
        "p" => handle_paragraph(node, ctx, state, doc),
        "tbl" => {
            // A 1×1 table is treated as furniture: its single cell's content is
            // processed as document-body blocks (docling unwraps it).
            let rows: Vec<XmlNode> = node.children().filter(|n| n.has_tag_name("tr")).collect();
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
                state.list_run_base = None;
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
    let p_pr = p.children().find(|n| n.has_tag_name("pPr"));
    let style_id = p_pr
        .and_then(|pr| pr.children().find(|n| n.has_tag_name("pStyle")))
        .and_then(|s| attr(s, "val"))
        .unwrap_or("");
    let style_name = ctx
        .style_names
        .get(style_id)
        .cloned()
        .unwrap_or_else(|| style_id.to_string());

    // Textbox content is emitted first, before the paragraph's own content — and
    // for *every* paragraph (a textbox can be anchored to a heading or list item,
    // not just a plain one). Each `<w:txbxContent>` yields its paragraphs' text
    // then any nested images, in document order. A whole textbox is skipped when
    // its combined text was already seen, dropping the `<mc:AlternateContent>`
    // duplicate (modern DrawingML + VML fallback carry the same textbox).
    if !skip_textbox {
        // Dedup textbox *paragraphs* (not whole textboxes): a non-empty one by
        // its text, an image-only one by its position — exactly docling's
        // `_handle_textbox_content`, which drops the `<mc:AlternateContent>`
        // duplicate as well as repeated identical labels in the same drawing.
        let mut seen: Vec<(String, usize)> = Vec::new();
        for tc in p.descendants().filter(|n| n.has_tag_name("txbxContent")) {
            for (idx, tp) in tc.children().filter(|n| n.has_tag_name("p")).enumerate() {
                let trimmed = paragraph_markdown(tp, ctx).trim().to_string();
                let key = if trimmed.is_empty() {
                    (String::new(), idx)
                } else {
                    (trimmed.clone(), usize::MAX)
                };
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
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
                for image in drawing_images(tp, ctx, false) {
                    doc.push(Node::Picture {
                        caption: None,
                        caption_href: None,
                        image,
                        classification: None,
                        caption_parent: Default::default(),
                    });
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
    for image in drawing_images(p, ctx, true) {
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image,
            classification: None,
            caption_parent: Default::default(),
        });
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
        state.list_run_base = None;
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
        let text = clean_checkbox_symbols(&paragraph_markdown(p, ctx));
        doc.push(Node::CheckboxItem { checked, text });
        state.list_run_base = None;
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
    let numbering = if p.descendants().any(|n| n.has_tag_name("numPr")) {
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
    let outline = ctx
        .style_outline
        .get(style_id)
        .copied()
        .filter(|l| (1..=9).contains(l));
    let level = match (named, outline) {
        (Some(1), _) => Some(1), // Title
        (Some(_), Some(out)) => Some(out + 1),
        (named, Some(out)) if !is_title_style(style_id, &style_name, ctx) => {
            debug_assert!(named.is_none());
            Some(out + 1)
        }
        (named, _) => named,
    };
    if let Some(level) = level {
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
                numbered_heading_text(&mut state.numbered_headers, docling_level, &text)
            } else {
                text
            };
            doc.push(Node::Heading { level, text });
            state.seen_heading = true;
        }
        state.list_run_base = None;
        state.last_list_num_id = None;
        return;
    }

    // A code paragraph — either its style marks it as code, or it is set in a
    // monospaced font and reads like code (docling PR #3735). Merged into the
    // preceding fenced block when it directly follows one.
    let prev_is_code = matches!(doc.nodes.last(), Some(Node::Code { .. }));
    if !rich && (is_code_style(style_id, ctx) || is_code_by_font(p, style_id, ctx, prev_is_code)) {
        state.list_run_base = None;
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

    if let Some((num_id, ilvl)) = numbering {
        let numbered = ctx
            .num_levels
            .get(&(num_id.clone(), ilvl))
            .map(|l| l.numbered)
            .unwrap_or(false);
        // The base indent of the current contiguous list run (the first item's
        // `ilvl`); nesting spans numIds within the run.
        let base = *state.list_run_base.get_or_insert(ilvl);
        let level = (ilvl - base).max(0) as u8;
        if text.is_empty() {
            return;
        }
        // Word's list identity is the `numId`: the item continues the last
        // list when it carries the same one and no body content intervened;
        // anything else is a new list, so the serializers never have to infer
        // the boundary from the numbering (#385).
        let first_in_list = state.last_list_num_id.as_deref() != Some(num_id.as_str());
        state.last_list_num_id = Some(num_id.clone());
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
                    dclx: None,
                    href: None,
                    layer: None,
                });
            } else {
                // A multilevel marker (`1.1.`) is a Markdown bullet with the
                // marker kept as a text prefix (`- 1.1. text`), but an ordered
                // DocLang item with a clean-text `<marker>` — carried in `dclx`.
                let dclx = Some(ListItemDclx {
                    ordered: true,
                    marker: Some(marker.clone()),
                    text: text.clone(),
                    runs: Vec::new(),
                });
                doc.push(Node::ListItem {
                    ordered: false,
                    number,
                    first_in_list,
                    text: format!("{marker} {text}"),
                    level,
                    marker: None,
                    location: None,
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
                let mut tuples = Vec::new();
                collect_run_tuples(p, Fmt::default(), None, ctx, &mut tuples);
                let groups = run_groups(tuples);
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
    state.list_run_base = None;
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
            let mut tuples = Vec::new();
            collect_run_tuples(p, Fmt::default(), None, ctx, &mut tuples);
            for (t, f, l) in run_groups(tuples) {
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
            let mut tuples = Vec::new();
            collect_run_tuples(p, Fmt::default(), None, ctx, &mut tuples);
            let groups = run_groups(tuples);
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
/// as textbox text instead).
fn drawing_images(node: XmlNode, ctx: &Ctx, skip_textbox: bool) -> Vec<Option<PictureImage>> {
    let keep = |n: XmlNode| !skip_textbox || !in_textbox(n);
    let blips: Vec<XmlNode> = node
        .descendants()
        .filter(|n| n.has_tag_name("blip") && keep(*n))
        .collect();
    if !blips.is_empty() {
        return blips
            .iter()
            .map(|b| attr(*b, "embed").and_then(|id| ctx.images.get(id)).cloned())
            .collect();
    }
    let vml: Vec<Option<PictureImage>> = node
        .descendants()
        .filter(|n| n.has_tag_name("imagedata") && keep(*n))
        .map(|d| attr(d, "id").and_then(|id| ctx.images.get(id)).cloned())
        .collect();
    if !vml.is_empty() {
        return vml;
    }
    if node.descendants().any(|n| {
        n.has_tag_name("drawing")
            && keep(n)
            // A drawing whose graphic is a parsed chart emits a Chart node
            // instead of the rendered-placeholder picture.
            && !n.descendants().any(|c| {
                c.has_tag_name("chart")
                    && attr(c, "id").is_some_and(|id| ctx.charts.contains_key(id))
            })
    }) {
        return vec![None];
    }
    Vec::new()
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

/// Markdown heading level from a paragraph style, by name — the name path of
/// docling's `_get_label_and_level` (#270 tail: custom styles without
/// `w:outlineLvl`). The style *id*, its display name, and their one-hop
/// `basedOn` counterparts are checked in that order for a "heading" substring
/// (docling's `is_heading`), and the FIRST label containing it decides alone —
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
    let base = ctx.style_based.get(style_id).map(String::as_str);
    let labels = [
        Some(style_id),
        Some(style_name),
        base,
        base.and_then(|b| ctx.style_names.get(b))
            .map(String::as_str),
    ];
    for label in labels.into_iter().flatten() {
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
            let level = digits.parse::<u8>().unwrap_or(u8::MAX - 1).max(1);
            Some(level.saturating_add(1))
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
pub(super) fn num_pr(p: XmlNode) -> Option<(String, i64)> {
    let num_pr = p.descendants().find(|n| n.has_tag_name("numPr"))?;
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
    let mut runs: Vec<(String, Fmt, Option<String>)> = Vec::new();
    collect_run_tuples(p, Fmt::default(), None, ctx, &mut runs);
    group_runs(runs)
}

/// All element children of a node.
pub(super) fn child_elements<'a, 'i>(n: XmlNode<'a, 'i>) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    n.children().filter(XmlNode::is_element)
}

/// Strip a leading checkbox glyph (matches docling's `_clean_checkbox_symbols`).
pub(super) fn clean_checkbox_symbols(text: &str) -> String {
    let t = text.trim();
    for sym in ['☐', '☑', '☒', '□', '■', '▪', '▫'] {
        if let Some(rest) = t.strip_prefix(sym) {
            return rest.trim().to_string();
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
    let mut groups: Vec<(String, Fmt, Option<String>)> = Vec::new();
    let mut group_text = String::new();
    let mut previous_format: Option<Fmt> = None;
    let mut last_format = Fmt::default();
    for (text, fmt, link) in runs {
        last_format = fmt;
        if (!text.trim().is_empty() && Some(fmt) != previous_format) || link.is_some() {
            if !group_text.trim().is_empty() {
                groups.push((
                    group_text.trim().to_string(),
                    previous_format.unwrap_or_default(),
                    None,
                ));
            }
            group_text.clear();
            if link.is_some() {
                groups.push((text.trim().to_string(), fmt, link));
                continue;
            }
            previous_format = Some(fmt);
        }
        group_text.push_str(&text);
    }
    if !group_text.trim().is_empty() {
        // The trailing group closes under the format that *opened* it, as in
        // docling — a whitespace-only run never opens a group (its text is
        // just appended), so it must not decide the group's format either.
        // Taking the last run's format instead lost the bold/italic of a
        // paragraph that ends with an unformatted space (#408).
        groups.push((
            group_text.trim().to_string(),
            previous_format.unwrap_or(last_format),
            None,
        ));
    }
    groups
}

/// One serialized Markdown segment per format group — docling's
/// `_get_paragraph_elements`.
fn run_segments(runs: Vec<(String, Fmt, Option<String>)>) -> Vec<String> {
    run_groups(runs)
        .iter()
        .map(|(t, f, l)| serialize_run(t, *f, l.as_deref()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// The segments joined with single spaces (a body paragraph's inline group).
fn group_runs(runs: Vec<(String, Fmt, Option<String>)>) -> String {
    run_segments(runs).join(" ")
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
                // The whole reconstructed text is stripped at its ends; the final
                // text fragment is additionally stripped in full.
                let s = if i == n - 1 {
                    t.trim()
                } else if i == 0 {
                    t.trim_start()
                } else {
                    t.as_str()
                };
                if !s.is_empty() {
                    out.push(escape_text(s));
                }
            }
        }
    }
    out.join(" ")
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
            let url = attr(child, "id").and_then(|id| ctx.rels.get(id)).cloned();
            let mut inner = Vec::new();
            collect_run_tuples(child, fmt, None, ctx, &mut inner);
            let text: String = inner.iter().map(|(t, _, _)| t.as_str()).collect();
            let lfmt = inner.first().map(|(_, f, _)| *f).unwrap_or(fmt);
            if !text.trim().is_empty() {
                out.push((text, lfmt, url.or(link.map(str::to_string))));
            }
        }
        // Transparent inline wrappers.
        "smartTag" | "ins" | "fldSimple" | "sdt" | "sdtContent" => {
            collect_run_tuples(child, fmt, link, ctx, out)
        }
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
        s = format!("[{s}]({url})");
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
    let rows: Vec<XmlNode> = tbl.children().filter(|n| n.has_tag_name("tr")).collect();
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
        rows: grid,
        location: None,
        structure,
        cell_blocks: any_rich.then_some(blocks),
        cells: None,
        caption: None,
        caption_parent: Default::default(),
    })
}

pub(super) fn grid_span(tc: XmlNode) -> usize {
    tc.descendants()
        .find(|n| n.has_tag_name("gridSpan"))
        .and_then(|n| attr(n, "val"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// The plain text of one child of a `w:r`, python-docx's `CT_R.text` — which
/// is where docling's paragraph text comes from, so this is the parity target.
/// Anything not in its `w:br | w:cr | w:noBreakHyphen | w:ptab | w:t | w:tab`
/// set contributes nothing.
pub(super) fn run_child_text(n: XmlNode) -> String {
    match n.tag_name().name() {
        "t" => n.text().unwrap_or("").to_string(),
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
/// Only `w:t` and `w:noBreakHyphen` are safe to pick up that way — `w:tab` and
/// `w:br` also appear in paragraph *properties* (`w:pPr/w:tabs/w:tab`), which
/// carry no text; `collect_run_tuples` handles those from the run itself.
pub(super) fn flat_text<'a, 'i>(n: XmlNode<'a, 'i>) -> Option<&'a str> {
    match n.tag_name().name() {
        "t" => Some(n.text().unwrap_or("")),
        "noBreakHyphen" => Some("-"),
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
/// then flattens (`\n` → space). Mirrors docling's `_is_rich_table_cell`.
fn cell_markdown(tc: XmlNode, ctx: &Ctx) -> String {
    if is_rich_cell(tc) {
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

/// Whether a cell must be parsed as rich content rather than plain text.
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
        match child.tag_name().name() {
            "p" => handle_paragraph_inner(child, ctx, &mut state, &mut sub, true, false),
            "tbl" => {
                if let Some(table) = parse_table_with(child, ctx, true) {
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
            }
            _ => {}
        }
    }
    // In-cell rendering: a heading inside the cell is plain text
    // (docling-core#540).
    sub.export_to_table_cell_markdown().trim().to_string()
}

/// A rich cell's DocLang block content — the structured counterpart of
/// [`rich_cell_markdown`]: the cell's paragraphs/lists plus *full* nested tables
/// (kept as `Node::Table`, not flattened), built by walking the cell like a
/// document body. Empty for a plain cell, whose flat text the serializer uses.
/// Markdown/JSON never consult this (they render the flat cell text).
fn cell_blocks_of(tc: XmlNode, ctx: &Ctx) -> Vec<Node> {
    if !is_rich_cell(tc) {
        return Vec::new();
    }
    let mut sub = DoclingDocument::new("");
    let mut state = ListState::default();
    for child in child_elements(tc) {
        match child.tag_name().name() {
            "p" => handle_paragraph_inner(child, ctx, &mut state, &mut sub, true, false),
            "tbl" => {
                if let Some(table) = parse_table_with(child, ctx, false) {
                    sub.push(Node::Table(table));
                }
            }
            _ => {}
        }
    }
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
        "hyperlink" => {
            for r in node.children().filter(|n| n.has_tag_name("r")) {
                out.extend(child_elements(r).map(run_child_text));
            }
        }
        "sdt" => {
            for t in node.descendants().filter(|n| n.has_tag_name("t")) {
                out.push_str(t.text().unwrap_or(""));
            }
        }
        "smartTag" | "customXml" | "ins" | "fldSimple" => {
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
    let call = cached_regex!(r#"[A-Za-z_]\((?:\s*\)|[^)]*[\d,._='"][^)]*\))"#).is_match(stripped);
    let def = cached_regex!(
        r"(?m)^[ \t]*(?:async\s+)?(?:def|class|if|elif|while|for|with|except|finally|try|catch|switch|function|func|fn|sub|proc)\s+\S[^\n]*:[ \t]*$"
    )
    .is_match(stripped);
    let has_code_char = has_strong || call || def;
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

/// Best-effort code language for a fenced block (docling's `detect_code_language`,
/// the conservative markers used for DOCX). Returns `None` (→ `unknown`) unless a
/// distinctive marker is present.
pub(super) fn detect_code_language(text: &str) -> Option<String> {
    let lang = |l: &str| Some(l.to_string());
    if cached_regex!(r"(?m)^[ \t]*(?:def|elif)\b|\b__name__\b|^[ \t]*from\s+\S+\s+import\b")
        .is_match(text)
    {
        return lang("Python");
    }
    if cached_regex!(r"(?i)^[ \t]*(select|insert|update|delete|create|alter|drop)\b").is_match(text)
        && cached_regex!(r"(?i)\b(from|into|table|where|values|set)\b").is_match(text)
    {
        return lang("SQL");
    }
    None
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
}
fn parse_styles(styles_xml: &str) -> StyleMaps {
    let mut names = HashMap::new();
    let mut nums = HashMap::new();
    let mut based = HashMap::new();
    let mut fonts = HashMap::new();
    let mut outlines = HashMap::new();
    let mut bolds = HashMap::new();
    let Ok(dom) = Document::parse(styles_xml) else {
        return StyleMaps {
            names,
            nums,
            based,
            fonts,
            outlines,
            bolds,
        };
    };
    for style in dom.descendants().filter(|n| n.has_tag_name("style")) {
        let Some(id) = attr(style, "styleId") else {
            continue;
        };
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
    }
    StyleMaps {
        names,
        nums,
        based,
        fonts,
        outlines,
        bolds,
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
];

/// Map `(numId, ilvl)` → its level properties, resolved through `numbering.xml`'s
/// `num` → `abstractNum` → level (`numFmt`, `start`, `lvlText`).
fn parse_numbering(numbering_xml: &str) -> HashMap<(String, i64), NumLevel> {
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
    // abstractNumId -> { ilvl -> NumLevel }
    let mut abstract_levels: HashMap<String, HashMap<i64, NumLevel>> = HashMap::new();
    for abs in dom.descendants().filter(|n| n.has_tag_name("abstractNum")) {
        let Some(abs_id) = attr(abs, "abstractNumId") else {
            continue;
        };
        let mut levels = HashMap::new();
        for lvl in abs.children().filter(|n| n.has_tag_name("lvl")) {
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
            levels.insert(
                ilvl,
                NumLevel {
                    numbered,
                    visible,
                    num_fmt: num_fmt.map(str::to_string),
                    start,
                    lvl_text,
                },
            );
        }
        abstract_levels.insert(abs_id.to_string(), levels);
    }
    for (num_id, abs_id) in num_to_abstract {
        if let Some(levels) = abstract_levels.get(&abs_id) {
            for (ilvl, lvl) in levels {
                out.insert((num_id.clone(), *ilvl), lvl.clone());
            }
        }
    }
    out
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
/// `_format_enum_counter`: `lowerLetter`/`upperLetter` run a…z, aa…zz (the
/// letter repeated), roman numerals, `decimalZero` pads to two digits, and
/// everything else (including no format) is the plain decimal.
fn format_enum_counter(counter: i64, num_fmt: Option<&str>) -> String {
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
/// to `1.a.`). A bare numeric template (`%1.%2.`) on a decimal level falls
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
        let stripped = stripped.trim_matches(|c: char| " .)(:[]".contains(c));
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
            super::DocxBackend.convert(&src).expect("converts")
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
        // Custom "Heading 0" clamps to level 1 (md ##).
        assert_eq!(heading_label_level("Heading 0"), Some(2));
        // A longer text part is not "heading": upstream's ("", 0) branch.
        assert_eq!(heading_label_level("My Heading 2"), None);
        // No digit split: heading only with a capital-H "Heading" in the raw
        // label (matches upstream's case-sensitive `"Heading" in p_style_id`).
        assert_eq!(heading_label_level("HeadingCustom"), Some(2));
        assert_eq!(heading_label_level("Heading 1a2"), Some(2));
        assert_eq!(heading_label_level("my heading"), None);
    }
}
