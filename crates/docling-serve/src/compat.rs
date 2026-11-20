//! Upstream docling-serve's API (#615): the routes and shapes Open WebUI,
//! n8n, Dify and LangChain's docling-serve client are written against, on
//! top of this server's own `/v1/convert`.
//!
//! - `POST /v1/convert/file` — multipart `files` (repeatable) plus option
//!   fields; `POST /v1/convert/source` — JSON `{"sources": […], "options":
//!   {…}, "target": {…}}` (also the older `http_sources` / `file_sources`
//!   shape). One document answers docling's `ConvertDocumentResponse`
//!   (`document.md_content` & co., `status`, `errors`, `processing_time`,
//!   `timings`); several, or `target_type=zip` / `{"kind": "zip"}`, a zip of
//!   `<stem>.<ext>` outputs. `/v1alpha/…` aliases serve docling-serve 0.x
//!   clients.
//! - `POST /v1/convert/{file,source}/async` → docling's `TaskStatusResponse`,
//!   `GET /v1/status/poll/{task_id}` the same shape, and the shared
//!   `GET /v1/result/{task_id}` the `ConvertDocumentResponse`.
//!
//! A document that fails to convert is a `200` envelope with `status:
//! "failure"` and its `errors` — what these clients read — not a 4xx. The
//! options are upstream's names and value types, mapped onto
//! [`docling::ConvertOptions`] ([`parse_fields`]); what has no equivalent
//! here (`pdf_backend`, `table_mode`, `abort_on_error`, the picture
//! description and preset knobs, an OCR engine or language this build does
//! not have) is accepted and ignored, so a client's `DOCLING_PARAMS` never
//! turns a conversion into an error.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{FromRequest, Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use docling::{DoclingDocument, ImageMode};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    api_error_message, body_error, convert_document, html_string, markdown_string, multipart_error,
    parse_source_specs, passthrough, require_outbound, sources_from_named_bytes, submit_job,
    text_string, trim_heap, ApiError, AppState, ConvertOptions, Converted, JobState, SourceItem,
    StoredResponse,
};

/// One of upstream's `to_formats` this server fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Md,
    Json,
    Html,
    Text,
    Doctags,
    Doclang,
}

impl Format {
    /// Upstream's `OutputFormat` value; formats this compatibility layer
    /// does not render (`yaml`, `vtt` — `/v1/convert?to=vtt` does, #614 — …)
    /// are `None` and ignored.
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Self::Md,
            "json" => Self::Json,
            "html" | "html_split_page" => Self::Html,
            "text" | "txt" => Self::Text,
            "doctags" => Self::Doctags,
            "doclang" => Self::Doclang,
            _ => return None,
        })
    }

    /// The `ExportDocumentResponse` field it fills.
    fn key(self) -> &'static str {
        match self {
            Self::Md => "md_content",
            Self::Json => "json_content",
            Self::Html => "html_content",
            Self::Text => "text_content",
            Self::Doctags => "doctags_content",
            Self::Doclang => "doclang_content",
        }
    }

    /// The file extension of its entry in a zip answer (docling-serve's).
    fn ext(self) -> &'static str {
        match self {
            Self::Md => "md",
            Self::Json => "json",
            Self::Html => "html",
            Self::Text => "txt",
            Self::Doctags => "doctags",
            Self::Doclang => "dclg.xml",
        }
    }
}

/// What a compat request asks for beyond the shared conversion options.
#[derive(Clone, Debug)]
pub(crate) struct Ask {
    formats: Vec<Format>,
    /// `target_type=zip` / `{"kind": "zip"}` — a zip even for one document.
    zip: bool,
    image_mode: ImageMode,
}

fn truthy(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn bad_value(name: &str, v: &str) -> ApiError {
    ApiError::Bad(format!("{name}: cannot read {v:?}"))
}

/// Map upstream's option fields — repeated `(name, value)` pairs, a list
/// option sending one pair per item, as multipart forms and the JSON options
/// object (flattened by [`json_fields`]) both arrive — onto this server's
/// options. docling's names are read first (`do_ocr` → `no_ocr`,
/// `force_ocr`, `do_table_structure`, `page_range`, `image_export_mode`,
/// `md_page_break_placeholder`, `do_pdf_heading_hierarchy`,
/// `md_compact_tables`, the enrichment switches, `pipeline`,
/// `ocr_engine` / `ocr_lang`); any other name this server knows as an option
/// of its own is set as on `/v1/convert` (`strict`, `password`, …); the
/// rest is ignored.
pub(crate) fn parse_fields(fields: &[(String, String)]) -> Result<(ConvertOptions, Ask), ApiError> {
    let mut opts = ConvertOptions::default();
    let mut formats: Vec<Format> = Vec::new();
    let mut zip = false;
    // docling-serve's form default (`ImageRefMode.EMBEDDED`).
    let mut image_mode = ImageMode::Embedded;
    let mut ocr_langs: Vec<String> = Vec::new();
    let mut page_range: Vec<String> = Vec::new();
    for (name, value) in fields {
        let flag = || truthy(value).ok_or_else(|| bad_value(name, value));
        let c = &mut opts.convert;
        match name.as_str() {
            "to_formats" => formats.extend(value.split(',').filter_map(Format::parse)),
            "image_export_mode" => {
                image_mode = match value.trim() {
                    "embedded" => ImageMode::Embedded,
                    // `referenced` writes image files next to the export —
                    // there is no such place in a response body.
                    "placeholder" | "referenced" => ImageMode::Placeholder,
                    _ => return Err(bad_value(name, value)),
                }
            }
            // Raw: a `\f` (Open WebUI's marker) is whitespace, not a value
            // to trim away.
            "md_page_break_placeholder" => c.page_break_placeholder = Some(value.clone()),
            "do_ocr" => c.no_ocr = Some(!flag()?),
            "force_ocr" => c.force_full_page_ocr = Some(flag()?),
            "do_table_structure" => c.no_table_former = Some(!flag()?),
            "do_pdf_heading_hierarchy" => c.heading_hierarchy = Some(flag()?),
            "md_compact_tables" => c.compact_tables = Some(flag()?),
            "do_code_enrichment" => c.do_code_enrichment = Some(flag()?),
            "do_formula_enrichment" => c.do_formula_enrichment = Some(flag()?),
            "do_picture_classification" => c.do_picture_classification = Some(flag()?),
            "document_timeout" => {
                c.document_timeout = Some(value.trim().parse().map_err(|_| bad_value(name, value))?)
            }
            "images_scale" => {
                c.images_scale = Some(value.trim().parse().map_err(|_| bad_value(name, value))?)
            }
            "page_range" => page_range.push(value.clone()),
            // EasyOCR/RapidOCR/macOS engines map onto the built-in PP-OCR;
            // the Tesseract spellings onto Tesseract.
            "ocr_engine" => {
                c.ocr_engine = matches!(value.trim(), "tesseract" | "tesserocr" | "tesseract_cli")
                    .then(|| "tesseract".to_string())
            }
            "ocr_lang" => ocr_langs.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string),
            ),
            // Only the two pipelines this server has; docling's `asr` /
            // `legacy` select by input format here.
            "pipeline" => {
                c.pipeline = match value.trim() {
                    "vlm" => Some("vlm".into()),
                    _ => None,
                }
            }
            "target_type" => zip = value.trim() == "zip",
            // docling's `PictureDescriptionApiOptions` as JSON (#670): its
            // `url` is the endpoint, `params.model` the model, an
            // `Authorization: Bearer …` header the key, `prompt` and
            // `concurrency` theirs. `do_picture_description` switches the
            // pass on, as upstream.
            "picture_description_api" => picture_description_api(c, value)?,
            // `picture_description_local` names a model to run in-process
            // (SmolVLM, granite-vision); there is no in-process VLM here, so
            // it is accepted and ignored — `do_picture_description` then
            // describes through the server's DOCLING_RS_VLM_* endpoint, if
            // any, as it would without it.
            "picture_description_local" => {}
            other if is_own_option(other) => opts.set_text(other, value)?,
            _ => {}
        }
    }
    if let Some(pages) = pages_from(&page_range) {
        opts.convert.pages = Some(pages);
    }
    pick_ocr_lang(&mut opts.convert, &ocr_langs);
    opts.convert.validate().map_err(crate::bad)?;
    if formats.is_empty() {
        formats.push(Format::Md);
    }
    // A format named twice (`to_formats=md` twice) is filled once.
    let mut seen = Vec::new();
    formats.retain(|f| {
        let new = !seen.contains(f);
        seen.push(*f);
        new
    });
    Ok((
        opts,
        Ask {
            formats,
            zip,
            image_mode,
        },
    ))
}

/// docling's `PictureDescriptionApiOptions` (a JSON object: `url`,
/// `headers`, `params`, `prompt`, `concurrency`, …) onto the
/// `picture_description_*` options.
fn picture_description_api(c: &mut docling::ConvertOptions, value: &str) -> Result<(), ApiError> {
    let v: Value =
        serde_json::from_str(value).map_err(|_| bad_value("picture_description_api", value))?;
    let Value::Object(map) = v else {
        return Err(bad_value("picture_description_api", value));
    };
    let text = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(url) = text(map.get("url")) {
        c.picture_description_endpoint = Some(url);
    }
    if let Some(model) = map.get("params").and_then(|p| text(p.get("model"))) {
        c.picture_description_model = Some(model);
    }
    if let Some(Value::Object(headers)) = map.get("headers") {
        let auth = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .and_then(|(_, v)| v.as_str());
        if let Some(token) = auth.and_then(|a| {
            a.strip_prefix("Bearer ")
                .or_else(|| a.strip_prefix("bearer "))
        }) {
            c.picture_description_api_key = Some(token.trim().to_string());
        }
    }
    if let Some(prompt) = text(map.get("prompt")) {
        c.picture_description_prompt = Some(prompt);
    }
    if let Some(n) = map.get("concurrency").and_then(Value::as_u64) {
        c.picture_description_concurrency = Some(n as usize);
    }
    Ok(())
}

/// Whether `name` is an option of this server's own (`/v1/convert`'s
/// spelling) that the compat routes pass through unchanged.
fn is_own_option(name: &str) -> bool {
    docling::ConvertOptions::field_names()
        .iter()
        .any(|f| f == name)
        || matches!(name, "strict" | "skip_ocr")
}

/// docling's `page_range` — a `[first, last]` pair (repeated field, or a
/// JSON array) or a `"A-B"` string — as this server's `"A-B"`. Upstream's
/// default `(1, sys.maxsize)` and anything starting at 1 that ends past any
/// real document is the whole document: `None`.
fn pages_from(values: &[String]) -> Option<String> {
    let (a, b) = match values {
        [one] if one.contains('-') => {
            let (a, b) = one.split_once('-')?;
            (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?)
        }
        [one] => {
            let n = one.trim().parse::<u64>().ok()?;
            (n, n)
        }
        [a, b] => (a.trim().parse().ok()?, b.trim().parse().ok()?),
        _ => return None,
    };
    if a <= 1 && b >= 1_000_000 {
        return None;
    }
    Some(format!("{a}-{}", b.min(1_000_000)))
}

/// docling's `ocr_lang` is a list of an engine's language codes (EasyOCR's
/// `en`/`de`, Tesseract's `eng`/`deu`). Under Tesseract the list joins into
/// its `-l` argument when every code is one Tesseract reads; under PP-OCR
/// the first code this build has a recognizer for wins (`en`, `zh`, …). A
/// list nothing here reads leaves the default — the conversion still runs.
fn pick_ocr_lang(c: &mut docling::ConvertOptions, langs: &[String]) {
    if langs.is_empty() {
        return;
    }
    let mut candidates: Vec<String> = Vec::new();
    if c.ocr_engine.as_deref() == Some("tesseract") {
        candidates.push(langs.join("+"));
    }
    candidates.extend(langs.iter().cloned());
    for candidate in candidates {
        c.ocr_lang = Some(candidate);
        if c.ocr_lang().is_ok() {
            return;
        }
    }
    c.ocr_lang = None;
}

/// A JSON options object as upstream's field pairs: a list becomes one pair
/// per item, a scalar one pair, `null` nothing, a nested object its JSON.
pub(crate) fn json_fields(options: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in options {
        let scalar = |v: &Value| match v {
            Value::String(s) => Some(s.clone()),
            Value::Null => None,
            other => Some(other.to_string()),
        };
        match value {
            Value::Array(items) => {
                out.extend(items.iter().filter_map(scalar).map(|v| (name.clone(), v)))
            }
            other => out.extend(scalar(other).map(|v| (name.clone(), v))),
        }
    }
    out
}

/// One document's `ExportDocumentResponse`: `filename` plus every content
/// field — the requested ones filled, the rest `null` as upstream sends
/// them. `doctags_content` stays `null`: this crate reads DocTags but has
/// no DocTags writer (DocLang, its successor, is `doclang_content`).
fn export_document(
    state: &AppState,
    filename: &str,
    document: Option<&DoclingDocument>,
    ask: &Ask,
    opts: &ConvertOptions,
) -> Value {
    let mut out = json!({
        "filename": filename,
        "md_content": null,
        "json_content": null,
        "html_content": null,
        "text_content": null,
        "doctags_content": null,
        "doclang_content": null,
    });
    let Some(doc) = document else {
        return out;
    };
    for f in &ask.formats {
        out[f.key()] = match f {
            Format::Md => json!(markdown_string(state, doc, ask.image_mode, opts)),
            Format::Json => doc.export_to_json_value(),
            Format::Html => json!(html_string(doc, ask.image_mode)),
            // #613: docling-core's `export_to_text`.
            Format::Text => json!(text_string(state, doc, opts)),
            Format::Doclang => json!(doc.export_to_doclang()),
            Format::Doctags => Value::Null,
        };
    }
    out
}

/// docling-serve's `ConvertDocumentResponse` for one conversion outcome. A
/// failed conversion is an envelope too (`status: "failure"`, the reason in
/// `errors[0].error_message`), never an HTTP error — what upstream's clients
/// read failures from.
fn envelope(
    state: &AppState,
    filename: &str,
    outcome: Result<Converted, ApiError>,
    ask: &Ask,
    opts: &ConvertOptions,
    started: Instant,
) -> Value {
    let (document, status, errors, confidence) = match &outcome {
        Ok(c) => (
            export_document(state, filename, Some(&c.document), ask, opts),
            c.status(),
            c.errors_json(),
            c.document.confidence.as_ref().map(|r| r.summary_json()),
        ),
        Err(_) => (
            export_document(state, filename, None, ask, opts),
            "failure",
            Value::Null,
            None,
        ),
    };
    let errors = match outcome {
        Err(e) => json!([{
            "component_type": "user_input",
            "module_name": "docling-rs",
            "error_message": api_error_message(e),
        }]),
        Ok(_) => errors,
    };
    let mut out = json!({
        "document": document,
        "status": status,
        "errors": errors,
        "processing_time": started.elapsed().as_secs_f64(),
        "timings": {},
    });
    if let Some(c) = confidence {
        out["confidence"] = c;
    }
    out
}

/// Convert every item and answer docling-serve's way: one document (and no
/// zip asked) → the `ConvertDocumentResponse` JSON; otherwise a zip of each
/// document's outputs as `<stem>.<ext>` (a failed one as
/// `<stem>.error.txt`), docling-serve's multi-document answer.
pub(crate) fn run(
    state: &AppState,
    items: Vec<(String, SourceItem)>,
    opts: &ConvertOptions,
    ask: &Ask,
) -> StoredResponse {
    let convert = |item: SourceItem| match item {
        Ok(source) => convert_document(state, source, opts),
        Err((_, e)) => Err(e),
    };
    if items.len() == 1 && !ask.zip {
        let (filename, item) = items.into_iter().next().expect("checked len");
        let started = Instant::now();
        let body = envelope(state, &filename, convert(item), ask, opts, started);
        return StoredResponse {
            errors: Vec::new(),
            content_type: "application/json",
            disposition: None,
            confidence: None,
            redaction: None,
            pdf_repairs: None,
            ocr_status: None,
            body: serde_json::to_vec(&body).expect("the envelope serializes"),
        };
    }
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (filename, item) in items {
        let stem = std::path::Path::new(&filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("document")
            .to_string();
        let mut unique = stem.clone();
        let mut n = 2;
        while !used.insert(unique.clone()) {
            unique = format!("{stem}-{n}");
            n += 1;
        }
        match convert(item) {
            Ok(c) => {
                let doc = export_document(state, &filename, Some(&c.document), ask, opts);
                for f in &ask.formats {
                    let body = match &doc[f.key()] {
                        Value::String(s) => s.clone().into_bytes(),
                        Value::Null => continue,
                        other => serde_json::to_vec_pretty(other).expect("JSON serializes"),
                    };
                    entries.push((format!("{unique}.{}", f.ext()), body));
                }
            }
            Err(e) => entries.push((
                format!("{unique}.error.txt"),
                api_error_message(e).into_bytes(),
            )),
        }
    }
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    StoredResponse {
        errors: Vec::new(),
        content_type: "application/zip",
        disposition: Some("attachment; filename=\"converted_docs.zip\"".into()),
        confidence: None,
        redaction: None,
        pdf_repairs: None,
        ocr_status: None,
        body: docling::dclx::zip_bytes(refs),
    }
}

/// The uploaded files (`files`, repeatable; `file` too) and every other part
/// as an option field. Query parameters count as fields as well.
async fn read_upload(
    body: axum::extract::Request,
    query: HashMap<String, String>,
) -> Result<(Vec<(String, SourceItem)>, Vec<(String, String)>), ApiError> {
    let mut multipart = Multipart::from_request(body, &())
        .await
        .map_err(|e| ApiError::Bad(format!("expected a multipart/form-data upload: {e}")))?;
    let mut fields: Vec<(String, String)> = query.into_iter().collect();
    let mut items: Vec<(String, SourceItem)> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| multipart_error("bad multipart field", e))?
    {
        let name = field.name().unwrap_or("").to_string();
        if matches!(name.as_str(), "files" | "file") {
            // Open WebUI names the part after the uploaded file's full path;
            // its last component carries the extension that selects the format.
            let file_name = field
                .file_name()
                .map(|n| n.rsplit(['/', '\\']).next().unwrap_or(n).to_string())
                .ok_or_else(|| ApiError::Bad("a files part needs a filename".into()))?;
            let bytes = field
                .bytes()
                .await
                .map_err(|e| multipart_error("reading upload", e))?;
            items.extend(named_items(&file_name, bytes.to_vec()));
        } else {
            let value = field
                .text()
                .await
                .map_err(|e| multipart_error(&format!("reading field {name}"), e))?;
            fields.push((name, value));
        }
    }
    if items.is_empty() {
        return Err(ApiError::Bad("missing 'files' part".into()));
    }
    Ok((items, fields))
}

/// A named upload's items with their envelope filenames: itself, or a
/// `.zip`'s documents (#557) under their archive labels.
fn named_items(file_name: &str, bytes: Vec<u8>) -> Vec<(String, SourceItem)> {
    let items = sources_from_named_bytes(file_name, bytes);
    if items.len() == 1 {
        return items
            .into_iter()
            .map(|i| (file_name.to_string(), i))
            .collect();
    }
    items
        .into_iter()
        .map(|i| {
            let label = match &i {
                Ok(source) => source.name.clone(),
                Err((label, _)) => label.clone(),
            };
            (label, i)
        })
        .collect()
}

/// `POST /v1/convert/source`'s body: docling-serve's `sources` (`kind`-tagged
/// `http` / `file` / object-store items), or the 0.x `http_sources` /
/// `file_sources` lists; `options`; and a `target` (`inbody` or `zip`).
#[derive(Deserialize)]
struct SourceRequest {
    #[serde(default)]
    sources: Vec<passthrough::SourceSpec>,
    #[serde(default)]
    http_sources: Vec<LegacyHttp>,
    #[serde(default)]
    file_sources: Vec<LegacyFile>,
    #[serde(default)]
    options: serde_json::Map<String, Value>,
    target: Option<Value>,
}

#[derive(Deserialize)]
struct LegacyHttp {
    url: String,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct LegacyFile {
    base64_string: String,
    filename: String,
}

async fn read_source_request(
    state: &Arc<AppState>,
    body: axum::extract::Request,
) -> Result<(Vec<(String, SourceItem)>, Vec<(String, String)>), ApiError> {
    let bytes = axum::body::to_bytes(body.into_body(), state.cfg.max_body_bytes)
        .await
        .map_err(|e| body_error("bad body", e))?;
    let req: SourceRequest =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Bad(format!("bad JSON body: {e}")))?;
    let mut specs = req.sources;
    specs.extend(
        req.http_sources
            .into_iter()
            .map(|h| passthrough::SourceSpec::Http {
                url: h.url,
                headers: h.headers,
            }),
    );
    // Upload names travel with their items so the envelope reports them.
    let mut names: Vec<Option<String>> = specs
        .iter()
        .map(|s| match s {
            passthrough::SourceSpec::File { filename, .. } => Some(filename.clone()),
            _ => None,
        })
        .collect();
    for f in req.file_sources {
        names.push(Some(f.filename.clone()));
        specs.push(passthrough::SourceSpec::File {
            base64_string: f.base64_string,
            filename: f.filename,
        });
    }
    if specs
        .iter()
        .any(|s| matches!(s, passthrough::SourceSpec::Http { .. }))
    {
        require_outbound(state, "URL inputs")?;
    }
    let single_names = names.len() == 1;
    let items = parse_source_specs(state, specs).await?;
    let named = items
        .into_iter()
        .map(|item| {
            let name = match (&item, single_names, names.first()) {
                (_, true, Some(Some(n))) => n.clone(),
                (Ok(source), ..) => source.name.clone(),
                (Err((label, _)), ..) => label.clone(),
            };
            (name, item)
        })
        .collect();
    let mut fields = json_fields(&req.options);
    match req
        .target
        .as_ref()
        .and_then(|t| t.get("kind"))
        .and_then(Value::as_str)
    {
        None | Some("inbody") => {}
        Some("zip") => fields.push(("target_type".into(), "zip".into())),
        Some(other) => {
            return Err(ApiError::Unsupported(format!(
                "target kind {other:?} is not served on this route (inbody, zip); \
                 /v1/convert takes the object-store and put targets"
            )))
        }
    }
    Ok((named, fields))
}

/// Admission control, then convert on the blocking pool under a conversion
/// permit — the sync half every compat route shares.
async fn answer(
    state: Arc<AppState>,
    items: Vec<(String, SourceItem)>,
    fields: Vec<(String, String)>,
) -> Result<Response, ApiError> {
    let (opts, ask) = parse_fields(&fields)?;
    if let Some(msg) = state.overloaded() {
        return Err(ApiError::Overloaded(msg));
    }
    let permit = state
        .permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|e| ApiError::Internal(format!("semaphore: {e}")))?;
    let st = state.clone();
    let stored = tokio::task::spawn_blocking(move || {
        let out = run(&st, items, &opts, &ask);
        trim_heap();
        out
    })
    .await
    .map_err(|e| ApiError::Internal(format!("convert task: {e}")))?;
    drop(permit);
    Ok(stored.into_response())
}

/// docling's `TaskStatusResponse` for a queued compat task.
fn task_status(id: &str, status: &str) -> Value {
    json!({
        "task_id": id,
        "task_type": "convert",
        "task_status": status,
        "task_position": null,
        "task_meta": null,
    })
}

/// The async half: queue the conversion, answer the task's status.
fn submit(
    state: &Arc<AppState>,
    items: Vec<(String, SourceItem)>,
    fields: Vec<(String, String)>,
) -> Result<Response, ApiError> {
    let (opts, ask) = parse_fields(&fields)?;
    if let Some(msg) = state.overloaded() {
        return Err(ApiError::Overloaded(msg));
    }
    let documents_total = items.len();
    let id = submit_job(state, documents_total, move |st, _rt| {
        Ok(run(st, items, &opts, &ask))
    })?;
    Ok(Json(task_status(&id, "pending")).into_response())
}

/// `POST /v1/convert/file` (and `/v1alpha/convert/file`).
pub(crate) async fn convert_file(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let (items, fields) = read_upload(body, query).await?;
    answer(state, items, fields).await
}

/// `POST /v1/convert/source` (and `/v1alpha/convert/source`).
pub(crate) async fn convert_source(
    State(state): State<Arc<AppState>>,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let (items, fields) = read_source_request(&state, body).await?;
    answer(state, items, fields).await
}

/// `POST /v1/convert/file/async`.
pub(crate) async fn convert_file_async(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let (items, fields) = read_upload(body, query).await?;
    submit(&state, items, fields)
}

/// `POST /v1/convert/source/async`.
pub(crate) async fn convert_source_async(
    State(state): State<Arc<AppState>>,
    body: axum::extract::Request,
) -> Result<Response, ApiError> {
    let (items, fields) = read_source_request(&state, body).await?;
    submit(&state, items, fields)
}

/// `GET /v1/status/poll/{task_id}`: docling's `TaskStatusResponse` for any
/// task (compat or `/v1/convert/async`); an unknown or expired id is a 404
/// with upstream's `detail`.
pub(crate) async fn status_poll(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let mut jobs = state.jobs.lock().unwrap();
    crate::purge_expired(&mut jobs, state.cfg.result_ttl_secs);
    match jobs.get(&id) {
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "Task not found."})),
        )
            .into_response(),
        Some(job) => {
            let mut body = task_status(&id, job.state.as_str());
            if let JobState::Failure(_, msg, _) = &job.state {
                body["error_message"] = json!(msg);
            }
            Json(body).into_response()
        }
    }
}

/// `X-Api-Key` (docling-serve's `DOCLING_SERVE_API_KEY`): when a key is
/// configured, every `/v1` and `/v1alpha` request must carry it; the
/// health, readiness, metrics and docs routes stay open, as upstream's do.
pub(crate) async fn require_api_key(
    State(key): State<Option<Arc<str>>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(key) = key else {
        return next.run(request).await;
    };
    let path = request.uri().path();
    let guarded = path.starts_with("/v1/") || path.starts_with("/v1alpha/");
    let given = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    if guarded && given != Some(&*key) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "X-Api-Key")],
            Json(json!({"detail": "Api key is required as X-Api-Key header."})),
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    /// Open WebUI's fields plus a typical `DOCLING_PARAMS`: docling's names
    /// map onto ours, what has no equivalent is ignored, nothing fails.
    #[test]
    fn upstream_fields_map_and_unknown_ones_are_ignored() {
        let (opts, ask) = parse_fields(&pairs(&[
            ("image_export_mode", "placeholder"),
            ("md_page_break_placeholder", "\u{c}"),
            ("to_formats", "md"),
            ("to_formats", "json"),
            ("do_ocr", "false"),
            ("ocr_engine", "easyocr"),
            ("ocr_lang", "fr"),
            ("ocr_lang", "en"),
            ("pdf_backend", "dlparse_v4"),
            ("table_mode", "accurate"),
            ("do_table_structure", "true"),
            ("abort_on_error", "false"),
            ("page_range", "2"),
            ("page_range", "5"),
            ("do_picture_description", "true"),
            (
                "picture_description_api",
                r#"{"url": "http://x/v1", "headers": {"Authorization": "Bearer sk-1"}, "params": {"model": "gpt-4o-mini", "max_tokens": 200}, "prompt": "Describe.", "concurrency": 3, "timeout": 20}"#,
            ),
            ("picture_description_local", r#"{"repo_id": "HuggingFaceTB/SmolVLM-256M-Instruct"}"#),
        ]))
        .expect("parses");
        {
            let c = &opts.convert;
            assert_eq!(c.do_picture_description, Some(true));
            assert_eq!(
                c.picture_description_endpoint.as_deref(),
                Some("http://x/v1")
            );
            assert_eq!(c.picture_description_model.as_deref(), Some("gpt-4o-mini"));
            assert_eq!(c.picture_description_api_key.as_deref(), Some("sk-1"));
            assert_eq!(c.picture_description_prompt.as_deref(), Some("Describe."));
            assert_eq!(c.picture_description_concurrency, Some(3));
        }
        assert_eq!(ask.formats, [Format::Md, Format::Json]);
        assert_eq!(ask.image_mode, ImageMode::Placeholder);
        let c = &opts.convert;
        assert_eq!(c.page_break_placeholder.as_deref(), Some("\u{c}"));
        assert_eq!(c.no_ocr, Some(true));
        assert_eq!(c.no_table_former, Some(false));
        assert_eq!(c.ocr_engine, None, "EasyOCR maps onto the built-in engine");
        assert_eq!(
            c.ocr_lang.as_deref(),
            Some("en"),
            "the first code PP-OCR reads"
        );
        assert_eq!(c.pages.as_deref(), Some("2-5"));
    }

    #[test]
    fn defaults_and_whole_document_ranges() {
        let (opts, ask) = parse_fields(&[]).expect("empty is fine");
        assert_eq!(ask.formats, [Format::Md]);
        assert_eq!(
            ask.image_mode,
            ImageMode::Embedded,
            "docling-serve's default"
        );
        assert!(!ask.zip);
        assert_eq!(opts.convert.pages, None);
        assert_eq!(
            pages_from(&["1".into(), "9223372036854775807".into()]),
            None
        );
        assert_eq!(pages_from(&["3-4".into()]).as_deref(), Some("3-4"));
        let (_, ask) = parse_fields(&pairs(&[("target_type", "zip")])).unwrap();
        assert!(ask.zip);
    }

    #[test]
    fn json_options_flatten_like_form_fields() {
        let v: Value = serde_json::from_str(
            r#"{"to_formats": ["md", "text"], "do_ocr": false, "page_range": [1, 3],
                "ocr_lang": null, "pdf_backend": "dlparse_v4"}"#,
        )
        .unwrap();
        let fields = json_fields(v.as_object().unwrap());
        let (opts, ask) = parse_fields(&fields).unwrap();
        assert_eq!(ask.formats, [Format::Md, Format::Text]);
        assert_eq!(opts.convert.no_ocr, Some(true));
        assert_eq!(opts.convert.pages.as_deref(), Some("1-3"));
    }
}
