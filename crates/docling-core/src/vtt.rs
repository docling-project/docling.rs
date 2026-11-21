//! WebVTT export (#614) — a port of docling-core's `WebVTTDocSerializer`
//! (`transforms/serializer/webvtt.py`, docling-core 2.101) and the
//! `WebVTTFile` / `WebVTTCueBlock` formatting it hands its cues to, which is
//! what docling's `--to vtt` writes.
//!
//! The walk is upstream's: the body-layer items in document order (picture
//! children skipped, captions kept), where only a text item whose `source`
//! is a track makes a cue — a WebVTT input's cues and an ASR segment
//! ([`Node::Track`](crate::Node::Track)) — and a title item names the file
//! (`WEBVTT <title>`). Everything else (tables, pictures, lists, untimed
//! text) is not represented, so a document without timed text is the bare
//! `WEBVTT` header, as upstream writes it. The items are read from the
//! document's JSON export, so this sees exactly the labels, groups,
//! formatting and tracks the JSON carries.
//!
//! Per item, its text gets the cue spans of its formatting — `<b>`, then
//! `<i>`, then `<u>` around that (upstream's `post_process` order) — and a
//! `<v voice>` span outermost. An inline group joins its items' texts.
//! Consecutive items of one cue (same identifier and timings — a voice span
//! broken over lines in the source) join with a line feed. Redundant tag
//! pairs (`</i><i>`, `</v>\n<v A>`) are merged by the ports of upstream's two
//! regular expressions below. Text is escaped as `WebVTTCueTextSpan` writes it
//! (`&amp;`, `&lt;`); upstream re-parses the cue text instead, and fails on a
//! raw `&` or `<`.

use serde_json::Value;

use crate::DoclingDocument;

/// Options of the WebVTT export — docling-core's `WebVTTParams`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VttExportOptions {
    /// Write `MM:SS.mmm` when a cue's hour is 0 (`omit_hours_if_zero`).
    pub omit_hours_if_zero: bool,
    /// Drop the `</v>` of a cue whose payload is one voice span
    /// (`omit_voice_end`).
    pub omit_voice_end: bool,
}

impl Default for VttExportOptions {
    /// What docling's `--to vtt` writes (`DoclingDocument.save_as_vtt`): hours
    /// always, voice end tags omitted.
    fn default() -> Self {
        Self {
            omit_hours_if_zero: false,
            omit_voice_end: true,
        }
    }
}

pub(crate) fn to_vtt(doc: &DoclingDocument, options: &VttExportOptions) -> String {
    // Tables stay typed (#875): cues never come from them.
    let (json, _tables) = crate::json::to_json_typed_tables(doc, false);
    let mut parts = Vec::new();
    let mut inline_done = std::collections::HashSet::new();
    walk(&json, &json["body"], &mut parts, &mut inline_done);
    serialize_doc(&parts, options)
}

/// One serialized top-level part: its text and the item that times it.
struct Part<'a> {
    text: String,
    title: bool,
    track: Option<&'a Value>,
}

fn resolve<'a>(json: &'a Value, r: &str) -> Option<&'a Value> {
    let path = r.strip_prefix("#/")?;
    if path == "body" {
        return Some(&json["body"]);
    }
    let (bucket, idx) = path.split_once('/')?;
    json.get(bucket)?.get(idx.parse::<usize>().ok()?)
}

fn children<'a>(json: &'a Value, item: &'a Value) -> Vec<&'a Value> {
    item["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["$ref"].as_str().and_then(|r| resolve(json, r)))
        .collect()
}

fn is_text(item: &Value) -> bool {
    item["self_ref"]
        .as_str()
        .is_some_and(|r| r.starts_with("#/texts/"))
}

/// docling-core's `_iterate_items_with_stack` with `with_groups`: every
/// body-layer node in pre-order, a picture's children skipped bar its
/// captions; an inline group serializes its items itself (they are then
/// `visited`).
fn walk<'a>(
    json: &'a Value,
    item: &'a Value,
    parts: &mut Vec<Part<'a>>,
    done: &mut std::collections::HashSet<&'a str>,
) {
    let self_ref = item["self_ref"].as_str().unwrap_or("");
    let body_layer = item["content_layer"].as_str().is_none_or(|l| l == "body");
    if body_layer && !done.contains(self_ref) {
        if is_text(item) {
            if let Some(part) = text_part(item, false) {
                parts.push(part);
            }
        } else if item["label"] == "inline" && self_ref.starts_with("#/groups/") {
            let kids = children(json, item);
            let mut text = String::new();
            for kid in &kids {
                if let Some(r) = kid["self_ref"].as_str() {
                    done.insert(r);
                }
                if is_text(kid) && kid["content_layer"].as_str().is_none_or(|l| l == "body") {
                    if let Some(part) = text_part(kid, true) {
                        text.push_str(&part.text);
                    }
                }
            }
            let text = remove_pairs_until_stable(text);
            if !text.is_empty() {
                // The group's first child times it.
                let first = kids.first().copied();
                parts.push(Part {
                    text,
                    title: false,
                    track: first.and_then(track_of),
                });
            }
        }
    }
    let picture = self_ref.starts_with("#/pictures/");
    let captions: Vec<&str> = item["captions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["$ref"].as_str())
        .collect();
    for (child_ref, child) in item["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["$ref"].as_str())
        .filter_map(|r| Some((r, resolve(json, r)?)))
    {
        if picture && !captions.contains(&child_ref) {
            continue;
        }
        walk(json, child, parts, done);
    }
}

fn track_of(item: &Value) -> Option<&Value> {
    let source = item["source"].as_array()?.first()?;
    (source["kind"] == "track").then_some(source)
}

/// `WebVTTTextSerializer.serialize`: a title's text as is, a timed item's
/// text with its formatting and voice spans, nothing for the rest.
fn text_part(item: &Value, inline: bool) -> Option<Part<'_>> {
    let text = item["text"].as_str().unwrap_or("");
    if item["label"] == "title" {
        return Some(Part {
            text: text.to_string(),
            title: true,
            track: None,
        });
    }
    let track = track_of(item)?;
    if text.is_empty() {
        return None;
    }
    let mut out = escape(text);
    let f = &item["formatting"];
    for (flag, tag) in [("bold", "b"), ("italic", "i"), ("underline", "u")] {
        if f[flag].as_bool() == Some(true) {
            out = format!("<{tag}>{out}</{tag}>");
        }
    }
    if let Some(voice) = track["voice"].as_str().filter(|v| !v.is_empty()) {
        out = format!(
            "<v {}>{out}</v>",
            voice.replace('&', "&amp;").replace('>', "&gt;")
        );
    }
    if inline {
        out = remove_pairs_until_stable(out);
    }
    Some(Part {
        text: out,
        title: false,
        track: Some(track),
    })
}

/// `WebVTTCueTextSpan.__str__`: `&` and the cue-text terminator `<` escaped.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;")
}

/// `WebVTTTimestamp.from_seconds(s)`: whole milliseconds, rounded half to
/// even like Python's `round`.
fn millis(seconds: f64) -> u64 {
    (seconds * 1000.0).round_ties_even().max(0.0) as u64
}

/// `WebVTTTimestamp.seconds` of [`millis`]: what upstream compares a later
/// item's start/end against when it merges items into one cue.
fn timestamp_seconds(ms: u64) -> f64 {
    let (h, rest) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rest) = (rest / 60_000, rest % 60_000);
    let (s, milli) = (rest / 1000, rest % 1000);
    (h * 3600 + m * 60 + s) as f64 + milli as f64 / 1000.0
}

fn format_timestamp(ms: u64, omit_hours_if_zero: bool) -> String {
    let (h, rest) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rest) = (rest / 60_000, rest % 60_000);
    let (s, milli) = (rest / 1000, rest % 1000);
    if omit_hours_if_zero && h == 0 {
        format!("{m:02}:{s:02}.{milli:03}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{milli:03}")
    }
}

struct Cue {
    identifier: Option<String>,
    start: u64,
    end: u64,
    text: String,
}

/// docling-core's `_cue_timings` (docling-core#1014): both ends in
/// milliseconds; a segment that rounds onto a zero-length range — a track
/// source accepts any end after its start, a WebVTT timestamp carries
/// milliseconds only — is stretched by the smallest step, 1 ms, and the
/// loss of precision reported, instead of writing `11.000 --> 11.000`
/// (upstream used to raise for the whole document).
fn cue_timings(start_s: f64, end_s: f64) -> (u64, u64) {
    let (start, end) = (millis(start_s), millis(end_s));
    if end > start {
        return (start, end);
    }
    crate::debug_log!(
        "vtt: track segment {start_s}..{end_s} collapses onto {} at WebVTT millisecond \
         precision, extending the cue end to {}",
        format_timestamp(start, false),
        format_timestamp(start + 1, false)
    );
    (start, start + 1)
}

/// `WebVTTDocSerializer.serialize_doc` with `WebVTTFile.format`.
fn serialize_doc(parts: &[Part], options: &VttExportOptions) -> String {
    let mut title: Option<&str> = None;
    let mut cues: Vec<Cue> = Vec::new();
    for part in parts {
        if part.text.is_empty() {
            continue;
        }
        if part.title {
            title = Some(&part.text);
            continue;
        }
        let Some(track) = part.track else {
            continue;
        };
        let start_s = track["start_time"].as_f64().unwrap_or(0.0);
        let end_s = track["end_time"].as_f64().unwrap_or(0.0);
        let identifier = track["identifier"].as_str().map(str::to_string);
        if let Some(cue) = cues.last_mut().filter(|c| {
            c.identifier == identifier
                && timestamp_seconds(c.start) == start_s
                && timestamp_seconds(c.end) == end_s
        }) {
            let joined = format!("{}\n{}", cue.text.trim_end(), part.text);
            cue.text = remove_pairs_until_stable(joined);
        } else {
            let (start, end) = cue_timings(start_s, end_s);
            cues.push(Cue {
                identifier,
                start,
                end,
                text: part.text.clone(),
            });
        }
    }
    let mut out = match title {
        Some(t) => format!("WEBVTT {t}\n"),
        None => "WEBVTT\n".to_string(),
    };
    for cue in &cues {
        out.push('\n');
        if let Some(id) = &cue.identifier {
            out.push_str(id);
            out.push('\n');
        }
        out.push_str(&format_timestamp(cue.start, options.omit_hours_if_zero));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(cue.end, options.omit_hours_if_zero));
        out.push('\n');
        let payload = cue.text.trim_end_matches('\n');
        if options.omit_voice_end && is_single_voice_span(payload) {
            out.push_str(payload.strip_suffix("</v>").unwrap_or(payload));
        } else {
            out.push_str(payload);
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_string()
}

/// Whether the payload is one `<v …>…</v>` span — `WebVTTCueBlock.format`'s
/// condition for `omit_voice_end` (a single component of kind `v`).
fn is_single_voice_span(payload: &str) -> bool {
    if !payload.starts_with("<v") || !payload.ends_with("</v>") {
        return false;
    }
    // The opening `<v …>` must be closed by the final `</v>` and nothing else
    // at depth 0.
    let mut depth = 0i32;
    let mut i = 0;
    let b = payload.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            let close = payload[i..].find('>').map(|j| i + j);
            let Some(end) = close else { return false };
            let tag = &payload[i + 1..end];
            if tag.starts_with('/') {
                depth -= 1;
                if depth == 0 && end + 1 != b.len() {
                    return false;
                }
            } else if !tag.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                // `<00:00:01.000>` timestamps are not spans.
                depth += 1;
            }
            i = end + 1;
        } else {
            if depth == 0 {
                return false;
            }
            i += 1;
        }
    }
    depth == 0
}

/// The span names upstream's merge patterns match (`[bciuv]|lang`).
fn span_name(s: &str) -> Option<&str> {
    if s.starts_with("lang") {
        Some("lang")
    } else {
        s.chars()
            .next()
            .filter(|c| "bciuv".contains(*c))
            .map(|_| &s[..1])
    }
}

/// A start tag `<name(.class)*( annotation)?>` at the start of `s` with the
/// given name (any name when `None`): (name, classes, annotation, length).
fn start_tag<'a>(s: &'a str, want: Option<&str>) -> Option<(&'a str, &'a str, &'a str, usize)> {
    let rest = s.strip_prefix('<')?;
    let name = match want {
        Some(w) => rest.starts_with(w).then_some(&rest[..w.len()])?,
        None => span_name(rest)?,
    };
    let mut pos = 1 + name.len();
    // (?:\.\w+)*
    let classes_start = pos;
    loop {
        let tail = &s[pos..];
        let Some(after_dot) = tail.strip_prefix('.') else {
            break;
        };
        let word = after_dot
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
            .map_or(after_dot.len(), |(i, _)| i);
        if word == 0 {
            break;
        }
        pos += 1 + word;
    }
    let classes = &s[classes_start..pos];
    // (?:\s+([^>]+))? then `>`
    let tail = &s[pos..];
    if let Some(stripped) = tail.strip_prefix('>') {
        let _ = stripped;
        return Some((name, classes, "", pos + 1));
    }
    let ws = tail
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map_or(tail.len(), |(i, _)| i);
    if ws == 0 {
        return None;
    }
    let anno_start = pos + ws;
    let close = s[anno_start..].find('>')?;
    if close == 0 {
        return None;
    }
    Some((
        name,
        classes,
        &s[anno_start..anno_start + close],
        anno_start + close + 1,
    ))
}

/// Upstream's first pattern, `re.sub` left to right: `<tag…>content</tag>
/// ws <tag…>` with matching classes and annotation becomes `<tag…>content
/// ws` — the second start tag and the first end tag dropped. `content` is the
/// text up to the first `</tag>` and may not hold a line break (`.` stops at
/// one); a non-matching pair is consumed unchanged.
fn merge_adjacent(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(m) = match_adjacent(&text[i..]) {
            out.push_str(&m.0);
            i += m.1;
        } else {
            let c = text[i..].chars().next().expect("in bounds");
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn match_adjacent(s: &str) -> Option<(String, usize)> {
    let (name, classes1, anno1, open_len) = start_tag(s, None)?;
    let end_tag = format!("</{name}>");
    let close = s[open_len..].find(&end_tag)?;
    let content = &s[open_len..open_len + close];
    if content.contains('\n') {
        return None;
    }
    let after_close = open_len + close + end_tag.len();
    let tail = &s[after_close..];
    let ws_len = tail
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map_or(tail.len(), |(i, _)| i);
    let ws = &tail[..ws_len];
    let (_, classes2, anno2, open2_len) = start_tag(&tail[ws_len..], Some(name))?;
    let total = after_close + ws_len + open2_len;
    if classes1 == classes2 && anno1 == anno2 {
        let anno = if anno1.is_empty() {
            String::new()
        } else {
            format!(" {anno1}")
        };
        Some((format!("<{name}{classes1}{anno}>{content}{ws}"), total))
    } else {
        Some((s[..total].to_string(), total))
    }
}

/// Upstream's second pattern: `</tag><other…><tag…>` becomes `<other…>`.
fn merge_around(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((keep, len)) = match_around(&text[i..]) {
            out.push_str(keep);
            i += len;
        } else {
            let c = text[i..].chars().next().expect("in bounds");
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

fn match_around(s: &str) -> Option<(&str, usize)> {
    let rest = s.strip_prefix("</")?;
    let name = span_name(rest)?;
    let after_name = 2 + name.len();
    if !s[after_name..].starts_with('>') {
        return None;
    }
    let mid_start = after_name + 1;
    let tail = &s[mid_start..];
    if !tail.starts_with('<') {
        return None;
    }
    let mid_close = tail[1..].find('>')? + 1;
    if mid_close < 2 {
        return None;
    }
    let mid = &tail[..mid_close + 1];
    let (_, _, _, open_len) = start_tag(&tail[mid.len()..], Some(name))?;
    Some((mid, mid_start + mid.len() + open_len))
}

/// `_remove_consecutive_pairs` until nothing changes.
fn remove_pairs_until_stable(mut text: String) -> String {
    loop {
        let next = merge_around(&merge_adjacent(&text));
        if next == text {
            return text;
        }
        text = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_like_upstreams_patterns() {
        // Each expected value is docling-core 2.101's `_remove_consecutive_pairs`
        // run to a fixed point on the same input.
        let cases = [
            ("<i>a</i> <i>b</i>", "<i>a b</i>"),
            (
                "<v Speaker A>OK,</v>\n<v Speaker A>I think</v>",
                "<v Speaker A>OK,\nI think</v>",
            ),
            ("<v A>x</v> <v B>y</v>", "<v A>x</v> <v B>y</v>"),
            (
                "<i><b>unexpected</b></i><i> </i><u><i>arcobaleno</i></u><i> of flavors</i>",
                "<i><b>unexpected</b> <u>arcobaleno</u> of flavors</i>",
            ),
            ("<b.loud>a</b><b.loud>b</b>", "<b.loud>ab</b>"),
            ("<b.loud>a</b><b>b</b>", "<b.loud>a</b><b>b</b>"),
            // `.` stops at a line break: no merge across one inside a span.
            ("<i>a\nb</i><i>c</i>", "<i>a\nb</i><i>c</i>"),
            ("<lang en>x</lang><lang en>y</lang>", "<lang en>xy</lang>"),
            // Two tags in between: neither pattern applies.
            ("<u>a</u><i></i><u>b</u>", "<u>a</u><i></i><u>b</u>"),
            ("<b>a</b>  \n <b>c</b>", "<b>a  \n c</b>"),
        ];
        for (input, want) in cases {
            assert_eq!(
                remove_pairs_until_stable(input.to_string()),
                want,
                "{input}"
            );
        }
    }

    /// An ASR segment as `docling-asr` builds it: the `[time: …]`
    /// paragraph wrapped in its track.
    fn segment(start: f64, end: f64, words: &str) -> crate::Node {
        crate::Node::Track {
            track: crate::tree::TreeTrack {
                start_time: start,
                end_time: end,
                identifier: None,
                voice: None,
            },
            cue: words.into(),
            inner: Box::new(crate::Node::Paragraph {
                text: format!("[time: {start}-{end}] {words}"),
            }),
        }
    }

    /// docling-core#1014
    /// (`test_webvtt_cue_shorter_than_a_millisecond_stays_serializable`): a
    /// sub-millisecond segment keeps a 1 ms cue; the next cue is unchanged.
    #[test]
    fn sub_millisecond_cue_stays_serializable() {
        let mut doc = DoclingDocument::new("sub-millisecond-cue");
        doc.push(segment(11.00001, 11.00002, "short cue"));
        doc.push(segment(12.0, 13.5, "normal cue"));
        assert_eq!(
            doc.export_to_vtt(),
            "WEBVTT\n\n00:00:11.000 --> 00:00:11.001\nshort cue\n\n\
             00:00:12.000 --> 00:00:13.500\nnormal cue"
        );
    }

    /// The flat (ASR) path against docling-core 2.101: the same segments as
    /// `add_text(..., source=TrackSource(...))` items give exactly these
    /// files — cues of the words alone, identical timings merged into one
    /// cue, the zero-duration bump visible, hours written.
    #[test]
    fn asr_segments_become_cues_like_upstream() {
        let mut doc = DoclingDocument::new("talk");
        for (s, e, t) in [
            (0.0, 2.345678, "And so my fellow Americans"),
            (2.5, 2.501, "ask not"),
            (3.0, 4.25, "what your country"),
            (3.0, 4.25, "can do for you"),
            (3661.2, 3662.0, "late remark"),
        ] {
            doc.push(segment(s, e, t));
        }
        assert_eq!(
            doc.export_to_vtt(),
            "WEBVTT\n\n00:00:00.000 --> 00:00:02.346\nAnd so my fellow Americans\n\n\
             00:00:02.500 --> 00:00:02.501\nask not\n\n00:00:03.000 --> 00:00:04.250\n\
             what your country\ncan do for you\n\n01:01:01.200 --> 01:01:02.000\nlate remark"
        );
        // The JSON item is docling 2.135's ASR item: the words as text, the
        // timing as its track source.
        let json = doc.export_to_json_value();
        assert_eq!(json["texts"][0]["text"], "And so my fellow Americans");
        assert_eq!(json["texts"][0]["orig"], "And so my fellow Americans");
        assert_eq!(
            json["texts"][0]["source"],
            serde_json::json!([{"kind": "track", "start_time": 0.0, "end_time": 2.345678}])
        );
        // Markdown is untouched by the wrapper.
        assert!(doc
            .export_to_markdown()
            .starts_with("[time: 0-2.345678] And so"));
    }

    /// Without timed text the file is the header — with the title, if any
    /// (docling-core writes `WEBVTT My Title` / `WEBVTT`).
    #[test]
    fn untimed_documents_are_the_bare_header() {
        let mut doc = DoclingDocument::new("x");
        doc.push(crate::Node::Heading {
            level: 1,
            text: "My Title".into(),
        });
        doc.push(crate::Node::Paragraph {
            text: "no timing".into(),
        });
        assert_eq!(doc.export_to_vtt(), "WEBVTT My Title");
        let mut doc = DoclingDocument::new("y");
        doc.push(crate::Node::Paragraph {
            text: "no timing".into(),
        });
        assert_eq!(doc.export_to_vtt(), "WEBVTT");
    }

    /// Cue text is escaped as `WebVTTCueTextSpan` writes it (upstream fails
    /// on a raw `&` or `<` instead).
    #[test]
    fn cue_text_is_escaped() {
        let mut doc = DoclingDocument::new("e");
        doc.push(segment(1.0, 2.0, "Q&A: a < b > c"));
        assert_eq!(
            doc.export_to_vtt(),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nQ&amp;A: a &lt; b > c"
        );
    }

    #[test]
    fn timestamps_round_half_even_and_format() {
        assert_eq!(format_timestamp(millis(4.963), false), "00:00:04.963");
        assert_eq!(format_timestamp(millis(14_586.5), false), "04:03:06.500");
        assert_eq!(format_timestamp(millis(62.0), true), "01:02.000");
        assert_eq!(millis(0.0005), 0); // 0.5 ms → even
        assert_eq!(millis(0.0015), 2);
        assert_eq!(timestamp_seconds(millis(4.963)), 4.963);
    }

    #[test]
    fn voice_end_is_omitted_only_for_a_lone_voice_span() {
        assert!(is_single_voice_span("<v A>OK,\nI think</v>"));
        assert!(is_single_voice_span("<v A><i>x</i> y</v>"));
        assert!(!is_single_voice_span("<v Esme>Hee!</v> <i>laughter</i>"));
        assert!(!is_single_voice_span("<v A>x</v><v B>y</v>"));
        assert!(!is_single_voice_span("plain"));
    }
}
