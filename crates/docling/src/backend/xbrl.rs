//! XBRL backend — docling's `XbrlDocumentBackend` (#466) without arelle.
//!
//! The document title is the `DocumentType`, `EntityRegistrantName` and
//! `DocumentPeriodEndDate` facts (the last non-empty value of each) run
//! together. The body is the text-block facts — concepts of
//! `textBlockItemType`, whose values are HTML — each converted as its own HTML
//! document (docling's `HTMLDocumentBackend` with `infer_furniture=False,
//! add_title=False`) and appended in document order, repeats included. The
//! numeric facts end the document as one key-value graph (docling-core's
//! `GraphData`, [`Node::KeyValueGraph`]): each fact a key cell over four value
//! cells (value, period, unit, decimals), then the concept hierarchy the
//! taxonomy's presentation linkbase gives each fact's concept and the
//! summation relationships of its calculation linkbase, weight and all —
//! see [`super::xbrl_dts`] for where the taxonomy comes from. The graph is
//! JSON-only; the Markdown export writes docling's
//! `<!-- missing-key-value-item -->` placeholder in its place.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node as XmlNode, ParsingOptions};

use super::xbrl_dts::{effective, Arc, Taxonomy, LINK_NS, XLINK_NS};
use super::xbrl_inline::{inline_filing, is_inline_root};
use crate::backend::markdown::escape_text;
use crate::backend::{DeclarativeBackend, NoFetch};
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::tree::{ItemTree, TreeKind};
use docling_core::{DoclingDocument, GraphCell, GraphLink, Node};

const XBRLI_NS: &str = "http://www.xbrl.org/2003/instance";

pub struct XbrlBackend;

impl DeclarativeBackend for XbrlBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        convert_xbrl(source, None)
    }
}

/// Convert an XBRL instance. `taxonomy` is the directory its taxonomy is read
/// from (docling's `XBRLBackendOptions.taxonomy`); `None` searches the
/// instance's own directory, where a filing keeps its extension schema.
pub(crate) fn convert_xbrl(
    source: &SourceDocument,
    taxonomy: Option<&Path>,
) -> Result<DoclingDocument, ConversionError> {
    let xml = source.text()?;
    super::xml_depth::check(&xml, "xbrl")?;
    let opts = ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let dom = Document::parse_with_options(&xml, opts)
        .map_err(|e| ConversionError::with_source("xbrl", e))?;
    let root = dom.root_element();
    let mut doc = DoclingDocument::new(&source.name);
    // Each text block's item tree, merged at the end the way docling's
    // `concatenate` merges the block documents into the title document.
    let mut blocks: Vec<Option<ItemTree>> = Vec::new();

    let taxonomy_dir: Option<PathBuf> = taxonomy.map(Path::to_path_buf).or_else(|| {
        source
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    });
    // An inline XBRL report (docling#4689) is an XHTML page whose facts are
    // tagged in place; the instance is read from its `ix:` elements. Both
    // shapes come down to the same facts, contexts and units.
    let filing = if is_inline_root(root) {
        inline_filing(root)
    } else {
        instance_filing(root)
    };
    let schema_refs: Vec<&str> = filing.schema_refs.iter().map(String::as_str).collect();
    let taxonomy = Taxonomy::discover(taxonomy_dir.as_deref(), &schema_refs);
    let facts = &filing.facts;
    let contexts = &filing.contexts;
    let units = &filing.units;

    // The title: the last non-empty value of each dei fact, joined as written
    // (docling strips the ends only); the file name when there is none.
    let (mut doc_type, mut doc_org, mut doc_period) = ("", "", "");
    for fact in facts {
        let value = fact.value.as_str();
        if value.is_empty() {
            continue;
        }
        match fact.local.as_str() {
            "DocumentType" => doc_type = value,
            "EntityRegistrantName" => doc_org = value,
            "DocumentPeriodEndDate" => doc_period = value,
            _ => {}
        }
    }
    let mut title = format!("{doc_type} {doc_org} {doc_period}")
        .trim()
        .to_string();
    if title.is_empty() {
        title = source.name.clone();
    }
    doc.push(Node::Heading {
        level: 1,
        text: escape_text(&title),
    });

    // Text blocks and the fact graph, in one pass over the facts.
    let mut graph = Graph::default();
    for fact in facts {
        let value = fact.value.as_str();
        if value.is_empty() {
            continue;
        }
        let namespace = fact.namespace.as_str();
        let local = fact.local.as_str();
        let concept = taxonomy.concept(namespace, local);
        let html = value.split_whitespace().collect::<Vec<_>>().join(" ");
        // A concept the taxonomy declares is a text block by type; one it does
        // not (its schema out of reach) by the shape docling's fixtures share:
        // the `…TextBlock` naming convention, or a value that is markup.
        let is_text_block = match concept {
            Some(c) => c.type_name == "textBlockItemType",
            None => local.ends_with("TextBlock") || html.starts_with('<'),
        };
        if is_text_block {
            let block = super::html::convert_html_with("text_block", &html, &NoFetch, false);
            doc.nodes.extend(block.nodes);
            blocks.push(block.tree);
        }
        // Numeric facts are the ones carrying a unit (XBRL requires one of a
        // numeric item and forbids it on any other).
        if let Some(unit_ref) = fact.unit_ref.as_deref() {
            let qname = match fact.prefix.as_deref() {
                Some(prefix) => format!("{prefix}:{local}"),
                None => concept.map_or_else(|| local.to_string(), |c| c.qname()),
            };
            let period = fact
                .context_ref
                .as_deref()
                .and_then(|id| contexts.get(id))
                .map_or("", String::as_str);
            let unit = units.get(unit_ref).map_or("", String::as_str);
            let decimals = fact.decimals.as_deref().unwrap_or("");
            graph.add_fact(local, &qname, value, period, unit, decimals);
        }
    }

    // 1) The presentation hierarchy above each numeric fact's concept.
    let presentation = effective(&taxonomy.presentation);
    let mut parent_of: HashMap<&str, &Arc> = HashMap::new();
    for arc in &presentation {
        parent_of.entry(arc.to.as_str()).or_insert(arc);
    }
    let mut visited: HashSet<String> = HashSet::new();
    for fact in facts {
        if fact.unit_ref.is_none() || fact.value.is_empty() {
            continue;
        }
        let namespace = fact.namespace.as_str();
        let local = fact.local.as_str();
        let concept = taxonomy.concept(namespace, local);
        let fact_qname = match fact.prefix.as_deref() {
            Some(prefix) => format!("{prefix}:{local}"),
            None => concept.map_or_else(|| local.to_string(), |c| c.qname()),
        };
        if !visited.insert(fact_qname.clone()) {
            continue;
        }
        // The concept's own cell, linked down to every fact of it.
        let concept_qname = concept.map_or_else(|| fact_qname.clone(), |c| c.qname());
        if let Some(fact_cells) = graph.fact_cells.get(&fact_qname).cloned() {
            let concept_cell = graph.hierarchy_cell(&concept_qname);
            for fact_cell in fact_cells {
                if fact_cell != concept_cell {
                    graph.add_link("to_child", concept_cell, fact_cell);
                }
            }
        }
        // Then up the presentation tree until a concept already placed.
        let mut current = concept_qname;
        while let Some(arc) = parent_of.get(current.as_str()) {
            let parent = arc.from.clone();
            let child_cell = graph.hierarchy_cell(&current);
            let parent_cell = graph.hierarchy_cell(&parent);
            graph.add_link("to_child", parent_cell, child_cell);
            if !visited.insert(parent.clone()) {
                break;
            }
            current = parent;
        }
    }

    // 2) The calculation relationships, each with its weight.
    for arc in effective(&taxonomy.calculation) {
        let parent_cell = graph.hierarchy_cell(&arc.from);
        let child_cell = graph.hierarchy_cell(&arc.to);
        graph.add_link("to_child", parent_cell, child_cell);
        let weight = arc
            .weight
            .as_deref()
            .and_then(|w| w.trim().parse::<f64>().ok())
            .unwrap_or(1.0);
        let weight_cell = graph.push_cell("value", format!("weight: {weight:?}"), "weight".into());
        graph.add_link("to_value", child_cell, weight_cell);
    }

    let graph = (!graph.cells.is_empty() && !graph.links.is_empty()).then(|| {
        doc.push(Node::KeyValueGraph {
            cells: graph.cells.clone(),
            links: graph.links.clone(),
        });
        (graph.cells, graph.links)
    });
    // A block too deep for the DOM walk has no tree: then the JSON, like the
    // other exports, comes from the flat nodes.
    if blocks.iter().all(Option::is_some) {
        let (tree, redirects) = assemble_tree(title, blocks.into_iter().flatten().collect(), graph);
        redirect_cell_text(&mut doc.nodes, &redirects);
        doc.tree = Some(tree);
    }
    Ok(doc)
}

/// A rich table cell concatenate left pointing at another cell's group: the
/// table (by document order), the cell and the cell whose group it shows.
struct Redirect {
    table: usize,
    cell: (usize, usize),
    shows: (usize, usize),
}

/// docling's Markdown renders a rich cell from the group it references, so a
/// redirected cell shows the other cell's content. Apply that to the flat
/// tables' Markdown grid.
fn redirect_cell_text(nodes: &mut [Node], redirects: &[Redirect]) {
    if redirects.is_empty() {
        return;
    }
    fn tables<'a>(nodes: &'a mut [Node], out: &mut Vec<&'a mut docling_core::Table>) {
        for node in nodes {
            match node {
                Node::Table(table) => out.push(table),
                Node::Group { children, .. } => tables(children, out),
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    tables(nodes, &mut found);
    let mut by_table: HashMap<usize, Vec<&Redirect>> = HashMap::new();
    for r in redirects {
        by_table.entry(r.table).or_default().push(r);
    }
    for (index, table) in found.into_iter().enumerate() {
        let Some(list) = by_table.get(&index) else {
            continue;
        };
        let before = table.rows.clone();
        for r in list {
            let text = before.get(r.shows.0).and_then(|row| row.get(r.shows.1));
            let slot = table
                .rows
                .get_mut(r.cell.0)
                .and_then(|row| row.get_mut(r.cell.1));
            if let (Some(text), Some(slot)) = (text, slot) {
                *slot = text.clone();
            }
        }
    }
}

/// docling's document: the title, each text block's items, the fact graph —
/// numbered as `DoclingDocument.concatenate` numbers them, in traversal
/// order, with its one quirk reproduced. Concatenating re-points a rich
/// table cell at its re-created group by matching the cell's reference
/// against each group's *old* reference in turn, updating the first cell
/// that matches; a cell already re-pointed can match a later group's old
/// reference when the numbers coincide, and then it is re-pointed again,
/// leaving the two cells crossed. The groundtruth carries those crossings,
/// so the merge here replays the same matching over the same numbers.
fn assemble_tree(
    title: String,
    blocks: Vec<ItemTree>,
    graph: Option<(Vec<GraphCell>, Vec<GraphLink>)>,
) -> (ItemTree, Vec<Redirect>) {
    let mut tree = ItemTree::default();
    tree.add(
        None,
        None,
        TreeKind::Text {
            label: "title".into(),
            text: title,
            orig: None,
            formatting: None,
            hyperlink: None,
            level: None,
            list: None,
        },
    );
    // Each group's index within its own block document — the reference
    // concatenate matches rich cells against.
    let mut block_group_index: Vec<Option<usize>> = vec![None];
    for block in blocks {
        for (id, item) in block.items.iter().enumerate() {
            let is_group = !item.deleted && matches!(item.kind, TreeKind::Group { .. });
            block_group_index.push(is_group.then(|| block.bucket_index(id)));
        }
        tree.append(block);
    }
    if let Some((cells, links)) = graph {
        tree.add(None, None, TreeKind::KeyValueGraph { cells, links });
        block_group_index.push(None);
    }
    let new_of = tree.renumber_in_traversal_order();
    let mut old_index: Vec<Option<usize>> = vec![None; tree.items.len()];
    for (old, new) in new_of.iter().enumerate() {
        if let Some(new) = new {
            old_index[*new] = block_group_index[old];
        }
    }
    // The groups by their new index, which is their position in id order.
    let groups: Vec<usize> = (0..tree.items.len())
        .filter(|&id| matches!(tree.items[id].kind, TreeKind::Group { .. }))
        .collect();
    let mut redirects = Vec::new();
    let mut table_index = 0;
    for table in 0..tree.items.len() {
        let TreeKind::Table { rich_cells, .. } = &tree.items[table].kind else {
            continue;
        };
        table_index += 1;
        if rich_cells.is_empty() {
            continue;
        }
        let owners: Vec<(usize, usize, usize)> = rich_cells.clone();
        let mut refs: Vec<Option<usize>> =
            rich_cells.iter().map(|&(_, _, g)| old_index[g]).collect();
        for child in tree.items[table].children.clone() {
            let (Some(old), Ok(new)) = (old_index[child], groups.binary_search(&child)) else {
                continue;
            };
            if let Some(pos) = refs.iter().position(|r| *r == Some(old)) {
                refs[pos] = Some(new);
            }
        }
        let TreeKind::Table { rich_cells, .. } = &mut tree.items[table].kind else {
            unreachable!("checked above");
        };
        for (cell, target) in rich_cells.iter_mut().zip(refs) {
            if let Some(&group) = target.and_then(|i| groups.get(i)) {
                cell.2 = group;
            }
        }
        for &(row, col, group) in rich_cells.iter() {
            if let Some(&(r, c, _)) = owners
                .iter()
                .find(|o| o.2 == group && (o.0, o.1) != (row, col))
            {
                redirects.push(Redirect {
                    table: table_index - 1,
                    cell: (row, col),
                    shows: (r, c),
                });
            }
        }
    }
    (tree, redirects)
}

/// One fact, from an instance's top-level item or an inline report's
/// `ix:nonFraction` / `ix:nonNumeric` — arelle's `ModelFact` as the backend
/// reads it.
pub(super) struct Fact {
    pub(super) namespace: String,
    pub(super) local: String,
    /// The prefix the document binds the namespace to, for the `prefix:local`
    /// QName the graph cells carry.
    pub(super) prefix: Option<String>,
    /// The value (arelle's `ModelFact.value`): stripped text; an inline
    /// fact's after its transformation, scale and sign.
    pub(super) value: String,
    pub(super) context_ref: Option<String>,
    /// Present on numeric facts only.
    pub(super) unit_ref: Option<String>,
    pub(super) decimals: Option<String>,
}

/// What a filing holds for the backend: its taxonomy entry points, its facts
/// in document order, and its contexts' periods and units' measures.
pub(super) struct Filing {
    pub(super) schema_refs: Vec<String>,
    pub(super) facts: Vec<Fact>,
    pub(super) contexts: HashMap<String, String>,
    pub(super) units: HashMap<String, String>,
}

/// An XBRL instance: `link:schemaRef`s, contexts, units and the facts as the
/// root's own children (arelle's `facts`, document order).
fn instance_filing(root: XmlNode) -> Filing {
    let schema_refs = root
        .children()
        .filter(|n| n.tag_name().namespace() == Some(LINK_NS) && n.tag_name().name() == "schemaRef")
        .filter_map(|n| n.attribute((XLINK_NS, "href")))
        .map(str::to_string)
        .collect();
    let facts = root
        .children()
        .filter(|n| {
            n.is_element() && !matches!(n.tag_name().namespace(), Some(XBRLI_NS) | Some(LINK_NS))
        })
        .map(|n| {
            let namespace = n.tag_name().namespace().unwrap_or("");
            Fact {
                namespace: namespace.to_string(),
                local: n.tag_name().name().to_string(),
                prefix: n.lookup_prefix(namespace).map(str::to_string),
                value: fact_value(n).to_string(),
                context_ref: n.attribute("contextRef").map(str::to_string),
                unit_ref: n.attribute("unitRef").map(str::to_string),
                decimals: n.attribute("decimals").map(str::to_string),
            }
        })
        .collect();
    Filing {
        schema_refs,
        facts,
        contexts: periods(root),
        units: unit_measures(root),
    }
}

/// A fact's value: its text, stripped (arelle's `ModelFact.value`).
fn fact_value<'a>(fact: XmlNode<'a, 'a>) -> &'a str {
    fact.text().map_or("", str::trim)
}

/// The key-value graph under construction — docling's cell list, link list
/// and the bookkeeping its backend keeps beside them.
#[derive(Default)]
struct Graph {
    cells: Vec<GraphCell>,
    links: Vec<GraphLink>,
    /// A numeric fact's qualified concept name → the key cells of its facts.
    fact_cells: HashMap<String, Vec<usize>>,
    /// A concept's qualified name → its hierarchy cell.
    hierarchy_cells: HashMap<String, usize>,
    created_links: HashSet<(usize, usize)>,
}

impl Graph {
    fn push_cell(&mut self, label: &str, text: String, orig: String) -> usize {
        let cell_id = self.cells.len();
        self.cells.push(GraphCell {
            label: label.into(),
            cell_id,
            text,
            orig,
        });
        cell_id
    }

    /// One numeric fact: a key cell over its four value cells. docling writes
    /// each value cell even when it has nothing to say (an empty text).
    fn add_fact(
        &mut self,
        local: &str,
        qname: &str,
        value: &str,
        period: &str,
        unit: &str,
        decimals: &str,
    ) {
        let key = self.push_cell("key", local.into(), qname.into());
        self.fact_cells.entry(qname.into()).or_default().push(key);
        let labelled = |prefix: &str, text: &str| {
            if text.is_empty() {
                String::new()
            } else {
                format!("{prefix}: {text}")
            }
        };
        for (text, orig) in [
            (labelled("value", value), "value"),
            (labelled("period", period), "period"),
            (labelled("currency", unit), "unit"),
            (labelled("decimals", decimals), "decimals"),
        ] {
            let cell = self.push_cell("value", text, orig.into());
            self.links.push(GraphLink {
                label: "to_value".into(),
                source_cell_id: key,
                target_cell_id: cell,
            });
        }
    }

    /// The cell standing for a concept in the hierarchy, created on first use.
    fn hierarchy_cell(&mut self, qname: &str) -> usize {
        if let Some(&cell) = self.hierarchy_cells.get(qname) {
            return cell;
        }
        let local = qname.rsplit(':').next().unwrap_or(qname).to_string();
        let cell = self.push_cell("key", local, qname.into());
        self.hierarchy_cells.insert(qname.into(), cell);
        cell
    }

    /// A hierarchy link, once per (source, target) pair.
    fn add_link(&mut self, label: &str, source: usize, target: usize) {
        if self.created_links.insert((source, target)) {
            self.links.push(GraphLink {
                label: label.into(),
                source_cell_id: source,
                target_cell_id: target,
            });
        }
    }
}

/// Each context's period as docling prints it: an instant as its date, a
/// duration as `start - end`, each date as the context declares it
/// (docling#4483: `instantDate` / `endDate`, where arelle's datetimes read a
/// date-only instant or end date as the start of the next day, which docling
/// used to print — one day later than written); a `forever` period prints
/// nothing.
pub(super) fn periods(root: XmlNode) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for context in root.children().filter(|n| is_xbrli(*n, "context")) {
        let Some(id) = context.attribute("id") else {
            continue;
        };
        let Some(period) = context.children().find(|n| is_xbrli(*n, "period")) else {
            continue;
        };
        let child_text = |name: &str| {
            period
                .children()
                .find(|n| is_xbrli(*n, name))
                .and_then(|n| n.text())
                .map(str::trim)
        };
        let text = if let Some(instant) = child_text("instant") {
            date_part(instant).to_string()
        } else if let (Some(start), Some(end)) = (child_text("startDate"), child_text("endDate")) {
            format!("{} - {}", date_part(start), date_part(end))
        } else {
            String::new()
        };
        out.insert(id.to_string(), text);
    }
    out
}

/// Each unit as docling prints it (docling#4568, arelle's `ModelUnit.measures`):
/// its measures' local names joined by a space, a divide's numerator then
/// ` / ` and its denominator (`USD / shares` — per-share facts used to read
/// as plain `USD`); each side sorted by namespace and local name, as
/// arelle's `QName`s sort.
pub(super) fn unit_measures(root: XmlNode) -> HashMap<String, String> {
    let measures = |parent: XmlNode| -> String {
        let mut names: Vec<(String, String)> = parent
            .children()
            .filter(|n| is_xbrli(*n, "measure"))
            .filter_map(|m| {
                let text = m.text()?.trim();
                let (prefix, local) = match text.split_once(':') {
                    Some((p, l)) => (Some(p), l),
                    None => (None, text),
                };
                let namespace = m.lookup_namespace_uri(prefix).unwrap_or("");
                Some((namespace.to_string(), local.to_string()))
            })
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|(_, local)| local)
            .collect::<Vec<_>>()
            .join(" ")
    };
    root.children()
        .filter(|n| is_xbrli(*n, "unit"))
        .filter_map(|unit| {
            let id = unit.attribute("id")?;
            let text = match unit.children().find(|n| is_xbrli(*n, "divide")) {
                Some(divide) => {
                    let side = |name: &str| {
                        divide
                            .children()
                            .find(|n| is_xbrli(*n, name))
                            .map(measures)
                            .unwrap_or_default()
                    };
                    let (num, den) = (side("unitNumerator"), side("unitDenominator"));
                    if num.is_empty() {
                        String::new()
                    } else if den.is_empty() {
                        num
                    } else {
                        format!("{num} / {den}")
                    }
                }
                None => measures(unit),
            };
            (!text.is_empty()).then(|| (id.to_string(), text))
        })
        .collect()
}

fn is_xbrli(node: XmlNode, name: &str) -> bool {
    node.is_element()
        && node.tag_name().namespace() == Some(XBRLI_NS)
        && node.tag_name().name() == name
}

/// The `YYYY-MM-DD` of an XML date or dateTime.
fn date_part(text: &str) -> &str {
    text.split('T').next().unwrap_or(text)
}

/// Whether a generic `.xml` is XBRL (financial facts), used by the converter's
/// XML sniffer.
pub fn looks_like_xbrl(head: &str) -> bool {
    head.contains("us-gaap")
        || head.contains("xbrli:")
        || head.contains("dei:DocumentType")
        || head.contains("http://www.xbrl.org")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    const INSTANCE: &str = r#"<xbrli:xbrl xmlns:xbrli="http://www.xbrl.org/2003/instance"
            xmlns:dei="d" xmlns:us-gaap="u" xmlns:iso4217="http://www.xbrl.org/2003/iso4217">
            <xbrli:context id="c0"><xbrli:entity><xbrli:identifier scheme="s">1</xbrli:identifier></xbrli:entity>
              <xbrli:period><xbrli:startDate>2025-01-01</xbrli:startDate><xbrli:endDate>2025-12-31</xbrli:endDate></xbrli:period></xbrli:context>
            <xbrli:context id="c1"><xbrli:entity><xbrli:identifier scheme="s">1</xbrli:identifier></xbrli:entity>
              <xbrli:period><xbrli:instant>2024-02-29</xbrli:instant></xbrli:period></xbrli:context>
            <xbrli:unit id="usd"><xbrli:measure>iso4217:USD</xbrli:measure></xbrli:unit>
            <dei:DocumentType contextRef="c0">10-Q</dei:DocumentType>
            <dei:EntityRegistrantName contextRef="c0">Acme Inc.</dei:EntityRegistrantName>
            <dei:DocumentPeriodEndDate contextRef="c0">2025-12-31</dei:DocumentPeriodEndDate>
            <us-gaap:NatureOfOperationsTextBlock contextRef="c0">&lt;p&gt;&lt;b&gt;NOTE 1&lt;/b&gt;&lt;/p&gt;&lt;p&gt;Body.&lt;/p&gt;</us-gaap:NatureOfOperationsTextBlock>
            <us-gaap:NatureOfOperationsTextBlock contextRef="c0">&lt;p&gt;&lt;b&gt;NOTE 1&lt;/b&gt;&lt;/p&gt;&lt;p&gt;Body.&lt;/p&gt;</us-gaap:NatureOfOperationsTextBlock>
            <us-gaap:Assets contextRef="c1" unitRef="usd" decimals="-3">1000</us-gaap:Assets>
            <us-gaap:Revenues contextRef="c0" unitRef="usd">50</us-gaap:Revenues>
            <us-gaap:Nil contextRef="c0" unitRef="usd" xsi:nil="true" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"/>
          </xbrli:xbrl>"#;

    fn convert(xml: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("x", InputFormat::XmlXbrl, xml.as_bytes().to_vec());
        XbrlBackend.convert(&src).unwrap()
    }

    #[test]
    fn title_text_blocks_and_placeholder_in_markdown() {
        let md = convert(INSTANCE).export_to_markdown();
        // Repeated text blocks stay, as docling keeps them.
        assert_eq!(
            md.trim_end(),
            "# 10-Q Acme Inc. 2025-12-31\n\n**NOTE 1**\n\nBody.\n\n**NOTE 1**\n\nBody.\n\n<!-- missing-key-value-item -->",
            "got:\n{md:?}"
        );
    }

    #[test]
    fn numeric_facts_become_the_key_value_graph() {
        let doc = convert(INSTANCE);
        let json = doc.export_to_json_value();
        let kv = &json["key_value_items"][0];
        assert_eq!(kv["self_ref"], "#/key_value_items/0");
        assert_eq!(kv["label"], "key_value_region");
        assert_eq!(
            json["body"]["children"].as_array().unwrap().last().unwrap()["$ref"],
            "#/key_value_items/0"
        );
        let cells = kv["graph"]["cells"].as_array().unwrap();
        // Two numeric facts (the nil one has no value) × (key + 4 values),
        // then a hierarchy cell per concept.
        assert_eq!(cells.len(), 12, "{cells:#?}");
        let texts: Vec<&str> = cells.iter().map(|c| c["text"].as_str().unwrap()).collect();
        assert_eq!(
            &texts[..10],
            &[
                "Assets",
                "value: 1000",
                "period: 2024-02-29",
                "currency: USD",
                "decimals: -3",
                "Revenues",
                "value: 50",
                "period: 2025-01-01 - 2025-12-31",
                "currency: USD",
                ""
            ]
        );
        assert_eq!(cells[0]["orig"], "us-gaap:Assets");
        let links = kv["graph"]["links"].as_array().unwrap();
        assert_eq!(
            links[0],
            serde_json::json!({"label": "to_value", "source_cell_id": 0, "target_cell_id": 1})
        );
        // The concept cells link down to their facts.
        assert!(links.iter().any(|l| l["label"] == "to_child"
            && l["source_cell_id"] == 10
            && l["target_cell_id"] == 0));
    }

    /// docling#4568 (`test_xbrl_divide_unit_keeps_denominator`): a divide
    /// unit reports its numerator and its denominator.
    #[test]
    fn divide_unit_keeps_its_denominator() {
        let xml = r#"<xbrli:xbrl xmlns:xbrli="http://www.xbrl.org/2003/instance" xmlns:iso4217="http://www.xbrl.org/2003/iso4217">
            <xbrli:unit id="usd"><xbrli:measure>iso4217:USD</xbrli:measure></xbrli:unit>
            <xbrli:unit id="usdPerShare"><xbrli:divide>
              <xbrli:unitNumerator><xbrli:measure>iso4217:USD</xbrli:measure></xbrli:unitNumerator>
              <xbrli:unitDenominator><xbrli:measure>xbrli:shares</xbrli:measure></xbrli:unitDenominator>
            </xbrli:divide></xbrli:unit></xbrli:xbrl>"#;
        let dom = Document::parse(xml).unwrap();
        let units = unit_measures(dom.root_element());
        assert_eq!(units["usd"], "USD");
        assert_eq!(units["usdPerShare"], "USD / shares");
        // The fixture's per-share facts (grve_10q_htm.xml).
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/xbrl/sources/grve_10q_htm.xml");
        let src = SourceDocument::from_file(&path).unwrap();
        let json = XbrlBackend.convert(&src).unwrap().export_to_json_value();
        let cells = json["key_value_items"][0]["graph"]["cells"]
            .as_array()
            .unwrap();
        let unit_of = |concept: &str| {
            let i = cells.iter().position(|c| c["orig"] == concept).unwrap();
            cells[i + 3]["text"].as_str().unwrap().to_string()
        };
        assert_eq!(
            unit_of("us-gaap:EarningsPerShareDiluted"),
            "currency: USD / shares"
        );
        assert_eq!(unit_of("us-gaap:Assets"), "currency: USD");
    }

    /// docling#4483 (its refreshed groundtruth): an
    /// instant and a duration print their dates as the context declares
    /// them, not arelle's next-day reading of a date-only end.
    #[test]
    fn period_dates_are_reported_as_declared() {
        let dom = Document::parse(INSTANCE).unwrap();
        let periods = periods(dom.root_element());
        assert_eq!(periods["c0"], "2025-01-01 - 2025-12-31");
        assert_eq!(periods["c1"], "2024-02-29");
    }
}
