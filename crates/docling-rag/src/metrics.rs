//! Per-document processing metrics, captured during ingestion and stored in the
//! document's JSON `metadata` column under the `"metrics"` key — a plain JSON
//! object, so new metrics can be added later without a schema migration.
//!
//! Shape:
//!
//! ```json
//! {
//!   "file_bytes": 48213,
//!   "pages": 12,
//!   "words": 5120,
//!   "chunks": 18,
//!   "embedded_words": 5490,
//!   "parsing":   { "seconds": 1.42, "words_per_sec": 3605.6, "pages_per_sec": 8.45 },
//!   "chunking":  { "seconds": 0.003, "words_per_sec": 1706666.7 },
//!   "embedding": { "seconds": 4.81, "words_per_sec": 1141.4 }
//! }
//! ```

use docling::InputFormat;
use serde::Serialize;

/// Timing and throughput for one processing phase.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct PhaseMetrics {
    /// Wall-clock duration of the phase.
    pub seconds: f64,
    /// Words processed per second; absent when the duration was too small to measure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words_per_sec: Option<f64>,
    /// Pages per second (parsing only, when the page count is known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages_per_sec: Option<f64>,
}

impl PhaseMetrics {
    fn new(words: usize, pages: Option<usize>, seconds: f64) -> Self {
        PhaseMetrics {
            seconds: round3(seconds),
            words_per_sec: rate(words, seconds),
            pages_per_sec: pages.and_then(|p| rate(p, seconds)),
        }
    }
}

/// All metrics recorded for one ingested document.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProcessingMetrics {
    /// Size of the source file as fetched, in bytes.
    pub file_bytes: u64,
    /// Page/slide count, when the format has one (PDF pages, PPTX slides).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<usize>,
    /// Words in the converted Markdown.
    pub words: usize,
    /// Number of chunks produced.
    pub chunks: usize,
    /// Total words actually embedded (includes chunk overlap and heading context).
    pub embedded_words: usize,
    /// Conversion to Markdown (`docling.rs`).
    pub parsing: PhaseMetrics,
    /// Markdown → chunks.
    pub chunking: PhaseMetrics,
    /// Chunks → vectors.
    pub embedding: PhaseMetrics,
}

/// Raw phase durations measured by the pipeline, in seconds.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timings {
    pub parse_secs: f64,
    pub chunk_secs: f64,
    pub embed_secs: f64,
}

impl ProcessingMetrics {
    /// Combine counts and timings into rate metrics.
    pub fn compute(
        file_bytes: u64,
        pages: Option<usize>,
        words: usize,
        chunks: usize,
        embedded_words: usize,
        t: Timings,
    ) -> Self {
        ProcessingMetrics {
            file_bytes,
            pages,
            words,
            chunks,
            embedded_words,
            parsing: PhaseMetrics::new(words, pages, t.parse_secs),
            chunking: PhaseMetrics::new(words, None, t.chunk_secs),
            embedding: PhaseMetrics::new(embedded_words, None, t.embed_secs),
        }
    }

    /// The metrics as a JSON value, ready to embed in document metadata.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// `count / secs`, rounded; `None` when the duration is too small to be meaningful.
fn rate(count: usize, secs: f64) -> Option<f64> {
    (secs > 1e-9).then(|| round1(count as f64 / secs))
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Best-effort page/slide count for multi-page formats. Returns `None` for
/// formats without a page notion or when the container can't be read.
pub fn count_pages(format: InputFormat, bytes: &[u8]) -> Option<usize> {
    match format {
        // Real page count via pdfium (the same backend the converter uses).
        InputFormat::Pdf => docling_pdf::pdfium_backend::page_count(bytes, None).ok(),
        // One "page" per slide; count zip entries without decompressing anything.
        InputFormat::Pptx => zip_entry_count(bytes, "ppt/slides/slide", ".xml"),
        // One "page" per sheet.
        InputFormat::Xlsx => zip_entry_count(bytes, "xl/worksheets/sheet", ".xml"),
        _ => None,
    }
}

fn zip_entry_count(bytes: &[u8], prefix: &str, suffix: &str) -> Option<usize> {
    let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let n = archive
        .file_names()
        .filter(|name| name.starts_with(prefix) && name.ends_with(suffix))
        .count();
    (n > 0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_rates_and_serializes() {
        let m = ProcessingMetrics::compute(
            1000,
            Some(4),
            2000,
            10,
            2200,
            Timings {
                parse_secs: 2.0,
                chunk_secs: 0.5,
                embed_secs: 4.0,
            },
        );
        assert_eq!(m.parsing.words_per_sec, Some(1000.0));
        assert_eq!(m.parsing.pages_per_sec, Some(2.0));
        assert_eq!(m.chunking.words_per_sec, Some(4000.0));
        assert_eq!(m.chunking.pages_per_sec, None);
        assert_eq!(m.embedding.words_per_sec, Some(550.0));

        let j = m.to_json();
        assert_eq!(j["file_bytes"], 1000);
        assert_eq!(j["pages"], 4);
        assert_eq!(j["parsing"]["pages_per_sec"], 2.0);
        // chunking has no pages_per_sec key at all (skipped when None).
        assert!(j["chunking"].get("pages_per_sec").is_none());
    }

    #[test]
    fn zero_duration_yields_no_rate() {
        let m = ProcessingMetrics::compute(1, None, 100, 1, 100, Timings::default());
        assert_eq!(m.parsing.words_per_sec, None);
        assert_eq!(m.parsing.pages_per_sec, None);
        // pages absent => key omitted from JSON.
        assert!(m.to_json().get("pages").is_none());
    }

    #[test]
    fn non_container_formats_have_no_pages() {
        assert_eq!(count_pages(InputFormat::Md, b"# hi"), None);
        assert_eq!(count_pages(InputFormat::Pptx, b"not a zip"), None);
    }
}
