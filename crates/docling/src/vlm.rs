//! VLM pipeline (issue #77) — remote OpenAI-compatible vision endpoint.
//!
//! The counterpart of docling's `VlmPipeline` in its remote form
//! (`ApiVlmOptions`): each PDF page is rendered to an image, sent to an
//! OpenAI-compatible `chat/completions` endpoint (LM Studio, Ollama, vLLM, or
//! a hosted service) together with a DocLang-eliciting prompt, and the
//! returned markup is parsed into a [`DoclingDocument`] — DocTags answers
//! (granite-docling-class models) via `docling_core::doctags`' tolerant
//! parser (#152), DocLang XML via the existing reader (`backend::doclang`),
//! and untagged prose via a line-per-paragraph fallback. Local ONNX
//! inference of a docling VLM is a later enhancement — this module
//! deliberately contains no model code, just the request loop.
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
                            fragments.push(strip_wrappers(&markup));
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
            fragments.push(strip_wrappers(&markup));
        }
        other => {
            return Err(ConversionError::Parse(format!(
                "vlm pipeline converts PDF and image inputs (got {other:?})"
            )));
        }
    }
    // Two grammars come back from the wire. DocTags (granite-docling-class
    // models: loc tokens, unclosed OTSL markers) goes to docling-core's
    // dedicated tolerant parser, which keeps geometry and span structure
    // (#152). Proper DocLang XML — or plain prose — takes the DocLang
    // reader path a `.dclg` file would. One DocTags-shaped page routes the
    // whole document: mixing grammars page-to-page is model misbehavior,
    // and the DocTags parser degrades to paragraphs on non-DocTags input.
    let doc = if fragments.iter().any(|f| looks_like_doctags(f)) {
        let mut doc = docling_core::doctags::parse_pages(fragments.iter().map(String::as_str));
        doc.name = source.name.clone();
        doc
    } else {
        let body: Vec<String> = fragments.iter().map(|f| prose_fallback(f)).collect();
        let xml = format!("<doclang version=\"0.7\">\n{}\n</doclang>", body.join("\n"));
        let synthetic =
            SourceDocument::from_bytes(&source.name, InputFormat::XmlDoclang, xml.into_bytes());
        DoclangBackend.convert(&synthetic)?
    };
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

/// Is this fragment DocTags rather than DocLang? Granite-class models always
/// carry `loc_` tokens; the OTSL/section-header vocabularies clinch it when a
/// (hypothetical) model omits locations.
fn looks_like_doctags(fragment: &str) -> bool {
    fragment.contains("<loc_")
        || fragment.contains("<otsl>")
        || fragment.contains("<fcel>")
        || fragment.contains("<section_header_level_")
}

/// DocLang-path fallback for a model that ignored the markup instruction
/// entirely (plain prose / Markdown, no tags): one `<text>` per non-empty
/// line, instead of silently dropping the page's content.
fn prose_fallback(fragment: &str) -> String {
    if !fragment.contains('<') && !fragment.trim().is_empty() {
        return fragment
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("<text>{}</text>", l.trim()))
            .collect::<Vec<_>>()
            .join("\n");
    }
    fragment.to_string()
}

/// Strip the wrappers models put around their answer: Markdown code fences
/// and a `<doclang …>`/`<doctag>` document root (the body keeps its grammar —
/// DocTags routing happens later on the stripped fragment).
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
    use super::{looks_like_doctags, prose_fallback, strip_wrappers};

    #[test]
    fn wrapper_stripping() {
        // Bare fragment passes through.
        assert_eq!(strip_wrappers("<text>hi</text>"), "<text>hi</text>");
        // Fenced answer is unwrapped.
        assert_eq!(
            strip_wrappers("```xml\n<text>hi</text>\n```"),
            "<text>hi</text>"
        );
        // A full document root is stripped down to its body.
        assert_eq!(
            strip_wrappers("<doclang version=\"0.7\"><text>hi</text></doclang>"),
            "<text>hi</text>"
        );
        assert_eq!(
            strip_wrappers("<doctag><text>hi</text></doctag>"),
            "<text>hi</text>"
        );
        // Prose around a root element: the root wins anywhere.
        assert_eq!(
            strip_wrappers("Here you go:\n<doclang><heading level=\"1\">T</heading></doclang>"),
            "<heading level=\"1\">T</heading>"
        );
    }

    /// Routing: granite-style DocTags goes to the docling-core parser (whose
    /// own tests pin the full markup semantics); DocLang and prose do not.
    #[test]
    fn doctags_routing() {
        assert!(looks_like_doctags(
            "<text><loc_1><loc_2><loc_3><loc_4>Body</text>"
        ));
        assert!(looks_like_doctags("<otsl><ched>A<nl></otsl>"));
        assert!(looks_like_doctags(
            "<section_header_level_1>T</section_header_level_1>"
        ));
        assert!(!looks_like_doctags("<heading level=\"2\">T</heading>"));
        assert!(!looks_like_doctags("Just prose, no markup."));
    }

    /// End-to-end through the same assembly the pipeline uses: the exact
    /// shape live granite-docling emits (from the #77 bring-up) renders with
    /// docling-parity structure.
    #[test]
    fn doctags_end_to_end_markdown() {
        let page = "<doctag><picture><loc_15><loc_10><loc_240><loc_60><other></picture>\
<section_header_level_1><loc_57><loc_70><loc_420><loc_78>Optimized Table Tokenization</section_header_level_1>\
<text><loc_57><loc_84><loc_420><loc_98>Body with A &amp; B & C.</text>\
<unordered_list><list_item><loc_60><loc_100><loc_420><loc_108>First item</list_item></unordered_list>\
<otsl><loc_57><loc_120><loc_420><loc_160><caption><loc_57><loc_115><loc_420><loc_119>Table 1. HPO results.</caption><ched>Col A<ched>Col B<nl><fcel>1<fcel>2<nl></otsl>\
<page_footer><loc_57><loc_280><loc_420><loc_288>7</page_footer></doctag>";
        let fragment = strip_wrappers(page);
        assert!(looks_like_doctags(&fragment));
        let md = docling_core::doctags::parse(&fragment).export_to_markdown();
        // Section level 1 → "##" (docling parity; bare "#" is the title).
        assert!(md.contains("## Optimized Table Tokenization"), "md: {md:?}");
        assert!(!md.contains("### Optimized"), "md: {md:?}");
        // The in-otsl caption becomes a paragraph before the table.
        assert!(
            md.find("Table 1. HPO results.").unwrap() < md.find("Col A").unwrap(),
            "caption must precede the table: {md:?}"
        );
        assert!(md.contains("Body with A & B & C."), "md: {md:?}");
        assert!(md.contains("- First item"), "md: {md:?}");
        assert!(md.contains("Col A |"), "md: {md:?}");
        // Furniture (page footer) stays out of the Markdown body.
        assert!(!md.contains("\n7\n"), "md: {md:?}");
    }

    #[test]
    fn prose_fallback_wraps_untagged_lines() {
        assert_eq!(
            prose_fallback("First line.\n\nSecond line."),
            "<text>First line.</text>\n<text>Second line.</text>"
        );
        // Tagged content is left for the DocLang reader untouched.
        assert_eq!(prose_fallback("<text>hi</text>"), "<text>hi</text>");
    }
}
