//! Markdown-aware chunking.
//!
//! The chunker parses Markdown into *sections* (split at every heading, carrying a
//! heading path), then slides a fixed-size window with fractional overlap over the
//! words of each section. A chunk never crosses a heading boundary, and each chunk
//! is prefixed with its heading path so the embedded text keeps its context.

mod markdown;

use crate::config::ChunkUnit;
use crate::model::Chunk;

pub use markdown::Section;

/// Words per token, used to convert a token budget into a word budget when
/// `unit == Token` (English text averages ≈1.3 tokens/word).
const WORDS_PER_TOKEN: f32 = 0.75;

/// A configured Markdown chunker.
#[derive(Debug, Clone)]
pub struct Chunker {
    /// Target chunk size, measured in `unit`s (default 300).
    pub size: usize,
    /// Fractional overlap between consecutive chunks (default 0.05 = 5%).
    pub overlap: f32,
    /// The unit `size`/`overlap` are measured in.
    pub unit: ChunkUnit,
}

impl Default for Chunker {
    fn default() -> Self {
        Chunker {
            size: 300,
            overlap: 0.05,
            unit: ChunkUnit::Word,
        }
    }
}

impl Chunker {
    /// Build a chunker from the resolved config.
    pub fn from_config(cfg: &crate::RagConfig) -> Self {
        Chunker {
            size: cfg.chunk_size,
            overlap: cfg.chunk_overlap,
            unit: cfg.chunk_unit,
        }
    }

    /// The window size in *words*, derived from the configured unit.
    fn word_budget(&self) -> usize {
        match self.unit {
            ChunkUnit::Word => self.size.max(1),
            // Interpret `size` as a token budget and convert to words.
            ChunkUnit::Token => ((self.size as f32 * WORDS_PER_TOKEN).round() as usize).max(1),
        }
    }

    /// Number of words carried from one chunk into the next.
    fn overlap_words(&self, budget: usize) -> usize {
        let o = (budget as f32 * self.overlap).round() as usize;
        // Keep at least one word of movement so windowing always makes progress.
        o.min(budget.saturating_sub(1))
    }

    /// Report a word count back in the configured unit (for `Chunk::token_count`).
    fn to_units(&self, words: usize) -> i64 {
        match self.unit {
            ChunkUnit::Word => words as i64,
            ChunkUnit::Token => (words as f32 / WORDS_PER_TOKEN).round() as i64,
        }
    }

    /// Chunk a Markdown document into [`Chunk`]s owned by `doc_id`.
    pub fn chunk(&self, doc_id: &str, markdown: &str) -> Vec<Chunk> {
        let budget = self.word_budget();
        let step = budget - self.overlap_words(budget); // ≥ 1 by construction
        let sections = markdown::parse_sections(markdown);

        let mut chunks = Vec::new();
        let mut ordinal: i64 = 0;
        for section in &sections {
            let words = &section.words;
            if words.is_empty() {
                continue;
            }
            let context = section.heading_context();
            let mut start = 0;
            loop {
                let end = (start + budget).min(words.len());
                let body = words[start..end].join(" ");
                let text = if context.is_empty() {
                    body
                } else {
                    format!("{context}\n\n{body}")
                };
                let mut chunk = Chunk::new(doc_id, ordinal, text, self.to_units(end - start));
                if !section.heading_path.is_empty() {
                    chunk.metadata = serde_json::json!({ "headings": section.heading_path });
                }
                chunks.push(chunk);
                ordinal += 1;
                if end >= words.len() {
                    break;
                }
                start += step;
            }
        }
        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_word_count(c: &Chunk) -> usize {
        // Body is everything after the blank line that follows the heading context.
        let body = c.text.rsplit("\n\n").next().unwrap_or(&c.text);
        body.split_whitespace().count()
    }

    #[test]
    fn respects_size_and_overlap() {
        // One section, 100 words, size 20, overlap 0.10 => step 18.
        let words: Vec<String> = (0..100).map(|i| format!("w{i}")).collect();
        let md = format!("# Title\n\n{}", words.join(" "));
        let chunker = Chunker {
            size: 20,
            overlap: 0.10,
            unit: ChunkUnit::Word,
        };
        let chunks = chunker.chunk("doc", &md);

        assert!(chunks.len() > 1);
        // Every non-final chunk carries exactly `size` body words.
        for c in &chunks[..chunks.len() - 1] {
            assert_eq!(body_word_count(c), 20, "chunk body size");
        }
        // Consecutive chunks overlap by budget-step = 2 words.
        let first_body: Vec<&str> = chunks[0]
            .text
            .rsplit("\n\n")
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        let second_body: Vec<&str> = chunks[1]
            .text
            .rsplit("\n\n")
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(
            &first_body[18..20],
            &second_body[0..2],
            "overlap tail carried forward"
        );
    }

    #[test]
    fn never_crosses_heading_boundary() {
        let md = "# A\n\nalpha beta gamma\n\n# B\n\ndelta epsilon";
        let chunker = Chunker {
            size: 100,
            overlap: 0.05,
            unit: ChunkUnit::Word,
        };
        let chunks = chunker.chunk("doc", md);
        // Two sections, each small => exactly two chunks.
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("alpha") && !chunks[0].text.contains("delta"));
        assert!(chunks[1].text.contains("delta") && !chunks[1].text.contains("alpha"));
    }

    #[test]
    fn prepends_heading_context() {
        let md = "# Guide\n\n## Setup\n\ninstall the thing";
        let chunker = Chunker::default();
        let chunks = chunker.chunk("doc", md);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.starts_with("# Guide > Setup"));
        assert!(chunks[0].text.contains("install the thing"));
        assert_eq!(chunks[0].metadata["headings"][0], "Guide");
    }

    #[test]
    fn token_unit_converts_budget() {
        let words: Vec<String> = (0..80).map(|i| format!("w{i}")).collect();
        let md = words.join(" ");
        // 40 tokens ≈ 30 words per chunk.
        let chunker = Chunker {
            size: 40,
            overlap: 0.0,
            unit: ChunkUnit::Token,
        };
        let chunks = chunker.chunk("doc", &md);
        assert_eq!(body_word_count(&chunks[0]), 30);
        // token_count is reported back in tokens (30 words / 0.75 = 40).
        assert_eq!(chunks[0].token_count, 40);
    }
}
