//! WebVTT (`.vtt`) backend — a port of docling's `WebVTTDocumentBackend` on
//! top of docling-core's `WebVTTFile` parser.
//!
//! Each cue's payload becomes one paragraph per line. Cue-text spans are parsed:
//! `<b>`/`<i>`/`<u>` apply formatting (bold inner, italic outer, underline no
//! marker), `<v …>` records the voice, while `<c …>`, `<lang …>` and cue
//! timestamps are transparent (their inner text is kept, the tag dropped). A
//! line's components are each wrapped in their Markdown markers and joined with
//! single spaces — docling-core's inline-group serialization.
//!
//! The JSON export gets docling's item tree: a `title` for the header's text,
//! one `text` item per single-component line, an inline group named
//! `WebVTT cue span` holding the components of a multi-span line, and on every
//! item the cue's `TrackSource` (`start_time`/`end_time` in seconds, the cue
//! identifier and the enclosing voice) plus the inherited `formatting`.

use docling_core::tree::{Formatting, ItemTree, TreeKind, TreeTrack};
use docling_core::{DoclingDocument, Node};

use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct WebVttBackend;

/// docling's `AnnotatedText` metadata: the enclosing voice and the formatting
/// a text run inherits from every span around it.
#[derive(Default, Clone)]
struct Meta {
    voice: Option<String>,
    formatting: Option<Formatting>,
}

/// One text run of a cue paragraph with its inherited metadata.
struct Run {
    text: String,
    meta: Meta,
}

/// A cue component: a text line (with docling-core's optional line terminator)
/// or a span with its nested components.
enum Comp {
    Text {
        text: String,
        terminator: bool,
    },
    Span {
        tag: Tag,
        annotation: Option<String>,
        children: Vec<Comp>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Bold,
    Italic,
    Underline,
    Voice,
    /// `<c>` and `<lang>`: no metadata of their own (docling-core parses the
    /// class list / language but the backend ignores both).
    Transparent,
}

/// Nesting depth past which a span is flattened into its parent (its tag
/// ignored) — docling-core recurses per span; we cap the recursion.
const MAX_SPAN_DEPTH: usize = 100;

struct Cue {
    start: f64,
    end: f64,
    identifier: Option<String>,
    payload: Vec<Comp>,
}

impl DeclarativeBackend for WebVttBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let content = source.text()?.replace("\r\n", "\n").replace('\r', "\n");
        let mut doc = DoclingDocument::new(&source.name);
        let mut tree = ItemTree::default();

        // The header is the first line only; docling-core's parser drops the
        // rest of its block (a cue-less block) — as our cue parser does.
        let (header, body) = content.split_once('\n').unwrap_or((&content, ""));
        if let Some(rest) = header.strip_prefix("WEBVTT") {
            let title = rest.trim();
            if !title.is_empty() {
                doc.push(Node::Heading {
                    level: 1,
                    text: escape_text(title),
                });
                tree.add(None, None, text_kind("title", title, None));
            }
        }

        for block in blank_line_blocks(body) {
            if block.starts_with("NOTE")
                || block.starts_with("STYLE")
                || block.starts_with("REGION")
            {
                continue;
            }
            let Some(cue) = parse_cue_block(block) else {
                continue;
            };
            let mut paras: Vec<Vec<Run>> = vec![Vec::new()];
            extract_components(&cue.payload, &mut Vec::new(), &mut paras);
            for para in &paras {
                if para.is_empty() {
                    continue;
                }
                let text = para.iter().map(serialize_run).collect::<Vec<_>>().join(" ");
                if !text.is_empty() {
                    doc.push(Node::Paragraph { text });
                }
                let track = |run: &Run| TreeTrack {
                    start_time: cue.start,
                    end_time: cue.end,
                    identifier: cue.identifier.clone(),
                    voice: run.meta.voice.clone().filter(|v| !v.is_empty()),
                };
                if let [run] = para.as_slice() {
                    let id = tree.add(
                        None,
                        None,
                        text_kind("text", &run.text, run.meta.formatting),
                    );
                    tree.items[id].source = Some(track(run));
                } else {
                    let group = tree.add(
                        None,
                        None,
                        TreeKind::Group {
                            label: "inline".into(),
                            name: "WebVTT cue span".into(),
                        },
                    );
                    for run in para {
                        let id = tree.add(
                            Some(group),
                            None,
                            text_kind("text", &run.text, run.meta.formatting),
                        );
                        tree.items[id].source = Some(track(run));
                    }
                }
            }
        }
        doc.tree = Some(tree);
        Ok(doc)
    }
}

fn text_kind(label: &str, text: &str, formatting: Option<Formatting>) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting,
        hyperlink: None,
        level: None,
        list: None,
    }
}

/// Split on blank (whitespace-only) lines — `re.split(r"\n\s*\n", body.strip())`.
fn blank_line_blocks(body: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let end = offset + line.len();
        if line.trim().is_empty() {
            if let Some(s) = start.take() {
                blocks.push(body[s..offset].trim());
            }
        } else if start.is_none() {
            start = Some(offset);
        }
        offset = end;
    }
    if let Some(s) = start {
        blocks.push(body[s..].trim());
    }
    blocks
}

/// docling-core's `WebVTTCueBlock.parse`: an optional identifier line, the
/// timings line (cue settings ignored), then the cue text. A block whose
/// timings are missing or malformed is dropped, as upstream drops it.
fn parse_cue_block(block: &str) -> Option<Cue> {
    let lines: Vec<&str> = block.lines().collect();
    let first = *lines.first()?;
    let (identifier, timing_line, cue_lines) = if !first.contains("-->") && lines.len() > 1 {
        (Some(first.to_string()), lines[1], &lines[2..])
    } else {
        (None, first, &lines[1..])
    };
    let mut parts = timing_line.split("-->");
    let start = parts.next()?.trim();
    let end = parts.next()?.trim();
    if parts.next().is_some() {
        return None;
    }
    let end = end.split([' ', '\t']).next().unwrap_or("");
    let (start, end) = (parse_timestamp(start)?, parse_timestamp(end)?);
    if end <= start {
        return None;
    }
    let mut cue_text = cue_lines.join("\n").trim().to_string();
    // A voice span may omit its end tag when it spans the whole cue.
    if cue_text.starts_with("<v") && !cue_text.contains("</v>") {
        cue_text.push_str("</v>");
    }
    Some(Cue {
        start,
        end,
        identifier,
        payload: parse_components(&cue_text),
    })
}

/// `(?:(\d{2,}):)?([0-5]\d):([0-5]\d)\.(\d{3})` → seconds, computed exactly
/// as docling-core does so the JSON float is identical.
fn parse_timestamp(raw: &str) -> Option<f64> {
    let (rest, millis) = raw.split_once('.')?;
    if millis.len() != 3 || !millis.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fields: Vec<&str> = rest.split(':').collect();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [m, s] => ("", *m, *s),
        [h, m, s] if h.len() >= 2 => (*h, *m, *s),
        _ => return None,
    };
    let two_digits = |f: &str| {
        (f.len() == 2 && f.bytes().all(|b| b.is_ascii_digit()) && f.as_bytes()[0] <= b'5')
            .then(|| f.parse::<u64>().ok())
            .flatten()
    };
    let hours = if hours.is_empty() {
        0
    } else if hours.bytes().all(|b| b.is_ascii_digit()) {
        hours.parse::<u64>().ok()?
    } else {
        return None;
    };
    let (minutes, seconds) = (two_digits(minutes)?, two_digits(seconds)?);
    let millis: u64 = millis.parse().ok()?;
    Some((hours * 3600 + minutes * 60 + seconds) as f64 + millis as f64 / 1000.0)
}

/// Is `tag` (the text between `<` and `>`) a cue timestamp tag — stripped
/// from the text without splitting the run around it (docling-core#744:
/// `the<…> quick<…> brown` is one span).
fn is_timestamp_tag(tag: &str) -> bool {
    parse_timestamp(tag).is_some()
}

/// Tokenize the cue text into nested components, docling-core's
/// `_pattern_tag` walk: `<i|b|c|u|v|lang[.class…][ annotation]>` open and
/// `</…>` close; text between tags splits into one component per line, each
/// but the last carrying a line terminator. Text inside a span that is never
/// closed is dropped, as upstream never appends the open span.
fn parse_components(cue_text: &str) -> Vec<Comp> {
    // Open spans: the tag (`None` past `MAX_SPAN_DEPTH`), its annotation and
    // the components parsed so far inside it.
    let mut stack: Vec<(Option<Tag>, Option<String>, Vec<Comp>)> = Vec::new();
    let mut root: Vec<Comp> = Vec::new();
    let mut buf = String::new();
    let mut rest = cue_text;

    let flush = |buf: &mut String, target: &mut Vec<Comp>| {
        if buf.is_empty() {
            return;
        }
        let text = std::mem::take(buf);
        let pieces: Vec<&str> = text.split('\n').collect();
        let n = pieces.len();
        for (i, line) in pieces.iter().enumerate() {
            if !line.is_empty() {
                target.push(Comp::Text {
                    text: line.to_string(),
                    terminator: i + 1 < n,
                });
            }
        }
    };

    while let Some(lt) = rest.find('<') {
        let Some(gt_rel) = rest[lt..].find('>') else {
            break;
        };
        let tag = &rest[lt + 1..lt + gt_rel];
        let after = &rest[lt + gt_rel + 1..];
        if is_timestamp_tag(tag) {
            buf.push_str(&rest[..lt]);
            rest = after;
            continue;
        }
        let Some((closing, kind, annotation)) = parse_tag(tag) else {
            // Not a cue span tag docling-core knows: kept as text upstream
            // (where the block then fails validation); we drop the tag and
            // keep the text.
            buf.push_str(&rest[..lt]);
            rest = after;
            continue;
        };
        buf.push_str(&rest[..lt]);
        flush(&mut buf, stack.last_mut().map_or(&mut root, |s| &mut s.2));
        rest = after;
        if closing {
            if let Some((t, annotation, children)) = stack.pop() {
                let target = stack.last_mut().map_or(&mut root, |s| &mut s.2);
                match t {
                    Some(t) if t != kind => {
                        // Mismatched end tag: upstream rejects the cue block.
                        return Vec::new();
                    }
                    Some(tag) => target.push(Comp::Span {
                        tag,
                        annotation,
                        children,
                    }),
                    None => target.extend(children),
                }
            }
        } else {
            let tag = (stack.len() < MAX_SPAN_DEPTH).then_some(kind);
            stack.push((tag, annotation, Vec::new()));
        }
    }
    buf.push_str(rest);
    flush(&mut buf, stack.last_mut().map_or(&mut root, |s| &mut s.2));
    root
}

/// `(/?)(i|b|c|u|v|lang)((?:\.[^\t\n\r &<>.]+)*)(?:[ \t]([^\n\r&>]*))?` on the
/// text between `<` and `>`; `None` when the tag is not one docling-core
/// recognizes.
fn parse_tag(tag: &str) -> Option<(bool, Tag, Option<String>)> {
    let (closing, body) = match tag.strip_prefix('/') {
        Some(b) => (true, b),
        None => (false, tag),
    };
    let name_len = body
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(body.len());
    let (name, mut after) = body.split_at(name_len);
    let kind = match name {
        "i" => Tag::Italic,
        "b" => Tag::Bold,
        "u" => Tag::Underline,
        "v" => Tag::Voice,
        "c" | "lang" => Tag::Transparent,
        _ => return None,
    };
    // Class list: `.name` segments without whitespace, `&`, `<`, `>` or `.`.
    while let Some(cls) = after.strip_prefix('.') {
        let len = cls
            .find(['\t', '\n', '\r', ' ', '&', '<', '>', '.'])
            .unwrap_or(cls.len());
        if len == 0 {
            return None;
        }
        after = &cls[len..];
    }
    let annotation = match after.chars().next() {
        None => None,
        Some(' ' | '\t') => {
            let a = &after[1..];
            if a.contains(['\n', '\r', '&']) {
                return None;
            }
            let a = a.trim();
            (!a.is_empty()).then(|| a.to_string())
        }
        Some(_) => return None,
    };
    Some((closing, kind, annotation))
}

/// docling's `_extract_components`: walk the components in reading order,
/// each text run taking the metadata of the spans around it; a component's
/// line terminator starts a new paragraph, so text after a multi-line span
/// joins the span's last paragraph (docling#4105).
fn extract_components(comps: &[Comp], parents: &mut Vec<Meta>, paras: &mut Vec<Vec<Run>>) {
    for comp in comps {
        let mut meta = parents.last().cloned().unwrap_or_default();
        match comp {
            Comp::Text { text, terminator } => {
                paras.last_mut().expect("paragraph").push(Run {
                    text: text.clone(),
                    meta,
                });
                if *terminator {
                    paras.push(Vec::new());
                }
            }
            Comp::Span {
                tag,
                annotation,
                children,
            } => {
                match tag {
                    Tag::Bold => {
                        meta.formatting.get_or_insert_with(Formatting::default).bold = true
                    }
                    Tag::Italic => {
                        meta.formatting
                            .get_or_insert_with(Formatting::default)
                            .italic = true
                    }
                    Tag::Underline => {
                        meta.formatting
                            .get_or_insert_with(Formatting::default)
                            .underline = true
                    }
                    Tag::Voice => meta.voice = annotation.clone(),
                    Tag::Transparent => {}
                }
                parents.push(meta);
                extract_components(children, parents, paras);
                parents.pop();
            }
        }
    }
}

/// Wrap a run's (escaped) text in its Markdown markers — bold inner, italic
/// outer, so bold+italic collapses to `***…***`. Underline carries none.
fn serialize_run(run: &Run) -> String {
    let mut s = escape_text(&run.text);
    let fmt = run.meta.formatting.unwrap_or_default();
    if fmt.bold {
        s = format!("**{s}**");
    }
    if fmt.italic {
        s = format!("*{s}*");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(vtt: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Vtt, vtt.as_bytes().to_vec());
        WebVttBackend.convert(&src).unwrap()
    }

    fn md(vtt: &str) -> String {
        convert(vtt).export_to_markdown()
    }

    #[test]
    fn strips_voice_and_skips_notes() {
        let out = md("WEBVTT\n\nNOTE hi\n\n00:01.000 --> 00:02.000\n<v Roger>Hello world\n");
        assert_eq!(out.trim(), "Hello world");
    }

    /// docling#4105: a line terminator inside a voice span starts a new
    /// paragraph, and the text after the span's end joins *that* paragraph —
    /// upstream's items are `["Hello", "there", " and afterwards"]`.
    #[test]
    fn text_after_multiline_span_stays_in_reading_order() {
        let out =
            md("WEBVTT\n\n00:00:01.000 --> 00:00:05.000\n<v Bob>Hello\nthere</v> and afterwards\n");
        assert_eq!(out.trim(), "Hello\n\nthere  and afterwards");
    }

    /// docling-core#744: karaoke cue timestamps (`<00:00:00.389>`) are stripped
    /// without splitting the span — one run, `the quick brown`.
    #[test]
    fn cue_timestamp_tags_are_stripped_from_the_run() {
        let out = md("WEBVTT\n\n00:00:00.030 --> 00:00:02.669\nthe<00:00:00.389> quick<00:00:00.750> brown\n");
        assert_eq!(out.trim(), "the quick brown");
    }

    /// docling-core#749 / docling#4157: bare CR and CRLF line terminators are
    /// valid WebVTT — signature, cue separation and payload all parse.
    #[test]
    fn cr_and_crlf_terminators_parse() {
        assert_eq!(
            md("WEBVTT\r\r00:00:00.000 --> 00:00:01.000\rHello world\r").trim(),
            "Hello world"
        );
        assert_eq!(
            md("WEBVTT\r\n\r\n00:00:00.000 --> 00:00:01.000\r\nHello\r\nworld\r\n").trim(),
            "Hello\n\nworld"
        );
    }

    #[test]
    fn nested_spans_serialize_with_inline_join() {
        // bold inside italic → ***x***; lang is transparent; components joined
        // with single spaces (so the un-stripped span spacing is preserved).
        let out = md("WEBVTT\n\n00:01.000 --> 00:02.000\n\
             a <i>b <lang es>c</lang></i> d\n");
        assert_eq!(out.trim(), "a  *b * *c*  d");
    }

    /// The JSON tree: a `title`, a `text` per single-run line with the cue's
    /// `TrackSource` (seconds, identifier, voice) and inherited formatting,
    /// and an inline `WebVTT cue span` group for a multi-run line.
    #[test]
    fn tree_carries_track_source_and_formatting() {
        let doc = convert(
            "WEBVTT Kitchen talk\n\nid-1\n01:02:03.500 --> 01:02:04.750 line:0\n\
             <v Chef>Hello <b>there</b></v>\nBye\n",
        );
        let json = doc.export_to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let texts = v["texts"].as_array().unwrap();
        assert_eq!(texts[0]["label"], "title");
        assert_eq!(texts[0]["text"], "Kitchen talk");
        assert!(texts[0].get("source").is_none());
        assert_eq!(v["groups"][0]["name"], "WebVTT cue span");
        assert_eq!(v["groups"][0]["label"], "inline");
        assert_eq!(texts[1]["parent"]["$ref"], "#/groups/0");
        assert_eq!(texts[1]["text"], "Hello ");
        assert_eq!(
            texts[1]["source"],
            serde_json::json!([{
                "kind": "track",
                "start_time": 3723.5,
                "end_time": 3724.75,
                "identifier": "id-1",
                "voice": "Chef",
            }])
        );
        assert!(texts[1].get("formatting").is_none());
        assert_eq!(texts[2]["text"], "there");
        assert_eq!(texts[2]["formatting"]["bold"], true);
        assert_eq!(texts[2]["source"][0]["voice"], "Chef");
        // The line after the voice span carries no voice — and joins the
        // span's paragraph: docling-core drops the terminator of the empty
        // text piece before `Bye` (the docling#4105 quirk).
        assert_eq!(texts[3]["text"], "Bye");
        assert_eq!(texts[3]["parent"]["$ref"], "#/groups/0");
        assert!(texts[3]["source"][0].get("voice").is_none());
        assert_eq!(v["body"]["children"].as_array().unwrap().len(), 2);
    }

    /// A block without valid timings is dropped like upstream; cue settings
    /// after the end time and an omitted `</v>` are accepted.
    #[test]
    fn malformed_timings_drop_the_block() {
        assert_eq!(md("WEBVTT\n\n00:01.000 -> 00:02.000\nlost\n").trim(), "");
        assert_eq!(md("WEBVTT\n\n00:02.000 --> 00:01.000\nlost\n").trim(), "");
        assert_eq!(
            md("WEBVTT\n\n00:01.000 --> 00:02.000 align:start\n<v Ann>kept\n").trim(),
            "kept"
        );
    }
}
