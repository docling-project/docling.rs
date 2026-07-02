//! Core data types shared across the pipeline: documents, chunks, scored hits.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// A source document, stored once with its metadata. Its text lives in [`Chunk`]s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Stable id (UUID v4, hyphenated) — primary key.
    pub id: String,
    /// Where the document came from (file path, ftp/sftp URI, …).
    pub source_uri: String,
    /// Human-readable title (first heading, or file stem).
    pub title: String,
    /// Content hash (sha256 hex) used to skip re-ingesting unchanged documents.
    pub hash: String,
    /// Arbitrary metadata (format, byte length, custom fields).
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// RFC-3339 creation timestamp (stored as TEXT so it is DB-portable).
    pub created_at: String,
}

impl Document {
    /// Build a document with a fresh UUID and the given content hash.
    pub fn new(
        source_uri: impl Into<String>,
        title: impl Into<String>,
        hash: impl Into<String>,
    ) -> Self {
        Document {
            id: new_id(),
            source_uri: source_uri.into(),
            title: title.into(),
            hash: hash.into(),
            metadata: serde_json::Value::Null,
            created_at: now_rfc3339(),
        }
    }

    /// Attach a metadata object, replacing any existing one.
    pub fn with_metadata(mut self, meta: serde_json::Value) -> Self {
        self.metadata = meta;
        self
    }
}

/// One retrievable chunk of a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Stable id (UUID v4, hyphenated) — primary key.
    pub id: String,
    /// Owning [`Document::id`].
    pub doc_id: String,
    /// 0-based position of this chunk within its document.
    pub ordinal: i64,
    /// The chunk text (already includes any prepended heading context).
    pub text: String,
    /// Number of units (words/tokens) counted at chunk time — for eval/telemetry.
    pub token_count: i64,
    /// Arbitrary per-chunk metadata (heading path, page, …).
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// The embedding vector; `None` until embedded / when loaded without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

impl Chunk {
    /// Construct a chunk (without embedding) for a document.
    pub fn new(
        doc_id: impl Into<String>,
        ordinal: i64,
        text: impl Into<String>,
        token_count: i64,
    ) -> Self {
        Chunk {
            id: new_id(),
            doc_id: doc_id.into(),
            ordinal,
            text: text.into(),
            token_count,
            metadata: serde_json::Value::Null,
            embedding: None,
        }
    }
}

/// A retrieval hit: a chunk plus the score that ranked it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scored {
    /// The retrieved chunk.
    pub chunk: Chunk,
    /// Higher is better. Cosine similarity for vector search, BM25 score for
    /// keyword search, fused rank score for hybrid/multi-query.
    pub score: f32,
}

impl Scored {
    /// Pair a chunk with a score.
    pub fn new(chunk: Chunk, score: f32) -> Self {
        Scored { chunk, score }
    }
}

/// The retrieval strategy to run for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetrievalMode {
    /// Dense vector (semantic) search only.
    Vector,
    /// Sparse keyword search only (Okapi BM25).
    Bm25,
    /// Vector + BM25, fused with Reciprocal Rank Fusion.
    Hybrid,
    /// LLM rewrites the query into N variants; results are fused (RRF).
    MultiQuery,
    /// LLM writes a hypothetical answer; its embedding drives vector search.
    Hyde,
}

impl RetrievalMode {
    /// Whether this mode needs a [`crate::llm::ChatModel`] to run.
    pub fn needs_llm(self) -> bool {
        matches!(self, RetrievalMode::MultiQuery | RetrievalMode::Hyde)
    }

    /// All modes, for the evaluation matrix.
    pub const ALL: [RetrievalMode; 5] = [
        RetrievalMode::Vector,
        RetrievalMode::Bm25,
        RetrievalMode::Hybrid,
        RetrievalMode::MultiQuery,
        RetrievalMode::Hyde,
    ];

    /// Modes that run without any network LLM — used by offline eval.
    pub const OFFLINE: [RetrievalMode; 3] = [
        RetrievalMode::Vector,
        RetrievalMode::Bm25,
        RetrievalMode::Hybrid,
    ];
}

impl std::fmt::Display for RetrievalMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            RetrievalMode::Vector => "vector",
            RetrievalMode::Bm25 => "bm25",
            RetrievalMode::Hybrid => "hybrid",
            RetrievalMode::MultiQuery => "multi-query",
            RetrievalMode::Hyde => "hyde",
        };
        f.write_str(s)
    }
}

impl FromStr for RetrievalMode {
    type Err = crate::RagError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "vector" | "dense" | "semantic" => Ok(RetrievalMode::Vector),
            "bm25" | "keyword" | "sparse" => Ok(RetrievalMode::Bm25),
            "hybrid" => Ok(RetrievalMode::Hybrid),
            "multi-query" | "multiquery" | "fusion" => Ok(RetrievalMode::MultiQuery),
            "hyde" => Ok(RetrievalMode::Hyde),
            other => Err(crate::RagError::config(format!(
                "unknown retrieval mode '{other}'"
            ))),
        }
    }
}

/// Generate a fresh hyphenated UUID v4.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// sha256 hex digest of the given bytes — used for document dedup.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Current time as an RFC-3339 string, without pulling in `chrono`.
///
/// Uses the system clock; formatted as UTC epoch seconds when the platform
/// clock is available, falling back to `"1970-01-01T00:00:00Z"`.
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format_epoch_utc(d.as_secs()),
        Err(_) => "1970-01-01T00:00:00Z".to_string(),
    }
}

/// Minimal civil-date formatter (UTC) so we avoid a chrono dependency for a
/// single timestamp field. Correct for all dates after the Unix epoch.
fn format_epoch_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrieval_mode_roundtrip() {
        for m in RetrievalMode::ALL {
            let s = m.to_string();
            assert_eq!(RetrievalMode::from_str(&s).unwrap(), m, "roundtrip {s}");
        }
        assert_eq!(
            RetrievalMode::from_str("KEYWORD").unwrap(),
            RetrievalMode::Bm25
        );
        assert!(RetrievalMode::from_str("nope").is_err());
    }

    #[test]
    fn hash_is_stable_and_hex() {
        let h = content_hash(b"hello world");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, content_hash(b"hello world"));
        assert_ne!(h, content_hash(b"hello worlds"));
    }

    #[test]
    fn epoch_formats_known_dates() {
        assert_eq!(format_epoch_utc(0), "1970-01-01T00:00:00Z");
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(format_epoch_utc(1_609_459_200), "2021-01-01T00:00:00Z");
    }
}
