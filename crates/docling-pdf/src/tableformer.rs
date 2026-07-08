//! TableFormer: table-structure recovery via docling-ibm-models, exported to
//! ONNX by `scripts/export_tableformer.py`. The image encoder + tag-transformer
//! encoder run once to a memory tensor; the decoder is then stepped
//! autoregressively to emit an OTSL structure-token sequence (the same model
//! docling runs). See PDF_CONFORMANCE.md.

use crate::pdfium_backend::TextCell;
use image::RgbImage;
use ort::session::Session;
use ort::value::{DynValue, Tensor};

const SIDE: u32 = 448;
// Verbatim from docling's tm_config.json image_normalization (more digits than
// f32 holds; kept exact for provenance).
#[allow(clippy::excessive_precision)]
const MEAN: [f32; 3] = [0.94247851, 0.94254675, 0.94292611];
#[allow(clippy::excessive_precision)]
const STD: [f32; 3] = [0.17910956, 0.17940403, 0.17931663];
const MAX_STEPS: usize = 1024;
/// Decoder geometry, fixed by the exported TableModel04_rs graph: the cached
/// decoder threads a `[N_LAYERS, past, 1, EMBED_DIM]` per-layer state cache.
const N_LAYERS: usize = 6;
const EMBED_DIM: usize = 512;

/// OTSL structure tokens (TableModel04_rs wordmap indices).
pub const START: i64 = 2;
pub const END: i64 = 3;
pub const ECEL: i64 = 4; // empty cell
pub const FCEL: i64 = 5; // full (content) cell
pub const LCEL: i64 = 6; // left-looking: extends the cell to its left (colspan)
pub const UCEL: i64 = 7; // up-looking: extends the cell above (rowspan)
pub const XCEL: i64 = 8; // cross: spans both ways
pub const NL: i64 = 9; // new row
pub const CHED: i64 = 10; // column header
pub const RHED: i64 = 11; // row header
pub const SROW: i64 = 12; // section row

/// A predicted table cell: an OTSL grid position (with spans) + its box in the
/// 448 image normalized cxcywh, and the OTSL tag.
#[derive(Debug, Clone)]
pub struct TableCell {
    pub row: usize,
    pub col: usize,
    pub colspan: usize,
    pub rowspan: usize,
    pub tag: i64,
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
}

pub struct TableFormer {
    encoder: Session,
    decoder: Session,
    bbox: Session,
    /// True when the decoder is the true-KV-cache export (`decoder_kv.onnx`:
    /// inputs `tag`/`cache_k`/`cache_v`, one token per step); false for the
    /// legacy layer-output-cache graph (`decoder.onnx`: full `tags` + `cache`).
    /// Detected from the session's input names, so an explicit
    /// `DOCLING_TABLEFORMER_DECODER` override works with either graph.
    kv: bool,
}

/// KV-cache geometry fixed by the `decoder_kv.onnx` export
/// (`[N_LAYERS, 1, KV_HEADS, past, KV_HEAD_DIM]`, `KV_HEADS × KV_HEAD_DIM = EMBED_DIM`).
const KV_HEADS: usize = 8;
const KV_HEAD_DIM: usize = 64;

/// The autoregressive decode state: `a` is the legacy layer-output cache, or
/// `cache_k` for the KV graph; `b` is `cache_v` (KV graph only). `None` = first
/// step (the zero-`past` empties are allocated per table by [`TableFormer::empty_cache`]).
#[derive(Default)]
struct DecodeCache {
    a: Option<DynValue>,
    b: Option<DynValue>,
}

/// Zero-`past` first-step cache tensors: `(cache, None)` for the legacy graph,
/// `(cache_k, Some(cache_v))` for the KV graph.
type EmptyCache = (Tensor<f32>, Option<Tensor<f32>>);

/// Encoder outputs that drive the cached decode loop: the per-layer cross-attention
/// K/V (projected from the image memory once, constant across decode steps) and
/// `enc_out` for the bbox decoder. Kept as owned `ort` values so each decode step
/// (and the bbox run) borrows them directly — no per-step extract/copy/re-wrap.
struct EncodeOut {
    ck: DynValue,
    cv: DynValue,
    eo: DynValue,
}

impl TableFormer {
    /// Load the exported encoder/decoder/bbox ONNX graphs (env overrides, else
    /// `models/tableformer/{encoder,decoder,bbox}.onnx`). Returns `None` if any is
    /// absent, so the pipeline falls back to geometric reconstruction.
    pub fn load() -> Option<Self> {
        Self::load_with(crate::intra_threads())
    }

    /// Like [`load`](Self::load) but with an explicit intra-op thread count, so a
    /// parallel page-worker pool can run each table model on fewer threads (the
    /// throughput comes from running pages concurrently, not from one fat model).
    pub fn load_with(intra: usize) -> Option<Self> {
        let enc = std::env::var("DOCLING_TABLEFORMER_ENCODER")
            .unwrap_or_else(|_| crate::resolve_asset("models/tableformer/encoder.onnx"));
        // Decoder preference (explicit override wins): INT8 variants first
        // unless DOCLING_RS_FP32 opts out; within a precision the true-KV-cache
        // export (`decoder_kv*`, one token per step) ranks behind the legacy
        // layer-output-cache graph it matches byte-for-byte — measured parity on
        // corpus-sized tables (ORT batches the legacy graph's prefix
        // re-projection efficiently), so the smaller legacy file stays the
        // default and `decoder_kv*` serves very-large-table workloads, where its
        // O(past) step cost wins.
        let dec = std::env::var("DOCLING_TABLEFORMER_DECODER").unwrap_or_else(|_| {
            let candidates: &[&str] = if crate::fp32_forced() {
                &[
                    "models/tableformer/decoder.onnx",
                    "models/tableformer/decoder_kv.onnx",
                ]
            } else {
                &[
                    "models/tableformer/decoder_int8.onnx",
                    "models/tableformer/decoder_kv_int8.onnx",
                    "models/tableformer/decoder.onnx",
                    "models/tableformer/decoder_kv.onnx",
                ]
            };
            candidates
                .iter()
                .map(|p| crate::resolve_asset(p))
                .find(|p| std::path::Path::new(p).exists())
                .unwrap_or_else(|| "models/tableformer/decoder.onnx".to_string())
        });
        let bbx = std::env::var("DOCLING_TABLEFORMER_BBOX")
            .unwrap_or_else(|_| crate::resolve_asset("models/tableformer/bbox.onnx"));
        if [&enc, &dec, &bbx]
            .iter()
            .any(|p| !std::path::Path::new(p).exists())
        {
            // The geometric fallback is a supported, intentional configuration
            // (docling has no ML table-structure equivalent baked in either), so
            // this stays a single quiet stderr note rather than an error — but it
            // fires every process (not per-worker) so a CWD-relative default that
            // silently misses its files (a very easy mistake for anything not run
            // from the repo root, e.g. an embedding app) is at least visible once.
            warn_missing_once(&enc, &dec, &bbx);
            return None;
        }
        // The decoder's KV-cache grows by one entry every autoregressive step, so
        // its input shapes differ on every `run()` call. ONNX Runtime's memory
        // pattern optimizer assumes stable shapes to plan buffer reuse; disabling
        // it for this session avoids repeatedly re-validating/re-touching that
        // plan (and the external-weights file) on each step.
        let build = |path: &str, mem_pattern: bool| -> Result<Session, String> {
            Session::builder()
                .map_err(|e| e.to_string())?
                .with_intra_threads(intra)
                .map_err(|e| e.to_string())?
                .with_memory_pattern(mem_pattern)
                .map_err(|e| e.to_string())?
                .commit_from_file(path)
                .map_err(|e| format!("tableformer load {path}: {e}"))
        };
        match (build(&enc, true), build(&dec, false), build(&bbx, true)) {
            (Ok(encoder), Ok(decoder), Ok(bbox)) => {
                let kv = decoder.inputs().iter().any(|i| i.name() == "cache_k");
                Some(Self {
                    encoder,
                    decoder,
                    bbox,
                    kv,
                })
            }
            _ => None,
        }
    }

    /// Run the image encoder and capture what the cached decoder loop needs: each
    /// decoder layer's cross-attention K/V (projected from the image memory once,
    /// shape `[N_LAYERS,1,H,S,head_dim]`) and `enc_out` for the bbox decoder.
    fn encode(&mut self, img: &RgbImage) -> Result<EncodeOut, String> {
        let input = preprocess(img)?;
        let mut enc_out = self
            .encoder
            .run(ort::inputs!["image" => input])
            .map_err(|e| format!("tableformer: encode: {e}"))?;
        let mut grab = |name: &str| -> Result<DynValue, String> {
            enc_out
                .remove(name)
                .ok_or_else(|| format!("tableformer: encoder output {name} missing"))
        };
        Ok(EncodeOut {
            ck: grab("cross_k")?,
            cv: grab("cross_v")?,
            eo: grab("enc_out")?,
        })
    }

    /// One doubly-cached decode step: feed the current `tags`, the constant cross
    /// K/V, and the growing self-attention `cache`; return the raw argmax tag and
    /// the last token's hidden state, advancing the cache. The cache stays an owned
    /// `ort` value — the previous step's `out_cache` output is fed back directly,
    /// never extracted or copied (it grows every step, so per-step copies were
    /// O(steps²) float traffic). `empty_cache` is the zero-`past` value used on the
    /// first step (ort's array constructors reject a 0-length dim, so it is
    /// allocated through the session allocator by the caller).
    fn decode_step(
        &mut self,
        tags: &[i64],
        enc: &EncodeOut,
        cache: &mut DecodeCache,
        empty: &EmptyCache,
    ) -> Result<(i64, Vec<f32>), String> {
        let mut dout = if self.kv {
            // KV graph: feed only the newly emitted tag; the projected K/V for
            // the whole prefix live in cache_k/cache_v and are fed back as-is.
            let last = *tags.last().expect("decode starts from <start>");
            let tag_t = Tensor::from_array(([1usize, 1usize], vec![last]))
                .map_err(|e| format!("tableformer: tag: {e}"))?;
            match (cache.a.as_ref(), cache.b.as_ref()) {
                (Some(k), Some(v)) => self.decoder.run(ort::inputs![
                    "tag" => tag_t, "cross_k" => &enc.ck, "cross_v" => &enc.cv,
                    "cache_k" => k, "cache_v" => v]),
                _ => self.decoder.run(ort::inputs![
                    "tag" => tag_t, "cross_k" => &enc.ck, "cross_v" => &enc.cv,
                    "cache_k" => &empty.0,
                    "cache_v" => empty.1.as_ref().expect("kv empty cache has both halves")]),
            }
        } else {
            let tags_t = Tensor::from_array(([tags.len(), 1usize], tags.to_vec()))
                .map_err(|e| format!("tableformer: tags: {e}"))?;
            match cache.a.as_ref() {
                None => self.decoder.run(ort::inputs![
                    "tags" => tags_t, "cross_k" => &enc.ck, "cross_v" => &enc.cv,
                    "cache" => &empty.0]),
                Some(c) => self.decoder.run(ort::inputs![
                    "tags" => tags_t, "cross_k" => &enc.ck, "cross_v" => &enc.cv,
                    "cache" => c]),
            }
        }
        .map_err(|e| format!("tableformer: decode: {e}"))?;
        let (_, logits) = dout["logits"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: logits: {e}"))?;
        let raw = argmax(logits) as i64;
        let (_, hidden) = dout["hidden"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: hidden: {e}"))?;
        let hidden = hidden.to_vec();
        if self.kv {
            cache.a = Some(
                dout.remove("out_cache_k")
                    .ok_or_else(|| "tableformer: out_cache_k missing".to_string())?,
            );
            cache.b = Some(
                dout.remove("out_cache_v")
                    .ok_or_else(|| "tableformer: out_cache_v missing".to_string())?,
            );
        } else {
            cache.a = Some(
                dout.remove("out_cache")
                    .ok_or_else(|| "tableformer: decoder output out_cache missing".to_string())?,
            );
        }
        Ok((raw, hidden))
    }

    /// The zero-`past` first-step cache(s), allocated through the session
    /// allocator (ort's array constructors reject a 0-length dim; the C API does
    /// allow it).
    fn empty_cache(&self) -> Result<EmptyCache, String> {
        let alloc = self.decoder.allocator();
        if self.kv {
            let mk = || {
                Tensor::<f32>::new(alloc, [N_LAYERS, 1, KV_HEADS, 0usize, KV_HEAD_DIM])
                    .map_err(|e| format!("tableformer: empty kv cache: {e}"))
            };
            Ok((mk()?, Some(mk()?)))
        } else {
            let c = Tensor::<f32>::new(alloc, [N_LAYERS, 0usize, 1, EMBED_DIM])
                .map_err(|e| format!("tableformer: empty cache: {e}"))?;
            Ok((c, None))
        }
    }

    /// Predict the OTSL structure-token sequence for a table-region image.
    pub fn predict_otsl(&mut self, img: &RgbImage) -> Result<Vec<i64>, String> {
        let enc = self.encode(img)?;
        // The two structure corrections mirror docling's `predict` exactly — note
        // its `line_num` is never incremented, so `xcel→lcel` applies on every row.
        let mut tags: Vec<i64> = vec![START];
        let mut out: Vec<i64> = Vec::new();
        let mut prev_ucel = false;
        let mut cache = DecodeCache::default();
        let empty = self.empty_cache()?;
        while out.len() < MAX_STEPS {
            let (raw, _hidden) = self.decode_step(&tags, &enc, &mut cache, &empty)?;
            let mut tag = raw;
            if tag == XCEL {
                tag = LCEL;
            }
            if prev_ucel && tag == LCEL {
                tag = FCEL;
            }
            if tag == END {
                break;
            }
            out.push(tag);
            tags.push(tag);
            prev_ucel = tag == UCEL;
        }
        Ok(out)
    }

    /// Full structure prediction: OTSL grid cells with per-cell boxes (in the 448
    /// image, normalized cxcywh). Collects per-cell decoder hidden states using
    /// docling's exact bbox bookkeeping (skip-after-row-break, first-lcel of a
    /// horizontal span), runs the bbox decoder, merges span boxes, then lays the
    /// cells onto the OTSL grid with row/col spans.
    pub fn predict_table_structure(&mut self, img: &RgbImage) -> Result<Vec<TableCell>, String> {
        let enc = self.encode(img)?;

        let mut tags: Vec<i64> = vec![START];
        let mut otsl: Vec<i64> = Vec::new();
        let mut hiddens: Vec<f32> = Vec::new(); // flattened [n, 512]
        let mut n = 0usize;
        let mut prev_ucel = false;
        let mut skip = true; // first tag after <start> is skipped
        let mut first_lcel = true;
        let mut bbox_ind = 0usize;
        let mut cur_bbox_ind = 0usize;
        let mut merge: std::collections::HashMap<usize, i64> = std::collections::HashMap::new();
        let mut cache = DecodeCache::default();
        let empty = self.empty_cache()?;
        while otsl.len() < MAX_STEPS {
            let (raw, hidden) = self.decode_step(&tags, &enc, &mut cache, &empty)?;
            let mut tag = raw;
            if tag == XCEL {
                tag = LCEL;
            }
            if prev_ucel && tag == LCEL {
                tag = FCEL;
            }
            if tag == END {
                break;
            }
            // docling's tag_H_buf / bboxes_to_merge bookkeeping.
            if !skip && matches!(tag, FCEL | ECEL | CHED | RHED | SROW | NL | UCEL) {
                hiddens.extend_from_slice(&hidden);
                n += 1;
                if !first_lcel {
                    merge.insert(cur_bbox_ind, bbox_ind as i64);
                }
                bbox_ind += 1;
            }
            if tag != LCEL {
                first_lcel = true;
            } else if first_lcel {
                hiddens.extend_from_slice(&hidden);
                n += 1;
                first_lcel = false;
                cur_bbox_ind = bbox_ind;
                merge.insert(cur_bbox_ind, -1);
                bbox_ind += 1;
            }
            skip = matches!(tag, NL | UCEL | XCEL);
            prev_ucel = tag == UCEL;
            otsl.push(tag);
            tags.push(tag);
        }
        if n == 0 {
            return Ok(Vec::new());
        }
        let tag_h = Tensor::from_array(([n, 512usize], hiddens))
            .map_err(|e| format!("tableformer: tag_h: {e}"))?;
        let bout = self
            .bbox
            .run(ort::inputs!["enc_out" => &enc.eo, "tag_h" => tag_h])
            .map_err(|e| format!("tableformer: bbox: {e}"))?;
        let (_, raw) = bout["boxes"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: boxes: {e}"))?;
        let boxes: Vec<[f32; 4]> = raw
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();
        let merged = merge_spans(&boxes, &merge);
        Ok(build_table_cells(&otsl, &merged))
    }

    /// Predict a table region's Markdown grid: crop the region (docling's
    /// page→1024px box-average then bbox crop), run the structure model, map each
    /// cell box back to page points, match the page's word cells into cells by
    /// intersection-over-word-area, and expand spans into a dense `rows × cols`
    /// grid. `region` is `(l, t, r, b)` in page points (top-left). Returns `None`
    /// if no structure is predicted.
    pub fn predict_table_rows(
        &mut self,
        page_image: &RgbImage,
        page_h: f32,
        region: [f32; 4],
        words: &[TextCell],
    ) -> Option<Vec<Vec<String>>> {
        // page → 1024px height (cv2.INTER_AREA), then crop the table bbox.
        let sf = 1024.0 / page_image.height() as f32;
        let pw = (page_image.width() as f32 * sf) as u32;
        let page1024 = crate::timing::timed("tableformer.inter_area", || {
            crate::resample::inter_area(page_image, pw, 1024)
        });
        let k = 1024.0 / page_h;
        let x = (region[0] * k).round().max(0.0) as u32;
        let y = (region[1] * k).round().max(0.0) as u32;
        let x2 = ((region[2] * k).round() as u32).min(page1024.width());
        let y2 = ((region[3] * k).round() as u32).min(page1024.height());
        if x2 <= x || y2 <= y {
            return None;
        }
        let crop = image::imageops::crop_imm(&page1024, x, y, x2 - x, y2 - y).to_image();
        let cells = crate::timing::timed("tableformer.structure", || {
            self.predict_table_structure(&crop)
        })
        .ok()?;
        if cells.is_empty() {
            return None;
        }
        let (rw, rh) = (region[2] - region[0], region[3] - region[1]);

        // Cell boxes in page points (top-left), aligned with `cells`.
        let boxes: Vec<[f32; 4]> = cells
            .iter()
            .map(|c| {
                [
                    region[0] + (c.cx - c.w / 2.0) * rw,
                    region[1] + (c.cy - c.h / 2.0) * rh,
                    region[0] + (c.cx + c.w / 2.0) * rw,
                    region[1] + (c.cy + c.h / 2.0) * rh,
                ]
            })
            .collect();

        // Assign each word to the cell it overlaps most (intersection / word area).
        let mut cell_words: Vec<Vec<usize>> = vec![Vec::new(); cells.len()];
        for (wi, w) in words.iter().enumerate() {
            let wa = ((w.r - w.l) * (w.b - w.t)).max(1.0);
            let mut best: Option<(f32, usize)> = None;
            for (ci, b) in boxes.iter().enumerate() {
                let ix = (w.r.min(b[2]) - w.l.max(b[0])).max(0.0);
                let iy = (w.b.min(b[3]) - w.t.max(b[1])).max(0.0);
                let io = ix * iy / wa;
                if io > 0.0 && best.is_none_or(|(bo, _)| io > bo) {
                    best = Some((io, ci));
                }
            }
            if let Some((_, ci)) = best {
                cell_words[ci].push(wi);
            }
        }

        let num_rows = cells.iter().map(|c| c.row + c.rowspan).max().unwrap_or(0);
        let num_cols = cells.iter().map(|c| c.col + c.colspan).max().unwrap_or(0);
        if num_rows == 0 || num_cols == 0 {
            return None;
        }
        let mut grid = vec![vec![String::new(); num_cols]; num_rows];
        for (ci, c) in cells.iter().enumerate() {
            // Keep words in text-stream order (the order they were collected =
            // their word index), matching docling's cell text assembly — geometric
            // re-sorting scrambles wrapped cells (`Inference time (secs)`).
            let wis = std::mem::take(&mut cell_words[ci]);
            let text = wis
                .iter()
                .map(|&i| words[i].text.trim())
                .collect::<Vec<_>>()
                .join(" ");
            // Spanned cells repeat their text across the covered grid positions.
            for row in grid.iter_mut().skip(c.row).take(c.rowspan) {
                for cell in row.iter_mut().skip(c.col).take(c.colspan) {
                    *cell = text.clone();
                }
            }
        }
        Some(grid)
    }
}

/// Note once per process that TableFormer's ONNX graphs weren't found, so tables
/// fall back to geometric reconstruction. The default paths are relative
/// (`models/tableformer/*.onnx`), which only resolves when the process's current
/// directory happens to be the repo root — a very easy miss for anything else
/// (an embedding app, a binding invoked from a different working directory, …),
/// and previously failed with no signal at all.
fn warn_missing_once(enc: &str, dec: &str, bbx: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "docling.rs: TableFormer models not found (checked {enc}, {dec}, {bbx}); \
             tables will use geometric reconstruction instead of ML table-structure \
             recognition. Set DOCLING_TABLEFORMER_ENCODER / DOCLING_TABLEFORMER_DECODER \
             / DOCLING_TABLEFORMER_BBOX to enable it (see README.md)."
        );
    });
}

/// docling's preprocessing: bilinear (cv2.INTER_LINEAR) resize the crop to 448²,
/// normalize `(x/255 − mean)/std`, laid out as (C, W, H) — docling transposes
/// (2,1,0), so width is the major spatial axis. The page→1024px box-average
/// (cv2.INTER_AREA) is the caller's job.
fn preprocess(img: &RgbImage) -> Result<Tensor<f32>, String> {
    let nn = (SIDE * SIDE) as usize;
    let side = SIDE as usize;
    let (sw, sh) = (img.width() as i32, img.height() as i32);
    let sxr = sw as f32 / SIDE as f32;
    let syr = sh as f32 / SIDE as f32;
    let mut data = vec![0f32; 3 * nn];
    for h in 0..side {
        let fy = (h as f32 + 0.5) * syr - 0.5;
        let wy = fy - fy.floor();
        let y0c = (fy.floor() as i32).clamp(0, sh - 1) as u32;
        let y1c = (fy.floor() as i32 + 1).clamp(0, sh - 1) as u32;
        for w in 0..side {
            let fx = (w as f32 + 0.5) * sxr - 0.5;
            let wx = fx - fx.floor();
            let x0c = (fx.floor() as i32).clamp(0, sw - 1) as u32;
            let x1c = (fx.floor() as i32 + 1).clamp(0, sw - 1) as u32;
            let p00 = img.get_pixel(x0c, y0c);
            let p01 = img.get_pixel(x1c, y0c);
            let p10 = img.get_pixel(x0c, y1c);
            let p11 = img.get_pixel(x1c, y1c);
            let idx = w * side + h; // (C, W, H): c*n + w*H + h
            for c in 0..3 {
                let top = p00[c] as f32 * (1.0 - wx) + p01[c] as f32 * wx;
                let bot = p10[c] as f32 * (1.0 - wx) + p11[c] as f32 * wx;
                let v = top * (1.0 - wy) + bot * wy;
                data[c * nn + idx] = (v / 255.0 - MEAN[c]) / STD[c];
            }
        }
    }
    Tensor::from_array(([1usize, 3, side, side], data))
        .map_err(|e| format!("tableformer: input: {e}"))
}

/// docling's `mergebboxes` (cxcywh): the union box of a horizontal span's first
/// and last cell.
fn mergebboxes(b1: [f32; 4], b2: [f32; 4]) -> [f32; 4] {
    let new_w = (b2[0] + b2[2] / 2.0) - (b1[0] - b1[2] / 2.0);
    let new_h = (b2[1] + b2[3] / 2.0) - (b1[1] - b1[3] / 2.0);
    let new_left = b1[0] - b1[2] / 2.0;
    let new_top = (b2[1] - b2[3] / 2.0).min(b1[1] - b1[3] / 2.0);
    [new_left + new_w / 2.0, new_top + new_h / 2.0, new_w, new_h]
}

/// Apply docling's span merges: each merge key combines its box with the partner
/// (`-1` → the last box); partners are dropped.
fn merge_spans(boxes: &[[f32; 4]], merge: &std::collections::HashMap<usize, i64>) -> Vec<[f32; 4]> {
    let skip: std::collections::HashSet<usize> = merge
        .values()
        .filter(|&&v| v >= 0)
        .map(|&v| v as usize)
        .collect();
    let mut out = Vec::new();
    for (i, &b) in boxes.iter().enumerate() {
        if let Some(&j) = merge.get(&i) {
            let partner = if j < 0 { boxes.len() - 1 } else { j as usize };
            out.push(mergebboxes(b, boxes[partner.min(boxes.len() - 1)]));
        } else if !skip.contains(&i) {
            out.push(b);
        }
    }
    out
}

const CELL_TAGS: [i64; 6] = [FCEL, ECEL, XCEL, CHED, RHED, SROW];

/// Lay the OTSL tag stream onto a grid (docling's `_build_table_cells`, OTSL
/// mode): cell tags create cells at (row, col); `lcel`/`ucel`/`xcel` are spans
/// (counted toward the column index but not cells). Colspan/rowspan are read off
/// the grid (consecutive `lcel`/`ucel` to the right/below). `boxes` are indexed
/// by cell order and aligned with the cells.
fn build_table_cells(otsl: &[i64], boxes: &[[f32; 4]]) -> Vec<TableCell> {
    // 2D grid of tags (rows split on NL) for span lookups.
    let mut grid: Vec<Vec<i64>> = vec![Vec::new()];
    for &t in otsl {
        if t == NL {
            grid.push(Vec::new());
        } else {
            grid.last_mut().unwrap().push(t);
        }
    }
    let mut cells = Vec::new();
    let mut cell_id = 0usize;
    for (r, row) in grid.iter().enumerate() {
        for (c, &tag) in row.iter().enumerate() {
            if !CELL_TAGS.contains(&tag) {
                continue;
            }
            let mut colspan = 1;
            while c + colspan < row.len() && matches!(row[c + colspan], LCEL | XCEL) {
                colspan += 1;
            }
            let mut rowspan = 1;
            while r + rowspan < grid.len()
                && grid[r + rowspan]
                    .get(c)
                    .is_some_and(|&t| matches!(t, UCEL | XCEL))
            {
                rowspan += 1;
            }
            let b = boxes.get(cell_id).copied().unwrap_or([0.0; 4]);
            cells.push(TableCell {
                row: r,
                col: c,
                colspan,
                rowspan,
                tag,
                cx: b[0],
                cy: b[1],
                w: b[2],
                h: b[3],
            });
            cell_id += 1;
        }
    }
    cells
}

fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .unwrap_or(0)
}
