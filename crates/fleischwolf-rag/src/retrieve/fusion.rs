//! Reciprocal Rank Fusion (RRF): combine several ranked result lists into one by
//! summing `1 / (k + rank)` across the lists. Rank-based, so it fuses scores from
//! different scales (cosine, BM25) without normalization.

use crate::model::Scored;
use std::collections::HashMap;

/// The conventional RRF constant.
pub const DEFAULT_RRF_K: f32 = 60.0;

/// Fuse ranked lists with RRF and return the top `top_k`.
///
/// Each input list is assumed to be sorted best-first. A chunk's fused score is
/// the sum over lists of `1 / (k + rank)`, where `rank` is its 1-based position.
pub fn rrf(rankings: &[Vec<Scored>], k: f32, top_k: usize) -> Vec<Scored> {
    let mut fused: HashMap<String, f32> = HashMap::new();
    let mut repr: HashMap<String, Scored> = HashMap::new();

    for list in rankings {
        for (rank, hit) in list.iter().enumerate() {
            let contribution = 1.0 / (k + (rank as f32 + 1.0));
            *fused.entry(hit.chunk.id.clone()).or_insert(0.0) += contribution;
            repr.entry(hit.chunk.id.clone())
                .or_insert_with(|| hit.clone());
        }
    }

    let mut out: Vec<Scored> = fused
        .into_iter()
        .map(|(id, score)| {
            let mut s = repr.remove(&id).expect("repr present for every fused id");
            s.score = score;
            s
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(top_k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Chunk;

    fn hit(id: &str, score: f32) -> Scored {
        let mut c = Chunk::new("d", 0, "t", 0);
        c.id = id.to_string();
        Scored::new(c, score)
    }

    #[test]
    fn rewards_agreement_across_lists() {
        // `b` is mid-rank in both lists; `a` is top of one but absent from the other.
        let l1 = vec![hit("a", 9.0), hit("b", 5.0), hit("c", 1.0)];
        let l2 = vec![hit("b", 9.0), hit("d", 5.0), hit("a", 0.5)];
        let fused = rrf(&[l1, l2], 1.0, 4);
        // `b` appears high in both, so it should win.
        assert_eq!(fused[0].chunk.id, "b");
    }

    #[test]
    fn dedups_and_limits() {
        let l1 = vec![hit("a", 1.0), hit("b", 1.0)];
        let l2 = vec![hit("a", 1.0)];
        let fused = rrf(&[l1, l2], 60.0, 10);
        assert_eq!(fused.len(), 2); // a merged, not duplicated
        assert_eq!(fused[0].chunk.id, "a"); // a scored by two lists
    }
}
