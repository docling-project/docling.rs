//! docling's item tree for Markdown — the structure the JSON export serializes.
//!
//! `markdown.rs` renders pulldown-cmark's GFM events into the flat node stream
//! Markdown / DocLang / LaTeX read. Upstream's `MarkdownDocumentBackend` walks
//! marko's **CommonMark** AST (`_iterate_elements`) and hands docling-core a
//! very different shape: a heading or list item is created lazily when its
//! first text run arrives, a paragraph of several inline runs is an `inline`
//! group of one text item per run (each with `formatting` / `hyperlink`), a
//! code span is a `code` item, a nested list hangs off the previous item of
//! the outer list, a GFM table sits under the current parent with a
//! `RichTableCell` for every formatted cell, hard/soft line breaks merge the
//! following run into the previous text item, and every item is numbered in
//! creation order.
//!
//! This module reproduces that walk. It parses the text once more with
//! marko's GFM extension as docling configures it (docling#4488, 2.135):
//! tables, `~~strikethrough~~` and bare URLs, no task lists or footnotes;
//! rebuilds marko's element tree from the events, and ports
//! `_iterate_elements` call for call into a
//! [`docling_core::tree::ItemTree`].
//!
//! A document with a raw HTML block takes upstream's detour: each block is
//! a `code` item of its source between two markers, the tree is exported
//! to HTML (docling's `serialize_markdown_for_html`, every picture an
//! `<img>` keyed by its `self_ref`), the markers and the `<pre><code>`
//! around them are cut so the block is markup again, and the HTML item
//! tree ([`super::html_tree`], no furniture inference, no `<title>` item)
//! is the document's — its pictures resolved back through the keys.

use std::collections::HashMap;

use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind};
use docling_core::{Table, TableCell};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::html_tree::docling_href;
use super::markdown::unescape_entities;

/// marko's element classes, as docling's walk tells them apart.
enum El {
    Document(Vec<El>),
    Heading {
        level: u8,
        children: Vec<El>,
    },
    Paragraph(Vec<El>),
    List {
        ordered: bool,
        start: u64,
        items: Vec<El>,
    },
    ListItem(Vec<El>),
    /// `Quote`, `ThematicBreak`, `LinkRefDef`, inline HTML, …: "some other
    /// element", children walked.
    Other(Vec<El>),
    /// marko's GFM `Table`: rows of cells, each cell its inline children.
    /// Row 0 is the header; the delimiter row is not one.
    Table(Vec<Vec<Vec<El>>>),
    CodeBlock {
        lang: Option<String>,
        text: String,
    },
    /// The block's source lines (marko's `HTMLBlock.body`).
    HtmlBlock(String),
    /// `RawText` or `Literal` (an escaped character is a node of its own).
    Text(String),
    /// `InlineHTML`: walked as nothing, but part of a table cell's text.
    InlineHtml(String),
    Emphasis(Vec<El>),
    Strong(Vec<El>),
    /// docling's `_GfmStrikethrough` (`~~text~~`).
    Strike(Vec<El>),
    Link {
        dest: String,
        children: Vec<El>,
    },
    Image {
        title: String,
        /// The destination (`![alt](dest)`), resolved to bytes under the
        /// image policy (#646).
        dest: String,
        children: Vec<El>,
    },
    CodeSpan(String),
    LineBreak {
        soft: bool,
    },
    /// A display formula that is a paragraph of its own (`$$…$$`): a
    /// `formula` item, as the LaTeX backend makes of a `displaymath`.
    Formula(String),
}

impl El {
    fn children(&self) -> &[El] {
        match self {
            El::Document(c)
            | El::Paragraph(c)
            | El::ListItem(c)
            | El::Other(c)
            | El::Emphasis(c)
            | El::Strong(c)
            | El::Strike(c) => c,
            El::Heading { children, .. }
            | El::Link { children, .. }
            | El::Image { children, .. } => children,
            El::List { items, .. } => items,
            _ => &[],
        }
    }

    /// A marko `BlockElement`: everything but inline content. An `Other`
    /// without children may be inline HTML, so it is not taken for one.
    fn is_block(&self) -> bool {
        !self.is_inline()
    }

    fn is_inline(&self) -> bool {
        matches!(
            self,
            El::Text(_)
                | El::Emphasis(_)
                | El::Strong(_)
                | El::Strike(_)
                | El::Link { .. }
                | El::Image { .. }
                | El::CodeSpan(_)
                | El::LineBreak { .. }
                | El::Formula(_)
                | El::InlineHtml(_)
        ) || matches!(self, El::Other(c) if c.is_empty())
    }
}

/// Rebuild marko's tree from pulldown's CommonMark events.
fn parse(text: &str) -> El {
    // Contiguous text events merge into one `RawText`; an escape leaves an
    // offset gap (the backslash), so the pieces around it stay separate — a
    // marko `Literal` (see `markdown.rs`).
    // Math on, then folded back into text except for a standalone display
    // formula (`markdown.rs`): the text runs come out as marko's.
    let options = Options::ENABLE_MATH | Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let raw: Vec<(Event, std::ops::Range<usize>)> =
        Parser::new_ext(text, options).into_offset_iter().collect();
    let raw = super::markdown::marko_strikethrough(raw, text);
    let raw =
        super::markdown::link_bare_urls(super::markdown::fold_math_into_text(raw, text), text);
    let mut events: Vec<Event> = Vec::with_capacity(raw.len());
    let mut k = 0;
    // Where the previous event's source ended: a text event starting one
    // byte later, behind a backslash, is a resolved escape whose source
    // range no longer shows the backslash.
    let mut prev_end = 0usize;
    while k < raw.len() {
        if matches!(raw[k].0, Event::Text(_)) {
            let mut merged = String::new();
            let mut end = raw[k].1.start;
            let run_escaped =
                raw[k].1.start == prev_end + 1 && text.as_bytes().get(prev_end) == Some(&b'\\');
            if run_escaped {
                // pulldown hands the escaped character over glued to the text
                // that follows it; marko keeps it as a `Literal` node of its
                // own, so it is split off here.
                if let Some((Event::Text(t), range)) = raw.get(k) {
                    let mut chars = t.chars();
                    if let Some(first) = chars.next() {
                        events.push(Event::Text(first.to_string().into()));
                        merged.push_str(chars.as_str());
                        end = range.end;
                        k += 1;
                    }
                }
            }
            while let Some((Event::Text(t), range)) = raw.get(k) {
                if range.start != end {
                    break;
                }
                merged.push_str(t);
                end = range.end;
                k += 1;
            }
            prev_end = end;
            if !merged.is_empty() {
                events.push(Event::Text(merged.into()));
            }
        } else {
            prev_end = raw[k].1.end;
            events.push(raw[k].0.clone());
            k += 1;
        }
    }
    let mut i = 0;
    El::Document(parse_children(&events, &mut i, 0))
}

/// Deepest container nesting rebuilt — `markdown.rs`'s `MAX_NESTING`
/// (markdown-it's, which docling's parser runs on): a 5 000-level list or
/// 50 000 `>` would otherwise overflow the stack here as it did there.
const MAX_NESTING: u16 = 100;

fn parse_children(events: &[Event], i: &mut usize, depth: u16) -> Vec<El> {
    let mut out = Vec::new();
    while *i < events.len() {
        match &events[*i] {
            Event::End(_) => {
                *i += 1;
                return out;
            }
            Event::Start(_) if depth >= MAX_NESTING => {
                super::markdown::skip_subtree(events, i);
                out.push(El::Other(Vec::new()));
            }
            Event::Start(tag) => {
                let tag = tag.clone();
                *i += 1;
                out.push(parse_container(tag, events, i, depth + 1));
            }
            Event::Text(t) => {
                out.push(El::Text(t.to_string()));
                *i += 1;
            }
            Event::Code(t) => {
                out.push(El::CodeSpan(t.to_string()));
                *i += 1;
            }
            Event::DisplayMath(t) => {
                out.push(El::Formula(t.trim().to_string()));
                *i += 1;
            }
            Event::SoftBreak => {
                out.push(El::LineBreak { soft: true });
                *i += 1;
            }
            Event::HardBreak => {
                out.push(El::LineBreak { soft: false });
                *i += 1;
            }
            Event::InlineHtml(t) => {
                out.push(El::InlineHtml(t.to_string()));
                *i += 1;
            }
            // A thematic break, a footnote/task marker: marko's "other"
            // elements, with no children docling would walk.
            _ => {
                out.push(El::Other(Vec::new()));
                *i += 1;
            }
        }
    }
    out
}

fn parse_container(tag: Tag, events: &[Event], i: &mut usize, depth: u16) -> El {
    match tag {
        Tag::Paragraph => El::Paragraph(parse_children(events, i, depth)),
        Tag::Heading { level, .. } => El::Heading {
            level: heading_level(level),
            children: parse_children(events, i, depth),
        },
        Tag::List(start) => El::List {
            ordered: start.is_some(),
            start: start.unwrap_or(1),
            items: parse_children(events, i, depth),
        },
        // marko always wraps an item's text in a `Paragraph`; pulldown puts a
        // tight item's inline content straight under the item.
        Tag::Item => {
            let children = parse_children(events, i, depth);
            let mut wrapped: Vec<El> = Vec::new();
            let mut para: Vec<El> = Vec::new();
            for child in children {
                if child.is_inline() {
                    para.push(child);
                } else {
                    if !para.is_empty() {
                        wrapped.push(El::Paragraph(std::mem::take(&mut para)));
                    }
                    wrapped.push(child);
                }
            }
            if !para.is_empty() {
                wrapped.push(El::Paragraph(para));
            }
            El::ListItem(wrapped)
        }
        Tag::BlockQuote(_) => El::Other(parse_children(events, i, depth)),
        Tag::CodeBlock(kind) => {
            let lang = match kind {
                CodeBlockKind::Fenced(info) => {
                    let lang = info.split_whitespace().next().unwrap_or("");
                    (!lang.is_empty()).then(|| lang.to_string())
                }
                CodeBlockKind::Indented => None,
            };
            let mut code = String::new();
            while *i < events.len() && !matches!(events[*i], Event::End(TagEnd::CodeBlock)) {
                if let Event::Text(t) = &events[*i] {
                    code.push_str(t);
                }
                *i += 1;
            }
            *i += 1;
            El::CodeBlock { lang, text: code }
        }
        Tag::HtmlBlock => {
            let mut body = String::new();
            while *i < events.len() && !matches!(events[*i], Event::End(TagEnd::HtmlBlock)) {
                if let Event::Html(t) | Event::Text(t) = &events[*i] {
                    body.push_str(t);
                }
                *i += 1;
            }
            *i += 1;
            El::HtmlBlock(body)
        }
        Tag::Emphasis => El::Emphasis(parse_children(events, i, depth)),
        Tag::Strong => El::Strong(parse_children(events, i, depth)),
        Tag::Strikethrough => El::Strike(parse_children(events, i, depth)),
        // pulldown's GFM rows already hold the header's width of cells, a
        // short row padded and a long one cut, as marko's `TableRow.parse`.
        Tag::Table(_) => {
            let mut rows: Vec<Vec<Vec<El>>> = Vec::new();
            while *i < events.len() {
                match &events[*i] {
                    Event::Start(Tag::TableHead | Tag::TableRow) => {
                        rows.push(Vec::new());
                        *i += 1;
                    }
                    Event::Start(Tag::TableCell) => {
                        *i += 1;
                        let cell = parse_children(events, i, depth);
                        if let Some(row) = rows.last_mut() {
                            row.push(cell);
                        }
                    }
                    Event::End(TagEnd::Table) => {
                        *i += 1;
                        break;
                    }
                    _ => *i += 1,
                }
            }
            El::Table(rows)
        }
        // marko's `AutoLink` (`<https://…>`, `<me@x.y>`, a GFM bare URL) is a
        // link like any other since docling#4791 (#773).
        Tag::Link {
            link_type,
            dest_url,
            ..
        } => El::Link {
            dest: super::markdown::link_dest(link_type, &dest_url),
            children: parse_children(events, i, depth),
        },
        Tag::Image {
            title, dest_url, ..
        } => El::Image {
            title: title.to_string(),
            dest: dest_url.to_string(),
            children: parse_children(events, i, depth),
        },
        _ => El::Other(parse_children(events, i, depth)),
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// `_only_plain_line_breaks`: text runs joined only by line breaks, with at
/// least one break — content the pending-line-break merge handles alone, so
/// no `inline` group is opened for it.
fn only_plain_line_breaks(children: &[El]) -> bool {
    children.iter().any(|c| matches!(c, El::LineBreak { .. }))
        && children
            .iter()
            .all(|c| matches!(c, El::Text(_) | El::LineBreak { .. }))
}

/// `_cell_plain_text`: a cell's text with its markers dropped — a code
/// span literal, other text unescaped, a link's or an image's text kept.
fn cell_plain_text(el: &El, out: &mut String) {
    match el {
        El::Text(t) | El::InlineHtml(t) => out.push_str(&unescape_entities(t)),
        El::CodeSpan(t) => out.push_str(t),
        El::Formula(t) => {
            out.push_str("$$");
            out.push_str(t);
            out.push_str("$$");
        }
        other => other
            .children()
            .iter()
            .for_each(|c| cell_plain_text(c, out)),
    }
}

/// `_cell_has_rich_content`: the cell holds emphasis, a code span, a link,
/// an image or a strikethrough, at any depth.
fn is_rich(el: &El) -> bool {
    matches!(
        el,
        El::Strong(_)
            | El::Emphasis(_)
            | El::CodeSpan(_)
            | El::Link { .. }
            | El::Image { .. }
            | El::Strike(_)
    ) || el.children().iter().any(is_rich)
}

/// A lazily created container, made when its first text run arrives.
enum Pending {
    Heading(u8),
    ListItem { enumerated: bool, marker: String },
}

struct Walker<'a> {
    /// Resolves an image's destination to its bytes (#646).
    images: &'a dyn super::images::ImageResolver,
    tree: ItemTree,
    pending_hard_break: bool,
    pending_soft_break: bool,
    html_blocks: usize,
    creation_stack: Vec<Pending>,
    list_ordered: HashMap<usize, bool>,
    list_start: HashMap<usize, u64>,
    list_counter: HashMap<usize, u64>,
    list_last_item: HashMap<usize, usize>,
}

/// docling's `_START_MARKER` / `_STOP_MARKER` around a raw HTML block.
const HTML_START: &str = "#_#_DOCLING_DOC_MD_HTML_EXPORT_START_#_#";
const HTML_STOP: &str = "#_#_DOCLING_DOC_MD_HTML_EXPORT_STOP_#_#";

/// Build docling's item tree for a Markdown document (through the HTML
/// backend when it holds a raw HTML block, see the module docs); `None`
/// when that detour fails, as upstream's conversion would — the flat
/// export's JSON stays then.
pub(super) fn build_tree(
    text: &str,
    images: &dyn super::images::ImageResolver,
) -> Option<ItemTree> {
    let root = parse(text);
    let mut w = Walker {
        images,
        tree: ItemTree {
            // The runs keep their boundary whitespace (schema 1.11.0, #997).
            inline_whitespace: true,
            ..ItemTree::default()
        },
        pending_hard_break: false,
        pending_soft_break: false,
        html_blocks: 0,
        creation_stack: Vec::new(),
        list_ordered: HashMap::new(),
        list_start: HashMap::new(),
        list_counter: HashMap::new(),
        list_last_item: HashMap::new(),
    };
    w.iterate(&root, None, None, None);
    if w.html_blocks == 0 {
        return Some(w.tree);
    }
    reparse_as_html(w.tree, w.html_blocks, images)
}

/// The key prefix of a picture's `<img src>` in the detour's HTML: upstream's
/// `data:application/x-docling-md-picture,{hash}/`. The hash only keeps the
/// source's own markup from colliding with a key; one constant does that.
const PICTURE_KEY: &str = "data:application/x-docling-md-picture,docling-rs/";

/// Upstream's `convert` past the walk when it met a raw HTML block: the tree
/// exported to HTML, each block's markers cut together with the `<pre><code>`
/// the code item rendered as (a count other than the blocks' is upstream's
/// internal error: `None`), and the HTML backend's tree over the result.
fn reparse_as_html(
    tree: ItemTree,
    blocks: usize,
    images: &dyn super::images::ImageResolver,
) -> Option<ItemTree> {
    let pictures: HashMap<String, Option<docling_core::PictureImage>> = {
        let mut out = HashMap::new();
        let mut n = 0usize;
        for item in &tree.items {
            if let TreeKind::Picture { image, .. } = &item.kind {
                out.insert(format!("{PICTURE_KEY}#/pictures/{n}"), image.clone());
                n += 1;
            }
        }
        out
    };
    let mut doc = docling_core::DoclingDocument::new("");
    doc.tree = Some(tree);
    let html = docling_core::to_html_with_picture_refs(&doc, PICTURE_KEY, HTML_START);
    let start = regex::Regex::new(&format!(r"<pre>\s*<code>\s*{HTML_START}")).ok()?;
    let stop = regex::Regex::new(&format!(r"{HTML_STOP}\s*</code>\s*</pre>")).ok()?;
    if start.find_iter(&html).count() != blocks || stop.find_iter(&html).count() != blocks {
        return None;
    }
    let html = start.replace_all(&html, "");
    let html = stop.replace_all(&html, "");
    struct Keys<'a> {
        pictures: HashMap<String, Option<docling_core::PictureImage>>,
        images: &'a dyn super::images::ImageResolver,
    }
    impl super::images::ImageResolver for Keys<'_> {
        fn resolve(&self, src: &str) -> Option<docling_core::PictureImage> {
            match self.pictures.get(src) {
                Some(image) => image.clone(),
                // The original HTML's own images keep the normal policy.
                None => self.images.resolve(src),
            }
        }
    }
    let keys = Keys { pictures, images };
    let parsed = scraper::Html::parse_document(&html);
    Some(super::html_tree::build_tree(&parsed, &keys, false, false))
}

fn text_kind(
    label: &str,
    text: &str,
    formatting: Option<Formatting>,
    hyperlink: Option<&str>,
    list: Option<ListMeta>,
    level: Option<u8>,
) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting,
        hyperlink: hyperlink.map(str::to_string),
        level,
        list,
    }
}

impl Walker<'_> {
    /// `_parse_gfm_table`: the table under the current parent, then its
    /// cells row by row — row 0 the column header, each cell's text its
    /// stripped plain text. A formatted cell is a `RichTableCell`: its runs
    /// are walked into an `inline` group under a `rich_cell_group_*` group of
    /// the table, both dropped again when no item came of them.
    fn table(&mut self, rows: &[Vec<Vec<El>>], parent: Option<usize>) {
        let Some(width) = rows.first().map(Vec::len) else {
            return;
        };
        let table_id = self.tree.add(
            parent,
            None,
            TreeKind::Table {
                table: Table::default(),
                rich_cells: Vec::new(),
                captions: Vec::new(),
            },
        );
        let index = self.tree.table_count() - 1;
        let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows.len());
        let mut cells = Vec::new();
        let mut rich = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            let mut texts = Vec::with_capacity(width);
            for c in 0..width {
                let content = row.get(c).map(Vec::as_slice).unwrap_or_default();
                let mut text = String::new();
                content.iter().for_each(|el| cell_plain_text(el, &mut text));
                let text = text.trim().to_string();
                if content.iter().any(is_rich) {
                    let group = self.tree.add(
                        Some(table_id),
                        None,
                        TreeKind::Group {
                            label: "unspecified".into(),
                            name: format!("rich_cell_group_{index}_{c}_{r}"),
                        },
                    );
                    let inline = self.tree.add(
                        Some(group),
                        None,
                        TreeKind::Group {
                            label: "inline".into(),
                            name: "group".into(),
                        },
                    );
                    let mut items = 0;
                    for el in content {
                        self.cell_inline(el, inline, &mut items, None, None);
                    }
                    if items > 0 {
                        rich.push((r, c, group));
                    } else {
                        self.tree.delete(inline);
                        self.tree.delete(group);
                    }
                }
                cells.push(TableCell {
                    text: text.clone(),
                    bbox: None,
                    start_row: r,
                    start_col: c,
                    row_span: 1,
                    col_span: 1,
                    column_header: r == 0,
                    row_header: false,
                    row_section: false,
                });
                texts.push(text);
            }
            grid.push(texts);
        }
        if let TreeKind::Table {
            table, rich_cells, ..
        } = &mut self.tree.items[table_id].kind
        {
            *table = Table {
                rows: grid,
                cells: Some(cells),
                ..Table::default()
            };
            *rich_cells = rich;
        }
    }

    /// `_iterate_cell_inline`: one inline node of a rich cell, as items of
    /// the cell's `inline` group — every run its own text (no line-break
    /// merging), a code span a `code` item, an image a picture whose title
    /// is a body-level caption (its alt text is not walked). `items` counts
    /// what was made.
    fn cell_inline(
        &mut self,
        el: &El,
        parent: usize,
        items: &mut usize,
        mut formatting: Option<Formatting>,
        mut hyperlink: Option<String>,
    ) {
        match el {
            El::Text(raw) | El::Formula(raw) => {
                // A text run keeps its raw fragment (docling#3891): marko
                // trimmed the cell's edges, whitespace-only runs between
                // formatted runs are significant.
                let text = match el {
                    El::Formula(_) => format!("$${}$$", raw.trim()),
                    _ => unescape_entities(raw),
                };
                if !text.is_empty() {
                    let kind =
                        text_kind("text", &text, formatting, hyperlink.as_deref(), None, None);
                    self.tree.add(Some(parent), None, kind);
                    *items += 1;
                }
                return;
            }
            El::CodeSpan(code) => {
                let text = code.trim();
                if !text.is_empty() {
                    self.tree.add(
                        Some(parent),
                        None,
                        TreeKind::Code {
                            text: text.to_string(),
                            orig: None,
                            language: None,
                            formatting,
                            hyperlink,
                            captions: Vec::new(),
                        },
                    );
                    *items += 1;
                }
                return;
            }
            El::Image { title, dest, .. } => {
                let mut captions = Vec::new();
                if !title.is_empty() {
                    let kind =
                        text_kind("caption", &unescape_entities(title), None, None, None, None);
                    captions.push(self.tree.add(None, None, kind));
                }
                self.tree.add(
                    Some(parent),
                    None,
                    TreeKind::Picture {
                        captions,
                        image: self.images.resolve(dest),
                        extra_classes: Vec::new(),
                        classification: None,
                        description: None,
                        confidence: None,
                        chart: None,
                        dpi: None,
                    },
                );
                *items += 1;
                return;
            }
            El::Strong(_) => formatting.get_or_insert_with(Formatting::default).bold = true,
            El::Emphasis(_) => formatting.get_or_insert_with(Formatting::default).italic = true,
            El::Strike(_) => {
                formatting
                    .get_or_insert_with(Formatting::default)
                    .strikethrough = true
            }
            El::Link { dest, .. } => hyperlink = Some(docling_href(dest)),
            _ => {}
        }
        for child in el.children() {
            self.cell_inline(child, parent, items, formatting, hyperlink.clone());
        }
    }

    fn create_list_item(
        &mut self,
        parent: Option<usize>,
        text: &str,
        enumerated: bool,
        marker: &str,
        formatting: Option<Formatting>,
        hyperlink: Option<&str>,
    ) -> usize {
        self.tree.add(
            parent,
            None,
            text_kind(
                "list_item",
                text,
                formatting,
                hyperlink,
                Some(ListMeta {
                    enumerated,
                    marker: marker.to_string(),
                }),
                None,
            ),
        )
    }

    /// `_create_heading_item`: level 1 is docling's `title`, deeper ones a
    /// `section_header` of level − 1.
    fn create_heading_item(
        &mut self,
        parent: Option<usize>,
        text: &str,
        level: u8,
        formatting: Option<Formatting>,
        hyperlink: Option<&str>,
    ) -> usize {
        let kind = if level == 1 {
            text_kind("title", text, formatting, hyperlink, None, None)
        } else {
            text_kind(
                "section_header",
                text,
                formatting,
                hyperlink,
                None,
                Some(level - 1),
            )
        };
        self.tree.add(parent, None, kind)
    }

    /// `_flush_creation_stack`: create the pending items, innermost last, with
    /// the run's text; a list item becomes the parent for what follows.
    fn flush_creation_stack(
        &mut self,
        snippet: &str,
        mut parent: Option<usize>,
        formatting: Option<Formatting>,
        hyperlink: Option<&str>,
    ) -> Option<usize> {
        while let Some(pending) = self.creation_stack.pop() {
            match pending {
                Pending::ListItem { enumerated, marker } => {
                    let parent_ref = parent;
                    let item = self.create_list_item(
                        parent, snippet, enumerated, &marker, formatting, hyperlink,
                    );
                    parent = Some(item);
                    if let Some(p) = parent_ref {
                        self.list_last_item.insert(p, item);
                        *self.list_counter.entry(p).or_insert(0) += 1;
                    }
                }
                Pending::Heading(level) => {
                    // Not kept as the parent: marko does not nest a section's
                    // content under its heading.
                    self.create_heading_item(parent, snippet, level, formatting, hyperlink);
                }
            }
        }
        parent
    }

    /// `_iterate_elements`.
    fn iterate(
        &mut self,
        el: &El,
        mut parent: Option<usize>,
        mut formatting: Option<Formatting>,
        mut hyperlink: Option<String>,
    ) {
        // A line break only joins runs of the same paragraph (docling#4525):
        // no break pending at a block survives into it.
        if el.is_block() {
            self.pending_hard_break = false;
            self.pending_soft_break = false;
        }
        // The payloads below this size were pushed by enclosing elements.
        let stack_size = self.creation_stack.len();
        match el {
            El::Heading { level, children } if !children.is_empty() => {
                if children.len() > 1 {
                    // The inline group is created further down, under it.
                    parent = Some(self.create_heading_item(
                        parent,
                        "",
                        *level,
                        formatting,
                        hyperlink.as_deref(),
                    ));
                } else {
                    self.creation_stack.push(Pending::Heading(*level));
                }
            }
            El::List {
                ordered,
                start,
                items,
            } => {
                let has_non_empty = items
                    .iter()
                    .any(|i| matches!(i, El::ListItem(c) if !c.is_empty()));
                if has_non_empty {
                    let group = self.tree.add(
                        parent,
                        None,
                        TreeKind::Group {
                            label: "list".into(),
                            name: "list".into(),
                        },
                    );
                    self.list_ordered.insert(group, *ordered);
                    if *ordered {
                        self.list_start.insert(group, *start);
                    }
                    parent = Some(group);
                }
            }
            El::ListItem(children) if matches!(children.first(), Some(El::Paragraph(c)) if !c.is_empty()) =>
            {
                let enumerated = parent
                    .and_then(|p| self.list_ordered.get(&p).copied())
                    .unwrap_or(false);
                let mut marker = String::new();
                if let (true, Some(p)) = (enumerated, parent) {
                    let start = self.list_start.get(&p).copied().unwrap_or(1);
                    let count = self.list_counter.get(&p).copied().unwrap_or(0);
                    marker = format!("{}.", start + count);
                }
                let Some(El::Paragraph(para)) = children.first() else {
                    unreachable!()
                };
                // The inline group is created further down; plain runs joined
                // by line breaks merge into one item instead.
                if para.len() > 1 && !only_plain_line_breaks(para) {
                    let parent_ref = parent;
                    let item = self.create_list_item(
                        parent,
                        "",
                        enumerated,
                        &marker,
                        formatting,
                        hyperlink.as_deref(),
                    );
                    parent = Some(item);
                    if let Some(p) = parent_ref {
                        self.list_last_item.insert(p, item);
                        *self.list_counter.entry(p).or_insert(0) += 1;
                    }
                } else {
                    self.creation_stack
                        .push(Pending::ListItem { enumerated, marker });
                }
            }
            El::Image {
                title,
                dest,
                children,
            } => {
                // An image without alt text emits no text run (#864,
                // docling#4870): when it is the content of a list item or
                // heading, that item is created now, with empty text, and the
                // picture goes under the list item — else the next text run
                // of the document, in a later block, took the item's place.
                if !self.creation_stack.is_empty() && children.is_empty() {
                    parent =
                        self.flush_creation_stack("", parent, formatting, hyperlink.as_deref());
                }
                // The `"title"` is the caption (on the body, like every
                // `add_text` without a parent); the alt text runs are walked
                // as the picture's children afterwards. The payload is what
                // the image policy resolves the destination to (#646) —
                // nothing by default, as upstream fetches none.
                let mut captions = Vec::new();
                if !title.is_empty() {
                    let cap = self.tree.add(
                        None,
                        None,
                        text_kind(
                            "caption",
                            &unescape_entities(title),
                            formatting,
                            hyperlink.as_deref(),
                            None,
                            None,
                        ),
                    );
                    captions.push(cap);
                }
                self.tree.add(
                    parent,
                    None,
                    TreeKind::Picture {
                        captions,
                        image: self.images.resolve(dest),
                        extra_classes: Vec::new(),
                        classification: None,
                        description: None,
                        confidence: None,
                        chart: None,
                        dpi: None,
                    },
                );
            }
            El::Emphasis(_) => {
                let mut f = formatting.unwrap_or_default();
                f.italic = true;
                formatting = Some(f);
            }
            El::Strong(_) => {
                let mut f = formatting.unwrap_or_default();
                f.bold = true;
                formatting = Some(f);
            }
            El::Strike(_) => {
                let mut f = formatting.unwrap_or_default();
                f.strikethrough = true;
                formatting = Some(f);
            }
            El::Link { dest, .. } => {
                hyperlink = Some(docling_href(dest));
            }
            El::Text(original) => {
                // The raw fragment: its edge whitespace is the boundary
                // between runs (docling#3891, `Foo *emph* bar` -> `"Foo "`,
                // `"emph"`, `" bar"`); a heading's or list item's own text is
                // trimmed, and a whitespace-only run never becomes one.
                let snippet = unescape_entities(original);
                let stripped = unescape_entities(original.trim());
                if !snippet.is_empty() && (!stripped.is_empty() || self.creation_stack.is_empty()) {
                    if !self.creation_stack.is_empty() {
                        parent = self.flush_creation_stack(
                            &stripped,
                            parent,
                            formatting,
                            hyperlink.as_deref(),
                        );
                    } else {
                        // A code span is its own item: text after a break
                        // never joins it.
                        let last =
                            self.tree
                                .last_text()
                                .filter(|&id| match &self.tree.items[id].kind {
                                    TreeKind::Text {
                                        formatting: f,
                                        hyperlink: h,
                                        ..
                                    } => *f == formatting && *h == hyperlink,
                                    _ => false,
                                });
                        match (self.pending_hard_break, self.pending_soft_break, last) {
                            (true, _, Some(id)) => self.append_text(id, "\n", &snippet),
                            (false, true, Some(id)) => self.append_text(id, " ", &snippet),
                            _ => {
                                // The break still separates runs that
                                // cannot merge.
                                let prefix = if self.pending_hard_break {
                                    "\n"
                                } else if self.pending_soft_break {
                                    " "
                                } else {
                                    ""
                                };
                                self.tree.add(
                                    parent,
                                    None,
                                    text_kind(
                                        "text",
                                        &format!("{prefix}{snippet}"),
                                        formatting,
                                        hyperlink.as_deref(),
                                        None,
                                        None,
                                    ),
                                );
                            }
                        }
                    }
                    self.pending_hard_break = false;
                    self.pending_soft_break = false;
                }
            }
            El::Formula(latex) if !latex.is_empty() => {
                self.tree.add(
                    parent,
                    None,
                    text_kind(
                        "formula",
                        latex,
                        formatting,
                        hyperlink.as_deref(),
                        None,
                        None,
                    ),
                );
            }
            El::CodeSpan(code) => {
                let snippet = code.trim().to_string();
                if !self.creation_stack.is_empty() && !snippet.is_empty() {
                    // The span is the container's own text; no separate code item.
                    self.flush_creation_stack(&snippet, parent, formatting, hyperlink.as_deref());
                    return;
                }
                if self.pending_hard_break || self.pending_soft_break {
                    // The break's separator never goes inside the code run:
                    // a plain run of its own (docling#3891).
                    let sep = if self.pending_hard_break { "\n" } else { " " };
                    self.tree
                        .add(parent, None, text_kind("text", sep, None, None, None, None));
                }
                self.pending_hard_break = false;
                self.pending_soft_break = false;
                self.tree.add(
                    parent,
                    None,
                    TreeKind::Code {
                        text: snippet,
                        orig: None,
                        language: None,
                        formatting,
                        hyperlink: hyperlink.clone(),
                        captions: Vec::new(),
                    },
                );
            }
            El::CodeBlock { lang, text } if !text.trim().is_empty() => {
                let snippet = text.trim().to_string();
                // `detect_code_language(text, hint=lang)`: a known hint wins,
                // else the content's own markers.
                let language =
                    super::code_language::detect_code_language(&snippet, lang.as_deref());
                self.tree.add(
                    parent,
                    None,
                    TreeKind::Code {
                        text: snippet,
                        orig: None,
                        language,
                        formatting,
                        hyperlink: hyperlink.clone(),
                        captions: Vec::new(),
                    },
                );
            }
            El::LineBreak { soft } => {
                if *soft {
                    self.pending_soft_break = true;
                } else {
                    self.pending_hard_break = true;
                }
            }
            El::HtmlBlock(body) => {
                self.html_blocks += 1;
                // Wrapped in markers that `reparse_as_html` cuts again.
                if !body.is_empty() {
                    self.tree.add(
                        parent,
                        None,
                        TreeKind::Code {
                            text: format!("{HTML_START}{}{HTML_STOP}", body.trim()),
                            orig: None,
                            language: None,
                            formatting,
                            hyperlink: hyperlink.clone(),
                            captions: Vec::new(),
                        },
                    );
                }
            }
            El::Table(rows) => {
                // Its cells are walked by `table`, not below.
                self.table(rows, parent);
                return;
            }
            El::Heading { .. }
            | El::ListItem(_)
            | El::CodeBlock { .. }
            | El::Formula(_)
            | El::Document(_)
            | El::Paragraph(_)
            | El::Other(_)
            | El::InlineHtml(_) => {}
        }

        let children = el.children();
        if matches!(el, El::Paragraph(_) | El::Heading { .. })
            && children.len() > 1
            && !only_plain_line_breaks(children)
        {
            parent = Some(self.tree.add(
                parent,
                None,
                TreeKind::Group {
                    label: "inline".into(),
                    name: "group".into(),
                },
            ));
        }

        if matches!(el, El::CodeBlock { .. } | El::Text(_)) {
            return;
        }
        for child in children {
            if matches!(el, El::ListItem(_)) && matches!(child, El::List { .. }) {
                if let Some(last) = parent.and_then(|p| self.list_last_item.get(&p).copied()) {
                    // A nested list hangs off the outer list's last item.
                    parent = Some(last);
                }
            }
            self.iterate(child, parent, formatting, hyperlink.clone());
            // The content of a list item or heading had no text run — only
            // inline HTML, say (#864, docling#4870): the item is created
            // with empty text now, or the next text run of the document, in
            // a later block, would take it. Not kept as the parent, as
            // upstream keeps it not.
            if self.creation_stack.len() > stack_size
                && matches!(el, El::ListItem(_) | El::Heading { .. })
            {
                let pending = self.creation_stack.split_off(stack_size);
                let outer = std::mem::replace(&mut self.creation_stack, pending);
                self.flush_creation_stack("", parent, formatting, hyperlink.as_deref());
                self.creation_stack = outer;
            }
        }
    }

    /// `doc.texts[-1].text += sep + snippet` (and `orig` alike).
    fn append_text(&mut self, id: usize, sep: &str, snippet: &str) {
        if let TreeKind::Text { text, .. } | TreeKind::Code { text, .. } =
            &mut self.tree.items[id].kind
        {
            text.push_str(sep);
            text.push_str(snippet);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docling_core::tree::TreeItem;

    fn tree(md: &str) -> ItemTree {
        build_tree(md, &super::super::images::NoFetch).expect("a tree")
    }

    fn label(it: &TreeItem) -> String {
        match &it.kind {
            TreeKind::Text { label, .. } => label.clone(),
            TreeKind::Code { .. } => "code".into(),
            TreeKind::Group { label, name } => format!("{label}:{name}"),
            TreeKind::Table { .. } => "table".into(),
            TreeKind::Picture { .. } => "picture".into(),
            TreeKind::FieldRegion { .. } => "field_region".into(),
            TreeKind::KeyValueGraph { .. } => "key_value_region".into(),
        }
    }

    fn text(it: &TreeItem) -> &str {
        match &it.kind {
            TreeKind::Text { text, .. } | TreeKind::Code { text, .. } => text,
            _ => "",
        }
    }

    /// #864 (docling#4870's tests): a heading or list item whose content
    /// has no text run — an alt-less image, inline HTML — is created with
    /// empty text, so the next block keeps its own text; an image-only list
    /// item holds its picture and its sub-list, and the next item keeps its
    /// number.
    #[test]
    fn a_container_without_text_does_not_take_the_next_block() {
        for heading in [
            "# ![](logo.png)",
            "# <img src=\"logo.png\" width=\"80\">",
            "![](logo.png)\n===",
        ] {
            let t = tree(&format!("{heading}\n\nIntro paragraph.\n"));
            let texts: Vec<(String, &str)> = t
                .items
                .iter()
                .filter(|it| matches!(it.kind, TreeKind::Text { .. }))
                .map(|it| (label(it), text(it)))
                .collect();
            assert_eq!(
                texts,
                [("title".into(), ""), ("text".into(), "Intro paragraph.")],
                "{heading}"
            );
        }
        let t = tree("1. ![](a.png)\n2. Second step\n3. Third step\n");
        let items: Vec<(&str, &str)> = t
            .items
            .iter()
            .filter_map(|it| match &it.kind {
                TreeKind::Text {
                    text,
                    list: Some(l),
                    ..
                } => Some((l.marker.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            [("1.", ""), ("2.", "Second step"), ("3.", "Third step")]
        );
        let first = t
            .items
            .iter()
            .position(|it| text(it).is_empty() && label(it) == "list_item")
            .unwrap();
        let picture = t
            .items
            .iter()
            .position(|it| label(it) == "picture")
            .unwrap();
        assert_eq!(t.items[picture].parent, Some(first));

        let t = tree("- ![](a.png)\n  - nested\n- next\n");
        let texts: Vec<&str> = t
            .items
            .iter()
            .filter(|it| matches!(it.kind, TreeKind::Text { .. }))
            .map(text)
            .collect();
        assert_eq!(texts, ["", "nested", "next"]);
        let empty = t
            .items
            .iter()
            .position(|it| label(it) == "list_item" && text(it).is_empty())
            .unwrap();
        let nested = t.items.iter().position(|it| text(it) == "nested").unwrap();
        let sub_list = t.items[nested].parent.unwrap();
        assert_eq!(t.items[sub_list].parent, Some(empty));
    }

    /// docling's `_iterate_elements`: a paragraph of several runs is an
    /// `inline` group of one text per run with its formatting, a code span a
    /// `code` item; a plain paragraph is one text; a title comes lazily from
    /// its first run.
    #[test]
    fn paragraph_runs_become_an_inline_group() {
        let t = tree("# Title\n\nPlain.\n\nFoo *em* **strong** ***both*** and `code`.\n");
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "title",
                "text",
                "inline:group",
                "text",
                "text",
                "text",
                "text",
                "text",
                "text",
                "text",
                "code",
                "text"
            ]
        );
        assert_eq!(t.body, vec![0, 1, 2]);
        // The runs keep their boundary whitespace; the spaces between the
        // formatted runs are runs of their own (docling 2.148, #997).
        assert_eq!(t.items[2].children, vec![3, 4, 5, 6, 7, 8, 9, 10, 11]);
        let fmt = |i: usize| match &t.items[i].kind {
            TreeKind::Text { formatting, .. } => formatting.map(|f| (f.bold, f.italic)),
            _ => None,
        };
        assert_eq!(text(&t.items[3]), "Foo ");
        assert_eq!((text(&t.items[4]), fmt(4)), ("em", Some((false, true))));
        assert_eq!(text(&t.items[5]), " ");
        assert_eq!((text(&t.items[6]), fmt(6)), ("strong", Some((true, false))));
        assert_eq!((text(&t.items[8]), fmt(8)), ("both", Some((true, true))));
        assert_eq!(text(&t.items[9]), " and ");
        assert_eq!(text(&t.items[11]), ".");
        assert_eq!(fmt(3), None, "an unformatted run carries no `formatting`");
    }

    /// Lists: a `list` group with `1.`-style markers counted from the list's
    /// start; a mixed-run item is an empty item over an inline group whose
    /// link run carries the normalized hyperlink; a nested list hangs off the
    /// outer list's last item.
    #[test]
    fn lists_follow_docling() {
        let t = tree("3. Pull the [**repo**](https://x.y)\n4. two\n   - nested\n5. three\n");
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "list:list",
                "list_item",
                "inline:group",
                "text",
                "text",
                "list_item",
                "list:list",
                "list_item",
                "list_item"
            ]
        );
        let marker = |i: usize| match &t.items[i].kind {
            TreeKind::Text { list: Some(l), .. } => (l.enumerated, l.marker.clone()),
            _ => panic!("not a list item"),
        };
        assert_eq!(marker(1), (true, "3.".into()));
        assert_eq!(text(&t.items[1]), "");
        assert_eq!(t.items[2].parent, Some(1));
        assert!(
            matches!(&t.items[4].kind, TreeKind::Text { hyperlink: Some(h), formatting: Some(f), .. } if h == "https://x.y/" && f.bold)
        );
        assert_eq!(marker(5), (true, "4.".into()));
        assert_eq!(
            t.items[6].parent,
            Some(5),
            "the nested list hangs off `two`"
        );
        assert_eq!(marker(7), (false, String::new()));
        assert_eq!(marker(8), (true, "5.".into()));
    }

    /// A heading of several runs is created empty with an inline group of its
    /// runs under it; a heading whose only run is a code span takes that text.
    #[test]
    fn headings_with_runs_wrap_an_inline_group() {
        let t = tree("## See [docs](https://e.com/) now\n\n### `code head`\n");
        assert_eq!(label(&t.items[0]), "section_header");
        assert_eq!(text(&t.items[0]), "");
        assert_eq!(label(&t.items[1]), "inline:group");
        assert_eq!(t.items[1].parent, Some(0));
        assert_eq!(text(&t.items[3]), "docs");
        assert_eq!(text(&t.items[5]), "code head");
        assert!(matches!(
            &t.items[5].kind,
            TreeKind::Text { level: Some(2), .. }
        ));
    }

    /// A GFM table sits under the current parent (docling#4488): a
    /// formatted cell is a rich cell whose runs are items of an `inline`
    /// group under a `rich_cell_group_*` group of the table, a plain one only
    /// its text; a table can interrupt a paragraph, and a hard break merges
    /// the next run into the previous text.
    #[test]
    fn gfm_tables_carry_rich_cells() {
        let t = tree(
            "- item\n\n  | **A** | B |\n  |---|---|\n  | `1` |\nintro\n| c |\n|---|\n\nline one  \nline two\n",
        );
        let labels: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(
            labels,
            [
                "list:list",
                "list_item",
                "table",
                "unspecified:rich_cell_group_0_0_0",
                "inline:group",
                "text",
                "unspecified:rich_cell_group_0_0_1",
                "inline:group",
                "code",
                "text",
                "table",
                "text",
            ]
        );
        // A list item is made lazily, from its first run, so the table after
        // that run is a child of the list, as in docling.
        assert_eq!(t.items[2].parent, Some(0));
        let TreeKind::Table {
            table, rich_cells, ..
        } = &t.items[2].kind
        else {
            panic!()
        };
        let cells = table.cells.as_ref().unwrap();
        assert_eq!(
            cells
                .iter()
                .map(|c| (c.text.as_str(), c.column_header))
                .collect::<Vec<_>>(),
            // The short row is padded to the header's width.
            [("A", true), ("B", true), ("1", false), ("", false)]
        );
        assert_eq!(rich_cells, &[(0, 0, 3), (1, 0, 6)]);
        let TreeKind::Text { formatting, .. } = &t.items[5].kind else {
            panic!()
        };
        assert!(formatting.is_some_and(|f| f.bold));
        assert_eq!(text(&t.items[9]), "intro");
        assert_eq!(t.items[10].parent, None);
        assert_eq!(text(&t.items[11]), "line one\nline two");
    }

    /// docling's `_GfmStrikethrough`: `~~x~~` with no tilde inside or around,
    /// flanking or not; a single tilde is text.
    #[test]
    fn strikethrough_follows_marko() {
        let t = tree("a ~~b~~ c ~d~ e ~~ f ~~ g\n");
        let runs: Vec<(String, bool)> = t
            .items
            .iter()
            .filter_map(|it| match &it.kind {
                TreeKind::Text {
                    text, formatting, ..
                } => Some((text.clone(), formatting.is_some_and(|f| f.strikethrough))),
                _ => None,
            })
            .collect();
        assert_eq!(
            runs,
            [
                ("a ".into(), false),
                ("b".into(), true),
                (" c ~d~ e ".into(), false),
                (" f ".into(), true),
                (" g".into(), false),
            ]
        );
    }

    /// A raw HTML block means upstream re-converts through the HTML backend;
    /// no tree is built (the flat export stays).
    /// A standalone display formula is a `formula` item; math inside prose
    /// leaves the runs as they were (one text run, not three around `$`).
    #[test]
    fn display_math_is_a_formula_item() {
        let t = tree("Costs $5 and $6 with $x$.\n\n$$\nE = mc^2\n$$\n\nAfter.\n");
        let items: Vec<(String, String)> = t
            .items
            .iter()
            .map(|it| (label(it), text(it).to_string()))
            .collect();
        assert_eq!(
            items,
            [
                ("text".to_string(), "Costs $5 and $6 with $x$.".to_string()),
                ("formula".to_string(), "E = mc^2".to_string()),
                ("text".to_string(), "After.".to_string()),
            ]
        );
    }

    /// A raw HTML block sends the document through the HTML backend: its
    /// markup is parsed (a table, not text), the rest keeps its structure.
    #[test]
    fn html_blocks_take_the_html_detour() {
        let t = build_tree(
            "# Title\n\npara\n\n<table><tr><th>a</th></tr><tr><td>b</td></tr></table>\n",
            &super::super::images::NoFetch,
        )
        .expect("a tree");
        let kinds: Vec<String> = t.items.iter().map(label).collect();
        assert_eq!(kinds, ["title", "text", "table"], "{kinds:?}");
        // The paragraph and the table nest under the title, as the HTML
        // backend nests everything after a heading.
        assert_eq!(t.items[1].parent, Some(0));
        assert_eq!(t.items[2].parent, Some(0));
    }

    /// Deep nesting is cut off instead of overflowing the stack.
    #[test]
    fn deep_nesting_is_bounded() {
        let deep = ">".repeat(50_000) + " x\n";
        assert!(build_tree(&deep, &super::super::images::NoFetch).is_some());
    }
}
