//! Local ONNX embedding provider (feature `onnx-embed`).
//!
//! Reuses the same `ort` pattern as `docling-pdf` (build a `Session`, feed
//! tensors, extract the output) plus a Hugging Face `tokenizers` tokenizer. Runs a
//! transformer encoder (e.g. `bge-m3`), mean-pools the last hidden state over the
//! attention mask, and L2-normalizes — the standard sentence-embedding recipe.
//!
//! This backend is compile-checked here but exercised only where the model files
//! and native ONNX Runtime are present; see the crate README.

use super::Embedder;
use crate::{math, RagConfig, RagError, Result};
use async_trait::async_trait;
use ort::session::Session;
use ort::value::Tensor;
use std::sync::{Arc, Mutex};
use tokenizers::Tokenizer;

/// Embedder backed by a local ONNX transformer encoder.
pub struct OnnxEmbedder {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Tokenizer>,
    dim: usize,
    id: String,
}

impl OnnxEmbedder {
    /// Load the ONNX model and tokenizer from the paths in `cfg`.
    pub fn from_config(cfg: &RagConfig) -> Result<Self> {
        let builder = Session::builder()
            .map_err(|e| RagError::Embedding(format!("ONNX session builder: {e}")))?;
        // Same execution-provider selection as the PDF pipeline (#74): one
        // DOCLING_RS_EP switch covers the embedder too — a `--features cuda`
        // build embeds on the GPU with per-session CPU fallback.
        let mut builder = docling_pdf::ep::apply(builder)
            .map_err(|e| RagError::Embedding(format!("embedder {e}")))?;
        let session = builder
            .commit_from_file(&cfg.embed_onnx_path)
            .map_err(|e| RagError::Embedding(format!("loading ONNX model: {e}")))?;
        let tokenizer = Tokenizer::from_file(&cfg.embed_tokenizer_path)
            .map_err(|e| RagError::Embedding(format!("loading tokenizer: {e}")))?;
        Ok(OnnxEmbedder {
            session: Arc::new(Mutex::new(session)),
            tokenizer: Arc::new(tokenizer),
            dim: cfg.embed_dim,
            id: format!("onnx:{}", cfg.embed_model),
        })
    }

    fn run(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| RagError::Embedding(format!("tokenize: {e}")))?;
        let batch = encodings.len();
        let seq = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);

        // Right-pad ids / mask / type-ids to a uniform sequence length.
        let mut ids = vec![0i64; batch * seq];
        let mut mask = vec![0i64; batch * seq];
        let types = vec![0i64; batch * seq];
        for (b, enc) in encodings.iter().enumerate() {
            for (t, (&id, &m)) in enc
                .get_ids()
                .iter()
                .zip(enc.get_attention_mask())
                .enumerate()
            {
                ids[b * seq + t] = id as i64;
                mask[b * seq + t] = m as i64;
            }
        }

        let id_tensor = Tensor::from_array(([batch, seq], ids))
            .map_err(|e| RagError::Embedding(e.to_string()))?;
        let mask_tensor = Tensor::from_array(([batch, seq], mask.clone()))
            .map_err(|e| RagError::Embedding(e.to_string()))?;
        let type_tensor = Tensor::from_array(([batch, seq], types))
            .map_err(|e| RagError::Embedding(e.to_string()))?;

        let mut session = self.session.lock().expect("onnx session mutex poisoned");
        let outputs = session
            .run(ort::inputs![
                "input_ids" => id_tensor,
                "attention_mask" => mask_tensor,
                "token_type_ids" => type_tensor,
            ])
            .map_err(|e| RagError::Embedding(format!("onnx run: {e}")))?;

        // last_hidden_state: [batch, seq, hidden]
        let (shape, data) = outputs["last_hidden_state"]
            .try_extract_tensor::<f32>()
            .map_err(|e| RagError::Embedding(e.to_string()))?;
        let hidden = *shape.last().unwrap_or(&0) as usize;

        let mut out = Vec::with_capacity(batch);
        for b in 0..batch {
            let mut pooled = vec![0.0f32; hidden];
            let mut count = 0.0f32;
            for t in 0..seq {
                if mask[b * seq + t] == 0 {
                    continue;
                }
                let base = (b * seq + t) * hidden;
                for (h, p) in pooled.iter_mut().enumerate() {
                    *p += data[base + h];
                }
                count += 1.0;
            }
            if count > 0.0 {
                for p in pooled.iter_mut() {
                    *p /= count;
                }
            }
            math::normalize(&mut pooled);
            out.push(pooled);
        }
        Ok(out)
    }
}

#[async_trait]
impl Embedder for OnnxEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let this = OnnxCtx {
            session: self.session.clone(),
            tokenizer: self.tokenizer.clone(),
            dim: self.dim,
            id: self.id.clone(),
        };
        let texts = texts.to_vec();
        // ONNX inference is blocking CPU work; keep it off the async runtime.
        tokio::task::spawn_blocking(move || {
            let e = OnnxEmbedder {
                session: this.session,
                tokenizer: this.tokenizer,
                dim: this.dim,
                id: this.id,
            };
            e.run(&texts)
        })
        .await
        .map_err(|e| RagError::Embedding(format!("join: {e}")))?
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        &self.id
    }
}

/// Owned handles moved into the blocking task.
struct OnnxCtx {
    session: Arc<Mutex<Session>>,
    tokenizer: Arc<Tokenizer>,
    dim: usize,
    id: String,
}
