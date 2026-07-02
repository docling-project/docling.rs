//! Pure-Rust Okapi BM25 over a set of chunks. Rebuilt per query from the store's
//! chunk corpus — fine at the eval scale this crate targets, and keeps keyword
//! search identical across every DB backend.

use crate::model::{Chunk, Scored};

/// Default Okapi BM25 term-frequency saturation.
const K1: f32 = 1.2;
/// Default Okapi BM25 length-normalization.
const B: f32 = 0.75;

/// An in-memory BM25 index.
pub struct Bm25Index {
    chunks: Vec<Chunk>,
    /// Tokenized body per chunk (indices align with `chunks`).
    tokens: Vec<Vec<String>>,
    doc_len: Vec<f32>,
    avgdl: f32,
    /// Document frequency per term.
    df: std::collections::HashMap<String, usize>,
    n: usize,
    k1: f32,
    b: f32,
}

/// Lowercase, split on non-alphanumeric characters, drop empties.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

impl Bm25Index {
    /// Build an index over the given chunks (default k1=1.2, b=0.75).
    pub fn build(chunks: Vec<Chunk>) -> Self {
        let tokens: Vec<Vec<String>> = chunks.iter().map(|c| tokenize(&c.text)).collect();
        let doc_len: Vec<f32> = tokens.iter().map(|t| t.len() as f32).collect();
        let n = chunks.len();
        let avgdl = if n == 0 {
            0.0
        } else {
            doc_len.iter().sum::<f32>() / n as f32
        };

        let mut df: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for toks in &tokens {
            let mut seen = std::collections::HashSet::new();
            for t in toks {
                if seen.insert(t) {
                    *df.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        Bm25Index {
            chunks,
            tokens,
            doc_len,
            avgdl,
            df,
            n,
            k1: K1,
            b: B,
        }
    }

    /// Robertson–Spärck-Jones IDF with the usual `+0.5` smoothing, floored at 0.
    fn idf(&self, term: &str) -> f32 {
        let df = *self.df.get(term).unwrap_or(&0) as f32;
        let n = self.n as f32;
        (((n - df + 0.5) / (df + 0.5)) + 1.0).ln()
    }

    /// Score every chunk against `query` and return the top `k` with score > 0.
    pub fn search(&self, query: &str, k: usize) -> Vec<Scored> {
        if self.n == 0 {
            return Vec::new();
        }
        let q_terms = tokenize(query);
        let mut scored: Vec<Scored> = Vec::with_capacity(self.n);
        for (i, toks) in self.tokens.iter().enumerate() {
            let mut score = 0.0f32;
            let dl = self.doc_len[i];
            for term in &q_terms {
                let f = toks.iter().filter(|t| *t == term).count() as f32;
                if f == 0.0 {
                    continue;
                }
                let idf = self.idf(term);
                let denom = f + self.k1 * (1.0 - self.b + self.b * dl / self.avgdl.max(1e-6));
                score += idf * (f * (self.k1 + 1.0)) / denom;
            }
            if score > 0.0 {
                scored.push(Scored::new(self.chunks[i].clone(), score));
            }
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, text: &str) -> Chunk {
        let mut c = Chunk::new("doc", 0, text, 0);
        c.id = id.to_string();
        c
    }

    #[test]
    fn ranks_exact_term_matches_first() {
        let index = Bm25Index::build(vec![
            chunk("a", "the postgres database stores vectors"),
            chunk("b", "a banana smoothie recipe with yogurt"),
            chunk("c", "vector search over a database index"),
        ]);
        let hits = index.search("database vector", 3);
        assert!(!hits.is_empty());
        // The two DB/vector chunks must outrank the smoothie one.
        let top_ids: Vec<&str> = hits.iter().map(|h| h.chunk.id.as_str()).collect();
        assert!(top_ids.contains(&"a") && top_ids.contains(&"c"));
        assert_ne!(hits[0].chunk.id, "b");
    }

    #[test]
    fn empty_index_and_no_match() {
        assert!(Bm25Index::build(vec![]).search("x", 5).is_empty());
        let index = Bm25Index::build(vec![chunk("a", "hello world")]);
        assert!(index.search("nonexistent", 5).is_empty());
    }
}
