//! AsciiDoc backend.
//!
//! A line-oriented port of docling's `AsciiDocBackend._parse`: titles (`= `),
//! section headers (`== `…), bullet/numbered lists, bare and `|===`-delimited
//! tables (with cell-format-specifier stripping), images and captions, and
//! multi-line paragraphs.

use docling_core::{DoclingDocument, Node, Table};

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

/// AsciiDoc cell specifier, e.g. `2*`, `^`, `.^`, `h` — the run that can be
/// glued before a `|` in a table line. Mirrors docling's `_CELL_SPEC`.
const CELL_SPEC: &str = r"(?:\d+(?:\.\d+)?[*+])*[<^>]?(?:\.[<^>])?[adehlms]?";

pub struct AsciiDocBackend;

impl DeclarativeBackend for AsciiDocBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let mut doc = DoclingDocument::new(&source.name);

        let mut p = Parser::default();
        // Iterate raw lines, preserving them as docling does (it never strips the
        // trailing newline-only state machine differently from `str::lines`).
        for line in text.lines() {
            p.feed(line, &mut doc);
        }
        p.finish(&mut doc);

        Ok(doc)
    }
}

#[derive(Default)]
struct Parser {
    text_data: Vec<String>,
    caption_data: Vec<String>,
    table_data: Vec<Vec<String>>,
    in_list: bool,
    in_table: bool,
    /// Indents of the currently-open list levels (index = markdown nesting level).
    list_indents: Vec<usize>,
    /// Per-level running number for ordered lists.
    list_counts: Vec<u64>,
    /// Whether the next emitted item starts a fresh list (for the serializer).
    fresh_list: bool,
    /// Which heading levels (docling 0-based) currently have an active ancestor.
    active: Vec<bool>,
    /// Images and orphan (level-skipping) headings: docling attaches these to the
    /// body root with no parent, so they render after the whole title subtree.
    deferred: Vec<Node>,
}

impl Parser {
    fn feed(&mut self, line: &str, doc: &mut DoclingDocument) {
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
        if let Some((indent, ordered, text)) = list_item(line) {
            self.push_list_item(indent, ordered, &text, doc);
            return;
        }
        if self.in_list {
            // A non-list line ends the current list and is itself swallowed —
            // docling's `elif in_list and not is_list_item` branch consumes it.
            self.end_list();
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

        // Picture — added with no parent, so deferred to the document end.
        if let Some(caption) = is_picture(line) {
            let cap = self.take_caption(doc);
            let _ = caption; // alt text is not rendered by docling's markdown export
            self.deferred.push(Node::Picture {
                caption: cap,
                image: None,
            });
            return;
        }

        // Caption: a line beginning with `.` (only when none is pending)
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

    fn push_list_item(
        &mut self,
        indent: usize,
        ordered: bool,
        text: &str,
        doc: &mut DoclingDocument,
    ) {
        if !self.in_list {
            self.in_list = true;
            self.list_indents = vec![indent];
            self.list_counts = vec![0];
            self.fresh_list = true;
        } else if indent > *self.list_indents.last().unwrap() {
            self.list_indents.push(indent);
            self.list_counts.push(0);
        } else {
            while self.list_indents.len() > 1 && indent < *self.list_indents.last().unwrap() {
                self.list_indents.pop();
                self.list_counts.pop();
            }
        }
        let level = (self.list_indents.len() - 1) as u8;
        let count = self.list_counts.last_mut().unwrap();
        *count += 1;
        let number = *count;
        // docling's AsciiDoc backend adds every item to a plain `LIST` group and
        // ignores the numbered/bullet distinction, so all items render as `-`.
        let _ = ordered;
        doc.push(Node::ListItem {
            ordered: false,
            number,
            first_in_list: self.fresh_list,
            text: escape_text(text.trim()),
            level,
        });
        self.fresh_list = false;
    }

    fn end_list(&mut self) {
        self.in_list = false;
        self.list_indents.clear();
        self.list_counts.clear();
    }

    fn take_caption(&mut self, doc: &mut DoclingDocument) -> Option<String> {
        if self.caption_data.is_empty() {
            return None;
        }
        let cap = self.caption_data.join(" ");
        self.caption_data.clear();
        let _ = doc;
        Some(escape_text(&cap))
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
            if let Some(cap) = self.take_caption(doc) {
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
            doc.push(Node::Table(Table { rows }));
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

/// A list item → (indent width, ordered, text).
fn list_item(line: &str) -> Option<(usize, bool, String)> {
    let caps = cached_regex!(r"^(\s*)(\*|-|\d+\.)\s+(.*)").captures(line)?;
    let indent = caps.get(1)?.as_str().len();
    let marker = caps.get(2)?.as_str();
    let text = caps.get(3)?.as_str().to_string();
    let ordered = !(marker == "*" || marker == "-");
    Some((indent, ordered, text))
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

fn is_picture(line: &str) -> Option<String> {
    if !line.starts_with("image::") {
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
    // `.text` but not `..` and not a list/other marker.
    let rest = line.strip_prefix('.')?;
    (!rest.is_empty()).then_some(rest)
}
