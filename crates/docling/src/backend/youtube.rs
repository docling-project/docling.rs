//! A YouTube watch URL as a document (#681) — MarkItDown's
//! `_youtube_converter`: the video's title, metadata and description from
//! the watch page, then its caption transcript.
//!
//! The page is what the URL fetch (#668) already brings back; this module
//! reads its `ytInitialPlayerResponse` (the player's JSON, which names the
//! caption tracks and the video's details), picks a track for the asked
//! language, fetches it from YouTube's `timedtext` endpoint as WebVTT and
//! hands the cues to the WebVTT backend — so the transcript is timed items
//! (`--to vtt` writes them back as cues), not one joined line. Nothing but
//! the captions is downloaded: no video, no audio, no playlist.
//!
//! The document follows MarkItDown's shape: `# YouTube`, `## <title>`, a
//! `### Video Metadata` list (views, keywords, runtime), `### Description`,
//! `### Transcript`. A page listing no caption track is an error naming the
//! video — there is nothing to transcribe (MarkItDown falls back to the
//! page's metadata alone; `youtube-transcript-api`'s own
//! `TranscriptsDisabled`). The language comes from the converter's
//! `asr_lang` (`--asr-lang`): `auto` / unset prefers English, then the first
//! track, a manually written track over an auto-generated one; a language
//! the video has no track for is translated by YouTube when it offers it
//! (`tlang`, as MarkItDown does), otherwise the error lists what there is.
//!
//! Routing is by the source's URL, as MarkItDown's `accepts` does: HTML
//! whose `base_url` is a YouTube watch / `youtu.be` / shorts / embed / live
//! URL goes here ([`crate::DocumentConverter`] checks [`video_id`]); a saved
//! YouTube page with no URL converts as HTML like any other page.

use docling_core::tree::{ItemTree, ListMeta, TreeKind};
use docling_core::{DoclingDocument, Node};

use crate::backend::markdown::{escape_text, unescape_entities};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::fetch::FetchPolicy;
use crate::source::SourceDocument;

/// The video id a YouTube URL names — `youtube.com/watch?v=ID` (any
/// subdomain), `youtu.be/ID`, `youtube.com/shorts/ID`, `/embed/ID`,
/// `/live/ID` — or `None` for any other URL (MarkItDown's `_get_video_id`,
/// plus `live`). The id is the 11 characters YouTube uses; anything else in
/// its place is not a video.
pub fn video_id(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host
        .split('@')
        .next_back()?
        .split(':')
        .next()?
        .to_ascii_lowercase();
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let path = path.split('#').next().unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let id = if host == "youtu.be" || host == "www.youtu.be" {
        segments.first().copied()
    } else if host == "youtube.com" || host.ends_with(".youtube.com") {
        match segments.first().copied() {
            Some("watch") => query
                .split('&')
                .find_map(|kv| kv.strip_prefix("v="))
                .map(|v| v.split('#').next().unwrap_or(v)),
            Some("shorts" | "embed" | "live") => segments.get(1).copied(),
            _ => None,
        }
    } else {
        None
    };
    id.filter(|id| {
        id.len() == 11
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
    .map(str::to_string)
}

/// One caption track the player lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionTrack {
    /// The `timedtext` URL (without a format); `&fmt=vtt` makes it WebVTT.
    pub base_url: String,
    /// BCP 47 code (`en`, `en-GB`, `de`).
    pub language: String,
    /// The track's display name (`English`, `English (auto-generated)`).
    pub name: Option<String>,
    /// Auto-generated speech recognition (`kind: "asr"`) rather than written.
    pub generated: bool,
    /// Whether YouTube will translate it on the fly (`tlang`).
    pub translatable: bool,
}

/// What a watch page says about its video — parsed, nothing fetched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchPage {
    pub video_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    /// The view count as the page prints it.
    pub views: Option<String>,
    pub keywords: Vec<String>,
    /// ISO 8601 duration (`PT1M5S`).
    pub duration: Option<String>,
    pub tracks: Vec<CaptionTrack>,
    /// Languages YouTube offers to translate a translatable track into.
    pub translation_languages: Vec<String>,
}

/// Read a watch page: `ytInitialPlayerResponse` first (`videoDetails`, the
/// caption tracks), the `<meta>` tags and `<title>` where it is silent,
/// `ytInitialData`'s `attributedDescriptionBodyText` for the description
/// the way MarkItDown reads it.
pub fn parse_watch_page(html: &str) -> WatchPage {
    let mut page = WatchPage::default();
    let metas = meta_tags(html);
    let meta = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| {
                metas
                    .iter()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.clone())
            })
            .filter(|v| !v.is_empty())
    };

    if let Some(player) = script_json(html, "ytInitialPlayerResponse") {
        let details = &player["videoDetails"];
        let text = |v: &serde_json::Value| v.as_str().map(str::to_string).filter(|s| !s.is_empty());
        page.video_id = text(&details["videoId"]);
        page.title = text(&details["title"]);
        page.description = text(&details["shortDescription"]);
        page.author = text(&details["author"]);
        page.views = text(&details["viewCount"]);
        if let Some(words) = details["keywords"].as_array() {
            page.keywords = words.iter().filter_map(&text).collect();
        }
        if let Some(seconds) = details["lengthSeconds"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
        {
            page.duration = Some(iso_duration(seconds));
        }
        let renderer = &player["captions"]["playerCaptionsTracklistRenderer"];
        if let Some(tracks) = renderer["captionTracks"].as_array() {
            for t in tracks {
                let (Some(base_url), Some(language)) =
                    (text(&t["baseUrl"]), text(&t["languageCode"]))
                else {
                    continue;
                };
                page.tracks.push(CaptionTrack {
                    base_url,
                    language,
                    name: text(&t["name"]["simpleText"])
                        .or_else(|| text(&t["name"]["runs"][0]["text"])),
                    generated: t["kind"].as_str() == Some("asr"),
                    translatable: t["isTranslatable"].as_bool().unwrap_or(false),
                });
            }
        }
        if let Some(langs) = renderer["translationLanguages"].as_array() {
            page.translation_languages = langs
                .iter()
                .filter_map(|l| text(&l["languageCode"]))
                .collect();
        }
    }

    if page.title.is_none() {
        page.title = meta(&["og:title", "name", "title"]).or_else(|| {
            html_title(html).map(|t| {
                t.strip_suffix(" - YouTube")
                    .unwrap_or(&t)
                    .trim()
                    .to_string()
            })
        });
    }
    if page.description.is_none() {
        page.description = script_json(html, "ytInitialData")
            .and_then(|data| {
                find_key(&data, "attributedDescriptionBodyText")
                    .and_then(|d| d["content"].as_str().map(str::to_string))
            })
            .filter(|d| !d.is_empty())
            .or_else(|| meta(&["description", "og:description"]));
    }
    if page.views.is_none() {
        page.views = meta(&["interactionCount"]);
    }
    if page.keywords.is_empty() {
        if let Some(words) = meta(&["keywords"]) {
            page.keywords = words
                .split(',')
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    if page.duration.is_none() {
        page.duration = meta(&["duration"]);
    }
    page
}

/// The track to transcribe for `lang` (a BCP 47 code, or `auto` / `None`),
/// and the language YouTube should translate it into when the video has no
/// track in `lang` but offers the translation. A written track beats an
/// auto-generated one of the same language; `en-GB` answers for `en`.
pub fn pick_track<'a>(
    page: &'a WatchPage,
    lang: Option<&str>,
) -> Result<(&'a CaptionTrack, Option<String>), ConversionError> {
    let video = page.video_id.as_deref().unwrap_or("?");
    if page.tracks.is_empty() {
        return Err(ConversionError::Parse(format!(
            "youtube: video {video} has no captions — there is no transcript to convert"
        )));
    }
    let in_language = |code: &str| {
        let code = code.to_ascii_lowercase();
        let mut found: Vec<&CaptionTrack> = page
            .tracks
            .iter()
            .filter(|t| {
                let l = t.language.to_ascii_lowercase();
                l == code || l.starts_with(&format!("{code}-"))
            })
            .collect();
        // Written captions first; the player lists them before `asr` too.
        found.sort_by_key(|t| t.generated);
        found.first().copied()
    };
    let wanted = lang
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("auto"));
    let Some(wanted) = wanted else {
        // MarkItDown: English, else the first track the video lists.
        let first = &page.tracks[0];
        return Ok((
            in_language("en")
                .or_else(|| in_language(&first.language))
                .unwrap_or(first),
            None,
        ));
    };
    if let Some(track) = in_language(wanted) {
        return Ok((track, None));
    }
    let offered = page
        .translation_languages
        .iter()
        .any(|l| l.eq_ignore_ascii_case(wanted));
    if offered {
        if let Some(source) = page
            .tracks
            .iter()
            .filter(|t| t.translatable)
            .min_by_key(|t| (t.generated, !t.language.eq_ignore_ascii_case("en")))
        {
            return Ok((source, Some(wanted.to_string())));
        }
    }
    let mut available: Vec<&str> = page.tracks.iter().map(|t| t.language.as_str()).collect();
    available.dedup();
    Err(ConversionError::Parse(format!(
        "youtube: video {video} has no {wanted} captions (available: {}); pass one of them as the language",
        available.join(", ")
    )))
}

/// The transcript URL for a track: its `baseUrl` as WebVTT, translated when
/// `tlang` is set.
pub fn transcript_url(track: &CaptionTrack, tlang: Option<&str>) -> String {
    let sep = if track.base_url.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut url = format!("{}{sep}fmt=vtt", track.base_url);
    if let Some(t) = tlang {
        url.push_str("&tlang=");
        url.push_str(t);
    }
    url
}

/// The document for a watch page and its WebVTT transcript (`None` when the
/// transcript was not fetched — the metadata alone). MarkItDown's shape,
/// with the cues as timed items.
pub fn transcript_document(name: &str, page: &WatchPage, vtt: Option<&str>) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    let mut tree = ItemTree::default();
    heading(&mut doc, &mut tree, 1, "YouTube");
    if let Some(title) = &page.title {
        heading(&mut doc, &mut tree, 2, title);
    }
    let mut stats: Vec<(&str, String)> = Vec::new();
    if let Some(views) = &page.views {
        stats.push(("Views", views.clone()));
    }
    if !page.keywords.is_empty() {
        stats.push(("Keywords", page.keywords.join(", ")));
    }
    if let Some(duration) = &page.duration {
        stats.push(("Runtime", duration.clone()));
    }
    if !stats.is_empty() {
        heading(&mut doc, &mut tree, 3, "Video Metadata");
        let group = tree.add(
            None,
            None,
            TreeKind::Group {
                label: "list".into(),
                name: "list".into(),
            },
        );
        for (i, (key, value)) in stats.iter().enumerate() {
            let text = format!("**{key}:** {}", escape_text(value));
            doc.push(Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: i == 0,
                text: text.clone(),
                level: 0,
                marker: None,
                location: None,
                dclx: None,
                href: None,
                layer: None,
            });
            tree.add(
                Some(group),
                None,
                // The same Markdown markup as the flat item, so the item
                // tree's serializers (the Python package exports from it)
                // print MarkItDown's `- **Views:** 1234` too.
                TreeKind::Text {
                    label: "list_item".into(),
                    text: text.clone(),
                    orig: None,
                    formatting: None,
                    hyperlink: None,
                    level: None,
                    list: Some(ListMeta {
                        enumerated: false,
                        marker: String::new(),
                    }),
                },
            );
        }
    }
    if let Some(description) = &page.description {
        heading(&mut doc, &mut tree, 3, "Description");
        for para in description
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            doc.push(Node::Paragraph {
                text: escape_text(para),
            });
            tree.add(
                None,
                None,
                TreeKind::Text {
                    label: "text".into(),
                    text: para.into(),
                    orig: None,
                    formatting: None,
                    hyperlink: None,
                    level: None,
                    list: None,
                },
            );
        }
    }
    if let Some(vtt) = vtt {
        heading(&mut doc, &mut tree, 3, "Transcript");
        let content = vtt.replace("\r\n", "\n").replace('\r', "\n");
        let body = content.split_once('\n').map_or("", |(_, body)| body);
        let body = dedupe_rolling_cues(body);
        crate::backend::webvtt::append_cues(&mut doc, &mut tree, &body);
    }
    // The cues' runs keep their own whitespace (schema 1.11.0, #997).
    tree.inline_whitespace = true;
    doc.tree = Some(tree);
    doc.metadata.title = page.title.clone();
    doc.metadata.description = page.description.clone();
    doc.metadata.keywords = page.keywords.clone();
    doc.metadata.authors = page.author.iter().cloned().collect();
    doc
}

/// A heading on both the flat nodes and the tree: level 1 is the `title`,
/// deeper ones `section_header`s a level down (docling's numbering).
fn heading(doc: &mut DoclingDocument, tree: &mut ItemTree, level: u8, text: &str) {
    doc.push(Node::Heading {
        level,
        text: escape_text(text),
    });
    let (label, lvl) = if level == 1 {
        ("title", None)
    } else {
        ("section_header", Some(level - 1))
    };
    tree.add(
        None,
        None,
        TreeKind::Text {
            label: label.into(),
            text: text.into(),
            orig: None,
            formatting: None,
            hyperlink: None,
            level: lvl,
            list: None,
        },
    );
}

/// YouTube's auto-generated WebVTT rolls: every cue repeats the previous
/// cue's line before its own, interleaves 10 ms cues holding only that
/// repeat, and times each word with `<00:00:00.799><c> word</c>` tags. Keep
/// each line once — in the cue it first appears — drop the cues left empty,
/// and drop the word tags (the WebVTT backend would read them as spans and
/// join the words with extra spaces), so the transcript reads straight
/// through. A written track is untouched: no line repeats the one before
/// it, and its `<b>` / `<i>` / `<v>` spans are not word tags.
pub fn dedupe_rolling_cues(body: &str) -> String {
    let mut out = String::new();
    let mut last_line: Option<String> = None;
    for block in body.split("\n\n") {
        let block = block.trim_matches('\n');
        if block.is_empty() {
            continue;
        }
        let Some(timing_at) = block.lines().position(|l| l.contains("-->")) else {
            out.push_str(block);
            out.push_str("\n\n");
            continue;
        };
        let mut lines = block.lines();
        let head: Vec<&str> = lines.by_ref().take(timing_at + 1).collect();
        let mut kept: Vec<String> = Vec::new();
        for line in lines {
            let line = strip_word_tags(line);
            let key = line.split_whitespace().collect::<Vec<_>>().join(" ");
            if key.is_empty() || last_line.as_deref() == Some(&key) {
                continue;
            }
            kept.push(key.clone());
            last_line = Some(key);
        }
        if kept.is_empty() {
            continue;
        }
        out.push_str(&head.join("\n"));
        out.push('\n');
        out.push_str(&kept.join("\n"));
        out.push_str("\n\n");
    }
    out
}

/// A cue line without YouTube's word-timing tags — `<00:00:00.799>` and the
/// `<c>` / `</c>` (or `<c.colorE5E5E5>`) spans around each word. Other tags
/// stay.
fn strip_word_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &after[..close];
        let word_tag = inner.starts_with(|c: char| c.is_ascii_digit())
            || inner == "c"
            || inner.starts_with("c.")
            || inner == "/c";
        if !word_tag {
            out.push_str(&rest[open..open + 1 + close + 1]);
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The backend the converter routes a YouTube page to: the page's bytes are
/// the source, the transcript is fetched here.
pub struct YoutubeBackend {
    /// The transcript language (`asr_lang`); `None` / `auto` as
    /// [`pick_track`] describes.
    pub lang: Option<String>,
    /// How the caption track is fetched. The SSRF guard applies to the
    /// `baseUrl` the page gives (a `youtube.com` host).
    pub fetch: FetchPolicy,
}

impl DeclarativeBackend for YoutubeBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let html = crate::backend::decode_html_bytes(&source.bytes);
        let mut page = parse_watch_page(&html);
        if page.video_id.is_none() {
            page.video_id = source.base_url.as_deref().and_then(video_id);
        }
        let (track, tlang) = pick_track(&page, self.lang.as_deref())?;
        let url = transcript_url(track, tlang.as_deref());
        let fetched = crate::fetch::fetch(&url, &self.fetch).map_err(|e| {
            ConversionError::Fetch(format!(
                "youtube: fetching the {} transcript of video {}: {e}",
                tlang.as_deref().unwrap_or(&track.language),
                page.video_id.as_deref().unwrap_or("?")
            ))
        })?;
        let vtt = String::from_utf8_lossy(&fetched.bytes);
        if !vtt.trim_start().starts_with("WEBVTT") {
            return Err(ConversionError::Parse(format!(
                "youtube: the transcript of video {} did not come back as WebVTT",
                page.video_id.as_deref().unwrap_or("?")
            )));
        }
        let name = page
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| source.name.clone());
        Ok(transcript_document(&name, &page, Some(&vtt)))
    }
}

/// `var NAME = {…};` in a `<script>`: the JSON value, parsed up to its
/// closing brace (serde stops at the end of one value, so the `;` and the
/// code after it are never read).
fn script_json(html: &str, name: &str) -> Option<serde_json::Value> {
    let mut from = 0;
    while let Some(pos) = html[from..].find(name) {
        let after = from + pos + name.len();
        let rest = html[after..].trim_start();
        if let Some(json) = rest.strip_prefix('=') {
            let json = json.trim_start();
            if json.starts_with('{') {
                if let Some(Ok(value)) = serde_json::Deserializer::from_str(json)
                    .into_iter::<serde_json::Value>()
                    .next()
                {
                    return Some(value);
                }
            }
        }
        from = after;
    }
    None
}

/// MarkItDown's `_findKey`: the first value under `key` anywhere in the tree.
fn find_key<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => items.iter().find_map(|v| find_key(v, key)),
        serde_json::Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find_key(v, key))),
        _ => None,
    }
}

/// Every `<meta>`'s `(itemprop | property | name, content)` — MarkItDown's
/// read of the head, entities decoded.
fn meta_tags(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = html[from..].find("<meta") {
        let start = from + pos;
        let Some(len) = html[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + len];
        from = start + len;
        let Some(content) = attribute(tag, "content") else {
            continue;
        };
        if let Some(key) = ["itemprop", "property", "name"]
            .iter()
            .find_map(|a| attribute(tag, a))
        {
            if !key.is_empty() && !content.is_empty() {
                out.push((key, content));
            }
        }
    }
    out
}

/// One attribute's value from a start tag (quoted either way).
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(name) {
        let at = from + pos;
        from = at + name.len();
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let rest = tag[at + name.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        if !before_ok {
            continue;
        }
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            return Some(unescape_entities(&rest[..end]));
        }
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        return Some(unescape_entities(&inner[..end]));
    }
    None
}

fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = start + lower[start..].find('>')? + 1;
    let close = open_end + lower[open_end..].find("</title>")?;
    let title = unescape_entities(html[open_end..close].trim());
    (!title.is_empty()).then_some(title)
}

/// `PT1H2M3S` for a length in seconds — what the page's
/// `<meta itemprop="duration">` prints and MarkItDown shows as the runtime.
fn iso_duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    let mut out = String::from("PT");
    if h > 0 {
        out.push_str(&format!("{h}H"));
    }
    if m > 0 {
        out.push_str(&format!("{m}M"));
    }
    if s > 0 || seconds == 0 {
        out.push_str(&format!("{s}S"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data/youtube")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn video_ids_of_every_url_form() {
        for url in [
            "https://www.youtube.com/watch?v=x7Zq9KfR2bE",
            "https://youtube.com/watch?v=x7Zq9KfR2bE&t=42s",
            "https://m.youtube.com/watch?feature=share&v=x7Zq9KfR2bE",
            "http://youtu.be/x7Zq9KfR2bE",
            "https://youtu.be/x7Zq9KfR2bE?si=abc",
            "https://www.youtube.com/shorts/x7Zq9KfR2bE",
            "https://www.youtube.com/embed/x7Zq9KfR2bE",
            "https://www.youtube.com/live/x7Zq9KfR2bE?feature=share",
        ] {
            assert_eq!(video_id(url).as_deref(), Some("x7Zq9KfR2bE"), "{url}");
        }
        for url in [
            "https://www.youtube.com/",
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/watch?list=PL123",
            "https://www.youtube.com/watch?v=short",
            "https://example.com/watch?v=x7Zq9KfR2bE",
            "https://notyoutube.com/watch?v=x7Zq9KfR2bE",
            "ftp://youtu.be/x7Zq9KfR2bE",
            "https://www.youtube.com/@channel",
        ] {
            assert_eq!(video_id(url), None, "{url}");
        }
    }

    #[test]
    fn the_watch_page_is_read_from_the_player_response() {
        let page = parse_watch_page(&fixture("watch.html"));
        assert_eq!(page.video_id.as_deref(), Some("x7Zq9KfR2bE"));
        assert_eq!(page.title.as_deref(), Some("Introducing docling.rs"));
        assert_eq!(page.author.as_deref(), Some("docling project"));
        assert_eq!(page.views.as_deref(), Some("1234"));
        assert_eq!(page.keywords, ["docling", "rust", "document conversion"]);
        assert_eq!(page.duration.as_deref(), Some("PT1M5S"));
        assert!(
            page.description.as_deref().unwrap().starts_with(
                "A tour of the converter: one binary, every format.\n\nChapters:\n0:00 Intro"
            ),
            "{:?}",
            page.description
        );
        let langs: Vec<(&str, bool, Option<&str>)> = page
            .tracks
            .iter()
            .map(|t| (t.language.as_str(), t.generated, t.name.as_deref()))
            .collect();
        assert_eq!(
            langs,
            [
                ("en", false, Some("English")),
                ("en", true, Some("English (auto-generated)")),
                ("de", false, Some("Deutsch")),
            ]
        );
        assert!(page.tracks[0]
            .base_url
            .starts_with("https://www.youtube.com/api/timedtext?v=x7Zq9KfR2bE&"));
        assert!(page.tracks.iter().all(|t| t.translatable));
        assert_eq!(page.translation_languages, ["fr", "es"]);
    }

    /// MarkItDown's reading of a page without the player JSON: `<title>`
    /// (minus ` - YouTube`) or the `og:` / `itemprop` metas, the description
    /// from `ytInitialData`, views / keywords / duration from the metas.
    #[test]
    fn meta_tags_answer_when_the_player_response_is_missing() {
        let html = r#"<html><head><title>Video title - YouTube</title>
            <meta itemprop="interactionCount" content="42"><meta name="keywords" content="example, two">
            <meta itemprop="duration" content="PT1M"><meta property="og:description" content="Fallback &amp; more">
            <script>var ytInitialData = {"a":[{"attributedDescriptionBodyText": {"content": "Video description"}}]};</script>
            </head><body></body></html>"#;
        let page = parse_watch_page(html);
        assert_eq!(page.title.as_deref(), Some("Video title"));
        assert_eq!(page.description.as_deref(), Some("Video description"));
        assert_eq!(page.views.as_deref(), Some("42"));
        assert_eq!(page.keywords, ["example", "two"]);
        assert_eq!(page.duration.as_deref(), Some("PT1M"));
        assert!(page.tracks.is_empty());
        let only_meta = parse_watch_page(
            r#"<meta property="og:title" content="From og"><meta property="og:description" content="Fallback &amp; more">"#,
        );
        assert_eq!(only_meta.title.as_deref(), Some("From og"));
        assert_eq!(only_meta.description.as_deref(), Some("Fallback & more"));
        assert_eq!(parse_watch_page("<html></html>"), WatchPage::default());
    }

    #[test]
    fn the_track_follows_the_language() {
        let page = parse_watch_page(&fixture("watch.html"));
        let pick = |lang: Option<&str>| {
            pick_track(&page, lang)
                .map(|(t, tlang)| (t.language.clone(), t.generated, tlang))
                .map_err(|e| e.to_string())
        };
        // Unset / auto: English, written before auto-generated.
        assert_eq!(pick(None), Ok(("en".into(), false, None)));
        assert_eq!(pick(Some("auto")), Ok(("en".into(), false, None)));
        assert_eq!(pick(Some("EN")), Ok(("en".into(), false, None)));
        assert_eq!(pick(Some("de")), Ok(("de".into(), false, None)));
        // No French track, but YouTube translates: the English one, `tlang=fr`.
        assert_eq!(
            pick(Some("fr")),
            Ok(("en".into(), false, Some("fr".into())))
        );
        let err = pick(Some("ja")).unwrap_err();
        assert!(
            err.contains("video x7Zq9KfR2bE has no ja captions (available: en, de)"),
            "{err}"
        );
        let track = pick_track(&page, Some("fr")).unwrap();
        assert_eq!(
            transcript_url(track.0, track.1.as_deref()),
            format!("{}&fmt=vtt&tlang=fr", page.tracks[0].base_url)
        );

        // Only an auto-generated track: it is the one.
        let mut asr_only = page.clone();
        asr_only.tracks.retain(|t| t.generated);
        assert_eq!(
            pick_track(&asr_only, None).unwrap().0.name.as_deref(),
            Some("English (auto-generated)")
        );
        // No English: the first track.
        let mut german = page.clone();
        german.tracks.retain(|t| t.language == "de");
        assert_eq!(pick_track(&german, None).unwrap().0.language, "de");
    }

    #[test]
    fn a_video_without_captions_is_a_clear_error() {
        let page = parse_watch_page(&fixture("watch_no_captions.html"));
        assert_eq!(page.title.as_deref(), Some("A silent film"));
        assert!(page.tracks.is_empty());
        let err = pick_track(&page, None).unwrap_err().to_string();
        assert!(err.contains("video q2WkY8rPz3M has no captions"), "{err}");
    }

    /// MarkItDown's shape, the transcript as timed items.
    #[test]
    fn the_document_has_markitdowns_shape() {
        let page = parse_watch_page(&fixture("watch.html"));
        let doc = transcript_document(
            "Introducing docling.rs",
            &page,
            Some(&fixture("captions_en.vtt")),
        );
        let md = doc.export_to_markdown();
        assert_eq!(
            md,
            "# YouTube\n\n## Introducing docling.rs\n\n### Video Metadata\n\n\
             - **Views:** 1234\n- **Keywords:** docling, rust, document conversion\n- **Runtime:** PT1M5S\n\n\
             ### Description\n\nA tour of the converter: one binary, every format.\n\n\
             Chapters:  \n0:00 Intro  \n0:30 Formats &amp; outputs\n\n\
             ### Transcript\n\ndocling.rs is one binary that converts\n\n\
             PDF, Office and web documents to Markdown\n\n\
             and JSON — no Python, no models to install\n\n\
             every format goes through the same document model\n"
        );
        // The cues keep their timing: `--to vtt` writes them back.
        let vtt = doc.export_to_vtt();
        assert!(
            vtt.contains("00:00:00.240 --> 00:00:03.120\ndocling.rs is one binary that converts"),
            "{vtt}"
        );
        assert!(
            vtt.contains(
                "00:00:30.000 --> 00:00:33.500\nevery format goes through the same document model"
            ),
            "{vtt}"
        );
        // docling-core's VTT header is the document's title item.
        assert!(
            vtt.starts_with("WEBVTT YouTube\n\n00:00:00.240 --> "),
            "{vtt}"
        );
        assert!(
            !vtt.contains("Introducing") && !vtt.contains("Views"),
            "{vtt}"
        );
        let json = doc.export_to_json();
        assert!(json.contains("\"text\": \"**Views:** 1234\""), "{json}");
        assert!(
            json.contains("\"label\": \"title\"") && json.contains("\"start_time\": 0.24"),
            "{json}"
        );
        assert_eq!(
            doc.metadata.title.as_deref(),
            Some("Introducing docling.rs")
        );
        assert_eq!(doc.metadata.authors, ["docling project"]);

        // Metadata alone (no transcript fetched): no Transcript section.
        let bare = transcript_document(
            "x",
            &WatchPage {
                title: Some("Only a title".into()),
                ..Default::default()
            },
            None,
        );
        assert_eq!(bare.export_to_markdown(), "# YouTube\n\n## Only a title\n");
    }

    /// YouTube's auto-generated track repeats every line in the next cue and
    /// in a 10 ms cue between: each line once, in the cue it first appears.
    #[test]
    fn rolling_asr_cues_read_straight_through() {
        let page = parse_watch_page(&fixture("watch.html"));
        let doc = transcript_document("t", &page, Some(&fixture("captions_en_asr.vtt")));
        let md = doc.export_to_markdown();
        let transcript = md.split("### Transcript\n\n").nth(1).unwrap();
        assert_eq!(
            transcript,
            "docling rs is one binary that converts\n\npdf office and web documents\n"
        );
        let vtt = doc.export_to_vtt();
        assert!(
            vtt.contains("00:00:00.240 --> 00:00:03.129\ndocling rs is one binary that converts"),
            "{vtt}"
        );
        assert!(
            vtt.contains("00:00:03.139 --> 00:00:06.470\npdf office and web documents"),
            "{vtt}"
        );
        // A written track is untouched (its header lines included); a
        // voice span is not a word tag.
        let en = fixture("captions_en.vtt");
        let body = en.split_once('\n').unwrap().1;
        assert_eq!(dedupe_rolling_cues(body).trim_end(), body.trim_end());
        assert_eq!(
            strip_word_tags("<v Ann>hi<00:00:01.000><c> there</c></v>"),
            "<v Ann>hi there</v>"
        );
    }

    #[test]
    fn durations_print_as_iso_8601() {
        assert_eq!(iso_duration(65), "PT1M5S");
        assert_eq!(iso_duration(3600), "PT1H");
        assert_eq!(iso_duration(3661), "PT1H1M1S");
        assert_eq!(iso_duration(0), "PT0S");
    }
}
