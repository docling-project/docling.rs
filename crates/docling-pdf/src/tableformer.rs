//! TableFormer: table-structure recovery via docling-ibm-models, exported to
//! ONNX by `scripts/install/export_tableformer.py`. The image encoder + tag-transformer
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
/// 448 image normalized cxcywh, the OTSL tag, and the bbox decoder's cell
/// class (docling's `cell_class`; 2 = full, ≤1 = predicted empty).
#[derive(Debug, Clone)]
pub struct TableCell {
    pub row: usize,
    pub col: usize,
    pub colspan: usize,
    pub rowspan: usize,
    pub tag: i64,
    pub class: i64,
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
        // export (`decoder_kv*`, one token per step, O(past) step cost) ranks
        // ahead of the legacy layer-output-cache graph it matches byte-for-byte
        // (91/91 snapshot corpus exact with either). Re-measured warm on the
        // corpus fixtures: the KV graph is ~13% faster on ordinary tables
        // (2206.01062) and ~17% on the huge-table page (2305.03393v1-pg9),
        // for +36 MB on disk — table-heavy single-page PDFs are exactly where
        // the pipeline is tightest against Python docling, so speed wins the
        // default and the legacy file stays as the smaller fallback.
        let dec = std::env::var("DOCLING_TABLEFORMER_DECODER").unwrap_or_else(|_| {
            let candidates: &[&str] = if crate::fp32_forced() {
                &[
                    "models/tableformer/decoder_kv.onnx",
                    "models/tableformer/decoder.onnx",
                ]
            } else {
                &[
                    "models/tableformer/decoder_kv_int8.onnx",
                    "models/tableformer/decoder_int8.onnx",
                    "models/tableformer/decoder_kv.onnx",
                    "models/tableformer/decoder.onnx",
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
        if crate::timing::enabled() {
            eprintln!("docling-pdf: tableformer decoder: {dec}");
        }
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
        // Per-cell class logits [n, 3] → argmax (docling's `outputs_class`).
        let (_, craw) = bout["classes"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: classes: {e}"))?;
        let classes: Vec<i64> = craw.chunks_exact(3).map(|c| argmax(c) as i64).collect();
        let (merged, merged_classes) = merge_spans(&boxes, &classes, &merge);
        Ok(build_table_cells(&otsl, &merged, &merged_classes))
    }

    /// Predict a table region's Markdown grid: crop the region (docling's
    /// page→1024px box-average then bbox crop), run the structure model, then
    /// match the page's word cells into the predicted cells with docling's
    /// matching post-processor ([`crate::tf_match`]) and expand spans into a
    /// dense `rows × cols` grid. `region` is `(l, t, r, b)` in page points
    /// (top-left). Returns `None` if no structure is predicted.
    pub fn predict_table_rows(
        &mut self,
        page_image: &RgbImage,
        region: [f32; 4],
        words: &[TextCell],
    ) -> Option<Vec<Vec<String>>> {
        // page → 1024px height (cv2.INTER_AREA), then crop the table bbox.
        // docling's coordinate chain, rounding included: the cluster bbox is
        // rounded to integer page points *first* (`round(cluster.bbox.l) *
        // scale`, banker's rounding), scaled by 2 (its table-structure page
        // scale), then by `1024 / <2x page-image height>`, and the crop indices
        // round again. Rounding after scaling instead shifts some crops by a
        // pixel — enough to change TableFormer's cell boxes on tall tables
        // (redp5110's TOC).
        let sf = 1024.0 / page_image.height() as f32;
        let pw = (page_image.width() as f32 * sf) as u32;
        let page1024 = crate::timing::timed("tableformer.inter_area", || {
            crate::resample::inter_area(page_image, pw, 1024)
        });
        let k = 2.0 * 1024.0 / page_image.height() as f64;
        let px = |v: f32| (v as f64).round_ties_even() * k;
        let x = (px(region[0]).round_ties_even()).max(0.0) as u32;
        let y = (px(region[1]).round_ties_even()).max(0.0) as u32;
        let x2 = (px(region[2]).round_ties_even() as u32).min(page1024.width());
        let y2 = (px(region[3]).round_ties_even() as u32).min(page1024.height());
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
        // Words that belong to the table: non-empty text, ≥80 % of the word's
        // area inside the table region (docling's `get_cells_in_bbox` ios test).
        // Ids stay the page-level word indices so text joins in stream order.
        let table_words: Vec<crate::tf_match::PdfWord> = words
            .iter()
            .enumerate()
            .filter(|(_, w)| !w.text.trim().is_empty())
            .filter_map(|(wi, w)| {
                let (l, t, r, b) = (w.l as f64, w.t as f64, w.r as f64, w.b as f64);
                let area = (r - l) * (b - t);
                let iw = (r.min(region[2] as f64) - l.max(region[0] as f64)).max(0.0);
                let ih = (b.min(region[3] as f64) - t.max(region[1] as f64)).max(0.0);
                if area > 0.0 && iw * ih / area > 0.8 {
                    Some(crate::tf_match::PdfWord {
                        id: wi,
                        bbox: [l, t, r, b],
                        text: w.text.trim().to_string(),
                    })
                } else {
                    None
                }
            })
            .collect();

        if !table_words.is_empty() && !simple_match() {
            return docling_match_rows(&cells, region, &table_words, words);
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
            let text = normalize_cell_text(text);
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

/// Append one JSON line per table into `<dir>/tf_match_dump.jsonl` with the
/// exact matcher inputs (hand-rolled JSON to avoid a serde dependency).
fn dump_match_inputs(
    dir: &str,
    tf_cells: &[crate::tf_match::TfCell],
    words: &[crate::tf_match::PdfWord],
) {
    use std::io::Write;
    let cells: Vec<String> = tf_cells
        .iter()
        .map(|c| {
            format!(
                r#"{{"bbox":[{},{},{},{}],"cell_id":{},"row_id":{},"column_id":{},"cell_class":{},"colspan_val":{},"rowspan_val":{}}}"#,
                c.bbox[0], c.bbox[1], c.bbox[2], c.bbox[3],
                c.cell_id, c.row_id, c.column_id, c.cell_class,
                c.colspan_val, c.rowspan_val
            )
        })
        .collect();
    let ws: Vec<String> = words
        .iter()
        .map(|w| {
            format!(
                r#"{{"id":{},"bbox":[{},{},{},{}],"text":{}}}"#,
                w.id,
                w.bbox[0],
                w.bbox[1],
                w.bbox[2],
                w.bbox[3],
                serde_json_escape(&w.text)
            )
        })
        .collect();
    let line = format!(
        r#"{{"table_cells":[{}],"pdf_cells":[{}]}}"#,
        cells.join(","),
        ws.join(",")
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{dir}/tf_match_dump.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Minimal JSON string escaping for the parity dump.
fn serde_json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `DOCLING_RS_TF_SIMPLE_MATCH=1` reverts to the pre-#60 best-overlap word
/// assignment (A/B escape hatch for the docling matching post-processor).
fn simple_match() -> bool {
    std::env::var("DOCLING_RS_TF_SIMPLE_MATCH").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// docling glues `@` to whatever follows it (`mAP @0.5`, an email):
/// the PDF's word cells split `@` from the next token, and joining them
/// with a space would widen the cell and — via the column pad — shift
/// every row of the table. The groundtruth never contains "@ ", so this
/// is always the right normalization.
fn normalize_cell_text(text: String) -> String {
    text.replace("@ ", "@")
}

/// docling's matched-cell grid assembly (`tf_predictor.predict` with
/// `do_cell_matching=True`): run the ported matching post-processor, group the
/// word→cell assignments per grid position, compress the surviving row/column
/// ids to sequential indexes, and expand spans into a dense `rows × cols` text
/// grid. Matching runs in docling's coordinate space — the table bbox rounded
/// to integers, everything ×2 (its page scale) — so the post-processor's
/// absolute rounding agrees.
fn docling_match_rows(
    cells: &[TableCell],
    region: [f32; 4],
    table_words: &[crate::tf_match::PdfWord],
    words: &[TextCell],
) -> Option<Vec<Vec<String>>> {
    const SCALE: f64 = 2.0; // docling's table-structure page scale
    let sl = (region[0] as f64).round_ties_even() * SCALE;
    let st = (region[1] as f64).round_ties_even() * SCALE;
    let sr = (region[2] as f64).round_ties_even() * SCALE;
    let sb = (region[3] as f64).round_ties_even() * SCALE;
    let (w2, h2) = (sr - sl, sb - st);

    let tf_cells: Vec<crate::tf_match::TfCell> = cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (cx, cy) = (c.cx as f64, c.cy as f64);
            let (w, h) = (c.w as f64, c.h as f64);
            crate::tf_match::TfCell {
                bbox: [
                    sl + (cx - w / 2.0) * w2,
                    st + (cy - h / 2.0) * h2,
                    sl + (cx + w / 2.0) * w2,
                    st + (cy + h / 2.0) * h2,
                ],
                cell_id: i,
                row_id: c.row,
                column_id: c.col,
                cell_class: c.class,
                colspan_val: if c.colspan > 1 { c.colspan } else { 0 },
                rowspan_val: if c.rowspan > 1 { c.rowspan } else { 0 },
            }
        })
        .collect();

    let scaled_words: Vec<crate::tf_match::PdfWord> = table_words
        .iter()
        .map(|w| crate::tf_match::PdfWord {
            id: w.id,
            bbox: [
                w.bbox[0] * SCALE,
                w.bbox[1] * SCALE,
                w.bbox[2] * SCALE,
                w.bbox[3] * SCALE,
            ],
            text: w.text.clone(),
        })
        .collect();

    // Debug: dump the matcher inputs as JSON lines for a side-by-side run
    // against docling's Python post-processor (parity harness, not a feature).
    if let Ok(dir) = std::env::var("DOCLING_RS_TF_MATCH_DUMP") {
        if !dir.is_empty() {
            dump_match_inputs(&dir, &tf_cells, &scaled_words);
        }
    }

    let (cells_wo, final_matches) =
        crate::tf_match::match_and_post_process(tf_cells, &scaled_words);

    // `_merge_tf_output`: group per (column, row) in ascending-pdf-id order;
    // the first word's table cell fixes the group's offsets and spans.
    struct Merged {
        start_row: usize,
        start_col: usize,
        row_span: usize,
        col_span: usize,
        word_ids: Vec<usize>,
    }
    let mut merged: Vec<Merged> = Vec::new();
    let mut key_ix: std::collections::HashMap<(usize, usize), usize> =
        std::collections::HashMap::new();
    for (&pdf_id, list) in &final_matches {
        let tm = list[0].table_cell_id;
        let Some(cell) = cells_wo.iter().find(|c| c.cell_id == tm) else {
            continue;
        };
        match key_ix.entry((cell.column_id, cell.row_id)) {
            std::collections::hash_map::Entry::Occupied(e) => {
                merged[*e.get()].word_ids.push(pdf_id);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(merged.len());
                merged.push(Merged {
                    start_row: cell.row_id,
                    start_col: cell.column_id,
                    row_span: cell.rowspan_val.max(1),
                    col_span: cell.colspan_val.max(1),
                    word_ids: vec![pdf_id],
                });
            }
        }
    }
    if merged.is_empty() {
        return None;
    }

    // `multi_table_predict`'s sort_row_col_indexes: compress the surviving
    // row/column ids to gap-free indexes.
    let mut start_cols: Vec<usize> = merged.iter().map(|m| m.start_col).collect();
    start_cols.sort_unstable();
    start_cols.dedup();
    let mut start_rows: Vec<usize> = merged.iter().map(|m| m.start_row).collect();
    start_rows.sort_unstable();
    start_rows.dedup();
    let mut num_rows = 0;
    let mut num_cols = 0;
    for m in &mut merged {
        m.start_col = start_cols.binary_search(&m.start_col).expect("own value");
        m.start_row = start_rows.binary_search(&m.start_row).expect("own value");
        num_cols = num_cols.max(m.start_col + m.col_span);
        num_rows = num_rows.max(m.start_row + m.row_span);
    }
    if num_rows == 0 || num_cols == 0 {
        return None;
    }

    let mut grid = vec![vec![String::new(); num_cols]; num_rows];
    for m in &merged {
        let text = m
            .word_ids
            .iter()
            .map(|&i| words[i].text.trim())
            .collect::<Vec<_>>()
            .join(" ");
        let text = normalize_cell_text(text);
        for row in grid.iter_mut().skip(m.start_row).take(m.row_span) {
            for cell in row.iter_mut().skip(m.start_col).take(m.col_span) {
                *cell = text.clone();
            }
        }
    }
    Some(grid)
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
/// (`-1` → the last box); partners are dropped. The merged cell keeps the
/// *first* box's class, matching docling's `outputs_class1.append(cls1)`.
fn merge_spans(
    boxes: &[[f32; 4]],
    classes: &[i64],
    merge: &std::collections::HashMap<usize, i64>,
) -> (Vec<[f32; 4]>, Vec<i64>) {
    let skip: std::collections::HashSet<usize> = merge
        .values()
        .filter(|&&v| v >= 0)
        .map(|&v| v as usize)
        .collect();
    let mut out = Vec::new();
    let mut out_classes = Vec::new();
    for (i, &b) in boxes.iter().enumerate() {
        let class = classes.get(i).copied().unwrap_or(2);
        if let Some(&j) = merge.get(&i) {
            let partner = if j < 0 { boxes.len() - 1 } else { j as usize };
            out.push(mergebboxes(b, boxes[partner.min(boxes.len() - 1)]));
            out_classes.push(class);
        } else if !skip.contains(&i) {
            out.push(b);
            out_classes.push(class);
        }
    }
    (out, out_classes)
}

const CELL_TAGS: [i64; 6] = [FCEL, ECEL, XCEL, CHED, RHED, SROW];

/// Lay the OTSL tag stream onto a grid (docling's `_build_table_cells`, OTSL
/// mode): cell tags create cells at (row, col); `lcel`/`ucel`/`xcel` are spans
/// (counted toward the column index but not cells). Colspan/rowspan are read off
/// the grid (consecutive `lcel`/`ucel` to the right/below). `boxes` are indexed
/// by cell order and aligned with the cells.
fn build_table_cells(otsl: &[i64], boxes: &[[f32; 4]], classes: &[i64]) -> Vec<TableCell> {
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
            // docling defaults a class-less cell to 2 (full).
            let class = classes.get(cell_id).copied().unwrap_or(2);
            cells.push(TableCell {
                row: r,
                col: c,
                colspan,
                rowspan,
                tag,
                class,
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
