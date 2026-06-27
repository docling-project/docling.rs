//! Markdown backend (CommonMark via `pulldown-cmark`).
//!
//! Two output modes, selected by [`MarkdownBackend::strict`]:
//!
//! * **legacy** (`strict = false`, the default) reproduces docling's
//!   `MarkdownDocumentBackend` (marko) + docling-core serializer round-trip,
//!   quirks and all: inline content split into "runs" rejoined with single
//!   spaces (`***x***.` → `***x*** .`), `_`/`&<>` escaping, HTML entities
//!   decoded then re-escaped, and a lone inline-code paragraph turned into a
//!   code block.
//! * **strict** (`strict = true`) emits cleaner, more conformant Markdown:
//!   inline text is kept verbatim (no run-spacing, no escaping), inline code is
//!   left literal, and a lone code span stays an inline-code paragraph.
//!
//! Table cells are plain text in both modes; pipe/newline escaping is done by
//! the serializer.

use docling_crab_core::{DoclingDocument, Node, Table};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

#[derive(Default)]
pub struct MarkdownBackend {
    /// Emit cleaner, more conformant Markdown rather than docling-legacy output.
    pub strict: bool,
}

impl DeclarativeBackend for MarkdownBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);

        // Merge only *contiguous* text events. pulldown splits both `[11]`
        // (failed link) and `2\.` (escape) into multiple text events, but the
        // escape leaves an offset gap (the backslash). marko keeps `[11]` as one
        // run yet treats the escaped `.` as a separate run — so merging the
        // contiguous pieces and leaving gapped ones split reproduces both.
        let raw: Vec<(Event, std::ops::Range<usize>)> =
            Parser::new_ext(text, opts).into_offset_iter().collect();
        let mut events: Vec<Event> = Vec::with_capacity(raw.len());
        let mut k = 0;
        while k < raw.len() {
            if matches!(raw[k].0, Event::Text(_)) {
                let mut merged = String::new();
                let mut end = raw[k].1.start;
                while let Some((Event::Text(t), range)) = raw.get(k) {
                    if range.start != end {
                        break;
                    }
                    merged.push_str(t);
                    end = range.end;
                    k += 1;
                }
                events.push(Event::Text(merged.into()));
            } else {
                events.push(raw[k].0.clone());
                k += 1;
            }
        }

        let mut doc = DoclingDocument::new(&source.name);
        let mut i = 0;
        self.parse_blocks(&events, &mut i, &mut doc.nodes, 0, Stop::Eof);
        Ok(doc)
    }
}

/// What terminates a `parse_blocks` run.
#[derive(Clone, Copy, PartialEq)]
enum Stop {
    Eof,
    BlockQuote,
}

impl MarkdownBackend {
    // -----------------------------------------------------------------------
    // Block structure
    // -----------------------------------------------------------------------

    fn parse_blocks(
        &self,
        events: &[Event],
        i: &mut usize,
        out: &mut Vec<Node>,
        list_level: u8,
        stop: Stop,
    ) {
        while *i < events.len() {
            match &events[*i] {
                Event::End(TagEnd::BlockQuote(_)) if stop == Stop::BlockQuote => {
                    *i += 1;
                    return;
                }
                // Any other End belongs to a container handled elsewhere — skip
                // it rather than abandoning the rest of the document.
                Event::End(_) => {
                    *i += 1;
                }
                Event::Start(Tag::HtmlBlock) => {
                    *i += 1;
                    let mut html = String::new();
                    while *i < events.len() && !matches!(events[*i], Event::End(TagEnd::HtmlBlock))
                    {
                        if let Event::Html(t) = &events[*i] {
                            html.push_str(t);
                        }
                        *i += 1;
                    }
                    consume_end(events, i);
                    // docling parses embedded raw-HTML blocks; reuse the HTML backend.
                    super::html::append_fragment(&html, out);
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    *i += 1;
                    self.parse_blocks(events, i, out, list_level, Stop::BlockQuote);
                }
                Event::Start(Tag::Paragraph) => {
                    // Legacy: a lone inline code span becomes a code block.
                    if !self.strict
                        && matches!(events.get(*i + 1), Some(Event::Code(_)))
                        && matches!(events.get(*i + 2), Some(Event::End(TagEnd::Paragraph)))
                    {
                        if let Some(Event::Code(c)) = events.get(*i + 1) {
                            let text = unescape_entities(c.trim());
                            if !text.is_empty() {
                                out.push(Node::Code {
                                    language: None,
                                    text,
                                });
                            }
                        }
                        *i += 3;
                        continue;
                    }
                    *i += 1;
                    let runs = self.collect_inline(events, i, false);
                    consume_end(events, i);
                    let text = self.join_runs(&runs, false);
                    if !text.is_empty() {
                        out.push(Node::Paragraph { text });
                    }
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    let lvl = heading_level(*level);
                    *i += 1;
                    let runs = self.collect_inline(events, i, false);
                    consume_end(events, i);
                    let text = self.join_runs(&runs, false);
                    if !text.is_empty() {
                        out.push(Node::Heading { level: lvl, text });
                    }
                }
                Event::Start(Tag::List(start)) => {
                    let ordered = start.is_some();
                    let start_num = start.unwrap_or(1);
                    *i += 1;
                    self.parse_list(events, i, out, list_level, ordered, start_num);
                }
                Event::Start(Tag::CodeBlock(kind)) => {
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => {
                            let lang = info.split_whitespace().next().unwrap_or("");
                            (!lang.is_empty()).then(|| lang.to_string())
                        }
                        CodeBlockKind::Indented => None,
                    };
                    *i += 1;
                    let mut code = String::new();
                    while *i < events.len() && !matches!(events[*i], Event::End(TagEnd::CodeBlock))
                    {
                        if let Event::Text(t) = &events[*i] {
                            code.push_str(t);
                        }
                        *i += 1;
                    }
                    consume_end(events, i);
                    let trimmed = code.trim_end();
                    // Legacy decodes entities in code; strict keeps it literal.
                    let text = if self.strict {
                        trimmed.to_string()
                    } else {
                        unescape_entities(trimmed)
                    };
                    if !text.is_empty() {
                        out.push(Node::Code { language, text });
                    }
                }
                Event::Start(Tag::Table(_)) => {
                    *i += 1;
                    self.parse_table(events, i, out);
                }
                _ => {
                    *i += 1;
                }
            }
        }
    }

    fn parse_list(
        &self,
        events: &[Event],
        i: &mut usize,
        out: &mut Vec<Node>,
        level: u8,
        ordered: bool,
        start: u64,
    ) {
        let mut number = start;
        let mut first = true;
        while *i < events.len() {
            match &events[*i] {
                Event::Start(Tag::Item) => {
                    *i += 1;
                    self.parse_item(events, i, out, level, ordered, number, first);
                    number += 1;
                    first = false;
                }
                Event::End(TagEnd::List(_)) => {
                    *i += 1;
                    return;
                }
                _ => {
                    *i += 1;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_item(
        &self,
        events: &[Event],
        i: &mut usize,
        out: &mut Vec<Node>,
        level: u8,
        ordered: bool,
        number: u64,
        first_in_list: bool,
    ) {
        let mut emitted = false;
        while *i < events.len() {
            match &events[*i] {
                Event::End(TagEnd::Item) => {
                    *i += 1;
                    break;
                }
                Event::Start(Tag::List(start)) => {
                    let nested_ordered = start.is_some();
                    let nested_start = start.unwrap_or(1);
                    *i += 1;
                    self.parse_list(events, i, out, level + 1, nested_ordered, nested_start);
                }
                Event::Start(Tag::Paragraph) => {
                    *i += 1;
                    let runs = self.collect_inline(events, i, false);
                    consume_end(events, i);
                    emit_item(
                        out,
                        &mut emitted,
                        ordered,
                        number,
                        first_in_list,
                        self.join_runs(&runs, false),
                        level,
                    );
                }
                ev if is_inline_start(ev) => {
                    let runs = self.collect_inline(events, i, false);
                    emit_item(
                        out,
                        &mut emitted,
                        ordered,
                        number,
                        first_in_list,
                        self.join_runs(&runs, false),
                        level,
                    );
                }
                _ => {
                    *i += 1;
                }
            }
        }
    }

    fn parse_table(&self, events: &[Event], i: &mut usize, out: &mut Vec<Node>) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        while *i < events.len() {
            match &events[*i] {
                Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                    *i += 1;
                    rows.push(self.parse_table_row(events, i));
                }
                Event::End(TagEnd::Table) => {
                    *i += 1;
                    break;
                }
                _ => {
                    *i += 1;
                }
            }
        }
        if !rows.is_empty() {
            out.push(Node::Table(Table { rows }));
        }
    }

    fn parse_table_row(&self, events: &[Event], i: &mut usize) -> Vec<String> {
        let mut cells = Vec::new();
        while *i < events.len() {
            match &events[*i] {
                Event::Start(Tag::TableCell) => {
                    *i += 1;
                    let runs = self.collect_inline(events, i, true);
                    consume_end(events, i);
                    cells.push(self.join_runs(&runs, true));
                }
                Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                    *i += 1;
                    break;
                }
                _ => {
                    *i += 1;
                }
            }
        }
        cells
    }

    // -----------------------------------------------------------------------
    // Inline runs
    // -----------------------------------------------------------------------

    /// Collect inline runs until a non-inline boundary (left unconsumed). Each
    /// (already contiguity-merged) text event is its own run; the join then
    /// separates runs with spaces (legacy) so an escape-split `2\.` becomes
    /// `2 .` while a contiguous `[11]` stays `[11]`.
    fn collect_inline(&self, events: &[Event], i: &mut usize, table: bool) -> Vec<String> {
        let mut runs: Vec<String> = Vec::new();
        while *i < events.len() {
            match &events[*i] {
                Event::Text(t) => {
                    self.push_text(t, table, &mut runs);
                    *i += 1;
                }
                Event::SoftBreak | Event::HardBreak => {
                    // The legacy join already inserts a space; strict concatenates,
                    // so it needs an explicit space here.
                    if self.strict && !table {
                        runs.push(" ".to_string());
                    }
                    *i += 1;
                }
                // Raw inline HTML tags are dropped.
                Event::InlineHtml(_) | Event::Html(_) => {
                    *i += 1;
                }
                Event::Code(t) => {
                    // Strict (non-table) keeps the code span literal; otherwise
                    // entities are decoded as docling does.
                    let content = if self.strict && !table {
                        t.to_string()
                    } else {
                        unescape_entities(t)
                    };
                    runs.push(if table {
                        content
                    } else {
                        format!("`{content}`")
                    });
                    *i += 1;
                }
                Event::Start(Tag::Emphasis) => runs.push(self.wrap_inline(events, i, table, "*")),
                Event::Start(Tag::Strong) => runs.push(self.wrap_inline(events, i, table, "**")),
                Event::Start(Tag::Strikethrough) => {
                    runs.push(self.wrap_inline(events, i, table, "~~"))
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    let url = dest_url.to_string();
                    *i += 1;
                    let inner = self.join_runs(&self.collect_inline(events, i, table), table);
                    consume_end(events, i);
                    runs.push(if table {
                        inner
                    } else {
                        format!("[{inner}]({url})")
                    });
                }
                Event::Start(Tag::Image { dest_url, .. }) => {
                    let url = dest_url.to_string();
                    *i += 1;
                    let alt = self.join_runs(&self.collect_inline(events, i, table), table);
                    consume_end(events, i);
                    runs.push(if table {
                        alt
                    } else {
                        format!("![{alt}]({url})")
                    });
                }
                _ => break,
            }
        }
        runs
    }

    fn wrap_inline(&self, events: &[Event], i: &mut usize, table: bool, marker: &str) -> String {
        *i += 1;
        let inner = self.join_runs(&self.collect_inline(events, i, table), table);
        consume_end(events, i);
        if table {
            inner
        } else {
            format!("{marker}{inner}{marker}")
        }
    }

    /// Push one text event as a run. Legacy (non-table) trims and escapes
    /// `_`/`&<>`; strict keeps it verbatim; table cells are trimmed plain text
    /// (the serializer escapes pipes).
    fn push_text(&self, text: &str, table: bool, runs: &mut Vec<String>) {
        if table {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                runs.push(trimmed.to_string());
            }
        } else if self.strict {
            if !text.is_empty() {
                runs.push(text.to_string());
            }
        } else {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                runs.push(escape_html(&escape_underscores(trimmed)));
            }
        }
    }

    /// Join runs. Legacy (and all table cells) drops empty runs and separates
    /// with single spaces; strict (non-table) concatenates verbatim, preserving
    /// the source spacing carried in the text runs.
    fn join_runs(&self, runs: &[String], table: bool) -> String {
        if self.strict && !table {
            runs.concat()
        } else {
            runs.iter()
                .filter(|r| !r.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        }
    }
}

/// Emit a list item once, only if it has text. docling drops empty items.
#[allow(clippy::too_many_arguments)]
fn emit_item(
    out: &mut Vec<Node>,
    emitted: &mut bool,
    ordered: bool,
    number: u64,
    first_in_list: bool,
    text: String,
    level: u8,
) {
    if !*emitted && !text.is_empty() {
        out.push(Node::ListItem {
            ordered,
            number,
            first_in_list,
            text,
            level,
        });
        *emitted = true;
    }
}

fn consume_end(events: &[Event], i: &mut usize) {
    if matches!(events.get(*i), Some(Event::End(_))) {
        *i += 1;
    }
}

fn is_inline_start(event: &Event) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::InlineHtml(_)
            | Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough)
            | Event::Start(Tag::Link { .. } | Tag::Image { .. })
    )
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

// ---------------------------------------------------------------------------
// Escaping helpers (mirror docling-core's serializer)
// ---------------------------------------------------------------------------

/// `html.escape(text, quote=False)`: only `& < >`.
pub(crate) fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// docling-core's plain-text escape: underscores then `& < >` (the same pair
/// `serialize_run` applies). Used by backends that emit text nodes directly.
pub(crate) fn escape_text(text: &str) -> String {
    escape_html(&escape_underscores(text))
}

/// Escape `_` as `\_` unless already escaped.
pub(crate) fn escape_underscores(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev = '\0';
    for ch in text.chars() {
        if ch == '_' && prev != '\\' {
            out.push('\\');
        }
        out.push(ch);
        prev = ch;
    }
    out
}

/// Decode the HTML entities docling's `html.unescape` resolves for the cases we
/// see (named common + numeric). Used for code spans/blocks, which pulldown
/// leaves literal but docling decodes.
fn unescape_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'&' {
            if let Some(semi) = text[idx..].find(';') {
                let entity = &text[idx + 1..idx + semi];
                if let Some(ch) = decode_entity(entity) {
                    out.push(ch);
                    idx += semi + 1;
                    continue;
                }
            }
        }
        let ch = text[idx..].chars().next().unwrap();
        out.push(ch);
        idx += ch.len_utf8();
    }
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "vert" => Some('|'),
        _ => {
            let code = if let Some(hex) = entity.strip_prefix("#x").or(entity.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()?
            } else if let Some(dec) = entity.strip_prefix('#') {
                dec.parse().ok()?
            } else {
                return None;
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(md: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Md, md.as_bytes().to_vec());
        MarkdownBackend { strict: false }.convert(&src).unwrap()
    }

    fn convert_strict(md: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Md, md.as_bytes().to_vec());
        let mut doc = MarkdownBackend { strict: true }.convert(&src).unwrap();
        doc.strict_markdown = true;
        doc
    }

    #[test]
    fn inline_runs_get_spaced_in_legacy() {
        let doc = convert("Foo *emphasis* **strong** ***both***.\n");
        assert_eq!(
            doc.export_to_markdown(),
            "Foo *emphasis* **strong** ***both*** .\n"
        );
    }

    #[test]
    fn strict_keeps_inline_clean() {
        let doc = convert_strict("Foo *emphasis* **strong** ***both***.\n");
        assert_eq!(
            doc.export_to_markdown(),
            "Foo *emphasis* **strong** ***both***.\n"
        );
    }

    #[test]
    fn strict_keeps_code_language_and_inline_code() {
        let doc = convert_strict("```rust\nlet x = 1;\n```\n");
        assert_eq!(doc.export_to_markdown(), "```rust\nlet x = 1;\n```\n");
    }

    #[test]
    fn ordered_list_honors_start() {
        let doc = convert("3. third\n4. fourth\n");
        assert_eq!(doc.export_to_markdown(), "3. third\n4. fourth\n");
    }

    #[test]
    fn parses_github_table_plain_cells() {
        let doc = convert("| **A** | B |\n|---|---|\n| x | y |\n");
        assert_eq!(
            doc.export_to_markdown(),
            "| A   | B   |\n|-----|-----|\n| x   | y   |\n"
        );
    }

    #[test]
    fn lone_code_span_becomes_code_block_in_legacy() {
        let doc = convert("`&amp; &lt;`\n");
        assert_eq!(
            doc.nodes,
            vec![Node::Code {
                language: None,
                text: "& <".into(),
            }]
        );
    }
}
