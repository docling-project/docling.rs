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

use docling_core::{DoclingDocument, Node, PictureImage, Table};
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
        let content_types = pkg.read("[Content_Types].xml").unwrap_or_default();
        let rid_to_part: HashMap<String, String> = pkg
            .rels_for("ppt/presentation.xml")
            .iter()
            .map(|r| (r.id.clone(), resolve("ppt", &r.target)))
            .collect();

        for rid in slide_rids(&presentation) {
            let Some(part) = rid_to_part.get(&rid).cloned() else {
                continue;
            };
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
                    handle_shape(shape, &valid_imgs, &images, &mut doc);
                }
            }
        }
        Ok(doc)
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
    doc: &mut DoclingDocument,
) {
    match shape.tag_name().name() {
        "grpSp" => {
            for child in shape.children().filter(XmlNode::is_element) {
                handle_shape(child, valid_imgs, images, doc);
            }
        }
        "graphicFrame" => {
            if let Some(tbl) = descendant(shape, "tbl") {
                if let Some(table) = parse_table(tbl) {
                    doc.push(Node::Table(table));
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
                doc.push(Node::Picture {
                    caption: None,
                    image: images.get(&rid).cloned(),
                });
            }
        }
        "sp" => handle_text_shape(shape, doc),
        _ => {}
    }
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

fn handle_text_shape(sp: XmlNode, doc: &mut DoclingDocument) {
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
                doc.push(Node::ListItem {
                    ordered: numbered,
                    number: n,
                    first_in_list: false,
                    text,
                    level: 0,
                    marker: None,
                });
            }
            None => {
                in_list = false;
                match kind {
                    Placeholder::Title => doc.push(Node::Heading { level: 1, text }),
                    // docling intends SECTION_HEADER for subtitles but a bug
                    // leaves the label as PARAGRAPH, so subtitles render as text.
                    _ => doc.push(Node::Paragraph { text }),
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

    let mut grid = vec![vec![String::new(); num_cols]; rows.len()];
    for (ri, row) in rows.iter().enumerate() {
        let cells: Vec<XmlNode> = row.children().filter(|n| n.has_tag_name("tc")).collect();
        for (ci, tc) in cells.iter().enumerate() {
            // Continuation cells of a merge are filled by their origin.
            if tc.attribute("hMerge").is_some() || tc.attribute("vMerge").is_some() {
                continue;
            }
            let text = cell_text(*tc);
            let span = |name: &str| -> usize {
                tc.attribute(name).and_then(|s| s.parse().ok()).unwrap_or(1)
            };
            let (gridspan, rowspan) = (span("gridSpan"), span("rowSpan"));
            let row_end = (ri + rowspan).min(rows.len());
            let col_end = (ci + gridspan).min(num_cols);
            for row in grid.iter_mut().take(row_end).skip(ri) {
                for cell in row.iter_mut().take(col_end).skip(ci) {
                    *cell = text.clone();
                }
            }
        }
    }
    Some(Table {
        rows: grid,
        location: None,
        structure: None,
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
