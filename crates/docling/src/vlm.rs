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
//! exponential backoff. A page whose request still fails, or whose answer was
//! cut off (`finish_reason` `length` / `content_filter`), is recorded as a
//! page error and the result is a `PartialSuccess` — docling's
//! `VlmPipeline._determine_status` (docling#4561, docling#4778; #762, #763);
//! only a document where no page produced output fails.

use std::io::Cursor;
use std::time::Duration;

use docling_core::{DoclingDocument, Node};

use crate::backend::doclang::DoclangBackend;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::result::{ConversionResult, ConversionStatus, ErrorItem};
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

/// Unlimited-OCR's official model-card prompt (docling#4037), kept verbatim:
/// the model answers other phrasings — generic "convert to markdown" included —
/// with an empty completion and finish_reason "stop".
pub const UNLIMITED_OCR_PROMPT: &str = "<image>document parsing.";

/// Chandra's layout prompt (docling's `CHANDRA_OCR_LAYOUT_PROMPT`), verbatim.
pub const CHANDRA_PROMPT: &str = concat!(
    "OCR this image to HTML, arranged as layout blocks. Each layout block should be ",
    "a div with the data-bbox attribute representing the bounding box of the block in ",
    "x0 y0 x1 y1 format. Bboxes are normalized 0-1000. The data-label attribute is ",
    "the label for the block.\n\n",
    "Use the following labels:\n",
    "- Caption\n- Footnote\n- Equation-Block\n- List-Group\n- Page-Header\n",
    "- Page-Footer\n- Image\n- Section-Header\n- Table\n- Text\n- Complex-Block\n",
    "- Code-Block\n- Form\n- Table-Of-Contents\n- Figure\n- Chemical-Block\n",
    "- Diagram\n- Bibliography\n- Blank-Page\n\n",
    "Only use these tags ['math', 'br', 'i', 'b', 'u', 'del', 'sup', 'sub', 'table', 'tr', 'td', ",
    "'p', 'th', 'div', 'pre', 'h1', 'h2', 'h3', 'h4', 'h5', 'ul', 'ol', 'li', ",
    "'input', 'a', 'span', 'img', 'hr', 'tbody', 'small', 'caption', 'strong', ",
    "'thead', 'big', 'code', 'chem'], ",
    "and these attributes ['class', 'colspan', 'rowspan', 'display', 'checked', 'type', 'border', ",
    "'value', 'style', 'href', 'alt', 'align', 'data-bbox', 'data-label'].\n\n",
    "Guidelines:\n",
    "* Inline math: Surround math with <math>...</math> tags. Math expressions ",
    "should be rendered in KaTeX-compatible LaTeX. Use display for block math.\n",
    "* Tables: Use colspan and rowspan attributes to match table structure.\n",
    "* Formatting: Maintain consistent formatting with the image, including spacing, ",
    "indentation, subscripts/superscripts, and special characters.\n",
    "* Images: Include a description of any images in the alt attribute of an <img> tag. ",
    "Do not fill out the src property. Describe in detail inside the div tag. ",
    "Also convert charts to high fidelity data, and convert diagrams to mermaid.\n",
    "* Forms: Mark checkboxes and radio buttons properly.\n",
    "* Text: join lines together properly into paragraphs using <p>...</p> tags. ",
    "Use <br> tags for line breaks within paragraphs, but only when absolutely ",
    "necessary to maintain meaning.\n",
    "* Chemistry: Use <chem>...</chem> tags for chemical formulas with reactive SMILES.\n",
    "* Lists: Preserve indents and proper list markers.\n",
    "* Use the simplest possible HTML structure that accurately represents the content ",
    "of the block.\n",
    "* Make sure the text is accurate and easy for a human to read and interpret. ",
    "Reading order should be correct and natural."
);

/// The default prompt for a model, by name (#322): the known grammars ship
/// their official prompts, everything else keeps the DocLang-eliciting
/// default. `DOCLING_RS_VLM_PROMPT` / the `prompt` option override all of it.
fn default_prompt_for(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.contains("unlimited") {
        UNLIMITED_OCR_PROMPT
    } else if lower.contains("chandra") {
        CHANDRA_PROMPT
    } else {
        DEFAULT_VLM_PROMPT
    }
}

impl VlmOptions {
    /// Build options from explicit values, falling back to the
    /// `DOCLING_RS_VLM_*` environment. Endpoint and model are required —
    /// there is no sensible default server to talk to.
    pub fn resolve(
        endpoint: Option<String>,
        model: Option<String>,
    ) -> Result<Self, ConversionError> {
        let env = docling_core::env::nonempty;
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
        chat_url(&self.endpoint)
    }
}

pub use crate::options::{PictureDescriptionMode, CAPTION_PROMPT, OCR_PROMPT};

/// The picture-description pass (#670): every picture of a document — a
/// DOCX/PPTX/XLSX/HTML embedded image, a PDF figure crop, a sampled video
/// frame — goes to an OpenAI-compatible vision endpoint and the answer
/// becomes its description annotation (`meta.description` in docling JSON,
/// printed after the caption in Markdown). MarkItDown's `llm_client` /
/// `llm_model` / `llm_prompt` and its markitdown-ocr plugin, on every format
/// and surface. Endpoint, model and key fall back to the `DOCLING_RS_VLM_*`
/// environment like the page pipeline's; the prompt does not
/// (`DOCLING_RS_VLM_PROMPT` asks for a page's markup, not a caption).
#[derive(Debug, Clone)]
pub struct PictureDescriptionOptions {
    /// The server's `/v1` base or the full `…/chat/completions` URL.
    pub endpoint: String,
    /// Model name as the server knows it.
    pub model: String,
    /// Bearer token, if the endpoint wants one.
    pub api_key: Option<String>,
    /// The instruction sent with every picture; `None` = the mode's prompt.
    pub prompt: Option<String>,
    /// Caption or OCR.
    pub mode: PictureDescriptionMode,
    /// Pictures in flight at once (default 1; the endpoint's rate limit sets
    /// the ceiling — 429s retry with backoff).
    pub concurrency: usize,
}

impl PictureDescriptionOptions {
    /// Build options from explicit values, the endpoint, model and key
    /// falling back to the `DOCLING_RS_VLM_*` environment. Endpoint and
    /// model are required.
    pub fn resolve(
        endpoint: Option<String>,
        model: Option<String>,
    ) -> Result<Self, ConversionError> {
        let env = docling_core::env::nonempty;
        let endpoint = endpoint
            .or_else(|| env("DOCLING_RS_VLM_ENDPOINT"))
            .ok_or_else(|| {
                ConversionError::Parse(
                    "picture description: no endpoint (pass --picture-description-endpoint or set \
                     DOCLING_RS_VLM_ENDPOINT)"
                        .into(),
                )
            })?;
        let model = model
            .or_else(|| env("DOCLING_RS_VLM_MODEL"))
            .ok_or_else(|| {
                ConversionError::Parse(
                    "picture description: no model (pass --picture-description-model or set \
                     DOCLING_RS_VLM_MODEL)"
                        .into(),
                )
            })?;
        Ok(Self {
            endpoint,
            model,
            api_key: env("DOCLING_RS_VLM_API_KEY"),
            prompt: None,
            mode: PictureDescriptionMode::default(),
            concurrency: 1,
        })
    }

    /// The prompt in force: the explicit one, else the mode's.
    pub fn prompt(&self) -> &str {
        self.prompt.as_deref().unwrap_or(self.mode.default_prompt())
    }

    /// `created_by` of the descriptions this produces: the model name.
    pub fn provenance(&self) -> String {
        self.model.clone()
    }

    /// Ask the endpoint about one image (`mimetype` as the data URI says
    /// it). The retry policy is the page loop's; an empty answer is `None`,
    /// the way MarkItDown reads a blank completion as "no text".
    pub fn describe(
        &self,
        agent: &ureq::Agent,
        mimetype: &str,
        image: &[u8],
    ) -> Result<Option<String>, RequestError> {
        let req = ImageRequest {
            url: chat_url(&self.endpoint),
            model: &self.model,
            api_key: self.api_key.as_deref(),
            prompt: self.prompt(),
            max_tokens: None,
            keep_special: false,
        };
        // A cut-off description is still the model's best answer about the
        // picture; only the page pipeline reports `finish_reason`.
        let text = request_image(agent, &req, mimetype, image)?.text;
        let text = text.trim();
        Ok((!text.is_empty()).then(|| text.to_string()))
    }
}

/// Why a request produced no answer: the endpoint rejected this image (a
/// 4xx, a malformed body — the next picture may fare better), or it could
/// not be reached at all (transport errors, 5xx and 408/429 exhausted their
/// retries, our own timeout — the next picture will not fare better).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    Rejected(String),
    Unavailable(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(m) | Self::Unavailable(m) => f.write_str(m),
        }
    }
}

impl From<RequestError> for String {
    fn from(e: RequestError) -> String {
        e.to_string()
    }
}

/// The `chat/completions` URL for an endpoint given as the `/v1` base or in
/// full.
fn chat_url(endpoint: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

/// Convert a PDF or image through the remote VLM: the document alone, see
/// [`convert_vlm_result`] for the pages that failed or came back cut off.
pub fn convert_vlm(
    source: &SourceDocument,
    opts: &VlmOptions,
) -> Result<DoclingDocument, ConversionError> {
    convert_vlm_result(source, opts).map(|r| r.document)
}

/// Convert a PDF or image through the remote VLM. PDF pages render via
/// pdfium at the ML pipeline's scale; a standalone image is sent as-is (it is
/// its own page).
///
/// A page whose request fails (a 4xx, retries exhausted, our timeout) is left
/// out and recorded as `VLM inference failed: …`; a page the server cut off
/// (`finish_reason` `length` — `max_tokens` — or `content_filter`) keeps its
/// partial text and is recorded as `VLM output incomplete (stop_reason=…)`.
/// Either makes the result a [`ConversionStatus::PartialSuccess`] whose
/// errors carry the page numbers — docling's `VlmPipeline._determine_status`
/// (docling#4561, docling#4778). Only when every page failed is it an error.
pub fn convert_vlm_result(
    source: &SourceDocument,
    opts: &VlmOptions,
) -> Result<ConversionResult, ConversionError> {
    let agent = agent();
    // Each answered page with its source page number and size (#1146).
    let mut fragments: Vec<PageAnswer> = Vec::new();
    let mut errors: Vec<ErrorItem> = Vec::new();
    // (page, reason) of the pages that produced no output.
    let mut failed: Vec<(usize, String)> = Vec::new();
    let mut pages = 0usize;
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
            // A page that fails is recorded and the walk goes on: the pages
            // around it are as good as they were (docling#4561).
            docling_pdf::pdfium_backend::for_each_page::<docling_pdf::PdfError, _>(
                &source.bytes,
                None,
                true,  // render page images — they are the whole input here
                false, // the text layer never reaches the model: skip its decode
                range,
                |i, _total, page| {
                    pages += 1;
                    let step =
                        encode_png(&page.image).and_then(|png| request_page(&agent, opts, &png));
                    let size = (page.width, page.height);
                    take_page(i + 1, size, step, &mut fragments, &mut errors, &mut failed);
                    Ok(())
                },
            )
            .map_err(pdf_err)?;
        }
        InputFormat::Image => {
            // The image file is already the page; no re-encode, no pdfium.
            pages = 1;
            let step = request_page(&agent, opts, &source.bytes);
            // An image input is page 1 at its pixel size.
            let size = image::ImageReader::new(Cursor::new(&source.bytes))
                .with_guessed_format()
                .ok()
                .and_then(|r| r.into_dimensions().ok())
                .map_or((0.0, 0.0), |(w, h)| (w as f32, h as f32));
            take_page(1, size, step, &mut fragments, &mut errors, &mut failed);
        }
        other => {
            return Err(ConversionError::Parse(format!(
                "vlm pipeline converts PDF and image inputs (got {other:?})"
            )));
        }
    }
    // No page produced output: a failed conversion, not a partial result
    // (docling's `ConversionStatus.FAILURE`).
    if pages > 0 && failed.len() == pages {
        let (page, reason) = &failed[0];
        let more = match pages {
            1 => String::new(),
            n => format!(" (all {n} pages failed)"),
        };
        return Err(ConversionError::Parse(format!(
            "vlm: page {page}: {reason}{more}"
        )));
    }
    // Two grammars come back from the wire. DocTags (granite-docling-class
    // models: loc tokens, unclosed OTSL markers) goes to docling-core's
    // dedicated tolerant parser, which keeps geometry and span structure
    // (#152). Proper DocLang XML — or plain prose — takes the DocLang
    // reader path a `.dclg` file would. One DocTags-shaped page routes the
    // whole document: mixing grammars page-to-page is model misbehavior,
    // and the DocTags parser degrades to paragraphs on non-DocTags input.
    // The model-specific grammars go first (#322): their token shapes are
    // unambiguous, and each parser shares the tolerance contract — hostile
    // output degrades to text, never to an error.
    // Each page is parsed on its own and the pages concatenated, as
    // docling's `_concatenate_page_documents` does, so every page's items
    // follow a `PageInfo` with its source page number and size: their boxes
    // become `prov` on the right page, and `pages` holds exactly the pages
    // converted — a `--pages 3-4` range included, a failed page leaving the
    // numbers after it unchanged (#1146, docling#4498).
    let texts: Vec<&str> = fragments.iter().map(|p| p.text.as_str()).collect();
    let doc = if texts.iter().any(|f| crate::backend::looks_like_chandra(f)) {
        // Chandra layout HTML (`<div data-bbox=… data-label=…>`).
        assemble_pages(&source.name, &fragments, false, |text| {
            crate::backend::chandra_pages(&source.name, &[text.to_string()])
        })
    } else if texts.iter().any(|f| {
        crate::backend::is_unlimited_ocr_markdown(f) || crate::backend::is_deepseek_markdown(f)
    }) {
        // Unlimited-OCR grounding output labels blocks the way DeepSeek-OCR
        // does (docling#3944): normalize its `<|det|>label [x,y,x,y]<|/det|>`
        // shape into the DeepSeek one and share that parser. Native
        // DeepSeek-shaped responses take the same path directly.
        assemble_pages(&source.name, &fragments, false, |text| {
            crate::backend::deepseek_text(&source.name, &crate::backend::normalize_unlimited(text))
        })
    } else if texts.iter().any(|f| looks_like_doctags(f)) {
        // DocTags boxes are on a 0–500 grid; the document's are 0–511.
        let mut doc = assemble_pages(&source.name, &fragments, true, |text| {
            docling_core::doctags::parse_pages([text])
        });
        rescale_located(&mut doc.nodes);
        doc
    } else {
        assemble_pages(&source.name, &fragments, false, |text| {
            parse_doclang_fragments(&source.name, &[text.to_string()])
        })
    };
    if doc.nodes.is_empty() {
        // Some request succeeded, so this is a content problem, not a
        // transport one: the model answered with nothing our reader keeps.
        // An empty stdout with exit 0 buried that; say it loudly instead.
        return Err(ConversionError::Parse(
            "vlm: the model's responses contained no parseable DocLang/DocTags/Chandra/\
             grounding blocks \
             (set DOCLING_RS_VLM_DEBUG=1 to print raw responses; a generic VLM may need \
             a DOCLING_RS_VLM_PROMPT that describes the expected markup)"
                .into(),
        ));
    }
    let status = if errors.is_empty() {
        ConversionStatus::Success
    } else {
        ConversionStatus::PartialSuccess
    };
    Ok(ConversionResult {
        document: doc,
        status,
        input_name: source.name.clone(),
        format: source.format,
        errors,
        redaction: None,
        pdf_repairs: None,
        ocr_status: None,
    })
}

/// One answered page: its 1-based source page number, its size (PDF points;
/// an image input's pixels) and the model's markup.
struct PageAnswer {
    page_no: usize,
    width: f32,
    height: f32,
    text: String,
}

/// The pages parsed one by one (`parse`) and concatenated, each behind a
/// [`Node::PageInfo`] with its number and size (#1146). A parser's own page
/// markers — the DocLang reader lays a document out on synthetic 512 × 512
/// pages — give way to the real one, and its item tree to the flat nodes,
/// which concatenate. `page_breaks` keeps the [`Node::PageBreak`] the
/// DocTags path put between pages.
fn assemble_pages(
    name: &str,
    pages: &[PageAnswer],
    page_breaks: bool,
    parse: impl Fn(&str) -> DoclingDocument,
) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    for (i, page) in pages.iter().enumerate() {
        if i > 0 && page_breaks {
            doc.nodes.push(Node::PageBreak);
        }
        doc.nodes.push(Node::PageInfo {
            page_no: page.page_no,
            width: page.width,
            height: page.height,
        });
        let part = parse(&page.text);
        doc.nodes.extend(
            part.nodes
                .into_iter()
                .filter(|n| !matches!(n, Node::PageInfo { .. })),
        );
        doc.links.extend(part.links);
    }
    doc
}

/// Move DocTags' `loc_` boxes (a 0–500 grid) onto the 0–511 grid
/// [`Node::Located`] carries, which the JSON export denormalizes by 512
/// and DocLang writes as is.
fn rescale_located(nodes: &mut [Node]) {
    for node in nodes {
        match node {
            Node::Located { location, inner } => {
                for v in location.iter_mut() {
                    *v = ((u32::from(*v).min(500) * 512 + 250) / 500).min(511) as u16;
                }
                rescale_located(std::slice::from_mut(inner));
            }
            Node::Group { children, .. } | Node::PictureChildren(children) => {
                rescale_located(children)
            }
            Node::Prov { inner, .. }
            | Node::Track { inner, .. }
            | Node::Furniture { inner, .. }
            | Node::Commented { inner, .. }
            | Node::ScriptText { inner, .. }
            | Node::DoclangOnly(inner) => rescale_located(std::slice::from_mut(inner)),
            _ => {}
        }
    }
}

/// File one page's answer: its markup into `fragments`, a cut-off answer
/// also as a page error, a failed request as a page error and in `failed`.
fn take_page(
    page_no: usize,
    (width, height): (f32, f32),
    step: Result<Completion, String>,
    fragments: &mut Vec<PageAnswer>,
    errors: &mut Vec<ErrorItem>,
    failed: &mut Vec<(usize, String)>,
) {
    match step {
        Ok(completion) => {
            if let Some(reason) = completion.incomplete() {
                errors.push(ErrorItem::vlm_page(
                    page_no,
                    format!("VLM output incomplete (stop_reason={reason})."),
                ));
            }
            fragments.push(PageAnswer {
                page_no,
                width,
                height,
                text: strip_wrappers(&completion.text),
            });
        }
        Err(e) => {
            errors.push(ErrorItem::vlm_page(
                page_no,
                format!("VLM inference failed: {e}."),
            ));
            failed.push((page_no, e));
        }
    }
}

fn pdf_err(e: docling_pdf::PdfError) -> ConversionError {
    ConversionError::with_source("pdf", e)
}

pub(crate) fn agent() -> ureq::Agent {
    // A VLM can chew on a dense page for minutes — and a CPU-served endpoint
    // for many more: #311's measurement clocked a routine page at 587 s on a
    // desktop CPU, a hair under this default. `DOCLING_RS_VLM_TIMEOUT`
    // (seconds) raises the per-request cap for such endpoints; retries make a
    // too-small value expensive (4 attempts each grind to the cap while the
    // server keeps generating for a client that already hung up).
    let timeout = docling_core::env::parse("DOCLING_RS_VLM_TIMEOUT").unwrap_or(600);
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(timeout)))
        // Keep non-2xx as inspectable responses for the retry decision.
        .http_status_as_error(false)
        .build()
        .into()
}

/// POST one page image, return the model's answer. Retries transport errors,
/// 408/429 and 5xx with exponential backoff (2s/4s/8s); other statuses and a
/// malformed body fail immediately.
fn request_page(
    agent: &ureq::Agent,
    opts: &VlmOptions,
    image: &[u8],
) -> Result<Completion, String> {
    // Unlimited-OCR's grounding markers are special tokens: without this vLLM
    // extension flag the server strips them from the completion and the page
    // comes back as flat text (docling#3944's api_overrides).
    let req = ImageRequest {
        url: opts.url(),
        model: &opts.model,
        api_key: opts.api_key.as_deref(),
        prompt: opts
            .prompt
            .as_deref()
            .unwrap_or(default_prompt_for(&opts.model)),
        max_tokens: Some(opts.max_tokens),
        keep_special: opts.model.to_ascii_lowercase().contains("unlimited"),
    };
    request_image(agent, &req, "image/png", image).map_err(String::from)
}

/// A model's answer: `choices[0].message.content` and the choice's
/// `finish_reason`.
#[derive(Debug)]
struct Completion {
    text: String,
    finish_reason: Option<String>,
}

impl Completion {
    /// The stop reason of an answer the server cut off — docling's
    /// `_map_stop_reason`: `length` (`max_tokens` reached) and
    /// `content_filter`; every other reason is a complete answer.
    fn incomplete(&self) -> Option<&str> {
        self.finish_reason
            .as_deref()
            .filter(|r| matches!(*r, "length" | "content_filter"))
    }
}

/// One image in, one completion out — what the page loop and the picture
/// description pass both send.
struct ImageRequest<'a> {
    url: String,
    model: &'a str,
    api_key: Option<&'a str>,
    prompt: &'a str,
    /// `max_tokens` when set; omitted from the body otherwise (the server's
    /// default, as MarkItDown sends it).
    max_tokens: Option<usize>,
    /// vLLM's `skip_special_tokens: false`.
    keep_special: bool,
}

/// POST `image` (a data URI of `mimetype`) with the request's prompt and
/// return `choices[0]`'s content and finish reason. Transport errors, 408/429 and 5xx
/// retry with exponential backoff (2s/4s/8s) and come back as
/// [`RequestError::Unavailable`] once exhausted; other statuses and a
/// malformed body are [`RequestError::Rejected`] at once.
fn request_image(
    agent: &ureq::Agent,
    req: &ImageRequest<'_>,
    mimetype: &str,
    image: &[u8],
) -> Result<Completion, RequestError> {
    let data_uri = format!(
        "data:{mimetype};base64,{}",
        docling_core::base64::encode(image)
    );
    let mut body = serde_json::json!({
        "model": req.model,
        // Deterministic-ish output: sampling noise only hurts a structured
        // markup task (docling's ApiVlmOptions ships temperature 0 too).
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": req.prompt },
                { "type": "image_url", "image_url": { "url": data_uri } },
            ],
        }],
    });
    if let Some(n) = req.max_tokens {
        body["max_tokens"] = serde_json::Value::from(n);
    }
    if req.keep_special {
        body["skip_special_tokens"] = serde_json::Value::Bool(false);
    }
    // DOCLING_RS_VLM_EXTRA_BODY: a JSON object merged into the request at the
    // top level — the escape hatch for server-specific knobs the OpenAI shape
    // doesn't cover. The motivating case: vLLM's "skip_special_tokens": false,
    // without which servers detokenize away granite-docling's DocTags
    // structure tokens and only loc tokens + bare text survive.
    if let Some(extra) = docling_core::env::nonempty("DOCLING_RS_VLM_EXTRA_BODY") {
        match serde_json::from_str::<serde_json::Value>(&extra) {
            Ok(serde_json::Value::Object(map)) => {
                for (k, v) in map {
                    body[k] = v;
                }
            }
            _ => {
                return Err(RequestError::Rejected(
                    "DOCLING_RS_VLM_EXTRA_BODY is not a JSON object; fix or unset it".into(),
                ))
            }
        }
    }
    let url = &req.url;
    let mut delay = Duration::from_secs(2);
    let mut last_err = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(delay);
            delay *= 2;
        }
        let mut post = agent.post(url).header("content-type", "application/json");
        if let Some(key) = req.api_key {
            post = post.header("authorization", &format!("Bearer {key}"));
        }
        // Hand-serialized body: the crate pulls ureq without its `json`
        // feature (fetch-images doesn't need it), and one to_string keeps it
        // that way.
        let payload = serde_json::to_string(&body).expect("static json shape");
        match post.send(payload.as_bytes()) {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let text = resp
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| RequestError::Unavailable(format!("{url}: read response: {e}")))?;
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
                    return Err(RequestError::Rejected(format!(
                        "{url}: HTTP {status}: {}",
                        text.chars().take(300).collect::<String>()
                    )));
                }
                let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                    RequestError::Rejected(format!("{url}: malformed JSON response: {e}"))
                })?;
                let content = parsed["choices"][0]["message"]["content"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| {
                        RequestError::Rejected(format!(
                            "{url}: no choices[0].message.content in response"
                        ))
                    });
                let finish_reason = parsed["choices"][0]["finish_reason"]
                    .as_str()
                    .map(str::to_string);
                if docling_core::env::flag("DOCLING_RS_VLM_DEBUG") {
                    match &content {
                        Ok(c) => eprintln!(
                            "vlm: raw model response ({} chars, finish_reason {}):\n{c}\n---",
                            c.len(),
                            finish_reason.as_deref().unwrap_or("unset")
                        ),
                        Err(_) => eprintln!("vlm: raw endpoint body:\n{text}\n---"),
                    }
                }
                return content.map(|text| Completion {
                    text,
                    finish_reason,
                });
            }
            // Our own timeout is not retryable: the model is deterministic,
            // so the same page grinds to the same timeout again — and against
            // a single-threaded server the retry queues *behind* the orphaned
            // generation it abandoned, multiplying the wasted wall-clock ×4
            // (#311's GPU run: a 3300 s page vs. a 1800 s cap). Raise
            // DOCLING_RS_VLM_TIMEOUT instead; the error says so.
            Err(ureq::Error::Timeout(_)) => {
                return Err(RequestError::Unavailable(format!(
                    "{url}: request exceeded the {}s cap (DOCLING_RS_VLM_TIMEOUT raises \
                     it; the server may still be finishing the abandoned generation)",
                    docling_core::env::parse("DOCLING_RS_VLM_TIMEOUT").unwrap_or(600_u64)
                )));
            }
            Err(e) => {
                last_err = format!("{url}: {e} (attempt {})", attempt + 1);
            }
        }
    }
    Err(RequestError::Unavailable(format!(
        "giving up after 4 attempts: {last_err}"
    )))
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

/// Assemble the non-DocTags fragments (proper DocLang XML, or the prose a model
/// emits when it ignores the markup instruction) into a document.
///
/// The DocLang reader is a *strict* XML parser: a misbehaving model that leaves
/// a `<heading>`/`<text>` unclosed or nests a stray root produces markup it
/// rejects outright (`expected 'heading' tag, not 'doclang' …`), which — when
/// propagated — fails the entire conversion on one bad page. The DocTags path
/// never does that (#152: hostile model output, best-effort document out), so
/// neither should this one: on a parse error, salvage the fragments through the
/// tolerant DocTags parser, which degrades broken/unknown tags to text and
/// never errors. Well-formed DocLang still takes the strict reader, keeping its
/// heading levels and table structure.
fn parse_doclang_fragments(name: &str, fragments: &[String]) -> DoclingDocument {
    let body: Vec<String> = fragments.iter().map(|f| prose_fallback(f)).collect();
    let xml = format!("<doclang version=\"0.7\">\n{}\n</doclang>", body.join("\n"));
    let synthetic = SourceDocument::from_bytes(name, InputFormat::XmlDoclang, xml.into_bytes());
    let mut doc = match DoclangBackend.convert(&synthetic) {
        Ok(doc) => doc,
        Err(_) => docling_core::doctags::parse_pages(fragments.iter().map(String::as_str)),
    };
    doc.name = name.to_string();
    doc
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
    use super::{default_prompt_for, CHANDRA_PROMPT, DEFAULT_VLM_PROMPT, UNLIMITED_OCR_PROMPT};
    use super::{looks_like_doctags, parse_doclang_fragments, prose_fallback, strip_wrappers};

    /// A misbehaving model can emit malformed DocLang — here an unclosed
    /// `<heading>` — that the strict XML reader rejects with
    /// `expected 'heading' tag, not 'doclang'`. That hard error dropped the
    /// whole `normal_4pages` fixture from the #153 corpus run; the fallback
    /// must salvage the page's text instead of failing the conversion.
    #[test]
    fn malformed_doclang_degrades_instead_of_erroring() {
        let frag = "<heading level=\"1\">Unclosed title\n<text>Body line.</text>".to_string();
        let md = parse_doclang_fragments("normal_4pages.pdf", &[frag]).export_to_markdown();
        // Best-effort salvage via the tolerant DocTags parser: the text that
        // the strict reader would have thrown away with the whole page survives.
        assert!(md.contains("Body line."), "md: {md:?}");
        assert!(md.contains("Unclosed title"), "md: {md:?}");
    }

    /// Well-formed DocLang still takes the strict reader, which keeps the
    /// heading *level* (the tolerant fallback would flatten it to text).
    #[test]
    fn wellformed_doclang_keeps_structure() {
        let frag = "<heading level=\"2\">Sec</heading>\n<text>Para.</text>".to_string();
        let md = parse_doclang_fragments("d.pdf", &[frag]).export_to_markdown();
        assert!(md.contains("## Sec"), "md: {md:?}");
        assert!(md.contains("Para."), "md: {md:?}");
    }

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
    /// #322: known model names get their official prompts; everything else
    /// keeps the DocLang default.
    #[test]
    fn model_names_pick_their_official_prompts() {
        assert_eq!(default_prompt_for("unlimited-ocr"), UNLIMITED_OCR_PROMPT);
        assert_eq!(
            default_prompt_for("baidu/Unlimited-OCR"),
            UNLIMITED_OCR_PROMPT
        );
        assert_eq!(default_prompt_for("chandra-ocr-2"), CHANDRA_PROMPT);
        assert_eq!(default_prompt_for("granite-docling"), DEFAULT_VLM_PROMPT);
        assert!(CHANDRA_PROMPT.starts_with("OCR this image to HTML"));
        assert_eq!(UNLIMITED_OCR_PROMPT, "<image>document parsing.");
    }
}
