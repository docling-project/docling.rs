//! RIS legal XML (#902) — the `risdok` documents of Austria's federal Legal
//! Information System (Rechtsinformationssystem des Bundes, the open-data
//! service `ogd.ris.bka.gv.at`): court and authority decisions and federal
//! law, namespace `http://www.bka.gv.at`. docling has no RIS reader, so this
//! is a docling.rs extension; a bare `.xml` whose root is `<risdok>` comes
//! here ([`looks_like_ris`], read by the `xml` route's dialect sniff), where
//! it used to be kept as one `xml` code block.
//!
//! The document is read as its markup declares it, in source order:
//!
//! * `nutzdaten` (the content) holds `abschnitt` sections — each one a
//!   `section` group named `abschnitt {nr}` in the item tree, so every item
//!   keeps the section it came from; `metadaten` and `layoutdaten` (page
//!   geometry) carry no content and are skipped;
//! * `kzinhalt` / `fzinhalt` (Kopf-/Fußzeile, the running header and footer
//!   of a page variant) are `page_header` / `page_footer` items on the
//!   furniture layer — left out of the Markdown like any page furniture;
//! * `ueberschrift` is a section header (`typ` `titel`/`g1` a level-1 one,
//!   `g2`…`g4` deeper), `absatz` a paragraph whose `b` / `i` / `u` / `sup` /
//!   `sub` runs keep their formatting (an inline group in the item tree) —
//!   a field (`feld`) or `span` reads as its text, a `tab` as a space;
//! * an `absatz` that is a bullet — a glyph (`-`, `–`, `•`, …) or nothing
//!   before its first `tab`, in a hanging indent for the latter — is a list
//!   item, consecutive ones one list; numbered paragraphs (`1.1.`, `3.2.1.`)
//!   stay paragraphs, their numbers part of the legal text;
//! * a `table` (HTML-shaped `tr` / `td` with `absatz` cell content,
//!   `colspan` / `rowspan` honoured) is a table; the zero-height layout row
//!   Word leaves behind is dropped;
//! * `abstand` (vertical spacing) is skipped; an unknown element is read
//!   through — its blocks in place, or one paragraph when it holds only text.
//!
//! The Markdown escapes text as every backend here does (docling's
//! `escape_text`: `_` and the HTML specials) and, unlike docling's, `*` as
//! well: RIS pseudonymizes with stars (`A*** GmbH`, `**** L***berg`,
//! `FN *0*271*h`), which would otherwise open and close emphasis — the
//! plain-text export reads `\*` back as a star. A styled run is wrapped in
//! `**` / `*` against its neighbours with no extra space (docling-core joins
//! an inline group's runs with spaces, which would split `„*x*“`). Table
//! cells keep docling's escaping, as every backend's do.
//!
//! The item tree keeps the source's blocks (#909, as the HTML backend does
//! since #903): every element that produced an item, every element with
//! text of its own, every empty `absatz` / `ueberschrift` / cell, as a
//! `SourceBlock` — tag, every attribute (`typ`, `ct`, `halign`, …), the
//! path `risdok/nutzdaten[1]/abschnitt[1]/absatz[2]`, its normalized text
//! with the inline runs' ranges. An `abschnitt` is the block its section
//! group was made from, with its `nr` / `typ`; any other element with
//! attributes that produced nothing is a container.

use std::collections::HashMap;

use docling_core::tree::{
    BlockOmission, Formatting, ItemTree, ListMeta, SourceBlock, SourceElement, SourceTextBuilder,
    TreeKind,
};
use docling_core::{
    inline_paragraph_node, ContentLayer, DoclingDocument, InlineRun, Node, Script, Table, TableCell,
};
use roxmltree::{Document, Node as XmlNode};

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

/// The RIS namespace (`<risdok xmlns="http://www.bka.gv.at">`).
const RIS_NS: &str = "http://www.bka.gv.at";

/// Bullet glyphs an `absatz` may open with before its first `tab`.
const BULLETS: &[&str] = &["-", "–", "—", "•", "·", "*", "o", "▪", "■", "○", "●", "►"];

/// Whether `marker` is a bullet: one of [`BULLETS`], or a single Symbol /
/// Wingdings glyph — Word stores those fonts' bullets in the private-use
/// block U+F000–U+F0FF (`U+F0B7` is Symbol's `•`), and RIS keeps them so.
fn is_bullet(marker: &str) -> bool {
    let mut chars = marker.chars();
    BULLETS.contains(&marker)
        || matches!(
            (chars.next(), chars.next()),
            (Some('\u{f000}'..='\u{f0ff}'), None)
        )
}

/// Whether the head of an XML file is a RIS document's: its first element is
/// `<risdok>` (the RIS namespace on it, or no namespace at all — the
/// open-data files always declare it, hand-trimmed excerpts may not).
pub(crate) fn looks_like_ris(head: &str) -> bool {
    let Some(tag) = crate::backend::feed::first_element_tag(head) else {
        return false;
    };
    let name = tag
        .trim_start_matches('<')
        .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .next()
        .unwrap_or("");
    name.rsplit(':').next() == Some("risdok") && (tag.contains(RIS_NS) || !tag.contains("xmlns"))
}

/// The RIS reader (#902): see the module docs.
pub struct RisBackend;

impl DeclarativeBackend for RisBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        super::xml_depth::check(&text, "ris")?;
        let dom = Document::parse_with_options(
            &text,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .map_err(|e| ConversionError::Parse(format!("ris: not well-formed XML ({e})")))?;
        let root = dom.root_element();
        if root.tag_name().name() != "risdok" {
            // Not RIS after all: the generic XML reading, with its warning.
            return crate::backend::text::convert_xml_text(source);
        }
        let mut b = Builder::default();
        for part in root.children().filter(XmlNode::is_element) {
            match part.tag_name().name() {
                "nutzdaten" => {
                    for el in part.children().filter(XmlNode::is_element) {
                        if el.tag_name().name() == "abschnitt" {
                            let name = match el.attribute("nr") {
                                Some(nr) => format!("abschnitt {nr}"),
                                None => "abschnitt".to_string(),
                            };
                            let first = b.tree.items.len();
                            let group = b.add(None, None, group("section", &name));
                            b.record(el, first);
                            b.blocks(el, Some(group));
                        } else {
                            b.blocks_of(el, None);
                        }
                    }
                }
                // No content: the (empty, in the open-data files) metadata
                // and the page geometry.
                "metadaten" | "layoutdaten" => {}
                _ => b.blocks_of(part, None),
            }
        }
        let mut doc = DoclingDocument::new(&source.name);
        let mut tree = b.tree;
        // Our own tree, so DocLang serializes it too (docling-core's
        // serializer over the tree, #884): formatting runs and the section
        // groups reach it as they reach the JSON.
        tree.doclang = true;
        // The runs keep their own whitespace (schema 1.11.0, #997).
        tree.inline_whitespace = true;
        collect_blocks(root, &b.el_of, &mut tree);
        doc.nodes = b.nodes;
        doc.tree = Some(tree);
        Ok(doc)
    }
}

/// The inline formatting an element puts on its text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Fmt {
    bold: bool,
    italic: bool,
    underline: bool,
    script: Script,
}

impl Fmt {
    fn with(self, tag: &str) -> Self {
        match tag {
            "b" | "strong" | "fett" => Fmt { bold: true, ..self },
            "i" | "em" | "kursiv" => Fmt {
                italic: true,
                ..self
            },
            "u" => Fmt {
                underline: true,
                ..self
            },
            "sup" => Fmt {
                script: Script::Super,
                ..self
            },
            "sub" => Fmt {
                script: Script::Sub,
                ..self
            },
            _ => self,
        }
    }

    fn is_plain(self) -> bool {
        self == Fmt::default()
    }

    fn formatting(self) -> Option<Formatting> {
        (!self.is_plain()).then_some(Formatting {
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strikethrough: false,
            script: self.script,
        })
    }

    fn run(self, text: &str) -> InlineRun {
        InlineRun {
            text: text.to_string(),
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            script: self.script,
            ..InlineRun::default()
        }
    }
}

/// Collect `el`'s inline content as `(text, formatting)` runs: formatting
/// elements set their style, `tab` / `br` read as a space, `amp` (the empty
/// element risdok writes the ampersand as, #918) as `&`, `aw` (an empty
/// paragraph mark) as nothing, any other element as its content.
fn collect(el: XmlNode, fmt: Fmt, out: &mut Vec<(String, Fmt)>) {
    for n in el.children() {
        if let Some(t) = n.text().filter(|_| n.is_text()) {
            out.push((t.to_string(), fmt));
        } else if n.is_element() {
            match n.tag_name().name() {
                "tab" | "br" => out.push((" ".to_string(), fmt)),
                "amp" => out.push(("&".to_string(), fmt)),
                "aw" => {}
                tag => collect(n, fmt.with(tag), out),
            }
        }
    }
}

/// The runs, whitespace collapsed to single spaces across run boundaries,
/// the whole trimmed, adjacent runs of one formatting merged and empty ones
/// dropped. A run of nothing but a space (the `tab` between a bold number
/// and a bold heading) joins the run before it rather than splitting the
/// paragraph into a blank item.
fn normalize(raw: Vec<(String, Fmt)>) -> Vec<(String, Fmt)> {
    let mut runs: Vec<(String, Fmt)> = Vec::new();
    let mut prev_space = true;
    for (text, fmt) in raw {
        let mut s = String::new();
        for c in text.chars() {
            if c.is_whitespace() {
                if !prev_space {
                    s.push(' ');
                }
                prev_space = true;
            } else {
                s.push(c);
                prev_space = false;
            }
        }
        if s.is_empty() {
            continue;
        }
        match runs.last_mut() {
            Some((t, f)) if *f == fmt || s == " " => t.push_str(&s),
            _ => runs.push((s, fmt)),
        }
    }
    if let Some((t, _)) = runs.last_mut() {
        let trimmed = t.trim_end().len();
        t.truncate(trimmed);
    }
    runs.retain(|(t, _)| !t.is_empty());
    runs
}

/// The Markdown of a run sequence: each run escaped, the styled ones wrapped
/// in their markers against their neighbours (the edge spaces moved outside
/// the markers). Underline and sub/superscript have no Markdown and print
/// plain, so runs that only differ in those merge first.
fn markdown(runs: &[(String, Fmt)]) -> String {
    let mut merged: Vec<(String, bool, bool)> = Vec::new();
    for (t, f) in runs {
        match merged.last_mut() {
            Some((mt, b, i)) if *b == f.bold && *i == f.italic => mt.push_str(t),
            _ => merged.push((t.clone(), f.bold, f.italic)),
        }
    }
    let mut out = String::new();
    for (t, bold, italic) in merged {
        let marker = match (bold, italic) {
            (true, true) => "***",
            (true, false) => "**",
            (false, true) => "*",
            (false, false) => "",
        };
        let core = t.trim();
        if marker.is_empty() || core.is_empty() {
            out.push_str(&md_escape(&t));
            continue;
        }
        let lead = &t[..t.len() - t.trim_start().len()];
        let trail = &t[t.trim_end().len()..];
        out.push_str(lead);
        out.push_str(marker);
        out.push_str(&md_escape(core));
        out.push_str(marker);
        out.push_str(trail);
    }
    out
}

/// Markdown text: docling's escaping, and the stars (module docs).
fn md_escape(text: &str) -> String {
    escape_text(text).replace('*', "\\*")
}

fn plain(runs: &[(String, Fmt)]) -> String {
    runs.iter().map(|(t, _)| t.as_str()).collect()
}

fn text_kind(label: &str, text: &str, fmt: Option<Formatting>) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting: fmt,
        hyperlink: None,
        level: None,
        list: (label == "list_item").then(ListMeta::default),
    }
}

fn group(label: &str, name: &str) -> TreeKind {
    TreeKind::Group {
        label: label.into(),
        name: name.into(),
    }
}

/// The section-header level of an `ueberschrift` `typ`: the judicature's
/// `titel` headings and law's top `g1` outline level are level 1, `g2`…`g4`
/// one deeper each; anything else (`para`, `art`, …) level 1.
fn heading_level(typ: Option<&str>) -> u8 {
    match typ {
        Some("g2") => 2,
        Some("g3") => 3,
        Some("g4") | Some("g5") => 4,
        _ => 1,
    }
}

/// The item tree and the flat nodes, built side by side in source order.
#[derive(Default)]
struct Builder {
    tree: ItemTree,
    nodes: Vec<Node>,
    /// The open list: its tree group and the parent it hangs in.
    list: Option<(usize, Option<usize>)>,
    /// The element each item was created for (#909): item → element.
    el_of: HashMap<usize, roxmltree::NodeId>,
}

impl Builder {
    fn add(&mut self, parent: Option<usize>, layer: Option<ContentLayer>, kind: TreeKind) -> usize {
        self.tree.add(parent, layer, kind)
    }

    /// Tie every item created since `first` to `el`, unless a nested
    /// creation already tied it to the element it was really made for.
    fn record(&mut self, el: XmlNode, first: usize) {
        for i in first..self.tree.items.len() {
            self.el_of.entry(i).or_insert_with(|| el.id());
        }
    }

    /// The blocks of a section (or another container), in order.
    fn blocks(&mut self, el: XmlNode, parent: Option<usize>) {
        for child in el.children().filter(XmlNode::is_element) {
            self.block(child, parent);
        }
        self.list = None;
    }

    /// An element read through: its blocks, or one paragraph when it holds
    /// only inline content.
    fn blocks_of(&mut self, el: XmlNode, parent: Option<usize>) {
        let has_blocks = el.children().any(|c| {
            c.is_element()
                && matches!(
                    c.tag_name().name(),
                    "absatz" | "ueberschrift" | "table" | "kzinhalt" | "fzinhalt" | "abschnitt"
                )
        });
        if has_blocks {
            self.blocks(el, parent);
        } else {
            self.list = None;
            self.paragraph(el, parent);
        }
    }

    fn block(&mut self, el: XmlNode, parent: Option<usize>) {
        let tag = el.tag_name().name();
        if tag != "absatz" {
            self.list = None;
        }
        match tag {
            "kzinhalt" => self.furniture(el, parent, "page_header"),
            "fzinhalt" => self.furniture(el, parent, "page_footer"),
            "ueberschrift" => self.heading(el, parent),
            "absatz" => {
                if !self.list_item(el, parent) {
                    self.list = None;
                    self.paragraph(el, parent);
                }
            }
            "table" => self.table(el, parent),
            "abstand" => {}
            _ => self.blocks_of(el, parent),
        }
    }

    /// A running header or footer: each of its paragraphs one furniture item.
    fn furniture(&mut self, el: XmlNode, parent: Option<usize>, label: &str) {
        for p in el.descendants().filter(|n| n.has_tag_name("absatz")) {
            let mut raw = Vec::new();
            collect(p, Fmt::default(), &mut raw);
            let text = plain(&normalize(raw));
            if text.is_empty() {
                continue;
            }
            let first = self.tree.items.len();
            self.add(
                parent,
                Some(ContentLayer::Furniture),
                text_kind(label, &text, None),
            );
            self.record(p, first);
            self.nodes.push(Node::FurnitureText {
                label: label.into(),
                text: md_escape(&text),
            });
        }
    }

    fn heading(&mut self, el: XmlNode, parent: Option<usize>) {
        let mut raw = Vec::new();
        collect(el, Fmt::default(), &mut raw);
        let text = plain(&normalize(raw));
        if text.is_empty() {
            return;
        }
        let level = heading_level(el.attribute("typ"));
        let mut kind = text_kind("section_header", &text, None);
        if let TreeKind::Text { level: l, .. } = &mut kind {
            *l = Some(level);
        }
        let first = self.tree.items.len();
        self.add(parent, None, kind);
        self.record(el, first);
        self.nodes.push(Node::Heading {
            level: level + 1,
            text: md_escape(&text),
        });
    }

    /// A paragraph: a text item, or an inline group of its runs.
    fn paragraph(&mut self, el: XmlNode, parent: Option<usize>) {
        let mut raw = Vec::new();
        collect(el, Fmt::default(), &mut raw);
        let runs = normalize(raw);
        if runs.is_empty() {
            return;
        }
        let first = self.tree.items.len();
        match runs.as_slice() {
            [(text, fmt)] => {
                self.add(parent, None, text_kind("text", text, fmt.formatting()));
            }
            _ => {
                let g = self.add(parent, None, group("inline", "group"));
                for (text, fmt) in &runs {
                    self.add(Some(g), None, text_kind("text", text, fmt.formatting()));
                }
            }
        }
        self.record(el, first);
        let inline: Vec<InlineRun> = runs.iter().map(|(t, f)| f.run(t)).collect();
        self.nodes
            .push(inline_paragraph_node(markdown(&runs), inline, false));
    }

    /// An `absatz` that is a bullet (module docs) as a list item; `false`
    /// when it is not one.
    fn list_item(&mut self, el: XmlNode, parent: Option<usize>) -> bool {
        let Some(tab) = el.children().find(|n| n.has_tag_name("tab")) else {
            return false;
        };
        let mut before = String::new();
        for n in el.children().take_while(|n| *n != tab) {
            before.push_str(
                &n.descendants()
                    .filter_map(|d| d.text().filter(|_| d.is_text()))
                    .collect::<String>(),
            );
        }
        let marker = before.trim();
        let hanging = el
            .attribute("style")
            .is_some_and(|s| s.replace(' ', "").contains("text-indent:-"));
        if !(is_bullet(marker) || (marker.is_empty() && hanging)) {
            return false;
        }
        let mut raw = Vec::new();
        for n in el.children().skip_while(|n| *n != tab).skip(1) {
            if let Some(t) = n.text().filter(|_| n.is_text()) {
                raw.push((t.to_string(), Fmt::default()));
            } else if n.is_element() {
                match n.tag_name().name() {
                    "tab" | "br" => raw.push((" ".to_string(), Fmt::default())),
                    "amp" => raw.push(("&".to_string(), Fmt::default())),
                    "aw" => {}
                    tag => collect(n, Fmt::default().with(tag), &mut raw),
                }
            }
        }
        let runs = normalize(raw);
        if runs.is_empty() {
            return false;
        }
        let first_in_list = !matches!(self.list, Some((_, p)) if p == parent);
        if first_in_list {
            let g = self.add(parent, None, group("list", "list"));
            self.list = Some((g, parent));
        }
        let (list, _) = self.list.expect("open list");
        let text = plain(&runs);
        let first = self.tree.items.len();
        let item = self.add(Some(list), None, text_kind("list_item", &text, None));
        if runs.len() > 1 || !runs[0].1.is_plain() {
            // A styled item: its runs as an inline group under the item.
            if let TreeKind::Text { text, .. } = &mut self.tree.items[item].kind {
                text.clear();
            }
            let g = self.add(Some(item), None, group("inline", "group"));
            for (t, f) in &runs {
                self.add(Some(g), None, text_kind("text", t, f.formatting()));
            }
        }
        self.record(el, first);
        self.nodes.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list,
            text: markdown(&runs),
            level: 0,
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        true
    }

    /// A `table`: its rows (`tr`, in `thead` / `tbody` too) as a cell grid,
    /// spans honoured, a cell's paragraphs joined by spaces.
    fn table(&mut self, el: XmlNode, parent: Option<usize>) {
        let rows: Vec<XmlNode> = el
            .descendants()
            .filter(|n| n.has_tag_name("tr"))
            // Only this table's rows, not a nested table's.
            .filter(|tr| {
                tr.ancestors()
                    .skip(1)
                    .find(|a| a.has_tag_name("table"))
                    .is_some_and(|t| t == el)
            })
            .collect();
        let mut cells: Vec<TableCell> = Vec::new();
        // Grid occupancy by earlier rowspans: (row, col) taken.
        let mut taken: std::collections::HashSet<(usize, usize)> = Default::default();
        let mut r = 0usize;
        for tr in rows {
            let tds: Vec<XmlNode> = tr
                .children()
                .filter(|n| n.has_tag_name("td") || n.has_tag_name("th"))
                .collect();
            let texts: Vec<String> = tds.iter().map(|td| cell_text(*td)).collect();
            // Word's zero-height layout row: empty cells that only carry widths.
            if texts.iter().all(String::is_empty)
                && (tr.attribute("height") == Some("0") || tds.iter().all(|td| !td.has_children()))
            {
                continue;
            }
            let mut c = 0usize;
            for (td, text) in tds.iter().zip(texts) {
                while taken.contains(&(r, c)) {
                    c += 1;
                }
                let span = |a: &str| {
                    td.attribute(a)
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .filter(|&n| n > 0)
                        .unwrap_or(1)
                        .min(1000)
                };
                let (rs, cs) = (span("rowspan"), span("colspan"));
                for dr in 0..rs {
                    for dc in 0..cs {
                        taken.insert((r + dr, c + dc));
                    }
                }
                cells.push(TableCell {
                    text,
                    bbox: None,
                    start_row: r,
                    start_col: c,
                    row_span: rs,
                    col_span: cs,
                    column_header: td.has_tag_name("th"),
                    row_header: false,
                    row_section: false,
                });
                c += cs;
            }
            r += 1;
        }
        if cells.is_empty() {
            return;
        }
        let num_rows = cells
            .iter()
            .map(|c| c.start_row + c.row_span)
            .max()
            .unwrap_or(0);
        let num_cols = cells
            .iter()
            .map(|c| c.start_col + c.col_span)
            .max()
            .unwrap_or(0);
        let grid = |escape: bool| -> Vec<Vec<String>> {
            let mut g = vec![vec![String::new(); num_cols]; num_rows];
            for cell in &cells {
                let t = if escape {
                    escape_text(&cell.text)
                } else {
                    cell.text.clone()
                };
                for row in g.iter_mut().skip(cell.start_row).take(cell.row_span) {
                    for slot in row.iter_mut().skip(cell.start_col).take(cell.col_span) {
                        slot.clone_from(&t);
                    }
                }
            }
            g
        };
        let tree_table = Table {
            rows: grid(false),
            cells: Some(cells.clone()),
            ..Table::default()
        };
        let first = self.tree.items.len();
        self.add(
            parent,
            None,
            TreeKind::Table {
                table: tree_table,
                rich_cells: Vec::new(),
                captions: Vec::new(),
            },
        );
        self.record(el, first);
        let flat_rows = grid(true);
        let flat_cells = cells
            .into_iter()
            .map(|c| TableCell {
                text: escape_text(&c.text),
                ..c
            })
            .collect();
        self.nodes.push(Node::Table(Table {
            rows: flat_rows,
            cells: Some(flat_cells),
            ..Table::default()
        }));
    }
}

/// The RIS elements whose text is read inline — part of the block around
/// them, with their range in its text (#911) — rather than blocks.
fn inline_tag(name: &str) -> bool {
    matches!(
        name,
        "b" | "i"
            | "u"
            | "sup"
            | "sub"
            | "span"
            | "feld"
            | "tab"
            | "br"
            | "amp"
            | "aw"
            | "s"
            | "strike"
    )
}

/// The elements that are a block even when empty.
const LEAF_TAGS: &[&str] = &["absatz", "ueberschrift", "td", "th"];

/// #909: enumerate the source's blocks in document order and tie each item
/// to the one it was made from — the HTML backend's rule (#903), on the RIS
/// tree: every element that produced an item, every non-inline element
/// with text of its own, every empty paragraph / heading / cell. An
/// `abschnitt` (or any container) that carries attributes is a container
/// with its identity; an element the reading skips — `metadaten`,
/// `layoutdaten`, `abstand` — explains itself. The list group a run of
/// bullet paragraphs shares belongs to no single element and has no block.
fn collect_blocks(root: XmlNode, el_of: &HashMap<usize, roxmltree::NodeId>, tree: &mut ItemTree) {
    let mut all: HashMap<roxmltree::NodeId, Vec<usize>> = HashMap::new();
    let mut ids: Vec<usize> = el_of.keys().copied().collect();
    ids.sort_unstable();
    for i in ids {
        all.entry(el_of[&i]).or_default().push(i);
    }
    let mut c = RisCollector {
        tree,
        all: &all,
        el_of,
        blocks: Vec::new(),
        block_of: HashMap::new(),
        containers: Vec::new(),
    };
    c.visit(root, 1, "", false, None, None, None);
    let (blocks, block_of, containers) = (c.blocks, c.block_of, c.containers);
    for (&i, el) in el_of {
        tree.items[i].block = block_of.get(el).copied();
    }
    tree.blocks = blocks;
    tree.containers = containers;
}

struct RisCollector<'t, 'a> {
    tree: &'t mut ItemTree,
    all: &'a HashMap<roxmltree::NodeId, Vec<usize>>,
    el_of: &'a HashMap<usize, roxmltree::NodeId>,
    blocks: Vec<SourceBlock>,
    block_of: HashMap<roxmltree::NodeId, usize>,
    containers: Vec<SourceElement>,
}

/// An element's attributes, by name.
fn xml_attrs(el: XmlNode) -> Vec<(String, String)> {
    let mut attrs: Vec<(String, String)> = el
        .attributes()
        .map(|a| (a.name().to_string(), a.value().to_string()))
        .collect();
    attrs.sort();
    attrs
}

impl RisCollector<'_, '_> {
    /// `pos`: the element's 1-based position among its parent's same-tag
    /// element children; `skipped`: inside `metadaten` / `layoutdaten` /
    /// `abstand`, which the reading leaves out; `table` the enclosing
    /// table's item, `cell_table` the same when inside a cell (whose text
    /// is in the grid); `block` the nearest enclosing block with items.
    #[allow(clippy::too_many_arguments)]
    fn visit(
        &mut self,
        el: XmlNode,
        pos: usize,
        parent_path: &str,
        skipped: bool,
        table: Option<usize>,
        cell_table: Option<usize>,
        block: Option<usize>,
    ) {
        let tag = el.tag_name().name();
        let path = if parent_path.is_empty() {
            tag.to_string()
        } else {
            format!("{parent_path}/{tag}[{pos}]")
        };
        let skipped = skipped || matches!(tag, "metadaten" | "layoutdaten" | "abstand");
        let items: Vec<usize> = self
            .all
            .get(&el.id())
            .map(|v| {
                v.iter()
                    .copied()
                    .filter(|&i| {
                        self.tree.items[i]
                            .parent
                            .is_none_or(|p| self.el_of.get(&p) != Some(&el.id()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let table = if tag == "table" {
            items
                .iter()
                .copied()
                .find(|&i| matches!(self.tree.items[i].kind, TreeKind::Table { .. }))
                .or(table)
        } else {
            table
        };
        let cell_table = if matches!(tag, "td" | "th") {
            table
        } else {
            cell_table
        };
        let (text, inlines) = own_text(el, &path, self.all);
        let is_block = !items.is_empty()
            || (!inline_tag(tag) && (!text.is_empty() || LEAF_TAGS.contains(&tag)));
        let mut block = block;
        if is_block {
            let omitted = if !items.is_empty() {
                None
            } else if skipped {
                Some(BlockOmission::Hidden)
            } else if let Some(t) = cell_table {
                Some(BlockOmission::InTable(t))
            } else if text.is_empty() {
                Some(BlockOmission::Empty)
            } else if let Some(b) = block {
                Some(BlockOmission::Merged(b))
            } else {
                Some(BlockOmission::Unmapped)
            };
            let idx = self.blocks.len();
            self.block_of.insert(el.id(), idx);
            self.blocks.push(SourceBlock {
                tag: tag.to_string(),
                id: el.attribute("id").map(str::to_string),
                class: el.attribute("class").map(str::to_string),
                attrs: xml_attrs(el),
                path: path.clone(),
                text,
                items: items.clone(),
                omitted,
                inlines,
            });
            if !items.is_empty() {
                block = Some(idx);
            }
        } else if el.attributes().next().is_some() {
            self.containers.push(SourceElement {
                tag: tag.to_string(),
                id: el.attribute("id").map(str::to_string),
                class: el.attribute("class").map(str::to_string),
                attrs: xml_attrs(el),
                path: path.clone(),
            });
        }
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for child in el.children().filter(XmlNode::is_element) {
            let n = seen.entry(child.tag_name().name()).or_insert(0);
            *n += 1;
            let n = *n;
            self.visit(child, n, &path, skipped, table, cell_table, block);
        }
    }
}

/// An element's own text (#904): its text nodes and those of the inline
/// elements inside it (a `tab` / `br` as a space), nested blocks' text left
/// out, whitespace normalized — with every inline element's range (#911).
fn own_text(
    el: XmlNode,
    path: &str,
    all: &HashMap<roxmltree::NodeId, Vec<usize>>,
) -> (String, Vec<docling_core::tree::SourceInline>) {
    fn gather(
        node: XmlNode,
        path: &str,
        all: &HashMap<roxmltree::NodeId, Vec<usize>>,
        b: &mut SourceTextBuilder,
    ) {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for child in node.children() {
            if let Some(t) = child.text().filter(|_| child.is_text()) {
                b.push_text(t);
            } else if child.is_element() {
                let name = child.tag_name().name();
                let n = seen.entry(name).or_insert(0);
                *n += 1;
                let n = *n;
                if !inline_tag(name) || all.contains_key(&child.id()) {
                    continue;
                }
                match name {
                    "tab" | "br" => b.push_text(" "),
                    "amp" => b.push_text("&"),
                    "aw" => {}
                    _ => {
                        let child_path = format!("{path}/{name}[{n}]");
                        b.open_inline(name, xml_attrs(child), child_path.clone());
                        gather(child, &child_path, all, b);
                        b.close_inline();
                    }
                }
            }
        }
    }
    let mut b = SourceTextBuilder::new();
    gather(el, path, all, &mut b);
    b.finish()
}

/// A table cell's text: its paragraphs (or its inline content when it has
/// none), each normalized, joined by a space.
fn cell_text(td: XmlNode) -> String {
    let paras: Vec<XmlNode> = td
        .descendants()
        .filter(|n| n.has_tag_name("absatz"))
        .collect();
    let parts: Vec<String> = if paras.is_empty() {
        let mut raw = Vec::new();
        collect(td, Fmt::default(), &mut raw);
        vec![plain(&normalize(raw))]
    } else {
        paras
            .into_iter()
            .map(|p| {
                let mut raw = Vec::new();
                collect(p, Fmt::default(), &mut raw);
                plain(&normalize(raw))
            })
            .collect()
    };
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(xml: &str) -> DoclingDocument {
        RisBackend
            .convert(&SourceDocument::from_bytes(
                "ris.xml",
                InputFormat::XmlRis,
                xml.as_bytes().to_vec(),
            ))
            .expect("convert")
    }

    fn wrap(body: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><risdok xmlns=\"http://www.bka.gv.at\">\
             <metadaten /><nutzdaten><abschnitt nr=\"1\" typ=\"ns\">{body}</abschnitt>\
             </nutzdaten></risdok>"
        )
    }

    /// #918: risdok writes the ampersand as the empty element `<amp />`; it
    /// reads as `&` in the item text and in the source block.
    #[test]
    fn amp_element_reads_as_ampersand() {
        let doc = convert(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <risdok xmlns=\"http://www.bka.gv.at\"><metadaten/><nutzdaten>\
             <abschnitt nr=\"1\" typ=\"ns\"><absatz typ=\"erltext\">Schenker <amp /> Co. AG</absatz>\
             </abschnitt></nutzdaten></risdok>",
        );
        assert_eq!(doc.export_to_text(), "Schenker & Co. AG");
        let t = doc.tree.as_ref().unwrap();
        let block = t
            .blocks
            .iter()
            .find(|b| b.path.ends_with("absatz[1]"))
            .unwrap();
        assert_eq!(block.text, "Schenker & Co. AG");
    }

    /// #909: the issue's reduction — a heading and a paragraph in a section
    /// — comes back as two source blocks with stable paths under
    /// `risdok/nutzdaten/abschnitt`, their attributes and normalized text;
    /// the section is the block its group was made from, with its `nr` /
    /// `typ`; the metadata is a skipped block when it holds text.
    #[test]
    fn source_blocks_locate_the_elements() {
        let doc = convert(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><risdok xmlns=\"http://www.bka.gv.at\">\
             <metadaten><titel>Meta</titel></metadaten><nutzdaten>\
             <abschnitt nr=\"1\" typ=\"ns\" paperf=\"n\" endnhier=\"n\">\
             <ueberschrift typ=\"titel\" halign=\"c\">Entscheidende Behörde</ueberschrift>\
             <absatz typ=\"erltext\" ct=\"text\" halign=\"l\">  Daten<b>schutz</b>behörde \
             <span>(DSB)</span></absatz><absatz typ=\"erltext\"></absatz>\
             </abschnitt></nutzdaten></risdok>",
        );
        let t = doc.tree.as_ref().unwrap();
        let blocks: Vec<(&str, &str, Option<BlockOmission>)> = t
            .blocks
            .iter()
            .map(|b| (b.path.as_str(), b.text.as_str(), b.omitted))
            .collect();
        assert_eq!(
            blocks,
            vec![
                (
                    "risdok/metadaten[1]/titel[1]",
                    "Meta",
                    Some(BlockOmission::Hidden)
                ),
                ("risdok/nutzdaten[1]/abschnitt[1]", "", None),
                (
                    "risdok/nutzdaten[1]/abschnitt[1]/ueberschrift[1]",
                    "Entscheidende Behörde",
                    None
                ),
                (
                    "risdok/nutzdaten[1]/abschnitt[1]/absatz[1]",
                    "Datenschutzbehörde (DSB)",
                    None
                ),
                (
                    "risdok/nutzdaten[1]/abschnitt[1]/absatz[2]",
                    "",
                    Some(BlockOmission::Empty)
                ),
            ]
        );
        let heading = &t.blocks[2];
        assert_eq!(
            heading.attrs,
            vec![
                ("halign".to_string(), "c".to_string()),
                ("typ".to_string(), "titel".to_string())
            ]
        );
        assert_eq!(heading.items.len(), 1);
        assert_eq!(t.items[heading.items[0]].block, Some(2));
        let para = &t.blocks[3];
        assert_eq!(para.attrs.len(), 3);
        // The paragraph's runs (an inline group) all point at its block, and
        // its inline elements keep their ranges.
        for &i in &para.items {
            assert_eq!(t.items[i].block, Some(3));
            for &c in &t.items[i].children {
                assert_eq!(t.items[c].block, Some(3));
            }
        }
        let inl: Vec<(&str, &str)> = para
            .inlines
            .iter()
            .map(|i| (i.path.as_str(), &para.text[i.start..i.end]))
            .collect();
        assert_eq!(
            inl,
            vec![
                ("risdok/nutzdaten[1]/abschnitt[1]/absatz[1]/b[1]", "schutz"),
                (
                    "risdok/nutzdaten[1]/abschnitt[1]/absatz[1]/span[1]",
                    "(DSB)"
                ),
            ]
        );
        assert_eq!(
            para.text_excluding(|i| i.tag == "span"),
            "Datenschutzbehörde"
        );
        assert_eq!(
            t.source_text([2, 3]),
            "Entscheidende Behörde\nDatenschutzbehörde (DSB)"
        );
        // The section: the block its group was made from, with its identity;
        // selecting under it gives its heading and paragraphs.
        let section = &t.blocks[1];
        assert_eq!(section.tag, "abschnitt");
        assert_eq!(section.attrs.len(), 4);
        assert!(matches!(
            &t.items[section.items[0]].kind,
            TreeKind::Group { label, .. } if label == "section"
        ));
        assert_eq!(t.blocks_under(&section.path), vec![1, 2, 3, 4]);
        assert!(t.containers.is_empty());
        assert!(t.items.iter().all(|it| it.block.is_some()));
    }

    /// A bullet list and a table: the list items' paragraphs are blocks,
    /// the shared list group belongs to none; the table is a block, its
    /// cells and their paragraphs are in its grid.
    #[test]
    fn lists_and_tables_have_their_blocks() {
        let doc = convert(&wrap(
            "<absatz typ=\"erltext\">-<tab/>eins</absatz><absatz typ=\"erltext\">-<tab/>zwei</absatz>\
             <table><tr><td><absatz>Zelle</absatz></td><td>roh</td></tr></table>",
        ));
        let t = doc.tree.as_ref().unwrap();
        let summary: Vec<(&str, &str, Option<BlockOmission>, usize)> = t
            .blocks
            .iter()
            .map(|b| (b.tag.as_str(), b.text.as_str(), b.omitted, b.items.len()))
            .collect();
        let table = t
            .items
            .iter()
            .position(|it| matches!(it.kind, TreeKind::Table { .. }))
            .unwrap();
        assert_eq!(
            summary,
            vec![
                ("abschnitt", "", None, 1),
                ("absatz", "- eins", None, 1),
                ("absatz", "- zwei", None, 1),
                ("table", "", None, 1),
                ("td", "", Some(BlockOmission::InTable(table)), 0),
                ("absatz", "Zelle", Some(BlockOmission::InTable(table)), 0),
                ("td", "roh", Some(BlockOmission::InTable(table)), 0),
            ]
        );
        let group = t
            .items
            .iter()
            .position(|it| matches!(&it.kind, TreeKind::Group { label, .. } if label == "list"))
            .unwrap();
        assert_eq!(t.items[group].block, None);
        assert!(t.items[group]
            .children
            .iter()
            .all(|&c| t.items[c].block.is_some()));
    }

    /// The issue's minimal sample: a heading and a paragraph, in order, as
    /// separate items under the section; no markup in the text.
    #[test]
    fn heading_and_paragraph_are_items() {
        let doc = convert(&wrap(
            "<ueberschrift typ=\"titel\" halign=\"j\">Entscheidende Behörde</ueberschrift>\
             <absatz typ=\"erltext\" ct=\"organ\" halign=\"j\">Datenschutzbehörde</absatz>",
        ));
        assert_eq!(
            doc.export_to_markdown(),
            "## Entscheidende Behörde\n\nDatenschutzbehörde\n"
        );
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(json["groups"][0]["name"], "abschnitt 1");
        assert_eq!(json["groups"][0]["label"], "section");
        let texts = json["texts"].as_array().unwrap();
        assert_eq!(texts[0]["label"], "section_header");
        assert_eq!(texts[0]["text"], "Entscheidende Behörde");
        assert_eq!(texts[0]["parent"]["$ref"], "#/groups/0");
        assert_eq!(texts[1]["label"], "text");
        assert_eq!(texts[1]["text"], "Datenschutzbehörde");
        assert!(!doc.export_to_text().contains('<'));
    }

    /// Formatting runs reach the JSON as an inline group and the Markdown
    /// as markers hugging the text; a tab reads as a space.
    #[test]
    fn formatting_runs_and_tabs() {
        let doc = convert(&wrap(
            "<absatz><b>1.</b><tab position=\"63.75\" /><u><b>Sachverhalt:</b></u></absatz>\
             <absatz>unter „<i>https://h***.a***.at/</i>“ keine</absatz>",
        ));
        assert_eq!(
            doc.export_to_markdown(),
            "**1. Sachverhalt:**\n\nunter „*https://h\\*\\*\\*.a\\*\\*\\*.at/*“ keine\n"
        );
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = json["texts"].as_array().unwrap();
        assert_eq!(texts[0]["text"], "1. ");
        assert_eq!(texts[0]["formatting"]["bold"], true);
        assert_eq!(texts[1]["text"], "Sachverhalt:");
        assert_eq!(texts[1]["formatting"]["underline"], true);
    }

    /// Running headers and footers are page furniture: in the JSON (and
    /// DocLang), not in the Markdown.
    #[test]
    fn headers_and_footers_are_furniture() {
        let doc = convert(&wrap(
            "<kzinhalt typ=\"p\"><absatz><tab/><feld><span>Datenschutzbehörde</span></feld>\
             <tab/><feld><span>04.09.2025</span></feld></absatz></kzinhalt>\
             <fzinhalt typ=\"p\"><absatz>www.ris.bka.gv.at<tab/>Seite <feld><span>2</span></feld>\
             </absatz></fzinhalt><absatz>Body.</absatz>",
        ));
        assert_eq!(doc.export_to_markdown(), "Body.\n");
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = json["texts"].as_array().unwrap();
        assert_eq!(texts[0]["label"], "page_header");
        assert_eq!(texts[0]["text"], "Datenschutzbehörde 04.09.2025");
        assert_eq!(texts[0]["content_layer"], "furniture");
        assert_eq!(texts[1]["label"], "page_footer");
        assert_eq!(texts[1]["text"], "www.ris.bka.gv.at Seite 2");
        let dl = doc.export_to_doclang();
        assert!(dl.contains("<page_header>"), "{dl}");
    }

    /// Bullet paragraphs (a glyph before the tab, or none in a hanging
    /// indent) are one list; numbered ones stay paragraphs.
    #[test]
    fn bullets_are_lists_numbers_are_paragraphs() {
        let doc = convert(&wrap(
            "<absatz style=\"padding-left:38px;text-indent:-38px;\">1.1.<tab/>Erster Punkt.</absatz>\
             <absatz style=\"padding-left:62px;text-indent:-24px;\">-<tab/>Gegen die Beschuldigte.</absatz>\
             <absatz style=\"padding-left:62px;text-indent:-24px;\">-<tab/>Mitgewirkt.</absatz>\
             <absatz style=\"padding-left:86px;text-indent:-24px;\"><tab/>Ohne Zeichen.</absatz>\
             <absatz style=\"padding-left:86px;text-indent:-24px;\">\u{f0b7}<tab/>Symbol.</absatz>",
        ));
        assert_eq!(
            doc.export_to_markdown(),
            "1.1. Erster Punkt.\n\n- Gegen die Beschuldigte.\n- Mitgewirkt.\n- Ohne Zeichen.\n- Symbol.\n"
        );
    }

    /// A table: the layout row dropped, spans honoured.
    #[test]
    fn tables_drop_the_layout_row() {
        let doc = convert(&wrap(
            "<table><tr><td><absatz typ=\"tabtext\"><b>Geldstrafe von Euro</b></absatz></td>\
             <td><absatz><b>Gemäß</b></absatz></td></tr>\
             <tr><td><absatz>€ 870,-</absatz></td><td><absatz>Art. 83 Abs. 4</absatz></td></tr>\
             <tr><td colspan=\"2\"><absatz>Summe</absatz></td></tr>\
             <tr height=\"0\"><td width=\"310\" /><td width=\"340\" /></tr></table>",
        ));
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let data = &json["tables"][0]["data"];
        assert_eq!(data["num_rows"], 3);
        assert_eq!(data["num_cols"], 2);
        assert_eq!(data["table_cells"][4]["col_span"], 2);
        let md = doc.export_to_markdown();
        assert!(md.contains("| € 870,-"), "{md}");
    }

    /// Pseudonym stars stay text: escaped in the Markdown, literal in the
    /// plain text and the JSON.
    #[test]
    fn pseudonym_stars_are_text() {
        let doc = convert(&wrap(
            "<absatz><u><b>Beschuldigte: A*** GmbH (FN *0*271*h)</b></u></absatz>\
             <absatz>mit Sitz in **** L***berg</absatz>",
        ));
        assert_eq!(
            doc.export_to_markdown(),
            "**Beschuldigte: A\\*\\*\\* GmbH (FN \\*0\\*271\\*h)**\n\n\
             mit Sitz in \\*\\*\\*\\* L\\*\\*\\*berg\n"
        );
        assert_eq!(
            doc.export_to_text(),
            "Beschuldigte: A*** GmbH (FN *0*271*h)\n\nmit Sitz in **** L***berg"
        );
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert_eq!(json["texts"][1]["text"], "mit Sitz in **** L***berg");
    }

    /// A bare `.xml` (and bytes with no name to go by) reach the RIS reader
    /// by their root, no longer one `xml` code block; `xml_ris` names the
    /// format.
    #[test]
    fn bare_xml_routes_to_ris() {
        let xml = wrap("<ueberschrift typ=\"titel\">Geschäftszahl</ueberschrift><absatz>2025-0.699.550</absatz>");
        for src in [
            SourceDocument::from_bytes("ris-min.xml", InputFormat::Xml, xml.clone().into_bytes()),
            SourceDocument::from_named_bytes("stdin", xml.clone().into_bytes(), None).unwrap(),
        ] {
            let md = crate::DocumentConverter::new()
                .convert(src)
                .expect("convert")
                .document
                .export_to_markdown();
            assert_eq!(md, "## Geschäftszahl\n\n2025-0.699.550\n");
        }
        assert_eq!(InputFormat::from_id("xml_ris"), Some(InputFormat::XmlRis));
        assert_eq!(InputFormat::XmlRis.as_str(), "xml_ris");
    }

    #[test]
    fn sniffs_the_risdok_root() {
        assert!(looks_like_ris(
            "<?xml version=\"1.0\"?><risdok stylesheet=\"x.xsl\" xmlns=\"http://www.bka.gv.at\">"
        ));
        assert!(looks_like_ris("<risdok><nutzdaten/></risdok>"));
        assert!(!looks_like_ris("<risdok xmlns=\"urn:other\">"));
        assert!(!looks_like_ris("<article><risdok/></article>"));
    }
}
