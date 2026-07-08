//! Retrieval quality metrics: recall@k, MRR, and nDCG@k.
//!
//! Relevance is judged by substring match: a retrieved chunk is relevant to a
//! query if its text contains one of the query's expected substrings
//! (case-insensitive). This keeps eval datasets portable — they need no knowledge
//! of chunk ids, which change with every chunking config.

use crate::model::Scored;

/// Per-query scores.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryMetrics {
    /// Fraction of the expected substrings matched by some top-`k` chunk.
    pub recall: f32,
    /// Reciprocal rank of the first relevant chunk (0 if none).
    pub mrr: f32,
    /// Normalized discounted cumulative gain over the top `k`.
    pub ndcg: f32,
}

/// Whether `text` satisfies any expected substring (case-insensitive).
pub fn is_relevant(text: &str, expected: &[String]) -> bool {
    let hay = text.to_ascii_lowercase();
    expected
        .iter()
        .any(|e| hay.contains(&e.to_ascii_lowercase()))
}

/// Compute recall@k / MRR / nDCG@k for one query's ranked results.
pub fn evaluate(hits: &[Scored], expected: &[String], k: usize) -> QueryMetrics {
    if expected.is_empty() {
        return QueryMetrics::default();
    }
    let top = &hits[..hits.len().min(k)];

    // Recall: how many distinct expected substrings were surfaced.
    let matched = expected
        .iter()
        .filter(|e| {
            let el = e.to_ascii_lowercase();
            top.iter()
                .any(|h| h.chunk.text.to_ascii_lowercase().contains(&el))
        })
        .count();
    let recall = matched as f32 / expected.len() as f32;

    // MRR: reciprocal rank of the first relevant hit.
    let mut mrr = 0.0;
    for (i, h) in top.iter().enumerate() {
        if is_relevant(&h.chunk.text, expected) {
            mrr = 1.0 / (i as f32 + 1.0);
            break;
        }
    }

    // nDCG with binary gains; ideal ranking places all found relevants first.
    let mut dcg = 0.0;
    let mut relevant_found = 0usize;
    for (i, h) in top.iter().enumerate() {
        if is_relevant(&h.chunk.text, expected) {
            dcg += 1.0 / ((i as f32 + 2.0).log2());
            relevant_found += 1;
        }
    }
    let idcg: f32 = (0..relevant_found)
        .map(|i| 1.0 / ((i as f32 + 2.0).log2()))
        .sum();
    let ndcg = if idcg > 0.0 { dcg / idcg } else { 0.0 };

    QueryMetrics { recall, mrr, ndcg }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Chunk;

    fn hit(text: &str) -> Scored {
        Scored::new(Chunk::new("d", 0, text, 0), 1.0)
    }

    #[test]
    fn perfect_ranking_scores_one() {
        let hits = vec![hit("the answer is chunking"), hit("unrelated")];
        let m = evaluate(&hits, &["chunking".into()], 5);
        assert!((m.recall - 1.0).abs() < 1e-6);
        assert!((m.mrr - 1.0).abs() < 1e-6);
        assert!((m.ndcg - 1.0).abs() < 1e-6);
    }

    #[test]
    fn relevant_lower_down_lowers_mrr() {
        let hits = vec![hit("noise"), hit("noise"), hit("real chunking answer")];
        let m = evaluate(&hits, &["chunking".into()], 5);
        assert!((m.mrr - (1.0 / 3.0)).abs() < 1e-6);
        assert!((m.recall - 1.0).abs() < 1e-6); // still found within k
    }

    #[test]
    fn no_match_scores_zero() {
        let hits = vec![hit("a"), hit("b")];
        let m = evaluate(&hits, &["missing".into()], 5);
        assert_eq!(m.recall, 0.0);
        assert_eq!(m.mrr, 0.0);
        assert_eq!(m.ndcg, 0.0);
    }
}
