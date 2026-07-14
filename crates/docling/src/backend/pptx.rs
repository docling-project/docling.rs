//! PPTX (PowerPoint) backend.
//!
//! Ports docling's `MsPowerpointDocumentBackend`: each slide's shape tree is
//! walked in order, emitting titles, paragraphs, bullet/numbered lists, tables,
//! and pictures. Bullet detection follows the practical rule docling's
//! `_get_effective_list_marker` resolves to: an explicit `buNone`/`buChar`/
//! `buAutoNum` wins; otherwise body placeholders inherit a bullet from the
//! master (so they default to a list) while plain text boxes default to a
//! paragraph.

use std::collections::{HashMap, HashSet};

use docling_core::{DoclingDocument, Node, PictureImage, Table, TableStructure};
use roxmltree::{Document, Node as XmlNode};

use crate::backend::ooxml::{content_type, picture_image, resolve, Package};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct PptxBackend;

impl DeclarativeBackend for PptxBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("pptx: bad zip".into()))?;
        let mut doc = DoclingDocument::new(&source.name);

        let presentation = pkg
            .read("ppt/presentation.xml")
            .ok_or_else(|| ConversionError::Parse("pptx: no presentation.xml".into()))?;
        let slide_size = slide_size(&presentation);
        let content_types = pkg.read("[Content_Types].xml").unwrap_or_default();
        let rid_to_part: HashMap<String, String> = pkg
            .rels_for("ppt/presentation.xml")
            .iter()
            .map(|r| (r.id.clone(), resolve("ppt", &r.target)))
            .collect();
        let authors = comment_authors(&mut pkg);

        for (slide_ix, rid) in slide_rids(&presentation).into_iter().enumerate() {
            let Some(part) = rid_to_part.get(&rid).cloned() else {
                continue;
            };
            // Placeholder geometry inherited from the slide's layout → master,
            // for shapes that carry no own `<a:xfrm>` (python-pptx resolves
            // `shape.left/top/...` up this chain).
            let phmap = slide_placeholders(&mut pkg, &part);
            // Relationship ids whose target is a real, image-typed part — only
            // these become pictures (linked/missing/wrong-type blips are dropped,
            // matching python-pptx + PIL).
            let dir = part
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or("")
                .to_string();
            let mut valid_imgs: HashSet<String> = HashSet::new();
            let mut images: HashMap<String, PictureImage> = HashMap::new();
            for r in pkg.rels_for(&part) {
                let p = resolve(&dir, &r.target);
                if !content_type(&content_types, &p)
                    .map(|ct| ct.starts_with("image/"))
                    .unwrap_or(false)
                {
                    continue;
                }
                valid_imgs.insert(r.id.clone());
                // Decodable images carry their pixels for export; the rest still
                // emit a placeholder picture.
                if let Some(img) = pkg.read_bytes(&p).and_then(|b| picture_image(&p, b)) {
                    images.insert(r.id, img);
                }
            }

            let Some(xml) = pkg.read(&part) else {
                continue;
            };
            let Ok(slide) = Document::parse(&xml) else {
                continue;
            };
            if let Some(tree) = descendant(slide.root_element(), "spTree") {
                for shape in tree.children().filter(XmlNode::is_element) {
                    handle_shape(shape, &valid_imgs, &images, slide_size, &phmap, &mut doc);
                }
            }
            // Speaker notes are slide content (docling gives them a zero-bbox
            // provenance on the slide's page), so they precede the page break.
            slide_notes(&mut pkg, &part, &dir, &mut doc);
            // DocLang page break: docling's serializer places each slide
            // boundary's break *after* the following slide's content (every
            // slide beyond the first trails one — same artifact as XLSX
            // sheets, see the xlsx module docs).
            if slide_ix > 0 {
                doc.push(Node::PageBreak);
            }
            // Review comments (`p:cm`) carry no provenance, so they serialize
            // after the page break, matching docling's comment_section groups.
            slide_comments(&mut pkg, &part, &dir, &authors, &mut doc);
        }
        Ok(doc)
    }
}

/// Author id → (name, initials) from `ppt/commentAuthors.xml`.
fn comment_authors(pkg: &mut Package) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    let Some(xml) = pkg.read("ppt/commentAuthors.xml") else {
        return map;
    };
    let Ok(doc) = Document::parse(&xml) else {
        return map;
    };
    for a in doc.descendants().filter(|n| n.has_tag_name("cmAuthor")) {
        map.insert(
            a.attribute("id").unwrap_or("").to_string(),
            (
                a.attribute("name").unwrap_or("").to_string(),
                a.attribute("initials").unwrap_or("").to_string(),
            ),
        );
    }
    map
}

/// Emit a slide's speaker notes: python-pptx's `notes_text_frame.text` (the
/// body placeholder's paragraphs joined with newlines, soft breaks as `\v`),
/// stripped, as one notes-layer text with a zero-bbox location.
fn slide_notes(pkg: &mut Package, part: &str, dir: &str, doc: &mut DoclingDocument) {
    for r in pkg.rels_for(part) {
        if !r.rel_type.ends_with("/notesSlide") {
            continue;
        }
        let p = resolve(dir, &r.target);
        let Some(xml) = pkg.read(&p) else {
            continue;
        };
        let Ok(ndoc) = Document::parse(&xml) else {
            continue;
        };
        let body = ndoc.descendants().find(|n| {
            n.has_tag_name("sp")
                && n.descendants()
                    .any(|d| d.has_tag_name("ph") && d.attribute("type") == Some("body"))
        });
        let Some(tx) = body.and_then(|sp| descendant(sp, "txBody")) else {
            continue;
        };
        let text = tx
            .children()
            .filter(|n| n.has_tag_name("p"))
            .map(|p| {
                let mut s = String::new();
                for child in p.children().filter(XmlNode::is_element) {
                    match child.tag_name().name() {
                        "r" | "fld" => {
                            if let Some(t) = child.children().find(|n| n.has_tag_name("t")) {
                                s.push_str(t.text().unwrap_or(""));
                            }
                        }
                        "br" => s.push('\u{b}'),
                        _ => {}
                    }
                }
                s
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if !text.is_empty() {
            doc.push(Node::Furniture {
                layer: docling_core::ContentLayer::Notes,
                inner: Box::new(Node::Located {
                    location: [0, 0, 0, 0],
                    inner: Box::new(Node::Paragraph {
                        text: text.to_string(),
                    }),
                }),
            });
        }
    }
}

/// Emit a slide's review comments as notes-layer texts, docling's format:
/// `[author: Name (IN), time: dt]: text` (either metadata part may be absent;
/// `dt` is the raw attribute string).
fn slide_comments(
    pkg: &mut Package,
    part: &str,
    dir: &str,
    authors: &HashMap<String, (String, String)>,
    doc: &mut DoclingDocument,
) {
    for r in pkg.rels_for(part) {
        if !r.rel_type.ends_with("/comments") {
            continue;
        }
        let p = resolve(dir, &r.target);
        let Some(xml) = pkg.read(&p) else {
            continue;
        };
        let Ok(cdoc) = Document::parse(&xml) else {
            continue;
        };
        for cm in cdoc.descendants().filter(|n| n.has_tag_name("cm")) {
            let text = cm
                .children()
                .find(|n| n.has_tag_name("text"))
                .and_then(|t| t.text())
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                continue;
            }
            let mut meta = Vec::new();
            if let Some((name, initials)) = authors.get(cm.attribute("authorId").unwrap_or("")) {
                if !name.is_empty() {
                    let mut a = format!("author: {name}");
                    if !initials.is_empty() {
                        a.push_str(&format!(" ({initials})"));
                    }
                    meta.push(a);
                }
            }
            if let Some(dt) = cm.attribute("dt").filter(|d| !d.is_empty()) {
                meta.push(format!("time: {dt}"));
            }
            let full = if meta.is_empty() {
                text.to_string()
            } else {
                format!("[{}]: {}", meta.join(", "), text)
            };
            doc.push(Node::Furniture {
                layer: docling_core::ContentLayer::Notes,
                inner: Box::new(Node::Paragraph { text: full }),
            });
        }
    }
}

/// Ordered slide relationship ids from `<p:sldIdLst>`.
fn slide_rids(presentation: &str) -> Vec<String> {
    let Ok(doc) = Document::parse(presentation) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.has_tag_name("sldId"))
        .filter_map(|n| {
            n.attributes()
                .find(|a| a.name() == "id" && a.namespace().is_some())
                .map(|a| a.value().to_string())
        })
        .collect()
}

fn handle_shape(
    shape: XmlNode,
    valid_imgs: &HashSet<String>,
    images: &HashMap<String, PictureImage>,
    slide_size: (i64, i64),
    phmap: &PhMap,
    doc: &mut DoclingDocument,
) {
    match shape.tag_name().name() {
        "grpSp" => {
            for child in shape.children().filter(XmlNode::is_element) {
                handle_shape(child, valid_imgs, images, slide_size, phmap, doc);
            }
        }
        "graphicFrame" => {
            if let Some(tbl) = descendant(shape, "tbl") {
                if let Some(table) = parse_table(tbl) {
                    push_located(
                        doc,
                        shape_location(shape, slide_size, phmap),
                        Node::Table(table),
                    );
                }
            }
        }
        "pic" => {
            // Emit only loadable embedded images (an `r:embed` into an image part).
            let embedded = descendant(shape, "blip").and_then(|b| {
                b.attributes()
                    .find(|a| a.name() == "embed")
                    .map(|a| a.value().to_string())
            });
            if let Some(rid) = embedded.filter(|rid| valid_imgs.contains(rid)) {
                push_located(
                    doc,
                    shape_location(shape, slide_size, phmap),
                    Node::Picture {
                        caption: None,
                        image: images.get(&rid).cloned(),
                    },
                );
            }
        }
        "sp" => handle_text_shape(shape, shape_location(shape, slide_size, phmap), doc),
        _ => {}
    }
}

/// Slide size (EMU) from `<p:sldSz cx cy>`, defaulting to the 4:3 standard.
fn slide_size(presentation: &str) -> (i64, i64) {
    Document::parse(presentation)
        .ok()
        .and_then(|d| {
            let sz = d.descendants().find(|n| n.has_tag_name("sldSz"))?;
            Some((
                sz.attribute("cx")?.parse().ok()?,
                sz.attribute("cy")?.parse().ok()?,
            ))
        })
        .unwrap_or((9144000, 6858000))
}

/// docling's `_generate_prov`: the shape's bbox normalized to DocLang's 0–511
/// grid. The bbox is bottom-left origin (so y is flipped). The shape's geometry
/// is its own `<a:xfrm>` if present, else the placeholder box it inherits from
/// the slide layout/master (python-pptx `shape.left/top/...`). A shape with no
/// resolvable geometry — or `left == 0` (docling's `if shape.left:` truthiness)
/// — takes the whole slide.
fn shape_location(shape: XmlNode, (w, h): (i64, i64), phmap: &PhMap) -> [u16; 4] {
    let geom = xfrm_geom(shape).or_else(|| inherited_geom(shape, phmap));
    let (left, top, cw, ch) = match geom {
        Some([x, y, cx, cy]) if x != 0 => (x, y, cx, cy),
        _ => (0, 0, w, h),
    };
    let n = |v: i64, dim: i64| -> u16 {
        if dim == 0 {
            return 0;
        }
        ((512.0 * v as f64 / dim as f64).round() as i64).clamp(0, 511) as u16
    };
    [
        n(left, w),
        n(h - (top + ch), h),
        n(left + cw, w),
        n(h - top, h),
    ]
}

/// A shape/placeholder's own transform `[x, y, cx, cy]` in EMU, from its
/// `<a:xfrm>` (`<p:xfrm>` for a graphic frame — `descendant` matches either).
fn xfrm_geom(node: XmlNode) -> Option<[i64; 4]> {
    let x = descendant(node, "xfrm")?;
    let off = x.children().find(|n| n.has_tag_name("off"))?;
    let ext = x.children().find(|n| n.has_tag_name("ext"))?;
    Some([
        off.attribute("x")?.parse().ok()?,
        off.attribute("y")?.parse().ok()?,
        ext.attribute("cx")?.parse().ok()?,
        ext.attribute("cy")?.parse().ok()?,
    ])
}

/// The geometry a placeholder shape inherits from its layout/master: match its
/// `<p:ph>` by `idx` first, then by `type` (python-pptx's inheritance keys).
fn inherited_geom(shape: XmlNode, phmap: &PhMap) -> Option<[i64; 4]> {
    let ph = descendant(shape, "ph")?;
    if let Some(idx) = ph.attribute("idx") {
        if let Some(g) = phmap.by_idx.get(idx) {
            return Some(*g);
        }
    }
    if let Some(t) = ph.attribute("type") {
        if let Some(g) = phmap.by_type.get(t) {
            return Some(*g);
        }
    }
    None
}

/// Placeholder geometries a slide can inherit, keyed by `<p:ph>` `idx` and
/// `type`. The layout is consulted before the master (layout wins).
#[derive(Default)]
struct PhMap {
    by_idx: HashMap<String, [i64; 4]>,
    by_type: HashMap<String, [i64; 4]>,
}

/// Build the [`PhMap`] for a slide: its layout part (via the slide's `.rels`)
/// then that layout's master (via the layout's `.rels`). Placeholders already
/// seen (layout) are not overwritten by the master.
fn slide_placeholders(pkg: &mut Package, slide_part: &str) -> PhMap {
    let mut map = PhMap::default();
    let slide_dir = slide_part.rsplit_once('/').map_or("", |(d, _)| d);
    let Some(layout_part) = rel_target(pkg, slide_part, slide_dir, "/slideLayout") else {
        return map;
    };
    if let Some(xml) = pkg.read(&layout_part) {
        collect_placeholders(&xml, &mut map);
    }
    let layout_dir = layout_part.rsplit_once('/').map_or("", |(d, _)| d);
    if let Some(master_part) = rel_target(pkg, &layout_part, layout_dir, "/slideMaster") {
        if let Some(xml) = pkg.read(&master_part) {
            collect_placeholders(&xml, &mut map);
        }
    }
    map
}

/// Resolve the first relationship of `part` whose type ends with `suffix` to a
/// package path (against `base_dir`).
fn rel_target(pkg: &mut Package, part: &str, base_dir: &str, suffix: &str) -> Option<String> {
    pkg.rels_for(part)
        .iter()
        .find(|r| r.rel_type.ends_with(suffix))
        .map(|r| resolve(base_dir, &r.target))
}

/// Record every placeholder's own `<a:xfrm>` geometry from a layout/master part,
/// keyed by its `<p:ph>` `idx` and `type`. `or_insert` keeps the earlier source
/// (layout before master).
fn collect_placeholders(xml: &str, map: &mut PhMap) {
    let Ok(doc) = Document::parse(xml) else {
        return;
    };
    let Some(tree) = descendant(doc.root_element(), "spTree") else {
        return;
    };
    for sp in tree.children().filter(|n| n.has_tag_name("sp")) {
        let Some(ph) = descendant(sp, "ph") else {
            continue;
        };
        let Some(geom) = xfrm_geom(sp) else {
            continue;
        };
        if let Some(idx) = ph.attribute("idx") {
            map.by_idx.entry(idx.to_string()).or_insert(geom);
        }
        if let Some(t) = ph.attribute("type") {
            map.by_type.entry(t.to_string()).or_insert(geom);
        }
    }
}

/// Wrap `node` in a [`Node::Located`] carrying the shape's provenance.
fn push_located(doc: &mut DoclingDocument, location: [u16; 4], node: Node) {
    doc.push(Node::Located {
        location,
        inner: Box::new(node),
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Placeholder {
    Title,
    Subtitle,
    Body,
    TextBox,
}

fn placeholder_kind(sp: XmlNode) -> Placeholder {
    match descendant(sp, "ph") {
        None => Placeholder::TextBox,
        Some(ph) => match ph.attribute("type") {
            Some("title") | Some("ctrTitle") => Placeholder::Title,
            Some("subTitle") => Placeholder::Subtitle,
            _ => Placeholder::Body,
        },
    }
}

fn handle_text_shape(sp: XmlNode, location: [u16; 4], doc: &mut DoclingDocument) {
    let Some(tx_body) = descendant(sp, "txBody") else {
        return;
    };
    // docling skips a shape whose whole text is blank.
    let kind = placeholder_kind(sp);
    let paragraphs: Vec<XmlNode> = tx_body.children().filter(|n| n.has_tag_name("p")).collect();
    if paragraphs
        .iter()
        .all(|p| paragraph_text(*p).trim().is_empty())
    {
        return;
    }

    let mut in_list = false;
    let mut number = 0u64;
    for para in paragraphs {
        let text = paragraph_text(para);
        match list_kind(para, kind) {
            Some(numbered) => {
                if !in_list {
                    in_list = true;
                    number = 0;
                }
                let n = if numbered {
                    number += 1;
                    number
                } else {
                    0
                };
                // Each item carries its shape's `<location>` (all items of a
                // body placeholder share the one box); the location rides on the
                // item itself so consecutive items still group into one `<list>`.
                // docling passes numbered items an `"N."` enumeration marker.
                doc.push(Node::ListItem {
                    ordered: numbered,
                    number: n,
                    first_in_list: false,
                    text,
                    level: 0,
                    marker: numbered.then(|| format!("{n}.")),
                    location: Some(location),
                    dclx: None,
                    href: None,
                    layer: None,
                });
            }
            None => {
                in_list = false;
                match kind {
                    Placeholder::Title => {
                        push_located(doc, location, Node::Heading { level: 1, text })
                    }
                    // docling intends SECTION_HEADER for subtitles but a bug
                    // leaves the label as PARAGRAPH, so subtitles render as text.
                    _ => push_located(doc, location, Node::Paragraph { text }),
                }
            }
        }
    }
}

/// Whether a paragraph is a list item: `Some(true)` numbered, `Some(false)`
/// bulleted, `None` not a list.
fn list_kind(para: XmlNode, placeholder: Placeholder) -> Option<bool> {
    if let Some(p_pr) = para.children().find(|n| n.has_tag_name("pPr")) {
        if p_pr.children().any(|n| n.has_tag_name("buNone")) {
            return None;
        }
        if p_pr.children().any(|n| n.has_tag_name("buAutoNum")) {
            return Some(true);
        }
        if p_pr.children().any(|n| n.has_tag_name("buChar")) {
            return Some(false);
        }
    }
    // No explicit marker: body placeholders inherit a bullet from the master.
    match placeholder {
        Placeholder::Body => Some(false),
        _ => None,
    }
}

/// Concatenate a paragraph's run text; line breaks (`<a:br>`) become spaces.
fn paragraph_text(para: XmlNode) -> String {
    let mut out = String::new();
    for child in para.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "r" | "fld" => {
                if let Some(t) = child.children().find(|n| n.has_tag_name("t")) {
                    out.push_str(t.text().unwrap_or(""));
                }
            }
            "br" => out.push(' '),
            _ => {}
        }
    }
    out
}

fn parse_table(tbl: XmlNode) -> Option<Table> {
    let rows: Vec<XmlNode> = tbl.children().filter(|n| n.has_tag_name("tr")).collect();
    let num_cols = rows
        .iter()
        .map(|r| r.children().filter(|n| n.has_tag_name("tc")).count())
        .max()
        .unwrap_or(0);
    if rows.is_empty() || num_cols == 0 {
        return None;
    }
    // `<a:tblPr firstRow="1">` marks the first row as a header band (→ `<ched/>`).
    let first_row_header = descendant(tbl, "tblPr")
        .and_then(|p| p.attribute("firstRow"))
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    // Every grid position has its own `<a:tc>` (merge continuations included), so
    // the column index maps directly. A `hMerge` continuation is a horizontal
    // span (`<lcel/>`), a `vMerge` continuation a vertical span (`<ucel/>`); the
    // structure overlay carries those for DocLang. The `rows` text grid keeps
    // docling's Markdown/JSON behaviour: the origin cell's text is replicated
    // across its whole `rowSpan × gridSpan` region.
    let mut grid = vec![vec![String::new(); num_cols]; rows.len()];
    let mut col_continuation = vec![vec![false; num_cols]; rows.len()];
    let mut row_continuation = vec![vec![false; num_cols]; rows.len()];
    for (ri, row) in rows.iter().enumerate() {
        let cells: Vec<XmlNode> = row.children().filter(|n| n.has_tag_name("tc")).collect();
        for (ci, tc) in cells.iter().enumerate().take(num_cols) {
            let h = tc.attribute("hMerge").is_some();
            let v = tc.attribute("vMerge").is_some();
            col_continuation[ri][ci] = h;
            row_continuation[ri][ci] = v;
            // Continuation cells carry no text of their own (DocLang emits only
            // the token); the origin below fills their grid text for Markdown.
            if h || v {
                continue;
            }
            let text = cell_text(*tc);
            let span = |name: &str| -> usize {
                tc.attribute(name).and_then(|s| s.parse().ok()).unwrap_or(1)
            };
            let row_end = (ri + span("rowSpan")).min(rows.len());
            let col_end = (ci + span("gridSpan")).min(num_cols);
            for grow in grid.iter_mut().take(row_end).skip(ri) {
                for cell in grow.iter_mut().take(col_end).skip(ci) {
                    *cell = text.clone();
                }
            }
        }
    }
    let header_row = (0..rows.len())
        .map(|ri| first_row_header && ri == 0)
        .collect();
    Some(Table {
        rows: grid,
        location: None,
        structure: Some(TableStructure {
            header_row,
            col_continuation,
            row_continuation,
            row_header: Vec::new(),
            col_header: Vec::new(),
        }),
        cell_blocks: None,
    })
}

/// A table cell's text: its paragraphs joined with newlines, then trimmed
/// (matching python-pptx `cell.text.strip()`; the serializer turns `\n` into a
/// space).
fn cell_text(tc: XmlNode) -> String {
    let Some(tx_body) = descendant(tc, "txBody") else {
        return String::new();
    };
    tx_body
        .children()
        .filter(|n| n.has_tag_name("p"))
        .map(|p| paragraph_text(p))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// First descendant element with the given local tag name.
fn descendant<'a, 'input>(node: XmlNode<'a, 'input>, name: &str) -> Option<XmlNode<'a, 'input>> {
    node.descendants().find(|n| n.has_tag_name(name))
}
