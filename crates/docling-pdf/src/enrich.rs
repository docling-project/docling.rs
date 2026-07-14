//! Optional enrichment models (docling's `do_picture_classification` /
//! `do_code_enrichment` / `do_formula_enrichment`, issue #76).
//!
//! * [`PictureClassifier`] — docling-project/DocumentFigureClassifier-v2.5, an
//!   EfficientNet image classifier over 26 figure classes (bar_chart, logo,
//!   signature, …). The HF repo ships the ONNX graph as-is; docling's ViT
//!   preprocessing is 224×224 bilinear + rescale + normalize, and the raw
//!   logits are softmaxed and sorted descending — the full distribution lands
//!   on the picture item like docling's `PictureClassificationData`.
//!
//! * [`CodeFormula`] — docling-project/CodeFormulaV2, an Idefics3/SmolVLM-class
//!   VLM that rewrites a code crop as clean source text prefixed with
//!   `<_language_>`, or a formula crop as LaTeX. Exported to three graphs by
//!   `scripts/install/export_code_formula.py` (vision tower+connector, token
//!   embeddings, and a KV-cached Llama decoder step verified argmax-identical
//!   to `transformers.generate`); this module ports the Idefics3 preprocessing
//!   (longest-edge 2048 resize → multiple-of-512 resize → 512×512 tiling + a
//!   squashed global tile), the tiled `<image>` prompt, and the greedy decode.

use image::RgbImage;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use docling_core::PictureClass;

/// docling crops enrichment inputs at `images_scale` pixels per point:
/// 2.0 for the picture classifier, 1.67 (≈120 dpi) for CodeFormula.
pub const CLASSIFIER_SCALE: f32 = 2.0;
pub const CODE_FORMULA_SCALE: f32 = 1.67;
/// CodeFormula expands the region box by 18% of its size on every side
/// (docling's `expansion_factor`) before cropping.
pub const CODE_FORMULA_EXPANSION: f32 = 0.18;

// ---------------------------------------------------------------------------
// Picture classifier
// ---------------------------------------------------------------------------

/// The 26 classes of DocumentFigureClassifier-v2.5, indexed by model class id
/// (`config.json` `id2label`).
const PICTURE_CLASSES: [&str; 26] = [
    "logo",
    "photograph",
    "icon",
    "engineering_drawing",
    "line_chart",
    "bar_chart",
    "other",
    "table",
    "flow_chart",
    "screenshot_from_computer",
    "signature",
    "screenshot_from_manual",
    "geographical_map",
    "pie_chart",
    "page_thumbnail",
    "stamp",
    "music",
    "calendar",
    "qr_code",
    "bar_code",
    "full_page_image",
    "scatter_plot",
    "chemistry_structure",
    "topographical_map",
    "crossword_puzzle",
    "box_plot",
];

const CLASSIFIER_SIDE: u32 = 224;
/// ViT preprocessing constants from the model's `preprocessor_config.json`.
const CLASSIFIER_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const CLASSIFIER_STD: [f32; 3] = [0.478_539_44, 0.473_286_4, 0.474_341_63];

pub struct PictureClassifier {
    session: Session,
}

impl PictureClassifier {
    /// Load from `DOCLING_PICTURE_CLASSIFIER_ONNX` /
    /// `models/picture_classifier(_int8).onnx`. `None` when the graph is
    /// absent — the caller warns once and skips classification.
    pub fn load_with(intra: usize) -> Option<Self> {
        let path = crate::model_path(
            "DOCLING_PICTURE_CLASSIFIER_ONNX",
            "models/picture_classifier.onnx",
            "models/picture_classifier_int8.onnx",
        );
        if !std::path::Path::new(&path).exists() {
            eprintln!(
                "docling-pdf: picture classifier model not found ({path}); \
                 picture classification skipped. Run scripts/install/download_dependencies.sh."
            );
            return None;
        }
        let session = Session::builder()
            .ok()?
            .with_intra_threads(intra)
            .ok()?
            .commit_from_file(&path)
            .map_err(|e| eprintln!("docling-pdf: picture classifier load {path}: {e}"))
            .ok()?;
        Some(Self { session })
    }

    /// Classify one picture crop: the full 26-class distribution, descending
    /// confidence (docling attaches all predicted classes, not just the top).
    pub fn classify(&mut self, crop: &RgbImage) -> Result<Vec<PictureClass>, String> {
        let resized = image::imageops::resize(
            crop,
            CLASSIFIER_SIDE,
            CLASSIFIER_SIDE,
            image::imageops::FilterType::Triangle,
        );
        let n = (CLASSIFIER_SIDE * CLASSIFIER_SIDE) as usize;
        let mut data = vec![0f32; 3 * n];
        for (i, px) in resized.pixels().enumerate() {
            for c in 0..3 {
                data[c * n + i] = (px[c] as f32 / 255.0 - CLASSIFIER_MEAN[c]) / CLASSIFIER_STD[c];
            }
        }
        let input = Tensor::from_array((
            [
                1usize,
                3,
                CLASSIFIER_SIDE as usize,
                CLASSIFIER_SIDE as usize,
            ],
            data,
        ))
        .map_err(|e| format!("picture classifier: input: {e}"))?;
        let outputs = self
            .session
            .run(ort::inputs!["input" => input])
            .map_err(|e| format!("picture classifier: inference: {e}"))?;
        let (_, logits) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("picture classifier: output: {e}"))?;
        // softmax → (class, prob) sorted descending, like docling's engine.
        let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let exp: Vec<f32> = logits.iter().map(|&v| (v - max).exp()).collect();
        let sum: f32 = exp.iter().sum();
        let mut preds: Vec<PictureClass> = exp
            .iter()
            .enumerate()
            .map(|(i, &e)| PictureClass {
                class_name: PICTURE_CLASSES.get(i).copied().unwrap_or("other").into(),
                confidence: e / sum,
            })
            .collect();
        preds.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        Ok(preds)
    }
}

// ---------------------------------------------------------------------------
// CodeFormula (Idefics3 VLM)
// ---------------------------------------------------------------------------

/// Which prompt the model gets for a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeFormulaKind {
    Code,
    Formula,
}

/// Idefics3 processor constants (CodeFormulaV2's `preprocessor_config.json` /
/// tokenizer). The token ids are fixed by the checkpoint's tokenizer.json.
const TILE: u32 = 512; // max_image_size.longest_edge
const LONGEST_EDGE: u32 = 2048; // size.longest_edge
const MAX_IMAGE_SIZE: u32 = 4096; // transformers' hard upper bound
const IMAGE_SEQ_LEN: usize = 64; // visual tokens per tile
const IMAGE_TOKEN_ID: i64 = 100270; // <image>
const EOS_ID: i64 = 100338; // <end_of_utterance>
const MODEL_MAX_LEN: usize = 8192;
const HIDDEN: usize = 576;
const N_LAYERS: usize = 30;
const N_KV: usize = 3;
const HEAD_DIM: usize = 64;

pub struct CodeFormula {
    vision: Session,
    embed: Session,
    decoder: Session,
    tokenizer: Tokenizer,
}

impl CodeFormula {
    /// Load the three graphs + tokenizer from `DOCLING_CODE_FORMULA_DIR`
    /// (default `models/code_formula/`). `None` when absent, with a one-time
    /// warning from the caller's slot.
    pub fn load_with(intra: usize) -> Option<Self> {
        let dir = std::env::var("DOCLING_CODE_FORMULA_DIR")
            .unwrap_or_else(|_| crate::resolve_asset("models/code_formula"));
        let file = |name: &str| format!("{dir}/{name}");
        // INT8 variants take priority when present, like the other models.
        let graph = |base: &str| {
            let int8 = file(&format!("{base}_int8.onnx"));
            if !crate::fp32_forced() && std::path::Path::new(&int8).exists() {
                int8
            } else {
                file(&format!("{base}.onnx"))
            }
        };
        for f in [&graph("vision"), &graph("embed"), &graph("decoder_kv")] {
            if !std::path::Path::new(f.as_str()).exists() {
                eprintln!(
                    "docling-pdf: CodeFormula model not found ({f}); code/formula \
                     enrichment skipped. Run scripts/install/download_dependencies.sh."
                );
                return None;
            }
        }
        let load = |p: String| {
            Session::builder()
                .ok()?
                .with_intra_threads(intra)
                .ok()?
                .commit_from_file(&p)
                .map_err(|e| eprintln!("docling-pdf: CodeFormula load {p}: {e}"))
                .ok()
        };
        let tokenizer = Tokenizer::from_file(file("tokenizer.json"))
            .map_err(|e| eprintln!("docling-pdf: CodeFormula tokenizer: {e}"))
            .ok()?;
        Some(Self {
            vision: load(graph("vision"))?,
            embed: load(graph("embed"))?,
            decoder: load(graph("decoder_kv"))?,
            tokenizer,
        })
    }

    /// Run the VLM on a code/formula crop and return the post-processed text
    /// (still carrying the `<_language_>` prefix for code — see
    /// [`extract_code_language`]).
    pub fn predict(&mut self, crop: &RgbImage, kind: CodeFormulaKind) -> Result<String, String> {
        // Debug aid: dump each crop the VLM sees (compare against docling's
        // `prepare_element` output when chasing a generation divergence).
        if let Ok(dir) = std::env::var("DOCLING_RS_ENRICH_DEBUG") {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let _ = crop.save(format!("{dir}/rs_crop_{n}.png"));
        }
        let (tiles, rows, cols) = preprocess_idefics3(crop);
        let n_tiles = tiles.len() / (3 * (TILE * TILE) as usize);

        // Vision tower over the tile batch → [T, 64, 576]. (Scoped: the
        // session outputs hold a mutable borrow of the session until dropped.)
        let feats: Vec<f32> = {
            let input = Tensor::from_array(([n_tiles, 3, TILE as usize, TILE as usize], tiles))
                .map_err(|e| format!("code-formula: vision input: {e}"))?;
            let outputs = self
                .vision
                .run(ort::inputs!["pixel_values" => input])
                .map_err(|e| format!("code-formula: vision: {e}"))?;
            let (_, feats) = outputs["image_features"]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("code-formula: vision output: {e}"))?;
            feats.to_vec()
        };

        // Prompt: the chat template with the single <image> expanded into the
        // per-tile token grid (transformers' Idefics3Processor layout).
        let query = match kind {
            CodeFormulaKind::Code => "<code>",
            CodeFormulaKind::Formula => "<formula>",
        };
        let prompt = format!(
            "<|start_of_role|>user:{}{query}<end_of_utterance>\nassistant:",
            image_prompt(rows, cols)
        );
        let enc = self
            .tokenizer
            .encode(prompt, false)
            .map_err(|e| format!("code-formula: tokenize: {e}"))?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&v| v as i64).collect();
        let seq = ids.len();

        // Token embeddings, then scatter the visual tokens into the <image>
        // positions (Idefics3's inputs_merger).
        let mut embeds = self.embed_ids(&ids)?;
        let image_positions: Vec<usize> = ids
            .iter()
            .enumerate()
            .filter(|(_, &t)| t == IMAGE_TOKEN_ID)
            .map(|(i, _)| i)
            .collect();
        if image_positions.len() != n_tiles * IMAGE_SEQ_LEN {
            return Err(format!(
                "code-formula: {} image tokens for {} tiles",
                image_positions.len(),
                n_tiles
            ));
        }
        for (v, &pos) in image_positions.iter().enumerate() {
            embeds[pos * HIDDEN..(pos + 1) * HIDDEN]
                .copy_from_slice(&feats[v * HIDDEN..(v + 1) * HIDDEN]);
        }

        // Greedy KV-cache decode until <end_of_utterance> (docling caps
        // generation at the model's 8192 context). The K/V caches stay owned
        // `ort` values fed straight back into the next step — never extracted
        // or copied (they grow every step, so per-step copies would be
        // O(steps²) float traffic; same pattern as the TableFormer decoder).
        // ort's array constructors reject a 0-length dim, so the zero-`past`
        // first-step tensors go through the session allocator.
        let mut cache: Option<(ort::value::DynValue, ort::value::DynValue)> = None;
        let empty = {
            let mk = || {
                Tensor::<f32>::new(
                    self.decoder.allocator(),
                    [N_LAYERS, 1, N_KV, 0usize, HEAD_DIM],
                )
                .map_err(|e| format!("code-formula: empty kv cache: {e}"))
            };
            (mk()?, mk()?)
        };
        let mut past_len = 0usize;
        let mut positions: Vec<i64> = (0..seq as i64).collect();
        let mut x = embeds;
        let mut x_seq = seq;
        let mut out_ids: Vec<u32> = Vec::new();
        let max_new = MODEL_MAX_LEN.saturating_sub(seq);
        for _ in 0..max_new {
            let embeds_t = Tensor::from_array(([1usize, x_seq, HIDDEN], x))
                .map_err(|e| format!("code-formula: embeds: {e}"))?;
            let pos_t = Tensor::from_array(([1usize, positions.len()], positions.clone()))
                .map_err(|e| format!("code-formula: positions: {e}"))?;
            let next = {
                let mut out = match cache.as_ref() {
                    Some((k, v)) => self.decoder.run(ort::inputs![
                        "inputs_embeds" => embeds_t, "position_ids" => pos_t,
                        "past_k" => k, "past_v" => v]),
                    None => self.decoder.run(ort::inputs![
                        "inputs_embeds" => embeds_t, "position_ids" => pos_t,
                        "past_k" => &empty.0, "past_v" => &empty.1]),
                }
                .map_err(|e| format!("code-formula: decoder: {e}"))?;
                let (_, logits) = out["logits"]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("code-formula: logits: {e}"))?;
                let next = logits
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .map(|(i, _)| i as i64)
                    .unwrap_or(EOS_ID);
                cache = Some((
                    out.remove("new_k")
                        .ok_or_else(|| "code-formula: new_k missing".to_string())?,
                    out.remove("new_v")
                        .ok_or_else(|| "code-formula: new_v missing".to_string())?,
                ));
                next
            };
            past_len += x_seq;
            if next == EOS_ID {
                break;
            }
            out_ids.push(next as u32);
            x = self.embed_ids(&[next])?;
            x_seq = 1;
            positions = vec![past_len as i64];
        }

        let text = self
            .tokenizer
            .decode(&out_ids, false)
            .map_err(|e| format!("code-formula: decode: {e}"))?;
        Ok(post_process(&text))
    }

    fn embed_ids(&mut self, ids: &[i64]) -> Result<Vec<f32>, String> {
        let input = Tensor::from_array(([1usize, ids.len()], ids.to_vec()))
            .map_err(|e| format!("code-formula: ids: {e}"))?;
        let out = self
            .embed
            .run(ort::inputs!["input_ids" => input])
            .map_err(|e| format!("code-formula: embed: {e}"))?;
        let (_, embeds) = out["inputs_embeds"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("code-formula: embed output: {e}"))?;
        Ok(embeds.to_vec())
    }
}

/// The `<image>` expansion for a rows×cols tile grid + global tile
/// (transformers' `Idefics3Processor.replace_image_token`).
fn image_prompt(rows: u32, cols: u32) -> String {
    let img = "<image>".repeat(IMAGE_SEQ_LEN);
    let mut s = String::new();
    for r in 1..=rows {
        for c in 1..=cols {
            s.push_str(&format!("<fake_token_around_image><row_{r}_col_{c}>{img}"));
        }
        s.push('\n');
    }
    s.push_str(&format!(
        "\n<fake_token_around_image><global-img>{img}<fake_token_around_image>"
    ));
    s
}

/// Idefics3 image preprocessing: longest-edge 2048 resize (LANCZOS), resize to
/// multiples of 512, split into 512×512 tiles (row-major) plus the whole image
/// squashed to 512×512 as the trailing global tile, then `(x/255 - 0.5)/0.5`.
/// Returns the flattened `[T,3,512,512]` tensor data and the grid shape.
fn preprocess_idefics3(crop: &RgbImage) -> (Vec<f32>, u32, u32) {
    use image::imageops::FilterType;
    // 1. longest edge → 2048 exactly (up- or down-scale), short side rounded
    //    to even, both clamped below 4096 (rescale_to_max_len).
    let (w0, h0) = crop.dimensions();
    let (mut w, mut h) = rescale_to_max_len(w0, h0, LONGEST_EDGE);
    (h, w) = scale_below_upper_bound(h, w, MAX_IMAGE_SIZE);
    let img = image::imageops::resize(crop, w, h, FilterType::Lanczos3);

    // 2. ceil each side to a multiple of the 512 tile (resize_for_vision_encoder).
    let (tw, th) = if w >= h {
        let tw = w.div_ceil(TILE) * TILE;
        let th0 = (tw as f64 / (w as f64 / h as f64)) as u32;
        (tw, th0.div_ceil(TILE) * TILE)
    } else {
        let th = h.div_ceil(TILE) * TILE;
        let tw0 = (th as f64 * (w as f64 / h as f64)) as u32;
        (tw0.div_ceil(TILE) * TILE, th)
    };
    let img = image::imageops::resize(&img, tw, th, FilterType::Lanczos3);

    // 3. tiles (row-major) + the global squash.
    let (rows, cols) = (th / TILE, tw / TILE);
    let mut tensor = Vec::with_capacity(((rows * cols + 1) * 3 * TILE * TILE) as usize);
    for r in 0..rows {
        for c in 0..cols {
            let tile = image::imageops::crop_imm(&img, c * TILE, r * TILE, TILE, TILE).to_image();
            push_normalized(&mut tensor, &tile);
        }
    }
    let global = image::imageops::resize(&img, TILE, TILE, FilterType::Lanczos3);
    push_normalized(&mut tensor, &global);
    (tensor, rows, cols)
}

/// transformers' `_resize_output_size_rescale_to_max_len`: longest edge to
/// `max_len` exactly, the short side `int(long/aspect)` bumped to even.
fn rescale_to_max_len(w0: u32, h0: u32, max_len: u32) -> (u32, u32) {
    let aspect = w0 as f64 / h0 as f64;
    let (w, h) = if w0 >= h0 {
        let w = max_len;
        let mut h = (w as f64 / aspect) as u32;
        if h % 2 != 0 {
            h += 1;
        }
        (w, h)
    } else {
        let h = max_len;
        let mut w = (h as f64 * aspect) as u32;
        if w % 2 != 0 {
            w += 1;
        }
        (w, h)
    };
    (w.max(1), h.max(1))
}

/// transformers' `_resize_output_size_scale_below_upper_bound` (a no-op unless
/// the even-bump pushed a side past the hard 4096 cap).
fn scale_below_upper_bound(h0: u32, w0: u32, max_len: u32) -> (u32, u32) {
    let aspect = w0 as f64 / h0 as f64;
    let (h, w) = if w0 >= h0 && w0 > max_len {
        let w = max_len;
        (((w as f64 / aspect) as u32).max(1), w)
    } else if h0 > w0 && h0 > max_len {
        let h = max_len;
        (h, ((h as f64 * aspect) as u32).max(1))
    } else {
        (h0, w0)
    };
    (h.max(1), w.max(1))
}

/// Append one 512×512 tile as CHW `(x/255 - 0.5)/0.5`.
fn push_normalized(tensor: &mut Vec<f32>, tile: &RgbImage) {
    let n = (TILE * TILE) as usize;
    let base = tensor.len();
    tensor.resize(base + 3 * n, 0.0);
    for (i, px) in tile.pixels().enumerate() {
        for c in 0..3 {
            tensor[base + c * n + i] = px[c] as f32 / 255.0 * 2.0 - 1.0;
        }
    }
}

/// docling's CodeFormulaModel post-processing: truncate at
/// `<end_of_utterance>`, remove the closing/query artifacts, strip leading
/// whitespace.
fn post_process(text: &str) -> String {
    let mut t = match text.find("<end_of_utterance>") {
        Some(i) => &text[..i],
        None => text,
    }
    .to_string();
    for tok in ["</code>", "</formula>", "<loc_0><loc_0><loc_500><loc_500>"] {
        t = t.replace(tok, "");
    }
    t.trim_start().to_string()
}

/// docling's `_extract_code_language`: an output beginning with
/// `<_language_>` yields `(remainder, Some(language))`.
pub fn extract_code_language(s: &str) -> (String, Option<String>) {
    let rest = match s.strip_prefix("<_") {
        Some(r) => r,
        None => return (s.to_string(), None),
    };
    // The language is everything up to the closing `_>` that contains neither
    // `_` nor `>` (docling's `[^_>]+`).
    match rest.find("_>") {
        Some(end) if !rest[..end].is_empty() && !rest[..end].contains(['_', '>']) => {
            let lang = rest[..end].to_string();
            let remainder = rest[end + 2..].trim_start().to_string();
            (remainder, Some(lang))
        }
        _ => (s.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_prefix_extraction() {
        assert_eq!(
            extract_code_language("<_JavaScript_> function f() {}"),
            (
                "function f() {}".to_string(),
                Some("JavaScript".to_string())
            )
        );
        assert_eq!(
            extract_code_language("plain text"),
            ("plain text".to_string(), None)
        );
        assert_eq!(
            extract_code_language("<_x_y_> t"),
            ("<_x_y_> t".to_string(), None)
        );
    }

    #[test]
    fn idefics3_grid_matches_processor() {
        // An 800×300 crop resizes to 2048×768, tiles to 2048×1024 → 4×2 grid
        // (+ global) — the shape verified against transformers' processor.
        let img = RgbImage::new(800, 300);
        let (tensor, rows, cols) = preprocess_idefics3(&img);
        assert_eq!((rows, cols), (2, 4));
        assert_eq!(tensor.len(), 9 * 3 * 512 * 512);
    }

    #[test]
    fn image_prompt_layout() {
        let p = image_prompt(1, 2);
        assert!(p.starts_with("<fake_token_around_image><row_1_col_1><image>"));
        assert!(p.contains("<row_1_col_2>"));
        let tail = "<fake_token_around_image><global-img>".to_owned()
            + &"<image>".repeat(64)
            + "<fake_token_around_image>";
        assert!(p.ends_with(&tail));
        assert_eq!(p.matches("<image>").count(), 3 * 64);
    }
}
