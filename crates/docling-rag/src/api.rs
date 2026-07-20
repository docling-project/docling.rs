//! REST API over a [`Pipeline`]: document info and search in every retrieval mode.
//!
//! Authentication is a static API-key list from config (`RAG_API_KEYS`), accepted
//! as `X-Api-Key: <key>` or `Authorization: Bearer <key>`. Auth is fail-closed:
//! [`router`] errors when the key list is empty. `GET /health` is public.
//!
//! Endpoints (all under auth except `/` and `/health`):
//!
//! | Method | Path                  | Description                                   |
//! |--------|-----------------------|-----------------------------------------------|
//! | GET    | `/`                   | built-in search UI (public; static HTML)      |
//! | GET    | `/health`             | liveness probe (public)                       |
//! | GET    | `/api/stats`          | document / chunk counts                       |
//! | GET    | `/api/documents`      | all documents with metadata + metrics         |
//! | POST   | `/api/documents`      | `?name=file.pdf`, raw bytes body → ingest     |
//! | GET    | `/api/documents/{id}` | one document by id                            |
//! | DELETE | `/api/documents/{id}` | remove the document and all its chunks        |
//! | GET    | `/api/search`         | `?q=…&mode=hybrid&k=5` (also accepts POST)    |
//! | POST   | `/api/search`         | `{"query", "mode?", "top_k?", "answer?", "extend?"}` |
//!
//! Search modes: `vector`, `bm25`, `hybrid`, `multi-query`, `hyde`. With
//! `answer=true` the LLM synthesizes a grounded answer (needs `OPENROUTER_API_KEY`).

use crate::model::RetrievalMode;
use crate::pipeline::{IngestOutcome, Pipeline};
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
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        });
    match provided {
        Some(key) if state.keys.contains(key) => next.run(req).await,
        _ => err(StatusCode::UNAUTHORIZED, "invalid or missing API key").into_response(),
    }
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
    Ok(Json(json!({"documents": docs})).into_response())
}

/// Upload parameters: the file name (drives format detection) as `?name=`.
#[derive(Debug, Deserialize)]
struct UploadParams {
    name: String,
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
    match state.pipeline.ingest_bytes(&r, body.to_vec()).await {
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
            let mut body = serde_json::to_value(&doc).unwrap_or_default();
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
