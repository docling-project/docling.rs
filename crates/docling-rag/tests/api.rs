//! REST API integration test, fully offline: memory store + hashing embedder,
//! server bound to an ephemeral port, exercised with a real HTTP client.

use docling_rag::config::{DbBackend, EmbedProvider, SourceKind};
use docling_rag::{api, Pipeline, RagConfig};

async fn spawn_server() -> (String, reqwest::Client) {
    let dir = std::env::temp_dir().join(format!(
        "rag-api-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("vectors.md"),
        "# Vector Search\n\nA vector database stores embeddings so that semantic \
         search can find passages by meaning rather than exact keywords.",
    )
    .unwrap();
    std::fs::write(
        dir.join("smoothie.md"),
        "# Smoothie\n\nBlend a banana with yogurt and honey for a quick breakfast smoothie.",
    )
    .unwrap();

    let cfg = RagConfig {
        db_backend: DbBackend::Memory,
        embed_provider: EmbedProvider::Hash,
        embed_dim: 256,
        source: SourceKind::Folder,
        source_path: dir.display().to_string(),
        ..RagConfig::default()
    };
    let pipeline = Pipeline::from_config(&cfg).await.unwrap();
    pipeline.ingest_all().await.unwrap();

    let app = api::router(pipeline, vec!["test-key".into(), "other-key".into()]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), reqwest::Client::new())
}

#[tokio::test]
async fn rest_api_end_to_end() {
    let (base, client) = spawn_server().await;

    // /health is public.
    let r = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(r.status(), 200);

    // Everything under /api requires a key.
    let r = client
        .get(format!("{base}/api/documents"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = client
        .get(format!("{base}/api/documents"))
        .header("X-Api-Key", "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // X-Api-Key works; documents carry their processing metrics.
    let r = client
        .get(format!("{base}/api/documents"))
        .header("X-Api-Key", "test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    let docs = body["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 2);
    assert!(docs[0]["metadata"]["metrics"]["words"].as_u64().unwrap() > 0);

    // Bearer auth works too; single-document lookup and 404.
    let id = docs[0]["id"].as_str().unwrap();
    let r = client
        .get(format!("{base}/api/documents/{id}"))
        .header("Authorization", "Bearer other-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = client
        .get(format!("{base}/api/documents/nope"))
        .header("X-Api-Key", "test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Stats.
    let r = client
        .get(format!("{base}/api/stats"))
        .header("X-Api-Key", "test-key")
        .send()
        .await
        .unwrap();
    let stats: serde_json::Value = r.json().await.unwrap();
    assert_eq!(stats["documents"], 2);

    // GET search in each offline mode finds the vector-database passage.
    for mode in ["vector", "bm25", "hybrid"] {
        let r = client
            .get(format!("{base}/api/search"))
            .query(&[
                ("q", "semantic search in a vector database"),
                ("mode", mode),
                ("k", "3"),
            ])
            .header("X-Api-Key", "test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "mode {mode}");
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["mode"], mode);
        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty(), "mode {mode} returned nothing");
        assert!(
            results[0]["chunk"]["text"]
                .as_str()
                .unwrap()
                .contains("vector database"),
            "mode {mode} ranked the wrong chunk"
        );
    }

    // POST body form.
    let r = client
        .post(format!("{base}/api/search"))
        .header("X-Api-Key", "test-key")
        .json(&serde_json::json!({"query": "banana breakfast", "mode": "bm25", "top_k": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(body["results"][0]["chunk"]["text"]
        .as_str()
        .unwrap()
        .contains("banana"));

    // Bad inputs: unknown mode and LLM mode without a key are 400s.
    let r = client
        .get(format!("{base}/api/search"))
        .query(&[("q", "x"), ("mode", "warp-drive")])
        .header("X-Api-Key", "test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = client
        .get(format!("{base}/api/search"))
        .query(&[("q", "x"), ("mode", "hyde")])
        .header("X-Api-Key", "test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn router_refuses_empty_key_list() {
    let cfg = RagConfig {
        db_backend: DbBackend::Memory,
        embed_provider: EmbedProvider::Hash,
        ..RagConfig::default()
    };
    let pipeline = Pipeline::from_config(&cfg).await.unwrap();
    assert!(api::router(pipeline, vec![]).is_err());
}

#[tokio::test]
async fn ui_page_is_public_and_self_contained() {
    let (base, client) = spawn_server().await;

    // No auth: the page itself carries no data.
    let r = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let body = r.text().await.unwrap();
    assert!(body.contains("docling-rag"), "page identifies itself");
    assert!(body.contains("/api/search"), "page talks to the search API");
    assert!(body.contains("localStorage"), "token persists client-side");
    // Self-contained: a strict deployment must not need any external origin.
    assert!(
        !body.contains("https://") && !body.contains("http://"),
        "no external assets"
    );
}

#[tokio::test]
async fn upload_and_delete_document() {
    let (base, client) = spawn_server().await;
    let auth = ("X-Api-Key", "test-key");

    // Upload needs auth like every /api route.
    let r = client
        .post(format!("{base}/api/documents?name=note.md"))
        .body("# Note\n\nUploaded through the API.")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Upload -> ingested with chunks; stats grow by one document.
    let before: serde_json::Value = client
        .get(format!("{base}/api/stats"))
        .header(auth.0, auth.1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r: serde_json::Value = client
        .post(format!("{base}/api/documents?name=note.md"))
        .header(auth.0, auth.1)
        .body("# Note\n\nUploaded through the API.")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["outcome"], "ingested");
    assert!(r["chunks"].as_u64().unwrap() >= 1);
    // The response carries the stored row's id and per-phase metrics.
    let uploaded_id = r["id"].as_str().unwrap().to_string();
    assert!(r["metrics"]["parsing"]["seconds"].is_number());
    assert!(r["metrics"]["embedding"]["seconds"].is_number());

    // GET one document: augmented with the live chunk count + progress flag.
    let one: serde_json::Value = client
        .get(format!("{base}/api/documents/{uploaded_id}"))
        .header(auth.0, auth.1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(one["chunks"].as_u64().unwrap() >= 1);
    assert_eq!(one["processing"], false);

    // Same bytes again -> deduplicated.
    let r: serde_json::Value = client
        .post(format!("{base}/api/documents?name=note.md"))
        .header(auth.0, auth.1)
        .body("# Note\n\nUploaded through the API.")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["outcome"], "skipped");

    // Find the uploaded doc, delete it, stats return to the baseline.
    let docs: serde_json::Value = client
        .get(format!("{base}/api/documents"))
        .header(auth.0, auth.1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = docs["documents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["source_uri"] == "upload:///note.md")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = client
        .delete(format!("{base}/api/documents/{id}"))
        .header(auth.0, auth.1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let after: serde_json::Value = client
        .get(format!("{base}/api/stats"))
        .header(auth.0, auth.1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["documents"], before["documents"]);

    // Deleting again -> 404; empty upload name -> 400.
    let r = client
        .delete(format!("{base}/api/documents/{id}"))
        .header(auth.0, auth.1)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = client
        .post(format!("{base}/api/documents?name="))
        .header(auth.0, auth.1)
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn extend_context_widens_hits_with_neighbors() {
    // Own server with a tiny chunk window so one document yields several
    // ordinal-adjacent chunks.
    let dir = std::env::temp_dir().join(format!(
        "rag-ext-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("guide.md"),
        "# Guide\n\nAlpha section text that fills the first window with parrots. \
         Bravo section text about semantic vector retrieval quality here. \
         Charlie section text closing the document with more words after.",
    )
    .unwrap();
    let cfg = RagConfig {
        db_backend: DbBackend::Memory,
        embed_provider: EmbedProvider::Hash,
        embed_dim: 128,
        source: SourceKind::Folder,
        source_path: dir.display().to_string(),
        chunk_size: 10,
        chunk_overlap: 0.0,
        ..RagConfig::default()
    };
    let pipeline = docling_rag::Pipeline::from_config(&cfg).await.unwrap();
    pipeline.ingest_all().await.unwrap();
    let app = api::router(pipeline, vec!["test-key".into()]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let body: serde_json::Value = client
        .post(format!("{base}/api/search"))
        .header("X-Api-Key", "test-key")
        .json(&serde_json::json!({
            "query": "semantic vector retrieval",
            "mode": "bm25",
            "top_k": 1,
            "extend": true,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hit = &body["results"][0];
    let own = hit["chunk"]["text"].as_str().unwrap();
    let ctx = hit["context"].as_str().unwrap();
    assert!(ctx.contains(own), "context contains the hit itself");
    assert!(ctx.len() > own.len(), "context is wider than the hit");
    // The middle chunk's context must pull text from a neighbor window.
    assert!(
        ctx.contains("parrots") || ctx.contains("closing the document"),
        "context should include a neighboring chunk: {ctx}"
    );

    // Without extend there is no context field.
    let body: serde_json::Value = client
        .get(format!("{base}/api/search?q=retrieval&mode=bm25&k=1"))
        .header("X-Api-Key", "test-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["results"][0].get("context").is_none());
}
