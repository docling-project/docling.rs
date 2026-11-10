//! Markdown backend (CommonMark via `pulldown-cmark`).
//!
//! Two output modes, selected by [`MarkdownBackend::strict`]:
//!
//! * **legacy** (`strict = false`, the default) reproduces docling's
//!   `MarkdownDocumentBackend` (marko) + docling-core serializer round-trip,
//!   quirks and all: inline content split into "runs" that keep their own
//!   boundary whitespace (docling#3891, `***x***.` stays `***x***.`),
//!   `_`/`&<>` escaping, HTML entities
//!   decoded then re-escaped, and a lone inline-code paragraph turned into a
//!   code block.
//! * **strict** (`strict = true`) emits cleaner, more conformant Markdown:
//!   inline text is kept verbatim (no run-spacing, no escaping), inline code is
//!   left literal, and a lone code span stays an inline-code paragraph.
//!
//! Table cells keep their inline formatting in both modes — `**bold**`,
//! `*italic*`, `~~struck~~`, `` `code` ``, links — as docling prints a
//! formatted cell since 2.135 (docling#4488); pipe/newline escaping is done by
//! the serializer.

use crate::backend::images::{FsImageResolver, ImagePolicy, ImageResolver, ImageSources, NoFetch};
use docling_core::{DoclingDocument, Node, Table};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

#[derive(Default)]
pub struct MarkdownBackend {
    /// Emit cleaner, more conformant Markdown rather than docling-legacy output.
    pub strict: bool,
    /// Which image destinations resolve to bytes (#646): `data:` URIs under
    /// `Embedded`, paths under the file's directory under `Local`, URLs
    /// under `Remote`; the default resolves none (upstream fetches none).
    /// A raw-HTML block's `<img>` goes through the same policy.
    pub images: ImagePolicy,
}

/// docling's `_GfmStrikethrough`, `(?<!~)~~([^~]+)~~(?!~)`: two tildes on
/// each side, none inside or around, with no flanking rule. pulldown's GFM
/// strikethrough also takes one (`~x~`) and a run with a tilde inside; such
/// a span goes back to the text it was, its tildes contiguous with the runs
/// around them. The other way round, a run of plain text holding the
/// pattern where pulldown saw no delimiters (`~~ spaced ~~`) is struck.
/// Used by both parsers, ahead of [`fold_math_into_text`].
pub(super) fn marko_strikethrough<'a>(
    raw: Vec<(Event<'a>, std::ops::Range<usize>)>,
    text: &'a str,
) -> Vec<(Event<'a>, std::ops::Range<usize>)> {
    let mut kept: Vec<bool> = Vec::new();
    let tildes = |r: std::ops::Range<usize>| (Event::Text(text[r.clone()].into()), r);
    let raw: Vec<_> = raw
        .into_iter()
        .map(|(event, range)| match event {
            Event::Start(Tag::Strikethrough) => {
                let src = &text[range.clone()];
                let keep = src.len() > 4
                    && src.starts_with("~~")
                    && src.ends_with("~~")
                    && !src[2..src.len() - 2].contains('~')
                    && !text[..range.start].ends_with('~')
                    && !text[range.end..].starts_with('~');
                kept.push(keep);
                if keep {
                    (event, range)
                } else {
                    let n = src.len() - src.trim_start_matches('~').len();
                    tildes(range.start..range.start + n)
                }
            }
            Event::End(TagEnd::Strikethrough) => {
                if kept.pop().unwrap_or(true) {
                    (event, range)
                } else {
                    let src = &text[range.clone()];
                    let n = src.len() - src.trim_end_matches('~').len();
                    tildes(range.end - n..range.end)
                }
            }
            _ => (event, range),
        })
        .collect();

    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut opaque = 0usize;
    let mut k = 0;
    while k < raw.len() {
        match &raw[k].0 {
            Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock | Tag::MetadataBlock(_)) => opaque += 1,
            Event::End(TagEnd::CodeBlock | TagEnd::HtmlBlock | TagEnd::MetadataBlock(_)) => {
                opaque = opaque.saturating_sub(1)
            }
            _ => {}
        }
        if opaque > 0 || !matches!(raw[k].0, Event::Text(_)) {
            out.push(raw[k].clone());
            k += 1;
            continue;
        }
        // A run of contiguous text whose source is its text: no escape or
        // resolved reference for a span to cut through.
        let (start, mut end, first) = (raw[k].1.start, raw[k].1.start, k);
        while let Some((Event::Text(t), r)) = raw.get(k) {
            if r.start != end || text.get(r.clone()) != Some(&**t) {
                break;
            }
            end = r.end;
            k += 1;
        }
        if k == first {
            out.push(raw[k].clone());
            k += 1;
            continue;
        }
        let mut spans = Vec::new();
        let mut i = start;
        while i + 1 < end {
            if bytes[i] == b'~' && bytes[i + 1] == b'~' && (i == 0 || bytes[i - 1] != b'~') {
                let mut j = i + 2;
                while j < end && bytes[j] != b'~' {
                    j += 1;
                }
                if j > i + 2
                    && j + 1 < end
                    && bytes[j + 1] == b'~'
                    && bytes.get(j + 2) != Some(&b'~')
                {
                    spans.push((i, j + 2));
                    i = j + 2;
                    continue;
                }
            }
            i += 1;
        }
        if spans.is_empty() {
            out.extend(raw[first..k].iter().cloned());
            continue;
        }
        let piece = |r: std::ops::Range<usize>| (Event::Text(text[r.clone()].into()), r);
        let mut at = start;
        for (s, e) in spans {
            if s > at {
                out.push(piece(at..s));
            }
            out.push((Event::Start(Tag::Strikethrough), s..e));
            out.push(piece(s + 2..e - 2));
            out.push((Event::End(TagEnd::Strikethrough), s..e));
            at = e;
        }
        if end > at {
            out.push(piece(at..end));
        }
    }
    out
}

/// What a GFM bare URL links to: marko's `gfm_el.Url` (an `AutoLink`).
#[derive(Clone, Copy, PartialEq)]
enum BareUrl {
    /// `http(s)://` or `ftp://`, linked as written.
    Url,
    /// `www.…`, linked with an `http://` prefix.
    Www,
    /// An e-mail address, a `mailto:` link.
    Email,
}

/// marko's GFM `Url.find` over the source text of one run of contiguous text
/// events (`run`, a byte range of `text`): the bare URLs it links, as
/// `(start, end, kind)` byte ranges of `text` (docling#4791, #773).
///
/// marko matches over the paragraph's whole inline source, so a match may
/// run past the text into the delimiters after it: `[^<\s]*` takes a closing
/// `*` or `**` along, and only one trailing `?!.,:*_~` is trimmed. A URL
/// whose trimmed end still overlaps that markup loses to the emphasis
/// (`**https://x.y/z**` and `*https://x.y/z*.` are not linked,
/// `*https://x.y/z* b` is), which here is a match ending past the run.
fn find_bare_urls(
    text: &str,
    run: std::ops::Range<usize>,
    in_table: bool,
) -> Vec<(usize, usize, BareUrl)> {
    // A URL holds no whitespace: nothing past the next one can be part of a
    // match that starts inside the run.
    let hay_end = text[run.end..]
        .find(|c: char| c.is_whitespace() || c == '<')
        .map_or(text.len(), |k| run.end + k);
    // Each pattern needs a literal — `://`, `www.`, `@` — inside its match,
    // and every match lies in `run.start..hay_end`: a pattern whose literal
    // is absent there cannot match, so it is neither compiled nor run. The
    // Unicode `\w` / `\s` classes make these four regexes ~20M
    // instructions to build — 5 ms, a third of a small Markdown file's
    // whole conversion — which a document without a bare URL never pays.
    let scope = &text[run.start..hay_end];
    let (has_url, has_www, has_email) = (
        scope.contains("://"),
        scope.contains("www."),
        scope.contains('@'),
    );
    if !(has_url || has_www || has_email) {
        return Vec::new();
    }
    let url = || cached_regex!(r"(?:https?|ftp)://([\w.\-]*?\.[\w.\-]+)[^<\s]*");
    let www = || cached_regex!(r"www\.([\w.\-]*?\.[\w.\-]+)[^<\s]*");
    let email = || cached_regex!(r"[\w.\-+]+@[\w.\-]*?\.[\w.\-]*[a-zA-Z0-9]");
    let email_whole = || cached_regex!(r"^[\w.\-+]+@[\w.\-]*?\.[\w.\-]*[a-zA-Z0-9]$");
    let entity_tail = cached_regex!(r"&[a-zA-Z]+;$");
    // The lookbehind `(?:^|(?<=[\s*_~(\uff00-\uffef]))`; a table cell's text
    // starts after its `|`.
    let starts_ok = |at: usize| match text[..at].chars().next_back() {
        // A link destination (`](https://…`) is no bare URL: marko reads the
        // link whole, which pulldown's math pass can keep it from seeing
        // (`[$$x$$](url)`).
        Some('(') if text[..at - 1].ends_with(']') => false,
        None => true,
        Some(c) => {
            c.is_whitespace()
                || matches!(c, '*' | '_' | '~' | '(')
                || ('\u{ff00}'..='\u{ffef}').contains(&c)
                || (in_table && c == '|')
        }
    };
    // The email alternative's lookahead `(?=[\s.<]|\Z)`, backtracking to a
    // shorter address as Python's regex engine would.
    let email_end = |start: usize, end: usize| -> Option<usize> {
        (start + 1..=end).rev().find(|&e| {
            text.is_char_boundary(e)
                && email_whole().is_match(&text[start..e])
                && text[e..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace() || c == '.' || c == '<')
        })
    };
    let domain_ok = |domain: &str| {
        let parts: Vec<&str> = domain.split('.').collect();
        parts.len() >= 2 && !parts[parts.len() - 2..].iter().any(|p| p.contains('_'))
    };
    let mut found = Vec::new();
    let mut pos = run.start;
    while pos < run.end {
        let hay = &text[pos..hay_end];
        // The leftmost candidate of the three patterns (an `http` URL before
        // an address at the same spot, as the alternation orders them).
        let candidates = [
            has_url.then(|| url().captures(hay)).flatten().map(|c| {
                (
                    c.get(0).unwrap(),
                    c.get(1).map(|d| d.as_str()),
                    BareUrl::Url,
                )
            }),
            has_www.then(|| www().captures(hay)).flatten().map(|c| {
                (
                    c.get(0).unwrap(),
                    c.get(1).map(|d| d.as_str()),
                    BareUrl::Www,
                )
            }),
            has_email
                .then(|| email().find(hay))
                .flatten()
                .map(|m| (m, None, BareUrl::Email)),
        ];
        let Some((m, domain, kind)) = candidates
            .into_iter()
            .flatten()
            .min_by_key(|(m, _, kind)| (m.start(), *kind != BareUrl::Url))
        else {
            break;
        };
        let start = pos + m.start();
        if start >= run.end {
            break;
        }
        let next = |at: usize| at + text[at..].chars().next().map_or(1, char::len_utf8);
        if !starts_ok(start) {
            // Python's `finditer` retries one character on.
            pos = next(start);
            continue;
        }
        let mut full_end = pos + m.end();
        if kind == BareUrl::Email {
            match email_end(start, full_end) {
                Some(e) => full_end = e,
                None => {
                    pos = next(start);
                    continue;
                }
            }
        }
        pos = full_end;
        if domain.is_some_and(|d| !domain_ok(d)) {
            continue;
        }
        // The trailing trim: one `?!.,:*_~`, unbalanced `)`s, or an entity.
        let link = &text[start..full_end];
        let mut end = full_end;
        if link.ends_with(['?', '!', '.', ',', ':', '*', '_', '~']) {
            end -= 1;
        } else if link.ends_with(')') && link.matches(')').count() > link.matches('(').count() {
            end -= link.matches(')').count() - link.matches('(').count();
        } else if let Some(m) = entity_tail.find(link) {
            end -= m.len();
        }
        if end <= run.end && end > start {
            found.push((start, end, kind));
        }
    }
    found
}

/// Link the GFM bare URLs (`https://…`, `www.…`, an address) of the text
/// runs, as marko's `gfm_el.Url` does and docling keeps since docling#4791
/// (#773): each becomes a link around its text — `Autolink` to the URL
/// (`http://` before a `www.`), or `Email` to the address. Text inside a
/// link, an image, a code or HTML block is left alone; a run is the
/// contiguous text events the parsers merge, so an escape (whose backslash
/// leaves a gap) ends one. Used by both parsers, after
/// [`fold_math_into_text`].
pub(super) fn link_bare_urls<'a>(
    raw: Vec<(Event<'a>, std::ops::Range<usize>)>,
    text: &'a str,
) -> Vec<(Event<'a>, std::ops::Range<usize>)> {
    let mut out = Vec::with_capacity(raw.len());
    let mut opaque = 0usize;
    let mut in_table = false;
    let mut k = 0;
    while k < raw.len() {
        match &raw[k].0 {
            Event::Start(
                Tag::Link { .. }
                | Tag::Image { .. }
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::MetadataBlock(_),
            ) => opaque += 1,
            Event::End(
                TagEnd::Link
                | TagEnd::Image
                | TagEnd::CodeBlock
                | TagEnd::HtmlBlock
                | TagEnd::MetadataBlock(_),
            ) => opaque = opaque.saturating_sub(1),
            Event::Start(Tag::Table(_)) => in_table = true,
            Event::End(TagEnd::Table) => in_table = false,
            _ => {}
        }
        if opaque > 0 || !matches!(raw[k].0, Event::Text(_)) {
            out.push(raw[k].clone());
            k += 1;
            continue;
        }
        let first = k;
        let mut run_end = raw[k].1.start;
        while let Some((Event::Text(_), r)) = raw.get(k) {
            if r.start != run_end {
                break;
            }
            run_end = r.end;
            k += 1;
        }
        let run = &raw[first..k];
        let urls = find_bare_urls(text, run[0].1.start..run_end, in_table);
        // A span must not cut through a resolved reference (`&amp;`), whose
        // text is not its source.
        let plain = |t: &str, r: &std::ops::Range<usize>| text.get(r.clone()) == Some(t);
        let urls: Vec<_> = urls
            .into_iter()
            .filter(|&(s, e, _)| {
                run.iter().all(|(ev, r)| match ev {
                    Event::Text(t) if !plain(t, r) => {
                        let inside = |c: usize| r.start < c && c < r.end;
                        !inside(s) && !inside(e)
                    }
                    _ => true,
                })
            })
            .collect();
        if urls.is_empty() {
            out.extend(run.iter().cloned());
            continue;
        }
        // Re-emit the run, cut at the spans' edges.
        let mut pieces: Vec<(String, std::ops::Range<usize>)> = Vec::new();
        for (ev, r) in run {
            let Event::Text(t) = ev else { continue };
            if !plain(t, r) {
                pieces.push((t.to_string(), r.clone()));
                continue;
            }
            let mut cuts: Vec<usize> = urls
                .iter()
                .flat_map(|&(s, e, _)| [s, e])
                .filter(|&c| r.start < c && c < r.end)
                .collect();
            cuts.sort_unstable();
            let mut at = r.start;
            for c in cuts.into_iter().chain([r.end]) {
                if c > at {
                    pieces.push((text[at..c].to_string(), at..c));
                }
                at = c;
            }
        }
        let mut p = 0;
        for &(s, e, kind) in &urls {
            while p < pieces.len() && pieces[p].1.end <= s {
                out.push((Event::Text(pieces[p].0.clone().into()), pieces[p].1.clone()));
                p += 1;
            }
            let inner: Vec<_> = pieces[p..]
                .iter()
                .take_while(|(_, r)| r.end <= e)
                .cloned()
                .collect();
            p += inner.len();
            let label: String = inner.iter().map(|(t, _)| t.as_str()).collect();
            let (link_type, dest) = match kind {
                BareUrl::Url => (pulldown_cmark::LinkType::Autolink, label.clone()),
                BareUrl::Www => (
                    pulldown_cmark::LinkType::Autolink,
                    format!("http://{label}"),
                ),
                BareUrl::Email => (pulldown_cmark::LinkType::Email, label.clone()),
            };
            out.push((
                Event::Start(Tag::Link {
                    link_type,
                    dest_url: dest.into(),
                    title: "".into(),
                    id: "".into(),
                }),
                s..e,
            ));
            for (t, r) in inner {
                out.push((Event::Text(t.into()), r));
            }
            out.push((Event::End(TagEnd::Link), s..e));
        }
        for (t, r) in &pieces[p..] {
            out.push((Event::Text(t.clone().into()), r.clone()));
        }
    }
    out
}

/// Math (`ENABLE_MATH`): a display formula that is a paragraph of its own —
/// `Start(Paragraph) DisplayMath End(Paragraph)` — stays an event for the
/// block parser to make a formula item of; every other math event (inline
/// `$x$`, a `$$…$$` run inside prose, a formula in a link or a cell) is
/// folded back into a text event spelled exactly as the source — delimiters
/// included — so the contiguity merge that follows, and every output, see
/// the text they always did (`costs $5 and $6` is one run, not three). Used
/// by both parsers.
pub(super) fn fold_math_into_text<'a>(
    raw: Vec<(Event<'a>, std::ops::Range<usize>)>,
    text: &'a str,
) -> Vec<(Event<'a>, std::ops::Range<usize>)> {
    let standalone = |k: usize| {
        matches!(
            raw.get(k.wrapping_sub(1)),
            Some((Event::Start(Tag::Paragraph), _))
        ) && matches!(raw.get(k + 1), Some((Event::End(TagEnd::Paragraph), _)))
    };
    raw.iter()
        .enumerate()
        .map(|(k, (event, range))| match event {
            Event::DisplayMath(_) if standalone(k) => (event.clone(), range.clone()),
            Event::InlineMath(_) | Event::DisplayMath(_) => (
                Event::Text(text.get(range.clone()).unwrap_or("").into()),
                range.clone(),
            ),
            _ => (event.clone(), range.clone()),
        })
        .collect()
}

/// Is this trimmed line a GFM table delimiter row (`--- | :---:` …)? Every
/// `|`-separated cell must be `-`s with optional edge colons, and at least one
/// pipe must be present (a bare `---` is a thematic break / setext underline).
fn is_delimiter_row(line: &str) -> bool {
    let line = line.trim();
    if !line.contains('|') {
        return false;
    }
    line.trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .all(|cell| {
            let dashes = cell.trim().trim_start_matches(':').trim_end_matches(':');
            !dashes.is_empty() && dashes.bytes().all(|b| b == b'-')
        })
}

/// GFM leaves a table row's edge pipes optional, but pulldown-cmark only
/// enters table mode on a leading one — a table written `Region | Q1` over a
/// `--- | ---` delimiter row came out as plain text (docling 2.122 reads it,
/// docling#3817). Detect that shape — a pipe-carrying line whose next line is
/// a delimiter row with the same cell count — and normalize the whole block's
/// edge pipes so pulldown parses it like the canonical spelling. Fenced code
/// is left untouched, and input with no such table borrows unchanged.
fn normalize_table_edge_pipes(text: &str) -> std::borrow::Cow<'_, str> {
    let cell_count = |line: &str| {
        line.trim()
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .count()
    };
    let is_fence = |line: &str| {
        let t = line.trim_start();
        t.starts_with("```") || t.starts_with("~~~")
    };

    let lines: Vec<&str> = text.split('\n').collect();
    let mut needs_fix = false;
    {
        let mut in_fence = false;
        for w in lines.windows(2) {
            let head = w[0].trim_end_matches('\r');
            if is_fence(head) {
                in_fence = !in_fence;
                continue;
            }
            let delim = w[1].trim_end_matches('\r');
            if !in_fence
                && head.contains('|')
                && !head.trim_start().starts_with('|')
                && is_delimiter_row(delim)
                && cell_count(head) == cell_count(delim)
            {
                needs_fix = true;
                break;
            }
        }
    }
    if !needs_fix {
        return std::borrow::Cow::Borrowed(text);
    }

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let head = lines[i].trim_end_matches('\r');
        if is_fence(head) {
            in_fence = !in_fence;
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let is_table_start = !in_fence
            && head.contains('|')
            && !head.trim_start().starts_with('|')
            && lines
                .get(i + 1)
                .is_some_and(|d| is_delimiter_row(d.trim_end_matches('\r')))
            && cell_count(head) == cell_count(lines[i + 1].trim_end_matches('\r'));
        if !is_table_start {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        // The block runs while lines keep carrying pipes; each gets both edge
        // pipes (rows already carrying one keep it).
        while i < lines.len() {
            let (body, cr) = match lines[i].strip_suffix('\r') {
                Some(b) => (b, "\r"),
                None => (lines[i], ""),
            };
            if !body.contains('|') || body.trim().is_empty() {
                break;
            }
            let t = body.trim();
            let mut fixed = String::with_capacity(t.len() + 4);
            if !t.starts_with('|') {
                fixed.push_str("| ");
            }
            fixed.push_str(t);
            if !t.ends_with('|') {
                fixed.push_str(" |");
            }
            fixed.push_str(cr);
            out.push(fixed);
            i += 1;
        }
    }
    std::borrow::Cow::Owned(out.join("\n"))
}

/// Whether the byte at `at` is a backslash escape's escaped character: an
/// odd number of backslashes right before it (an even run is escaped
/// backslashes, not an escape of what follows).
pub(super) fn escaped_at(text: &str, at: usize) -> bool {
    let run = text.as_bytes()[..at.min(text.len())]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count();
    run % 2 == 1
}

/// docling's `_strip_frontmatter` (docling#4596): the text after a leading
/// `---` … `---` (or `...`) block that is a YAML *mapping*, else the text
/// unchanged. As in Pandoc, an opening `---` followed by a blank line is a
/// thematic break, and an empty block is not a mapping. Upstream asks
/// `yaml.safe_load`; [`is_yaml_mapping`] is a conservative stand-in, so no
/// YAML parser enters the library for one test.
fn strip_frontmatter(text: &str) -> &str {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return text;
    };
    fn line_body(l: &str) -> &str {
        l.trim_end_matches('\n').trim_end_matches('\r')
    }
    if line_body(first).trim_end_matches([' ', '\t']) != "---" || !first.ends_with('\n') {
        return text;
    }
    let mut offset = first.len();
    let mut body_end = None;
    for (n, line) in lines.enumerate() {
        let content = line_body(line);
        if n == 0 && content.trim_matches([' ', '\t']).is_empty() {
            return text;
        }
        let fence = content.trim_end_matches([' ', '\t']);
        if fence == "---" || fence == "..." {
            body_end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let Some((body_end, end)) = body_end else {
        return text;
    };
    if is_yaml_mapping(&text[first.len()..body_end]) {
        &text[end..]
    } else {
        text
    }
}

/// Whether a frontmatter block reads as a YAML mapping: its first
/// significant line is a `key:` entry (or a `{…}` flow mapping), every line
/// at that indentation is another entry (or a `- ` item of the entry above,
/// which YAML allows unindented), deeper lines continue an entry, and the
/// flow brackets and quotes balance. A block of prose (`Just a sentence`,
/// a setext heading), a list or an unclosed `[` is not one — upstream's
/// `yaml.safe_load` gives a string, a list or an error for those. YAML that
/// this misjudges (a complex `?` key, tab indentation) keeps its text.
fn is_yaml_mapping(body: &str) -> bool {
    let key = cached_regex!(
        r#"^(?:"[^"]*"|'[^']*'|[^\s#\-?:,\[\]{}!&*|>'"%@`][^#]*?|-[^\s#][^#]*?)[ \t]*:(?:[ \t]|$)"#
    );
    let mut base: Option<usize> = None;
    let mut entries = 0usize;
    let (mut depth, mut quote) = (0i32, None::<char>);
    for raw in body.lines() {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim_start_matches(' ');
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with('\t') {
            return false;
        }
        let indent = line.len() - trimmed.len();
        match base {
            None => {
                if trimmed.starts_with('{') {
                    entries += 1;
                } else if !key.is_match(trimmed) {
                    return false;
                }
                base = Some(indent);
                entries += 1;
            }
            Some(b) if indent == b && depth == 0 && quote.is_none() => {
                let item = trimmed == "-" || trimmed.starts_with("- ");
                if !(key.is_match(trimmed) || item) {
                    return false;
                }
                entries += usize::from(!item);
            }
            Some(b) if indent < b => return false,
            Some(_) => {}
        }
        // Flow brackets and quotes, outside comments.
        let mut prev = ' ';
        for ch in trimmed.chars() {
            match quote {
                Some(q) if ch == q => quote = None,
                Some(_) => {}
                None => match ch {
                    '#' if prev == ' ' || prev == '\t' => break,
                    '"' | '\'' if matches!(prev, ' ' | ':' | '[' | '{' | ',') => quote = Some(ch),
                    '[' | '{' => depth += 1,
                    ']' | '}' => depth -= 1,
                    _ => {}
                },
            }
            if depth < 0 {
                return false;
            }
            prev = ch;
        }
        // A single-quoted or double-quoted scalar may span lines; a quote
        // still open at the block's end is an error (checked below).
    }
    entries > 0 && depth == 0 && quote.is_none()
}

impl DeclarativeBackend for MarkdownBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        // GFM makes a table row's edge pipes optional, but pulldown-cmark only
        // recognizes a table whose header starts with one. docling 2.122
        // (docling#3817) reads the pipe-less spelling; normalizing the edges
        // up front lets pulldown parse the same tables. `text` stays borrowed
        // (no allocation) when nothing needs normalizing — the common case.
        let text = source.text()?;
        // docling#4596 (#1022): a leading YAML frontmatter block is metadata,
        // not content — dropped before either walk parses the text.
        let text = strip_frontmatter(&text);
        let normalized = normalize_table_edge_pipes(text);
        let text = normalized.as_ref();
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_MATH);

        // Merge neighbouring text events into one run. pulldown splits both
        // `[11]` (a failed link) and `2\.` (an escape) into several text
        // events; the escape leaves an offset gap — the backslash. docling
        // merges marko's neighbouring `RawText` and escaped `Literal` nodes
        // into one run too (docling#4991, #1022: `snake\_case here` is one
        // text, not `snake` / `_` / `case here`), as it always did for a
        // table cell's plain text (docling#4313: `a\|b` is the cell `a|b`).
        let raw: Vec<(Event, std::ops::Range<usize>)> =
            Parser::new_ext(text, opts).into_offset_iter().collect();
        let raw = marko_strikethrough(raw, text);
        let raw = link_bare_urls(fold_math_into_text(raw, text), text);
        let mut events: Vec<Event> = Vec::with_capacity(raw.len());
        let mut k = 0;
        while k < raw.len() {
            if matches!(raw[k].0, Event::Text(_)) {
                let mut merged = String::new();
                let mut end = raw[k].1.start;
                while let Some((Event::Text(t), range)) = raw.get(k) {
                    let escape_gap = escaped_at(text, range.start) && range.start == end + 1;
                    if range.start != end && !escape_gap {
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

        // One resolver (and one per-document budget) for both walks; the
        // `None` tier keeps the no-op resolver, so the default path is the
        // pre-#646 one.
        let fs_resolver;
        let images: &dyn ImageResolver = if self.images.sources == ImageSources::None {
            &NoFetch
        } else {
            fs_resolver = FsImageResolver::new(
                source.base_dir().map(|p| p.to_path_buf()),
                source.base_url.clone(),
                self.images.clone(),
            );
            &fs_resolver
        };
        let mut doc = DoclingDocument::new(&source.name);
        let mut i = 0;
        self.parse_blocks(&events, &mut i, &mut doc.nodes, 0, Stop::Eof, 0, images);
        // The JSON serializes docling's item tree — marko's CommonMark view of
        // the document, walked as upstream does ([`super::md_tree`]); the flat
        // nodes above stay the source for every other serializer. A document
        // with a raw HTML block keeps the flat export (see the module docs).
        doc.tree = super::md_tree::build_tree(text, images);
        Ok(doc)
    }
}

/// What terminates a `parse_blocks` run.
#[derive(Clone, Copy, PartialEq)]
enum Stop {
    Eof,
    BlockQuote,
    /// The blocks of a list item after its text: up to its `End(Item)`,
    /// left for `parse_item` to consume.
    Item,
}

/// Deepest block nesting (block quotes, lists) the parser descends into —
/// markdown-it's `maxNesting`, which is what docling's Markdown backend runs
/// on. Deeper structure is skipped: `parse_blocks`/`parse_list`/`parse_item`
/// recurse per level, and 50 000 `>` (48 KB) or a 5 000-level list overflowed
/// the stack — an abort, not an error.
const MAX_NESTING: u16 = 100;

/// Skip the balanced event subtree that starts at the `Start` event under
/// `*i` (inclusive of its `End`), without recursion.
pub(super) fn skip_subtree(events: &[Event], i: &mut usize) {
    let mut depth = 0usize;
    while *i < events.len() {
        match &events[*i] {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    *i += 1;
                    return;
                }
            }
            _ => {}
        }
        *i += 1;
    }
}

impl MarkdownBackend {
    // -----------------------------------------------------------------------
    // Block structure
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn parse_blocks(
        &self,
        events: &[Event],
        i: &mut usize,
        out: &mut Vec<Node>,
        list_level: u8,
        stop: Stop,
        depth: u16,
        images: &dyn ImageResolver,
    ) {
        while *i < events.len() {
            match &events[*i] {
                Event::Start(Tag::BlockQuote(_) | Tag::List(_)) if depth >= MAX_NESTING => {
                    skip_subtree(events, i);
                }
                Event::End(TagEnd::BlockQuote(_)) if stop == Stop::BlockQuote => {
                    *i += 1;
                    return;
                }
                Event::End(TagEnd::Item) if stop == Stop::Item => return,
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
                    // docling parses embedded raw-HTML blocks; reuse the HTML
                    // backend, under the same image policy (#646).
                    super::html::append_fragment(&html, out, images);
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    *i += 1;
                    self.parse_blocks(
                        events,
                        i,
                        out,
                        list_level,
                        Stop::BlockQuote,
                        depth + 1,
                        images,
                    );
                }
                Event::Start(Tag::Paragraph) => {
                    // A paragraph that is one display formula (`$$…$$` on
                    // its own, one line or several) is a formula item — what
                    // a Jupyter notebook's or a README's equations are;
                    // Markdown prints it `$$latex$$` again, JSON a `formula`
                    // item, HTML MathML. Math inside prose stays text.
                    if let Some(Event::DisplayMath(latex)) = events.get(*i + 1) {
                        if matches!(events.get(*i + 2), Some(Event::End(TagEnd::Paragraph))) {
                            let latex = latex.trim().to_string();
                            if !latex.is_empty() {
                                out.push(Node::Formula {
                                    orig: latex.clone(),
                                    latex,
                                    location: None,
                                });
                            }
                            *i += 3;
                            continue;
                        }
                    }
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
                                    orig: None,
                                    pretty: None,
                                });
                            }
                        }
                        *i += 3;
                        continue;
                    }
                    *i += 1;
                    // A paragraph that is one image is a picture — the flat
                    // chain's counterpart of the tree's `TreeKind::Picture`
                    // (#760, docling#4777): its title the caption, its bytes
                    // when the policy resolves the destination (#646), its
                    // alt text a paragraph after it.
                    if let Some(nodes) = self.lone_image_picture(events, *i, images) {
                        skip_subtree(events, i); // the image
                        consume_end(events, i); // the paragraph
                        out.extend(nodes);
                        continue;
                    }
                    let runs = self.collect_inline(events, i, false);
                    consume_end(events, i);
                    let text = self.block_text(&runs);
                    if !text.is_empty() {
                        out.push(Node::Paragraph { text });
                    }
                }
                Event::Start(Tag::Heading { level, .. }) => {
                    let lvl = heading_level(*level);
                    *i += 1;
                    let runs = self.collect_inline(events, i, false);
                    consume_end(events, i);
                    let text = self.block_text(&runs);
                    if !text.is_empty() {
                        out.push(Node::Heading { level: lvl, text });
                    }
                }
                Event::Start(Tag::List(start)) => {
                    let ordered = start.is_some();
                    let start_num = start.unwrap_or(1);
                    *i += 1;
                    self.parse_list(
                        events,
                        i,
                        out,
                        list_level,
                        ordered,
                        start_num,
                        depth + 1,
                        images,
                    );
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
                        out.push(Node::Code {
                            language,
                            text,
                            orig: None,
                            pretty: None,
                        });
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

    #[allow(clippy::too_many_arguments)]
    fn parse_list(
        &self,
        events: &[Event],
        i: &mut usize,
        out: &mut Vec<Node>,
        level: u8,
        ordered: bool,
        start: u64,
        depth: u16,
        images: &dyn ImageResolver,
    ) {
        let mut number = start;
        let mut first = true;
        while *i < events.len() {
            match &events[*i] {
                Event::Start(Tag::Item) => {
                    *i += 1;
                    self.parse_item(events, i, out, level, ordered, number, first, depth, images);
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

    /// The nodes for a paragraph whose only content is an image at
    /// `events[start]`, as docling's `_iterate_elements` builds them (and
    /// keeps through its HTML round trip since docling#4777, #760): a picture
    /// captioned by the image's title — a caption item created ahead of it —
    /// carrying the bytes when `images` resolves the destination, then the
    /// alt text as a paragraph. `None` when the paragraph holds more than the
    /// image (it is parsed as text).
    fn lone_image_picture(
        &self,
        events: &[Event],
        start: usize,
        images: &dyn ImageResolver,
    ) -> Option<Vec<Node>> {
        let Some(Event::Start(Tag::Image {
            dest_url, title, ..
        })) = events.get(start)
        else {
            return None;
        };
        let mut end = start;
        skip_subtree(events, &mut end);
        if !matches!(events.get(end), Some(Event::End(TagEnd::Paragraph))) {
            return None;
        }
        let mut j = start + 1;
        let alt = self.block_text(&self.collect_inline(events, &mut j, false));
        let mut nodes = vec![Node::Picture {
            caption: Some(escape_text(title)).filter(|t| !t.is_empty()),
            caption_href: None,
            image: images.resolve(dest_url),
            classification: None,
            description: None,
            caption_parent: docling_core::CaptionParent::Container,
            caption_location: None,
        }];
        if !alt.is_empty() {
            nodes.push(Node::Paragraph { text: alt });
        }
        Some(nodes)
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
        depth: u16,
        images: &dyn ImageResolver,
    ) {
        let mut emitted = false;
        while *i < events.len() {
            match &events[*i] {
                Event::End(TagEnd::Item) => {
                    *i += 1;
                    break;
                }
                Event::Start(Tag::List(_) | Tag::BlockQuote(_)) if depth >= MAX_NESTING => {
                    skip_subtree(events, i);
                }
                Event::Start(Tag::List(start)) => {
                    let nested_ordered = start.is_some();
                    let nested_start = start.unwrap_or(1);
                    *i += 1;
                    self.parse_list(
                        events,
                        i,
                        out,
                        level + 1,
                        nested_ordered,
                        nested_start,
                        depth + 1,
                        images,
                    );
                }
                // The item's blocks after its text — another paragraph, a
                // code block, a quote, a table, raw HTML, a heading — follow
                // it as the blocks they are, as docling prints them below the
                // item line (they used to be dropped, all of them).
                Event::Start(Tag::Paragraph) if emitted => {
                    self.parse_blocks(events, i, out, level + 1, Stop::Item, depth + 1, images);
                }
                Event::Start(
                    Tag::CodeBlock(_)
                    | Tag::BlockQuote(_)
                    | Tag::Table(_)
                    | Tag::HtmlBlock
                    | Tag::Heading { .. },
                ) => {
                    self.parse_blocks(events, i, out, level + 1, Stop::Item, depth + 1, images);
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
                        self.block_text(&runs),
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
                        self.block_text(&runs),
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
            out.push(Node::Table(Table {
                footnotes: Vec::new(),
                rows,
                location: None,
                structure: None,
                cell_blocks: None,
                cells: None,
                caption: None,
                caption_parent: Default::default(),
                caption_location: None,
                document_index: false,
            }));
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
        // docling#4019 (2.126): a line break inside a paragraph, heading or
        // list item is kept. A hard break joins the runs around it with `\n`
        // (GFM `  \n` once serialized), a soft one with a space; two runs of
        // the same formatting merge into one item across the break
        // (`**Bold A**` ⏎ `**Bold B**` → `**Bold A Bold B**`), differently
        // formatted ones stay separate items, the second carrying the `\n`.
        let mut pending: Option<Break> = None;
        while *i < events.len() {
            match &events[*i] {
                Event::Text(t) => {
                    let before = runs.len();
                    self.push_text(t, table, &mut runs);
                    if runs.len() > before {
                        self.apply_break(&mut runs, pending.take(), table);
                    }
                    *i += 1;
                }
                Event::SoftBreak | Event::HardBreak => {
                    let hard = matches!(events[*i], Event::HardBreak);
                    if table {
                        // A cell is one line: docling reads the raw row, where
                        // the break is whitespace (the legacy join's space).
                    } else if self.strict {
                        // Strict concatenates, so the separator is explicit.
                        runs.push(if hard { "\n" } else { " " }.to_string());
                    } else {
                        pending = Some(if hard { Break::Hard } else { Break::Soft });
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
                    runs.push(docling_core::code_span(&content));
                    *i += 1;
                }
                Event::Start(Tag::Emphasis) => {
                    runs.push(self.wrap_inline(events, i, table, "*"));
                    self.apply_break(&mut runs, pending.take(), table);
                }
                Event::Start(Tag::Strong) => {
                    runs.push(self.wrap_inline(events, i, table, "**"));
                    self.apply_break(&mut runs, pending.take(), table);
                }
                Event::Start(Tag::Strikethrough) => {
                    runs.push(self.wrap_inline(events, i, table, "~~"));
                    self.apply_break(&mut runs, pending.take(), table);
                }
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    ..
                }) => {
                    let url = link_dest(*link_type, dest_url);
                    *i += 1;
                    let inner = self.join_runs(&self.collect_inline(events, i, table), table);
                    consume_end(events, i);
                    // A table cell keeps its links too (docling 2.140 prints
                    // `[label](url)` in a cell); edge whitespace is hoisted.
                    runs.push(docling_core::inline::hoist(&inner, |core| {
                        docling_core::hyperlink::markdown_link(core, &url)
                    }));
                }
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    let url = dest_url.to_string();
                    *i += 1;
                    let alt = self.join_runs(&self.collect_inline(events, i, table), table);
                    consume_end(events, i);
                    runs.push(if table {
                        // A picture in a table cell (#760, docling#4777):
                        // docling's rich cell prints it with its title
                        // caption ahead of the placeholder; the alt text is
                        // not part of it.
                        if title.is_empty() {
                            "<!-- image -->".to_string()
                        } else {
                            format!("{title}  <!-- image -->")
                        }
                    } else {
                        format!("![{alt}]({url})")
                    });
                }
                _ => break,
            }
        }
        runs
    }

    /// Resolve a pending line break once the run after it has been pushed
    /// (legacy, non-table): the last two runs merge when they share their
    /// formatting — both plain, or both wrapped in the same marker — with
    /// `\n` (hard) or a space (soft) between their texts; otherwise a hard
    /// break prefixes the new run with `\n` and a soft one is the join's space.
    fn apply_break(&self, runs: &mut Vec<String>, pending: Option<Break>, table: bool) {
        let Some(brk) = pending else {
            return;
        };
        if table || self.strict || runs.len() < 2 {
            return;
        }
        let new = runs.pop().expect("two runs");
        let prev = runs.pop().expect("two runs");
        let sep = match brk {
            Break::Hard => "\n",
            Break::Soft => " ",
        };
        let marker_of = |r: &str| -> Option<&'static str> {
            ["**", "~~", "*"]
                .into_iter()
                .find(|m| r.len() > 2 * m.len() && r.starts_with(m) && r.ends_with(m))
        };
        let plain = |r: &str| !r.starts_with(['*', '~', '`', '[', '!']);
        match (marker_of(&prev), marker_of(&new)) {
            (None, None) if plain(&prev) && plain(&new) => {
                runs.push(format!("{prev}{sep}{new}"));
            }
            (Some(m), Some(n)) if m == n => {
                let inner_prev = &prev[m.len()..prev.len() - m.len()];
                let inner_new = &new[m.len()..new.len() - m.len()];
                runs.push(format!("{m}{inner_prev}{sep}{inner_new}{m}"));
            }
            _ => {
                // The break still separates runs that cannot merge: a
                // newline, or a space for a soft break (docling#3891).
                runs.push(prev);
                runs.push(match brk {
                    Break::Hard => format!("\n{new}"),
                    Break::Soft => format!(" {new}"),
                });
            }
        }
    }

    fn wrap_inline(&self, events: &[Event], i: &mut usize, table: bool, marker: &str) -> String {
        *i += 1;
        let inner = self.join_runs(&self.collect_inline(events, i, table), table);
        consume_end(events, i);
        // A table cell keeps its formatting too (docling#4488, 2.135). The
        // runs' edge whitespace goes outside the markers (docling-core's
        // `hoist_decoration_whitespace`, #997): `**bold** tail`.
        docling_core::inline::hoist(&inner, |core| format!("{marker}{core}{marker}"))
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
        } else if !text.is_empty() {
            // The raw fragment, its edge whitespace kept: that is the run
            // boundary docling's runs carry since docling#3891 (`Foo
            // *emph* bar` → `"Foo "`, `"emph"`, `" bar"`), and a
            // whitespace-only fragment between runs is significant. Block
            // edges are trimmed by [`Self::block_text`].
            let (lead, core, trail) = docling_core::inline::split_edges(text);
            runs.push(format!(
                "{lead}{}{trail}",
                escape_html(&escape_underscores(core))
            ));
        }
    }

    /// A block's runs joined, its outer edges trimmed (the runs keep the
    /// interior boundaries).
    fn block_text(&self, runs: &[String]) -> String {
        let text = self.join_runs(runs, false);
        if self.strict {
            text
        } else {
            text.trim().to_string()
        }
    }

    /// Join runs. Table cells drop empty runs and separate with single
    /// spaces; elsewhere the runs are concatenated verbatim, preserving the
    /// source spacing carried in the text runs.
    fn join_runs(&self, runs: &[String], table: bool) -> String {
        // Outside a table both modes concatenate: legacy runs carry their
        // boundary whitespace too since docling#3891 (#997).
        if !table {
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
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        *emitted = true;
    }
}

fn consume_end(events: &[Event], i: &mut usize) {
    if matches!(events.get(*i), Some(Event::End(_))) {
        *i += 1;
    }
}

/// A line break waiting for the run that follows it (see `collect_inline`).
#[derive(Clone, Copy)]
enum Break {
    Soft,
    Hard,
}

/// A link's target: an e-mail autolink (`<me@x.y>`, a bare address) is a
/// `mailto:` link, as marko's `AutoLink` makes it.
pub(super) fn link_dest(link_type: pulldown_cmark::LinkType, dest: &str) -> String {
    match link_type {
        pulldown_cmark::LinkType::Email => format!("mailto:{dest}"),
        _ => dest.to_string(),
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
pub(super) fn unescape_entities(text: &str) -> String {
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
            } else {
                entity.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    /// docling#4318: the space around inline emphasis inside a table cell
    /// belongs to the cell (`**C** Cadre` is `C Cadre`, not `CCadre`).
    #[test]
    fn table_cells_keep_the_space_around_inline_emphasis() {
        let doc = convert(
            "| H | I |\n|---|---|\n| **C** Cadre | x |\n| foo **bar** | y |\n| **A** **B** | z |\n| *italic* and **bold** | w |\n",
        );
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let cells: Vec<String> = json["tables"][0]["data"]["table_cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            cells,
            [
                "H",
                "I",
                "C Cadre",
                "x",
                "foo bar",
                "y",
                "A B",
                "z",
                "italic and bold",
                "w"
            ]
        );
    }

    fn convert(md: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Md, md.as_bytes().to_vec());
        MarkdownBackend {
            strict: false,
            images: Default::default(),
        }
        .convert(&src)
        .unwrap()
    }

    fn convert_strict(md: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t", InputFormat::Md, md.as_bytes().to_vec());
        let mut doc = MarkdownBackend {
            strict: true,
            images: Default::default(),
        }
        .convert(&src)
        .unwrap();
        doc.strict_markdown = true;
        doc
    }

    /// The JSON's text items as `(label, text)` and its group labels.
    fn json_texts(md: &str) -> (Vec<(String, String)>, Vec<String>) {
        let json = convert(md).export_to_json_value();
        let texts = json["texts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let s = |k: &str| t[k].as_str().unwrap_or_default().to_string();
                (s("label"), s("text"))
            })
            .collect();
        let groups = json["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["label"].as_str().unwrap().to_string())
            .collect();
        (texts, groups)
    }

    /// #1022, docling#4991 (`test_backslash_escape_stays_in_its_text_run`):
    /// a backslash escape is part of the text run around it, not an item of
    /// its own, and an escape protects the entity around it.
    #[test]
    fn a_backslash_escape_stays_in_its_text_run() {
        let cases: &[(&str, &[(&str, &str)])] = &[
            (r"snake\_case here", &[("text", "snake_case here")]),
            (
                r"Price is \$5 and 50\% off",
                &[("text", "Price is $5 and 50% off")],
            ),
            (r"\*not emphasis\*", &[("text", "*not emphasis*")]),
            (r"1\. not a list", &[("text", "1. not a list")]),
            (r"- item with \* star", &[("list_item", "item with * star")]),
            (r"# Head \# x", &[("title", "Head # x")]),
            (r"> quote \* star", &[("text", "quote * star")]),
            (
                r"Use `x` and snake\_case",
                &[("text", "Use "), ("code", "x"), ("text", " and snake_case")],
            ),
            (
                r"**bold \* star** plain",
                &[("text", "bold * star"), ("text", " plain")],
            ),
            (r"a \&amp; b", &[("text", "a &amp; b")]),
            (r"&\#35;x", &[("text", "&#35;x")]),
            (r"&amp\; e", &[("text", "&amp; e")]),
            (r"&\#169; vs &#169;", &[("text", "&#169; vs ©")]),
            (r"R&D &amp; \_x &#169;", &[("text", "R&D & _x ©")]),
        ];
        for (md, expected) in cases {
            let (texts, groups) = json_texts(md);
            let want: Vec<(String, String)> = expected
                .iter()
                .map(|(l, t)| (l.to_string(), t.to_string()))
                .collect();
            assert_eq!(texts, want, "{md}");
            // One run of text is one item: no inline group wraps it.
            if expected.len() == 1 {
                assert!(!groups.iter().any(|g| g == "inline"), "{md}: {groups:?}");
            }
        }
    }

    /// docling#4991: an escape inside link text stays in the one text item
    /// that carries the link; inside a rich table cell the runs are `z` and
    /// `* w`, not `z`, `*` and `w`.
    #[test]
    fn a_backslash_escape_in_a_link_or_rich_cell_stays_in_its_run() {
        let json = convert(r"[a\_b](http://x.y)").export_to_json_value();
        let texts = json["texts"].as_array().unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0]["text"], "a_b");
        assert!(texts[0]["hyperlink"]
            .as_str()
            .unwrap()
            .starts_with("http://x.y"));

        let (texts, _) = json_texts("| h |\n|---|\n| **z**\\* w |");
        let texts: Vec<&str> = texts.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, ["z", "* w"]);
    }

    /// #1022, docling#4596 (`test_convert_strips_leading_frontmatter`): a
    /// leading `---` block that is a YAML mapping is dropped; a `---`
    /// followed by a blank line stays a thematic break, and a block that is
    /// not a mapping stays content.
    #[test]
    fn a_leading_yaml_frontmatter_is_dropped() {
        let cases = [
            ("---\ntitle: T\n---\n\n# H\n\nbody\n", "# H\n\nbody"),
            (
                "---\r\ntitle: T\r\n---\r\n\r\n# H\r\n\r\nbody\r\n",
                "# H\n\nbody",
            ),
            (
                "---\ntitle: T\n\ntags: [a, b]\n---\n\n# H\n\nbody\n",
                "# H\n\nbody",
            ),
            ("---\ntitle: T\n...\n\n# H\n\nbody\n", "# H\n\nbody"),
            ("---\n\n# H\n\nbody\n", "# H\n\nbody"),
            (
                "---\n\nNote: this is important.\n\n---\n\nbody\n",
                "Note: this is important.\n\nbody",
            ),
            (
                "---\nJust a sentence\n---\n\nbody\n",
                "## Just a sentence\n\nbody",
            ),
            ("---\n---\n\n# H\n\nbody\n", "# H\n\nbody"),
            ("---\ntitle: T\n---", ""),
            ("---   \ntitle: T\n---  \n\n# H\n\nbody\n", "# H\n\nbody"),
        ];
        for (md, expected) in cases {
            let out = convert(md).export_to_markdown();
            assert_eq!(out.trim_end(), expected, "{md:?}");
        }
        // Frontmatter directly followed by content keeps the content
        // (`test_convert_frontmatter_followed_by_content_keeps_the_content`).
        let out = convert("---\ntitle: T\n---\nIntro\nSection\n---\n\nbody\n").export_to_markdown();
        assert!(!out.contains("title: T"), "{out}");
        for word in ["Intro", "Section", "body"] {
            assert!(out.contains(word), "{out}");
        }
    }

    /// The mapping test behind [`strip_frontmatter`]: what `yaml.safe_load`
    /// gives a dict for, and what it does not.
    #[test]
    fn frontmatter_must_be_a_yaml_mapping() {
        for yes in [
            "title: T\n",
            "title: \"a: b\"\ntags:\n  - x\n  - y\n",
            "tags:\n- x\n- y\nauthor: me # comment\n",
            "{a: 1, b: [2, 3]}\n",
            "# a comment\nkey: |\n  block\n  text\n",
        ] {
            assert!(is_yaml_mapping(yes), "{yes:?}");
        }
        for no in [
            "",
            "Just a sentence\n",
            "- a\n- b\n",
            "title: [unclosed\n",
            "title: T\nnot an entry\n",
            "\ttitle: T\n",
        ] {
            assert!(!is_yaml_mapping(no), "{no:?}");
        }
        // Not at the very start: content, not frontmatter.
        assert_eq!(
            strip_frontmatter("x\n---\na: 1\n---\n"),
            "x\n---\na: 1\n---\n"
        );
        assert_eq!(strip_frontmatter("---\na: 1\n---\nrest"), "rest");
    }

    /// #1040: a raw HTML block sends the document through the HTML backend
    /// (upstream's detour); each following paragraph's inline group stays
    /// its own — upstream's `<span class='inline-group'>`s parsed back as
    /// one paragraph, merging `**2.1** First.` and `**2.2** Second.`.
    #[test]
    fn paragraphs_after_a_raw_html_block_keep_their_own_inline_groups() {
        for prefix in [
            "<!-- note -->\n\n",
            "<!--\nmulti\n-->\n\n",
            "<div>hi</div>\n\n",
            "",
        ] {
            let md = format!("{prefix}## 2. PURPOSE\n\n**2.1** First.\n\n**2.2** Second.\n");
            let json = convert(&md).export_to_json_value();
            let sizes: Vec<usize> = json["groups"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|g| g["label"] == "inline")
                .map(|g| g["children"].as_array().unwrap().len())
                .collect();
            assert_eq!(sizes, [2, 2], "{prefix:?}");
            let out = convert(&md).export_to_markdown();
            assert!(out.contains("First.\n\n**2.2**"), "{prefix:?}: {out}");
        }
    }

    /// Block nesting is capped at markdown-it's `maxNesting` (100): 50 000
    /// `>` or a 5 000-level list used to recurse the parser off the stack.
    /// The document still converts, with the content past the cap dropped.
    #[test]
    fn pathological_nesting_converts_instead_of_overflowing() {
        let quotes = ">".repeat(50_000) + " deep\n\nafter\n";
        let md = convert(&quotes).export_to_markdown();
        assert!(md.contains("after"), "{md:?}");
        let list: String = (0..5_000)
            .map(|i| format!("{}- item {i}\n", "  ".repeat(i)))
            .collect();
        let md = convert(&list).export_to_markdown();
        assert!(md.contains("item 0") && md.contains("item 99"), "{md:?}");
        // Within the cap every level is kept.
        let shallow: String = (0..20)
            .map(|i| format!("{}- item {i}\n", "  ".repeat(i)))
            .collect();
        let md = convert(&shallow).export_to_markdown();
        assert!(md.contains("item 19"), "{md:?}");
    }

    #[test]
    fn inline_runs_keep_their_own_spacing_in_legacy() {
        // docling#3891 (#997): the runs carry their whitespace, no invented
        // space before the period.
        let doc = convert("Foo *emphasis* **strong** ***both***.\n");
        assert_eq!(
            doc.export_to_markdown(),
            "Foo *emphasis* **strong** ***both***.\n"
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

    /// `$$…$$` as a paragraph of its own is a formula item; inline math,
    /// a `$$…$$` inside prose or a link, and a price are the text they were.
    #[test]
    fn display_math_is_a_formula_and_inline_math_stays_text() {
        let doc = convert(
            "Costs $5 and $6, with $x^2$ inline and [$$E=mc^2$$](https://e.org) linked.\n\n\
             $$\n H(y) = -\\sum_i y_i \\log y_i \n$$\n\n\
             $$ a + b $$\n\n- item with $$c$$ inside\n",
        );
        let nodes: Vec<String> = doc
            .nodes
            .iter()
            .map(|n| match n {
                Node::Paragraph { text } => format!("P:{text}"),
                Node::Formula { latex, orig, .. } => format!("F:{latex}|{orig}"),
                Node::ListItem { text, .. } => format!("L:{text}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            nodes,
            [
                "P:Costs $5 and $6, with $x^2$ inline and [$$E=mc^2$$](https://e.org) linked.",
                "F:H(y) = -\\sum_i y_i \\log y_i|H(y) = -\\sum_i y_i \\log y_i",
                "F:a + b|a + b",
                "L:item with $$c$$ inside",
            ]
        );
        let md = doc.export_to_markdown();
        assert!(md.contains("$$H(y) = -\\sum_i y_i \\log y_i$$"), "{md}");
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

    /// A list item's blocks after its text — a second paragraph, a code
    /// block, a quote, a table — follow the item line, as docling prints
    /// them; they were all dropped. The list goes on numbering after them.
    #[test]
    fn list_item_blocks_after_its_text_are_kept() {
        let doc = convert(
            "1. one\n\n   extra para\n\n2. two\n\n   ```\n   code\n   ```\n\n\
             3. three\n\n   > quoted\n\n4. four\n\n   | a | b |\n   |---|---|\n   | 1 | 2 |\n",
        );
        assert_eq!(
            doc.export_to_markdown(),
            "1. one\n\nextra para\n\n2. two\n\n```\ncode\n```\n\n3. three\n\nquoted\n\n\
             4. four\n\n|   a |   b |\n|-----|-----|\n|   1 |   2 |\n"
        );
    }

    /// A cell keeps its inline formatting, as docling prints it since 2.135
    /// (docling#4488) — it used to be flattened to plain text.
    #[test]
    fn parses_github_table_formatted_cells() {
        let doc = convert("| **A** | B |\n|---|---|\n| x `y` | ~~z~~ |\n");
        assert_eq!(
            doc.export_to_markdown(),
            "| **A**   | B     |\n|---------|-------|\n| x `y`   | ~~z~~ |\n"
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
                orig: None,
                pretty: None,
            }]
        );
    }
}
