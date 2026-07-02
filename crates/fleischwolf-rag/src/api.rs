//! REST API over a [`Pipeline`]: document info and search in every retrieval mode.
//!
//! Authentication is a static API-key list from config (`RAG_API_KEYS`), accepted
//! as `X-Api-Key: <key>` or `Authorization: Bearer <key>`. Auth is fail-closed:
//! [`router`] errors when the key list is empty. `GET /health` is public.
//!
//! Endpoints (all under auth except `/health`):
//!
//! | Method | Path                  | Description                                   |
//! |--------|-----------------------|-----------------------------------------------|
//! | GET    | `/health`             | liveness probe (public)                       |
//! | GET    | `/api/stats`          | document / chunk counts                       |
//! | GET    | `/api/documents`      | all documents with metadata + metrics         |
//! | GET    | `/api/documents/{id}` | one document by id                            |
//! | GET    | `/api/search`         | `?q=…&mode=hybrid&k=5` (also accepts POST)    |
//! | POST   | `/api/search`         | `{"query", "mode?", "top_k?", "answer?"}`     |
//!
//! Search modes: `vector`, `bm25`, `hybrid`, `multi-query`, `hyde`. With
//! `answer=true` the LLM synthesizes a grounded answer (needs `OPENROUTER_API_KEY`).

use crate::model::RetrievalMode;
use crate::pipeline::Pipeline;
use crate::{RagError, Result};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
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
    let state = Arc::new(AppState { pipeline, keys: keys.into_iter().collect() });

    let protected = Router::new()
        .route("/api/stats", get(stats))
        .route("/api/documents", get(list_documents))
        .route("/api/documents/{id}", get(get_document))
        .route("/api/search", get(search_get).post(search_post))
        .layer(middleware::from_fn_with_state(state.clone(), auth));

    Ok(Router::new()
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
    let docs = state.pipeline.store().list_documents().await.map_err(internal)?;
    Ok(Json(json!({"documents": docs})).into_response())
}

async fn get_document(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult {
    let docs = state.pipeline.store().list_documents().await.map_err(internal)?;
    match docs.into_iter().find(|d| d.id == id) {
        Some(doc) => Ok(Json(doc).into_response()),
        None => Err(err(StatusCode::NOT_FOUND, format!("no document with id '{id}'"))),
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
    let k = params.top_k.unwrap_or(state.pipeline.config().top_k).clamp(1, 100);

    if params.answer {
        let a = state
            .pipeline
            .answer(&params.query, mode, k)
            .await
            .map_err(|e| match e {
                RagError::Llm(_) => err(StatusCode::BAD_REQUEST, e),
                other => internal(other),
            })?;
        return Ok(Json(json!({
            "query": params.query,
            "mode": mode.to_string(),
            "answer": a.text,
            "results": a.sources,
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
    Ok(Json(json!({
        "query": params.query,
        "mode": mode.to_string(),
        "results": hits,
    }))
    .into_response())
}
