//! AsciiDoc backend.
//!
//! A line-oriented port of docling's `AsciiDocBackend._parse`: titles (`= `),
//! section headers (`== `…), bullet/numbered lists (with `+` continuation and
//! literal-block/image children), `....`-delimited literal blocks, bare and
//! `|===`-delimited tables (with cell-format-specifier stripping), images and
//! captions, and multi-line paragraphs.

use docling_core::{DoclingDocument, Node, Table};

use crate::backend::images::{FsImageResolver, ImageResolver, NoFetch};
use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

/// AsciiDoc cell specifier, e.g. `2*`, `^`, `.^`, `h` — the run that can be
/// glued before a `|` in a table line. Mirrors docling's `_CELL_SPEC`.
const CELL_SPEC: &str = r"(?:\d+(?:\.\d+)?[*+])*[<^>]?(?:\.[<^>])?[adehlms]?";

/// docling's `_LIST_ITEM_PATTERN` (docling#4118): besides `*`, `-` and `1.`,
/// AsciiDoc's dotted (`.`, `..`, `...`) and lettered/roman (`a.`, `i.`) ordered
/// markers open list items.
const LIST_ITEM: &str = r"^(\s*)(\*|-|\.+|\d+\.|\w+\.)\s+(.*)";

/// The delimiter opening and closing a literal block.
const LITERAL_FENCE: &str = "....";

#[derive(Default)]
pub struct AsciiDocBackend {
    /// When set, an `image::target[]` target is resolved to the actual image
    /// bytes — docling's `AsciiDocBackendOptions.fetch_images` together with
    /// its `enable_local_fetch`/`enable_remote_fetch` (docling#4156). Off by
    /// default, matching docling: a picture is then emitted with no image at
    /// all (until 2.126 docling fabricated a `file://…` `ImageRef` here).
    pub fetch_images: bool,
}

impl DeclarativeBackend for AsciiDocBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        if self.fetch_images {
            let resolver = FsImageResolver::new(
                source.base_dir().map(|p| p.to_path_buf()),
                source.base_url.clone(),
            );
            Ok(parse(text, &source.name, &resolver))
        } else {
            Ok(parse(text, &source.name, &NoFetch))
        }
    }
}

fn parse(text: &str, name: &str, images: &dyn ImageResolver) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    let mut p = Parser {
        images: Some(images),
        ..Parser::default()
    };
    // Iterate raw lines, preserving them as docling does (it never strips the
    // trailing newline-only state machine differently from `str::lines`).
    for block in blocks(text) {
        match block {
            Block::Line(line) => p.feed(line, &mut doc),
            Block::Literal(code) => p.feed_literal(code, &mut doc),
        }
    }
    p.finish(&mut doc);
    doc
}

/// One unit of input: a raw line, or the body of a `....` literal block.
enum Block<'a> {
    Line(&'a str),
    Literal(String),
}

/// docling's `_iter_blocks`: fold each `....`-delimited run of lines into a
/// single literal block. An unterminated block still yields its body at EOF.
fn blocks(text: &str) -> Vec<Block<'_>> {
    let mut out = Vec::new();
    let mut literal: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim() == LITERAL_FENCE {
            match literal.take() {
                None => literal = Some(Vec::new()),
                Some(body) => out.push(Block::Literal(body.join("\n"))),
            }
            continue;
        }
        match &mut literal {
            Some(body) => body.push(line),
            None => out.push(Block::Line(line)),
        }
    }
    if let Some(body) = literal {
        out.push(Block::Literal(body.join("\n")));
    }
    out
}

/// One open list level — docling's `parents`/`indents` pair for a `ListGroup`.
struct ListLevel {
    /// The indent width of the items that opened this level.
    indent: usize,
    /// How many children the group holds so far. docling-core numbers an
    /// enumerated item by its *position among the group's children*, and a
    /// nested list is a child of the group (not of the preceding item), so it
    /// takes a slot too: `. a` / `.. b` / `. c` renders `1.`, `1.`, `3.`.
    slots: u64,
    /// docling-core renders a group's items as numbers only when the group's
    /// *first* item is enumerated; otherwise every item gets a `-`, whatever
    /// its own marker was.
    enumerated: bool,
}

#[derive(Default)]
struct Parser<'r> {
    text_data: Vec<String>,
    caption_data: Vec<String>,
    table_data: Vec<Vec<String>>,
    in_list: bool,
    in_table: bool,
    /// The currently-open list levels, outermost first.
    levels: Vec<ListLevel>,
    /// Whether the next emitted item starts a fresh list (for the serializer).
    fresh_list: bool,
    /// Index in `doc.nodes` of the most recent list item, so a continuation
    /// block can be attached to it as docling attaches a child.
    last_item: Option<usize>,
    /// Set by a lone `+` inside a list: the next literal block or image stays
    /// inside the open item instead of closing the list.
    list_continuation: bool,
    /// Which heading levels (docling 0-based) currently have an active ancestor.
    active: Vec<bool>,
    /// Images and orphan (level-skipping) headings: docling attaches these to the
    /// body root with no parent, so they render after the whole title subtree.
    deferred: Vec<Node>,
    images: Option<&'r dyn ImageResolver>,
}

/// What is being fed to [`Parser::close_list_if_needed`] — docling passes a
/// `<literal-block>` sentinel line for the block case, which matches none of
/// its line predicates but is recognised as a continuation block.
enum Trigger<'a> {
    Line(&'a str),
    Literal,
}

impl Parser<'_> {
    /// docling's `_close_list_if_needed`: anything that is not a list item, a
    /// blank line, a `+`, or a continuation block claimed by a preceding `+`
    /// ends the open list. Unlike docling ≤ 2.124 the line is *not* swallowed —
    /// it goes on to be parsed as a heading/table/picture/text.
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
            self.end_list();
        }
    }

    /// A `....` literal block: docling's `add_code`, nested under the open list
    /// item when a `+` claimed it.
    fn feed_literal(&mut self, code: String, doc: &mut DoclingDocument) {
        self.close_list_if_needed(Trigger::Literal);
        self.flush_text(doc);
        self.flush_caption(doc);
        if !(self.in_list && self.fold_child(doc, &format!("```\n{code}\n```"))) {
            doc.push(Node::Code {
                language: None,
                text: code,
                orig: None,
                pretty: None,
            });
        }
        self.list_continuation = false;
    }

    fn feed(&mut self, line: &str, doc: &mut DoclingDocument) {
        self.close_list_if_needed(Trigger::Line(line));

        // Title: `= ` — the root of the section tree.
        if let Some(rest) = is_title(line) {
            doc.push(Node::Heading {
                level: 1,
                text: escape_text(rest.trim()),
            });
            self.set_active(0);
            return;
        }

        // Section header: `==+ `
        if let Some((n, text)) = section_header(line) {
            let node = Node::Heading {
                level: n.min(6),
                text: escape_text(text.trim()),
            };
            // A heading whose immediate parent level is absent (e.g. `====`
            // under `==`) is orphaned to the body root and deferred.
            let docling_level = (n - 1) as usize;
            if docling_level >= 1 && !self.is_active(docling_level - 1) {
                self.deferred.push(node);
            } else {
                doc.push(node);
                self.set_active(docling_level);
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
        if self.in_list && line.trim() == "+" {
            self.list_continuation = true;
            return;
        }

        // Table start delimiter `|===`
        if line.trim() == "|===" && !self.in_table {
            self.in_table = true;
            return;
        }
        // A table row
        if is_table_line(line) {
            self.in_table = true;
            self.table_data.push(parse_table_line(line));
            return;
        }
        // End of a table (any non-row line, including the closing `|===`)
        if self.in_table {
            self.flush_table(doc);
            // fall through: the line may still be text/caption/etc., except `|===`.
            if line.trim() == "|===" {
                return;
            }
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
        if !line.trim().is_empty() && !self.caption_data.is_empty() {
            self.caption_data.push(line.trim().to_string());
            return;
        }

        // Plain text: blank line flushes the accumulated paragraph
        if line.trim().is_empty() {
            self.flush_text(doc);
        } else {
            self.text_data.push(line.trim().to_string());
        }
    }

    fn push_list_item(&mut self, item: ListItem<'_>, doc: &mut DoclingDocument) {
        if !self.in_list {
            self.in_list = true;
            // A caption pending in front of a list is docling's own text item
            // (the `.Procedure` / `.Verification` lead-ins), flushed when the
            // list opens rather than swallowed by the first item.
            self.flush_caption(doc);
            self.levels = vec![ListLevel {
                indent: item.indent,
                slots: 0,
                enumerated: item.numbered,
            }];
            self.fresh_list = true;
        } else if item.indent > self.levels.last().unwrap().indent {
            // The nested group itself occupies a slot in its parent group.
            self.levels.last_mut().unwrap().slots += 1;
            self.levels.push(ListLevel {
                indent: item.indent,
                slots: 0,
                enumerated: item.numbered,
            });
        } else {
            while self.levels.len() > 1 && item.indent < self.levels.last().unwrap().indent {
                self.levels.pop();
            }
        }
        let level = (self.levels.len() - 1) as u8;
        let group = self.levels.last_mut().unwrap();
        group.slots += 1;
        // An explicit numeric marker (`1.`, `12.`) is printed verbatim by
        // docling-core, whatever the item's position; every other enumerated
        // item is numbered by that position.
        let (ordered, number) = match numeric_marker(item.marker) {
            Some(n) => (true, n),
            None => (group.enumerated, group.slots),
        };
        doc.push(Node::ListItem {
            ordered,
            number,
            first_in_list: self.fresh_list,
            text: escape_text(item.text.trim()),
            level,
            marker: numeric_marker(item.marker).map(|_| item.marker.to_string()),
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        self.last_item = Some(doc.nodes.len() - 1);
        self.fresh_list = false;
        self.list_continuation = false;
    }

    fn push_picture(&mut self, uri: &str, doc: &mut DoclingDocument) {
        let cap = self.take_caption();
        let image = self.images.and_then(|r| r.resolve(uri));
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
        self.deferred.push(Node::Picture {
            caption: cap,
            caption_href: None,
            image,
            classification: None,
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
        self.levels.clear();
        self.last_item = None;
        self.list_continuation = false;
    }

    /// Take a pending caption for the picture or table that claims it.
    fn take_caption(&mut self) -> Option<String> {
        if self.caption_data.is_empty() {
            return None;
        }
        let cap = self.caption_data.join(" ");
        self.caption_data.clear();
        Some(escape_text(&cap))
    }

    /// Emit a pending caption as docling's standalone caption text item — what
    /// happens when a list or a literal block follows it instead of a figure.
    fn flush_caption(&mut self, doc: &mut DoclingDocument) {
        if let Some(text) = self.take_caption() {
            doc.push(Node::Caption { text, href: None });
        }
    }

    fn flush_text(&mut self, doc: &mut DoclingDocument) {
        if !self.text_data.is_empty() {
            let text = self.text_data.join(" ");
            self.text_data.clear();
            doc.push(Node::Paragraph {
                text: escape_text(&text),
            });
        }
    }

    fn flush_table(&mut self, doc: &mut DoclingDocument) {
        if !self.table_data.is_empty() {
            // A pending caption is attached to this table and renders before it.
            if let Some(cap) = self.take_caption() {
                doc.push(Node::Paragraph { text: cap });
            }
            let num_cols = self.table_data.iter().map(Vec::len).max().unwrap_or(0);
            let rows: Vec<Vec<String>> = self
                .table_data
                .drain(..)
                .map(|mut r| {
                    r.resize(num_cols, String::new());
                    r
                })
                .collect();
            doc.push(Node::Table(Table {
                rows,
                location: None,
                structure: None,
                cell_blocks: None,
                cells: None,
                caption: None,
            }));
        }
        self.in_table = false;
        self.table_data.clear();
    }

    fn finish(&mut self, doc: &mut DoclingDocument) {
        self.flush_text(doc);
        if self.in_table {
            self.flush_table(doc);
        }
        // Root-attached items (images, orphan headings) render last.
        doc.nodes.append(&mut self.deferred);
    }

    fn is_active(&self, level: usize) -> bool {
        self.active.get(level).copied().unwrap_or(false)
    }

    /// Mark `level` active and clear all deeper levels (a heading resets its
    /// descendants), mirroring docling's `parents` bookkeeping.
    fn set_active(&mut self, level: usize) {
        if self.active.len() <= level {
            self.active.resize(level + 1, false);
        }
        self.active[level] = true;
        self.active.truncate(level + 1);
    }
}

fn is_title(line: &str) -> Option<&str> {
    line.strip_prefix("= ")
}

/// `== Section` → (number-of-`=`, text). A bare `=` (title) is excluded.
fn section_header(line: &str) -> Option<(u8, String)> {
    if !line.starts_with("==") {
        return None;
    }
    let caps = cached_regex!(r"^(=+)\s+(.*)").captures(line)?;
    let level = caps.get(1)?.as_str().len() as u8;
    Some((level, caps.get(2)?.as_str().to_string()))
}

/// A parsed list item, docling's `_parse_list_item`.
struct ListItem<'a> {
    /// docling's `indent`: the leading whitespace, plus one per extra `.` — a
    /// dotted marker encodes its depth in the marker itself (`..` nests under
    /// `.`), not in the indentation.
    indent: usize,
    marker: &'a str,
    numbered: bool,
    text: &'a str,
}

fn list_item(line: &str) -> Option<ListItem<'_>> {
    let caps = cached_regex!(LIST_ITEM).captures(line)?;
    let marker = caps.get(2)?.as_str();
    let mut indent = caps.get(1)?.as_str().len();
    if marker.starts_with('.') {
        indent += marker.len() - 1;
    }
    Some(ListItem {
        indent,
        marker,
        numbered: !(marker == "*" || marker == "-"),
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

fn is_table_line(line: &str) -> bool {
    cached_regex!(&format!(r"^{CELL_SPEC}\|.*\|")).is_match(line)
}

/// Strip cell specifiers glued before a `|`, split on `|`, drop the leading
/// empty field, and trim — exactly as docling's `_parse_table_line`.
fn parse_table_line(line: &str) -> Vec<String> {
    let cleaned = cached_regex!(&format!(r"(^|\s){CELL_SPEC}(\|)")).replace_all(line, "$1$2");
    cleaned
        .split('|')
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

    #[test]
    fn literal_block_becomes_a_code_item() {
        assert_eq!(
            md("= T\n\npara\n\n....\nraw literal\n  indented\n....\n\nafter\n"),
            "# T\n\npara\n\n```\nraw literal\n  indented\n```\n\nafter\n"
        );
        // An unterminated block still yields its body (docling's `_iter_blocks`
        // flushes what it has at EOF).
        assert_eq!(md("....\nno close\n"), "```\nno close\n```\n");
        // A caption in front of a literal block is docling's own text item.
        assert_eq!(
            md(".Literal example\n....\nraw\n....\n"),
            "Literal example\n\n```\nraw\n```\n"
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
        // depth (`..` nests under `.`), and the nested group takes a position in
        // the parent group, so the item after it is numbered past the gap.
        assert_eq!(
            md(". one\n. two\n.. nested\n. three\na. lettered\n"),
            "1. one\n2. two\n    1. nested\n4. three\n5. lettered\n"
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
        // A group whose *first* item is a bullet renders every positional item
        // as a bullet, whatever its own marker was.
        assert_eq!(md("* bullet\n. dotted\n"), "- bullet\n- dotted\n");
    }

    #[test]
    fn a_broken_run_of_explicit_markers_is_split_into_sibling_lists() {
        // Known deviation (docs/MIGRATION.md): our Markdown serializer
        // reconstructs list boundaries from the item numbering, because the
        // backends that need it most (docx, rtf) cannot flag them — so a jump
        // in explicit markers reads as a new list and gets a blank line, where
        // docling keeps one list because it tracked the group itself.
        assert_eq!(md("1. one\n5. five\n"), "1. one\n\n5. five\n");
    }

    #[test]
    fn a_caption_in_front_of_a_list_is_kept_as_text() {
        assert_eq!(
            md(".Procedure\n\n. one\n\n.Verification\n\n* check\n"),
            "Procedure\n\n1. one\n\nVerification\n\n- check\n"
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

    #[test]
    fn a_lone_word_and_period_stays_a_paragraph() {
        // `\w+\.` + `\s+` matches a bare "Intro." only when the line still
        // carries its newline, which is how docling reads a *file* but not a
        // stream — where it agrees with us and keeps the paragraph.
        assert_eq!(md("= T\n\nIntro.\n"), "# T\n\nIntro.\n");
        // A first word ending in a period *does* open an item, as docling's own
        // widened pattern does on either input — and it is enumerated, so the
        // group numbers it.
        assert_eq!(md("Fig. 1 shows the result\n"), "1. 1 shows the result\n");
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
            AsciiDocBackend { fetch_images: true }
                .convert(&source)
                .unwrap(),
        )
        .expect("the local image is read");
        assert_eq!((fetched.width, fetched.height), (2, 3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
