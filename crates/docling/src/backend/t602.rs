//! T602 (`.602`) — the Czechoslovak DOS word processor of Software602
//! (Text602, 1980s–90s), a docling.rs extension (#216); docling reaches it
//! only through LibreOffice's T602 import filter, whose reading this
//! follows.
//!
//! A T602 file is 8-bit text, one line per `CR LF`:
//!
//! * a line that opens with `@` and two capitals is a setting — `@CT n`
//!   picks the code page (0 Kamenický, the default; 1 Latin-2 / CP852;
//!   2 KOI8-ČS), the rest (margins, page length, tabs) are layout and are
//!   skipped; a line that opens with `.` is a dot command or a comment, not
//!   text;
//! * a byte below 0x20 switches the character style: `0x02` bold, `0x04`
//!   italic, `0x0F` wide (read as bold), `0x13` underline, `0x14`
//!   superscript, `0x16` subscript; a second one returns to plain; the
//!   others (condensed, user styles, tall/big) carry no text;
//! * `0x8D` before the line end marks a line the editor wrapped — the
//!   paragraph goes on, a soft hyphen `0xAD` before it joins the word; the
//!   editor's hard space `0xFE` is a space; `0x1A` (DOS end of file) ends
//!   the text.
//!
//! T602 has no paragraph mark: its text is hard-wrapped to the page width.
//! So a blank line ends a paragraph and the lines between are one paragraph
//! read through (the alignment spaces collapse) — what LibreOffice's filter
//! does with its "reformat paragraphs" option. Two kinds of block keep more
//! of their layout: a frame or table drawn with the code page's box
//! characters stays preformatted, and a line opening with a bullet (`-`,
//! `*`, `•`) starts a list item.

use docling_core::{DoclingDocument, Node, Script};

use crate::backend::dos_codepage::{self, CP852, KAMENICKY, KOI8_CS};
use crate::backend::styled::{Para, Style};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct T602Backend;

/// Whether `bytes` open like a T602 file: the `@CT ` code-page line every
/// saved document starts with (LibreOffice's detection). Files without the
/// header (older ones) come by their `.602` extension.
pub(crate) fn looks_like_t602(bytes: &[u8]) -> bool {
    bytes.starts_with(b"@CT ")
}

impl DeclarativeBackend for T602Backend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        Ok(convert(&source.bytes, &source.name))
    }
}

/// The style a control byte toggles; `None` for one that carries no style.
fn toggle(style: &mut Style, b: u8) {
    let plain = Style::default();
    let target = match b {
        0x02 | 0x0f => Style {
            bold: true,
            ..plain
        },
        0x04 => Style {
            italic: true,
            ..plain
        },
        0x13 => {
            // Underline combines with the font, as in T602.
            style.underline = !style.underline;
            return;
        }
        0x14 => Style {
            script: Script::Super,
            ..plain
        },
        0x16 => Style {
            script: Script::Sub,
            ..plain
        },
        _ => return,
    };
    let underline = style.underline;
    // The fonts exclude each other: the same one again is plain.
    let same = Style {
        underline: false,
        ..*style
    } == target;
    *style = if same { plain } else { target };
    style.underline = underline;
}

/// One text line: its styled pieces; after a soft hyphen at a wrap
/// (`hyphen`), the next line goes on the same word.
struct Line {
    pieces: Vec<(String, Style)>,
    hyphen: bool,
}

impl Line {
    fn plain(&self) -> String {
        self.pieces.iter().map(|(t, _)| t.as_str()).collect()
    }
}

/// A bullet a line opens with (after its indent): `-`, `*`, `•`, `·`, `` —
/// the marker's length in chars, the space after it included.
fn bullet(text: &str) -> Option<usize> {
    let t = text.trim_start();
    let mut chars = t.chars();
    let marker = chars.next()?;
    let space = chars.next()?;
    (matches!(marker, '-' | '*' | '•' | '·' | '\u{25aa}') && space == ' ')
        .then(|| text.len() - t.len() + marker.len_utf8() + 1)
}

fn convert(bytes: &[u8], name: &str) -> DoclingDocument {
    let bytes = match bytes.iter().position(|&b| b == 0x1a) {
        Some(end) => &bytes[..end],
        None => bytes,
    };
    let mut doc = DoclingDocument::new(name);
    let mut table: &[char; 128] = &KAMENICKY;
    let mut style = Style::default();
    let mut block: Vec<Line> = Vec::new();
    for raw in bytes.split(|&b| b == b'\n') {
        let mut line = raw.strip_suffix(b"\r").unwrap_or(raw);
        // A setting: `@` + two capitals.
        if line.len() >= 3
            && line[0] == b'@'
            && line[1].is_ascii_uppercase()
            && line[2].is_ascii_uppercase()
        {
            if &line[1..3] == b"CT" {
                let n = std::str::from_utf8(&line[3..])
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok());
                table = match n {
                    Some(1) => &CP852,
                    Some(2) => &KOI8_CS,
                    _ => &KAMENICKY,
                };
            }
            continue;
        }
        // A dot command or comment line.
        if line.first() == Some(&b'.') {
            continue;
        }
        // The editor's own wrap: the paragraph goes on.
        let soft = line.last() == Some(&0x8d);
        if soft {
            line = &line[..line.len() - 1];
        }
        let hyphen = soft && line.last() == Some(&0xad);
        if hyphen {
            line = &line[..line.len() - 1];
        }
        let mut pieces: Vec<(String, Style)> = Vec::new();
        let mut text = Vec::new();
        for &b in line {
            match b {
                b'\t' | 0xfe => text.push(b' '),
                b if b < 0x20 => {
                    if !text.is_empty() {
                        pieces.push((dos_codepage::decode(&text, table), style));
                        text.clear();
                    }
                    toggle(&mut style, b);
                }
                b => text.push(b),
            }
        }
        if !text.is_empty() {
            pieces.push((dos_codepage::decode(&text, table), style));
        }
        let line = Line { pieces, hyphen };
        if line.plain().trim().is_empty() && !soft {
            emit_block(std::mem::take(&mut block), &mut doc);
        } else {
            block.push(line);
        }
    }
    emit_block(block, &mut doc);
    doc
}

/// A blank-line-separated block: a frame or table drawn with box
/// characters stays preformatted; otherwise its lines read through into a
/// paragraph, a bulleted line opening a list item.
fn emit_block(block: Vec<Line>, doc: &mut DoclingDocument) {
    if block.is_empty() {
        return;
    }
    let plains: Vec<String> = block.iter().map(Line::plain).collect();
    let drawn = plains
        .iter()
        .filter(|l| l.chars().any(|c| ('\u{2500}'..='\u{259f}').contains(&c)))
        .count()
        * 2
        >= plains.len();
    if drawn {
        let indent = plains
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0);
        let text = plains
            .iter()
            .map(|l| l.get(indent..).unwrap_or("").trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        doc.push(Node::Code {
            language: None,
            text,
            orig: None,
            pretty: None,
        });
        return;
    }
    let mut para = Para::new();
    let mut in_item = false;
    let mut join_word = false;
    let flush = |para: &mut Para, in_item: bool, doc: &mut DoclingDocument| {
        let Some(node) = para.take().into_node() else {
            return;
        };
        if !in_item {
            doc.push(node);
            return;
        }
        let text = match node {
            Node::Paragraph { text } => text,
            Node::InlineGroup { md_text, .. } => md_text,
            _ => return,
        };
        let first = !matches!(doc.nodes.last(), Some(Node::ListItem { .. }));
        doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: first,
            text,
            level: 0,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
    };
    for (line, plain) in block.iter().zip(&plains) {
        let mut skip = 0usize;
        if !join_word {
            if let Some(n) = bullet(plain) {
                flush(&mut para, in_item, doc);
                in_item = true;
                skip = n;
            }
            para.space();
        }
        // The pieces past the bullet (bytes of the line's plain text).
        let mut at = 0usize;
        for (text, style) in &line.pieces {
            let end = at + text.len();
            if end > skip {
                let from = skip.saturating_sub(at);
                let piece = &text[from..];
                // A word joined across a soft hyphen: no space before it.
                let piece = if join_word { piece.trim_start() } else { piece };
                para.push(piece, *style);
                if !piece.is_empty() {
                    join_word = false;
                }
            }
            at = end;
        }
        join_word = line.hyphen;
    }
    flush(&mut para, in_item, doc);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paras(doc: &DoclingDocument) -> Vec<String> {
        doc.nodes
            .iter()
            .map(|n| match n {
                Node::Paragraph { text } => text.clone(),
                Node::InlineGroup { md_text, .. } => md_text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn settings_code_pages_styles_and_wrapped_lines() {
        let src = b"@CT 0\r\n@LM 5\r\n.PA comment\r\n   Z\x8fKON \x02tu\x02n\xa0\r\n  \
            dal\xa8\xa1 \x04kurz\x04\xad\x8d\r\n    va\r\n\r\nDruh\x0fy\x0f odstavec\x8d\r\npokra\x87uje\r\n\x1aEOF garbage";
        let doc = convert(src, "t");
        assert_eq!(
            paras(&doc),
            vec![
                "ZÁKON **tu**ná další *kurz*va".to_string(),
                "Druh**y** odstavec pokračuje".to_string(),
            ]
        );
        // A drawn frame stays preformatted; bulleted lines are list items.
        let doc = convert(
            b"  \xda\xc4\xbf\r\n  \xb3x\xb3\r\n  \xc0\xc4\xd9\r\n\r\n   - one\r\n   - two\r\n     more\r\n",
            "t",
        );
        assert!(
            matches!(&doc.nodes[0], Node::Code { text, .. } if text == "\u{250c}\u{2500}\u{2510}\n\u{2502}x\u{2502}\n\u{2514}\u{2500}\u{2518}"),
            "{:?}",
            doc.nodes[0]
        );
        assert!(
            matches!(&doc.nodes[1], Node::ListItem { text, first_in_list: true, .. } if text == "one")
        );
        assert!(
            matches!(&doc.nodes[2], Node::ListItem { text, first_in_list: false, .. } if text == "two more")
        );
        // `@CT 1`: Latin-2 (CP852).
        let doc = convert(b"@CT 1\r\nP\xfd\xa1li\xe7 \xa7lu\x9cou\x9fk\xec\r\n", "t");
        assert_eq!(paras(&doc), vec!["Příliš žluťoučký".to_string()]);
        assert!(looks_like_t602(b"@CT 0\r\n") && !looks_like_t602(b"@ct"));
    }
}
