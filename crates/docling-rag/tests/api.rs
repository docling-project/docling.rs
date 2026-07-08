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
