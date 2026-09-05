//! REST API over a [`Pipeline`]: document info and search in every retrieval mode.
//!
//! Authentication is a static API-key list from config (`RAG_API_KEYS`), accepted
//! as `X-Api-Key: <key>`, `Authorization: Bearer <key>`, or — for links a browser
//! opens directly, where no header can be set — `?api_key=<key>`. Auth is
//! fail-closed: [`router`] errors when the key list is empty. `GET /health` is public.
//!
//! Endpoints (all under auth except `/` and `/health`):
//!
//! | Method | Path                  | Description                                   |
//! |--------|-----------------------|-----------------------------------------------|
//! | GET    | `/`                   | built-in search UI (public; static HTML)      |
//! | GET    | `/health`             | liveness probe (public)                       |
//! | GET    | `/api/stats`          | document / chunk counts                       |
//! | GET    | `/api/documents`      | all documents with metadata + metrics         |
//! | POST   | `/api/documents`      | `?name=file.pdf` + enrich flags, raw bytes body → ingest |
//! | GET    | `/api/documents/{id}` | one document by id                            |
//! | GET    | `/api/documents/{id}/markdown` | the parsed Markdown (`text/markdown`) |
//! | DELETE | `/api/documents/{id}` | remove the document and all its chunks        |
//! | GET    | `/api/search`         | `?q=…&mode=hybrid&k=5` (also accepts POST)    |
//! | POST   | `/api/search`         | `{"query", "mode?", "top_k?", "answer?", "extend?"}` |
//!
//! Search modes: `vector`, `bm25`, `hybrid`, `multi-query`, `hyde`. With
//! `answer=true` the LLM synthesizes a grounded answer (needs `OPENROUTER_API_KEY`).

use crate::model::RetrievalMode;
use crate::pipeline::{ConvertOptions, IngestOutcome, Pipeline};
use crate::source::SourceRef;
use crate::{RagError, Result};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

struct AppState {
    pipeline: Pipeline,
    keys: HashSet<String>,
}

/// Build the router. Errors if `keys` is empty (auth is fail-closed).
pub fn router(pipeline: Pipeline, keys: Vec<String>) -> Result<Router> {
    if keys.is_empty() {
        return Err(RagError::config(
            "RAG_API_KEYS must contain at least one key to start the REST API",
        ));
    }
    let state = Arc::new(AppState {
        pipeline,
        keys: keys.into_iter().collect(),
    });

    let protected = Router::new()
        .route("/api/stats", get(stats))
        .route("/api/documents", get(list_documents).post(upload_document))
        .route(
            "/api/documents/{id}",
            get(get_document).delete(delete_document),
        )
        .route("/api/documents/{id}/markdown", get(document_markdown))
        .route("/api/search", get(search_get).post(search_post))
        // Uploads are raw document bytes; axum's 2 MB default would reject
        // any real PDF. 256 MiB comfortably covers the corpus' heaviest docs.
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), auth));

    Ok(Router::new()
        // The built-in search UI: one self-contained page, no external assets.
        // Public like /health — the page itself holds no data; every API call
        // it makes carries the key the user stored in localStorage.
        .route("/", get(|| async { Html(include_str!("ui.html")) }))
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .merge(protected)
        .with_state(state))
}

/// Bind `addr` and serve until the process is stopped.
pub async fn serve(pipeline: Pipeline, addr: &str, keys: Vec<String>) -> Result<()> {
    let app = router(pipeline, keys)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| RagError::config(format!("cannot bind {addr}: {e}")))?;
    tracing::info!(%addr, "REST API listening");
    axum::serve(listener, app)
        .await
        .map_err(|e| RagError::config(format!("server error: {e}")))
}

async fn auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let provided = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string)
        })
        // Links the browser opens directly (e.g. the UI's "md" view) cannot
        // set headers, so the key is also accepted as a query parameter.
        .or_else(|| query_param(req.uri().query(), "api_key"));
    match provided {
        Some(key) if state.keys.contains(&key) => next.run(req).await,
        _ => err(StatusCode::UNAUTHORIZED, "invalid or missing API key").into_response(),
    }
}

/// One value out of a raw query string, percent-decoded (`+` is a space).
fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = [bytes[i + 1], bytes[i + 2]];
                match std::str::from_utf8(&hex)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

type ApiResult = std::result::Result<Response, (StatusCode, Json<serde_json::Value>)>;

fn err(code: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(json!({"error": msg.to_string()})))
}

fn internal(e: RagError) -> (StatusCode, Json<serde_json::Value>) {
    err(StatusCode::INTERNAL_SERVER_ERROR, e)
}

async fn stats(State(state): State<Arc<AppState>>) -> ApiResult {
    let store = state.pipeline.store();
    let documents = store.count_documents().await.map_err(internal)?;
    let chunks = store.count_chunks().await.map_err(internal)?;
    Ok(Json(json!({"documents": documents, "chunks": chunks})).into_response())
}

async fn list_documents(State(state): State<Arc<AppState>>) -> ApiResult {
    let docs = state
        .pipeline
        .store()
        .list_documents()
        .await
        .map_err(internal)?;
    let docs: Vec<serde_json::Value> = docs.iter().map(doc_json).collect();
    Ok(Json(json!({"documents": docs})).into_response())
}

/// A document as the JSON API exposes it: metadata minus the full parsed
/// Markdown, which can be megabytes — the UI polls the document list, and
/// the text has its own endpoint (`…/{id}/markdown`).
fn doc_json(doc: &crate::model::Document) -> serde_json::Value {
    let mut v = serde_json::to_value(doc).unwrap_or_default();
    if let Some(meta) = v.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        if meta.remove("markdown").is_some() {
            meta.insert("has_markdown".into(), json!(true));
        }
    }
    v
}

/// Upload parameters: the file name (drives format detection) as `?name=`,
/// plus optional enrichment switches (`?enrich_pictures=true&…`) mapping to
/// docling's enrichment models — each needs its model files on disk
/// (`download_dependencies.sh`; code/formula need `--enrich`).
#[derive(Debug, Deserialize)]
struct UploadParams {
    name: String,
    #[serde(default)]
    enrich_pictures: bool,
    #[serde(default)]
    enrich_code: bool,
    #[serde(default)]
    enrich_formulas: bool,
}

/// `POST /api/documents?name=report.pdf` with the raw file bytes as the body:
/// convert → chunk → embed → store, exactly the ingest pipeline. Responds
/// with the outcome (`ingested` + chunk count, or `skipped` when an identical
/// document is already stored).
async fn upload_document(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UploadParams>,
    body: axum::body::Bytes,
) -> ApiResult {
    // Keep only the final path segment: the name is caller-supplied and only
    // needed for format detection + display, never as a filesystem path.
    let name = params
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name must not be empty"));
    }
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "empty body"));
    }
    let r = SourceRef {
        uri: format!("upload:///{name}"),
        name: name.clone(),
        rel_path: name.clone(),
    };
    let opts = ConvertOptions {
        enrich_pictures: params.enrich_pictures,
        enrich_code: params.enrich_code,
        enrich_formulas: params.enrich_formulas,
    };
    match state
        .pipeline
        .ingest_bytes_with(&r, body.to_vec(), opts)
        .await
    {
        Ok(IngestOutcome::Ingested(chunks)) => {
            // Include the stored row's id + per-phase processing metrics so
            // the caller (the UI) can show where the time went.
            let stored = state
                .pipeline
                .store()
                .list_documents()
                .await
                .ok()
                .and_then(|docs| docs.into_iter().find(|d| d.source_uri == r.uri));
            let (id, metrics) = stored
                .map(|d| (json!(d.id), d.metadata.get("metrics").cloned()))
                .unwrap_or((serde_json::Value::Null, None));
            Ok(Json(json!({
                "outcome": "ingested",
                "name": name,
                "chunks": chunks,
                "id": id,
                "metrics": metrics,
            }))
            .into_response())
        }
        Ok(IngestOutcome::Skipped) => Ok(Json(json!({
            "outcome": "skipped",
            "name": name,
        }))
        .into_response()),
        // A document the converter rejects is the caller's input, not ours.
        Err(e @ RagError::Conversion(_)) => Err(err(StatusCode::BAD_REQUEST, e)),
        Err(other) => Err(internal(other)),
    }
}

/// `DELETE /api/documents/{id}`: remove the document and all its chunks.
async fn delete_document(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let docs = state
        .pipeline
        .store()
        .list_documents()
        .await
        .map_err(internal)?;
    if !docs.iter().any(|d| d.id == id) {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!("no document with id '{id}'"),
        ));
    }
    state
        .pipeline
        .store()
        .delete_document(&id)
        .await
        .map_err(internal)?;
    // The keyword index still holds this document's chunks.
    state.pipeline.invalidate_keyword_index();
    Ok(Json(json!({"deleted": id})).into_response())
}

async fn get_document(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    let docs = state
        .pipeline
        .store()
        .list_documents()
        .await
        .map_err(internal)?;
    match docs.into_iter().find(|d| d.id == id) {
        Some(doc) => {
            // Augment with the live chunk count and an in-progress marker
            // (the document row exists with a `pending:` hash while its
            // ingest is still running) — the UI polls this during uploads.
            let chunks = state
                .pipeline
                .store()
                .count_chunks_for(&doc.id)
                .await
                .map_err(internal)?;
            let processing = doc.hash.starts_with("pending:");
            let mut body = doc_json(&doc);
            if let Some(obj) = body.as_object_mut() {
                obj.insert("chunks".into(), json!(chunks));
                obj.insert("processing".into(), json!(processing));
            }
            Ok(Json(body).into_response())
        }
        None => Err(err(
            StatusCode::NOT_FOUND,
            format!("no document with id '{id}'"),
        )),
    }
}

/// `GET /api/documents/{id}/markdown`: the parsed Markdown as stored at
/// ingest, served as `text/markdown` so a browser tab renders/downloads it
/// directly. 404 for unknown ids and for documents ingested before the
/// Markdown was persisted (re-upload to backfill).
async fn document_markdown(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult {
    let docs = state
        .pipeline
        .store()
        .list_documents()
        .await
        .map_err(internal)?;
    let doc = docs
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no document with id '{id}'")))?;
    match doc.metadata.get("markdown").and_then(|m| m.as_str()) {
        Some(md) => Ok((
            [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
            md.to_string(),
        )
            .into_response()),
        None => Err(err(
            StatusCode::NOT_FOUND,
            "no stored markdown for this document (ingested before markdown was persisted — re-upload to backfill)",
        )),
    }
}

/// Search parameters, shared by the GET (query-string) and POST (JSON) forms.
#[derive(Debug, Deserialize)]
struct SearchParams {
    /// The search query (`q` also accepted on GET).
    #[serde(alias = "q")]
    query: String,
    /// vector | bm25 | hybrid | multi-query | hyde. Defaults to the configured mode.
    mode: Option<String>,
    /// Number of results (default: configured top_k).
    #[serde(alias = "k")]
    top_k: Option<usize>,
    /// Also synthesize an LLM answer grounded in the results.
    #[serde(default)]
    answer: bool,
    /// Extend every hit with its ordinal neighbors (one chunk before, one
    /// after, same document) — each result gains a `context` string. Purely
    /// presentational: scoring and the LLM answer see the original chunks.
    #[serde(default)]
    extend: bool,
}

/// Serialize hits, optionally widening each one to `prev + hit + next` from
/// the store (adjacent window chunks may repeat their small overlap — that's
/// inherent to the chunker, not stitched away here).
async fn results_json(
    state: &Arc<AppState>,
    hits: &[crate::model::Scored],
    extend: bool,
) -> serde_json::Value {
    if !extend {
        return json!(hits);
    }
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let context = state
            .pipeline
            .store()
            .chunk_neighborhood(&hit.chunk.doc_id, hit.chunk.ordinal)
            .await
            .map(|n| {
                n.iter()
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_else(|_| hit.chunk.text.clone());
        let mut v = json!(hit);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("context".into(), json!(context));
        }
        out.push(v);
    }
    json!(out)
}

async fn search_get(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> ApiResult {
    run_search(state, params).await
}

async fn search_post(
    State(state): State<Arc<AppState>>,
    Json(params): Json<SearchParams>,
) -> ApiResult {
    run_search(state, params).await
}

async fn run_search(state: Arc<AppState>, params: SearchParams) -> ApiResult {
    if params.query.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "query must not be empty"));
    }
    let mode = match &params.mode {
        Some(m) => RetrievalMode::from_str(m).map_err(|e| err(StatusCode::BAD_REQUEST, e))?,
        None => state.pipeline.config().retrieval_mode,
    };
    let k = params
        .top_k
        .unwrap_or(state.pipeline.config().top_k)
        .clamp(1, 100);

    if params.answer {
        let a = state
            .pipeline
            .answer(&params.query, mode, k)
            .await
            .map_err(|e| match e {
                RagError::Llm(_) => err(StatusCode::BAD_REQUEST, e),
                other => internal(other),
            })?;
        let results = results_json(&state, &a.sources, params.extend).await;
        return Ok(Json(json!({
            "query": params.query,
            "mode": mode.to_string(),
            "answer": a.text,
            "results": results,
        }))
        .into_response());
    }

    let hits = state
        .pipeline
        .query(mode, &params.query, k)
        .await
        .map_err(|e| match e {
            RagError::Llm(_) => err(StatusCode::BAD_REQUEST, e),
            other => internal(other),
        })?;
    let results = results_json(&state, &hits, params.extend).await;
    Ok(Json(json!({
        "query": params.query,
        "mode": mode.to_string(),
        "results": results,
    }))
    .into_response())
}
