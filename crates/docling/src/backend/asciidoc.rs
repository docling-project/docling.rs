//! AsciiDoc backend.
//!
//! A line-oriented port of docling's `AsciiDocBackend._parse` (as of 2.140):
//! titles (`= `), section headers (`== `…), bullet/numbered lists (with `+`
//! continuation and literal-block/image children), `....`/`----`-delimited
//! literal and listing blocks, bare and `|===`-delimited tables (with
//! cell-format-specifier stripping), images and captions, and multi-line
//! paragraphs with `*bold*`, `_italic_` and `` `mono` `` marks. Comments,
//! block attributes and anchors are skipped.
//!
//! Alongside the flat nodes the parser builds docling's item tree for the
//! JSON export, mirroring `_parse`'s `parents` / `indents` bookkeeping: the
//! title is the root the sections hang off, a heading is parented to the
//! nearest present ancestor level, every list opens a `list` group at the
//! next level and a nested list (a deeper indent, or another marker kind)
//! a group under the item above it, items are children of the innermost
//! group, a `+`-continued picture or literal block is a child of the open
//! list item, block titles become body-level `caption` items that tables /
//! pictures / code reference — or a bold paragraph before anything else. The
//! flat list nodes take their depth and numbers from that tree, so the
//! Markdown nests and numbers exactly as docling-core serializes it.

use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind};
use docling_core::{DoclingDocument, InlineRun, Node, Table, TableCell};

use crate::backend::images::{FsImageResolver, ImagePolicy, ImageResolver, ImageSources, NoFetch};
use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

/// AsciiDoc cell specifier, e.g. `2*`, `^`, `.^`, `h` — the run that can be
/// glued before a `|` in a table line. Mirrors docling's `_CELL_SPEC`.
/// AsciiDoc writes the span as `[colspan][.rowspan]` followed by `+` or `*`,
/// and either number may be omitted, so `.2+` is a rowspan on its own
/// (docling#4290, 2.129); requiring one of the two keeps a bare `+` from
/// matching.
const CELL_SPEC: &str = r"(?:(?:\d+(?:\.\d+)?|\.\d+)[*+])*[<^>]?(?:\.[<^>])?[adehlms]?";

/// docling's `_LIST_ITEM_PATTERN` (docling#4118, docling#4403, docling#4827):
/// besides `*`, `-` and `1.`, AsciiDoc's dotted (`.`, `..`, `...`) and
/// one-letter (`a.`, `B.`) ordered markers open list items, and a run of
/// `*`s nests a bullet the way a run of `.`s nests a number. One letter
/// only, so a line that starts with an abbreviation (`Dr.`, `Fig.`) stays
/// text.
const LIST_ITEM: &str = r"^(\s*)(\*+|-|\.+|\d+\.|[a-zA-Z]\.)\s+(.*)";

/// The delimiter opening and closing a literal block.
const LITERAL_FENCE: &str = "....";
/// The delimiter opening and closing a listing block — a `code` item like the
/// literal block, optionally labelled by a `[source,lang]` attribute line
/// right before it (docling#4400).
const LISTING_FENCE: &str = "----";
/// A line of four or more slashes opens a comment block, closed only by an
/// identical line; exactly two slashes start a line comment — `///` and
/// `//// text` are body text, as Asciidoctor reads them (docling#4394).
fn is_comment_block_delimiter(stripped: &str) -> bool {
    stripped.len() >= 4 && stripped.bytes().all(|b| b == b'/')
}
/// Example, sidebar, quote, open and passthrough block delimiters: consumed
/// so they do not leak into the text — with a matching closer the lines up
/// to it are read like any others, without one the delimiter only breaks
/// the paragraph (docling#4662).
const CONTENT_BLOCK_DELIMITERS: &[&str] = &["====", "****", "____", "--", "++++"];
/// `[source]` / `[source,python]` / `[source, java, linenums]`: the optional
/// language is the first attribute after `source`.
const SOURCE_ATTR: &str = r"^\[source(?:,\s*([\w+#.-]+))?[^\]]*\]$";

#[derive(Default)]
pub struct AsciiDocBackend {
    /// Which `image::target[]` targets resolve to the actual image bytes
    /// (#646) — docling's `AsciiDocBackendOptions.fetch_images` together with
    /// its `enable_local_fetch`/`enable_remote_fetch` (docling#4156). Off by
    /// default, matching docling: a picture is then emitted with no image at
    /// all (until 2.126 docling fabricated a `file://…` `ImageRef` here).
    pub images: ImagePolicy,
}

impl DeclarativeBackend for AsciiDocBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        if self.images.sources != ImageSources::None {
            let resolver = FsImageResolver::new(
                source.base_dir().map(|p| p.to_path_buf()),
                source.base_url.clone(),
                self.images.clone(),
            );
            Ok(parse(&text, &source.name, &resolver))
        } else {
            Ok(parse(&text, &source.name, &NoFetch))
        }
    }
}

fn parse(text: &str, name: &str, images: &dyn ImageResolver) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    let mut p = Parser {
        images: Some(images),
        parents: vec![None; LEVELS],
        indents: vec![None; LEVELS],
        list_kinds: vec![None; LEVELS],
        ..Parser::default()
    };
    for block in blocks(text) {
        match block {
            Block::Line(line) => p.feed(line, &mut doc),
            Block::Literal { code, language } => p.feed_literal(code, language, &mut doc),
            Block::Comment(text) => p.feed_comment(text, &mut doc),
        }
    }
    p.finish(&mut doc);
    // The runs keep their boundary whitespace (schema 1.11.0, #997).
    p.tree.inline_whitespace = true;
    doc.tree = Some(std::mem::take(&mut p.tree));
    doc
}

/// docling keeps ten section levels (`parents[0..10]`).
const LEVELS: usize = 10;

/// One unit of input: a raw line, the body of a `....` literal / `----`
/// listing block (with the `[source,lang]` language, if any), or a comment's
/// text.
enum Block<'a> {
    Line(&'a str),
    Literal {
        code: String,
        language: Option<String>,
    },
    /// A `//` comment (a run of consecutive `//` lines is one) or a `////`
    /// comment block, its delimiters removed (#863, docling#4663).
    Comment(String),
}

/// docling's `_make_comment`: the lines joined, stripped; a comment
/// without text is none.
fn comment(lines: &[String]) -> Option<Block<'static>> {
    let text = lines.join("\n").trim().to_string();
    (!text.is_empty()).then_some(Block::Comment(text))
}

/// docling's `_iter_blocks`: fold each `....`- or `----`-delimited run of
/// lines into a single code block — only the matching closer ends it, and a
/// `[source,lang]` attribute line consumed right before a `----` labels the
/// listing's language. A comment is a block of its own (#863, docling#4663,
/// 2.141): a run of consecutive `//` lines is one comment, everything
/// between two identical `////`(`/`…) lines another, and an unclosed one comments out
/// the rest of the document, as Asciidoctor reads it. A comment block hides
/// every construct, so it is checked ahead of the block openers (a `----`
/// between `////` delimiters is comment text). An example / sidebar / quote
/// / open / passthrough block's delimiter lines are dropped and the lines
/// between them run through this loop like any other (a comment inside one
/// is a comment); its closer leaves a blank line so the text after it
/// starts a fresh paragraph; a delimiter with no closer (a stray `--`
/// separator) is a blank line, so it breaks the paragraph without
/// swallowing the rest of the document or reaching the text
/// (docling#4662). An unterminated literal block still yields its body at
/// EOF.
fn blocks(text: &str) -> Vec<Block<'_>> {
    // Python's `split("\n")`: a trailing newline leaves an empty last line,
    // which an unterminated literal block keeps (`no close\n`).
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let source_attr = cached_regex!(SOURCE_ATTR);
    // Exactly two slashes: `///` is text and `////` a block delimiter.
    let is_line_comment = |l: &str| l.starts_with("//") && !l.starts_with("///");
    let mut out = Vec::new();
    let mut comment_data: Option<Vec<String>> = None;
    // The line that opened the comment block: only an identical one closes it.
    let mut comment_delimiter = "";
    // Content block delimiters still waiting for their closer, innermost
    // last.
    let mut open_content: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let stripped = lines[i].trim();
        if let Some(data) = comment_data.as_mut() {
            if stripped == comment_delimiter {
                out.extend(comment(data));
                comment_data = None;
            } else {
                data.push(lines[i].to_string());
            }
            i += 1;
            continue;
        }
        if is_comment_block_delimiter(stripped) {
            comment_data = Some(Vec::new());
            comment_delimiter = stripped;
            i += 1;
            continue;
        }
        if is_line_comment(lines[i]) {
            let mut run = Vec::new();
            while i < lines.len() && is_line_comment(lines[i]) {
                run.push(lines[i][2..].trim().to_string());
                i += 1;
            }
            out.extend(comment(&run));
            continue;
        }
        if stripped == LITERAL_FENCE || stripped == LISTING_FENCE {
            let (body, next) = delimited_body(&lines, i + 1, stripped);
            out.push(Block::Literal {
                code: body,
                language: None,
            });
            i = next;
            continue;
        }
        if open_content.last() == Some(&stripped) {
            open_content.pop();
            out.push(Block::Line(""));
            i += 1;
            continue;
        }
        if CONTENT_BLOCK_DELIMITERS.contains(&stripped) {
            if lines[i + 1..].iter().any(|l| l.trim() == stripped) {
                open_content.push(stripped);
            } else {
                out.push(Block::Line(""));
            }
            i += 1;
            continue;
        }
        if let Some(caps) = source_attr.captures(stripped) {
            if lines.get(i + 1).is_some_and(|l| l.trim() == LISTING_FENCE) {
                let language = caps.get(1).map(|m| m.as_str().to_string());
                let (body, next) = delimited_body(&lines, i + 2, LISTING_FENCE);
                out.push(Block::Literal {
                    code: body,
                    language,
                });
                i = next;
                continue;
            }
        }
        out.push(Block::Line(lines[i]));
        i += 1;
    }
    if let Some(data) = comment_data {
        out.extend(comment(&data));
    }
    out
}

/// The lines from `start` up to the next `delimiter` line (or EOF), joined,
/// and the index after that closer.
fn delimited_body(lines: &[&str], start: usize, delimiter: &str) -> (String, usize) {
    let mut i = start;
    let mut body: Vec<&str> = Vec::new();
    while i < lines.len() && lines[i].trim() != delimiter {
        body.push(lines[i].trim_end_matches(['\r', '\n']));
        i += 1;
    }
    (body.join("\n"), (i + 1).min(lines.len()))
}

#[derive(Default)]
struct Parser<'r> {
    text_data: Vec<String>,
    caption_data: Vec<String>,
    table_data: Vec<Vec<String>>,
    in_list: bool,
    in_table: bool,
    /// A `|===` table is open: a blank line only separates its rows and a
    /// line without a leading `|` continues the cell above (docling#4761).
    in_delimited_table: bool,
    /// Whether the next emitted item starts a fresh list (for the serializer).
    fresh_list: bool,
    /// Index in `doc.nodes` of the most recent list item, so a continuation
    /// block can be attached to it as docling attaches a child.
    last_item: Option<usize>,
    /// Set by a lone `+` inside a list: the next literal block or image stays
    /// inside the open item instead of closing the list.
    list_continuation: bool,
    images: Option<&'r dyn ImageResolver>,
    /// docling's item tree (see the module docs).
    tree: ItemTree,
    /// docling's `parents`: the item open at each level — the title at 0,
    /// headings at their level, list groups at the level after their parent.
    parents: Vec<Option<usize>>,
    /// docling's `indents`: the indent width that opened the list group at a
    /// level (left in place when a list closes, exactly like upstream).
    indents: Vec<Option<usize>>,
    /// docling's `list_kinds`: the marker kind of the list group at each
    /// level (`.` for every ordered marker, else the bullet character), for
    /// the list being parsed (docling#4822).
    list_kinds: Vec<Option<char>>,
    /// The tree id of the most recent list item — docling's `last_list_item`.
    last_item_tree: Option<usize>,
    /// The unescaped text of the caption [`Self::take_caption`] last returned.
    raw_caption: String,
    /// Comments met inside a list, for the flat nodes: they reach them when
    /// the list ends (see [`Parser::feed_comment`]).
    list_notes: Vec<String>,
}

/// What is being fed to [`Parser::close_list_if_needed`] — docling passes a
/// `<literal-block>` sentinel line for the block case, which matches none of
/// its line predicates but is recognised as a continuation block.
enum Trigger<'a> {
    Line(&'a str),
    Literal,
}

impl Parser<'_> {
    /// docling's `_get_current_level`: the level before the first empty slot.
    fn current_level(&self) -> usize {
        (1..LEVELS)
            .find(|&k| self.parents[k].is_none())
            .map_or(0, |k| k - 1)
    }

    /// docling's `_get_current_parent`: the item at [`Self::current_level`].
    fn current_parent(&self) -> Option<usize> {
        (1..LEVELS)
            .find(|&k| self.parents[k].is_none())
            .and_then(|k| self.parents[k - 1])
    }

    fn is_list_group(&self, id: usize) -> bool {
        matches!(&self.tree.items[id].kind, TreeKind::Group { label, .. } if label == "list")
    }

    /// A text item in the tree (`add_text` / `add_heading` / `add_list_item`).
    fn add_tree_text(
        &mut self,
        parent: Option<usize>,
        label: &str,
        text: &str,
        formatting: Option<Formatting>,
        level: Option<u8>,
        list: Option<ListMeta>,
    ) -> usize {
        self.tree.add(
            parent,
            None,
            TreeKind::Text {
                label: label.into(),
                text: text.into(),
                orig: None,
                formatting,
                hyperlink: None,
                level,
                list,
            },
        )
    }

    /// docling: `parents[level + 1] = add_group(parent=…, name="list",
    /// label=LIST)`, `indents[level + 1] = indent`,
    /// `list_kinds[level + 1] = kind`.
    fn open_list_group(&mut self, level: usize, parent: Option<usize>, indent: usize, kind: char) {
        if level + 1 >= LEVELS {
            return;
        }
        let g = self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: "list".into(),
                name: "list".into(),
            },
        );
        self.parents[level + 1] = Some(g);
        self.indents[level + 1] = Some(indent);
        self.list_kinds[level + 1] = Some(kind);
    }

    /// A block title claimed by a table / picture / code block: docling adds
    /// it with `add_text(label=CAPTION)` and *no parent* — a body-level item
    /// the floating item then references in `captions`.
    fn add_tree_caption(&mut self, text: &str) -> usize {
        self.add_tree_text(None, "caption", text, None, None, None)
    }

    /// docling's `_close_list_if_needed`: anything that is not a list item, a
    /// blank line, a `+`, or a continuation block claimed by a preceding `+`
    /// ends the open list — every level of it, so the text after a nested
    /// list is not added to the outer list group (docling#4721). The line is
    /// *not* swallowed: it goes on to be parsed as a heading/table/picture/
    /// text.
    fn close_list_if_needed(&mut self, trigger: Trigger) {
        if !self.in_list {
            return;
        }
        let keep = match trigger {
            Trigger::Literal => self.list_continuation,
            Trigger::Line(line) => {
                let stripped = line.trim();
                list_item(line).is_some()
                    || stripped.is_empty()
                    || stripped == "+"
                    || (self.list_continuation && is_picture(line))
            }
        };
        if !keep {
            // The indents stay recorded, as upstream leaves them.
            let mut level = self.current_level();
            while level > 0 && self.parents[level].is_some_and(|g| self.is_list_group(g)) {
                self.parents[level] = None;
                level -= 1;
            }
            self.end_list();
        }
    }

    /// A `....` literal or `----` listing block: docling's `add_code`, nested
    /// under the open list item when a `+` claimed it. A `[source,lang]`
    /// hint is trusted over the content, an unknown one falls back to the
    /// content's markers (docling's `detect_code_language`); without a hint
    /// the language stays unknown, as upstream passes `None`.
    fn feed_literal(&mut self, code: String, language: Option<String>, doc: &mut DoclingDocument) {
        let language = language
            .and_then(|hint| super::code_language::detect_code_language(&code, Some(&hint)));
        self.close_list_if_needed(Trigger::Literal);
        if !self.in_list {
            self.flush_list_notes(doc);
        }
        self.flush_text(doc);
        // A pending block title is the code item's *caption* since docling
        // 2.127 (`add_code(caption=…)`), and docling-core's Markdown renders
        // a code item's captions after the block — where 2.126 wrote the
        // title as a text item ahead of it.
        let caption = self.take_caption();
        let tree_caption = if caption.is_some() {
            let raw = self.raw_caption.clone();
            Some(self.add_tree_caption(&raw))
        } else {
            None
        };
        let parent = if self.in_list {
            self.last_item_tree
        } else {
            self.current_parent()
        };
        // docling's `add_code(caption=…)`: the code item references the
        // body-level caption in its `captions`.
        self.tree.add(
            parent,
            None,
            TreeKind::Code {
                text: code.clone(),
                orig: None,
                language: language.clone(),
                formatting: None,
                hyperlink: None,
                captions: tree_caption.into_iter().collect(),
            },
        );
        if !(self.in_list && self.fold_child(doc, &docling_core::code_fence("", &code))) {
            doc.push(Node::Code {
                language,
                text: code,
                orig: None,
                pretty: None,
            });
        }
        if let Some(text) = caption {
            doc.push(Node::Caption { text, href: None });
        }
        self.list_continuation = false;
    }

    fn feed(&mut self, line: &str, doc: &mut DoclingDocument) {
        let stripped = line.trim();
        if self.in_delimited_table && self.continue_table(line) {
            return;
        }
        self.close_list_if_needed(Trigger::Line(line));
        if !self.in_list {
            self.flush_list_notes(doc);
        }

        // Block attributes, anchors and entry attributes (`[cols="2,1"]`,
        // `[NOTE]`, `[[some-id]]`, `:experimental:`) are metadata for the
        // block that follows or the document, never body content
        // (docling#4431). An empty `[]` is not one.
        if cached_regex!(r"^\[\[.+\]\]$").is_match(stripped)
            || cached_regex!(r"^\[[^\[\]]+\]$").is_match(stripped)
            || cached_regex!(r"^:[\w.-]+:(\s.*)?$").is_match(stripped)
        {
            return;
        }

        // Title: `= ` — the root of the section tree.
        if let Some(rest) = is_title(line) {
            doc.push(Node::Heading {
                level: 1,
                text: escape_text(rest.trim()),
            });
            // docling: `parents[0] = add_text(label=TITLE)` — on the body.
            let id = self.add_tree_text(None, "title", rest.trim(), None, None, None);
            self.parents[0] = Some(id);
            return;
        }

        // Section header: `==+ `. docling ≥ 2.127 parents a heading to its
        // nearest *present* ancestor level, so a `====` straight under a `==`
        // stays where it is read (docling ≤ 2.126 hung it off the body root,
        // which rendered it after the whole section tree).
        if let Some((n, text)) = section_header(line) {
            // A heading ends the paragraph being accumulated: without this
            // flush the text before it joined the text after it and landed
            // in the wrong section (docling#4428).
            self.flush_text(doc);
            doc.push(Node::Heading {
                level: n.min(6),
                text: escape_text(text.trim()),
            });
            // docling: level = `=` count − 1, parent = the nearest present
            // ancestor level, deeper levels cleared.
            let level = (usize::from(n) - 1).min(LEVELS - 1);
            let ancestor = (0..level).rev().find_map(|k| self.parents[k]);
            let id = self.add_tree_text(
                ancestor,
                "section_header",
                text.trim(),
                None,
                Some(level as u8),
                None,
            );
            self.parents[level] = Some(id);
            for k in level + 1..LEVELS {
                self.parents[k] = None;
            }
            return;
        }

        // List item
        if let Some(item) = list_item(line) {
            self.push_list_item(item, doc);
            return;
        }
        // A lone `+` inside a list: the next literal block or image belongs to
        // the open item.
        if self.in_list && stripped == "+" {
            self.list_continuation = true;
            return;
        }

        // `|===` opens or closes a delimited table.
        if stripped == "|===" {
            if self.in_table {
                self.flush_table(doc);
            } else {
                self.in_table = true;
                self.in_delimited_table = true;
            }
            return;
        }
        // A table row: one cell per line is enough (docling#4431, the
        // vertical layout).
        if is_table_line(line) {
            self.in_table = true;
            self.table_data.push(parse_table_line(line));
            return;
        }
        // Any other line ends a bare table — and goes with it, as docling's
        // `elif in_table` arm consumes it.
        if self.in_table {
            self.flush_table(doc);
            return;
        }

        // Picture
        if let Some(uri) = picture_uri(line) {
            self.push_picture(&uri, doc);
            return;
        }

        // Caption: a line beginning with `.` followed by a non-space (only when
        // none is pending)
        if let Some(rest) = is_caption(line) {
            if self.caption_data.is_empty() {
                self.caption_data.push(rest.to_string());
                return;
            }
        }
        // Continuation of a multi-line caption
        if !stripped.is_empty() && !self.caption_data.is_empty() {
            self.caption_data.push(stripped.to_string());
            return;
        }

        if stripped.is_empty() {
            // docling's `_flush_blank_line`: the paragraph, then a block title
            // no floating item claimed, as a bold paragraph (docling#4431 — it
            // used to absorb every line after it).
            self.flush_text(doc);
            self.flush_caption_as_paragraph(doc);
        } else {
            self.text_data.push(stripped.to_string());
        }
    }

    /// docling's `_continue_table`: inside `|===`, a blank line only
    /// separates rows and a line without a leading `|` continues the cell
    /// above it (docling#4761). `false` for any other line — the closer, a
    /// row, or text before the first row.
    fn continue_table(&mut self, line: &str) -> bool {
        let stripped = line.trim();
        if stripped == "|===" || is_table_line(line) {
            return false;
        }
        if stripped.is_empty() {
            return true;
        }
        let Some(cell) = self.table_data.last_mut().and_then(|row| row.last_mut()) else {
            return false;
        };
        *cell = format!("{cell} {stripped}").trim_start().to_string();
        true
    }

    /// A pending block title that no picture, table or code block claimed:
    /// docling's `_flush_caption_as_paragraph`, a bold paragraph under `parent`
    /// (a list group has no caption slot either).
    fn flush_caption_as_paragraph_under(
        &mut self,
        parent: Option<usize>,
        doc: &mut DoclingDocument,
    ) {
        if let Some(text) = self.take_caption() {
            doc.push(Node::Paragraph {
                text: format!("**{text}**"),
            });
            let raw = self.raw_caption.clone();
            self.add_tree_text(
                parent,
                "paragraph",
                &raw,
                Some(Formatting {
                    bold: true,
                    ..Formatting::default()
                }),
                None,
                None,
            );
        }
    }

    fn flush_caption_as_paragraph(&mut self, doc: &mut DoclingDocument) {
        let parent = self.current_parent();
        self.flush_caption_as_paragraph_under(parent, doc);
    }

    fn push_list_item(&mut self, item: ListItem<'_>, doc: &mut DoclingDocument) {
        // docling's `parents` / `indents` bookkeeping for the tree.
        let mut level = self.current_level();
        let kind = item.kind();
        if !self.in_list {
            self.in_list = true;
            // A block title pending in front of a list (the `.Procedure` /
            // `.Verification` lead-ins): docling ≥ 2.127 emits it as a *bold*
            // paragraph right before the list.
            let parent = self.parents[level];
            self.flush_caption_as_paragraph_under(parent, doc);
            self.list_kinds = vec![None; LEVELS];
            self.open_list_group(level, parent, item.indent, kind);
            self.fresh_list = true;
        } else if item.indent > self.indents[level].unwrap_or(0) {
            // A nested list belongs to the item above it (docling#4721): as a
            // sibling of the items it would count as an item of the outer
            // list, numbering the next ordered item one too high.
            let parent = self.last_item_tree;
            self.open_list_group(level, parent, item.indent, kind);
        } else if item.indent < self.indents[level].unwrap_or(0) {
            while level > 0 && item.indent < self.indents[level].unwrap_or(0) {
                // Only pop while an outer group exists to fall back to;
                // otherwise the current group stays the list root.
                if self.indents[level - 1].is_none() {
                    break;
                }
                self.parents[level] = None;
                self.indents[level] = None;
                level -= 1;
            }
        }
        self.match_list_kind(&item, kind);

        let tree_parent = self.current_parent();
        let explicit = numeric_marker(item.marker);
        let tree_marker = explicit.map_or(String::new(), |_| item.marker.to_string());
        let id = self.add_tree_text(
            tree_parent,
            "list_item",
            item.text.trim(),
            None,
            None,
            Some(ListMeta {
                enumerated: item.numbered,
                marker: tree_marker,
            }),
        );
        self.last_item_tree = Some(id);

        // The flat node takes what docling-core's list serializer reads off
        // the tree: the depth of list groups around the item, and its number
        // — an explicit `12.` printed verbatim, else the item's position in
        // its group when the group's *first* item is enumerated (a bullet
        // `-` otherwise, whatever the item's own marker).
        let (depth, position, group_enumerated) = self.list_position(id);
        let (ordered, number) = match explicit {
            Some(n) => (true, n),
            None => (group_enumerated, position),
        };
        doc.push(Node::ListItem {
            ordered,
            number,
            first_in_list: self.fresh_list,
            text: escape_text(item.text.trim()),
            level: depth,
            marker: explicit.map(|_| item.marker.to_string()),
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        self.last_item = Some(doc.nodes.len() - 1);
        self.fresh_list = false;
        self.list_continuation = false;
    }

    /// docling's `_match_list_kind`: AsciiDoc nests lists by marker, not by
    /// indentation (docling#4822). An item of another kind than its group
    /// returns to the closest enclosing list of its kind at the same indent,
    /// or else opens a nested list under the item above it.
    fn match_list_kind(&mut self, item: &ListItem<'_>, kind: char) {
        let level = self.current_level();
        if self.list_kinds[level] == Some(kind) {
            return;
        }
        let mut outer = level;
        while outer > 0 {
            outer -= 1;
            let Some(outer_kind) = self.list_kinds[outer] else {
                break;
            };
            if self.indents[outer] != Some(item.indent) {
                break;
            }
            if outer_kind == kind {
                for closed in outer + 1..=level {
                    self.parents[closed] = None;
                    self.indents[closed] = None;
                }
                return;
            }
        }
        let parent = self.last_item_tree;
        self.open_list_group(level, parent, item.indent, kind);
    }

    /// How many list groups enclose `group` — its items' Markdown depth.
    fn group_depth(&self, group: usize) -> u8 {
        let mut depth = 0u8;
        let mut at = self.tree.items[group].parent;
        while let Some(a) = at {
            if self.is_list_group(a) {
                depth += 1;
            }
            at = self.tree.items[a].parent;
        }
        depth
    }

    /// For a list item of the tree: how many list groups enclose it below
    /// the outermost (its Markdown depth), its 1-based position among its
    /// group's children, and whether that group's first child is enumerated.
    fn list_position(&self, id: usize) -> (u8, u64, bool) {
        let group = self.tree.items[id].parent;
        let mut depth = 0u8;
        let mut at = group.and_then(|g| self.tree.items[g].parent);
        while let Some(a) = at {
            if self.is_list_group(a) {
                depth += 1;
            }
            at = self.tree.items[a].parent;
        }
        let Some(group) = group else {
            return (depth, 1, false);
        };
        let children = &self.tree.items[group].children;
        let position = children.iter().position(|&c| c == id).map_or(1, |p| p + 1) as u64;
        let first_enumerated = children.first().is_some_and(|&c| {
            matches!(
                &self.tree.items[c].kind,
                TreeKind::Text { list: Some(l), .. } if l.enumerated
            )
        });
        (depth, position, first_enumerated)
    }

    fn push_picture(&mut self, uri: &str, doc: &mut DoclingDocument) {
        let cap = self.take_caption();
        let image = self.images.and_then(|r| r.resolve(uri));
        // Tree: the caption is a body-level item the picture references; the
        // picture hangs off the open list item or the current section.
        let captions: Vec<usize> = if cap.is_some() {
            let raw = self.raw_caption.clone();
            vec![self.add_tree_caption(&raw)]
        } else {
            Vec::new()
        };
        let parent = if self.in_list {
            self.last_item_tree
        } else {
            self.current_parent()
        };
        self.tree.add(
            parent,
            None,
            TreeKind::Picture {
                captions,
                image: image.clone(),
                extra_classes: Vec::new(),
                classification: None,
                description: None,
                confidence: None,
                chart: None,
                dpi: None,
            },
        );
        // Inside a list docling nests the picture under the open item, so it
        // is folded into the item's text (see `fold_child`): the marker lands
        // where docling prints it, at the cost of the separate JSON picture
        // item and of any bytes `fetch_images` resolved. A picture in a list is
        // only reachable through a `+` continuation, where docling never has a
        // caption either.
        if self.in_list {
            let mut block = String::new();
            if let Some(cap) = &cap {
                block.push_str(cap);
                block.push('\n');
            }
            block.push_str("<!-- image -->");
            if self.fold_child(doc, &block) {
                self.list_continuation = false;
                return;
            }
        }
        // docling ≥ 2.127 parents the picture to the current section (it used
        // to go to the body root and render after everything).
        doc.push(Node::Picture {
            caption: cap,
            caption_href: None,
            image,
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
        self.list_continuation = false;
    }

    /// Attach an already-rendered Markdown block to the open list item, the way
    /// the HTML backend folds an `<li>`'s images into the item text: our node
    /// list is flat, so docling's *children of a list item* are carried in the
    /// item's own text, which the Markdown serializer prints unwrapped after
    /// the item line. Each child block is indented to the item's own depth,
    /// matching docling-core's list serializer (which prefixes the nesting
    /// indent to the first line of every part).
    fn fold_child(&mut self, doc: &mut DoclingDocument, block: &str) -> bool {
        let Some(idx) = self.last_item else {
            return false;
        };
        let Some(Node::ListItem { text, level, .. }) = doc.nodes.get_mut(idx) else {
            return false;
        };
        text.push('\n');
        text.push_str(&"    ".repeat(*level as usize));
        text.push_str(block);
        true
    }

    fn end_list(&mut self) {
        self.in_list = false;
        self.last_item = None;
        self.last_item_tree = None;
        self.list_continuation = false;
    }

    /// docling's `_add_comment_block` (#863, docling#4663): a comment is
    /// markup, not content — it neither ends the list nor the paragraph it
    /// sits in. It becomes a `text` item on the notes layer under the open
    /// list item, else under the current section; a comment between two
    /// lines of a paragraph lands before that paragraph, as upstream's does.
    /// The flat nodes carry it as a notes-layer paragraph — exports leave
    /// that layer out by default — but one met inside a list waits for the
    /// list's end there: a node between two list items would split the
    /// list the default Markdown prints.
    fn feed_comment(&mut self, text: String, doc: &mut DoclingDocument) {
        let parent = if self.in_list {
            self.last_item_tree
        } else {
            self.current_parent()
        };
        self.tree.add(
            parent,
            Some(docling_core::ContentLayer::Notes),
            TreeKind::Text {
                label: "text".into(),
                text: text.clone(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: None,
            },
        );
        if self.in_list {
            self.list_notes.push(text);
        } else {
            push_note(doc, &text);
        }
    }

    /// The comments held back while a list was open, now that it is not.
    fn flush_list_notes(&mut self, doc: &mut DoclingDocument) {
        for text in std::mem::take(&mut self.list_notes) {
            push_note(doc, &text);
        }
    }

    /// Take a pending caption for the picture or table that claims it — the
    /// Markdown-escaped text; the raw joined text stays in `raw_caption` for
    /// the tree item.
    fn take_caption(&mut self) -> Option<String> {
        if self.caption_data.is_empty() {
            return None;
        }
        let cap = self.caption_data.join(" ");
        self.caption_data.clear();
        self.raw_caption = cap.clone();
        Some(escape_text(&cap))
    }

    /// docling's `_flush_text_data`: the accumulated lines as one paragraph —
    /// or, when `*bold*`, `_italic_` or `` `mono` `` marks style part of it
    /// (docling#4824), an `inline` group of one item per run: a text with its
    /// bold/italic formatting, a monospace run a `code` item.
    fn flush_text(&mut self, doc: &mut DoclingDocument) {
        if self.text_data.is_empty() {
            return;
        }
        let text = self.text_data.join(" ");
        self.text_data.clear();
        let parent = self.current_parent();
        // Text still pending when a list opened is flushed into the list group
        // (docling's `parent=_get_current_parent(parents)`), which
        // docling-core's list serializer prints as one more line of the list,
        // right after the item before it — an inline group glued onto that
        // line (see `Node::ListText`).
        let list_level = parent
            .filter(|&p| self.is_list_group(p))
            .map(|g| self.group_depth(g));
        let runs = parse_inline(&text, false, false);
        if !runs.iter().any(|r| r.bold || r.italic || r.code) {
            let escaped = escape_text(&text);
            doc.push(match list_level {
                Some(level) => Node::ListText {
                    level,
                    text: escaped,
                    glue: false,
                },
                None => Node::Paragraph { text: escaped },
            });
            self.add_tree_text(parent, "paragraph", &text, None, None, None);
            return;
        }
        let group = self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: "inline".into(),
                name: "group".into(),
            },
        );
        // The DocLang runs keep the stripped shape (ticket B).
        let node_runs: Vec<InlineRun> = runs
            .iter()
            .filter(|r| !r.text.trim().is_empty())
            .map(|r| InlineRun {
                text: r.text.trim().to_string(),
                bold: r.bold,
                italic: r.italic,
                code: r.code,
                ..InlineRun::default()
            })
            .collect();
        // docling#3891: the runs keep their boundary whitespace and only the
        // block's outer edges are trimmed; a code run's edge whitespace moves
        // out into a plain run of its own, never inside the backticks.
        let mut spaced: Vec<(String, bool, bool, bool)> = Vec::new();
        for run in &runs {
            let (lead, core, trail) = docling_core::inline::split_edges(&run.text);
            if !run.code || core.is_empty() {
                spaced.push((run.text.clone(), run.bold, run.italic, false));
                continue;
            }
            for (part, code) in [(lead, false), (core, true), (trail, false)] {
                if !part.is_empty() {
                    let (b, i) = if code {
                        (run.bold, run.italic)
                    } else {
                        (false, false)
                    };
                    spaced.push((part.to_string(), b, i, code));
                }
            }
        }
        let spaced = docling_core::inline::trim_run_edges(
            spaced,
            |r| r.0.as_str(),
            |r, t| r.0 = t,
            |_| true,
        );
        let mut md_text = String::new();
        for (t, bold, italic, code) in spaced {
            if code {
                self.tree.add(
                    Some(group),
                    None,
                    TreeKind::Code {
                        text: t.clone(),
                        orig: None,
                        language: None,
                        formatting: None,
                        hyperlink: None,
                        captions: Vec::new(),
                    },
                );
                md_text.push_str(&docling_core::code_span(&t));
            } else {
                let formatting = (bold || italic).then(|| Formatting {
                    bold,
                    italic,
                    ..Formatting::default()
                });
                self.add_tree_text(Some(group), "text", &t, formatting, None, None);
                md_text.push_str(&docling_core::inline::hoist(&t, |core| {
                    let escaped = escape_text(core);
                    match (bold, italic) {
                        (true, true) => format!("***{escaped}***"),
                        (true, false) => format!("**{escaped}**"),
                        (false, true) => format!("*{escaped}*"),
                        (false, false) => escaped,
                    }
                }));
            }
        }
        // Under a heading the group is the heading's child, which DocLang
        // writes without an enclosing `<text>`.
        if let Some(level) = list_level {
            doc.push(Node::ListText {
                level,
                text: md_text,
                glue: true,
            });
            return;
        }
        let unwrapped =
            parent.is_some_and(|p| matches!(self.tree.items[p].kind, TreeKind::Text { .. }));
        doc.push(Node::InlineGroup {
            unwrapped,
            runs: node_runs,
            md_text,
        });
    }

    fn flush_table(&mut self, doc: &mut DoclingDocument) {
        if !self.table_data.is_empty() {
            // A pending caption is attached to this table and renders before it.
            let mut captions = Vec::new();
            if let Some(cap) = self.take_caption() {
                doc.push(Node::Paragraph { text: cap });
                let raw = self.raw_caption.clone();
                captions.push(self.add_tree_caption(&raw));
            }
            let num_cols = self.table_data.iter().map(Vec::len).max().unwrap_or(0);
            let raw_rows: Vec<Vec<String>> = self.table_data.clone();
            let rows: Vec<Vec<String>> = self
                .table_data
                .drain(..)
                .map(|mut r| {
                    r.resize(num_cols, String::new());
                    r
                })
                .collect();
            let table = Table {
                rows,
                location: None,
                structure: None,
                cell_blocks: None,
                cells: None,
                caption: None,
                caption_parent: Default::default(),
                caption_location: None,
            };
            // docling's `_populate_table_as_grid` writes a cell only where
            // the source row has one — a short row leaves its tail empty in
            // the grid (no cell, no header flag) — so the tree's table carries
            // the ragged cells while the flat rows stay padded for Markdown.
            let cells: Vec<TableCell> = raw_rows
                .iter()
                .enumerate()
                .flat_map(|(r, row)| {
                    row.iter().enumerate().map(move |(c, text)| TableCell {
                        text: text.clone(),
                        bbox: None,
                        start_row: r,
                        start_col: c,
                        row_span: 1,
                        col_span: 1,
                        column_header: r == 0,
                        row_header: false,
                        row_section: false,
                    })
                })
                .collect();
            let parent = self.current_parent();
            self.tree.add(
                parent,
                None,
                TreeKind::Table {
                    table: Table {
                        cells: Some(cells),
                        ..table.clone()
                    },
                    rich_cells: Vec::new(),
                    captions,
                },
            );
            doc.push(Node::Table(table));
        }
        self.in_table = false;
        self.in_delimited_table = false;
        self.table_data.clear();
    }

    fn finish(&mut self, doc: &mut DoclingDocument) {
        self.flush_text(doc);
        self.flush_list_notes(doc);
        if self.in_table {
            // An unclosed table still consumes its pending caption.
            self.flush_table(doc);
        } else {
            // A block title never followed by a floating element still
            // reaches the document.
            self.flush_caption_as_paragraph(doc);
        }
    }
}

/// A comment as a flat node: a notes-layer paragraph.
fn push_note(doc: &mut DoclingDocument, text: &str) {
    doc.push(Node::Furniture {
        layer: docling_core::ContentLayer::Notes,
        inner: Box::new(Node::Paragraph {
            text: escape_text(text),
        }),
    });
}

/// One run of a paragraph's text — docling's `_InlineRun`.
struct Run {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,
}

/// Python's `\w` for one character.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// One alternative of docling's `_INLINE_FORMAT_RE` tried at `at`: the end of
/// the match and the inner text's range, or `None`. Doubled marks
/// (unconstrained) take the shortest non-empty run up to the closing pair;
/// a single mark (constrained) needs no word character before it, a
/// non-space right inside each end, and no word character after it.
fn match_mark(
    chars: &[char],
    at: usize,
    mark: char,
    doubled: bool,
) -> Option<(usize, std::ops::Range<usize>)> {
    if doubled {
        if chars.get(at) != Some(&mark) || chars.get(at + 1) != Some(&mark) {
            return None;
        }
        let start = at + 2;
        // `.+?`: at least one character, then the closing pair.
        (start + 1..chars.len().saturating_sub(1))
            .find(|&e| chars[e] == mark && chars[e + 1] == mark)
            .map(|e| (e + 2, start..e))
    } else {
        if chars.get(at) != Some(&mark) || (at > 0 && is_word(chars[at - 1])) {
            return None;
        }
        let start = at + 1;
        if chars.get(start).is_none_or(|c| c.is_whitespace()) {
            return None;
        }
        (start + 1..chars.len())
            .find(|&e| {
                chars[e] == mark
                    && !chars[e - 1].is_whitespace()
                    && chars.get(e + 1).is_none_or(|&c| !is_word(c))
            })
            .map(|e| (e + 1, start..e))
    }
}

/// docling's `_parse_inline`: split text into plain, bold, italic and
/// monospace runs by `_INLINE_FORMAT_RE`, unconstrained marks (` `` `,
/// `**`, `__`) before constrained ones (`` ` ``, `*`, `_`) at each position,
/// so `snake_case_name` and `2*3*4` stay plain. A bold or italic run's inner
/// text is parsed again with that style on (`*_both_*`); a monospace run is
/// literal.
fn parse_inline(text: &str, bold: bool, italic: bool) -> Vec<Run> {
    let chars: Vec<char> = text.chars().collect();
    let mut runs = Vec::new();
    let plain = |from: usize, to: usize| Run {
        text: chars[from..to].iter().collect(),
        bold,
        italic,
        code: false,
    };
    let mut position = 0;
    let mut at = 0;
    while at < chars.len() {
        // (mark, doubled, kind): 0 = mono, 1 = bold, 2 = italic.
        let found = [
            ('`', true, 0),
            ('*', true, 1),
            ('_', true, 2),
            ('`', false, 0),
            ('*', false, 1),
            ('_', false, 2),
        ]
        .into_iter()
        .find_map(|(mark, doubled, kind)| {
            match_mark(&chars, at, mark, doubled).map(|(end, inner)| (end, inner, kind))
        });
        let Some((end, inner, kind)) = found else {
            at += 1;
            continue;
        };
        if at > position {
            runs.push(plain(position, at));
        }
        let inner_text: String = chars[inner].iter().collect();
        match kind {
            0 => runs.push(Run {
                text: inner_text,
                bold: false,
                italic: false,
                code: true,
            }),
            1 => runs.extend(parse_inline(&inner_text, true, italic)),
            _ => runs.extend(parse_inline(&inner_text, bold, true)),
        }
        position = end;
        at = end;
    }
    if position < chars.len() {
        runs.push(plain(position, chars.len()));
    }
    runs
}

/// `= Title` → the title text, trimmed, its closing `=` dropped.
fn is_title(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("= ")?.trim();
    Some(strip_closing_marker(rest, "="))
}

/// `== Section` → (number-of-`=`, text trimmed, closing marks dropped). A
/// bare `=` (title) is excluded.
fn section_header(line: &str) -> Option<(u8, String)> {
    if !line.starts_with("==") {
        return None;
    }
    let caps = cached_regex!(r"^(=+)\s+(.*)").captures(line)?;
    let marker = caps.get(1)?.as_str();
    let text = strip_closing_marker(caps.get(2)?.as_str().trim(), marker);
    Some((marker.len() as u8, text.to_string()))
}

/// docling's `_strip_closing_marker` (docling#4938): Asciidoctor accepts a
/// symmetric title, closed by the same marks that open it after whitespace
/// (`== Intro ==`), and leaves them out of the text; marks of another
/// length are part of the title (`=== Details ==`).
fn strip_closing_marker<'a>(text: &'a str, marker: &str) -> &'a str {
    match text.strip_suffix(marker) {
        Some(head) if head.ends_with([' ', '\t']) => head.trim_end_matches([' ', '\t']),
        _ => text,
    }
}

/// A parsed list item, docling's `_parse_list_item`.
struct ListItem<'a> {
    /// docling's `indent`: the leading whitespace, plus one per extra `.` or
    /// `*` — such a marker encodes its depth in itself (`..` nests under `.`),
    /// not in the indentation.
    indent: usize,
    marker: &'a str,
    numbered: bool,
    text: &'a str,
}

impl ListItem<'_> {
    /// docling's `_list_kind`: every ordered marker is one kind (`.`); a
    /// bullet's kind is its marker character, so `*` and `-` lists nest.
    fn kind(&self) -> char {
        if self.numbered {
            '.'
        } else {
            self.marker.chars().next().unwrap_or('-')
        }
    }
}

fn list_item(line: &str) -> Option<ListItem<'_>> {
    let caps = cached_regex!(LIST_ITEM).captures(line)?;
    let marker = caps.get(2)?.as_str();
    let mut indent = caps.get(1)?.as_str().len();
    // A run of `.`s or `*`s nests by its length (docling#4403).
    if marker.starts_with(['.', '*']) {
        indent += marker.len() - 1;
    }
    Some(ListItem {
        indent,
        marker,
        numbered: !(marker.starts_with('*') || marker == "-"),
        text: caps.get(3)?.as_str(),
    })
}

/// The number in an explicit `12.` marker — docling's `marker[:-1].isdigit()`
/// test for the marker it hands to docling-core (which then prints it verbatim
/// instead of numbering the item by position).
fn numeric_marker(marker: &str) -> Option<u64> {
    let digits = marker.strip_suffix('.')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// docling's `_is_table_line`: a cell delimiter right after an optional cell
/// specifier — a cell may be the only one on its line (docling#4431).
fn is_table_line(line: &str) -> bool {
    cached_regex!(&format!(r"^{CELL_SPEC}\|")).is_match(line)
}

/// docling's `_parse_table_line`: strip cell specifiers glued before a `|`,
/// split on every `|` not escaped as `\|` (which is a pipe in the cell text,
/// docling#4378), drop the leading empty field, and trim.
fn parse_table_line(line: &str) -> Vec<String> {
    let cleaned = cached_regex!(&format!(r"(^|\s){CELL_SPEC}(\|)")).replace_all(line, "$1$2");
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = cleaned.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell)),
            _ => cell.push(c),
        }
    }
    cells.push(cell);
    cells
        .into_iter()
        .skip(1)
        .map(|c| c.trim().to_string())
        .collect()
}

fn is_picture(line: &str) -> bool {
    line.starts_with("image::")
}

/// The `image::target[attrs]` target. docling falls back to the whole line when
/// the macro is malformed (an unresolvable "uri", which then fetches nothing).
fn picture_uri(line: &str) -> Option<String> {
    if !is_picture(line) {
        return None;
    }
    Some(
        cached_regex!(r"^image::(.+)\[(.*)\]$")
            .captures(line)
            .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
            .unwrap_or_else(|| line.to_string()),
    )
}

fn is_caption(line: &str) -> Option<&str> {
    // `.text`, but not `. text` (an ordered list item) and not a bare `.`.
    let rest = line.strip_prefix('.')?;
    (!rest.starts_with(char::is_whitespace) && !rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::images::NoFetch;

    fn md(src: &str) -> String {
        parse(src, "t", &NoFetch).export_to_markdown()
    }

    fn json(src: &str) -> serde_json::Value {
        serde_json::from_str(&parse(src, "t", &NoFetch).export_to_json()).unwrap()
    }

    /// docling's `_parse` tree (verified item for item against docling
    /// 2.129 on this input): the title is the root, a heading hangs off the
    /// nearest present ancestor level, a list opens a `list` group whose
    /// deeper indents nest further groups, items are the innermost group's
    /// children, a block title before a list is a bold paragraph, and a
    /// table's block title is a body-level `caption` the table references.
    #[test]
    fn tree_mirrors_docling_parents_and_indents() {
        let v = json(
            "= Title\n\nAbstract.\n\n== Section 1\n\n* a\n  * b\n* c\n\n\
                      === Deep\n\n.Steps\n. one\n. two\n\n.Table title\n|===\n|H|I|\n|1|2|\n|===\n",
        );
        let texts = v["texts"].as_array().unwrap();
        let by = |i: usize| {
            (
                texts[i]["label"].as_str().unwrap(),
                texts[i]["parent"]["$ref"].as_str().unwrap(),
            )
        };
        assert_eq!(by(0), ("title", "#/body"));
        assert_eq!(by(1), ("paragraph", "#/texts/0"));
        assert_eq!(by(2), ("section_header", "#/texts/0"));
        assert_eq!(texts[2]["level"], 1);
        // `* a` opens groups/0 under the section; `  * b` nests groups/1
        // under the item `a` (docling#4721); `* c` dedents back to groups/0.
        assert_eq!(by(3), ("list_item", "#/groups/0"));
        assert_eq!(by(4), ("list_item", "#/groups/1"));
        assert_eq!(by(5), ("list_item", "#/groups/0"));
        assert_eq!(v["groups"][0]["parent"]["$ref"], "#/texts/2");
        assert_eq!(v["groups"][1]["parent"]["$ref"], "#/texts/3");
        assert_eq!(texts[3]["marker"], "");
        assert_eq!(texts[3]["enumerated"], false);
        // `===` under `==`: level 2, parented to the level-1 heading.
        assert_eq!(by(6), ("section_header", "#/texts/2"));
        assert_eq!(texts[6]["level"], 2);
        // `.Steps` before the dotted list: a bold paragraph under `Deep`; the
        // list group follows it there, its items enumerated with no marker.
        assert_eq!(by(7), ("paragraph", "#/texts/6"));
        assert_eq!(texts[7]["formatting"]["bold"], true);
        assert_eq!(v["groups"][2]["parent"]["$ref"], "#/texts/6");
        assert_eq!(by(8), ("list_item", "#/groups/2"));
        assert_eq!(texts[8]["enumerated"], true);
        assert_eq!(texts[8]["marker"], "");
        // The table's block title: a caption on the *body*, referenced.
        assert_eq!(by(10), ("caption", "#/body"));
        assert_eq!(v["tables"][0]["parent"]["$ref"], "#/texts/6");
        assert_eq!(v["tables"][0]["captions"][0]["$ref"], "#/texts/10");
        assert_eq!(
            v["tables"][0]["data"]["table_cells"][0]["column_header"],
            true
        );
    }

    /// A `+`-continued picture is a child of the open list item (docling's
    /// `parent=last_list_item`), and a short table row leaves its tail
    /// without cells (`_populate_table_as_grid` writes only what the row has).
    #[test]
    fn tree_nests_continued_pictures_and_keeps_ragged_rows() {
        let v = json("= T\n\n. step\n+\nimage::a.png[]\n\n|A|B|C|\n|1|\n");
        assert_eq!(v["pictures"][0]["parent"]["$ref"], "#/texts/1");
        assert_eq!(v["texts"][1]["children"][0]["$ref"], "#/pictures/0");
        // `line.split("|")[1:]` keeps the trailing empty field, so the rows
        // are 4 and 2 cells wide; the short row is not padded to 4.
        let cells = v["tables"][0]["data"]["table_cells"].as_array().unwrap();
        assert_eq!(cells.len(), 6, "4 header cells + 2, no padding");
        assert_eq!(v["tables"][0]["data"]["num_cols"], 4);
        assert_eq!(
            cells
                .iter()
                .filter(|c| c["start_row_offset_idx"] == 1)
                .count(),
            2
        );
    }

    #[test]
    fn literal_block_becomes_a_code_item() {
        assert_eq!(
            md("= T\n\npara\n\n....\nraw literal\n  indented\n....\n\nafter\n"),
            "# T\n\npara\n\n```\nraw literal\n  indented\n```\n\nafter\n"
        );
        // An unterminated block still yields its body (docling's `_iter_blocks`
        // flushes what it has at EOF) — with the empty line Python's
        // `split("\n")` leaves after the file's last newline.
        assert_eq!(md("....\nno close\n"), "```\nno close\n\n```\n");
        assert_eq!(md("....\nno close"), "```\nno close\n```\n");
        // A block title in front of a literal block is the code item's
        // caption, which docling-core renders after the block (2.127+).
        assert_eq!(
            md(".Literal example\n....\nraw\n....\n"),
            "```\nraw\n```\n\nLiteral example\n"
        );
        assert_eq!(
            md("= T\n\npara\n\n.Cap here\n....\nraw\n....\n\nafter\n"),
            "# T\n\npara\n\n```\nraw\n```\n\nCap here\n\nafter\n"
        );
    }

    #[test]
    fn plus_keeps_a_literal_block_and_an_image_inside_the_item() {
        // docling nests both under the open list item, so they render between
        // the item lines with no blank line and the numbering runs on.
        assert_eq!(
            md(". one\n+\n....\ncode\n....\n. two\n+\nimage::a.png[]\n. three\n"),
            "1. one\n```\ncode\n```\n2. two\n<!-- image -->\n3. three\n"
        );
        // Without the `+` the block closes the list instead.
        assert_eq!(
            md("* one\n\n....\ncode\n....\n"),
            "- one\n\n```\ncode\n```\n"
        );
    }

    #[test]
    fn a_child_block_is_indented_to_its_item_depth() {
        // docling-core prefixes the list indent to the first line of every part
        // it emits, the child blocks included.
        assert_eq!(
            md(". outer\n.. inner\n+\n....\ndeep\n....\n.. inner two\n+\nimage::a.png[]\n"),
            "1. outer\n    1. inner\n    ```\ndeep\n```\n    2. inner two\n    <!-- image -->\n"
        );
    }

    #[test]
    fn dotted_and_lettered_markers_open_ordered_items() {
        // docling#4118 widened the marker set; a dotted marker carries its own
        // depth (`..` nests under `.`), and the nested group hangs off the
        // item above it (docling#4721) — it no longer takes a position in the
        // outer group, so the numbering runs on without a gap.
        assert_eq!(
            md(". one\n. two\n.. nested\n. three\na. lettered\n"),
            "1. one\n2. two\n    1. nested\n3. three\n4. lettered\n"
        );
    }

    #[test]
    fn an_explicit_numeric_marker_is_printed_verbatim() {
        // docling hands docling-core the source marker for `\d+.` items only,
        // and it prints those instead of numbering them by position.
        assert_eq!(
            md("1. one\n2. two\n.. sub\n"),
            "1. one\n2. two\n    1. sub\n"
        );
        // An item of another marker kind opens a list nested under the item
        // above it, as Asciidoctor nests by marker (docling#4822).
        assert_eq!(md("* bullet\n. dotted\n"), "- bullet\n    1. dotted\n");
    }

    #[test]
    fn a_broken_run_of_explicit_markers_stays_one_list() {
        // #385: the list is the group this backend tracked, whatever the
        // explicit markers say — docling prints them verbatim in one list. The
        // Markdown serializer used to read the jump as a new list and insert a
        // blank line (a deviation documented until the boundary guesses went).
        assert_eq!(md("1. one\n5. five\n"), "1. one\n5. five\n");
        // An ordered item between bullets nests under the bullet above it;
        // the next bullet returns to the outer list (docling#4822).
        assert_eq!(
            md("* bullet one\n1. explicit one\n* bullet two\n"),
            "- bullet one\n    1. explicit one\n- bullet two\n"
        );
    }

    /// docling ≥ 2.127: a block title in front of a list is a bold paragraph
    /// (a list group has no caption slot); in front of a literal block it is
    /// still the code item's caption text.
    #[test]
    fn a_block_title_in_front_of_a_list_is_a_bold_paragraph() {
        assert_eq!(
            md(".Procedure\n\n. one\n\n.Verification\n\n* check\n"),
            "**Procedure**\n\n1. one\n\n**Verification**\n\n- check\n"
        );
    }

    /// docling ≥ 2.127 parents a level-skipping heading to its nearest present
    /// ancestor and a picture to the current section, so both stay in reading
    /// order (2.126 hung them off the body root, after the whole tree).
    #[test]
    fn skipped_levels_and_pictures_keep_reading_order() {
        assert_eq!(
            md("= T\n\n== A\n\n==== Deep\n\ntext\n\n== B\n\n.Cap\nimage::x.png[]\n\nafter\n"),
            "# T\n\n## A\n\n#### Deep\n\ntext\n\n## B\n\nCap\n\n<!-- image -->\n\nafter\n"
        );
    }

    /// docling#4290 (2.129): a rowspan-only cell specifier (`.2+|`) is a
    /// specifier, not cell text; a `2.3+|` span and a bare `|` still read.
    #[test]
    fn rowspan_only_cell_specifiers_are_stripped() {
        assert_eq!(parse_table_line(".2+|A |B"), vec!["A", "B"]);
        assert_eq!(parse_table_line("2.3+|D ^.^h|E"), vec!["D", "E"]);
        assert!(is_table_line(".2+|A |B"));
        assert_eq!(
            md("|===\n.2+|A |B\n|===\n"),
            "| A   | B   |\n|-----|-----|\n"
        );
    }

    #[test]
    fn a_non_list_line_ends_the_list_without_being_swallowed() {
        // docling ≤ 2.124 dropped this line; docling#4118 parses it (a blank
        // line or a `+` leaves the list open instead).
        assert_eq!(
            md("= T\n\n* one\n* two\n\n== Head\n\npara\n"),
            "# T\n\n- one\n- two\n\n## Head\n\npara\n"
        );
    }

    /// docling#4428: the text before a heading is its own paragraph, under
    /// the section it was written in — not joined to the text after the
    /// heading and moved to the new section.
    #[test]
    fn a_heading_flushes_the_pending_paragraph_into_its_own_section() {
        let doc = parse(
            "= T\n\n== S1\n\n=== S1.1\n\nbody\n== S2\n\nbody2\n",
            "t",
            &NoFetch,
        );
        assert_eq!(
            doc.export_to_markdown().trim(),
            "# T\n\n## S1\n\n### S1.1\n\nbody\n\n## S2\n\nbody2"
        );
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let texts = json["texts"].as_array().unwrap();
        let body = texts.iter().find(|t| t["text"] == "body").unwrap();
        let s11 = texts.iter().find(|t| t["text"] == "S1.1").unwrap();
        assert_eq!(body["parent"]["$ref"], s11["self_ref"]);
        let body2 = texts.iter().find(|t| t["text"] == "body2").unwrap();
        let s2 = texts.iter().find(|t| t["text"] == "S2").unwrap();
        assert_eq!(body2["parent"]["$ref"], s2["self_ref"]);
    }

    #[test]
    fn a_lone_word_and_period_stays_a_paragraph() {
        // A marker needs whitespace after it, so a bare "Intro." is text.
        assert_eq!(md("= T\n\nIntro.\n"), "# T\n\nIntro.\n");
        // A line starting with an abbreviation is text: a lettered marker has
        // one letter only (docling#4827).
        assert_eq!(md("Fig. 1 shows the result\n"), "Fig. 1 shows the result\n");
        assert_eq!(md("b. option\n"), "1. option\n");
    }

    /// docling's `_parse_inline` (docling#4824): doubled marks anywhere,
    /// single ones at word boundaries only; bold and italic nest, monospace
    /// is literal.
    #[test]
    fn inline_marks_split_runs() {
        let runs = |t: &str| -> Vec<(String, u8)> {
            parse_inline(t, false, false)
                .into_iter()
                .map(|r| {
                    let k = u8::from(r.bold) | u8::from(r.italic) << 1 | u8::from(r.code) << 2;
                    (r.text, k)
                })
                .collect()
        };
        assert_eq!(
            runs("a *b* _c_ `d` **e**f"),
            [
                ("a ".into(), 0),
                ("b".into(), 1),
                (" ".into(), 0),
                ("c".into(), 2),
                (" ".into(), 0),
                ("d".into(), 4),
                (" ".into(), 0),
                ("e".into(), 1),
                ("f".into(), 0),
            ]
        );
        assert_eq!(runs("*_both_*"), [("both".into(), 3)]);
        // No mark at a word boundary: plain text.
        assert_eq!(
            runs("snake_case_name 2*3*4 a* b*"),
            [("snake_case_name 2*3*4 a* b*".into(), 0)]
        );
        // Under a heading the paragraph is an inline group of formatted items.
        assert_eq!(
            md("= T\n\nSome *bold* and `x`.\n"),
            "# T\n\nSome **bold** and `x`.\n"
        );
    }

    /// docling#4761 / #4378 / #4662: a `|===` table stays one table across a
    /// blank line, a wrapped line continues its cell, `\|` is a pipe in the
    /// cell, and a delimiter with no closer only breaks the paragraph.
    #[test]
    fn tables_and_stray_delimiters_follow_docling() {
        assert_eq!(
            md("|===\n|A |B\n\n|a\\|b |c\nwrapped\n|===\n"),
            "| A        | B         |\n|----------|-----------|\n| a&#124;b | c wrapped |\n"
        );
        assert_eq!(md("before\n====\nafter\n"), "before\n\nafter\n");
        // Comments and attribute lines are no content.
        assert_eq!(
            md("// note\n[NOTE]\n////\nhidden\n////\n:attr: x\ntext\n"),
            "text\n"
        );
    }

    /// docling#4938: a symmetric title drops its closing marks; marks of
    /// another length are part of the title, as in Asciidoctor.
    #[test]
    fn symmetric_titles_drop_their_closing_marks() {
        let v = json("= Doc =\n\n== Intro ==\n\ntext\n\n=== Details ==\n");
        let headings: Vec<(&str, &str)> = v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["label"] == "title" || t["label"] == "section_header")
            .map(|t| (t["label"].as_str().unwrap(), t["text"].as_str().unwrap()))
            .collect();
        assert_eq!(
            headings,
            [
                ("title", "Doc"),
                ("section_header", "Intro"),
                ("section_header", "Details ==")
            ]
        );
        assert!(md("== Intro ==\n").starts_with("## Intro\n"));
    }

    /// The texts of the notes-layer items (the comments).
    fn notes(src: &str) -> Vec<String> {
        json(src)["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["content_layer"] == "notes")
            .map(|t| t["text"].as_str().unwrap().to_string())
            .collect()
    }

    /// docling#4394: only exactly two slashes start a line comment, and a
    /// comment block opened by four or more slashes closes only on an
    /// identical line.
    #[test]
    fn comment_delimiters_follow_asciidoctor() {
        let src = "/// not a comment\n//// nor this\n\n// a note\n";
        assert_eq!(md(src), "/// not a comment //// nor this\n");
        assert_eq!(notes(src), ["a note"]);
        for delimiter in ["/////", "///////"] {
            let src = format!(
                "before\n\n{delimiter}\nhidden\n////\nstill hidden\n{delimiter}\n\nafter\n"
            );
            assert_eq!(md(&src), "before\n\nafter\n");
            assert_eq!(notes(&src), ["hidden\n////\nstill hidden"]);
        }
    }

    /// docling's `elif in_table` arm consumes the line that ends a bare
    /// table; a paragraph pending when a list opens is flushed into the list
    /// group, one more line of the list (an inline group glued to the item).
    #[test]
    fn table_ends_and_pending_paragraphs_follow_docling() {
        assert_eq!(
            md("|a|b\n|c|d\ngone\nkept\n"),
            "| a   | b   |\n|-----|-----|\n| c   | d   |\n\nkept\n"
        );
        assert_eq!(md("para\n* a\n\n* b\n"), "- a\npara\n- b\n");
        assert_eq!(md("some *x*\n. one\n"), "1. onesome **x**\n");
        let v = json("para\n* a\n");
        assert_eq!(v["texts"][1]["label"], "paragraph");
        assert_eq!(v["texts"][1]["parent"]["$ref"], "#/groups/0");
    }

    #[test]
    fn an_image_carries_no_imageref_until_images_are_fetched() {
        let png = {
            let img = image::RgbImage::from_pixel(2, 3, image::Rgb([1, 2, 3]));
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            buf.into_inner()
        };
        let dir = std::env::temp_dir().join(format!("docling-adoc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), &png).unwrap();
        let src = dir.join("doc.asciidoc");
        std::fs::write(&src, "= T\n\nimage::a.png[Alt]\n").unwrap();

        let picture = |doc: DoclingDocument| {
            doc.nodes
                .into_iter()
                .find_map(|n| match n {
                    Node::Picture { image, .. } => Some(image),
                    _ => None,
                })
                .expect("a picture")
        };
        let source = SourceDocument::from_file(&src).unwrap();
        // docling#4156: off by default the picture carries no image at all (it
        // used to get a fabricated `file://…` ImageRef).
        assert!(picture(AsciiDocBackend::default().convert(&source).unwrap()).is_none());
        let fetched = picture(
            AsciiDocBackend {
                images: ImagePolicy::new(ImageSources::Local),
            }
            .convert(&source)
            .unwrap(),
        )
        .expect("the local image is read");
        assert_eq!((fetched.width, fetched.height), (2, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
