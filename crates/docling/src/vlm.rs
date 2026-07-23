//! VLM pipeline (issue #77) — remote OpenAI-compatible vision endpoint.
//!
//! The counterpart of docling's `VlmPipeline` in its remote form
//! (`ApiVlmOptions`): each PDF page is rendered to an image, sent to an
//! OpenAI-compatible `chat/completions` endpoint (LM Studio, Ollama, vLLM, or
//! a hosted service) together with a DocLang-eliciting prompt, and the
//! returned markup is parsed by the existing DocLang reader
//! (`backend::doclang`) into a [`DoclingDocument`]. Local ONNX inference of a
//! docling VLM is a later enhancement — this module deliberately contains no
//! model code, just the request loop.
//!
//! HTTP goes over `ureq`, the crate's existing blocking client
//! (`fetch-images` pulls the same one, keeping a single HTTP stack in the
//! graph — the converter is synchronous, so an async client would only add a
//! runtime). Transient failures (transport errors, 408/429, 5xx) retry with
//! exponential backoff; anything else fails the conversion loudly — a VLM
//! conversion with silently dropped pages would be worse than an error.

use std::io::Cursor;
use std::time::Duration;

use docling_core::DoclingDocument;

use crate::backend::doclang::DoclangBackend;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::source::SourceDocument;

/// Configuration for the remote VLM conversion. Everything has an env-var
/// fallback so `--pipeline vlm` works without repeating flags:
/// `DOCLING_RS_VLM_ENDPOINT`, `DOCLING_RS_VLM_MODEL`, `DOCLING_RS_VLM_PROMPT`,
/// `DOCLING_RS_VLM_API_KEY`.
#[derive(Debug, Clone)]
pub struct VlmOptions {
    /// Base URL of the OpenAI-compatible server (`http://localhost:11434/v1`)
    /// or the full `…/chat/completions` URL — the suffix is appended when
    /// missing, so both spellings work.
    pub endpoint: String,
    /// Model name as the server knows it (e.g. `granite-docling`).
    pub model: String,
    /// The instruction sent with every page image. Defaults to docling's
    /// DocLang-eliciting prompt ([`DEFAULT_VLM_PROMPT`]).
    pub prompt: Option<String>,
    /// Bearer token, if the endpoint wants one. Local servers don't.
    pub api_key: Option<String>,
    /// 1-based inclusive page window (`--pages` composes with the VLM
    /// pipeline exactly like with the ML one).
    pub page_range: Option<(usize, usize)>,
    /// `max_tokens` for each completion. A dense page of DocLang easily runs
    /// long; the default (8192) fits every corpus page with headroom.
    pub max_tokens: usize,
}

/// docling's page-conversion instruction for its DocLang-emitting VLMs.
pub const DEFAULT_VLM_PROMPT: &str = "Convert this page to docling.";

impl VlmOptions {
    /// Build options from explicit values, falling back to the
    /// `DOCLING_RS_VLM_*` environment. Endpoint and model are required —
    /// there is no sensible default server to talk to.
    pub fn resolve(
        endpoint: Option<String>,
        model: Option<String>,
    ) -> Result<Self, ConversionError> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let endpoint = endpoint
            .or_else(|| env("DOCLING_RS_VLM_ENDPOINT"))
            .ok_or_else(|| {
                ConversionError::Parse(
                    "vlm: no endpoint (pass --vlm-endpoint or set DOCLING_RS_VLM_ENDPOINT)".into(),
                )
            })?;
        let model = model
            .or_else(|| env("DOCLING_RS_VLM_MODEL"))
            .ok_or_else(|| {
                ConversionError::Parse(
                    "vlm: no model (pass --vlm-model or set DOCLING_RS_VLM_MODEL)".into(),
                )
            })?;
        Ok(Self {
            endpoint,
            model,
            prompt: env("DOCLING_RS_VLM_PROMPT"),
            api_key: env("DOCLING_RS_VLM_API_KEY"),
            page_range: None,
            max_tokens: 8192,
        })
    }

    fn url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

/// Convert a PDF or image through the remote VLM. PDF pages render via
/// pdfium at the ML pipeline's scale; a standalone image is sent as-is (it is
/// its own page). Every page must convert — a failed page fails the document.
pub fn convert_vlm(
    source: &SourceDocument,
    opts: &VlmOptions,
) -> Result<DoclingDocument, ConversionError> {
    let agent = agent();
    let mut fragments: Vec<String> = Vec::new();
    match source.format {
        InputFormat::Pdf => {
            // 1-based window → 0-based inclusive, validated like Pipeline::pages.
            let total = docling_pdf::pdfium_backend::page_count(&source.bytes, None)
                .map_err(|e| ConversionError::Parse(format!("vlm: open pdf: {e}")))?;
            let range = match opts.page_range {
                Some((first, last)) => {
                    if first == 0 || last < first {
                        return Err(ConversionError::Parse(format!(
                            "invalid page range {first}-{last} (pages are 1-based, first <= last)"
                        )));
                    }
                    if first > total {
                        return Err(ConversionError::Parse(format!(
                            "page range {first}-{last} is outside the document ({total} page(s))"
                        )));
                    }
                    Some((first - 1, last.min(total) - 1))
                }
                None => None,
            };
            // `for_each_page`'s error type must implement From<PdfiumError>,
            // which ConversionError doesn't — so VLM/encode failures park
            // their message in `vlm_err` and abort the walk with a sentinel;
            // only genuine pdfium errors surface through PdfError itself.
            let mut vlm_err: Option<String> = None;
            let walk = docling_pdf::pdfium_backend::for_each_page::<docling_pdf::PdfError, _>(
                &source.bytes,
                None,
                true, // render page images — they are the whole input here
                range,
                |i, _total, page| {
                    let step =
                        encode_png(&page.image).and_then(|png| request_page(&agent, opts, &png));
                    match step {
                        Ok(markup) => {
                            fragments.push(doclang_fragment(&markup));
                            Ok(())
                        }
                        Err(e) => {
                            vlm_err = Some(format!("page {}: {e}", i + 1));
                            Err(docling_pdf::PdfError::Pdfium("vlm abort".into()))
                        }
                    }
                },
            );
            if let Some(msg) = vlm_err {
                return Err(ConversionError::Parse(format!("vlm: {msg}")));
            }
            walk.map_err(pdf_err)?;
        }
        InputFormat::Image => {
            // The image file is already the page; no re-encode, no pdfium.
            let markup = request_page(&agent, opts, &source.bytes)
                .map_err(|e| ConversionError::Parse(format!("vlm: {e}")))?;
            fragments.push(doclang_fragment(&markup));
        }
        other => {
            return Err(ConversionError::Parse(format!(
                "vlm pipeline converts PDF and image inputs (got {other:?})"
            )));
        }
    }
    // One document out of the per-page fragments, through the tolerant
    // DocLang reader — exactly what a `.dclg` file would take.
    let xml = format!(
        "<doclang version=\"0.7\">\n{}\n</doclang>",
        fragments.join("\n")
    );
    let synthetic =
        SourceDocument::from_bytes(&source.name, InputFormat::XmlDoclang, xml.into_bytes());
    let doc = DoclangBackend.convert(&synthetic)?;
    if doc.nodes.is_empty() {
        // The request loop succeeded, so this is a content problem, not a
        // transport one: the model answered with nothing our reader keeps.
        // An empty stdout with exit 0 buried that; say it loudly instead.
        return Err(ConversionError::Parse(
            "vlm: the model's responses contained no parseable DocLang/DocTags blocks \
             (set DOCLING_RS_VLM_DEBUG=1 to print raw responses; a generic VLM may need \
             a DOCLING_RS_VLM_PROMPT that describes the expected markup)"
                .into(),
        ));
    }
    Ok(doc)
}

fn pdf_err(e: docling_pdf::PdfError) -> ConversionError {
    ConversionError::with_source("pdf", e)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        // A VLM can chew on a dense page for minutes, especially on CPU.
        .timeout_global(Some(Duration::from_secs(600)))
        // Keep non-2xx as inspectable responses for the retry decision.
        .http_status_as_error(false)
        .build()
        .into()
}

/// POST one page image, return the model's text. Retries transport errors,
/// 408/429 and 5xx with exponential backoff (2s/4s/8s); other statuses and a
/// malformed body fail immediately.
fn request_page(agent: &ureq::Agent, opts: &VlmOptions, image: &[u8]) -> Result<String, String> {
    let data_uri = format!(
        "data:image/png;base64,{}",
        docling_core::base64::encode(image)
    );
    let mut body = serde_json::json!({
        "model": opts.model,
        // Deterministic-ish output: sampling noise only hurts a structured
        // markup task (docling's ApiVlmOptions ships temperature 0 too).
        "temperature": 0,
        "max_tokens": opts.max_tokens,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text",
                  "text": opts.prompt.as_deref().unwrap_or(DEFAULT_VLM_PROMPT) },
                { "type": "image_url", "image_url": { "url": data_uri } },
            ],
        }],
    });
    // DOCLING_RS_VLM_EXTRA_BODY: a JSON object merged into the request at the
    // top level — the escape hatch for server-specific knobs the OpenAI shape
    // doesn't cover. The motivating case: vLLM's "skip_special_tokens": false,
    // without which servers detokenize away granite-docling's DocTags
    // structure tokens and only loc tokens + bare text survive.
    if let Ok(extra) = std::env::var("DOCLING_RS_VLM_EXTRA_BODY") {
        match serde_json::from_str::<serde_json::Value>(&extra) {
            Ok(serde_json::Value::Object(map)) => {
                for (k, v) in map {
                    body[k] = v;
                }
            }
            _ => {
                return Err(
                    "DOCLING_RS_VLM_EXTRA_BODY is not a JSON object; fix or unset it".into(),
                )
            }
        }
    }
    let url = opts.url();
    let mut delay = Duration::from_secs(2);
    let mut last_err = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(delay);
            delay *= 2;
        }
        let mut req = agent.post(&url).header("content-type", "application/json");
        if let Some(key) = &opts.api_key {
            req = req.header("authorization", &format!("Bearer {key}"));
        }
        // Hand-serialized body: the crate pulls ureq without its `json`
        // feature (fetch-images doesn't need it), and one to_string keeps it
        // that way.
        let payload = serde_json::to_string(&body).expect("static json shape");
        match req.send(payload.as_bytes()) {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let text = resp
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| format!("{url}: read response: {e}"))?;
                if status == 408 || status == 429 || status >= 500 {
                    // Keep a body snippet: for OpenAI-style servers the real
                    // reason (insufficient_quota vs. a plain rate limit)
                    // lives there, and hiding it made give-ups undiagnosable.
                    last_err = format!(
                        "{url}: HTTP {status} (attempt {}): {}",
                        attempt + 1,
                        text.replace(['\n', '\r'], " ")
                            .chars()
                            .take(300)
                            .collect::<String>()
                    );
                    continue;
                }
                if status != 200 {
                    return Err(format!(
                        "{url}: HTTP {status}: {}",
                        text.chars().take(300).collect::<String>()
                    ));
                }
                let parsed: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| format!("{url}: malformed JSON response: {e}"))?;
                let content = parsed["choices"][0]["message"]["content"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{url}: no choices[0].message.content in response"));
                if std::env::var_os("DOCLING_RS_VLM_DEBUG").is_some() {
                    match &content {
                        Ok(c) => {
                            eprintln!("vlm: raw model response ({} chars):\n{c}\n---", c.len())
                        }
                        Err(_) => eprintln!("vlm: raw endpoint body:\n{text}\n---"),
                    }
                }
                return content;
            }
            Err(e) => {
                last_err = format!("{url}: {e} (attempt {})", attempt + 1);
            }
        }
    }
    Err(format!("giving up after 4 attempts: {last_err}"))
}

/// Reduce one model response to DocLang *body* markup, ready to concatenate
/// under a single `<doclang>` root.
///
/// Models wrap their answer unpredictably: Markdown code fences, a full
/// `<doclang …>` document, a legacy `<doctag>` root, or a bare fragment of
/// block elements. The wrappers strip first, then the body goes through the
/// DocTags→DocLang lexical translation ([`doctags_to_doclang`]) — a no-op
/// for already-DocLang answers, load-bearing for granite-docling-class
/// models whose raw DocTags token stream is not XML at all.
fn doclang_fragment(response: &str) -> String {
    let translated = doctags_to_doclang(&strip_wrappers(response));
    // A model that ignored the markup instruction entirely (plain prose /
    // Markdown, no tags) still carries the page text — wrap it as one
    // paragraph per non-empty line instead of silently dropping everything.
    if !translated.contains('<') && !translated.trim().is_empty() {
        return translated
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("<text>{}</text>", l.trim()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    translated
}

/// The wrapper-stripping half of [`doclang_fragment`].
fn strip_wrappers(response: &str) -> String {
    let mut text = response.trim();
    // ```xml … ``` / ``` … ``` fences.
    if let Some(rest) = text.strip_prefix("```") {
        let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or(rest);
        text = rest
            .rsplit_once("```")
            .map(|(r, _)| r)
            .unwrap_or(rest)
            .trim();
    }
    // Unwrap a <doclang>/<doctag> root down to its children.
    for root in ["doclang", "doctag", "doctags"] {
        let open = format!("<{root}");
        if let Some(start) = text.find(&open) {
            if let Some(gt) = text[start..].find('>') {
                let inner_start = start + gt + 1;
                let close = format!("</{root}>");
                let inner_end = text.rfind(&close).unwrap_or(text.len());
                if inner_start <= inner_end {
                    return text[inner_start..inner_end].trim().to_string();
                }
            }
        }
    }
    text.to_string()
}

/// Translate a legacy **DocTags** fragment (what granite-docling-class models
/// actually emit) into well-formed DocLang the XML reader accepts.
///
/// DocTags is a token stream, not XML: `<loc_57>` location tokens, OTSL cell
/// markers (`<fcel>Text<fcel>…<nl>`), picture-class and checkbox tokens are
/// all *unclosed*. The DocLang reader already speaks OTSL — its `<table>`
/// parser takes self-closed `<fcel/>` markers with the text between them —
/// so the translation is purely lexical, one pass over the tags:
///
/// - `<loc_N>` and `<page_break>` tokens drop (the reader's geometry comes
///   from `<location>`, which VLM output doesn't carry);
/// - `<otsl>` → `<table>`, `<section_header_level_K>` → `<heading
///   level="K">`, `<title>` → `<heading level="1">`, `<paragraph>` →
///   `<text>`, `<ordered_list>`/`<unordered_list>` → `<list>`,
///   `<list_item>` → the `<ldiv/>` item marker DocLang lists use;
/// - `<page_header>`/`<page_footer>` → `<text>` opening with a
///   `<layer value="furniture"/>` head, so headers/footers stay out of the
///   Markdown body exactly like the ML pipeline's furniture;
/// - any other tag that never sees a matching closer in the fragment —
///   OTSL cell markers, picture classes, checkbox states, whatever a future
///   model invents — is emitted self-closed, which the tolerant reader
///   recurses through harmlessly;
/// - stray `&` (not an entity) and dangling `<` escape to entities, since
///   model text is not XML-escaped.
///
/// A fragment that is already DocLang passes through intact: every paired
/// element has its closer, none of the rename names exist in DocLang, and
/// proper entities are left alone.
fn doctags_to_doclang(fragment: &str) -> String {
    // Tag names that appear in closing form — those pairs are kept as-is;
    // everything else unknown is a single marker token.
    let mut closers: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let bytes = fragment.as_bytes();
    let mut i = 0;
    while let Some(off) = fragment[i..].find("</") {
        let start = i + off + 2;
        let end = fragment[start..]
            .find('>')
            .map(|e| start + e)
            .unwrap_or(fragment.len());
        closers.insert(fragment[start..end].trim());
        i = end;
    }

    let mut out = String::with_capacity(fragment.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c != '<' {
            if c == '&' {
                // Escape a bare ampersand; keep real entities (&amp; &#123;).
                let rest = &fragment[i + 1..];
                let is_entity = rest
                    .split_once(';')
                    .map(|(name, _)| {
                        !name.is_empty()
                            && name.len() <= 8
                            && name
                                .chars()
                                .all(|ch| ch.is_ascii_alphanumeric() || ch == '#')
                    })
                    .unwrap_or(false);
                out.push_str(if is_entity { "&" } else { "&amp;" });
            } else {
                out.push(c);
            }
            i += c.len_utf8();
            continue;
        }
        // A tag candidate: `<`, optional `/`, then a name. Anything else is
        // literal text (`a < b`) and escapes.
        let close = bytes.get(i + 1) == Some(&b'/');
        let name_start = i + 1 + usize::from(close);
        let Some(end) = fragment[i..].find('>').map(|e| i + e) else {
            out.push_str("&lt;");
            i += 1;
            continue;
        };
        let inner = fragment[name_start..end].trim();
        let name = inner
            .split([' ', '\t', '\n', '/'])
            .next()
            .unwrap_or_default();
        if name.is_empty()
            || !name
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic())
        {
            out.push_str("&lt;");
            i += 1;
            continue;
        }
        let self_closed = inner.ends_with('/');
        i = end + 1;
        // Dropped tokens.
        if name.starts_with("loc_") || name == "page_break" || name == "doctag" || name == "doctags"
        {
            continue;
        }
        // Renames (open and close forms).
        let level = name
            .strip_prefix("section_header_level_")
            .and_then(|v| v.parse::<u8>().ok());
        if close {
            match name {
                _ if level.is_some() => out.push_str("</heading>"),
                "title" => out.push_str("</heading>"),
                "otsl" => out.push_str("</table>"),
                // An item's text runs until the next `<ldiv/>`; the closer is
                // structural noise in DocLang.
                "list_item" => {}
                "paragraph" | "page_header" | "page_footer" => out.push_str("</text>"),
                "ordered_list" | "unordered_list" => out.push_str("</list>"),
                _ => {
                    out.push_str("</");
                    out.push_str(name);
                    out.push('>');
                }
            }
            continue;
        }
        match name {
            _ if level.is_some() => {
                out.push_str(&format!(
                    "<heading level=\"{}\">",
                    level.unwrap().clamp(1, 6)
                ));
            }
            "title" => out.push_str("<heading level=\"1\">"),
            "otsl" => out.push_str("<table>"),
            // DocLang list items are `<ldiv/>` markers followed by the item's
            // text, not wrapper elements.
            "list_item" => out.push_str("<ldiv/>"),
            "paragraph" => out.push_str("<text>"),
            "page_header" | "page_footer" => out.push_str("<text><layer value=\"furniture\"/>"),
            "ordered_list" => out.push_str("<list class=\"ordered\">"),
            "unordered_list" => out.push_str("<list>"),
            _ if self_closed || closers.contains(name) => {
                // A proper pair (DocLang vocabulary) or already self-closed:
                // pass through with attributes intact.
                out.push('<');
                out.push_str(inner);
                out.push('>');
            }
            _ => {
                // A bare marker token with no closer anywhere — OTSL cells,
                // picture classes, checkbox states. Self-close it.
                out.push('<');
                out.push_str(name);
                out.push_str("/>");
            }
        }
    }
    out
}

/// PNG-encode a rendered page (the wire format every OpenAI-compatible
/// server accepts as a data URI).
fn encode_png(image: &image::RgbImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("encode page image: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::doclang_fragment;

    #[test]
    fn fragment_normalization() {
        // Bare fragment passes through.
        assert_eq!(doclang_fragment("<text>hi</text>"), "<text>hi</text>");
        // Fenced answer is unwrapped.
        assert_eq!(
            doclang_fragment("```xml\n<text>hi</text>\n```"),
            "<text>hi</text>"
        );
        // A full document root is stripped down to its body.
        assert_eq!(
            doclang_fragment("<doclang version=\"0.7\"><text>hi</text></doclang>"),
            "<text>hi</text>"
        );
        // Legacy doctag root likewise.
        assert_eq!(
            doclang_fragment("<doctag><text>hi</text></doctag>"),
            "<text>hi</text>"
        );
        // Prose around a fenced fragment is ignored by the fence rule only
        // when the fence comes first; a root element wins anywhere.
        assert_eq!(
            doclang_fragment("Here you go:\n<doclang><heading level=\"1\">T</heading></doclang>"),
            "<heading level=\"1\">T</heading>"
        );
    }

    /// Raw granite-docling output is DocTags — loc tokens, unclosed OTSL
    /// markers, section_header naming — and must come out as parseable
    /// DocLang (this exact shape produced the "expected 'loc_420' tag"
    /// failure against a live Ollama endpoint).
    #[test]
    fn doctags_translation() {
        let page = "<doctag><picture><loc_15><loc_10><loc_240><loc_60><other></picture>\
<section_header_level_1><loc_57><loc_70><loc_420><loc_78>Optimized Table Tokenization</section_header_level_1>\
<text><loc_57><loc_84><loc_420><loc_98>Body with A &amp; B & C.</text>\
<unordered_list><list_item><loc_60><loc_100><loc_420><loc_108>First item</list_item></unordered_list>\
<otsl><loc_57><loc_120><loc_420><loc_160><ched>Col A<ched>Col B<nl><fcel>1<fcel>2<nl></otsl>\
<page_footer><loc_57><loc_280><loc_420><loc_288>7</page_footer><page_break></doctag>";
        let fragment = doclang_fragment(page);
        let xml = format!("<doclang version=\"0.7\">{fragment}</doclang>");
        let doc = crate::backend::DeclarativeBackend::convert(
            &crate::backend::doclang::DoclangBackend,
            &crate::source::SourceDocument::from_bytes(
                "page",
                crate::format::InputFormat::XmlDoclang,
                xml.into_bytes(),
            ),
        )
        .expect("translated DocTags must parse");
        let md = doc.export_to_markdown();
        assert!(md.contains("# Optimized Table Tokenization"), "md: {md:?}");
        assert!(md.contains("Body with A & B & C."), "md: {md:?}");
        assert!(md.contains("- First item"), "md: {md:?}");
        assert!(md.contains("Col A |"), "md: {md:?}");
        assert!(md.contains("1 |"), "md: {md:?}");
        // Furniture (page footer) stays out of the Markdown body.
        assert!(!md.contains("\n7\n"), "md: {md:?}");
    }

    /// DocLang-native fragments survive the translation byte-for-byte.
    #[test]
    fn doclang_passthrough() {
        let native = "<heading level=\"2\">T</heading>\n<text>Hi &amp; bye <bold>b</bold></text>\n<table>\n<fcel/>a\n<nl/>\n</table>";
        assert_eq!(doclang_fragment(native), native);
    }
}
