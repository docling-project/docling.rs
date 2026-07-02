//! In-memory vector store. Never persisted; used by tests and quick evals.

use super::{top_k_by_cosine, VectorStore};
use crate::model::{Chunk, Document, Scored};
use crate::Result;
use async_trait::async_trait;
use std::sync::RwLock;

/// A store that keeps everything in process memory.
#[derive(Default)]
pub struct MemoryStore {
    docs: RwLock<Vec<Document>>,
    chunks: RwLock<Vec<Chunk>>,
}

impl MemoryStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        MemoryStore::default()
    }
}

#[async_trait]
impl VectorStore for MemoryStore {
    async fn migrate(&self) -> Result<()> {
        Ok(())
    }

    async fn upsert_document(&self, doc: &Document) -> Result<()> {
        let mut docs = self.docs.write().unwrap();
        if let Some(existing) = docs.iter_mut().find(|d| d.id == doc.id) {
            *existing = doc.clone();
        } else {
            docs.push(doc.clone());
        }
        Ok(())
    }

    async fn find_document_by_hash(&self, hash: &str) -> Result<Option<String>> {
        Ok(self
            .docs
            .read()
            .unwrap()
            .iter()
            .find(|d| d.hash == hash)
            .map(|d| d.id.clone()))
    }

    async fn insert_chunks(&self, chunks: &[Chunk]) -> Result<()> {
        self.chunks.write().unwrap().extend_from_slice(chunks);
        Ok(())
    }

    async fn vector_search(&self, query: &[f32], k: usize) -> Result<Vec<Scored>> {
        let chunks = self.chunks.read().unwrap();
        let candidates = chunks.iter().filter_map(|c| {
            c.embedding.as_ref().map(|e| {
                let mut bare = c.clone();
                bare.embedding = None;
                (bare, e.clone())
            })
        });
        Ok(top_k_by_cosine(query, candidates.collect::<Vec<_>>(), k))
    }

    async fn all_chunks(&self) -> Result<Vec<Chunk>> {
        Ok(self
            .chunks
            .read()
            .unwrap()
            .iter()
            .map(|c| {
                let mut bare = c.clone();
                bare.embedding = None;
                bare
            })
            .collect())
    }

    async fn count_chunks(&self) -> Result<usize> {
        Ok(self.chunks.read().unwrap().len())
    }

    async fn count_documents(&self) -> Result<usize> {
        Ok(self.docs.read().unwrap().len())
    }

    async fn list_documents(&self) -> Result<Vec<Document>> {
        Ok(self.docs.read().unwrap().clone())
    }

    async fn clear(&self) -> Result<()> {
        self.docs.write().unwrap().clear();
        self.chunks.write().unwrap().clear();
        Ok(())
    }
}
