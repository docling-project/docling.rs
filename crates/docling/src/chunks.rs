//! Chunk-record JSON export shared by the CLI (`--to chunks`) and the HTTP
//! server (`to=chunks`): the hierarchical chunker's records always, plus the
//! hybrid chunker's when a tokenizer is available — `DOCLING_CHUNK_TOKENIZER`,
//! or `models/chunk/tokenizer.json` as populated by
//! `scripts/install/download_dependencies.sh` (requires the `chunking` build
//! feature; `DOCLING_CHUNK_MAX_TOKENS` overrides the default budget of 256).

use docling_core::chunker::{contextualize, DocChunk, HierarchicalChunker};
use docling_core::DoclingDocument;

fn records(chunks: &[DocChunk]) -> serde_json::Value {
    serde_json::Value::Array(
        chunks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "text": c.text,
                    "headings": c.headings,
                    "doc_items": c.doc_items.iter().map(|i| i.self_ref.clone()).collect::<Vec<_>>(),
                    "contextualize": contextualize(c),
                })
            })
            .collect(),
    )
}

/// The chunk records for `document` as a JSON object
/// (`{"hierarchical": [...], "hybrid": [...]?}`). Tokenizer problems don't
/// fail the export — the hybrid records are skipped and the problem is
/// reported through `warn`.
pub fn chunk_records(
    document: &DoclingDocument,
    warn: &mut dyn FnMut(String),
) -> serde_json::Value {
    let hierarchical = HierarchicalChunker.chunk(document);
    #[cfg_attr(not(feature = "chunking"), allow(unused_mut, unused_variables))]
    let mut out = serde_json::json!({ "hierarchical": records(&hierarchical) });

    #[cfg(feature = "chunking")]
    if let Ok(tok_path) = std::env::var("DOCLING_CHUNK_TOKENIZER").or_else(|_| {
        docling_core::chunker::resolve_tokenizer_path(None)
            .map_err(|_| std::env::VarError::NotPresent)
    }) {
        let max_tokens = std::env::var("DOCLING_CHUNK_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);
        match docling_core::chunker::HuggingFaceTokenizer::from_file(&tok_path, max_tokens) {
            Ok(tok) => {
                let hybrid = docling_core::chunker::HybridChunker::new(tok).chunk(document);
                out["hybrid"] = records(&hybrid);
            }
            Err(e) => warn(e.to_string()),
        }
    }
    #[cfg(not(feature = "chunking"))]
    let _ = warn;
    out
}
