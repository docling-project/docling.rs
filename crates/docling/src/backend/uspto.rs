//! USPTO patent XML backend — a port of docling's `PatentUsptoDocumentBackend`.
//! Dispatches on the document root to the modern ICE path
//! (`us-patent-application`/`-grant`), the pap-v15 applications path
//! (`patent-application-publication`) or the ST.32 grant path (`PATDOC`). Emits
//! the title (#), the ABSTRACT (###) + text, headings, paragraphs, the CLAIMS,
//! and CALS `<table>`s (ported from docling's `XmlTable`). The legacy APS
//! plain-text format (`pftaps*.txt`) has its own line-oriented parser
//! ([`convert_aps`]); maths are out of scope.

use std::borrow::Cow;

use roxmltree::{Document, Node as XmlNode, ParsingOptions};

use crate::backend::markdown::escape_text;
use crate::backend::uspto_entities::NAMED_ENTITIES;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::tree::{ItemTree, TreeKind};
use docling_core::{DoclingDocument, Node, Table, TableStructure};

/// Whether a plain-text file is a legacy APS (Automated Patent System) patent —
/// its first non-blank line is the `PATN` record marker. docling routes such a
/// `.txt` to `PatentUsptoGrantAps`.
pub fn looks_like_aps(text: &str) -> bool {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim_end() == "PATN")
}

/// Parse a legacy APS patent (Patent Grant Full Text Data/APS, 1976–2001) —
/// a port of docling's `PatentUsptoGrantAps`. The file is a sequence of
/// `KEY  value` lines (two or more spaces separate the key) with indented
/// continuation lines, grouped under bare section markers (`ABST`, `BSUM`,
/// `DETD`, `CLMS`, `DRWD`, …). The title (`TTL`) becomes the document title,
/// the abstract's `PAL` lines one paragraph under an `ABSTRACT` heading, each
/// `PAC` caption a heading, the `PAR`/`PA1`–`PA3` paragraphs its text, and the
/// claims (`NUM` + `PAR`) paragraphs under a `CLAIMS` heading. Everything else
/// (bibliographic records, `##STRn##` structure placeholders) is dropped.
pub fn convert_aps(source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
    let raw = source.text()?;
    let mut aps = Aps::default();
    let (mut section, mut key, mut value) = (String::new(), String::new(), String::new());
    for line in raw.lines() {
        let cols = split_on_double_space(line);
        // A section marker or a new key flushes the pending field.
        let starts_new = match cols {
            (_, None) => true,
            (k, Some(_)) => !k.is_empty(),
        };
        if !key.is_empty() && !value.is_empty() && starts_new {
            aps.store_content(&section, &key, &value);
            key.clear();
            value.clear();
        }
        match cols {
            (title, None) => {
                section = title.to_string();
                aps.store_section(&section);
            }
            (k, Some(v)) if !k.is_empty() => {
                key = k.to_string();
                value = v.to_string();
            }
            // Indented continuation line — unless it is a chemical-structure
            // placeholder (`##STR12##`), which carries no text.
            (_, Some(v)) if !is_structure_placeholder(v) => {
                value.push(' ');
                value.push_str(v);
            }
            _ => {}
        }
    }
    if !key.is_empty() && !value.is_empty() {
        aps.store_content(&section, &key, &value);
    }
    Ok(aps.finish(&source.name))
}

/// `re.split(r"\s{2,}", line, maxsplit=1)`: the text before the first run of
/// two or more whitespace characters and, when there is one, the text after it.
fn split_on_double_space(line: &str) -> (&str, Option<&str>) {
    let mut run_start: Option<usize> = None;
    for (i, ch) in line.char_indices() {
        if ch.is_whitespace() {
            let start = *run_start.get_or_insert(i);
            // Second whitespace char of the run: the run is long enough;
            // extend it to its end and split there.
            if i > start {
                let after = line[i..]
                    .char_indices()
                    .find(|(_, c)| !c.is_whitespace())
                    .map_or(line.len(), |(j, _)| i + j);
                return (&line[..start], Some(&line[after..]));
            }
        } else {
            run_start = None;
        }
    }
    (line, None)
}

/// `^##STR\d+##$` — an APS chemical-structure drawing placeholder.
fn is_structure_placeholder(s: &str) -> bool {
    s.strip_prefix("##STR")
        .and_then(|r| r.strip_suffix("##"))
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// The keys `PatentUsptoGrantAps.Field` knows; any other key's value is
/// ignored.
const APS_FIELDS: &[&str] = &[
    "WKU", "TTL", "PAR", "PA1", "PA2", "PA3", "PAL", "PAC", "NUM", "NAM", "ICL", "ISD", "APD",
    "PNO", "APN", "APT", "CNT",
];

/// docling's APS parser state: the item tree being built and its
/// `level`/`parents` heading hierarchy (`parents[level]` is the item new
/// content goes under; `None` = the body).
struct Aps {
    tree: ItemTree,
    level: i32,
    parents: std::collections::BTreeMap<i32, Option<usize>>,
}

impl Default for Aps {
    fn default() -> Self {
        Self {
            tree: ItemTree::default(),
            level: 1,
            parents: [(1, None)].into_iter().collect(),
        }
    }
}

impl Aps {
    fn parent(&self) -> Option<usize> {
        self.parents.get(&self.level).copied().flatten()
    }

    fn add_text(&mut self, label: &str, text: &str, level: Option<u8>) -> usize {
        let parent = self.parent();
        self.tree.add(
            parent,
            None,
            TreeKind::Text {
                label: label.into(),
                text: text.into(),
                // docling's `orig` is the text at creation: a paragraph the
                // parser later appends to keeps its first fragment here.
                orig: Some(text.into()),
                formatting: None,
                hyperlink: None,
                level,
                list: None,
            },
        )
    }

    /// `add_heading(value, level=L, parent=parents[L])` at the level docling
    /// picks for captions and tagged sections — `PatentHeading.ABSTRACT`'s 2
    /// when a level 2 exists, else 1 — then descend under it.
    fn add_heading_at_base(&mut self, text: &str, base: i32) {
        self.level = if self.parents.contains_key(&base) {
            base
        } else {
            1
        };
        let id = self.add_text("section_header", text, Some(self.level as u8));
        self.parents.insert(self.level + 1, Some(id));
        self.level += 1;
    }

    /// The last text item among the current parent's children (`None` at the
    /// body: docling reads `parent.children` only for a real parent).
    fn last_text_item(&self) -> Option<usize> {
        let parent = self.parent()?;
        self.tree.items[parent]
            .children
            .iter()
            .rev()
            .find(|&&c| matches!(self.tree.items[c].kind, TreeKind::Text { .. }))
            .copied()
    }

    fn append_text(&mut self, id: usize, extra: &str) {
        if let TreeKind::Text { text, .. } = &mut self.tree.items[id].kind {
            text.push_str(extra);
        }
    }

    fn text_of(&self, id: usize) -> &str {
        match &self.tree.items[id].kind {
            TreeKind::Text { text, .. } => text,
            _ => "",
        }
    }

    /// `store_section`: only `ABST` and `CLMS` open a heading of their own;
    /// the other sections' headings come from their `PAC` captions.
    fn store_section(&mut self, section: &str) {
        let heading = match section {
            "ABST" => "ABSTRACT",
            "CLMS" => "CLAIMS",
            _ => return,
        };
        self.add_heading_at_base(heading, 2);
    }

    /// `store_content`: place one `KEY  value` field.
    fn store_content(&mut self, section: &str, field: &str, value: &str) {
        if !APS_FIELDS.contains(&field) {
            return;
        }
        let paragraph_field = matches!(field, "PAR" | "PA1" | "PA2" | "PA3");
        let body_section = matches!(section, "BSUM" | "DETD" | "DRWD");
        if field == "TTL" {
            let id = self.add_text("title", value, None);
            self.parents.insert(self.level + 1, Some(id));
            self.level += 1;
        } else if field == "PAL" && section == "ABST" {
            // The abstract is one paragraph: later `PAL`s append to the first.
            match self.last_text_item() {
                Some(id) => self.append_text(id, &format!(" {value}")),
                None => {
                    self.add_text("paragraph", value, None);
                }
            }
        } else if field == "NUM" && section == "CLMS" {
            // A claim number opens an empty paragraph its `PAR`s fill in.
            self.add_text("paragraph", "", None);
        } else if paragraph_field && section == "CLMS" {
            let id = self
                .last_text_item()
                .unwrap_or_else(|| self.add_text("paragraph", "", None));
            let extra = if self.text_of(id).is_empty() {
                value.trim().to_string()
            } else {
                format!(" {}", value.trim())
            };
            self.append_text(id, &extra);
        } else if field == "PAC" && body_section {
            // Captions are siblings of the abstract: no level information.
            self.add_heading_at_base(value, 2);
        } else if paragraph_field && body_section {
            self.add_text("paragraph", value, None);
        }
    }

    /// The document: docling's item tree for the JSON export and the flat
    /// nodes (title `#`, a level-L heading as `#` × (L+1), paragraphs) for the
    /// other serializers.
    fn finish(self, name: &str) -> DoclingDocument {
        let mut doc = DoclingDocument::new(name);
        for item in &self.tree.items {
            let TreeKind::Text {
                label, text, level, ..
            } = &item.kind
            else {
                continue;
            };
            match label.as_str() {
                "title" => doc.push(Node::Heading {
                    level: 1,
                    text: escape_text(text),
                }),
                "section_header" => doc.push(Node::Heading {
                    level: level.unwrap_or(1) + 1,
                    text: escape_text(text),
                }),
                _ if !text.is_empty() => doc.push(Node::Paragraph {
                    text: escape_text(text),
                }),
                _ => {}
            }
        }
        doc.tree = Some(self.tree);
        doc
    }
}

pub struct UsptoBackend;

impl DeclarativeBackend for UsptoBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let raw = source.text()?;
        super::xml_depth::check(&raw, "uspto")?;
        let mut doc = DoclingDocument::new(&source.name);

        let xml = resolve_named_entities(&raw);
        let opts = ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let dom = Document::parse_with_options(&xml, opts)
            .map_err(|e| ConversionError::with_source("uspto", e))?;

        // Dispatch on the document root, mirroring docling's `_set_parser`.
        match dom.root_element().tag_name().name() {
            "patent-application-publication" => parse_app_v1(&dom, &mut doc),
            root if root.eq_ignore_ascii_case("PATDOC") => parse_grant_v2(&dom, &mut doc),
            _ => parse_ice(&dom, &mut doc), // modern us-patent-application / -grant
        }
        Ok(doc)
    }
}

/// Modern ICE path (`us-patent-application` / `us-patent-grant`, v4x).
fn parse_ice(dom: &Document, doc: &mut DoclingDocument) {
    if let Some(title) = dom
        .descendants()
        .find(|n| n.has_tag_name("invention-title"))
        .map(node_text)
        .filter(|s| !s.is_empty())
    {
        doc.push(Node::Heading {
            level: 1,
            text: escape_text(&title),
        });
    }

    if let Some(abs) = dom.descendants().find(|n| n.has_tag_name("abstract")) {
        let paras = paragraphs(abs);
        if !paras.is_empty() {
            doc.push(Node::Heading {
                level: 3,
                text: "ABSTRACT".into(),
            });
            // docling emits the abstract as a single text item — its
            // paragraphs (with any chemistry-drawing `<p>` dropped as empty)
            // are joined into one, not split per `<p>`.
            doc.push(Node::Paragraph {
                text: escape_text(&paras.join(" ")),
            });
        }
    }

    if let Some(desc) = dom.descendants().find(|n| n.has_tag_name("description")) {
        walk_description(desc, doc);
    }

    if let Some(claims) = dom.descendants().find(|n| n.has_tag_name("claims")) {
        doc.push(Node::Heading {
            level: 3,
            text: "CLAIMS".into(),
        });
        for claim in claims.children().filter(|c| c.has_tag_name("claim")) {
            for ct in claim.children().filter(|c| c.has_tag_name("claim-text")) {
                let t = node_text(ct);
                if !t.is_empty() {
                    doc.push(Node::Paragraph {
                        text: escape_text(&t),
                    });
                }
            }
        }
    }
}

// ===========================================================================
// Legacy application v1.x (`pap-v15`): `patent-application-publication` root.
// Ported from docling's `PatentUsptoAppV1.PatentHandler`.
// ===========================================================================

/// Heading-nesting state machine mirroring docling's `self.level` / `parents`.
struct HeadingLevels {
    level: i32,
    present: std::collections::BTreeSet<i32>,
}

impl HeadingLevels {
    fn new() -> Self {
        let mut present = std::collections::BTreeSet::new();
        present.insert(1);
        Self { level: 1, present }
    }

    /// docling's `PatentHeading` sections (ABSTRACT/CLAIMS, base level 2): the
    /// emitted DocLang level is `base + 1` when `base` is already a known level.
    fn tagged_section_level(&self, base: i32) -> u8 {
        let lvl = if self.present.contains(&base) {
            base
        } else {
            1
        };
        (lvl + 1) as u8
    }

    /// A `<heading lvl="L">`: pick `self.level`, return the DocLang level to
    /// emit, then advance — the port of docling's heading branch.
    fn heading_level(&mut self, lvl_attr: i32) -> u8 {
        let cand = lvl_attr + 1;
        self.level = if self.present.contains(&cand) {
            cand
        } else {
            *self.present.iter().next().unwrap()
        };
        let dclx = (self.level + 1) as u8;
        self.present.insert(self.level + 1);
        self.level += 1;
        dclx
    }
}

/// Raw styled text (super/subscript applied, whitespace *not* collapsed) — what
/// docling accumulates for the abstract (which keeps its trailing space).
fn styled_raw(node: XmlNode) -> String {
    let mut s = String::new();
    raw_text(node, &mut s);
    s
}

fn parse_app_v1(dom: &Document, doc: &mut DoclingDocument) {
    let mut lv = HeadingLevels::new();
    walk_app_v1(dom.root_element(), doc, &mut lv);
}

fn walk_app_v1(node: XmlNode, doc: &mut DoclingDocument, lv: &mut HeadingLevels) {
    for child in node.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "title-of-invention" => {
                let t = node_text(child);
                if !t.is_empty() {
                    doc.push(Node::Heading {
                        level: 1,
                        text: escape_text(&t),
                    });
                    lv.level += 1;
                    lv.present.insert(lv.level);
                }
            }
            "subdoc-abstract" => {
                let abstract_text: String = child
                    .descendants()
                    .filter(|n| n.has_tag_name("paragraph"))
                    .map(styled_raw)
                    .collect();
                if !abstract_text.trim().is_empty() {
                    doc.push(Node::Heading {
                        level: lv.tagged_section_level(2),
                        text: "ABSTRACT".into(),
                    });
                    doc.push(Node::Paragraph {
                        text: escape_text(&abstract_text),
                    });
                }
            }
            "heading" => {
                let lvl_attr = child.attribute("lvl").and_then(as_index).unwrap_or(1) as i32;
                let t = node_text(child);
                if !t.is_empty() {
                    let level = lv.heading_level(lvl_attr);
                    doc.push(Node::Heading {
                        level,
                        text: escape_text(&t),
                    });
                }
            }
            "paragraph" => {
                // docling adds a table at each `<table>` position (fires at
                // `</table>`), then the paragraph's own text at `</paragraph>`.
                if child.descendants().any(|n| n.has_tag_name("table")) {
                    push_tables(child, doc);
                }
                let t = node_text(child);
                if !t.is_empty() {
                    doc.push(Node::Paragraph {
                        text: escape_text(&t),
                    });
                }
            }
            "subdoc-claims" => {
                let claims: Vec<String> = child
                    .descendants()
                    .filter(|n| n.has_tag_name("claim"))
                    .map(node_text)
                    .filter(|s| !s.is_empty())
                    .collect();
                if !claims.is_empty() {
                    doc.push(Node::Heading {
                        level: lv.tagged_section_level(2),
                        text: "CLAIMS".into(),
                    });
                    for claim in claims {
                        doc.push(Node::Paragraph {
                            text: escape_text(&claim),
                        });
                    }
                }
            }
            "tables" | "table" => push_tables(child, doc),
            "math-cwu" | "math" | "maths" => {}
            _ => walk_app_v1(child, doc, lv),
        }
    }
}

// ===========================================================================
// Legacy grant v2 (`PATDOC`, ST.32 `us-grant-025`). Ported from docling's
// `PatentUsptoGrantV2.PatentHandler`. Text lives in `<PDAT>` leaves; the tag
// stack decides styling (`SP`/`SB`/`ITALIC`) and destination (title, abstract,
// paragraph, heading, claim).
// ===========================================================================

/// Styled text with runs of whitespace collapsed to single spaces — PATDOC
/// splits text across many `<PDAT>` leaves separated by source indentation, so
/// collapsing reproduces docling's single-spaced paragraphs/claims.
fn grant_text(node: XmlNode) -> String {
    node_text(node)
}

fn parse_grant_v2(dom: &Document, doc: &mut DoclingDocument) {
    let mut lv = HeadingLevels::new();
    walk_grant_v2(dom.root_element(), doc, &mut lv);
}

fn walk_grant_v2(node: XmlNode, doc: &mut DoclingDocument, lv: &mut HeadingLevels) {
    for child in node.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "B540" => {
                let t = grant_text(child);
                if !t.is_empty() {
                    doc.push(Node::Heading {
                        level: 1,
                        text: escape_text(&t),
                    });
                    lv.level += 1;
                    lv.present.insert(lv.level);
                }
            }
            "SDOAB" => {
                let t = grant_text(child);
                if !t.is_empty() {
                    doc.push(Node::Heading {
                        level: lv.tagged_section_level(2),
                        text: "ABSTRACT".into(),
                    });
                    doc.push(Node::Paragraph {
                        text: escape_text(&t),
                    });
                }
            }
            // A heading inside the claim statement ("What is claimed is:",
            // under <SDOCL>) is skipped; the <CL> under it still yields claims.
            "H" => {
                if child.ancestors().any(|a| a.has_tag_name("SDOCL")) {
                    continue;
                }
                let lvl_attr = child.attribute("LVL").and_then(as_index).unwrap_or(1) as i32;
                let t = grant_text(child);
                if !t.is_empty() {
                    let level = lv.heading_level(lvl_attr);
                    doc.push(Node::Heading {
                        level,
                        text: escape_text(&t),
                    });
                }
            }
            "PARA" => {
                if child.descendants().any(|n| n.has_tag_name("table")) {
                    push_tables(child, doc);
                }
                let t = grant_text(child);
                if !t.is_empty() {
                    doc.push(Node::Paragraph {
                        text: escape_text(&t),
                    });
                }
            }
            "CL" => {
                let claims: Vec<String> = child
                    .children()
                    .filter(|c| c.has_tag_name("CLM"))
                    .map(grant_text)
                    .filter(|s| !s.is_empty())
                    .collect();
                if !claims.is_empty() {
                    doc.push(Node::Heading {
                        level: lv.tagged_section_level(2),
                        text: "CLAIMS".into(),
                    });
                    for claim in claims {
                        doc.push(Node::Paragraph {
                            text: escape_text(&claim),
                        });
                    }
                }
            }
            "tables" | "table" => push_tables(child, doc),
            "CWU" => {}
            _ => walk_grant_v2(child, doc, lv),
        }
    }
}

/// Walk `<description>`: `<heading level="N">` → a heading (`#`×(N+2)), `<p>` →
/// a paragraph; recurse into other containers.
fn walk_description(node: XmlNode, doc: &mut DoclingDocument) {
    for child in node.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "heading" => {
                let level = child
                    .attribute("level")
                    .and_then(|v| v.parse::<u8>().ok())
                    .unwrap_or(1);
                let t = node_text(child);
                if !t.is_empty() {
                    doc.push(Node::Heading {
                        level: level + 2,
                        text: escape_text(&t),
                    });
                }
            }
            "p" => {
                // A `<p>` may wrap `<tables>` (USPTO nests them inside a
                // paragraph). docling adds a table at each `<table>` position and
                // the wrapping paragraph emits no text of its own.
                if child.descendants().any(|n| n.has_tag_name("table")) {
                    push_tables(child, doc);
                } else {
                    let t = node_text(child);
                    if !t.is_empty() {
                        doc.push(Node::Paragraph {
                            text: escape_text(&t),
                        });
                    }
                }
            }
            "maths" => {}
            "tables" | "table" => push_tables(child, doc),
            _ => walk_description(child, doc),
        }
    }
}

// ---------------------------------------------------------------------------
// CALS table parsing — a port of docling's `XmlTable._parse_table` /
// `_create_tg_range`. USPTO tables are CALS (`<tgroup>/<colspec>/<thead>/
// <tbody>/<row>/<entry>`); several `<tgroup>`s with differing column counts are
// merged into one grid `ncols_max` wide, spanned cell text is replicated across
// the physical columns, and empty rows are dropped. The DocLang OTSL overlay
// (`TableStructure`) records header rows and horizontal-span continuations.
// ---------------------------------------------------------------------------

/// Emit a [`Node::Table`] for every `<table>` element in `node`'s subtree
/// (or `node` itself), in document order — mirroring docling, which records one
/// table per `<table>` tag it encounters.
fn push_tables(node: XmlNode, doc: &mut DoclingDocument) {
    let tables: Vec<XmlNode> = if node.has_tag_name("table") {
        vec![node]
    } else {
        node.descendants()
            .filter(|n| n.has_tag_name("table"))
            .collect()
    };
    for tn in tables {
        if let Some(t) = parse_table(tn) {
            doc.push(Node::Table(t));
        }
    }
}

/// Parse a CALS `<colspec>` width (`"42pt"`, `"24.47mm"`) to a number.
fn parse_colwidth(s: &str) -> f64 {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    // Clamp non-finite results (a few hundred digits parses to ±inf, and
    // inf-summed offsets breed NaN) — hostile input must not poison the sort.
    cleaned
        .parse::<f64>()
        .ok()
        .filter(|w| w.is_finite())
        .unwrap_or(0.0)
}

/// Python `str.isnumeric()` for the ASCII digit case we care about.
fn as_index(s: &str) -> Option<i64> {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse::<i64>().ok()
    } else {
        None
    }
}

fn sorted_unique(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v.dedup();
    v
}

/// Build the per-tgroup unified column range mapping (`cell_offst`).
/// `tgs[itg]` = that tgroup's colspec widths. Returns one `cell_offst` vector
/// per tgroup, or `None` if the offsets are inconsistent (docling bails to an
/// empty table).
fn create_tg_range(tgs: &[Vec<f64>]) -> Option<Vec<Vec<i64>>> {
    if tgs.is_empty() {
        return Some(Vec::new());
    }
    // Cumulative column boundary offsets per tgroup (len = ncols + 1).
    let mut offsets: Vec<Vec<f64>> = Vec::with_capacity(tgs.len());
    for cws in tgs {
        let mut off = Vec::with_capacity(cws.len() + 1);
        let mut o = 0.0;
        for &cw in cws {
            off.push(o);
            o += cw;
        }
        off.push(o);
        offsets.push(off);
    }
    // Unified boundary set across all tgroups; zero-width columns are injected
    // back as duplicate boundaries (docling does not de-dupe that final sort).
    let mut min_off: Vec<f64> = offsets[0].clone();
    let mut offset_w0: Vec<f64> = Vec::new();
    for (itg, cws) in tgs.iter().enumerate() {
        for (ic, &cw) in cws.iter().enumerate() {
            if cw == 0.0 {
                offset_w0.push(offsets[itg][ic]);
            }
        }
        let union: Vec<f64> = offsets[itg].iter().chain(min_off.iter()).copied().collect();
        min_off = sorted_unique(union);
    }
    offset_w0 = sorted_unique(offset_w0);
    let mut combined: Vec<f64> = min_off.iter().chain(offset_w0.iter()).copied().collect();
    combined.sort_by(f64::total_cmp);
    min_off = combined;

    // Map each tgroup's columns onto the unified grid.
    let mut result: Vec<Vec<i64>> = Vec::with_capacity(tgs.len());
    for col_offset in &offsets {
        let mut cell_offst: Vec<i64> = vec![0];
        let mut i = 1usize;
        let mut range_: i64 = 1;
        for min_i in 1..min_off.len() {
            let min_offst = min_off[min_i];
            let offst = *col_offset.get(i)?;
            if min_offst == offst {
                if col_offset.len() == i + 1 && min_off.len() > min_i + 1 {
                    range_ += 1;
                } else {
                    cell_offst.push(cell_offst[cell_offst.len() - 1] + range_);
                    range_ = 1;
                    i += 1;
                }
            } else if min_offst < offst {
                range_ += 1;
            } else {
                return None;
            }
        }
        result.push(cell_offst);
    }
    Some(result)
}

/// A cell's plain text — docling uses BeautifulSoup `get_text().strip()` for
/// table entries (no super/subscript translation, unlike paragraph text).
fn cell_text(node: XmlNode) -> String {
    let mut s = String::new();
    for d in node.descendants() {
        if d.is_text() {
            if let Some(t) = d.text() {
                s.push_str(t);
            }
        }
    }
    s.trim().to_string()
}

/// Port of `XmlTable._parse_table`: CALS `<table>` → a [`Table`] with an OTSL
/// [`TableStructure`] overlay. Returns `None` for a broken/empty table.
fn parse_table(table: XmlNode) -> Option<Table> {
    let tgroups: Vec<XmlNode> = table
        .children()
        .filter(|n| n.has_tag_name("tgroup"))
        .collect();
    let tgs_cw: Vec<Vec<f64>> = tgroups
        .iter()
        .map(|tg| {
            tg.children()
                .filter(|n| n.has_tag_name("colspec"))
                .map(|cs| cs.attribute("colwidth").map(parse_colwidth).unwrap_or(0.0))
                .collect()
        })
        .collect();

    let tgs_range = create_tg_range(&tgs_cw)?;
    if tgs_range.is_empty() {
        return None;
    }
    let ncols_max = tgs_cw.iter().map(Vec::len).max().unwrap_or(0);
    if ncols_max == 0 {
        return None;
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut header_row: Vec<bool> = Vec::new();
    let mut col_continuation: Vec<Vec<bool>> = Vec::new();

    for (itg, tg) in tgroups.iter().enumerate() {
        let cell_offst = &tgs_range[itg];
        let row_nodes: Vec<XmlNode> = tg
            .descendants()
            .filter(|n| n.has_tag_name("row") || n.has_tag_name("tr"))
            .collect();
        for row_sec in row_nodes {
            let is_header = row_sec.parent().is_some_and(|p| p.has_tag_name("thead"));
            let entries: Vec<XmlNode> = row_sec
                .children()
                .filter(|n| n.has_tag_name("entry") || n.has_tag_name("td"))
                .collect();

            let mut row_text: Vec<String> = Vec::new();
            let mut row_cont: Vec<bool> = Vec::new();
            let mut ncols = 0usize;
            let mut is_row_empty = true;
            let mut wrong_nbr_cols = false;

            for (ientry, entry) in entries.iter().enumerate() {
                let text = cell_text(*entry);
                let start = entry
                    .attribute("namest")
                    .and_then(as_index)
                    .unwrap_or(ientry as i64 + 1);
                let (end, shift) = match entry.attribute("nameend").and_then(as_index) {
                    Some(e) => (e, 0),
                    None => (ientry as i64 + 2, 1),
                };
                // A malformed span (an index outside the declared columns) marks
                // the row as broken rather than indexing past `cell_offst` —
                // docling#3822 / #179. `start` needs the upper bound too: it is
                // the `namest` attribute, so `namest="9"` on a 2-column tgroup
                // used to panic here instead of degrading the row.
                let n_offst = cell_offst.len() as i64;
                if start < 1 || start > n_offst || end > n_offst {
                    wrong_nbr_cols = true;
                    break;
                }
                let r0 = cell_offst[(start - 1) as usize];
                let r1 = cell_offst[(end - 1) as usize] - shift;
                if !text.is_empty() {
                    is_row_empty = false;
                }
                let mut irep = r0;
                let mut first = true;
                while irep <= r1 {
                    ncols += 1;
                    row_text.push(text.clone());
                    row_cont.push(!first);
                    first = false;
                    irep += 1;
                }
            }

            if wrong_nbr_cols {
                row_text.clear();
                row_cont.clear();
                ncols = 0;
            }
            while ncols < ncols_max {
                row_text.push(String::new());
                row_cont.push(false);
                ncols += 1;
            }

            if !is_row_empty {
                rows.push(row_text);
                col_continuation.push(row_cont);
                header_row.push(is_header);
            }
        }
    }

    if rows.is_empty() {
        return None;
    }
    Some(Table {
        rows,
        location: None,
        structure: Some(TableStructure {
            header_row,
            col_continuation,
            // CALS spans are horizontal-only; no vertical-span continuations.
            row_continuation: Vec::new(),
            row_header: Vec::new(),
            col_header: Vec::new(),
        }),
        cell_blocks: None,
        cells: None,
        caption: None,
        caption_parent: Default::default(),
    })
}

/// Each `<p>` descendant's normalized text.
fn paragraphs(node: XmlNode) -> Vec<String> {
    node.descendants()
        .filter(|n| n.has_tag_name("p"))
        .map(node_text)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Recursive normalized text of a node, skipping `<maths>` (docling drops formulas).
fn node_text(node: XmlNode) -> String {
    let mut s = String::new();
    raw_text(node, &mut s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn raw_text(node: XmlNode, out: &mut String) {
    // Walk every child in order. Text is captured from text-node children
    // directly (not via node.text()/tail shortcuts), so text following a
    // processing instruction or comment — e.g. the leading "R" in
    // `<?in-line-formulae?>R<sup>1</sup>—CO…` — is not dropped.
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                out.push_str(&t.replace('\n', " "));
            }
        } else if child.is_element() {
            match child.tag_name().name() {
                // Super/subscript: <sup>/<sub> (ICE), <superscript>/<subscript>
                // (app-v1), <SP>/<SB> (PATDOC).
                tag @ ("sup" | "sub" | "superscript" | "subscript" | "SP" | "SB") => {
                    let mut inner = String::new();
                    raw_text(child, &mut inner);
                    let sup = matches!(tag, "sup" | "superscript" | "SP");
                    out.extend(inner.chars().map(|c| script_char(c, sup)));
                }
                // PATDOC math italic (<ITALIC>) maps letters to Unicode italics.
                "ITALIC" => {
                    let mut inner = String::new();
                    raw_text(child, &mut inner);
                    out.extend(inner.chars().map(math_italic_char));
                }
                // Formulas, tables and the [NNNN] paragraph number never
                // contribute to surrounding text.
                "maths" | "math-cwu" | "table" | "tables" | "number" | "CWU" => {}
                _ => raw_text(child, out),
            }
        }
    }
}

/// Map an ASCII letter to its Unicode mathematical-italic form for PATDOC
/// `<ITALIC>` runs (docling's `mathematical_italic` table; note 'X' is absent
/// upstream, so it is left unchanged here too).
fn math_italic_char(c: char) -> char {
    match c {
        'A' => '\u{1D434}',
        'B' => '\u{1D435}',
        'C' => '\u{1D436}',
        'D' => '\u{1D437}',
        'E' => '\u{1D438}',
        'F' => '\u{1D439}',
        'G' => '\u{1D43A}',
        'H' => '\u{1D43B}',
        'I' => '\u{1D43C}',
        'J' => '\u{1D43D}',
        'K' => '\u{1D43E}',
        'L' => '\u{1D43F}',
        'M' => '\u{1D440}',
        'N' => '\u{1D441}',
        'O' => '\u{1D442}',
        'P' => '\u{1D443}',
        'Q' => '\u{1D444}',
        'R' => '\u{1D445}',
        'S' => '\u{1D446}',
        'T' => '\u{1D447}',
        'U' => '\u{1D448}',
        'V' => '\u{1D449}',
        'W' => '\u{1D44A}',
        'Y' => '\u{1D44C}',
        'Z' => '\u{1D44D}',
        'a' => '\u{1D44E}',
        'b' => '\u{1D44F}',
        'c' => '\u{1D450}',
        'd' => '\u{1D451}',
        'e' => '\u{1D452}',
        'f' => '\u{1D453}',
        'g' => '\u{1D454}',
        'h' => '\u{1D455}',
        'i' => '\u{1D456}',
        'j' => '\u{1D457}',
        'k' => '\u{1D458}',
        'l' => '\u{1D459}',
        'm' => '\u{1D45A}',
        'n' => '\u{1D45B}',
        'o' => '\u{1D45C}',
        'p' => '\u{1D45D}',
        'q' => '\u{1D45E}',
        'r' => '\u{1D45F}',
        's' => '\u{1D460}',
        't' => '\u{1D461}',
        'u' => '\u{1D462}',
        'v' => '\u{1D463}',
        'w' => '\u{1D464}',
        'x' => '\u{1D465}',
        'y' => '\u{1D466}',
        'z' => '\u{1D467}',
        _ => c,
    }
}

/// Map a digit/sign/letter to its Unicode super- or subscript form (else
/// unchanged) — matching docling's `style_html` translation tables exactly.
fn script_char(c: char, sup: bool) -> char {
    if sup {
        match c {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' | '−' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            'a' => 'ª',
            'o' => 'º',
            'i' => 'ⁱ',
            'n' => 'ⁿ',
            _ => c,
        }
    } else {
        match c {
            '0' => '₀',
            '1' => '₁',
            '2' => '₂',
            '3' => '₃',
            '4' => '₄',
            '5' => '₅',
            '6' => '₆',
            '7' => '₇',
            '8' => '₈',
            '9' => '₉',
            '+' => '₊',
            '-' | '−' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'o' => 'ₒ',
            'x' => 'ₓ',
            _ => c,
        }
    }
}

/// Resolve non-predefined named character references (`&trade;`, `&agr;`,
/// `&lsqb;`, …) into their literal characters so roxmltree — which only knows
/// the five XML built-ins and internally declared entities — can parse legacy
/// USPTO SGML documents. Mirrors docling's `skippedEntity` handling: the ISO
/// 8879 Greek names fold onto their HTML5 counterparts inside the generated
/// table, recognized entities expand, and unrecognized ones are dropped.
///
/// The XML built-ins (`amp`/`lt`/`gt`/`quot`/`apos`) and numeric references
/// (`&#…;`) are left untouched for the parser to resolve. Entity references
/// whose name uses characters outside `[A-Za-z0-9]` (the unparsed-graphics
/// `NDATA` entities USPTO declares in its internal subset) are also dropped —
/// they are illegal in element content and would abort the parse.
fn resolve_named_entities(xml: &str) -> Cow<'_, str> {
    if !xml.contains('&') {
        return Cow::Borrowed(xml);
    }
    let mut out = String::with_capacity(xml.len());
    let mut i = 0;
    while let Some(rel) = xml[i..].find('&') {
        let amp = i + rel;
        out.push_str(&xml[i..amp]);
        // Find the terminating ';' within a bounded window (entity names are short).
        let end = xml[amp + 1..]
            .char_indices()
            .take(64)
            .find(|&(_, c)| c == ';')
            .map(|(off, _)| amp + 1 + off);
        let Some(semi) = end else {
            out.push('&');
            i = amp + 1;
            continue;
        };
        let name = &xml[amp + 1..semi];
        i = semi + 1;
        // Numeric references and the XML built-ins pass through verbatim.
        if name.starts_with('#') || matches!(name, "amp" | "lt" | "gt" | "quot" | "apos") {
            out.push('&');
            out.push_str(name);
            out.push(';');
            continue;
        }
        // Only plain-alphanumeric names can be table entities; anything else is
        // a declared graphics entity — drop it (docling skips those too).
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        // Recognized names expand; unrecognized ones are dropped (docling skips
        // them too).
        if let Ok(idx) = NAMED_ENTITIES.binary_search_by(|&(n, _)| n.cmp(name)) {
            push_xml_escaped(&mut out, NAMED_ENTITIES[idx].1);
        }
    }
    out.push_str(&xml[i..]);
    Cow::Owned(out)
}

/// Append `s`, re-escaping the three characters that would otherwise disturb
/// the surrounding XML once this string is fed back to the parser.
fn push_xml_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    /// The APS plain-text parser (docling's `PatentUsptoGrantAps`): `TTL`
    /// title, the abstract's `PAL`s as one paragraph, `PAC` captions as
    /// headings, claims (`NUM` + `PAR`/`PA1`) as paragraphs under `CLAIMS`;
    /// indented continuation lines join their field, `##STRn##` placeholders
    /// and bibliographic records are dropped.
    #[test]
    fn aps_plain_text_patent() {
        let txt = "PATN\nWKU  012345678\nTTL  Widget with a  double space\nINVT\nNAM  Doe; Jane\n\
                   ABST\nPAL  First part of the\n      abstract.\nPAL  Second part.\n      ##STR1##\n\
                   BSUM\nPAC  BACKGROUND\nPAR  Para one\n      continues.\nPA1  Para two.\n\
                   CLMS\nSTM  What is claimed is:\nNUM  1.\nPAR  1. A widget\n      comprising a knob.\n\
                   PA1  wherein the knob is red.\nNUM  2.\nPAR  2. The widget of claim 1.\n";
        assert!(looks_like_aps(txt));
        let src = SourceDocument::from_bytes("t", InputFormat::Md, txt.as_bytes().to_vec());
        let doc = convert_aps(&src).unwrap();
        assert_eq!(
            doc.export_to_markdown().trim_end(),
            "# Widget with a  double space\n\n### ABSTRACT\n\n\
             First part of the abstract. Second part.\n\n### BACKGROUND\n\n\
             Para one continues.\n\nPara two.\n\n### CLAIMS\n\n\
             1. A widget comprising a knob. wherein the knob is red.\n\n2. The widget of claim 1."
        );
        let v: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = v["texts"].as_array().unwrap();
        // Headings hang off the title; `orig` keeps a paragraph's first
        // fragment (docling appends to `text` only), a claim's is empty.
        assert_eq!(texts[0]["label"], "title");
        assert_eq!(texts[0]["children"].as_array().unwrap().len(), 3);
        assert_eq!(texts[1]["label"], "section_header");
        assert_eq!(texts[1]["level"], 2);
        assert_eq!(texts[1]["parent"]["$ref"], "#/texts/0");
        assert_eq!(texts[2]["orig"], "First part of the abstract.");
        assert_eq!(texts[2]["text"], "First part of the abstract. Second part.");
        assert_eq!(texts[7]["orig"], "");
        assert_eq!(
            texts[7]["text"],
            "1. A widget comprising a knob. wherein the knob is red."
        );
        assert_eq!(texts[7]["parent"]["$ref"], "#/texts/6");
    }

    #[test]
    fn resolves_iso_and_html_entities_and_drops_unknown() {
        assert_eq!(resolve_named_entities("a&trade;b"), "a\u{2122}b");
        assert_eq!(resolve_named_entities("x&agr;y"), "x\u{3b1}y"); // ISO 8879 alpha
        assert_eq!(resolve_named_entities("p&lsqb;q&rsqb;"), "p[q]");
        assert_eq!(
            resolve_named_entities("keep &amp; and &#65;"),
            "keep &amp; and &#65;"
        );
        assert_eq!(resolve_named_entities("drop&zzznope;it"), "dropit");
        assert_eq!(resolve_named_entities("amp&AMP;ersand"), "amp&amp;ersand");
        // Declared NDATA graphics entity (dot in the name) — dropped.
        assert_eq!(resolve_named_entities("g&US001.TIF;h"), "gh");
        assert_eq!(
            resolve_named_entities("no entities here"),
            "no entities here"
        );
    }

    #[test]
    fn title_abstract_headings_and_scripts() {
        let xml = r#"<us-patent-application>
            <us-bibliographic-data-application>
              <invention-title>A Device</invention-title>
            </us-bibliographic-data-application>
            <abstract><p>An H<sub>2</sub>O cell at 10<sup>-3</sup>.</p></abstract>
            <description>
              <heading level="1">BACKGROUND</heading>
              <p>Body of NO<sub>3</sub><sup>-</sup>.</p>
              <heading level="2">Detail</heading>
            </description>
          </us-patent-application>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.starts_with(
                "# A Device\n\n### ABSTRACT\n\nAn H₂O cell at 10⁻³.\n\n### BACKGROUND\n\nBody of NO₃⁻.\n\n#### Detail"
            ),
            "got:\n{md}"
        );
    }

    #[test]
    fn keeps_text_following_a_processing_instruction() {
        // The leading run before an <?in-line-formulae?> PI is the PI's tail;
        // it must not be dropped (docling keeps "R¹—CO", not "¹—CO").
        let xml = r#"<us-patent-application>
            <description>
              <p><?in-line-formulae description="In-line Formulae" end="lead"?>R<sup>1</sup>&#x2014;CO</p>
            </description>
          </us-patent-application>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(md.contains("R¹—CO"), "got:\n{md}");
    }

    fn dclx_of(xml: &str) -> String {
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        UsptoBackend.convert(&src).unwrap().export_to_doclang()
    }

    #[test]
    fn app_v1_title_abstract_heading_levels() {
        // pap-v15: title -> <heading>, abstract joined, heading lvl="1" -> level 3.
        let xml = r#"<patent-application-publication>
            <subdoc-bibliographic-information>
              <title-of-invention>My Widget</title-of-invention>
            </subdoc-bibliographic-information>
            <subdoc-abstract><paragraph>An abstract about H<subscript>2</subscript>O.</paragraph></subdoc-abstract>
            <subdoc-description>
              <section><heading lvl="1">EXAMPLE 1</heading>
                <paragraph id="P-1" lvl="0"><number>[0001]</number> Body text here.</paragraph>
              </section>
            </subdoc-description>
          </patent-application-publication>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.starts_with("# My Widget\n\n### ABSTRACT\n"),
            "got:\n{md}"
        );
        assert!(md.contains("An abstract about H₂O."), "got:\n{md}");
        // The [0001] number is dropped; heading present; body without the number.
        assert!(md.contains("EXAMPLE 1"), "got:\n{md}");
        assert!(
            md.contains("Body text here.") && !md.contains("[0001]"),
            "got:\n{md}"
        );
    }

    #[test]
    fn grant_v2_patdoc_pdat_and_styles() {
        // PATDOC: text in <PDAT>, <SB> subscript, <ITALIC> math-italic, <CWU> skipped.
        let xml = r#"<PATDOC><SDOBI><B540><PTEXT><PDAT>Turbo Code</PDAT></PTEXT></B540></SDOBI>
            <SDODE>
              <H LVL="1"><PTEXT><PDAT>FIELD</PDAT></PTEXT></H>
              <PARA><PTEXT><PDAT>Array N</PDAT><SB><PDAT>1</PDAT></SB><PDAT> uses </PDAT><ITALIC><PDAT>x</PDAT></ITALIC><CWU><PDAT>DROP</PDAT></CWU></PTEXT></PARA>
            </SDODE>
          </PATDOC>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(md.starts_with("# Turbo Code\n"), "got:\n{md}");
        assert!(md.contains("Array N₁ uses"), "subscript wrong:\n{md}");
        assert!(md.contains('\u{1D465}'), "italic x not mapped:\n{md}"); // 𝑥
        assert!(!md.contains("DROP"), "CWU not skipped:\n{md}");
    }

    #[test]
    fn cals_table_spans_and_header() {
        // A `<p>`-wrapped CALS table: `namest`/`nameend` span → `<ched/>`+`<lcel/>`,
        // thead → header, empty `<entry/>` → `<ecel/>`, empty rows dropped.
        let xml = r#"<us-patent-application><description><p><tables><table>
            <tgroup cols="3">
              <colspec colname="1" colwidth="40pt"/>
              <colspec colname="2" colwidth="40pt"/>
              <colspec colname="3" colwidth="40pt"/>
              <thead>
                <row><entry namest="1" nameend="3">Title</entry></row>
              </thead>
              <tbody>
                <row><entry>a</entry><entry>b</entry><entry>c</entry></row>
              </tbody>
            </tgroup>
          </table></tables></p></description></us-patent-application>"#;
        let dclx = dclx_of(xml);
        // Collapse indentation to a token stream for a layout-robust check.
        let toks: Vec<&str> = dclx.split_whitespace().collect();
        let joined = toks.join(" ");
        assert!(
            joined.contains("<ched/> Title <lcel/> <lcel/> <nl/>"),
            "spanning header not <ched/>+<lcel/>×2:\n{dclx}"
        );
        assert!(
            joined.contains("<fcel/> a <fcel/> b <fcel/> c <nl/>"),
            "data row wrong:\n{dclx}"
        );
    }

    /// #179 / docling#3822: a CALS `namest`/`nameend` pointing past the declared
    /// columns must degrade the row, never index out of bounds. The malformed
    /// row is dropped (its cells are cleared, so it reads empty) while a
    /// well-formed sibling table still parses.
    #[test]
    fn cals_table_out_of_range_span_degrades() {
        let malformed = r#"<us-patent-application><description><p><tables><table>
            <tgroup cols="2">
              <colspec colname="1" colwidth="40pt"/>
              <colspec colname="2" colwidth="40pt"/>
              <tbody>
                <row><entry namest="9">a</entry></row>
                <row><entry>x</entry><entry>y</entry></row>
              </tbody>
            </tgroup>
          </table></tables></p></description></us-patent-application>"#;
        let src =
            SourceDocument::from_bytes("p", InputFormat::XmlUspto, malformed.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        // The good row survives; the malformed one contributed no cell text.
        assert!(md.contains("| x"), "well-formed row lost:\n{md}");
        assert!(!md.contains("a"), "out-of-range cell must not land:\n{md}");

        // `nameend` past the columns, and a reversed span, degrade the same way.
        for span in [
            r#"namest="1" nameend="9""#,
            r#"namest="2" nameend="1""#,
            r#"namest="0""#,
        ] {
            let xml = format!(
                r#"<us-patent-application><description><p><tables><table>
                <tgroup cols="2">
                  <colspec colname="1" colwidth="40pt"/>
                  <colspec colname="2" colwidth="40pt"/>
                  <tbody>
                    <row><entry {span}>bad</entry></row>
                    <row><entry>x</entry><entry>y</entry></row>
                  </tbody>
                </tgroup>
              </table></tables></p></description></us-patent-application>"#
            );
            let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.into_bytes());
            let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
            assert!(md.contains("| x"), "span {span:?} lost the good row:\n{md}");
            assert!(!md.contains("bad"), "span {span:?} emitted a cell:\n{md}");
        }
    }

    #[test]
    fn cals_table_drops_empty_rows_and_reads_scripts_as_plain() {
        // An all-empty rule row is dropped; sup/sub in a cell stay plain text
        // (docling uses get_text() for cells, no unicode script translation).
        let xml = r#"<us-patent-application><description><p><tables><table>
            <tgroup cols="2">
              <colspec colname="1" colwidth="40pt"/>
              <colspec colname="2" colwidth="40pt"/>
              <tbody>
                <row><entry/><entry/></row>
                <row><entry>m<sup>2</sup></entry><entry>x</entry></row>
              </tbody>
            </tgroup>
          </table></tables></p></description></us-patent-application>"#;
        let dclx = dclx_of(xml);
        // one data row only (empty row dropped) -> exactly one <nl/> inside the table
        let table = dclx.split("<table>").nth(1).unwrap();
        assert_eq!(
            table.matches("<nl/>").count(),
            1,
            "empty row not dropped:\n{dclx}"
        );
        assert!(
            dclx.contains("m2"),
            "cell script converted (should be plain):\n{dclx}"
        );
    }

    #[test]
    fn abstract_paragraphs_join_into_one_text() {
        // docling emits the abstract as a single text item; a chemistry-drawing
        // <p> in the middle is dropped, the surrounding text stays one paragraph.
        let xml = r#"<us-patent-application>
            <abstract>
              <p>The invention relates to compounds of the formula (I)</p>
              <p><chemistry><img file="C00001.TIF"/></chemistry></p>
              <p>in which X has the meaning given above.</p>
            </abstract>
          </us-patent-application>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlUspto, xml.as_bytes().to_vec());
        let md = UsptoBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.contains(
                "### ABSTRACT\n\nThe invention relates to compounds of the formula (I) in which X has the meaning given above."
            ),
            "got:\n{md}"
        );
    }
}
