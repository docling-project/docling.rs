//! PPTX (PowerPoint) backend.
//!
//! Ports docling's `MsPowerpointDocumentBackend`: each slide's shape tree is
//! walked in order, emitting titles, paragraphs, bullet/numbered lists, tables,
//! and pictures. Bullet detection follows docling's `_get_effective_list_marker`:
//! the paragraph's own `a:pPr` marker wins (`buNone`/`buChar`/`buAutoNum`/
//! `buBlip`), then the owning shape's `a:lstStyle` level properties for the
//! paragraph's level supply an inherited one (#406); a placeholder then reads
//! its layout placeholder's `a:lstStyle` and, through it, the master's
//! `p:txStyles` (#627, docling#4581); with no marker anywhere an indented
//! paragraph is a list item (docling's `level > 0` fallback) and any other
//! stays a paragraph.
//!
//! The walk builds two things at once. The flat [`Node`] stream is what
//! Markdown / DocLang / LaTeX render. The JSON export reads docling's item
//! tree instead ([`docling_core::tree::ItemTree`], filled here as
//! `MsPowerpointDocumentBackend._walk_linear` would): one `chapter` group per
//! slide, a `paragraph`/`title` text or a `list` group of `list_item`s per
//! text shape, tables with only their non-empty cells, pictures with the
//! file's dpi, charts as classified pictures with a caption, speaker notes
//! and `comment_section` groups on the `notes` layer — every item carrying
//! upstream's provenance verbatim (the shape's EMU box tagged `BOTTOMLEFT`,
//! a `charspan` over the item's text) and numbered in creation order. The
//! slides convert in parallel; each one's fragment is merged in slide order
//! with [`ItemTree::append`], which renumbers it as if built sequentially.

use std::collections::{HashMap, HashSet};

use docling_core::tree::{ItemTree, ListMeta, TreeKind, TreeProv};
use docling_core::{
    ContentLayer, DoclingDocument, Node, PictureDescription, PictureImage, Table, TableCell,
    TableStructure,
};
use rayon::prelude::*;
use roxmltree::{Document, Node as XmlNode};

use crate::backend::docx::format_enum_counter;
use crate::backend::ooxml::{
    content_type, image_dpi, is_metafile, picture_image, resolve, Package,
};
use crate::backend::pptx_chart::{self, PptxChart};
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

        // Slides are independent, so they convert in parallel — each worker
        // clones the package (cheap: shared bytes + zip central directory)
        // and builds its fragment; the ordered collect keeps output identical
        // to the sequential walk.
        let slides: Vec<(usize, String)> = slide_rids(&presentation)
            .into_iter()
            .enumerate()
            .filter_map(|(ix, rid)| rid_to_part.get(&rid).map(|p| (ix, p.clone())))
            .collect();
        let frags: Vec<Option<SlideFrag>> = slides
            .par_iter()
            .map(|(ix, part)| {
                convert_slide(pkg.clone(), part, *ix, slide_size, &content_types, &authors)
            })
            .collect();
        let mut tree = ItemTree::default();
        for ((slide_ix, _), frag) in slides.into_iter().zip(frags) {
            let Some(SlideFrag {
                content,
                comments,
                tree: slide_tree,
            }) = frag
            else {
                continue;
            };
            tree.append(slide_tree);
            // docling records every slide as a page sized in EMU, which is
            // what its item provenance is expressed in (#402).
            doc.push(Node::PageInfo {
                page_no: slide_ix + 1,
                width: slide_size.0 as f32,
                height: slide_size.1 as f32,
            });
            // docling gives each slide a `chapter` group named `slide-{0-based}`
            // and hangs everything the slide holds off it (#402), so a note or
            // a caption belongs to its slide rather than to the document body.
            // Groups are transparent to Markdown and DocLang, so only the JSON
            // gains the structure.
            doc.push(Node::Group {
                label: "chapter".into(),
                name: Some(format!("slide-{slide_ix}")),
                layer: None,
                children: content,
            });
            // DocLang page break: docling's serializer places each slide
            // boundary's break *after* the following slide's content (every
            // slide beyond the first trails one — same artifact as XLSX
            // sheets, see the xlsx module docs).
            if slide_ix > 0 {
                doc.push(Node::PageBreak);
            }
            // Review comments (`p:cm`) carry no provenance, so they serialize
            // after the page break, matching docling's comment_section groups.
            doc.nodes.extend(comments);
        }
        // The JSON serializes docling's item tree (see the module docs); the
        // flat nodes above stay the source for every other serializer.
        doc.tree = Some(tree);
        Ok(doc)
    }
}

/// One converted slide: its flat content nodes (shapes + speaker notes), its
/// review-comment nodes, and its fragment of docling's item tree (the slide
/// group with everything under it, then the slide's `comment_section`
/// groups as body children — upstream's creation order within a slide).
struct SlideFrag {
    content: Vec<Node>,
    comments: Vec<Node>,
    tree: ItemTree,
}

/// What every shape handler on a slide reads.
struct SlideCtx<'a> {
    /// Relationship ids whose target is an image-typed part.
    valid_imgs: &'a HashSet<String>,
    /// The decodable ones, with their pixels.
    images: &'a HashMap<String, PictureImage>,
    /// The undecodable ones that are Windows metafiles: docling keeps those
    /// as payload-less pictures and drops any other undecodable image.
    metafiles: &'a HashSet<String>,
    /// Native chart parts by relationship id (#756).
    charts: &'a HashMap<String, PptxChart>,
    /// Every relationship target of the slide part by id, as written — what
    /// a shape's `a:hlinkClick` resolves through (#767).
    targets: &'a HashMap<String, String>,
    slide_size: (i64, i64),
    phmap: &'a PhMap,
    /// 1-based slide number — docling's `page_no`.
    page_no: usize,
}

/// What every shape handler on a slide writes: the flat nodes and the tree
/// fragment, whose `slide` group every body item hangs off.
struct SlideOut {
    doc: DoclingDocument,
    tree: ItemTree,
    slide: usize,
}

impl SlideOut {
    /// How much has been written so far — the tree items and the flat nodes
    /// — for [`move_to_layer`](Self::move_to_layer).
    fn mark(&self) -> (usize, usize) {
        (self.tree.items.len(), self.doc.nodes.len())
    }

    /// Move what was written since `mark` from the layers in `from` (`None`
    /// = body) to `layer`, as docling re-tags `item.content_layer`: each tree
    /// item, and each flat node — a list item through its own layer (so a
    /// list stays one list), a furniture node by re-tagging it, any other
    /// body node by wrapping it in [`Node::Furniture`]. Items on a layer not
    /// in `from` (speaker notes, comments) keep it.
    fn move_to_layer(
        &mut self,
        (items, nodes): (usize, usize),
        layer: ContentLayer,
        from: &[Option<ContentLayer>],
    ) {
        for item in &mut self.tree.items[items..] {
            if from.contains(&item.layer) {
                item.layer = Some(layer);
            }
        }
        for node in &mut self.doc.nodes[nodes..] {
            match node {
                Node::Furniture { layer: l, .. } => {
                    if from.contains(&Some(*l)) {
                        *l = layer;
                    }
                }
                Node::ListItem { layer: l, .. } => {
                    if from.contains(l) {
                        *l = Some(layer);
                    }
                }
                _ if from.contains(&None) => {
                    let inner = std::mem::replace(node, Node::PageBreak);
                    *node = Node::Furniture {
                        layer,
                        inner: Box::new(inner),
                    };
                }
                _ => {}
            }
        }
    }
}

/// Convert one slide part, or `None` when the part is absent or not parsable
/// XML (such a slide contributes nothing — not even a page break).
fn convert_slide(
    mut pkg: Package,
    part: &str,
    slide_ix: usize,
    slide_size: (i64, i64),
    content_types: &str,
    authors: &HashMap<String, (String, String)>,
) -> Option<SlideFrag> {
    // Placeholder geometry inherited from the slide's layout → master,
    // for shapes that carry no own `<a:xfrm>` (python-pptx resolves
    // `shape.left/top/...` up this chain).
    let phmap = slide_placeholders(&mut pkg, part);
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
    let mut metafiles: HashSet<String> = HashSet::new();
    // Native charts (docling PR #3794, every plot since docling#4773): each
    // chart part parsed up front as python-pptx reads it, keyed by its
    // relationship id — see `pptx_chart`.
    let mut charts: HashMap<String, PptxChart> = HashMap::new();
    let rels = pkg.rels_for(part);
    let targets: HashMap<String, String> = rels
        .iter()
        .map(|r| (r.id.clone(), r.target.clone()))
        .collect();
    for r in rels {
        let p = resolve(&dir, &r.target);
        if r.rel_type.ends_with("/chart") {
            if let Some(chart) = pkg.read(&p).as_deref().and_then(pptx_chart::parse) {
                charts.insert(r.id.clone(), chart);
            }
            continue;
        }
        if !content_type(content_types, &p)
            .map(|ct| ct.starts_with("image/"))
            .unwrap_or(false)
        {
            continue;
        }
        valid_imgs.insert(r.id.clone());
        // Decodable images carry their pixels for export; the rest still
        // emit a placeholder picture.
        if let Some(bytes) = pkg.read_bytes(&p) {
            let is_meta = is_metafile(&bytes);
            match picture_image(&p, bytes) {
                Some(img) => {
                    images.insert(r.id, img);
                }
                None if is_meta => {
                    metafiles.insert(r.id);
                }
                None => {}
            }
        }
    }

    let xml = pkg.read(part)?;
    // `mc:AlternateContent` settled before parsing, as the DOCX backends do
    // (`mc.rs`): PowerPoint stores a text box holding an equation as an
    // `a14` Choice with a picture Fallback (#575), and the Choice is read.
    // python-pptx iterates only the known shape elements, so docling drops
    // every such block — equations, and any other shape behind a Choice,
    // whose Fallback is kept here; a deliberate divergence (the content is
    // what PowerPoint and anydoc show).
    let xml = super::mc::resolve_alternate_content(&xml);
    let slide = Document::parse(&xml).ok()?;
    let page_no = slide_ix + 1;
    let ctx = SlideCtx {
        valid_imgs: &valid_imgs,
        images: &images,
        metafiles: &metafiles,
        charts: &charts,
        targets: &targets,
        slide_size,
        phmap: &phmap,
        page_no,
    };
    let mut out = SlideOut {
        doc: DoclingDocument::new("slide"),
        tree: ItemTree::default(),
        slide: 0,
    };
    // docling's `slide-{0-based}` chapter group, created before the page.
    out.slide = out.tree.add(
        None,
        None,
        TreeKind::Group {
            label: "chapter".into(),
            name: format!("slide-{slide_ix}"),
        },
    );
    if let Some(tree) = descendant(slide.root_element(), "spTree") {
        for shape in shapes_by_position(tree, &phmap) {
            handle_shape(shape, &ctx, &mut out);
        }
    }
    // Speaker notes are slide content (docling gives them a zero-bbox
    // provenance on the slide's page), so they precede the page break.
    slide_notes(&mut pkg, part, &dir, page_no, &mut out);
    // docling#4647: PowerPoint does not present a slide marked `show="0"`
    // (Hide Slide), so its content — the slide's chapter group, its body and
    // its furniture — moves to the invisible layer, like a hidden Excel
    // sheet: available, but out of the default exports. Speaker notes keep
    // the notes layer.
    if matches!(
        slide.root_element().attribute("show"),
        Some("0") | Some("false")
    ) {
        out.move_to_layer(
            (0, 0),
            ContentLayer::Invisible,
            &[None, Some(ContentLayer::Furniture)],
        );
    }
    let mut comments = DoclingDocument::new("comments");
    slide_comments(
        &mut pkg,
        part,
        &dir,
        slide_ix,
        authors,
        &mut comments,
        &mut out.tree,
    );
    Some(SlideFrag {
        content: out.doc.nodes,
        comments: comments.nodes,
        tree: out.tree,
    })
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
fn slide_notes(pkg: &mut Package, part: &str, dir: &str, page_no: usize, out: &mut SlideOut) {
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
            out.doc.push(Node::Furniture {
                layer: ContentLayer::Notes,
                inner: Box::new(Node::Located {
                    location: [0, 0, 0, 0],
                    inner: Box::new(Node::Paragraph {
                        text: text.to_string(),
                    }),
                }),
            });
            // docling: a `text` item on the notes layer under the slide, with
            // a zero `BoundingBox()` — top-left origin, unlike the shapes.
            out.tree.add_with_prov(
                Some(out.slide),
                Some(ContentLayer::Notes),
                text_kind("text", text),
                TreeProv {
                    page_no,
                    bbox: [0.0; 4],
                    bottom_left: false,
                    charspan: [0, text.chars().count()],
                },
            );
        }
    }
}

/// A plain text item of the tree: docling's `add_text(label, text)`.
fn text_kind(label: &str, text: &str) -> TreeKind {
    linked_text_kind(label, text, None)
}

/// A text item of the tree carrying its shape's hyperlink, if any.
fn linked_text_kind(label: &str, text: &str, hyperlink: Option<&str>) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting: None,
        hyperlink: hyperlink.map(str::to_string),
        level: None,
        list: None,
    }
}

/// The link on a whole shape — docling's `_shape_hyperlink` (docling#4787,
/// 2.139, #767): the `a:hlinkClick` under the shape's `p:cNvPr`, resolved
/// through the slide's relationships. Only python-pptx's `HYPERLINK` (no
/// `action` attribute) and `OPEN_FILE` (`ppaction://hlinkfile`) actions
/// count: a jump to another slide also carries a relationship target
/// (`slide2.xml`), but nothing a reader can follow. A target without a
/// scheme is kept as a path (`appendix/notes.docx`), an absolute one
/// normalized like pydantic's `AnyUrl` and dropped when malformed — the
/// rules the ODF backend's links follow.
fn shape_hyperlink(shape: XmlNode, ctx: &SlideCtx) -> Option<String> {
    let click = shape
        .children()
        .filter(XmlNode::is_element)
        .flat_map(|nv| nv.children().filter(|c| c.has_tag_name("cNvPr")))
        .find_map(|pr| pr.children().find(|c| c.has_tag_name("hlinkClick")))?;
    if let Some(action) = click.attribute("action") {
        // python-pptx's `action_verb`: the URL before any `?`, past its
        // 11-character `ppaction://` prefix.
        let verb = action
            .split('?')
            .next()
            .unwrap_or("")
            .get(11..)
            .unwrap_or("");
        if verb != "hlinkfile" {
            return None;
        }
    }
    let id = click
        .attributes()
        .find(|a| a.name() == "id")
        .map(|a| a.value())
        .filter(|id| !id.is_empty())?;
    let address = ctx.targets.get(id).filter(|t| !t.is_empty())?;
    super::odf::hyperlink_from_href(Some(address))
}

/// A flat node's text for an item carrying its shape's link: the whole text
/// as a Markdown link, as docling-core serializes an item's `hyperlink`.
fn linked_text(text: &str, link: Option<&str>) -> String {
    match link {
        Some(url) if !text.is_empty() => docling_core::hyperlink::markdown_link(text, url),
        _ => text.to_string(),
    }
}

/// Emit a slide's review comments as notes-layer texts, docling's format:
/// `[author: Name (IN), time: dt]: text` (either metadata part may be absent;
/// `dt` is the raw attribute string). In the tree each one is a
/// `comment_section` group named `comment-slide{page}-{idx}` (the comment's
/// `idx`, else the 0-based slide index) under the body, holding the note.
fn slide_comments(
    pkg: &mut Package,
    part: &str,
    dir: &str,
    slide_ix: usize,
    authors: &HashMap<String, (String, String)>,
    doc: &mut DoclingDocument,
    tree: &mut ItemTree,
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
                layer: ContentLayer::Notes,
                inner: Box::new(Node::Paragraph { text: full.clone() }),
            });
            let idx = cm
                .attribute("idx")
                .unwrap_or(&slide_ix.to_string())
                .to_string();
            let group = tree.add(
                None,
                Some(ContentLayer::Notes),
                TreeKind::Group {
                    label: "comment_section".into(),
                    name: format!("comment-slide{}-{idx}", slide_ix + 1),
                },
            );
            tree.add(
                Some(group),
                Some(ContentLayer::Notes),
                text_kind("text", &full),
            );
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

#[allow(clippy::too_many_arguments)]
/// Order a shape collection in visual reading order (docling#3393's
/// `_iter_shapes_by_position`): resolve each shape's top/left (its own
/// `<a:xfrm>`, else the inherited placeholder geometry — python-pptx's
/// `shape.top`), sort by top (stable, so ties keep z-order), band shapes whose
/// top sits within 0.05" (45720 EMU) of the *previous* shape's top into one
/// row — adjacency is chained, so a contiguous band can span several
/// tolerance steps — and sort each row left-to-right. Shapes with no
/// resolvable geometry sort last in their original order (upstream's
/// `fallback_position`).
fn shapes_by_position<'a>(tree: XmlNode<'a, 'a>, phmap: &PhMap) -> Vec<XmlNode<'a, 'a>> {
    const ROW_TOLERANCE_EMU: i64 = 45720; // 0.05 inch

    struct Info<'a> {
        index: usize,
        node: XmlNode<'a, 'a>,
        top: i64,
        left: i64,
    }
    let mut infos: Vec<Info<'a>> = tree
        .children()
        .filter(|n| matches!(n.tag_name().name(), "sp" | "grpSp" | "pic" | "graphicFrame"))
        .enumerate()
        .map(|(index, node)| {
            let geom = xfrm_geom(node).or_else(|| inherited_geom(node, phmap));
            let (left, top) = geom.map_or((i64::MAX, i64::MAX), |g| (g[0], g[1]));
            Info {
                index,
                node,
                top,
                left,
            }
        })
        .collect();
    infos.sort_by_key(|s| (s.top, s.index));

    let mut out = Vec::with_capacity(infos.len());
    let mut row: Vec<Info<'a>> = Vec::new();
    let mut prev_top: Option<i64> = None;
    fn flush<'a>(row: &mut Vec<Info<'a>>, out: &mut Vec<XmlNode<'a, 'a>>) {
        row.sort_by_key(|s| (s.left, s.index));
        out.extend(row.drain(..).map(|s| s.node));
    }
    for info in infos {
        if prev_top.is_some_and(|p| info.top - p > ROW_TOLERANCE_EMU) {
            flush(&mut row, &mut out);
        }
        prev_top = Some(info.top);
        row.push(info);
    }
    flush(&mut row, &mut out);
    out
}

fn handle_shape(shape: XmlNode, ctx: &SlideCtx, out: &mut SlideOut) {
    if !is_hidden_shape(shape) {
        handle_shape_content(shape, ctx, out);
        return;
    }
    // docling#4733: PowerPoint does not present a shape hidden in the
    // Selection Pane. It still goes through the normal handlers, so nothing
    // is lost; whatever they add for it — for a group, for every shape inside
    // it — moves from the body to the invisible layer.
    let mark = out.mark();
    handle_shape_content(shape, ctx, out);
    out.move_to_layer(mark, ContentLayer::Invisible, &[None]);
}

/// Whether PowerPoint's Selection Pane hides the shape — docling's
/// `_is_hidden_shape`: `hidden` = `1`/`true` on the `p:cNvPr` that every
/// shape kind carries under its non-visual properties (`./*/p:cNvPr`).
fn is_hidden_shape(shape: XmlNode) -> bool {
    shape.children().filter(XmlNode::is_element).any(|nv| {
        nv.children().any(|c| {
            c.has_tag_name("cNvPr") && matches!(c.attribute("hidden"), Some("1") | Some("true"))
        })
    })
}

fn handle_shape_content(shape: XmlNode, ctx: &SlideCtx, out: &mut SlideOut) {
    let location = shape_location(shape, ctx.slide_size, ctx.phmap);
    match shape.tag_name().name() {
        "grpSp" => {
            // Group children re-sort in visual order too (docling#3393); their
            // coordinates share the group's child space, so relative order is
            // well-defined. Groups carry no placeholder geometry — the phmap
            // lookup just never fires for them.
            for child in shapes_by_position(shape, ctx.phmap) {
                handle_shape(child, ctx, out);
            }
        }
        "graphicFrame" => {
            if let Some(tbl) = descendant(shape, "tbl") {
                if let Some(table) = parse_table(tbl) {
                    // docling keeps only the cells with text (`_handle_tables`)
                    // and skips a table with none at all.
                    let cells = tree_table_cells(tbl);
                    if !cells.is_empty() {
                        out.tree.add_with_prov(
                            Some(out.slide),
                            None,
                            TreeKind::Table {
                                footnotes: Vec::new(),
                                table: Table {
                                    rows: table.rows.clone(),
                                    cells: Some(cells),
                                    ..Table::default()
                                },
                                rich_cells: Vec::new(),
                                captions: Vec::new(),
                            },
                            shape_prov(shape, ctx, 0),
                        );
                    }
                    push_located(&mut out.doc, location, Node::Table(table));
                }
            } else if let Some(chart) = descendant(shape, "chart")
                .and_then(|c| c.attributes().find(|a| a.name() == "id"))
                .and_then(|a| ctx.charts.get(a.value()))
            {
                // A native chart frame (docling PR #3794): one prediction
                // per plot type (the back-most plot's first) + the data grid,
                // chart title as the caption. docling adds the caption to the
                // slide first — with the chart's box and a charspan over the
                // title — then the picture.
                let PptxChart {
                    labels,
                    title,
                    table,
                } = chart;
                let kind = labels[0].to_string();
                let table = table.clone().unwrap_or_default();
                let caption = title.as_deref().filter(|t| !t.is_empty()).map(|t| {
                    out.tree.add_with_prov(
                        Some(out.slide),
                        None,
                        text_kind("caption", t),
                        shape_prov(shape, ctx, t.chars().count()),
                    )
                });
                out.tree.add_with_prov(
                    Some(out.slide),
                    None,
                    TreeKind::Picture {
                        captions: caption.into_iter().collect(),
                        image: None,
                        classification: Some(kind.clone()),
                        extra_classes: labels[1..].iter().map(|l| l.to_string()).collect(),
                        description: None,
                        confidence: None,
                        chart: Some(table.clone()),
                        dpi: None,
                    },
                    shape_prov(shape, ctx, 0),
                );
                out.doc.push(Node::Chart {
                    kind: kind.clone(),
                    table: table.clone(),
                    caption: title.clone(),
                    location: Some(location),
                });
            }
        }
        "pic" => {
            // Emit only loadable embedded images (an `r:embed` into an image part).
            let embedded = descendant(shape, "blip").and_then(|b| {
                b.attributes()
                    .find(|a| a.name() == "embed")
                    .map(|a| a.value().to_string())
            });
            if let Some(rid) = embedded.filter(|rid| ctx.valid_imgs.contains(rid)) {
                push_picture(shape, &rid, location, ctx, out);
            }
        }
        "sp" => {
            // docling#4989 (#1018): a shape filled with a picture (Format
            // Shape > Fill > Picture — `p:spPr/a:blipFill/a:blip`) is a
            // picture, then whatever text the shape carries. A fill that only
            // links an external file (`r:link`, no `r:embed`) or points at a
            // missing part adds no picture, as upstream's `_picture_fill_rid`
            // / `_handle_pictures` skip it; the text stays either way.
            if let Some(rid) = picture_fill_rid(shape).filter(|rid| ctx.valid_imgs.contains(*rid)) {
                push_picture(shape, rid, location, ctx, out);
            }
            handle_text_shape(shape, location, ctx, out)
        }
        _ => {}
    }
}

/// The relationship id of a `p:sp`'s embedded picture fill:
/// `p:spPr/a:blipFill/a:blip/@r:embed`, `None` for any other fill or a
/// fill without an embedded image — docling's `_picture_fill_rid`.
fn picture_fill_rid<'a>(sp: XmlNode<'a, '_>) -> Option<&'a str> {
    sp.children()
        .find(|n| n.has_tag_name("spPr"))?
        .children()
        .find(|n| n.has_tag_name("blipFill"))?
        .children()
        .find(|n| n.has_tag_name("blip"))?
        .attributes()
        .find(|a| a.name() == "embed")
        .map(|a| a.value())
        .filter(|v| !v.is_empty())
}

/// Emit the picture whose image part `rid` names for `shape` (a `p:pic`, or a
/// picture-filled `p:sp`) — docling's `_handle_pictures`.
fn push_picture(shape: XmlNode, rid: &str, location: [u16; 4], ctx: &SlideCtx, out: &mut SlideOut) {
    let image = ctx.images.get(rid);
    // The author's alt text (#676): the shape's `p:cNvPr/@descr`.
    let alt = descendant(shape, "cNvPr")
        .and_then(crate::backend::ooxml::alt_text_of)
        .map(PictureDescription::alt_text);
    // docling: PIL decodes it → a picture with the image (at the
    // file's dpi); a metafile PIL cannot → a payload-less picture;
    // anything else undecodable → no picture at all.
    if image.is_some() || ctx.metafiles.contains(rid) {
        out.tree.add_with_prov(
            Some(out.slide),
            None,
            TreeKind::Picture {
                captions: Vec::new(),
                image: image.cloned(),
                extra_classes: Vec::new(),
                classification: None,
                description: alt.clone(),
                confidence: None,
                chart: None,
                dpi: image.map(|img| pptx_dpi(&img.data)),
            },
            shape_prov(shape, ctx, 0),
        );
    }
    push_located(
        &mut out.doc,
        location,
        Node::Picture {
            caption: None,
            caption_href: None,
            image: image.cloned(),
            classification: None,
            description: alt,
            caption_parent: Default::default(),
            caption_location: None,
        },
    );
}

/// docling's `_generate_prov`: the shape's box in EMU — `shape.left/top/
/// width/height` (its own `<a:xfrm>`, else the placeholder geometry it
/// inherits), the whole slide when none resolves — as
/// `BoundingBox.from_tuple((l, t, l + w, t + h), origin=TOPLEFT)`. python-pptx
/// reports left/top from the slide's top-left corner with y growing downward,
/// which is what docling#4294 (2.130) tags it as; the box used to be tagged
/// `BOTTOMLEFT`, which read the tuple as `(l, b, r, t)` and swapped the
/// vertical edges. `charspan` covers `char_len` characters.
fn shape_prov(shape: XmlNode, ctx: &SlideCtx, char_len: usize) -> TreeProv {
    let [mut l, mut t, mut r, mut b] = shape_box(shape, ctx.slide_size, ctx.phmap);
    // `from_tuple`'s normalization: `l <= r`, and for a top-left box `t <= b`.
    if r < l {
        std::mem::swap(&mut l, &mut r);
    }
    if b < t {
        std::mem::swap(&mut t, &mut b);
    }
    TreeProv {
        page_no: ctx.page_no,
        bbox: [l, t, r, b],
        bottom_left: false,
        charspan: [0, char_len],
    }
}

/// python-pptx's `Image.dpi`: PIL's horizontal dpi rounded (half to even,
/// Python's `round`), 72 when the file records none or the value falls
/// outside 1–2048.
fn pptx_dpi(data: &[u8]) -> u32 {
    match image_dpi(data).map(f64::round_ties_even) {
        Some(d) if (1.0..=2048.0).contains(&d) => d as u32,
        _ => 72,
    }
}

/// The `TableCell`s docling's `_handle_tables` records for a table: one per
/// `<a:tc>` (merge continuations included — python-pptx's `row.cells`) whose
/// stripped text is non-empty, spanning its `rowSpan` × `gridSpan`, every
/// first-row cell a column header.
fn tree_table_cells(tbl: XmlNode) -> Vec<TableCell> {
    let mut cells = Vec::new();
    for (ri, row) in tbl.children().filter(|n| n.has_tag_name("tr")).enumerate() {
        for (ci, tc) in row.children().filter(|n| n.has_tag_name("tc")).enumerate() {
            let text = cell_text(tc);
            if text.is_empty() {
                continue;
            }
            let span = |name: &str| -> usize {
                tc.attribute(name).and_then(|s| s.parse().ok()).unwrap_or(1)
            };
            cells.push(TableCell {
                text,
                bbox: None,
                start_row: ri,
                start_col: ci,
                row_span: span("rowSpan"),
                col_span: span("gridSpan"),
                column_header: ri == 0,
                row_header: false,
                row_section: false,
            });
        }
    }
    cells
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

/// The shape's DocLang `<location>`: its box ([`shape_box`] — own or
/// inherited geometry mapped out of its groups, the whole slide when it has
/// none) on DocLang's 0–511 grid, as docling-core's
/// `_create_location_tokens_for_bbox` derives it from the provenance box:
/// top-left origin (docling#4294 tags PPTX provenance `TOPLEFT`, so y runs
/// down the slide), each edge `round(512 · v / size)` with Python's
/// half-to-even rounding, clamped to `0..=511`. The location used to flip y
/// as for a bottom-left box, which turned every shape's box upside down
/// against docling's. A shape at x = 0 EMU keeps its own box (docling#3990,
/// 2.120).
fn shape_location(shape: XmlNode, (w, h): (i64, i64), phmap: &PhMap) -> [u16; 4] {
    let [left, top, right, bottom] = shape_box(shape, (w, h), phmap);
    let n = |v: f64, dim: i64| -> u16 {
        if dim == 0 {
            return 0;
        }
        ((512.0 * v / dim as f64).round_ties_even() as i64).clamp(0, 511) as u16
    };
    [
        n(left.min(right), w),
        n(top.min(bottom), h),
        n(left.max(right), w),
        n(top.max(bottom), h),
    ]
}

/// A shape's box `[left, top, right, bottom]` on the slide, in EMU: its own
/// or inherited geometry ([`xfrm_geom`], [`inherited_geom`]) mapped out of
/// every enclosing group ([`group_to_slide`]), the whole slide when it has
/// none.
fn shape_box(shape: XmlNode, (w, h): (i64, i64), phmap: &PhMap) -> [f64; 4] {
    match xfrm_geom(shape).or_else(|| inherited_geom(shape, phmap)) {
        Some([x, y, cx, cy]) => {
            let [x, y, cx, cy] = [x, y, cx, cy].map(|v| v as f64);
            group_to_slide(shape, [x, y, x + cx, y + cy])
        }
        None => [0.0, 0.0, w as f64, h as f64],
    }
}

/// Map a grouped shape's box from its group's child coordinates to the
/// slide — docling's `_to_slide_coordinates` (docling#4859, 2.140, #766). A
/// shape inside a `p:grpSp` is positioned in the group's child space
/// (`a:chOff`/`a:chExt`), which matches the slide only until the group is
/// moved, resized or flipped; each enclosing group maps that space onto its
/// own box (`a:off`/`a:ext`), innermost first: `off + (v − chOff) · ext /
/// chExt` (scale 1 for a zero `chExt`), `flipH`/`flipV` mirroring within
/// `off..off + ext`. Group rotation is not applied, and a group whose
/// transform is incomplete or unparsable is skipped.
fn group_to_slide(shape: XmlNode, [mut l, mut t, mut r, mut b]: [f64; 4]) -> [f64; 4] {
    let mut parent = shape.parent_element();
    while let Some(group) = parent.filter(|p| p.has_tag_name("grpSp")) {
        parent = group.parent_element();
        let Some(xfrm) = group
            .children()
            .find(|n| n.has_tag_name("grpSpPr"))
            .and_then(|pr| pr.children().find(|n| n.has_tag_name("xfrm")))
        else {
            continue;
        };
        let pair = |tag: &str, a: &str, b: &str| -> Option<(f64, f64)> {
            let n = xfrm.children().find(|n| n.has_tag_name(tag))?;
            let v = |k: &str| n.attribute(k)?.parse::<i64>().ok().map(|v| v as f64);
            Some((v(a)?, v(b)?))
        };
        let (Some((off_x, off_y)), Some((ext_x, ext_y)), Some((ch_x, ch_y)), Some((ch_cx, ch_cy))) = (
            pair("off", "x", "y"),
            pair("ext", "cx", "cy"),
            pair("chOff", "x", "y"),
            pair("chExt", "cx", "cy"),
        ) else {
            continue;
        };
        let scale_x = if ch_cx != 0.0 { ext_x / ch_cx } else { 1.0 };
        let scale_y = if ch_cy != 0.0 { ext_y / ch_cy } else { 1.0 };
        l = off_x + (l - ch_x) * scale_x;
        r = off_x + (r - ch_x) * scale_x;
        t = off_y + (t - ch_y) * scale_y;
        b = off_y + (b - ch_y) * scale_y;
        let flag = |k: &str| matches!(xfrm.attribute(k), Some("1") | Some("true"));
        if flag("flipH") {
            (l, r) = (2.0 * off_x + ext_x - r, 2.0 * off_x + ext_x - l);
        }
        if flag("flipV") {
            (t, b) = (2.0 * off_y + ext_y - b, 2.0 * off_y + ext_y - t);
        }
    }
    [l, t, r, b]
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
///
/// Also the tail of docling's list-marker cascade (#627): the markers each
/// layout placeholder's list style declares, and the master's text styles.
#[derive(Default)]
struct PhMap {
    by_idx: HashMap<String, [i64; 4]>,
    by_type: HashMap<String, [i64; 4]>,
    /// The layout's placeholders in shape-tree order, by `idx` (python-pptx's
    /// default 0 when absent), each with the markers of its
    /// `a:lstStyle/a:lvl{N}pPr` — empty levels when it has no text body.
    layout_lists: Vec<(u32, LevelMarkers)>,
    /// The master's `p:txStyles`: `titleStyle`, `bodyStyle`, `otherStyle`.
    master_styles: [LevelMarkers; 3],
}

/// The marker [`declared_marker`] reads from each of the nine `a:lvl{N}pPr`
/// of a list style, indexed by paragraph level.
type LevelMarkers = [Option<Option<ListKind>>; 9];

/// What a list paragraph's effective marker declares: a bullet (`buChar`,
/// `buBlip`, or no marker on an indented paragraph) or an `a:buAutoNum`
/// with its counting scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListKind {
    Bulleted,
    Numbered(Option<AutoScheme>),
}

impl ListKind {
    fn numbered(self) -> bool {
        matches!(self, ListKind::Numbered(_))
    }
}

/// An `a:buAutoNum/@type` that counts with Latin letters or Roman numerals
/// (docling#5122, #1018 — docling's `_AUTO_NUMBER_SCHEME`): the counter as
/// the OOXML `numFmt` [`format_enum_counter`] renders, and the text around
/// it. Every other scheme (`arabicPeriod`, `arabicParenR`,
/// `circleNumDbPlain`, the East Asian ones, …) keeps the decimal `N.`, as
/// numbered DOCX lists do upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AutoScheme {
    num_fmt: &'static str,
    prefix: &'static str,
    suffix: &'static str,
}

impl AutoScheme {
    pub(super) fn parse(ty: &str) -> Option<Self> {
        let (num_fmt, rest) = [
            ("alphaLc", "lowerLetter"),
            ("alphaUc", "upperLetter"),
            ("romanLc", "lowerRoman"),
            ("romanUc", "upperRoman"),
        ]
        .into_iter()
        .find_map(|(head, fmt)| ty.strip_prefix(head).map(|rest| (fmt, rest)))?;
        let (prefix, suffix) = match rest {
            "ParenBoth" => ("(", ")"),
            "ParenR" => ("", ")"),
            "Period" => ("", "."),
            _ => return None,
        };
        Some(Self {
            num_fmt,
            prefix,
            suffix,
        })
    }
}

/// The marker PowerPoint shows for the `n`th auto-numbered paragraph —
/// docling's `_auto_number_marker`.
pub(super) fn auto_number_marker(n: u64, scheme: Option<AutoScheme>) -> String {
    match scheme {
        Some(s) => format!(
            "{}{}{}",
            s.prefix,
            format_enum_counter(n as i64, Some(s.num_fmt)),
            s.suffix
        ),
        None => format!("{n}."),
    }
}

/// Which `p:txStyles` bucket a placeholder takes its master marker from —
/// docling's `_get_master_text_style_node`: BODY/OBJECT (`body`, `obj`, or no
/// `type`, which python-pptx reads as OBJECT) → `bodyStyle`, TITLE →
/// `titleStyle`, everything else — `ctrTitle` and `subTitle` included —
/// `otherStyle`. Deliberately not [`placeholder_kind`], which lumps a centred
/// title in with the titles and every other type in with the body.
fn master_style_slot(ph: XmlNode) -> usize {
    match ph.attribute("type") {
        Some("title") => 0,
        None | Some("body") | Some("obj") => 1,
        Some(_) => 2,
    }
}

/// A `<p:ph>`'s `idx` the way python-pptx reads it: 0 when absent.
fn ph_idx(ph: XmlNode) -> u32 {
    ph.attribute("idx")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// The markers of a list-style-like element's `a:lvl{N}pPr` children
/// (`descend`: anywhere below it, as docling's `.//a:lvl{N}pPr` searches a
/// master text style; otherwise direct children, as in an `a:lstStyle`).
fn level_markers(style: Option<XmlNode>, descend: bool) -> LevelMarkers {
    let mut out = LevelMarkers::default();
    let Some(style) = style else {
        return out;
    };
    for (lvl, slot) in out.iter_mut().enumerate() {
        let tag = format!("lvl{}pPr", lvl + 1);
        let found = if descend {
            style.descendants().find(|n| n.has_tag_name(tag.as_str()))
        } else {
            style.children().find(|n| n.has_tag_name(tag.as_str()))
        };
        *slot = found.and_then(declared_marker);
    }
    out
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
        map.layout_lists = layout_list_styles(&xml);
    }
    let layout_dir = layout_part.rsplit_once('/').map_or("", |(d, _)| d);
    if let Some(master_part) = rel_target(pkg, &layout_part, layout_dir, "/slideMaster") {
        if let Some(xml) = pkg.read(&master_part) {
            collect_placeholders(&xml, &mut map);
            map.master_styles = master_text_styles(&xml);
        }
    }
    map
}

/// Every placeholder of a layout with its list-style markers, in the order
/// python-pptx's `layout.placeholders` yields them: the shape tree's direct
/// children carrying a `<p:ph>` (a placeholder inside a group is not one).
/// docling reads the first `p:txBody` below the shape, then its `a:lstStyle`.
fn layout_list_styles(xml: &str) -> Vec<(u32, LevelMarkers)> {
    let Ok(doc) = Document::parse(xml) else {
        return Vec::new();
    };
    let Some(tree) = descendant(doc.root_element(), "spTree") else {
        return Vec::new();
    };
    tree.children()
        .filter(|n| n.is_element())
        .filter_map(|shape| {
            let ph = shape
                .children()
                .find(|n| n.is_element())
                .and_then(|nv| nv.children().find(|n| n.has_tag_name("nvPr")))
                .and_then(|pr| pr.children().find(|n| n.has_tag_name("ph")))?;
            let list_style = descendant(shape, "txBody")
                .and_then(|b| b.children().find(|n| n.has_tag_name("lstStyle")));
            Some((ph_idx(ph), level_markers(list_style, false)))
        })
        .collect()
}

/// A master's `p:txStyles` markers, `[titleStyle, bodyStyle, otherStyle]`.
fn master_text_styles(xml: &str) -> [LevelMarkers; 3] {
    let Ok(doc) = Document::parse(xml) else {
        return Default::default();
    };
    let styles = descendant(doc.root_element(), "txStyles");
    ["titleStyle", "bodyStyle", "otherStyle"].map(|name| {
        let style = styles.and_then(|s| s.children().find(|n| n.has_tag_name(name)));
        level_markers(style, true)
    })
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

/// Whether a shape is a title placeholder (`title` or `ctrTitle`), whose
/// non-list text docling labels `title`. A subtitle stays a paragraph on
/// purpose: docling settled it in docling#3785 and, after briefly moving it to
/// SECTION_HEADER behind an option, reverted to that in docling#4190.
fn is_title_placeholder(ph: Option<XmlNode>) -> bool {
    matches!(
        ph.and_then(|ph| ph.attribute("type")),
        Some("title") | Some("ctrTitle")
    )
}

fn handle_text_shape(sp: XmlNode, location: [u16; 4], ctx: &SlideCtx, out: &mut SlideOut) {
    let Some(tx_body) = descendant(sp, "txBody") else {
        return;
    };
    // docling skips a shape whose whole text is blank.
    let ph = descendant(sp, "ph");
    let is_title = is_title_placeholder(ph);
    let paragraphs: Vec<XmlNode> = tx_body.children().filter(|n| n.has_tag_name("p")).collect();
    if paragraphs
        .iter()
        .all(|p| paragraph_text(*p).trim().is_empty())
    {
        return;
    }

    // docling#4746: the footer, date and slide-number placeholders are slide
    // chrome, like a PDF's page header and footer — everything the shape
    // adds goes to the furniture layer (on a hidden slide it then moves to
    // the invisible layer with the rest of the slide).
    let chrome = matches!(
        ph.and_then(|ph| ph.attribute("type")),
        Some("dt") | Some("ftr") | Some("sldNum")
    );
    let mark = out.mark();
    // docling#4787: the shape's own link goes on every item it adds.
    let link = shape_hyperlink(sp, ctx);
    let link = link.as_deref();

    // The lists still open while the frame is walked, outermost first —
    // docling's `_OpenList` stack (docling#4397, 2.130): a paragraph at a
    // deeper `a:pPr/@lvl` opens a `list` group nested under the last item of
    // the enclosing list, a shallower one pops back to its level, and every
    // group numbers its enumerated items on its own counter. A non-list
    // paragraph closes them all.
    let mut open_lists: Vec<OpenList> = Vec::new();
    for para in paragraphs {
        let text = paragraph_text(para);
        // docling's `charspan` is over the paragraph's own text, `len()` in
        // code points.
        let prov = shape_prov(sp, ctx, text.chars().count());
        match list_marker(para, Some(tx_body), ph, ctx.phmap) {
            Some(kind) => {
                let numbered = kind.numbered();
                let level = paragraph_level(para);
                while open_lists.len() > 1 && open_lists.last().is_some_and(|l| l.level > level) {
                    open_lists.pop();
                }
                let first_in_list = open_lists.is_empty();
                if open_lists.is_empty() {
                    let group = out.tree.add(Some(out.slide), None, list_group_kind());
                    open_lists.push(OpenList::new(group, level));
                } else if open_lists.last().is_some_and(|l| level > l.level) {
                    // Nested under the enclosing list's last item (falling
                    // back to its group when that item is still missing).
                    let parent = open_lists
                        .last()
                        .and_then(|l| l.last_item.or(Some(l.group)));
                    let group = out.tree.add(parent, None, list_group_kind());
                    open_lists.push(OpenList::new(group, level));
                }
                let depth = open_lists.len() - 1;
                let current = open_lists.last_mut().expect("a list is open");
                let n = if numbered {
                    // A numbered group starts from its first enumerated
                    // paragraph's own `a:buAutoNum/@startAt` (PowerPoint's
                    // "Start at"), as docling's `_get_auto_number_start`.
                    if current.counter == 0 {
                        current.counter = auto_number_start(para) - 1;
                    }
                    current.counter += 1;
                    current.counter
                } else {
                    0
                };
                current.items += 1;
                let first_enumerated = *current.first_enumerated.get_or_insert(numbered);
                // docling passes numbered items the marker of their
                // effective `a:buAutoNum/@type` (`a.`, `IV.`, `(C)`, else
                // `N.`, docling#5122) and bulleted ones an empty one.
                let marker = match kind {
                    ListKind::Numbered(scheme) => Some(auto_number_marker(n, scheme)),
                    ListKind::Bulleted => None,
                };
                let item = out.tree.add_with_prov(
                    Some(current.group),
                    None,
                    TreeKind::Text {
                        label: "list_item".into(),
                        text: text.clone(),
                        orig: None,
                        formatting: None,
                        hyperlink: link.map(str::to_string),
                        level: None,
                        list: Some(ListMeta {
                            enumerated: numbered,
                            marker: marker.clone().unwrap_or_default(),
                        }),
                    },
                    prov,
                );
                current.last_item = Some(item);
                // Each item carries its shape's `<location>` (all items of a
                // body placeholder share the one box); the location rides on the
                // item itself so consecutive items still group into one `<list>`.
                // docling-core's Markdown numbers an unmarked item by its
                // position when its group's first item is enumerated
                // (`first_item_is_enumerated`) — a bullet that joins a
                // numbered group renders `N.`, not `-`.
                let (ordered, number) = match (numbered, first_enumerated) {
                    (false, true) => (true, current.items),
                    _ => (numbered, n),
                };
                out.doc.push(Node::ListItem {
                    ordered,
                    number,
                    first_in_list,
                    text: linked_text(&text, link),
                    level: depth as u8,
                    marker,
                    location: Some(location),
                    dclx: None,
                    href: link.map(str::to_string),
                    layer: None,
                });
            }
            None if !is_title && paragraph_math(para).is_some() => {
                open_lists.clear();
                for latex in paragraph_math(para).unwrap_or_default() {
                    let prov = shape_prov(sp, ctx, latex.chars().count());
                    out.tree.add_with_prov(
                        Some(out.slide),
                        None,
                        linked_text_kind("formula", &latex, link),
                        prov,
                    );
                    out.doc.push(Node::Formula {
                        orig: latex.clone(),
                        latex,
                        location: Some(location),
                    });
                }
            }
            None => {
                open_lists.clear();
                // docling labels a title placeholder's text `title` and any
                // other non-list text `paragraph` (a `text` in Markdown terms).
                let label = if is_title { "title" } else { "paragraph" };
                out.tree.add_with_prov(
                    Some(out.slide),
                    None,
                    linked_text_kind(label, &text, link),
                    prov,
                );
                let text = linked_text(&text, link);
                let node = if is_title {
                    Node::Heading { level: 1, text }
                } else {
                    Node::Paragraph { text }
                };
                push_located(&mut out.doc, location, node);
            }
        }
    }
    if chrome {
        out.move_to_layer(mark, ContentLayer::Furniture, &[None]);
    }
}

/// A `list` group still accepting items while a text frame is walked —
/// docling's `_OpenList`.
struct OpenList {
    /// The tree id of the group items at `level` are added to.
    group: usize,
    /// The paragraph level (`a:pPr/@lvl`) of the group's items.
    level: usize,
    /// Enumerated items added so far — the `N.` counter.
    counter: u64,
    /// The most recent item, which parents any list nested below it.
    last_item: Option<usize>,
    /// Items added so far (a nested group hangs off an item, not the group).
    items: u64,
    /// Whether the group's first item is enumerated, once it has one.
    first_enumerated: Option<bool>,
}

impl OpenList {
    fn new(group: usize, level: usize) -> Self {
        Self {
            group,
            level,
            counter: 0,
            last_item: None,
            items: 0,
            first_enumerated: None,
        }
    }
}

fn list_group_kind() -> TreeKind {
    TreeKind::Group {
        label: "list".into(),
        name: "list".into(),
    }
}

/// The marker a properties element (`a:pPr` or `a:lvl{N}pPr`) declares, if it
/// declares one at all — docling's `_parse_bullet_from_paragraph_properties`.
/// `Some(None)` is an explicit `buNone`: "this is deliberately not a list".
/// Checked in docling's order — `buNone`, `buChar`, `buAutoNum`, `buBlip` —
/// so a malformed properties element carrying both a character and a
/// numbering bullet reads as the bullet, as upstream's does.
fn declared_marker(p_pr: XmlNode) -> Option<Option<ListKind>> {
    let child = |tag: &str| p_pr.children().find(|n| n.has_tag_name(tag));
    if child("buNone").is_some() {
        Some(None)
    } else if child("buChar").is_some() {
        Some(Some(ListKind::Bulleted))
    } else if let Some(auto) = child("buAutoNum") {
        // The scheme comes from the same properties element that decided
        // the paragraph is numbered — docling's effective `buAutoNum/@type`.
        let scheme = auto.attribute("type").and_then(AutoScheme::parse);
        Some(Some(ListKind::Numbered(scheme)))
    } else if child("buBlip").is_some() {
        Some(Some(ListKind::Bulleted))
    } else {
        None
    }
}

/// The number a numbered paragraph's list starts from: its own
/// `a:pPr/a:buAutoNum/@startAt`, 1 when absent or invalid. The schema
/// (`ST_TextBulletStartAtNum`) bounds it to 1..=32767, and docling#5122
/// (#1018) ignores anything outside: a start of 2000000000 would otherwise
/// build letter markers of ~77 million characters.
fn auto_number_start(para: XmlNode) -> u64 {
    para.children()
        .find(|n| n.has_tag_name("pPr"))
        .and_then(|pr| pr.children().find(|n| n.has_tag_name("buAutoNum")))
        .and_then(|bu| bu.attribute("startAt"))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| (1..=MAX_AUTO_NUMBER_START).contains(v))
        .unwrap_or(1)
}

/// docling's `_MAX_AUTO_NUMBER_START`.
pub(super) const MAX_AUTO_NUMBER_START: u64 = 32767;

/// A paragraph's outline level (`a:pPr/@lvl`, 0 when absent), which selects the
/// `a:lvl{lvl+1}pPr` that supplies an inherited marker.
fn paragraph_level(para: XmlNode) -> usize {
    para.children()
        .find(|n| n.has_tag_name("pPr"))
        .and_then(|pr| pr.attribute("lvl"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Whether a paragraph is a list item: `Some(true)` numbered, `Some(false)`
/// bulleted, `None` not a list.
///
/// docling's `_get_effective_list_marker` order: the paragraph's own `a:pPr`
/// first, then — and this is what a styled deck relies on — the *owning shape's*
/// list style, `a:txBody/a:lstStyle/a:lvl{lvl+1}pPr` selected by the paragraph's
/// level (#406). A placeholder (`ph`, its `<p:ph>`) goes on to the layout
/// placeholder with the same `idx` — idx only, as `layout.placeholders.get`
/// matches — and, only when there is one, to the master's text style for its
/// type (#627). A layout `buNone` therefore beats the master's bullet
/// (docling#4581): the "Section Header" or "Title Slide" text of the default
/// template is plain text, not a list. The level only picks which level
/// properties supply the marker; nesting is the caller's.
fn list_marker(
    para: XmlNode,
    tx_body: Option<XmlNode>,
    ph: Option<XmlNode>,
    phmap: &PhMap,
) -> Option<ListKind> {
    let lvl = paragraph_level(para);
    if let Some(p_pr) = para.children().find(|n| n.has_tag_name("pPr")) {
        if let Some(kind) = declared_marker(p_pr) {
            return kind;
        }
    }
    if let Some(lvl_pr) = tx_body
        .and_then(|b| b.children().find(|n| n.has_tag_name("lstStyle")))
        .and_then(|st| {
            st.children()
                .find(|n| n.has_tag_name(format!("lvl{}pPr", lvl + 1).as_str()))
        })
    {
        if let Some(kind) = declared_marker(lvl_pr) {
            return kind;
        }
    }
    if let Some(ph) = ph {
        let idx = ph_idx(ph);
        if let Some((_, layout)) = phmap.layout_lists.iter().find(|(i, _)| *i == idx) {
            let inherited = layout.get(lvl).copied().flatten().or_else(|| {
                phmap.master_styles[master_style_slot(ph)]
                    .get(lvl)
                    .copied()
                    .flatten()
            });
            if let Some(kind) = inherited {
                return kind;
            }
        }
    }
    // No marker anywhere: an indented paragraph is a list item even without
    // one — docling's `paragraph.level > 0` fallback.
    (lvl > 0).then_some(ListKind::Bulleted)
}

/// [`list_marker`] as `Some(true)` numbered, `Some(false)` bulleted, `None`
/// not a list.
#[cfg(test)]
fn list_kind(
    para: XmlNode,
    tx_body: Option<XmlNode>,
    ph: Option<XmlNode>,
    phmap: &PhMap,
) -> Option<bool> {
    list_marker(para, tx_body, ph, phmap).map(ListKind::numbered)
}

/// Concatenate a paragraph's run text; line breaks (`<a:br>`) become spaces
/// and each equation (`<a14:m>`, see [`paragraph_math`]) an inline `$…$`,
/// set off from the surrounding words by a space.
fn paragraph_text(para: XmlNode) -> String {
    let mut out = String::new();
    // Just after an equation: the next text gets a separating space unless
    // it brings its own.
    let mut after_math = false;
    for child in para.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "r" | "fld" => {
                if let Some(t) = child.children().find(|n| n.has_tag_name("t")) {
                    let t = t.text().unwrap_or("");
                    if after_math && !t.is_empty() && !t.starts_with(char::is_whitespace) {
                        out.push(' ');
                    }
                    after_math &= t.is_empty();
                    out.push_str(t);
                }
            }
            "br" => {
                out.push(' ');
                after_math = false;
            }
            "m" => {
                for eq in math_latex(child) {
                    if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                        out.push(' ');
                    }
                    out.push('$');
                    out.push_str(&eq);
                    out.push('$');
                    after_math = true;
                }
            }
            _ => {}
        }
    }
    out
}

/// The LaTeX of each `<m:oMath>` inside an `<a14:m>` paragraph child — the
/// same OMML → LaTeX converter the DOCX backend uses (#575). python-pptx has
/// no notion of `a14:m`, so docling drops PowerPoint equations entirely;
/// emitting them is a deliberate divergence (the text is otherwise lost).
fn math_latex(m: XmlNode) -> Vec<String> {
    m.descendants()
        .filter(|n| n.has_tag_name("oMath"))
        .map(crate::backend::omml::to_latex)
        .filter(|eq| !eq.is_empty())
        .collect()
}

/// A paragraph made of equations alone (no run text): their LaTeX, which the
/// caller emits as standalone `$$…$$` formulas the way the DOCX backend
/// treats an OMML-only paragraph. `None` for any paragraph with words.
fn paragraph_math(para: XmlNode) -> Option<Vec<String>> {
    let mut eqs = Vec::new();
    for child in para.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "r" | "fld" => {
                let blank = child
                    .children()
                    .find(|n| n.has_tag_name("t"))
                    .and_then(|t| t.text())
                    .is_none_or(|t| t.trim().is_empty());
                if !blank {
                    return None;
                }
            }
            "m" => eqs.extend(math_latex(child)),
            _ => {}
        }
    }
    (!eqs.is_empty()).then_some(eqs)
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
        footnotes: Vec::new(),
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
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
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
#[cfg(test)]
mod list_marker_tests {
    use super::{layout_list_styles, list_kind, master_text_styles, PhMap};

    /// #406: a paragraph with no bullet properties of its own takes its marker
    /// from the owning shape's `a:lstStyle`, selected by the paragraph's level —
    /// docling's `_get_effective_list_marker` step 2. The level only picks the
    /// `a:lvl{N}pPr`; it does not nest the item.
    #[test]
    fn a_shape_list_style_supplies_the_inherited_marker() {
        let xml = r#"<p:sp xmlns:p="p" xmlns:a="a"><p:txBody>
            <a:lstStyle>
              <a:lvl1pPr><a:buChar char="■"/></a:lvl1pPr>
              <a:lvl2pPr><a:buChar char="–"/></a:lvl2pPr>
              <a:lvl3pPr><a:buAutoNum type="arabicPeriod"/></a:lvl3pPr>
              <a:lvl4pPr><a:buNone/></a:lvl4pPr>
            </a:lstStyle>
            <a:p><a:r><a:t>level 1 bullet</a:t></a:r></a:p>
            <a:p><a:pPr lvl="1"/><a:r><a:t>level 2 bullet</a:t></a:r></a:p>
            <a:p><a:pPr lvl="2"/><a:r><a:t>level 3 numbered</a:t></a:r></a:p>
            <a:p><a:pPr lvl="3"/><a:r><a:t>level 4: style says no bullet</a:t></a:r></a:p>
            <a:p><a:pPr lvl="5"/><a:r><a:t>level 6: no style at all</a:t></a:r></a:p>
            <a:p><a:pPr lvl="2"><a:buNone/></a:pPr><a:r><a:t>own buNone wins</a:t></a:r></a:p>
        </p:txBody></p:sp>"#;
        let dom = roxmltree::Document::parse(xml).unwrap();
        let body = dom
            .descendants()
            .find(|n| n.has_tag_name("txBody"))
            .unwrap();
        let kinds: Vec<Option<bool>> = body
            .children()
            .filter(|n| n.has_tag_name("p"))
            .map(|p| list_kind(p, Some(body), None, &PhMap::default()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                Some(false), // lvl1pPr buChar
                Some(false), // lvl2pPr buChar
                Some(true),  // lvl3pPr buAutoNum
                None,        // lvl4pPr buNone: deliberately not a list
                Some(false), // no marker anywhere, but indented: docling's level > 0 fallback
                None,        // the paragraph's own buNone beats the style's buAutoNum
            ]
        );
    }

    /// With no marker anywhere — no layout or master to inherit from either —
    /// a paragraph is plain text: there is no body-placeholder default.
    #[test]
    fn without_any_marker_a_placeholder_paragraph_is_text() {
        let dom = roxmltree::Document::parse(
            r#"<p:sp xmlns:p="p" xmlns:a="a"><p:nvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:lstStyle/><a:p><a:r><a:t>x</a:t></a:r></a:p></p:txBody></p:sp>"#,
        )
        .unwrap();
        let ph = dom.descendants().find(|n| n.has_tag_name("ph"));
        let body = dom
            .descendants()
            .find(|n| n.has_tag_name("txBody"))
            .unwrap();
        let para = body.children().find(|n| n.has_tag_name("p")).unwrap();
        assert_eq!(list_kind(para, Some(body), ph, &PhMap::default()), None);
        assert_eq!(list_kind(para, Some(body), None, &PhMap::default()), None);
    }

    /// #627: a placeholder's paragraph goes on to the layout placeholder with
    /// its `idx`, then the master text style for its type — docling's
    /// `_get_effective_list_marker` steps 3 and 4. The master is only reached
    /// through a matching layout placeholder.
    #[test]
    fn layout_then_master_supply_a_placeholder_marker() {
        let layout = r#"<p:sldLayout xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
            <p:sp><p:nvSpPr><p:cNvPr id="2" name="t"/><p:cNvSpPr/><p:nvPr><p:ph type="ctrTitle"/></p:nvPr></p:nvSpPr><p:txBody/></p:sp>
            <p:sp><p:nvSpPr><p:cNvPr id="3" name="b"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr>
              <p:txBody><a:lstStyle><a:lvl2pPr><a:buNone/></a:lvl2pPr></a:lstStyle></p:txBody></p:sp>
            <p:sp><p:nvSpPr><p:cNvPr id="4" name="s"/><p:cNvSpPr/><p:nvPr><p:ph type="subTitle" idx="2"/></p:nvPr></p:nvSpPr><p:txBody/></p:sp>
        </p:spTree></p:cSld></p:sldLayout>"#;
        let master = r#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles>
            <p:titleStyle><a:lvl1pPr><a:buNone/></a:lvl1pPr></p:titleStyle>
            <p:bodyStyle><a:lvl1pPr><a:buChar char="•"/></a:lvl1pPr><a:lvl2pPr><a:buChar char="•"/></a:lvl2pPr><a:lvl3pPr><a:buAutoNum type="arabicPeriod"/></a:lvl3pPr></p:bodyStyle>
            <p:otherStyle><a:lvl1pPr><a:buAutoNum type="arabicPeriod"/></a:lvl1pPr></p:otherStyle>
        </p:txStyles></p:sldMaster>"#;
        let map = PhMap {
            layout_lists: layout_list_styles(layout),
            master_styles: master_text_styles(master),
            ..PhMap::default()
        };
        let kind = |ph: &str, lvl: u8| {
            let xml = format!(
                r#"<p:sp xmlns:p="p" xmlns:a="a"><p:nvSpPr><p:nvPr>{ph}</p:nvPr></p:nvSpPr><p:txBody><a:p><a:pPr lvl="{lvl}"/><a:r><a:t>x</a:t></a:r></a:p></p:txBody></p:sp>"#
            );
            let dom = roxmltree::Document::parse(&xml).unwrap();
            let ph = dom.descendants().find(|n| n.has_tag_name("ph"));
            let body = dom.descendants().find(|n| n.has_tag_name("txBody"));
            let para = dom.descendants().find(|n| n.has_tag_name("p")).unwrap();
            list_kind(para, body, ph, &map)
        };
        // Body: lvl1 from the master's bodyStyle bullet, lvl2 the layout's
        // buNone, lvl3 the master's numbering.
        assert_eq!(kind(r#"<p:ph idx="1"/>"#, 0), Some(false));
        assert_eq!(kind(r#"<p:ph idx="1"/>"#, 1), None);
        assert_eq!(kind(r#"<p:ph idx="1"/>"#, 2), Some(true));
        // A centred title (idx 0, matching the layout's) and a subtitle both
        // read otherStyle, not titleStyle.
        assert_eq!(kind(r#"<p:ph type="ctrTitle"/>"#, 0), Some(true));
        assert_eq!(kind(r#"<p:ph type="subTitle" idx="2"/>"#, 0), Some(true));
        // `title` reads titleStyle's buNone.
        assert_eq!(kind(r#"<p:ph type="title"/>"#, 0), None);
        // No layout placeholder with idx 7: the master is not consulted and
        // only the level > 0 fallback is left.
        assert_eq!(kind(r#"<p:ph idx="7"/>"#, 0), None);
        assert_eq!(kind(r#"<p:ph idx="7"/>"#, 1), Some(false));
        // A text box never inherits.
        assert_eq!(kind("", 0), None);
    }
}

#[cfg(test)]
mod json_tree_tests {
    use super::*;
    use crate::backend::ooxml::image_dpi_tests::png;
    use crate::backend::DeclarativeBackend;
    use crate::{InputFormat, SourceDocument};
    use serde_json::Value;

    /// Drop what a conformance comparison never looks at: the file's
    /// `origin` and schema `version`, an image's bytes (PIL re-encodes
    /// them; ours are the embedded file's), and the alt-text description a
    /// picture carries here and not upstream (#676).
    fn normalize(v: &mut Value) {
        match v {
            Value::Object(m) => {
                m.remove("origin");
                m.remove("version");
                if let Some(uri) = m.get_mut("uri") {
                    *uri = Value::Null;
                }
                if m.get("meta")
                    .and_then(|meta| meta.pointer("/description/created_by"))
                    == Some(&Value::from("alt_text"))
                {
                    m.remove("meta");
                }
                if let Some(Value::Array(notes)) = m.get_mut("annotations") {
                    notes.retain(|a| a.get("provenance") != Some(&Value::from("alt_text")));
                }
                m.values_mut().for_each(normalize);
            }
            Value::Array(a) => a.iter_mut().for_each(normalize),
            _ => {}
        }
    }

    /// The JSON is docling's item tree: on every mirrored fixture it is
    /// structurally identical to upstream's groundtruth (docling 2.129) —
    /// slide groups, `paragraph`/`title`/`list_item` labels and markers,
    /// list groups, the non-empty table cells, pictures with the file's
    /// dpi, chart captions, notes, `comment_section` groups, and every
    /// item's raw-EMU provenance with its per-item `charspan`.
    #[test]
    fn json_is_structurally_identical_to_docling_groundtruth() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/pptx");
        let mut compared = 0;
        for entry in std::fs::read_dir(root.join("sources")).expect("pptx corpus") {
            let path = entry.expect("entry").path();
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            let Ok(gt) =
                std::fs::read_to_string(root.join("groundtruth").join(format!("{name}.json")))
            else {
                continue;
            };
            let bytes = std::fs::read(&path).expect("fixture bytes");
            let doc = PptxBackend
                .convert(&SourceDocument::from_bytes(&name, InputFormat::Pptx, bytes))
                .expect("converts");
            let mut ours: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
            let mut want: Value = serde_json::from_str(&gt).unwrap();
            for v in [&mut ours, &mut want] {
                v.as_object_mut().unwrap().remove("name");
                normalize(v);
            }
            assert_eq!(
                ours, want,
                "{name}: JSON differs from docling's groundtruth"
            );
            compared += 1;
        }
        assert!(compared >= 8, "{compared} fixtures compared");
    }

    /// python-pptx's `Image.dpi`: Pillow's value rounded half-to-even, 72
    /// when absent or outside 1–2048.
    #[test]
    fn dpi_follows_python_pptx() {
        let phys = |ppu: u32| {
            let mut d = ppu.to_be_bytes().to_vec();
            d.extend(ppu.to_be_bytes());
            d.push(1);
            d
        };
        assert_eq!(
            pptx_dpi(&png(&[(b"pHYs", phys(11811))])),
            300,
            "299.9994 rounds up"
        );
        assert_eq!(pptx_dpi(&png(&[(b"pHYs", phys(2835))])), 72, "72.009");
        assert_eq!(
            pptx_dpi(&png(&[(b"pHYs", phys(100_000))])),
            72,
            "2540 is out of range"
        );
        assert_eq!(pptx_dpi(&png(&[(b"IEND", vec![])])), 72, "no pHYs");
        assert_eq!(pptx_dpi(b"GIF89a"), 72);
    }
}

#[cfg(test)]
mod math_tests {
    use super::*;

    /// #575: `<a14:m>` equations — inline `$…$` among words, a formula of
    /// their own when the paragraph holds nothing else.
    #[test]
    fn paragraph_equations_are_latex() {
        let xml = r#"<p:txBody xmlns:p="p" xmlns:a="a" xmlns:a14="a14" xmlns:m="m">
            <a:p><a:r><a:t>Area</a:t></a:r><a14:m><m:oMath><m:r><m:t>A=</m:t></m:r>
              <m:sSup><m:e><m:r><m:t>r</m:t></m:r></m:e><m:sup><m:r><m:t>2</m:t></m:r></m:sup></m:sSup>
            </m:oMath></a14:m><a:r><a:t> here</a:t></a:r></a:p>
            <a:p><a14:m><m:oMathPara><m:oMath><m:r><m:t>E=mc^2</m:t></m:r></m:oMath></m:oMathPara></a14:m></a:p>
        </p:txBody>"#;
        let dom = roxmltree::Document::parse(xml).unwrap();
        let paras: Vec<XmlNode> = dom
            .descendants()
            .filter(|n| n.has_tag_name("p") && n.tag_name().namespace() == Some("a"))
            .collect();
        assert_eq!(paragraph_text(paras[0]), "Area $A=r^{2}$ here");
        assert_eq!(paragraph_math(paras[0]), None);
        assert_eq!(paragraph_text(paras[1]), "$E=mc^2$");
        assert_eq!(paragraph_math(paras[1]), Some(vec!["E=mc^2".to_string()]));
    }

    /// PowerPoint wraps a text box holding an equation in
    /// `mc:AlternateContent`: after `mc.rs` settles it, the `a14` Choice's
    /// shape is on the tree and the picture Fallback is not.
    #[test]
    fn equation_shapes_come_from_the_a14_choice() {
        let xml = r#"<p:spTree xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main">
            <mc:AlternateContent><mc:Choice Requires="a14"><p:sp><p:nvSpPr><p:cNvPr id="2" name="eq"/></p:nvSpPr></p:sp></mc:Choice>
              <mc:Fallback><p:sp><p:nvSpPr><p:cNvPr id="2" name="eq picture"/></p:nvSpPr></p:sp></mc:Fallback></mc:AlternateContent>
        </p:spTree>"#;
        let xml = crate::backend::mc::resolve_alternate_content(xml);
        let dom = roxmltree::Document::parse(&xml).unwrap();
        let names: Vec<&str> = shapes_by_position(dom.root_element(), &PhMap::default())
            .into_iter()
            .filter_map(|sp| descendant(sp, "cNvPr").and_then(|c| c.attribute("name")))
            .collect();
        assert_eq!(names, vec!["eq"]);
    }
}
