//! Deterministic, network-free embedder using the hashing trick.
//!
//! Not a real semantic model, but it produces stable vectors where texts that
//! share tokens land near each other in cosine space — enough to wire up and
//! test the whole pipeline (ingest → store → retrieve → eval) offline.

use super::Embedder;
use crate::math;
use crate::Result;
use async_trait::async_trait;

/// Feature-hashing embedder of a fixed dimensionality.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
    id: String,
}

impl HashEmbedder {
    /// Create a hashing embedder producing `dim`-dimensional unit vectors.
    pub fn new(dim: usize) -> Self {
        HashEmbedder {
            dim: dim.max(1),
            id: format!("hash:{dim}"),
        }
    }

    fn embed_text(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        let tokens: Vec<&str> = text.split_whitespace().collect();
        for w in &tokens {
            self.add_feature(&mut v, w.to_ascii_lowercase().as_bytes());
        }
        // Bigrams add a little word-order signal.
        for pair in tokens.windows(2) {
            let bg = format!(
                "{}_{}",
                pair[0].to_ascii_lowercase(),
                pair[1].to_ascii_lowercase()
            );
            self.add_feature(&mut v, bg.as_bytes());
        }
        math::normalize(&mut v);
        v
    }

    fn add_feature(&self, v: &mut [f32], key: &[u8]) {
        let h = fnv1a(key);
        let idx = (h % self.dim as u64) as usize;
        let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
        v[idx] += sign;
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_text(t)).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        &self.id
    }
}

/// 64-bit FNV-1a — small, fast, dependency-free, and deterministic across runs.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::cosine;

    #[tokio::test]
    async fn deterministic_and_unit_length() {
        let e = HashEmbedder::new(256);
        let a = e.embed_one("the quick brown fox").await.unwrap();
        let b = e.embed_one("the quick brown fox").await.unwrap();
        assert_eq!(a, b);
        assert!((math::norm(&a) - 1.0).abs() < 1e-5);
        assert_eq!(a.len(), 256);
    }

    #[tokio::test]
    async fn shared_tokens_are_more_similar() {
        let e = HashEmbedder::new(1024);
        let q = e.embed_one("database vector search").await.unwrap();
        let near = e.embed_one("vector search over a database").await.unwrap();
        let far = e.embed_one("banana smoothie recipe").await.unwrap();
        assert!(cosine(&q, &near) > cosine(&q, &far));
    }
}
