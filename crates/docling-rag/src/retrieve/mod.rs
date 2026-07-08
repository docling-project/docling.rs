//! Retrieval: dense vector, sparse BM25, and the advanced modes that combine or
//! rewrite queries (Hybrid, Multi-Query fusion, HyDE).

pub mod bm25;
pub mod fusion;

use crate::embed::Embedder;
use crate::llm::ChatModel;
use crate::model::{RetrievalMode, Scored};
use crate::store::VectorStore;
use crate::{RagError, Result};
use std::sync::Arc;

/// Orchestrates the retrieval modes over a store + embedder (+ optional LLM).
#[derive(Clone)]
pub struct Retriever {
    store: Arc<dyn VectorStore>,
    embedder: Arc<dyn Embedder>,
    /// Required by the Multi-Query and HyDE modes; `None` disables them.
    chat: Option<Arc<dyn ChatModel>>,
    rrf_k: f32,
    multiquery_n: usize,
}

impl Retriever {
    /// Build a retriever. Pass `chat = None` to run without an LLM (vector/bm25/hybrid only).
    pub fn new(
        store: Arc<dyn VectorStore>,
        embedder: Arc<dyn Embedder>,
        chat: Option<Arc<dyn ChatModel>>,
    ) -> Self {
        Retriever {
            store,
            embedder,
            chat,
            rrf_k: fusion::DEFAULT_RRF_K,
            multiquery_n: 4,
        }
    }

    /// Override the RRF constant (default 60).
    pub fn with_rrf_k(mut self, k: f32) -> Self {
        self.rrf_k = k;
        self
    }

    /// Override the number of Multi-Query rewrites (default 4).
    pub fn with_multiquery_n(mut self, n: usize) -> Self {
        self.multiquery_n = n.max(1);
        self
    }

    /// Retrieve the top `k` chunks for `query` using `mode`.
    pub async fn retrieve(
        &self,
        mode: RetrievalMode,
        query: &str,
        k: usize,
    ) -> Result<Vec<Scored>> {
        match mode {
            RetrievalMode::Vector => self.vector(query, k).await,
            RetrievalMode::Bm25 => self.bm25(query, k).await,
            RetrievalMode::Hybrid => self.hybrid(query, k).await,
            RetrievalMode::MultiQuery => self.multi_query(query, k).await,
            RetrievalMode::Hyde => self.hyde(query, k).await,
        }
    }

    /// Dense vector search.
    pub async fn vector(&self, query: &str, k: usize) -> Result<Vec<Scored>> {
        let emb = self.embedder.embed_one(query).await?;
        self.store.vector_search(&emb, k).await
    }

    /// Sparse BM25 keyword search over the whole chunk corpus.
    pub async fn bm25(&self, query: &str, k: usize) -> Result<Vec<Scored>> {
        let chunks = self.store.all_chunks().await?;
        let index = bm25::Bm25Index::build(chunks);
        Ok(index.search(query, k))
    }

    /// Hybrid RAG: fuse dense + sparse results with RRF. Over-fetches each arm so
    /// fusion has depth to work with.
    pub async fn hybrid(&self, query: &str, k: usize) -> Result<Vec<Scored>> {
        let depth = (k * 4).max(20);
        let vec_hits = self.vector(query, depth).await?;
        let bm_hits = self.bm25(query, depth).await?;
        Ok(fusion::rrf(&[vec_hits, bm_hits], self.rrf_k, k))
    }

    /// Multi-Query (Fusion) RAG: the LLM rewrites the question into several diverse
    /// queries; each is retrieved (hybrid) and the results are fused with RRF.
    pub async fn multi_query(&self, query: &str, k: usize) -> Result<Vec<Scored>> {
        let chat = self.require_chat()?;
        let system = "You rewrite a user's question into diverse search queries that \
                      surface relevant documents. Output only the queries, one per line, \
                      with no numbering or commentary.";
        let user = format!(
            "Rewrite this question into {} diverse search queries:\n\n{}",
            self.multiquery_n, query
        );
        let raw = chat.ask(system, &user).await?;
        let mut queries: Vec<String> = raw
            .lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches(|c: char| {
                        c.is_ascii_digit() || c == '.' || c == '-' || c == ')'
                    })
                    .trim()
            })
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .take(self.multiquery_n)
            .collect();
        // Always include the original query.
        queries.push(query.to_string());

        let depth = (k * 4).max(20);
        let mut rankings = Vec::with_capacity(queries.len());
        for q in &queries {
            rankings.push(self.hybrid(q, depth).await?);
        }
        Ok(fusion::rrf(&rankings, self.rrf_k, k))
    }

    /// HyDE: the LLM writes a hypothetical answer; its embedding drives the search.
    pub async fn hyde(&self, query: &str, k: usize) -> Result<Vec<Scored>> {
        let chat = self.require_chat()?;
        let system = "You are helping a search system. Write a short, factual passage \
                      (2-4 sentences) that could plausibly answer the user's question, as \
                      if quoted from a relevant document. Do not hedge or mention that it \
                      is hypothetical.";
        let hypothetical = chat.ask(system, query).await?;
        // Search with the hypothetical document's embedding; fall back to the raw
        // query text if the model returned nothing usable.
        let search_text = if hypothetical.trim().is_empty() {
            query
        } else {
            &hypothetical
        };
        let emb = self.embedder.embed_one(search_text).await?;
        self.store.vector_search(&emb, k).await
    }

    fn require_chat(&self) -> Result<&Arc<dyn ChatModel>> {
        self.chat.as_ref().ok_or_else(|| {
            RagError::Llm("this retrieval mode needs an LLM; set OPENROUTER_API_KEY".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::llm::Message;
    use crate::model::{Chunk, Document};
    use crate::store::memory::MemoryStore;
    use async_trait::async_trait;

    async fn seeded_store() -> Arc<dyn VectorStore> {
        let store = Arc::new(MemoryStore::new());
        let embedder = HashEmbedder::new(512);
        let doc = Document::new("mem://t", "T", "h");
        store.upsert_document(&doc).await.unwrap();
        let texts = [
            "postgres vector database stores embeddings for semantic search",
            "a banana smoothie recipe with yogurt and honey",
            "rust async runtime tokio spawns tasks on a thread pool",
        ];
        let mut chunks = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            let mut c = Chunk::new(&doc.id, i as i64, *t, 0);
            c.embedding = Some(
                crate::embed::Embedder::embed(&embedder, &[t.to_string()])
                    .await
                    .unwrap()
                    .pop()
                    .unwrap(),
            );
            chunks.push(c);
        }
        store.insert_chunks(&chunks).await.unwrap();
        store
    }

    #[tokio::test]
    async fn vector_bm25_hybrid_find_relevant_chunk() {
        let store = seeded_store().await;
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(512));
        let r = Retriever::new(store, embedder, None);

        for mode in RetrievalMode::OFFLINE {
            let hits = r
                .retrieve(mode, "semantic search vector database", 3)
                .await
                .unwrap();
            assert!(!hits.is_empty(), "{mode} returned nothing");
            assert!(
                hits[0].chunk.text.contains("vector database"),
                "{mode} ranked the wrong chunk first: {}",
                hits[0].chunk.text
            );
        }
    }

    struct MockChat;
    #[async_trait]
    impl ChatModel for MockChat {
        async fn complete(&self, messages: &[Message]) -> Result<String> {
            let user = messages.last().map(|m| m.content.as_str()).unwrap_or("");
            if user.contains("Rewrite") {
                Ok("vector database\nsemantic search embeddings\npostgres storage".into())
            } else {
                // HyDE hypothetical answer.
                Ok("A vector database stores embeddings and performs semantic search.".into())
            }
        }
    }

    #[tokio::test]
    async fn multiquery_and_hyde_use_the_llm() {
        let store = seeded_store().await;
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(512));
        let chat: Arc<dyn ChatModel> = Arc::new(MockChat);
        let r = Retriever::new(store, embedder, Some(chat));

        let mq = r
            .retrieve(RetrievalMode::MultiQuery, "how are embeddings stored?", 3)
            .await
            .unwrap();
        assert!(mq.iter().any(|h| h.chunk.text.contains("vector database")));

        let hyde = r
            .retrieve(RetrievalMode::Hyde, "how are embeddings stored?", 3)
            .await
            .unwrap();
        assert!(hyde
            .iter()
            .any(|h| h.chunk.text.contains("vector database")));
    }

    #[tokio::test]
    async fn llm_modes_error_without_chat() {
        let store = seeded_store().await;
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(512));
        let r = Retriever::new(store, embedder, None);
        assert!(r.retrieve(RetrievalMode::Hyde, "q", 3).await.is_err());
    }
}
