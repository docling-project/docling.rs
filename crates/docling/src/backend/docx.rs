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
//! `omml.rs`. Out of scope for now (tracked in MIGRATION.md §5): position-sorted
//! layout of grouped/anchored drawings and the `<mc:AlternateContent>` image
//! de-duplication for grouped shapes.

use std::collections::HashMap;

use docling_core::{
    DoclingDocument, InlineRun, ListItemDclx, Node, PictureImage, Script, Table,
};
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

        let (style_names, style_nums) = parse_styles(&styles);
        let num_levels = parse_numbering(&numbering);

        let dom =
            Document::parse(&document).map_err(|e| ConversionError::Parse(format!("docx: {e}")))?;
        let ctx = Ctx {
            style_names: &style_names,
            style_nums: &style_nums,
            num_levels: &num_levels,
            rels: &rels,
            images: &images,
        };

        let mut doc = DoclingDocument::new(&source.name);
        let Some(body) = dom.descendants().find(|n| n.has_tag_name("body")) else {
            return Ok(doc);
        };
        let mut state = ListState::default();
        for node in body.children().filter(XmlNode::is_element) {
            process_block(node, &ctx, &mut state, &mut doc);
        }
        // Reviewer comments (docling's `notes` layer) are appended after the body
        // as furniture text; Markdown/JSON drop them, DocLang emits `<layer
        // value="notes"/>` items.
        for comment in parse_comments(&mut pkg) {
            doc.nodes
                .push(Node::Furniture(Box::new(Node::Paragraph { text: comment })));
        }
        Ok(doc)
    }
}

/// Parse `word/comments.xml` into docling's per-comment note strings:
/// `[author: {author} ({initials}), time: {iso}]: {text}` (the author/initials
/// parts drop out when absent). Empty when the part is missing.
fn parse_comments(pkg: &mut Package) -> Vec<String> {
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
        let text: String = c
            .descendants()
            .filter(|n| n.has_tag_name("t"))
            .filter_map(|n| n.text())
            .collect();
        let head = if author.is_empty() {
            format!("[time: {date}]")
        } else if initials.is_empty() {
            format!("[author: {author}, time: {date}]")
        } else {
            format!("[author: {author} ({initials}), time: {date}]")
        };
        out.push(format!("{head}: {text}"));
    }
    out
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

struct Ctx<'a> {
    style_names: &'a HashMap<String, String>,
    style_nums: &'a HashMap<String, (String, i64)>, // styleId -> (numId, ilvl)
    num_levels: &'a HashMap<(String, i64), NumLevel>, // (numId, ilvl) -> level props
    rels: &'a HashMap<String, String>,
    images: &'a HashMap<String, PictureImage>, // image relationship id -> extracted image
}

/// Mutable list/heading numbering state carried across the body walk.
#[derive(Default)]
struct ListState {
    counters: HashMap<(String, i64), i64>, // (numId, ilvl) -> running number
    numbered_headers: HashMap<u8, u64>,    // heading level -> running number
    list_run_base: Option<i64>,            // base ilvl of the current contiguous list run
    prev_textbox: Vec<String>,             // textbox labels emitted by the previous paragraph
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
                .map(|r| {
                    r.children()
                        .filter(|n| n.has_tag_name("tc"))
                        .map(grid_span)
                        .sum::<usize>()
                })
                .max()
                .unwrap_or(0);
            if rows.len() == 1 && num_cols == 1 {
                if let Some(cell) = rows[0].children().find(|n| n.has_tag_name("tc")) {
                    for child in child_elements(cell) {
                        process_block(child, ctx, state, doc);
                    }
                }
            } else if let Some(table) = parse_table(node, ctx) {
                doc.push(Node::Table(table));
                state.list_run_base = None;
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
        let mut current: Vec<String> = Vec::new();
        let mut any_textbox = false;
        for tc in p.descendants().filter(|n| n.has_tag_name("txbxContent")) {
            any_textbox = true;
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
                // Drop a label already emitted by the immediately preceding
                // paragraph's textboxes — docling's global processed-element guard
                // skips a duplicated drawing anchored to an adjacent paragraph.
                if !trimmed.is_empty() && state.prev_textbox.contains(&trimmed) {
                    continue;
                }
                // Process the paragraph fully (list items, formatting); `skip_textbox`
                // stops it re-extracting nested textboxes, which this loop covers.
                if !trimmed.is_empty() {
                    current.push(trimmed.clone());
                    handle_paragraph_inner(tp, ctx, state, doc, false, true);
                }
                for image in drawing_images(tp, ctx, false) {
                    doc.push(Node::Picture {
                        caption: None,
                        image,
                    });
                }
            }
        }
        if any_textbox {
            state.prev_textbox = current;
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
        let marker = if checked { "- [x] " } else { "- [ ] " };
        doc.push(Node::Paragraph {
            text: format!("{marker}{text}"),
        });
        state.list_run_base = None;
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
        ctx.style_nums.get(style_id).cloned()
    };

    // A heading style wins over a list: a numbered heading gets a computed
    // number prefix (`## 1 Section 1`) rather than becoming a list item.
    if let Some(level) = heading_level(&style_name) {
        if !text.is_empty() {
            let text = if numbering.is_some() {
                let docling_level = level.saturating_sub(1).max(1);
                numbered_heading_text(&mut state.numbered_headers, docling_level, &text)
            } else {
                text
            };
            doc.push(Node::Heading { level, text });
        }
        state.list_run_base = None;
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
                    first_in_list: false,
                    text,
                    level,
                    // docling's DOCX backend passes the enumeration marker.
                    marker: Some(marker),
                    location: None,
                    dclx: None,
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
                    first_in_list: false,
                    text: format!("{marker} {text}"),
                    level,
                    marker: None,
                    location: None,
                    dclx,
                });
            }
        } else {
            // A bullet item; inline equations carry structured `<formula>` runs in
            // the DocLang overlay while Markdown keeps the flat `$…$` text. (Plain
            // formatting is left to the flat-text re-parse, which matches docling's
            // list-item rendering more closely than reconstructed runs.)
            let dclx = has_equations.then(|| ListItemDclx {
                ordered: false,
                marker: None,
                text: text.clone(),
                runs: inline_equation_runs(&eq_parts),
            });
            doc.push(Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: false,
                text,
                level,
                marker: None,
                location: None,
                dclx,
            });
        }
        return;
    }

    // A plain (non-list) paragraph ends the current list run.
    state.list_run_base = None;

    // docling emits a paragraph's images *before* its text (the `drawing_blip`
    // branch runs `_handle_pictures` then `_handle_text_elements`). Images inside
    // a textbox are skipped here — they're extracted as textbox text instead.
    // Modern DrawingML (`<a:blip>`) wins over legacy VML (`<v:imagedata>`): when a
    // paragraph has both (an `<mc:AlternateContent>` Choice + Fallback for the
    // same image) docling's `elif` counts only the blips, never the fallback.
    for image in drawing_images(p, ctx, true) {
        doc.push(Node::Picture {
            caption: None,
            image,
        });
    }

    if !text.is_empty() {
        if has_equations {
            // A body paragraph with inline equations becomes an InlineGroup whose
            // equation fragments are `<formula>` runs; Markdown/JSON keep the flat
            // `$…$` text.
            let runs = inline_equation_runs(&eq_parts);
            doc.push(docling_core::inline_paragraph_node(text, runs, false));
        } else if rich {
            // In a rich cell each format segment is its own block (joined with
            // blank lines, i.e. double spaces once flattened into the cell).
            let mut runs = Vec::new();
            collect_run_tuples(p, Fmt::default(), None, ctx, &mut runs);
            for seg in run_segments(runs) {
                doc.push(Node::Paragraph { text: seg });
            }
        } else {
            // A body paragraph with inline formatting becomes an InlineGroup so
            // DocLang carries the structure; Markdown/JSON still see `text`. The
            // docx body is flat (docling parents these on the body group), so the
            // group is always wrapped.
            let mut tuples = Vec::new();
            collect_run_tuples(p, Fmt::default(), None, ctx, &mut tuples);
            let runs = run_inline_runs(tuples);
            doc.push(docling_core::inline_paragraph_node(text, runs, false));
        }
    } else if !rich && !has_equations && !has_drawing(p) {
        // docling emits an empty text item for a blank body paragraph
        // (`skip_empty_text=False`); paragraphs carrying a drawing skip it. This
        // is DocLang/JSON-only — Markdown drops empty paragraphs.
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
/// modern `<a:blip r:embed>` win over legacy `<v:imagedata r:id>`. With
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
    node.descendants()
        .filter(|n| n.has_tag_name("imagedata") && keep(*n))
        .map(|d| attr(d, "id").and_then(|id| ctx.images.get(id)).cloned())
        .collect()
}

fn in_textbox(n: XmlNode) -> bool {
    n.ancestors()
        .any(|a| a.has_tag_name("txbxContent") || a.has_tag_name("textbox"))
}

/// An attribute by *local* name, ignoring its namespace (OOXML attributes are
/// namespaced, e.g. `w:val`, which roxmltree's bare `attribute()` won't match).
fn attr<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

/// Prepend a multilevel heading number (`1`, `1.1`, `2`, …) to a numbered
/// heading at docling level `level` (1-indexed), maintaining per-level counters.
/// Mirrors docling's `_add_heading` numbering: bump this level, zero deeper
/// consecutive levels, then walk up prefixing each ancestor's counter (filling a
/// skipped `0` ancestor with `1`, the "no empty sublevels" rule).
fn numbered_heading_text(headers: &mut HashMap<u8, u64>, level: u8, text: &str) -> String {
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

/// Markdown heading level from a style name. docling renders a heading at level
/// `N` as `#`×(N+1), and a Title as `#`; so "heading 1" → `##`, Title → `#`.
fn heading_level(style_name: &str) -> Option<u8> {
    let lower = style_name.to_ascii_lowercase();
    if lower == "title" {
        return Some(1);
    }
    let rest = lower.strip_prefix("heading")?.trim();
    rest.parse::<u8>().ok().map(|n| n.saturating_add(1))
}

/// `(numId, ilvl)` for an element carrying explicit list numbering. A `numId`
/// of 0 means "no list" in OOXML and yields `None`.
fn num_pr(p: XmlNode) -> Option<(String, i64)> {
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
fn child_elements<'a, 'i>(n: XmlNode<'a, 'i>) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    n.children().filter(XmlNode::is_element)
}

/// Strip a leading checkbox glyph (matches docling's `_clean_checkbox_symbols`).
fn clean_checkbox_symbols(text: &str) -> String {
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
        groups.push((group_text.trim().to_string(), last_format, None));
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

/// The structured [`InlineRun`]s for a body paragraph — one per format group,
/// carrying the formatting DocLang needs (including underline/sub/superscript,
/// which have no Markdown marker). The hyperlink is dropped (DocLang inline
/// scope keeps only the anchor text).
fn run_inline_runs(runs: Vec<(String, Fmt, Option<String>)>) -> Vec<InlineRun> {
    run_groups(runs)
        .into_iter()
        .filter(|(t, _, _)| !t.is_empty())
        .map(|(t, f, _)| f.to_inline_run(&t))
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
                for t in child
                    .descendants()
                    .filter(|n| n.has_tag_name("t") && !in_math(*n))
                {
                    if let Some(txt) = t.text() {
                        parts.push(EqPart::Text(txt.to_string()));
                    }
                }
            }
        }
    } else {
        for node in p.descendants() {
            if node.has_tag_name("t") && !in_math(node) {
                if let Some(txt) = node.text() {
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

#[derive(Clone, Copy, Default, PartialEq)]
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
            let text: String = child_elements(child)
                .map(|n| match n.tag_name().name() {
                    "t" => n.text().unwrap_or("").to_string(),
                    "br" | "cr" => "\n".to_string(),
                    "tab" => "\t".to_string(),
                    _ => String::new(),
                })
                .collect();
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
    let rows: Vec<XmlNode> = tbl.children().filter(|n| n.has_tag_name("tr")).collect();
    let num_cols = rows
        .iter()
        .map(|r| {
            r.children()
                .filter(|n| n.has_tag_name("tc"))
                .map(|tc| grid_span(tc))
                .sum::<usize>()
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
    for (ri, row) in rows.iter().enumerate() {
        let mut ci = 0usize;
        for tc in row.children().filter(|n| n.has_tag_name("tc")) {
            while ci < num_cols && !grid[ri][ci].is_empty() {
                ci += 1;
            }
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
            ci += span;
        }
    }
    Some(Table {
        rows: grid,
        location: None,
        structure: None,
        cell_blocks: any_rich.then_some(blocks),
    })
}

fn grid_span(tc: XmlNode) -> usize {
    tc.descendants()
        .find(|n| n.has_tag_name("gridSpan"))
        .and_then(|n| attr(n, "val"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
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
    sub.export_to_markdown().trim().to_string()
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
/// formatting markers.
fn plain_paragraph_text(p: XmlNode) -> String {
    let mut out = String::new();
    for child in child_elements(p) {
        let omaths = omaths_of(child);
        if omaths.is_empty() {
            for t in child.descendants().filter(|n| n.has_tag_name("t")) {
                out.push_str(t.text().unwrap_or(""));
            }
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

/// From `styles.xml`: `styleId` → display name, and `styleId` → list numbering
/// `(numId, ilvl)` for styles that define numbering (e.g. "List Bullet").
type StyleMaps = (HashMap<String, String>, HashMap<String, (String, i64)>);
fn parse_styles(styles_xml: &str) -> StyleMaps {
    let mut names = HashMap::new();
    let mut nums = HashMap::new();
    let Ok(dom) = Document::parse(styles_xml) else {
        return (names, nums);
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
        if let Some(num) = num_pr(style) {
            nums.insert(id.to_string(), num);
        }
    }
    (names, nums)
}

/// One numbering level's properties (resolved `num` → `abstractNum` → `lvl`).
#[derive(Clone, Default)]
struct NumLevel {
    numbered: bool,
    start: i64,
    lvl_text: String,
}

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
            let numbered = lvl
                .children()
                .find(|n| n.has_tag_name("numFmt"))
                .and_then(|n| attr(n, "val"))
                .map(|v| v != "bullet")
                .unwrap_or(true);
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

/// The `start` value for `(numId, ilvl)`, defaulting to 1.
fn level_start(num_levels: &HashMap<(String, i64), NumLevel>, num_id: &str, ilvl: i64) -> i64 {
    num_levels
        .get(&(num_id.to_string(), ilvl))
        .map(|l| l.start)
        .unwrap_or(1)
}

/// Increment the counter for `(numId, ilvl)` (seeding from its `start`) and reset
/// all deeper levels — docling's `_get_list_counter`.
fn get_list_counter(
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
/// its `%N` placeholders substituted; a bare numeric template (`%1.%2.`) falls
/// back to the hierarchical `1.2.` form joining `counter[0..=ilvl]`.
fn build_enum_marker(
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
    let lvl_text = num_levels
        .get(&(num_id.to_string(), ilvl))
        .map(|l| l.lvl_text.as_str())
        .unwrap_or("");
    let re_placeholder = cached_regex!(r"%(\d+)");
    if re_placeholder.is_match(lvl_text) {
        let stripped: String = re_placeholder.replace_all(lvl_text, "").into_owned();
        let stripped = stripped.trim_matches(|c: char| " .)(:[]".contains(c));
        if !stripped.is_empty() {
            return re_placeholder
                .replace_all(lvl_text, |caps: &regex::Captures| {
                    let lvl_idx: i64 = caps[1].parse::<i64>().unwrap_or(1) - 1;
                    counter_at(lvl_idx).to_string()
                })
                .into_owned();
        }
    }
    let parts: Vec<String> = (0..=ilvl).map(|lvl| counter_at(lvl).to_string()).collect();
    parts.join(".") + "."
}
