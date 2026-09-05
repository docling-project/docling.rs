//! Pure-Rust Okapi BM25 over the store's chunk corpus.
//!
//! The index is an **inverted index**: one postings list per term, so a query
//! only ever touches the chunks that actually contain one of its terms instead
//! of scoring the whole corpus. Building it is the expensive part (it reads
//! every chunk), which is why [`Bm25Cache`] keeps one alive across queries —
//! hybrid and multi-query retrieval issue several searches per question.
//!
//! Keyword search stays in Rust rather than delegating to a backend's
//! full-text engine so scores are identical on SQLite, Postgres and the
//! in-memory store.

use crate::model::{Chunk, Scored};
use crate::store::VectorStore;
use crate::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Okapi BM25 tuning parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25Params {
    /// Term-frequency saturation: higher rewards repeated terms for longer.
    pub k1: f32,
    /// Length normalization, 0..=1: 0 ignores chunk length, 1 normalizes fully.
    pub b: f32,
}

impl Default for Bm25Params {
    /// The classic Okapi defaults (also Lucene's): k1 = 1.2, b = 0.75.
    fn default() -> Self {
        Bm25Params { k1: 1.2, b: 0.75 }
    }
}

/// One term occurrence inside a chunk: `(chunk index, term frequency)`.
type Posting = (u32, u32);

/// An in-memory BM25 index over a fixed set of chunks.
pub struct Bm25Index {
    chunks: Vec<Chunk>,
    /// Postings per term, each sorted by chunk index. `df` is `postings.len()`.
    postings: HashMap<String, Vec<Posting>>,
    doc_len: Vec<f32>,
    avgdl: f32,
    params: Bm25Params,
}

/// Split `text` into lowercase alphanumeric terms.
///
/// Case folding is Unicode-aware (`str::to_lowercase`), so `Постгрес` and
/// `постгрес` are the same term — an ASCII-only fold would silently make
/// keyword search case-sensitive for every non-Latin corpus.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| {
            // Fast path: the overwhelmingly common all-ASCII term.
            if s.is_ascii() {
                s.to_ascii_lowercase()
            } else {
                s.to_lowercase()
            }
        })
        .collect()
}

impl Bm25Index {
    /// Build an index over `chunks` with the default parameters.
    pub fn build(chunks: Vec<Chunk>) -> Self {
        Self::build_with(chunks, Bm25Params::default())
    }

    /// Build an index over `chunks` with explicit BM25 parameters.
    pub fn build_with(chunks: Vec<Chunk>, params: Bm25Params) -> Self {
        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        let mut doc_len = Vec::with_capacity(chunks.len());

        for (i, chunk) in chunks.iter().enumerate() {
            let mut tokens = tokenize(&chunk.text);
            doc_len.push(tokens.len() as f32);
            // Sorting turns "count each term's occurrences" into a run scan over
            // the tokens, which are then moved into the index rather than cloned.
            tokens.sort_unstable();
            let mut tokens = tokens.into_iter().peekable();
            while let Some(term) = tokens.next() {
                let mut freq = 1u32;
                while tokens.peek() == Some(&term) {
                    tokens.next();
                    freq += 1;
                }
                // Chunks are visited in order, so every postings list stays sorted.
                postings.entry(term).or_default().push((i as u32, freq));
            }
        }

        let n = chunks.len();
        let avgdl = if n == 0 {
            0.0
        } else {
            doc_len.iter().sum::<f32>() / n as f32
        };
        Bm25Index {
            chunks,
            postings,
            doc_len,
            avgdl,
            params,
        }
    }

    /// Number of indexed chunks.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Whether the index holds no chunks.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Robertson–Spärck-Jones IDF with the usual `+0.5` smoothing, in the
    /// `ln(1 + …)` form that stays positive for every term (Lucene's).
    fn idf(&self, df: usize) -> f32 {
        let n = self.chunks.len() as f32;
        let df = df as f32;
        (((n - df + 0.5) / (df + 0.5)) + 1.0).ln()
    }

    /// Score every chunk that shares a term with `query` and return the top `k`
    /// (score > 0), best first. Ties keep corpus order, so results are stable.
    pub fn search(&self, query: &str, k: usize) -> Vec<Scored> {
        if self.chunks.is_empty() || k == 0 {
            return Vec::new();
        }
        // Query-term frequencies: a term repeated in the query weighs that much
        // more, matching the plain sum-over-query-occurrences formulation.
        let mut q_tf: HashMap<String, f32> = HashMap::new();
        for t in tokenize(query) {
            *q_tf.entry(t).or_insert(0.0) += 1.0;
        }

        let (k1, b) = (self.params.k1, self.params.b);
        let avgdl = self.avgdl.max(1e-6);
        let mut acc: HashMap<u32, f32> = HashMap::new();
        for (term, qf) in &q_tf {
            let Some(list) = self.postings.get(term) else {
                continue;
            };
            let idf = self.idf(list.len()) * qf;
            for &(doc, freq) in list {
                let f = freq as f32;
                let dl = self.doc_len[doc as usize];
                let denom = f + k1 * (1.0 - b + b * dl / avgdl);
                *acc.entry(doc).or_insert(0.0) += idf * (f * (k1 + 1.0)) / denom;
            }
        }

        let mut hits: Vec<(u32, f32)> = acc.into_iter().filter(|&(_, s)| s > 0.0).collect();
        // Sort by score, then by corpus position: `acc` is a HashMap, so without
        // the second key equal-scoring chunks would come back in random order.
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        hits.truncate(k);
        hits.into_iter()
            .map(|(doc, score)| Scored::new(self.chunks[doc as usize].clone(), score))
            .collect()
    }
}

/// A lazily-built, shared [`Bm25Index`] over a store's whole chunk corpus.
///
/// Building the index reads every chunk, so a retriever that rebuilt it per
/// query paid that cost several times for a single question — hybrid runs one
/// keyword search per query, multi-query one per rewrite. The cache builds it
/// once and hands out an `Arc`.
///
/// Freshness is checked on every use against a cheap store fingerprint (the
/// document and chunk counts, one `COUNT(*)` each) plus a generation counter
/// that [`Bm25Cache::invalidate`] bumps — the pipeline calls that on every
/// write, which is what catches an edit that happens to leave the counts
/// unchanged. A different process writing to the same database is only caught
/// by the counts, so a shared store can serve one stale result after such a
/// same-count edit.
pub struct Bm25Cache {
    /// `None` until the first search; replaced whenever the fingerprint moves.
    cached: tokio::sync::Mutex<Option<(Fingerprint, Arc<Bm25Index>)>>,
    generation: AtomicU64,
}

/// What the cached index was built from. Any change rebuilds it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fingerprint {
    documents: usize,
    chunks: usize,
    generation: u64,
    params: Bm25Params,
}

impl Default for Bm25Cache {
    fn default() -> Self {
        Self::new()
    }
}

impl Bm25Cache {
    /// An empty cache.
    pub fn new() -> Self {
        Bm25Cache {
            cached: tokio::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// Drop the cached index. Call after any write to the store.
    pub fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// The index for `store`, rebuilding it if the corpus or `params` moved.
    pub async fn index(
        &self,
        store: &Arc<dyn VectorStore>,
        params: Bm25Params,
    ) -> Result<Arc<Bm25Index>> {
        let fingerprint = Fingerprint {
            documents: store.count_documents().await?,
            chunks: store.count_chunks().await?,
            generation: self.generation.load(Ordering::Relaxed),
            params,
        };
        // Held across the build so concurrent searches wait for one index
        // instead of each building their own.
        let mut cached = self.cached.lock().await;
        if let Some((fp, index)) = cached.as_ref() {
            if *fp == fingerprint {
                return Ok(index.clone());
            }
        }
        let chunks = store.all_chunks().await?;
        // Tokenizing a whole corpus is CPU-bound; keep it off the async worker.
        let index = tokio::task::spawn_blocking(move || Bm25Index::build_with(chunks, params))
            .await
            // A blocking task is never cancelled, so the only way to fail the
            // join is a panic inside the build — re-raise it as it would have
            // been raised had the build run inline.
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
        let index = Arc::new(index);
        *cached = Some((fingerprint, index.clone()));
        Ok(index)
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

    /// Straight transcription of the BM25 formula, scoring every chunk with a
    /// linear scan — what the index is expected to reproduce exactly.
    fn reference(chunks: &[Chunk], query: &str, p: Bm25Params, k: usize) -> Vec<(String, f32)> {
        let tokens: Vec<Vec<String>> = chunks.iter().map(|c| tokenize(&c.text)).collect();
        let n = chunks.len() as f32;
        let avgdl = tokens.iter().map(|t| t.len() as f32).sum::<f32>() / n;
        let mut out: Vec<(String, f32)> = Vec::new();
        for (i, toks) in tokens.iter().enumerate() {
            let mut score = 0.0f32;
            for term in tokenize(query) {
                let f = toks.iter().filter(|t| **t == term).count() as f32;
                if f == 0.0 {
                    continue;
                }
                let df = tokens.iter().filter(|d| d.contains(&term)).count() as f32;
                let idf = (((n - df + 0.5) / (df + 0.5)) + 1.0).ln();
                let dl = toks.len() as f32;
                let denom = f + p.k1 * (1.0 - p.b + p.b * dl / avgdl);
                score += idf * (f * (p.k1 + 1.0)) / denom;
            }
            if score > 0.0 {
                out.push((chunks[i].id.clone(), score));
            }
        }
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        out.truncate(k);
        out
    }

    fn corpus() -> Vec<Chunk> {
        vec![
            chunk("a", "the postgres database stores vectors"),
            chunk("b", "a banana smoothie recipe with yogurt"),
            chunk("c", "vector search over a database index"),
            chunk("d", "database database database, a very database chunk"),
            chunk(
                "e",
                "a long chunk about databases and vector search and search engines and \
                 index structures that goes on for a while so that length normalization \
                 has something to bite on",
            ),
        ]
    }

    #[test]
    fn ranks_exact_term_matches_first() {
        let index = Bm25Index::build(corpus());
        let hits = index.search("database vector", 3);
        assert!(!hits.is_empty());
        let top_ids: Vec<&str> = hits.iter().map(|h| h.chunk.id.as_str()).collect();
        assert!(top_ids.contains(&"a") && top_ids.contains(&"c"));
        assert!(!top_ids.contains(&"b"), "the smoothie chunk shares no term");
    }

    #[test]
    fn matches_the_reference_implementation() {
        for params in [
            Bm25Params::default(),
            Bm25Params { k1: 0.5, b: 0.0 },
            Bm25Params { k1: 2.0, b: 1.0 },
        ] {
            let index = Bm25Index::build_with(corpus(), params);
            for query in [
                "database",
                "database vector search",
                "search search",
                "banana yogurt smoothie",
                "nothing here matches",
            ] {
                let got = index.search(query, 10);
                let want = reference(&corpus(), query, params, 10);
                assert_eq!(got.len(), want.len(), "{query} @ {params:?}");
                for (g, w) in got.iter().zip(&want) {
                    assert_eq!(g.chunk.id, w.0, "{query} @ {params:?}");
                    assert!(
                        (g.score - w.1).abs() < 1e-4,
                        "{query} @ {params:?}: {} vs {}",
                        g.score,
                        w.1
                    );
                }
            }
        }
    }

    #[test]
    fn saturation_and_length_normalization_follow_k1_and_b() {
        // b = 0 ignores length, so the long chunk `e` is no longer penalized for
        // its filler; with the default b = 0.75 the short chunk `c` wins.
        let normalized = Bm25Index::build(corpus()).search("vector search index", 5);
        let flat = Bm25Index::build_with(corpus(), Bm25Params { k1: 1.2, b: 0.0 })
            .search("vector search index", 5);
        assert_eq!(normalized[0].chunk.id, "c");
        assert_eq!(flat[0].chunk.id, "e");

        // k1 -> 0 collapses term frequency to a single occurrence, so the chunk
        // that just repeats "database" loses its edge over one plain mention.
        let saturated =
            Bm25Index::build_with(corpus(), Bm25Params { k1: 0.0, b: 0.75 }).search("database", 5);
        let repeated = Bm25Index::build(corpus()).search("database", 5);
        assert_eq!(repeated[0].chunk.id, "d");
        assert_ne!(saturated[0].chunk.id, "d");
    }

    #[test]
    fn case_folding_is_unicode_aware() {
        let index = Bm25Index::build(vec![
            chunk("ru", "Постгрес хранит векторы"),
            chunk("de", "Größe der Straße"),
        ]);
        assert_eq!(index.search("постгрес", 5)[0].chunk.id, "ru");
        assert_eq!(index.search("ПОСТГРЕС", 5)[0].chunk.id, "ru");
        assert_eq!(index.search("straße", 5)[0].chunk.id, "de");
    }

    #[test]
    fn ties_keep_corpus_order() {
        // Three identical chunks score identically; the ranking must not depend
        // on hash iteration order.
        let chunks: Vec<Chunk> = ["x", "y", "z"]
            .iter()
            .map(|id| chunk(id, "identical text about vectors"))
            .collect();
        for _ in 0..8 {
            let hits = Bm25Index::build(chunks.clone()).search("vectors", 3);
            let ids: Vec<&str> = hits.iter().map(|h| h.chunk.id.as_str()).collect();
            assert_eq!(ids, ["x", "y", "z"]);
        }
    }

    #[test]
    fn empty_index_and_no_match() {
        let empty = Bm25Index::build(vec![]);
        assert!(empty.is_empty());
        assert!(empty.search("x", 5).is_empty());
        let index = Bm25Index::build(vec![chunk("a", "hello world")]);
        assert_eq!(index.len(), 1);
        assert!(index.search("nonexistent", 5).is_empty());
        assert!(index.search("hello", 0).is_empty());
    }

    mod cache {
        use super::*;
        use crate::store::memory::MemoryStore;

        async fn store_with(texts: &[&str]) -> Arc<dyn VectorStore> {
            let store: Arc<dyn VectorStore> = Arc::new(MemoryStore::new());
            let doc = crate::model::Document::new("mem://t", "T", "h");
            store.upsert_document(&doc).await.unwrap();
            add_chunks(&store, &doc.id, texts).await;
            store
        }

        async fn add_chunks(store: &Arc<dyn VectorStore>, doc_id: &str, texts: &[&str]) {
            let chunks: Vec<Chunk> = texts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut c = Chunk::new(doc_id, i as i64, *t, 0);
                    c.embedding = Some(vec![0.0; 4]);
                    c
                })
                .collect();
            store.insert_chunks(&chunks).await.unwrap();
        }

        #[tokio::test]
        async fn reuses_one_index_until_the_corpus_moves() {
            let store = store_with(&["vector database", "banana smoothie"]).await;
            let cache = Bm25Cache::new();
            let p = Bm25Params::default();

            let first = cache.index(&store, p).await.unwrap();
            let second = cache.index(&store, p).await.unwrap();
            assert!(
                Arc::ptr_eq(&first, &second),
                "an unchanged corpus must not be re-tokenized"
            );

            // A new chunk moves the count fingerprint.
            let doc_id = store.list_documents().await.unwrap()[0].id.clone();
            add_chunks(&store, &doc_id, &["tokio async runtime"]).await;
            let third = cache.index(&store, p).await.unwrap();
            assert!(!Arc::ptr_eq(&second, &third));
            assert_eq!(third.len(), 3);
            assert_eq!(third.search("tokio", 5).len(), 1, "new chunk is searchable");
        }

        #[tokio::test]
        async fn invalidate_rebuilds_even_at_an_unchanged_count() {
            let store = store_with(&["vector database"]).await;
            let cache = Bm25Cache::new();
            let p = Bm25Params::default();
            let first = cache.index(&store, p).await.unwrap();

            // Replace the corpus with a same-sized one: only the explicit
            // invalidation can catch this, the counts are identical.
            store.clear().await.unwrap();
            let doc = crate::model::Document::new("mem://t2", "T2", "h2");
            store.upsert_document(&doc).await.unwrap();
            add_chunks(&store, &doc.id, &["tokio async runtime"]).await;
            assert!(
                Arc::ptr_eq(&first, &cache.index(&store, p).await.unwrap()),
                "counts alone cannot see a same-sized replacement"
            );

            cache.invalidate();
            let rebuilt = cache.index(&store, p).await.unwrap();
            assert_eq!(rebuilt.search("tokio", 5).len(), 1);
            assert!(rebuilt.search("vector", 5).is_empty());
        }

        #[tokio::test]
        async fn changed_params_rebuild() {
            let store = store_with(&["vector database"]).await;
            let cache = Bm25Cache::new();
            let first = cache.index(&store, Bm25Params::default()).await.unwrap();
            let tuned = cache
                .index(&store, Bm25Params { k1: 2.0, b: 0.4 })
                .await
                .unwrap();
            assert!(!Arc::ptr_eq(&first, &tuned));
        }
    }
}
