//! Markdown-aware chunking.
//!
//! The chunker parses Markdown into *sections* (split at every heading, carrying a
//! heading path), then slides a fixed-size window with fractional overlap over the
//! words of each section. A chunk never crosses a heading boundary, and each chunk
//! is prefixed with its heading path so the embedded text keeps its context.
//!
//! The section parsing and windowing live in `docling::chunker::WindowChunker`
//! (shared with the Python/Node bindings); this module maps its chunks onto
//! retrievable [`Chunk`]s and adds the incremental [`StreamingChunker`] buffer.

use crate::config::{ChunkUnit, ChunkerKind};
use crate::model::Chunk;
use crate::{RagError, Result};
use docling::chunker::{parse_sections_with_stack, WindowChunker};

pub use docling::chunker::Section;

/// Chunk a converted document with docling's structure-aware chunkers
/// (`RAG_CHUNKER=hierarchical|hybrid`), mapping each `DocChunk` onto a
/// retrievable [`Chunk`]. The embedded text is docling's `contextualize()`
/// rendering (heading path + chunk body), the analogue of the window chunker's
/// heading-context prefix; the heading path and source item refs are kept as
/// chunk metadata.
pub fn docling_chunks(
    doc_id: &str,
    document: &docling::DoclingDocument,
    kind: ChunkerKind,
    tokenizer: Option<&str>,
    max_tokens: usize,
) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    docling_chunks_with(doc_id, document, kind, tokenizer, max_tokens, &mut |c| {
        chunks.push(c);
        true
    })?;
    Ok(chunks)
}

/// Streaming [`docling_chunks`]: `sink` receives each retrievable [`Chunk`] as
/// the docling chunkers produce it, so chunks can flow into embedding while
/// the rest of the document is still being chunked. A `false` return from
/// `sink` cancels the chunking.
pub fn docling_chunks_with(
    doc_id: &str,
    document: &docling::DoclingDocument,
    kind: ChunkerKind,
    tokenizer: Option<&str>,
    max_tokens: usize,
    sink: &mut dyn FnMut(Chunk) -> bool,
) -> Result<()> {
    use docling::chunker::{contextualize, DocChunk, HierarchicalChunker, HybridChunker};
    let mut index: i64 = 0;
    let mut map_sink = |c: DocChunk| -> bool {
        let text = contextualize(&c);
        let words = text.split_whitespace().count();
        let mut chunk = Chunk::new(doc_id, index, text, words as i64);
        chunk.metadata = serde_json::json!({
            "headings": c.headings,
            "doc_items": c.doc_items.iter().map(|d| d.self_ref.clone()).collect::<Vec<_>>(),
        });
        index += 1;
        sink(chunk)
    };
    match kind {
        ChunkerKind::Hierarchical => HierarchicalChunker.chunk_with(document, &mut map_sink),
        ChunkerKind::Hybrid => {
            // RAG_CHUNK_TOKENIZER, or the download script's default location
            // (models/chunk/tokenizer.json) when unset.
            let tok = docling::chunker::HuggingFaceTokenizer::resolve(tokenizer, max_tokens)
                .map_err(RagError::config)?;
            HybridChunker::new(tok).chunk_with(document, &mut map_sink)
        }
        ChunkerKind::Window => {
            return Err(RagError::config(
                "docling_chunks handles the hierarchical/hybrid chunkers only",
            ))
        }
    }
    Ok(())
}

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

    /// Report a word count back in the configured unit (for `Chunk::token_count`).
    fn to_units(&self, words: usize) -> i64 {
        match self.unit {
            ChunkUnit::Word => words as i64,
            ChunkUnit::Token => (words as f32 / WORDS_PER_TOKEN).round() as i64,
        }
    }

    /// Chunk a Markdown document into [`Chunk`]s owned by `doc_id`.
    ///
    /// Implemented over [`StreamingChunker`] so batch and streaming ingestion
    /// share one code path (and cannot diverge).
    pub fn chunk(&self, doc_id: &str, markdown: &str) -> Vec<Chunk> {
        let mut streaming = self.streaming(doc_id);
        let mut chunks = streaming.push(markdown);
        chunks.extend(streaming.finish());
        chunks
    }

    /// Start an incremental chunking session for one document. Feed Markdown
    /// pieces with [`StreamingChunker::push`] as they arrive from the parser and
    /// collect completed chunks immediately; call [`StreamingChunker::finish`]
    /// after the last piece.
    pub fn streaming(&self, doc_id: &str) -> StreamingChunker {
        StreamingChunker {
            chunker: self.clone(),
            doc_id: doc_id.to_string(),
            buffer: String::new(),
            heading_stack: Vec::new(),
            ordinal: 0,
            in_fence: false,
        }
    }

    /// Slide the window over one completed section, appending chunks.
    /// The windowing is `docling::chunker::WindowChunker`'s; this maps each
    /// window onto a retrievable [`Chunk`] (ordinal, configured units,
    /// heading-context-prefixed text, heading metadata).
    fn pack_section(
        &self,
        doc_id: &str,
        section: &Section,
        ordinal: &mut i64,
        out: &mut Vec<Chunk>,
    ) {
        let window = WindowChunker::new(self.word_budget(), self.overlap);
        window.pack_section(section, &mut |c| {
            let words = c.text.split_whitespace().count();
            let text = WindowChunker::contextualize(&c);
            let mut chunk = Chunk::new(doc_id, *ordinal, text, self.to_units(words));
            if let Some(headings) = &c.headings {
                chunk.metadata = serde_json::json!({ "headings": headings });
            }
            out.push(chunk);
            *ordinal += 1;
            true
        });
    }
}

/// Incremental Markdown chunker: buffers streamed pieces and emits chunks for
/// every section completed so far. A section only completes at the next heading
/// (chunks never cross headings), so the buffer is cut at the start of the last
/// heading line — everything before it is fully-formed blocks. Heading lines
/// inside code fences are not cut points.
pub struct StreamingChunker {
    chunker: Chunker,
    doc_id: String,
    /// Markdown received but not yet chunked (the still-open tail section).
    buffer: String,
    /// Heading path in effect at the start of `buffer`.
    heading_stack: Vec<String>,
    /// Next chunk ordinal.
    ordinal: i64,
    /// Whether `buffer` starts inside a ``` code fence.
    in_fence: bool,
}

impl StreamingChunker {
    /// Feed the next piece of Markdown; returns chunks for sections it completed.
    pub fn push(&mut self, piece: &str) -> Vec<Chunk> {
        self.buffer.push_str(piece);

        // Find the byte offset of the START of the last ATX-heading line outside
        // a code fence (cut point). An ATX heading always terminates the block
        // before it, so everything ahead of the cut is complete Markdown.
        let mut fence_open = self.in_fence;
        let mut cut: Option<usize> = None;
        let mut offset = 0;
        for line in self.buffer.split_inclusive('\n') {
            // Only complete lines can be judged — a trailing fragment like "#"
            // might still grow into "#hashtag" (paragraph text, not a heading).
            let complete = line.ends_with('\n');
            if is_fence_marker(line) {
                if complete {
                    fence_open = !fence_open;
                }
            } else if complete && !fence_open && offset > 0 && is_atx_heading(line) {
                cut = Some(offset);
            }
            offset += line.len();
        }

        let Some(cut) = cut else {
            return Vec::new();
        };
        let flushed: String = self.buffer.drain(..cut).collect();
        // The cut line is a heading outside any fence, so the tail starts unfenced.
        self.in_fence = false;
        self.emit(&flushed)
    }

    /// Chunk whatever is left in the buffer (the final section).
    pub fn finish(&mut self) -> Vec<Chunk> {
        let rest = std::mem::take(&mut self.buffer);
        if rest.trim().is_empty() {
            return Vec::new();
        }
        self.emit(&rest)
    }

    fn emit(&mut self, markdown: &str) -> Vec<Chunk> {
        let (sections, stack) =
            parse_sections_with_stack(markdown, std::mem::take(&mut self.heading_stack));
        self.heading_stack = stack;
        let mut out = Vec::new();
        for section in &sections {
            self.chunker
                .pack_section(&self.doc_id, section, &mut self.ordinal, &mut out);
        }
        out
    }
}

/// CommonMark allows up to 3 leading spaces before block markers (4+ means an
/// indented code block).
fn strip_up_to_3_spaces(line: &str) -> &str {
    let mut s = line;
    for _ in 0..3 {
        match s.strip_prefix(' ') {
            Some(rest) => s = rest,
            None => break,
        }
    }
    s
}

/// A ``` / ~~~ code-fence marker line.
fn is_fence_marker(line: &str) -> bool {
    let t = strip_up_to_3_spaces(line);
    t.starts_with("```") || t.starts_with("~~~")
}

/// A real ATX heading: 1–6 `#` followed by whitespace or end-of-line
/// (`#hashtag` is paragraph text, not a heading).
fn is_atx_heading(line: &str) -> bool {
    let t = strip_up_to_3_spaces(line);
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&hashes)
        && matches!(
            t.as_bytes().get(hashes),
            None | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch chunking and streaming chunking must produce identical chunks
    /// (text, ordinal, metadata) no matter where the input is split.
    #[test]
    fn streaming_equals_batch_at_awkward_splits() {
        let md = "\
intro words before any heading
# Chapter 1
alpha beta gamma delta epsilon zeta eta theta
## Section 1.1
```
# not a heading, just code
code line two
```
more prose after the fence with several words here
#hashtag is paragraph text, not a heading
# Chapter 2
final words of the document";

        let chunker = Chunker {
            size: 8,
            overlap: 0.25,
            unit: ChunkUnit::Word,
        };
        let batch = chunker.chunk("doc", md);
        assert!(batch.len() > 3, "test doc should produce several chunks");

        // Split at every possible byte boundary pair (two cuts => three pieces).
        for step in [1usize, 3, 7, 11, 24] {
            let mut streaming = chunker.streaming("doc");
            let mut got = Vec::new();
            let bytes = md.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let end = (i + step).min(bytes.len());
                // Split only at char boundaries (guaranteed here: ASCII test doc).
                got.extend(streaming.push(std::str::from_utf8(&bytes[i..end]).unwrap()));
                i = end;
            }
            got.extend(streaming.finish());

            assert_eq!(got.len(), batch.len(), "chunk count differs at step {step}");
            for (g, b) in got.iter().zip(&batch) {
                assert_eq!(g.text, b.text, "text differs at step {step}");
                assert_eq!(g.ordinal, b.ordinal, "ordinal differs at step {step}");
                assert_eq!(g.metadata, b.metadata, "metadata differs at step {step}");
                assert_eq!(g.token_count, b.token_count);
            }
        }
    }

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

#[cfg(test)]
mod docling_chunker_tests {
    use super::*;

    fn convert(md: &str) -> docling::DoclingDocument {
        let src = docling::SourceDocument::from_bytes("t.md", docling::InputFormat::Md, md.into());
        docling::DocumentConverter::new()
            .convert(src)
            .expect("convert")
            .document
    }

    #[test]
    fn hierarchical_maps_docchunks_onto_rag_chunks() {
        let doc = convert("# Guide\n\n## Setup\n\nInstall the tools.\n\n- clone\n- build\n");
        let chunks =
            docling_chunks("doc-1", &doc, ChunkerKind::Hierarchical, None, 0).expect("chunk");
        assert!(chunks.len() >= 2);
        let setup = chunks
            .iter()
            .find(|c| c.text.contains("Install"))
            .expect("setup chunk");
        // Embedded text is the contextualized rendering (heading path + body).
        assert_eq!(setup.text, "Guide\nSetup\nInstall the tools.");
        assert_eq!(setup.doc_id, "doc-1");
        assert_eq!(setup.metadata["headings"][1], "Setup");
        assert!(setup.metadata["doc_items"][0]
            .as_str()
            .unwrap()
            .starts_with("#/"));
        // Ordinals are the chunk sequence.
        assert!(chunks.windows(2).all(|w| w[0].ordinal + 1 == w[1].ordinal));
    }

    #[test]
    fn hybrid_without_tokenizer_is_a_config_error() {
        // With no explicit path the resolver falls back to the download
        // script's default location — only its absence is an error.
        if std::path::Path::new(docling::chunker::DEFAULT_TOKENIZER_PATH).exists() {
            return;
        }
        let doc = convert("# A\n\ntext\n");
        assert!(docling_chunks("d", &doc, ChunkerKind::Hybrid, None, 256).is_err());
    }

    #[test]
    fn window_kind_is_rejected() {
        let doc = convert("# A\n\ntext\n");
        assert!(docling_chunks("d", &doc, ChunkerKind::Window, None, 0).is_err());
    }

    #[test]
    fn streaming_sink_sees_the_batch_chunks_and_can_cancel() {
        let doc = convert("# Guide\n\n## Setup\n\nInstall the tools.\n\n- clone\n- build\n");
        let all = docling_chunks("d", &doc, ChunkerKind::Hierarchical, None, 0).expect("chunk");
        assert!(all.len() >= 2);

        // The sink receives the same chunks in the same order...
        let mut streamed = Vec::new();
        docling_chunks_with("d", &doc, ChunkerKind::Hierarchical, None, 0, &mut |c| {
            streamed.push(c);
            true
        })
        .expect("stream");
        assert_eq!(
            streamed.iter().map(|c| &c.text).collect::<Vec<_>>(),
            all.iter().map(|c| &c.text).collect::<Vec<_>>()
        );

        // ...and a false return cancels after the first chunk.
        let mut n = 0;
        docling_chunks_with("d", &doc, ChunkerKind::Hierarchical, None, 0, &mut |_| {
            n += 1;
            false
        })
        .expect("cancel");
        assert_eq!(n, 1);
    }
}
