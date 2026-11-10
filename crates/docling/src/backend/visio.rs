//! Visio backend (issue #214) — a docling.rs extension; Python docling has no
//! Visio reader, so there is no byte-conformance target. Covers the modern
//! OPC formats (.vsdx and its macro-enabled twin .vsdm) — the same zip + XML
//! envelope machinery as the DOCX/PPTX backends ([`Package`]); the legacy
//! binary .vsd and 2003-XML .vdx are follow-ups.
//!
//! Diagrams-as-documentation is the use case: architecture and process charts
//! that RAG pipelines currently drop. The mapping:
//!
//! - every page becomes a section — a level-1 heading with the page name
//! - shape text flows in reading order (top-to-bottom, then left-to-right;
//!   Visio's origin is bottom-left, so higher `PinY` reads first), group
//!   sub-shapes positioned via the parent's coordinate system
//! - connectors (shapes with `BeginX`/`EndX` endpoint cells) become a
//!   relations table: From | To (| Label when any connector carries text),
//!   endpoints resolved through the page's `<Connects>` section
//! - a shape with no text of its own inherits its master's default text
//!   (`Master` attribute → `masters/masterN.xml`), matching Visio semantics
//!
//! Pure geometry (shapes with no text anywhere) contributes nothing.

use std::collections::HashMap;

use docling_core::{DoclingDocument, Node, Table};
use roxmltree::{Document, Node as XmlNode};

use crate::backend::ooxml::{resolve, Package};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct VisioBackend;

impl DeclarativeBackend for VisioBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let mut pkg = Package::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("visio: bad zip".into()))?;
        let pages_xml = pkg
            .read("visio/pages/pages.xml")
            .ok_or_else(|| ConversionError::Parse("visio: no pages.xml".into()))?;
        let page_rels: HashMap<String, String> = pkg
            .rels_for("visio/pages/pages.xml")
            .into_iter()
            .map(|r| (r.id, resolve("visio/pages", &r.target)))
            .collect();
        let masters = load_master_texts(&mut pkg);

        let mut doc = DoclingDocument::new(&source.name);
        let pages = Document::parse(&pages_xml)
            .map_err(|e| ConversionError::Parse(format!("visio: pages.xml: {e}")))?;
        let mut converted = 0usize;
        for page in pages
            .root_element()
            .descendants()
            .filter(|n| n.has_tag_name("Page"))
        {
            // Background pages are furniture (title blocks, borders), not
            // content — Visio never shows them standalone.
            if page.attribute("Background") == Some("1") {
                continue;
            }
            let name = page
                .attribute("Name")
                .or_else(|| page.attribute("NameU"))
                .unwrap_or("Page");
            let Some(part) = page
                .children()
                .find(|n| n.has_tag_name("Rel"))
                .and_then(|rel| rel.attributes().find(|a| a.name() == "id"))
                .and_then(|a| page_rels.get(a.value()))
            else {
                continue;
            };
            let Some(xml) = pkg.read(part) else { continue };
            if render_page(&xml, name, &masters, &mut doc) {
                converted += 1;
            }
        }
        if converted == 0 {
            return Err(ConversionError::Parse(
                "visio: no convertible page content".into(),
            ));
        }
        Ok(doc)
    }
}

/// One shape from a page, positioned absolutely, with its sub-shapes.
/// Groups stay a tree: a group's children are emitted together as a block
/// (an ER entity's attribute rows must not interleave with a neighboring
/// entity's just because they sit at similar heights).
struct VisioShape {
    id: String,
    /// Absolute box top/left edge in page inches; Visio Y grows upward, so
    /// the *largest* top reads first.
    top: f64,
    left: f64,
    width: f64,
    height: f64,
    text: String,
    /// `BeginX`/`EndX` endpoint cells present — a 1-D connector.
    connector: bool,
    /// A connector's glue points, for adopting nearby floating labels
    /// (ER cardinalities sit next to an endpoint, not inside any shape).
    begin: Option<(f64, f64)>,
    end: Option<(f64, f64)>,
    children: Vec<VisioShape>,
}

impl VisioShape {
    fn area(&self) -> f64 {
        self.width * self.height
    }
    fn center(&self) -> (f64, f64) {
        (self.left + self.width / 2.0, self.top - self.height / 2.0)
    }
    /// Whether `point` falls inside this shape's box.
    fn contains(&self, point: (f64, f64)) -> bool {
        point.0 >= self.left
            && point.0 <= self.left + self.width
            && point.1 <= self.top
            && point.1 >= self.top - self.height
    }
}

/// `Master` attribute → the master's default shape text, harvested from
/// `visio/masters/masterN.xml`. A page shape without its own `<Text>`
/// inherits this (Visio's instance-overrides-master model).
fn load_master_texts(pkg: &mut Package) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(masters_xml) = pkg.read("visio/masters/masters.xml") else {
        return out;
    };
    let rels: HashMap<String, String> = pkg
        .rels_for("visio/masters/masters.xml")
        .into_iter()
        .map(|r| (r.id, resolve("visio/masters", &r.target)))
        .collect();
    let Ok(masters) = Document::parse(&masters_xml) else {
        return out;
    };
    for master in masters
        .root_element()
        .descendants()
        .filter(|n| n.has_tag_name("Master"))
    {
        let Some(id) = master.attribute("ID") else {
            continue;
        };
        let Some(part) = master
            .children()
            .find(|n| n.has_tag_name("Rel"))
            .and_then(|rel| rel.attributes().find(|a| a.name() == "id"))
            .and_then(|a| rels.get(a.value()))
        else {
            continue;
        };
        let Some(xml) = pkg.read(part) else { continue };
        let Ok(part_doc) = Document::parse(&xml) else {
            continue;
        };
        // Only the master's *root* shape text is the instance default; texts
        // of internal sub-shapes (e.g. the Relationship master's hidden
        // M1–M4 cardinality slots) are placeholders, not content.
        let text = part_doc
            .root_element()
            .descendants()
            .find(|n| n.has_tag_name("Shape"))
            .and_then(|sh| sh.children().find(|n| n.has_tag_name("Text")))
            .map(|t| normalize(&text_of(&t)))
            .unwrap_or_default();
        if !text.is_empty() {
            out.insert(id.to_string(), text);
        }
    }
    out
}

/// Render one page part into the document: heading, shape paragraphs in
/// reading order, then the connector relations table. Returns whether the
/// page produced any content.
fn render_page(
    xml: &str,
    name: &str,
    masters: &HashMap<String, String>,
    doc: &mut DoclingDocument,
) -> bool {
    let Ok(page) = Document::parse(xml) else {
        return false;
    };
    let root = page.root_element();
    let mut shapes = Vec::new();
    if let Some(top) = root.children().find(|n| n.has_tag_name("Shapes")) {
        for shape in top.children().filter(|n| n.has_tag_name("Shape")) {
            if let Some(sh) = collect_shape(&shape, 0.0, 0.0, masters, true) {
                shapes.push(sh);
            }
        }
    }
    // Visio files often keep visual containment *flat*: an ER entity's
    // attribute rows are page-level siblings drawn inside the entity's box.
    // Geometric adoption — nest each shape under the smallest non-connector
    // shape whose box contains its center — so a container reads as one
    // block instead of interleaving with its neighbors.
    let attached_labels = adopt_contained(&mut shapes);
    // Reading order: top-to-bottom, then left-to-right. total_cmp keeps the
    // comparator total (NaN-free sort panic guard, cf. the PDF pipeline).
    shapes.sort_by(|a, b| b.top.total_cmp(&a.top).then(a.left.total_cmp(&b.left)));

    // Walk the ordered tree: paragraphs from ordinary shapes (a group's
    // children stay together as its block), connectors set aside for the
    // relations table with every subtree text (arrowheads, cardinality
    // labels) folded into the connector's label.
    let mut paragraphs: Vec<String> = Vec::new();
    let mut connectors: Vec<(&VisioShape, Vec<String>)> = Vec::new();
    fn walk<'a>(
        shapes: &'a [VisioShape],
        paragraphs: &mut Vec<String>,
        connectors: &mut Vec<(&'a VisioShape, Vec<String>)>,
    ) {
        for s in shapes {
            if s.connector {
                let mut label = Vec::new();
                subtree_text(s, &mut label);
                connectors.push((s, label));
                continue;
            }
            if !s.text.is_empty() {
                paragraphs.push(s.text.clone());
            }
            walk(&s.children, paragraphs, connectors);
        }
    }
    walk(&shapes, &mut paragraphs, &mut connectors);

    // Connector endpoints: the connector's BeginX/EndX cells connect *to* the
    // source/target shape's sheet.
    let mut begins: HashMap<&str, &str> = HashMap::new();
    let mut ends: HashMap<&str, &str> = HashMap::new();
    for connect in root.descendants().filter(|n| n.has_tag_name("Connect")) {
        let (Some(from), Some(cell), Some(to)) = (
            connect.attribute("FromSheet"),
            connect.attribute("FromCell"),
            connect.attribute("ToSheet"),
        ) else {
            continue;
        };
        match cell {
            "BeginX" => {
                begins.insert(from, to);
            }
            "EndX" => {
                ends.insert(from, to);
            }
            _ => {}
        }
    }
    // Endpoint id → (shape, its top-level root).
    let mut by_id: HashMap<&str, (&VisioShape, &VisioShape)> = HashMap::new();
    fn index<'a>(
        shapes: &'a [VisioShape],
        root: Option<&'a VisioShape>,
        by_id: &mut HashMap<&'a str, (&'a VisioShape, &'a VisioShape)>,
    ) {
        for s in shapes {
            let r = root.unwrap_or(s);
            by_id.insert(s.id.as_str(), (s, r));
            index(&s.children, Some(r), by_id);
        }
    }
    index(&shapes, None, &mut by_id);
    // An endpoint's display name: the glued shape's first text (reading
    // order — an ER entity group carries its title in a child shape); a
    // textless glue target (an arrowhead, a picture) borrows its top-level
    // container's name instead.
    let label = |id: &str| -> String {
        by_id
            .get(id)
            .map(|(s, root)| {
                let mut texts = Vec::new();
                subtree_text(s, &mut texts);
                if texts.is_empty() {
                    subtree_text(root, &mut texts);
                }
                texts.first().cloned().unwrap_or_default()
            })
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("Shape {id}"))
    };
    let mut relations: Vec<[String; 3]> = Vec::new();
    for (conn, conn_label) in &connectors {
        let (from, to) = (begins.get(conn.id.as_str()), ends.get(conn.id.as_str()));
        // A dangling connector (an endpoint glued to nothing) still names the
        // side it *is* glued to; drop it only when glued to nothing at all.
        if from.is_none() && to.is_none() {
            continue;
        }
        let mut parts = conn_label.clone();
        if let Some(adopted) = attached_labels.get(conn.id.as_str()) {
            parts.extend(adopted.iter().cloned());
        }
        let side = |end: Option<&&str>| end.map(|id| label(id)).unwrap_or_default();
        relations.push([side(from), side(to), parts.join(" ")]);
    }

    if paragraphs.is_empty() && relations.is_empty() {
        return false;
    }
    doc.push(Node::Heading {
        level: 1,
        text: name.to_string(),
    });
    for text in paragraphs {
        doc.push(Node::Paragraph { text });
    }
    if !relations.is_empty() {
        let labeled = relations.iter().any(|r| !r[2].is_empty());
        let mut rows = Vec::with_capacity(relations.len() + 1);
        let header = if labeled {
            vec!["From".to_string(), "To".to_string(), "Label".to_string()]
        } else {
            vec!["From".to_string(), "To".to_string()]
        };
        rows.push(header);
        for r in relations {
            let [from, to, text] = r;
            rows.push(if labeled {
                vec![from, to, text]
            } else {
                vec![from, to]
            });
        }
        doc.push(Node::Table(Table {
            footnotes: Vec::new(),
            rows,
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
            caption_parent: Default::default(),
            caption_location: None,
            document_index: false,
        }));
    }
    true
}

/// Floating text this small is a connector annotation (an ER cardinality,
/// a flow label), not standalone content — when it sits close enough to a
/// connector endpoint it becomes part of that connector's label.
const LABEL_MAX_AREA: f64 = 0.5; // sq in
const LABEL_MAX_DIST: f64 = 0.6; // in

/// Two flat-file fixups, both geometric:
///
/// 1. every top-level shape whose center falls inside a strictly larger
///    non-connector shape's box moves under the smallest such container;
/// 2. a small floating text shape near a connector endpoint is adopted as
///    that connector's label (returned as connector id → texts, ordered
///    begin-side first).
fn adopt_contained(shapes: &mut Vec<VisioShape>) -> HashMap<String, Vec<String>> {
    // Labels first, while everything is still top-level: a text shape near
    // a glue point leaves the shape list and joins the connector's label.
    let mut attached: HashMap<String, Vec<(f64, String)>> = HashMap::new();
    let mut keep = Vec::with_capacity(shapes.len());
    type GluePoints = (String, Option<(f64, f64)>, Option<(f64, f64)>);
    let snapshot: Vec<GluePoints> = shapes
        .iter()
        .filter(|s| s.connector)
        .map(|s| (s.id.clone(), s.begin, s.end))
        .collect();
    for shape in shapes.drain(..) {
        let mut adopted = false;
        if !shape.connector
            && shape.children.is_empty()
            && !shape.text.is_empty()
            && shape.area() < LABEL_MAX_AREA
        {
            let c = shape.center();
            let mut best: Option<(f64, &str, f64)> = None;
            for (id, begin, end) in &snapshot {
                for (rank, pt) in [(0.0, *begin), (1.0, *end)] {
                    let Some(pt) = pt else { continue };
                    let d = ((c.0 - pt.0).powi(2) + (c.1 - pt.1).powi(2)).sqrt();
                    if d <= LABEL_MAX_DIST && best.is_none_or(|(bd, _, _)| d < bd) {
                        best = Some((d, id.as_str(), rank));
                    }
                }
            }
            if let Some((_, id, rank)) = best {
                attached
                    .entry(id.to_string())
                    .or_default()
                    .push((rank, one_line(&shape.text)));
                adopted = true;
            }
        }
        if !adopted {
            keep.push(shape);
        }
    }
    *shapes = keep;

    // Pass 1: geometric containment. Assign each shape to the smallest
    // strictly-larger container whose box holds its center. A shape whose
    // box holds most of the page's shapes is a background frame / banner,
    // not a semantic container — it adopts nothing.
    let n = shapes.len();
    let frame: Vec<bool> = shapes
        .iter()
        .map(|cand| {
            if cand.connector || n < 2 {
                return false;
            }
            let held = shapes
                .iter()
                .filter(|s| !std::ptr::eq(*s, cand) && cand.contains(s.center()))
                .count();
            held * 10 >= (n - 1) * 6
        })
        .collect();
    let mut parent: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        if shapes[i].connector {
            continue;
        }
        let c = shapes[i].center();
        let mut best: Option<usize> = None;
        for (j, cand) in shapes.iter().enumerate() {
            if j == i || cand.connector || frame[j] || cand.area() <= shapes[i].area() {
                continue;
            }
            if cand.contains(c) && best.is_none_or(|b| cand.area() < shapes[b].area()) {
                best = Some(j);
            }
        }
        parent[i] = best;
    }
    // Rebuild: move adopted shapes into their container's children. Chains
    // (A in B in C) resolve because each shape records its *smallest*
    // container directly.
    let mut moved: Vec<Option<VisioShape>> = shapes.drain(..).map(Some).collect();
    for i in 0..n {
        if parent[i].is_some() {
            let child = moved[i].take().expect("moved once");
            let mut target = parent[i];
            // The container itself may have been adopted; the child still
            // belongs to it, so find it wherever it now lives.
            while let Some(t) = target {
                if let Some(container) = moved[t].as_mut() {
                    container.children.push(child);
                    break;
                }
                target = parent[t];
            }
        }
    }
    for slot in moved.into_iter().flatten() {
        shapes.push(slot);
    }
    fn sort_children(shapes: &mut [VisioShape]) {
        for s in shapes {
            s.children
                .sort_by(|a, b| b.top.total_cmp(&a.top).then(a.left.total_cmp(&b.left)));
            sort_children(&mut s.children);
        }
    }
    sort_children(shapes);

    attached
        .into_iter()
        .map(|(id, mut texts)| {
            texts.sort_by(|a, b| a.0.total_cmp(&b.0));
            (id, texts.into_iter().map(|(_, t)| t).collect())
        })
        .collect()
}

/// All non-empty texts of a shape subtree, reading order, one line each.
fn subtree_text(shape: &VisioShape, out: &mut Vec<String>) {
    let t = one_line(&shape.text);
    if !t.is_empty() {
        out.push(t);
    }
    for child in &shape.children {
        subtree_text(child, out);
    }
}

/// Build a `<Shape>` subtree. `ox`/`oy` is the parent group's origin in
/// page coordinates: a child's pin is local to its parent, and a group maps
/// its children through `pin - locpin` (the group's local origin on the page).
fn collect_shape(
    shape: &XmlNode,
    ox: f64,
    oy: f64,
    masters: &HashMap<String, String>,
    inherit: bool,
) -> Option<VisioShape> {
    if shape.attribute("Del") == Some("1") {
        return None;
    }
    let cell = |name: &str| -> Option<f64> {
        shape
            .children()
            .find(|n| n.has_tag_name("Cell") && n.attribute("N") == Some(name))
            .and_then(|c| c.attribute("V"))
            .and_then(|v| v.parse::<f64>().ok())
    };
    let pin_x = ox + cell("PinX").unwrap_or(0.0);
    let pin_y = oy + cell("PinY").unwrap_or(0.0);
    let loc_x = cell("LocPinX").unwrap_or(0.0);
    let loc_y = cell("LocPinY").unwrap_or(0.0);
    // Reading order anchors on the box's top-left corner, not the pin: a
    // large background field is pinned at its *center*, but its title reads
    // first because its top edge is highest.
    let width = cell("Width").unwrap_or(0.0);
    let height = cell("Height").unwrap_or(0.0);
    let top = pin_y - loc_y + height;
    let left = pin_x - loc_x;
    let begin = cell("BeginX")
        .zip(cell("BeginY"))
        .map(|(x, y)| (ox + x, oy + y));
    let end = cell("EndX")
        .zip(cell("EndY"))
        .map(|(x, y)| (ox + x, oy + y));
    let connector = cell("BeginX").is_some() || cell("EndX").is_some();

    let own_text = shape
        .children()
        .find(|n| n.has_tag_name("Text"))
        .map(|t| normalize(&text_of(&t)));
    let text = match own_text {
        Some(t) if !t.is_empty() => t,
        // An empty <Text/> is an explicit override ("no text"); only a shape
        // with *no* Text element falls back to its master's default.
        Some(_) => String::new(),
        // Sub-shapes never inherit: a master's internal placeholders (e.g.
        // the Relationship master's hidden M1–M4 cardinality slots) are not
        // page content unless the instance overrides them.
        None if inherit => shape
            .attribute("Master")
            .and_then(|m| masters.get(m))
            .cloned()
            .unwrap_or_default(),
        None => String::new(),
    };

    let mut children = Vec::new();
    if let Some(nested) = shape.children().find(|n| n.has_tag_name("Shapes")) {
        let (child_ox, child_oy) = (pin_x - loc_x, pin_y - loc_y);
        for child in nested.children().filter(|n| n.has_tag_name("Shape")) {
            if let Some(c) = collect_shape(&child, child_ox, child_oy, masters, false) {
                children.push(c);
            }
        }
        children.sort_by(|a, b| b.top.total_cmp(&a.top).then(a.left.total_cmp(&b.left)));
    }
    Some(VisioShape {
        id: shape.attribute("ID").unwrap_or_default().to_string(),
        top,
        left,
        width,
        height,
        text,
        connector,
        begin,
        end,
        children,
    })
}

/// The visible text of a `<Text>` element: its text nodes, in order. Marker
/// children (`<cp/>`, `<pp/>`, `<fld/>`) carry formatting/field indices; their
/// tails are ordinary text and arrive as separate text nodes.
fn text_of(text_el: &XmlNode) -> String {
    text_el
        .descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect()
}

/// Normalize shape text: Visio's `\u{2028}` soft line breaks and CRLF both
/// become plain newlines; surrounding whitespace goes.
fn normalize(text: &str) -> String {
    text.replace('\u{2028}', "\n")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Table-cell form of a shape text: one line, single-spaced.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_doc(page_xml: &str) -> DoclingDocument {
        let mut doc = DoclingDocument::new("test");
        assert!(render_page(page_xml, "Page-1", &HashMap::new(), &mut doc));
        doc
    }

    #[test]
    fn shapes_flow_in_reading_order() {
        // Visio Y grows upward: the y=8 shape is *above* y=5.
        let doc = page_doc(
            r#"<PageContents xmlns='http://schemas.microsoft.com/office/visio/2012/main'>
              <Shapes>
                <Shape ID='1'><Cell N='PinX' V='4'/><Cell N='PinY' V='5'/><Text>Lower</Text></Shape>
                <Shape ID='2'><Cell N='PinX' V='2'/><Cell N='PinY' V='8'/><Text>Upper</Text></Shape>
                <Shape ID='3'><Cell N='PinX' V='6'/><Cell N='PinY' V='8'/><Text>Right</Text></Shape>
              </Shapes>
            </PageContents>"#,
        );
        assert_eq!(
            doc.nodes,
            vec![
                Node::Heading {
                    level: 1,
                    text: "Page-1".into()
                },
                Node::Paragraph {
                    text: "Upper".into()
                },
                Node::Paragraph {
                    text: "Right".into()
                },
                Node::Paragraph {
                    text: "Lower".into()
                },
            ]
        );
    }

    #[test]
    fn connectors_become_relations_table() {
        let doc = page_doc(
            r#"<PageContents xmlns='http://schemas.microsoft.com/office/visio/2012/main'>
              <Shapes>
                <Shape ID='1'><Cell N='PinY' V='9'/><Text>App</Text></Shape>
                <Shape ID='2'><Cell N='PinY' V='5'/><Text>DB</Text></Shape>
                <Shape ID='9'><Cell N='PinY' V='7'/><Cell N='BeginX' V='1'/><Cell N='EndX' V='2'/><Text>reads</Text></Shape>
              </Shapes>
              <Connects>
                <Connect FromSheet='9' FromCell='BeginX' FromPart='9' ToSheet='1' ToCell='PinX' ToPart='3'/>
                <Connect FromSheet='9' FromCell='EndX' FromPart='12' ToSheet='2' ToCell='PinX' ToPart='3'/>
              </Connects>
            </PageContents>"#,
        );
        assert_eq!(
            doc.nodes,
            vec![
                Node::Heading {
                    level: 1,
                    text: "Page-1".into()
                },
                Node::Paragraph { text: "App".into() },
                Node::Paragraph { text: "DB".into() },
                Node::Table(Table {
                    footnotes: Vec::new(),
                    rows: vec![
                        vec!["From".into(), "To".into(), "Label".into()],
                        vec!["App".into(), "DB".into(), "reads".into()],
                    ],
                    location: None,
                    structure: None,
                    cell_blocks: None,
                    cells: None,
                    caption: None,
                    caption_parent: Default::default(),
                    caption_location: None,
                    document_index: false,
                }),
            ]
        );
    }

    #[test]
    fn group_children_position_through_parent_origin() {
        // Group box top = pin 5 − locpin 1 + height 3 = 7, above the sibling
        // (top 5): the whole group block — its own text, then its children —
        // reads before the sibling, and children never interleave with it.
        let doc = page_doc(
            r#"<PageContents xmlns='http://schemas.microsoft.com/office/visio/2012/main'>
              <Shapes>
                <Shape ID='1' Type='Group'>
                  <Cell N='PinX' V='5'/><Cell N='PinY' V='5'/>
                  <Cell N='LocPinX' V='1'/><Cell N='LocPinY' V='1'/><Cell N='Height' V='3'/>
                  <Text>Entity</Text>
                  <Shapes>
                    <Shape ID='2'><Cell N='PinX' V='0.5'/><Cell N='PinY' V='1.5'/><Text>Child</Text></Shape>
                  </Shapes>
                </Shape>
                <Shape ID='3'><Cell N='PinX' V='5'/><Cell N='PinY' V='5'/><Text>Sibling</Text></Shape>
              </Shapes>
            </PageContents>"#,
        );
        let texts: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Paragraph { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Entity", "Child", "Sibling"]);
    }

    #[test]
    fn empty_text_element_overrides_master_default() {
        let mut masters = HashMap::new();
        masters.insert("7".to_string(), "Default".to_string());
        let mut doc = DoclingDocument::new("test");
        assert!(render_page(
            r#"<PageContents xmlns='http://schemas.microsoft.com/office/visio/2012/main'>
              <Shapes>
                <Shape ID='1' Master='7'><Cell N='PinY' V='9'/></Shape>
                <Shape ID='2' Master='7'><Cell N='PinY' V='5'/><Text></Text></Shape>
              </Shapes>
            </PageContents>"#,
            "P",
            &masters,
            &mut doc,
        ));
        // Shape 1 inherits the master text; shape 2's empty override drops it.
        assert_eq!(
            doc.nodes,
            vec![
                Node::Heading {
                    level: 1,
                    text: "P".into()
                },
                Node::Paragraph {
                    text: "Default".into()
                },
            ]
        );
    }

    #[test]
    fn soft_breaks_and_marker_tails_normalize() {
        let doc = page_doc(
            "<PageContents xmlns='http://schemas.microsoft.com/office/visio/2012/main'>
              <Shapes>
                <Shape ID='1'><Cell N='PinY' V='9'/><Text><cp IX='0'/><pp IX='0'/>Sekund\u{e4}rer\u{2028}Speicher\r\n</Text></Shape>
              </Shapes>
            </PageContents>",
        );
        assert_eq!(
            doc.nodes[1],
            Node::Paragraph {
                text: "Sekundärer\nSpeicher".into()
            }
        );
    }
}
