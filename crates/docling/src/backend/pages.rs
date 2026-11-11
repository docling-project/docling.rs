//! Apple Pages (`.pages`) — the content model docling's completed Pages
//! backend reads both container generations into (docling#4062, #383), and
//! how it becomes a [`DoclingDocument`].
//!
//! Both generations describe the same things — paragraphs made of runs, lists,
//! tables, pictures, page furniture, comments — so they are modelled once here
//! and read into that model by [`super::pages_iwa`] (Pages 5+, the IWA object
//! graph) and [`super::pages_xml`] (iWork '09, the `sf:` XML). Turning the
//! result into nodes is [`emit`]'s job, which keeps the two readers from having
//! to agree on anything else. Every function mirrors a name in upstream's
//! `docling/backend/iwork/content.py` or `iwork_backend.py`.

use docling_core::{
    inline_paragraph_node, ContentLayer, DoclingDocument, InlineRun, Node, Script, Table, TableCell,
};

use crate::backend::markdown::escape_text;

/// Apple marks inline attachments (images, footnote anchors) with U+FFFC inside
/// the text run. There is no text there to emit.
const OBJECT_REPLACEMENT: char = '\u{FFFC}';

/// `SuperscriptType` values of a character style: one raises the text, two
/// lowers it; zero is what Pages writes for ordinary text.
pub(crate) fn script_of(value: u64) -> Option<Script> {
    match value {
        1 => Some(Script::Super),
        2 => Some(Script::Sub),
        _ => None,
    }
}

/// `kNone`: the depth is unlabelled, which is what plain body text carries.
pub(crate) const LABEL_TYPE_NONE: u64 = 0;
/// `kString`: the depth draws a fixed marker — the entry at that depth of the
/// style's `strings`, a bullet character usually. `kImage` (1) draws a picture
/// instead and is treated the same way, since there is no text in it to show.
pub(crate) const LABEL_TYPE_STRING: u64 = 2;
/// `kNumber`: the depth is numbered, so the list is an ordered one.
pub(crate) const LABEL_TYPE_NUMBER: u64 = 3;

/// The character formatting docling records (`Formatting`): a run without any
/// of it carries `None` rather than an all-false value, as upstream's
/// `build_formatting` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct Formatting {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub script: Script,
}

impl Formatting {
    /// `build_formatting`: `None` when the style says nothing docling records.
    pub(crate) fn build(
        bold: bool,
        italic: bool,
        underline: bool,
        strike: bool,
        script: Option<Script>,
    ) -> Option<Formatting> {
        if !(bold || italic || underline || strike) && script.is_none() {
            return None;
        }
        Some(Formatting {
            bold,
            italic,
            underline,
            strike,
            script: script.unwrap_or_default(),
        })
    }
}

/// A stretch of text sharing one character style and one link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub text: String,
    pub fmt: Option<Formatting>,
    pub link: Option<String>,
}

impl Run {
    pub(crate) fn plain(text: impl Into<String>) -> Run {
        Run {
            text: text.into(),
            fmt: None,
            link: None,
        }
    }
}

/// How Pages labels one list item: its depth, and the marker it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListLabel {
    pub depth: usize,
    pub enumerated: bool,
    pub marker: String,
}

/// A Pages list style: what each nesting depth is labelled with. Both fields
/// are parallel arrays indexed by depth, so a style describes the whole ladder
/// of nine levels at once rather than one level at a time.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ListStyle {
    pub label_types: Vec<u64>,
    pub strings: Vec<String>,
}

impl ListStyle {
    /// How a paragraph at `depth` is labelled, or `None` if plain — which is
    /// what Pages' "None" style does at every depth, and is how a paragraph
    /// that merely inherits a list style stays body text.
    pub(crate) fn label(&self, depth: usize) -> Option<ListLabel> {
        let label_type = *self.label_types.get(depth)?;
        if label_type == LABEL_TYPE_NONE {
            return None;
        }
        if label_type == LABEL_TYPE_NUMBER {
            return Some(ListLabel {
                depth,
                enumerated: true,
                marker: String::new(),
            });
        }
        let marker = self.strings.get(depth).cloned().unwrap_or_default();
        // An image bullet has no text to show, so it falls back to the marker
        // docling uses for an unlabelled item.
        Some(ListLabel {
            depth,
            enumerated: false,
            marker: if marker.is_empty() {
                "-".to_string()
            } else {
                marker
            },
        })
    }
}

/// The docling label a Pages paragraph style implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Label {
    Text,
    Title,
    /// docling section-header level (1-based, capped at 6).
    Heading(u8),
}

/// One block of body text with the label its Pages style implies. Kept as
/// runs rather than a single string because Pages applies character styles to
/// arbitrary stretches of it, and a bold phrase in the middle of a sentence
/// has to stay attached to that phrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Paragraph {
    pub runs: Vec<Run>,
    pub label: Label,
    pub list: Option<ListLabel>,
    /// The comment anchors (annotation-field ids, comment-field ids) that fall
    /// inside the paragraph.
    pub anchors: Vec<String>,
}

impl Paragraph {
    pub(crate) fn new(runs: Vec<Run>, label: Label) -> Paragraph {
        Paragraph {
            runs,
            label,
            list: None,
            anchors: Vec::new(),
        }
    }

    /// The paragraph's full text, with its runs joined back together.
    pub(crate) fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }
}

/// An image anchored in the text flow. `data` is `None` when the image's
/// bytes are not in the container — Pages writes a placeholder for media it
/// has not downloaded — so the picture is still placed, just without an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Picture {
    pub data: Option<Vec<u8>>,
    pub name: String,
}

/// One series of a chart: its name, and its value in each category (`None`
/// where the chart has no number, so a gap stays in its place).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChartSeries {
    pub name: String,
    pub values: Vec<Option<f64>>,
}

/// A chart, and the data it was last drawn from — docling's `Chart`
/// (docling#4376, #466). iWork keeps no picture of a chart, only the model
/// the app draws one from, so this is everything there is to recover.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Chart {
    /// docling's picture classification label (`pie_chart`, `bar_chart`, …).
    pub label: String,
    /// The title the chart shows, if any.
    pub title: Option<String>,
    pub categories: Vec<String>,
    pub series: Vec<ChartSeries>,
}

impl Chart {
    /// `_chart_table`: the data laid out categories down and series across —
    /// a blank corner, a header row of series names, a row-header column of
    /// categories — the layout the PowerPoint and Excel backends give a
    /// chart's data. `None` when the chart holds nothing to plot.
    pub(crate) fn table(&self) -> Option<Table> {
        let rows = self
            .series
            .iter()
            .map(|s| s.values.len())
            .chain(std::iter::once(self.categories.len()))
            .max()
            .unwrap_or(0);
        if self.series.is_empty() || rows == 0 {
            return None;
        }
        let mut texts: Vec<Vec<String>> = Vec::with_capacity(rows + 1);
        texts.push(
            std::iter::once(String::new())
                .chain(self.series.iter().map(|s| s.name.clone()))
                .collect(),
        );
        for row in 0..rows {
            let category = self.categories.get(row).cloned().unwrap_or_default();
            texts.push(
                std::iter::once(category)
                    .chain(
                        self.series
                            .iter()
                            .map(|s| chart_value(s.values.get(row).copied().flatten())),
                    )
                    .collect(),
            );
        }
        let cells = texts
            .iter()
            .enumerate()
            .flat_map(|(row, line)| {
                line.iter().enumerate().map(move |(col, text)| TableCell {
                    text: text.clone(),
                    bbox: None,
                    start_row: row,
                    start_col: col,
                    row_span: 1,
                    col_span: 1,
                    column_header: row == 0,
                    row_header: row > 0 && col == 0,
                    row_section: false,
                })
            })
            .collect();
        Some(Table {
            rows: texts,
            cells: Some(cells),
            ..Default::default()
        })
    }
}

/// `_chart_value`: a value as the chart's data editor shows it — a whole
/// number without the `.0` a float prints with, nothing for a gap.
fn chart_value(value: Option<f64>) -> String {
    match value {
        None => String::new(),
        Some(v) if v.fract() == 0.0 && v.abs() < 1e15 => format!("{}", v as i64),
        Some(v) => format!("{v}"),
    }
}

/// One piece of document content, in the order Pages lays it out.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Block {
    Paragraph(Paragraph),
    Picture(Picture),
    Table(Table),
    Chart(Chart),
}

/// One comment thread entry, and the identifier of the text it annotates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Comment {
    pub text: String,
    pub anchor: String,
}

/// Everything one Pages document holds. Page furniture is kept apart from the
/// body flow rather than interleaved with it: a header belongs to every page a
/// page master covers, not to one point in the text, so there is no position
/// in `blocks` that would be right for it.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct Content {
    pub blocks: Vec<Block>,
    pub headers: Vec<Paragraph>,
    pub footers: Vec<Paragraph>,
    pub footnotes: Vec<Paragraph>,
    pub comments: Vec<Comment>,
}

/// The run tables of one `TSWP.StorageArchive`: each pairs a character index
/// with the value that applies from there until the next entry, so they are
/// read together when the storage is split into paragraphs. Indices count
/// code points, as Python's `str` does.
#[derive(Clone, Debug, Default)]
pub(crate) struct StorageRuns {
    pub styles: Vec<(usize, Option<String>)>,
    pub characters: Vec<(usize, Option<Formatting>)>,
    pub lists: Vec<(usize, Option<ListStyle>)>,
    pub depths: Vec<(usize, usize)>,
    pub links: Vec<(usize, Option<String>)>,
}

/// `clean`: drop the placeholders Apple writes where an inline attachment sits.
/// Whitespace is deliberately left alone: a run boundary can fall
/// mid-sentence, so the space on either side of a formatted phrase belongs to
/// the paragraph and is trimmed once, by [`trim`], rather than at every
/// boundary.
pub(crate) fn clean(text: &str) -> String {
    text.replace(OBJECT_REPLACEMENT, "")
}

/// `trim`: a paragraph's outer whitespace removed without disturbing its run
/// boundaries; empty runs dropped. Empty when the paragraph holds nothing but
/// whitespace.
pub(crate) fn trim(runs: Vec<Run>) -> Vec<Run> {
    let mut kept: Vec<Run> = runs.into_iter().filter(|r| !r.text.is_empty()).collect();
    while let Some(head) = kept.first_mut() {
        let trimmed = head.text.trim_start().to_string();
        if trimmed.is_empty() {
            kept.remove(0);
        } else {
            head.text = trimmed;
            break;
        }
    }
    while let Some(tail) = kept.last_mut() {
        let trimmed = tail.text.trim_end().to_string();
        if trimmed.is_empty() {
            kept.pop();
        } else {
            tail.text = trimmed;
            break;
        }
    }
    kept
}

/// `value_at`: the value a run table puts in force at `index` — the last
/// entry at or before it, or `None` when the table starts after it.
pub(crate) fn value_at<T: Clone>(table: &[(usize, T)], index: usize) -> Option<T> {
    let mut current = None;
    for (position, value) in table {
        if *position > index {
            break;
        }
        current = Some(value.clone());
    }
    current
}

/// `runs_for`: one line (a paragraph of the storage's text, as code points
/// starting at character `start`) cut into runs at the style and link
/// boundaries inside it, trimmed.
pub(crate) fn runs_for(line: &[char], start: usize, runs: &StorageRuns) -> Vec<Run> {
    let text_of = |from: usize, to: usize| clean(&line[from..to].iter().collect::<String>());
    if runs.characters.is_empty() && runs.links.is_empty() {
        return trim(vec![Run::plain(text_of(0, line.len()))]);
    }
    // Boundaries are absolute character indices; keep the ones inside this line.
    let end = start + line.len();
    let mut inside: Vec<usize> = runs
        .characters
        .iter()
        .map(|(i, _)| *i)
        .chain(runs.links.iter().map(|(i, _)| *i))
        .filter(|&i| start < i && i < end)
        .collect();
    inside.sort_unstable();
    inside.dedup();
    let mut boundaries = vec![start];
    boundaries.extend(inside);
    let mut pieces = Vec::new();
    for (position, &begin) in boundaries.iter().enumerate() {
        let stop = boundaries.get(position + 1).copied().unwrap_or(end);
        let text = text_of(begin - start, stop - start);
        if !text.is_empty() {
            pieces.push(Run {
                text,
                fmt: value_at(&runs.characters, begin).flatten(),
                link: value_at(&runs.links, begin).flatten(),
            });
        }
    }
    trim(pieces)
}

/// `list_label_at`: how the paragraph starting at `offset` is labelled as a
/// list item.
pub(crate) fn list_label_at(runs: &StorageRuns, offset: usize) -> Option<ListLabel> {
    let style = value_at(&runs.lists, offset).flatten()?;
    style.label(value_at(&runs.depths, offset).unwrap_or(0))
}

/// `split_paragraphs`: a storage's text split into labelled paragraphs of
/// formatted runs. Apple separates paragraphs with newlines and pads empty
/// ones, so blank results are dropped rather than emitted as empty items.
pub(crate) fn split_paragraphs(text: &[char], runs: &StorageRuns) -> Vec<Paragraph> {
    let mut paragraphs = Vec::new();
    let mut offset = 0usize;
    for line in text.split(|&c| c == '\n') {
        let pieces = runs_for(line, offset, runs);
        if !pieces.is_empty() {
            let label = label_for_style(value_at(&runs.styles, offset).flatten().as_deref());
            paragraphs.push(Paragraph {
                runs: pieces,
                label,
                list: list_label_at(runs, offset),
                anchors: Vec::new(),
            });
        }
        offset += line.len() + 1; // + 1 for the newline that split consumed
    }
    paragraphs
}

/// docling's `label_for_style`: Pages names its built-in styles the same way
/// in both container generations ("Title", "Heading 1", "Subheading", "Body"),
/// so one mapping serves the IWA and XML readers. Custom styles are unknown
/// and stay body text; so does an anonymous (ad-hoc formatting) style.
pub(crate) fn label_for_style(style_name: Option<&str>) -> Label {
    let Some(name) = style_name else {
        return Label::Text;
    };
    let name = name.trim();
    if name.is_empty() {
        return Label::Text;
    }
    let lowered = name.to_lowercase();
    if lowered == "title" {
        return Label::Title;
    }
    if lowered == "subtitle" || lowered == "subheading" {
        return Label::Heading(2);
    }
    // `^heading\s*(\d+)?$`: a bare "Heading" is the top level — Pages' Layout
    // template pairs it with "Subheading" rather than numbering them.
    if let Some(rest) = lowered.strip_prefix("heading") {
        let digits = rest.trim_start();
        if digits.is_empty() {
            return Label::Heading(1);
        }
        if digits.chars().all(|c| c.is_ascii_digit()) {
            let level = digits.parse::<u64>().unwrap_or(u64::MAX).min(6) as u8;
            return Label::Heading(level);
        }
    }
    Label::Text
}

/// `authored`: a comment prefixed with its author, the way the Word backend
/// renders one.
pub(crate) fn authored(author: Option<&str>, text: &str) -> String {
    match author {
        Some(a) => format!("[author: {a}]: {text}"),
        None => text.to_string(),
    }
}

/// `unique_paragraphs`: drop repeats (by text), keeping the first of each,
/// without reordering — Pages writes a first-page, an even-page and an odd-page
/// variant of every header and footer whether or not the author filled them in.
pub(crate) fn unique_paragraphs(paragraphs: Vec<Paragraph>) -> Vec<Paragraph> {
    let mut seen = std::collections::HashSet::new();
    paragraphs
        .into_iter()
        .filter(|p| seen.insert(p.text()))
        .collect()
}

// --- emission (upstream's `IWorkPagesDocumentBackend.convert`) --------------

/// Turn the content into nodes, in the order Pages lays the document out: the
/// blocks, then the page furniture (headers, footers, footnotes — docling's
/// furniture layer, out of the reading order), then the comments (its notes
/// layer, each linked to the paragraph holding the text it was written about
/// whenever that text was recovered).
pub(crate) fn emit(content: Content, doc: &mut DoclingDocument) {
    // `annotated.setdefault(anchor, item)`: a comment goes to the *first*
    // paragraph that carries its anchor.
    let mut anchored: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, block) in content.blocks.iter().enumerate() {
        if let Block::Paragraph(p) = block {
            for a in &p.anchors {
                anchored.entry(a.as_str()).or_insert(i);
            }
        }
    }
    // Comment index → the block it annotates.
    let mut targets: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for (ci, c) in content.comments.iter().enumerate() {
        if let Some(&block) = anchored.get(c.anchor.as_str()) {
            targets.entry(block).or_default().push(ci);
        }
    }

    let mut lists = ListStack::default();
    for (i, block) in content.blocks.iter().enumerate() {
        let node = match block {
            Block::Paragraph(p) => match &p.list {
                Some(label) => lists.item(p, label),
                None => {
                    lists.close();
                    paragraph_node(p)
                }
            },
            // A table or a picture ends any list it follows, the same as body text.
            Block::Picture(pic) => {
                lists.close();
                Node::Picture {
                    caption: None,
                    caption_href: None,
                    image: pic
                        .data
                        .as_ref()
                        .and_then(|d| crate::backend::ooxml::picture_image(&pic.name, d.clone())),
                    classification: None,
                    description: None,
                    caption_parent: Default::default(),
                    caption_location: None,
                }
            }
            Block::Table(t) => {
                lists.close();
                Node::Table(t.clone())
            }
            // Only the Keynote reader yields charts; the shape is the one
            // the PowerPoint backend gives them.
            Block::Chart(c) => {
                lists.close();
                Node::Chart {
                    kind: c.label.clone(),
                    table: c.table().unwrap_or_default(),
                    caption: c.title.clone(),
                    location: None,
                }
            }
        };
        match targets.remove(&i) {
            Some(comments) => doc.push(Node::Commented {
                comments,
                inner: Box::new(node),
            }),
            None => doc.push(node),
        }
    }
    // `_add_furniture`: headers, footers, footnotes on the furniture layer.
    for paragraphs in [&content.headers, &content.footers, &content.footnotes] {
        for p in paragraphs {
            doc.push(Node::Furniture {
                layer: ContentLayer::Furniture,
                inner: Box::new(paragraph_node(p)),
            });
        }
    }
    // `_add_comments`: docling-core's `add_comment` — a bare notes-layer text
    // item under the body (no `comment_section` group, unlike the docx
    // backend's), referenced from the item it annotates.
    for c in &content.comments {
        doc.push(Node::CommentSection {
            name: String::new(),
            text: escape_text(&c.text),
            refs_note_text: true,
            grouped: false,
        });
    }
}

/// The list groups open while consecutive list items keep arriving (upstream's
/// `_ListStack`). Pages records a nesting depth per paragraph rather than
/// opening and closing lists, so the structure is inferred: a deeper item opens
/// levels down to its depth, a shallower one closes back to it, and any other
/// block ends the list entirely. Our nodes carry the depth as `level` and the
/// start of a fresh top-level list as `first_in_list`; ordered items are
/// numbered by their position at their depth, as docling's serializer does.
#[derive(Default)]
pub(crate) struct ListStack {
    open: bool,
    /// Items emitted so far at each depth of the open list.
    counts: Vec<u64>,
}

impl ListStack {
    pub(crate) fn close(&mut self) {
        self.open = false;
        self.counts.clear();
    }

    pub(crate) fn item(&mut self, p: &Paragraph, label: &ListLabel) -> Node {
        let first_in_list = !self.open;
        self.open = true;
        self.counts.truncate(label.depth + 1);
        while self.counts.len() <= label.depth {
            self.counts.push(0);
        }
        self.counts[label.depth] += 1;
        let (uniform, _) = uniform_run(&p.runs);
        // `add_list_item(text=paragraph.text, formatting=uniform.formatting,
        // hyperlink=…)`: a list item carries a single formatting, so mixed
        // runs keep their text and lose the formatting.
        let text = match uniform {
            Some(run) => serialize_run(&p.text(), run.fmt, None),
            None => escape_text(&p.text()),
        };
        Node::ListItem {
            ordered: label.enumerated,
            number: self.counts[label.depth],
            first_in_list,
            text,
            level: label.depth.min(u8::MAX as usize) as u8,
            marker: Some(label.marker.clone()),
            location: None,
            split: None,
            dclx: None,
            href: uniform.and_then(|r| r.link.clone()),
            layer: None,
        }
    }
}

/// `_uniform`: whether every run shares one formatting and one link — then the
/// first run stands for the paragraph.
pub(crate) fn uniform_run(runs: &[Run]) -> (Option<&Run>, bool) {
    let runs: Vec<&Run> = runs.iter().filter(|r| !r.text.is_empty()).collect();
    let Some(first) = runs.first() else {
        return (None, true);
    };
    let uniform = runs
        .iter()
        .all(|r| r.fmt == first.fmt && r.link == first.link);
    (uniform.then_some(*first), uniform)
}

/// `_add_paragraph` / `_add_runs`: a title, a section header at its level, or
/// body text. A paragraph whose runs differ in formatting or link becomes an
/// inline group of items (docling's shape for mixed runs, whose Markdown
/// concatenates them, #997); a uniform one is a single item.
pub(crate) fn paragraph_node(p: &Paragraph) -> Node {
    match p.label {
        // `Node::Heading` level 1 is docling's title; a section header of
        // docling level N is our level N + 1. Headings carry the plain text.
        Label::Title => Node::Heading {
            level: 1,
            text: escape_text(&p.text()),
        },
        Label::Heading(level) => Node::Heading {
            level: level.saturating_add(1),
            text: escape_text(&p.text()),
        },
        Label::Text => {
            let runs: Vec<&Run> = p.runs.iter().filter(|r| !r.text.is_empty()).collect();
            let (uniform, is_uniform) = uniform_run(&p.runs);
            if is_uniform {
                let fmt = uniform.and_then(|r| r.fmt);
                let link = uniform.and_then(|r| r.link.as_deref());
                let md = serialize_run(&p.text(), fmt, link);
                let inline: Vec<InlineRun> = uniform
                    .map(|r| vec![inline_run(&p.text(), r.fmt)])
                    .unwrap_or_default();
                inline_paragraph_node(md, inline, false)
            } else {
                // The runs keep their own whitespace and concatenate, their
                // markers hoisted inside it (docling-core 2.106, #997):
                // `Plain ` + `bold` is `Plain **bold**`.
                let md: String = runs
                    .iter()
                    .map(|r| {
                        docling_core::inline::hoist(&r.text, |core| {
                            serialize_run(core, r.fmt, r.link.as_deref())
                        })
                    })
                    .collect();
                let md = md.trim().to_string();
                let inline = runs.iter().map(|r| inline_run(&r.text, r.fmt)).collect();
                inline_paragraph_node(md, inline, false)
            }
        }
    }
}

fn inline_run(text: &str, fmt: Option<Formatting>) -> InlineRun {
    let f = fmt.unwrap_or_default();
    InlineRun {
        text: text.to_string(),
        bold: f.bold,
        italic: f.italic,
        underline: f.underline,
        strike: f.strike,
        script: f.script,
        code: false,
        formula: false,
    }
}

/// docling-core's Markdown for one formatted run: `**bold**`, `*italic*`,
/// `~~strikethrough~~`, `[text](link)`; underline and sub/superscript have no
/// Markdown marker (they ride on the structured run for DocLang).
fn serialize_run(text: &str, fmt: Option<Formatting>, link: Option<&str>) -> String {
    let mut s = escape_text(text);
    if let Some(f) = fmt {
        if f.bold {
            s = format!("**{s}**");
        }
        if f.italic {
            s = format!("*{s}*");
        }
        if f.strike {
            s = format!("~~{s}~~");
        }
    }
    if let Some(url) = link {
        s = docling_core::hyperlink::markdown_link(&s, url);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// docling's `label_for_style` table (its test_style_names_map_to_labels).
    #[test]
    fn style_names_map_to_labels_like_docling() {
        use Label::*;
        for (name, want) in [
            (Some("Title"), Title),
            (Some("Heading 1"), Heading(1)),
            (Some("Heading 2"), Heading(2)),
            (Some("Heading"), Heading(1)),
            (Some("heading 9"), Heading(6)),
            (Some("Subheading"), Heading(2)),
            (Some("Subtitle"), Heading(2)),
            (Some("Body"), Text),
            (Some("Free Form"), Text),
            (Some("Footnote Text"), Text),
            (Some("Heading one"), Text),
            (None, Text),
        ] {
            assert_eq!(label_for_style(name), want, "{name:?}");
        }
    }

    /// docling's `split_paragraphs`: runs keyed by code-point index, blanks
    /// dropped, U+FFFC attachments removed.
    #[test]
    fn paragraph_split_follows_style_runs() {
        let runs = StorageRuns {
            styles: vec![(0, Some("Title".into())), (6, Some("Body".into()))],
            ..Default::default()
        };
        let paras = split_paragraphs(&chars("Titl\u{FFFC}e\n\nBödy one\nBody two"), &runs);
        let got: Vec<(String, Label)> = paras.iter().map(|p| (p.text(), p.label)).collect();
        assert_eq!(
            got,
            vec![
                ("Title".to_string(), Label::Title),
                ("Bödy one".to_string(), Label::Text),
                ("Body two".to_string(), Label::Text),
            ]
        );
    }

    /// Character-style and link boundaries cut a line into runs; the space
    /// around a formatted phrase stays with the paragraph (trimmed once, at the
    /// ends), and a run boundary at the very start does not make an empty run.
    #[test]
    fn runs_keep_their_boundary_spaces_and_trim_only_the_ends() {
        let bold = Formatting::build(true, false, false, false, None);
        // "see bold word here!  " — bold covers [4, 8), the link [14, 18).
        let runs = StorageRuns {
            characters: vec![(0, None), (4, bold), (8, None)],
            links: vec![(14, Some("https://x.y/".into())), (18, None)],
            ..Default::default()
        };
        let got = runs_for(&chars("see bold word here!  "), 0, &runs);
        assert_eq!(
            got,
            vec![
                Run::plain("see "),
                Run {
                    text: "bold".into(),
                    fmt: bold,
                    link: None
                },
                Run::plain(" word "),
                Run {
                    text: "here".into(),
                    fmt: None,
                    link: Some("https://x.y/".into())
                },
                Run::plain("!"),
            ]
        );
        // A whitespace-only paragraph is nothing.
        assert!(runs_for(&chars("   "), 0, &runs).is_empty());
    }

    /// A list style's label ladder: `kNone` leaves a depth plain, `kNumber`
    /// numbers it, `kString`/`kImage` draw the marker (or `-` without one).
    #[test]
    fn list_styles_label_each_depth() {
        let style = ListStyle {
            label_types: vec![LABEL_TYPE_STRING, LABEL_TYPE_NUMBER, 1, LABEL_TYPE_NONE],
            strings: vec!["•".into(), String::new(), String::new()],
        };
        assert_eq!(
            style.label(0),
            Some(ListLabel {
                depth: 0,
                enumerated: false,
                marker: "•".into()
            })
        );
        assert_eq!(
            style.label(1),
            Some(ListLabel {
                depth: 1,
                enumerated: true,
                marker: String::new()
            })
        );
        assert_eq!(style.label(2).map(|l| l.marker), Some("-".to_string()));
        assert_eq!(style.label(3), None);
        assert_eq!(style.label(9), None);
    }

    /// Emission: a uniform bold paragraph is one Markdown-marked item, mixed
    /// runs join with single spaces, consecutive list items at rising depths
    /// number by position, a table ends the list, and a comment is a bare
    /// notes-layer item linked from the paragraph carrying its anchor.
    #[test]
    fn emission_mirrors_the_upstream_convert() {
        let bold = Formatting::build(true, false, false, false, None);
        let mut para = Paragraph::new(
            vec![
                Run::plain("Plain "),
                Run {
                    text: "bold".into(),
                    fmt: bold,
                    link: None,
                },
            ],
            Label::Text,
        );
        para.anchors = vec!["a1".into()];
        let item = |text: &str, depth: usize, enumerated: bool| {
            let mut p = Paragraph::new(vec![Run::plain(text)], Label::Text);
            p.list = Some(ListLabel {
                depth,
                enumerated,
                marker: if enumerated {
                    String::new()
                } else {
                    "•".into()
                },
            });
            p
        };
        let content = Content {
            blocks: vec![
                Block::Paragraph(Paragraph::new(vec![Run::plain("T")], Label::Title)),
                Block::Paragraph(para),
                Block::Paragraph(Paragraph::new(
                    vec![Run {
                        text: "all bold".into(),
                        fmt: bold,
                        link: None,
                    }],
                    Label::Text,
                )),
                Block::Paragraph(item("one", 0, true)),
                Block::Paragraph(item("two", 0, true)),
                Block::Paragraph(item("deep", 1, false)),
                Block::Table(Table {
                    rows: vec![vec!["c".into()]],
                    ..Default::default()
                }),
                Block::Paragraph(item("again", 0, false)),
            ],
            headers: vec![Paragraph::new(vec![Run::plain("HEAD")], Label::Text)],
            comments: vec![Comment {
                text: "note".into(),
                anchor: "a1".into(),
            }],
            ..Default::default()
        };
        let mut doc = DoclingDocument::new("t");
        emit(content, &mut doc);
        let md = doc.export_to_markdown();
        // Mixed runs: docling-core wraps each run's raw text and joins the
        // inline parts with one space, so "Plain " + "bold" reads
        // "Plain  **bold**" — the double space is upstream's, kept for parity.
        assert_eq!(
            md,
            "# T\n\nPlain **bold**\n\n**all bold**\n\n1. one\n2. two\n    - deep\n\n| c   |\n|-----|\n\n- again\n"
        );
        assert!(matches!(&doc.nodes[1], Node::Commented { comments, .. } if comments == &[0]));
        assert!(matches!(
            &doc.nodes[2],
            Node::InlineGroup { runs, .. } if runs.len() == 1 && runs[0].bold
        ));
        assert!(matches!(
            doc.nodes.last(),
            Some(Node::CommentSection { grouped: false, text, .. }) if text == "note"
        ));
        let v: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let notes: Vec<&serde_json::Value> = v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["content_layer"] == "notes")
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["parent"]["$ref"], "#/body");
        assert_eq!(notes[0]["label"], "text");
        let annotated = &v["texts"][1];
        assert_eq!(annotated["comments"][0]["$ref"], notes[0]["self_ref"]);
        assert!(v["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["label"] != "comment_section"));
    }
}
