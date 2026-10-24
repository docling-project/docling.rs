//! TableFormer: table-structure recovery via docling-ibm-models, exported to
//! ONNX by `scripts/install/export_tableformer.py`. The image encoder + tag-transformer
//! encoder run once to a memory tensor; the decoder is then stepped
//! autoregressively to emit an OTSL structure-token sequence (the same model
//! docling runs). See docs/PDF_CONFORMANCE.md.

use crate::pdfium_backend::TextCell;
// The ONNX-free half (preprocessing, structure corrections, bbox bookkeeping,
// span merge, OTSL→grid) lives in tf_core so the browser build (#157 stage 3)
// runs the same logic; this file owns the three `ort` sessions and the
// owned-value KV-cache fast path.
use crate::tf_core::{
    argmax, build_table_cells, correct, merge_spans, preprocess_input, BboxBook, TableCell, END,
    MAX_ROW_TAGS, MAX_STEPS, START, UCEL,
};
use image::RgbImage;
use ort::session::Session;
use ort::value::{DynValue, Tensor, TensorRef};

const SIDE: usize = crate::tf_core::SIDE as usize;
const EMBED_DIM: usize = crate::tf_core::EMBED_DIM;
/// Decoder geometry, fixed by the exported TableModel04_rs graph: the cached
/// decoder threads a `[N_LAYERS, past, 1, EMBED_DIM]` per-layer state cache.
const N_LAYERS: usize = 6;

/// Resolve the encoder / decoder / bbox files exactly as [`TableFormer::load`]
/// will (shared with `model_inventory`, so diagnostics can never drift from
/// what actually loads). Explicit `DOCLING_TABLEFORMER_*` overrides win; the
/// decoder otherwise picks by preference — INT8 variants first unless
/// `DOCLING_RS_FP32` opts out, and within a precision the true-KV-cache
/// export (`decoder_kv*`, one token per step, O(past) step cost) ranks ahead
/// of the legacy layer-output-cache graph it matches byte-for-byte (91/91
/// snapshot corpus exact with either; the KV graph re-measured ~13–17% faster
/// warm, so speed wins the default and the legacy file stays as the smaller
/// fallback). `decoder_kv` ranks ABOVE `decoder_int8`: the #97 hoisted fp32
/// KV graph is faster than the quantized legacy graph on every machine
/// measured, and it is byte-exact (its own int8 variant is not produced — see
/// quantize_models.py). `decoder_kv_layers` ranks just above `decoder_kv`:
/// the same graph with per-layer caches (#871, [`DecoderStyle::KvLayered`]),
/// bit-identical and without per-step allocations that grow with the table.
pub fn resolved_paths() -> (String, String, String) {
    // The encoder ranks its fp16-weight repack (`encoder_fp16.onnx`, #374 —
    // the same graph with the weights stored as fp16 and cast back to fp32
    // at load, ~half the download, fp32 compute) ahead of the fp32 file
    // unless `DOCLING_RS_FP32` opts out; an explicit override wins.
    let enc = docling_core::env::nonempty("DOCLING_TABLEFORMER_ENCODER").unwrap_or_else(|| {
        let candidates: &[&str] = if crate::prefer_fp32() {
            &[".models/tableformer/encoder.onnx"]
        } else {
            &[
                ".models/tableformer/encoder_fp16.onnx",
                ".models/tableformer/encoder.onnx",
            ]
        };
        candidates
            .iter()
            .map(|p| crate::resolve_asset(p))
            .find(|p| std::path::Path::new(p).exists())
            .unwrap_or_else(|| crate::resolve_asset(".models/tableformer/encoder.onnx"))
    });
    let dec = docling_core::env::nonempty("DOCLING_TABLEFORMER_DECODER").unwrap_or_else(|| {
        let candidates: &[&str] = if crate::prefer_fp32() {
            &[
                ".models/tableformer/decoder_kv_layers.onnx",
                ".models/tableformer/decoder_kv.onnx",
                ".models/tableformer/decoder.onnx",
            ]
        } else {
            &[
                ".models/tableformer/decoder_kv_int8.onnx",
                ".models/tableformer/decoder_kv_layers.onnx",
                ".models/tableformer/decoder_kv.onnx",
                ".models/tableformer/decoder_int8.onnx",
                ".models/tableformer/decoder.onnx",
            ]
        };
        candidates
            .iter()
            .map(|p| crate::resolve_asset(p))
            .find(|p| std::path::Path::new(p).exists())
            .unwrap_or_else(|| ".models/tableformer/decoder.onnx".to_string())
    });
    let bbx = docling_core::env::nonempty("DOCLING_TABLEFORMER_BBOX")
        .unwrap_or_else(|| crate::resolve_asset(".models/tableformer/bbox.onnx"));
    (enc, dec, bbx)
}

pub struct TableFormer {
    encoder: Session,
    decoder: Session,
    bbox: Session,
    /// Which decoder graph flavour is loaded, detected from the session's
    /// input names (so an explicit `DOCLING_TABLEFORMER_DECODER` override
    /// works with any of them).
    style: DecoderStyle,
    /// The `KvHoisted` decoder's `tag` input has a symbolic batch axis (the
    /// dynamic-batch `decoder_kv.onnx` export): a page's tables decode
    /// together, one step for all of them — see [`Self::predict_tables_on`].
    /// The older fixed-`[1,1]` export decodes the tables one after another.
    batched: bool,
}

/// The three decoder-graph generations the loop supports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DecoderStyle {
    /// `decoder.onnx`: layer-output cache; feeds the full `tags` prefix and a
    /// single `cache` every step.
    Legacy,
    /// The pre-#97 `decoder_kv.onnx`: one tag per step, `cache_k`/`cache_v`,
    /// with the stacked `cross_k`/`cross_v` re-split inside every step.
    KvStacked,
    /// The #97 `decoder_kv.onnx`: one tag per step, and the constant cross
    /// tensors arrive as 2×`N_LAYERS` per-layer inputs (`cross_kt_i` already
    /// transposed for q·Kᵀ, `cross_v_i`), computed once per table by the
    /// encoder — the step graph does no work proportional to their size.
    KvHoisted,
    /// `decoder_kv_layers.onnx` (#871, `scripts/install/kv_layers.py`): the
    /// `KvHoisted` graph with the self-attention cache per layer — `Kᵀ` and
    /// `V` in and out for each layer instead of two stacked tensors — whose
    /// outputs land in buffers the loop owns ([`LayerCache`]).
    KvLayered,
}

impl DecoderStyle {
    /// One tag per step with the per-layer cross tensors the encoder emits.
    fn hoisted(self) -> bool {
        matches!(self, DecoderStyle::KvHoisted | DecoderStyle::KvLayered)
    }
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
    /// The `KvLayered` caches, set up on the first step.
    layers: Option<LayerCache>,
}

/// The `KvLayered` decoder's self-attention caches, in memory docling owns
/// (#871). With ONNX Runtime allocating them, every step of a table made a
/// cache one token longer than the last and freed the previous one —
/// hundreds of ever-larger blocks per table, which on linux/arm64 with the
/// CPU arena off fragmented glibc's heap until a 6 GiB container ran out of
/// memory. Here each step reads its caches from one of two buffers and ONNX
/// Runtime writes the one-token-longer caches straight into the other (the
/// outputs are preallocated over it, [`docling_onnx::borrowed_f32`]); the
/// next step swaps them. Each buffer holds the `2 × N_LAYERS` tensors in
/// regions of `cap` floats — per layer `Kᵀ` `[rows, H, hd, past]`, then `V`
/// `[rows, H, past, hd]` — sized for [`MAX_STEPS`] tokens: zeroed (`calloc`,
/// so mapped lazily), only the prefix a table reaches becomes resident, and
/// they are released when the table (or the page's batch) is done.
struct LayerCache {
    rows: usize,
    past: usize,
    cap: usize,
    bufs: [Vec<f32>; 2],
    /// Which of `bufs` holds the current caches.
    cur: usize,
    /// Zero-`past` first-step inputs (ort's slice constructors reject a
    /// 0-length dim, the session allocator does not).
    empty: Vec<Tensor<f32>>,
}

impl LayerCache {
    fn new(alloc: &ort::memory::Allocator, rows: usize) -> Result<Self, String> {
        let cap = rows * KV_HEADS * MAX_STEPS * KV_HEAD_DIM;
        let mut empty = Vec::with_capacity(2 * N_LAYERS);
        for j in 0..2 * N_LAYERS {
            empty.push(
                Tensor::<f32>::new(alloc, Self::shape(j, rows, 0))
                    .map_err(|e| format!("tableformer: empty kv cache: {e}"))?,
            );
        }
        Ok(Self {
            rows,
            past: 0,
            cap,
            bufs: [vec![0.0; 2 * N_LAYERS * cap], vec![0.0; 2 * N_LAYERS * cap]],
            cur: 0,
            empty,
        })
    }

    /// Tensor `j`'s shape at `past` tokens: `Kᵀ` for the first `N_LAYERS`.
    fn shape(j: usize, rows: usize, past: usize) -> [usize; 4] {
        if j < N_LAYERS {
            [rows, KV_HEADS, KV_HEAD_DIM, past]
        } else {
            [rows, KV_HEADS, past, KV_HEAD_DIM]
        }
    }

    fn input_name(j: usize) -> String {
        if j < N_LAYERS {
            format!("cache_kt_{j}")
        } else {
            format!("cache_v_{}", j - N_LAYERS)
        }
    }
}

/// Zero-`past` first-step cache tensors: `(cache, None)` for the legacy graph,
/// `(cache_k, Some(cache_v))` for the KV graph.
type EmptyCache = (Tensor<f32>, Option<Tensor<f32>>);

/// Encoder outputs that drive the cached decode loop: the per-layer cross-attention
/// K/V (projected from the image memory once, constant across decode steps) and
/// `enc_out` for the bbox decoder. Kept as owned `ort` values so each decode step
/// (and the bbox run) borrows them directly — no per-step extract/copy/re-wrap.
struct EncodeOut {
    /// Stacked `[N_LAYERS,1,H,S,hd]` cross K/V — the `Legacy`/`KvStacked`
    /// decoders' inputs. `None` for `KvHoisted`, which reads the per-layer
    /// tensors instead: the stacked pair is 2×9.6 MB per table, and a page's
    /// tables are now all held encoded at once for the batched loop.
    ck: Option<DynValue>,
    cv: Option<DynValue>,
    eo: DynValue,
    /// `KvHoisted` only: per-layer `[cross_kt_0..N, cross_v_0..N]`, index-aligned
    /// with the decoder's input names, borrowed by every decode step.
    per_layer: Vec<(String, DynValue)>,
}

impl TableFormer {
    /// Load the exported encoder/decoder/bbox ONNX graphs (env overrides, else
    /// `.models/tableformer/{encoder,decoder,bbox}.onnx`). Returns `None` if any is
    /// absent, so the pipeline falls back to geometric reconstruction.
    pub fn load() -> Option<Self> {
        Self::load_with(crate::intra_threads())
    }

    /// Like [`load`](Self::load) but with an explicit intra-op thread count, so a
    /// parallel page-worker pool can run each table model on fewer threads (the
    /// throughput comes from running pages concurrently, not from one fat model).
    ///
    /// See [`resolved_paths`] for the encoder/decoder/bbox file selection.
    pub fn load_with(intra: usize) -> Option<Self> {
        // (resolution shared with the model inventory — see resolved_paths)
        let (enc, dec, bbx) = resolved_paths();
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
        // plan (and the external-weights file) on each step. The bbox head has
        // the same problem one level up: its `tag_h` input is `[ncells, 512]`
        // and every table has a different cell count, so with the pattern
        // planner on each run re-plans — and on this graph the plan is *worse*
        // than none: 290 ms vs 54 ms for a 100-cell table, 560 vs 94 ms for
        // 200 cells (ORT 1.22, 4 threads). It was 0.26 s per table on the
        // corpus, more than the encoder.
        //
        // The decoder runs on ONE intra-op thread. A step is 49 small GEMMs
        // over a single token — it streams the layer weights, it does not
        // compute — so extra threads only add synchronisation: measured 4.1 ms
        // per step on 1 thread vs 5.5 on 4 (7.1 vs 4.9 once the cache is 100+
        // long). In the pool it also stops a table decode from taking all the
        // cores away from the other workers' layout inference. And a
        // single-thread session has a fixed reduction order, so table
        // structure no longer varies run-to-run on near-tie tokens the way
        // multi-threaded float sums let it (the conformance scripts pin one
        // thread for exactly that reason; the default now matches them). The
        // encoder keeps the shared budget: one 448×448 CNN + transformer pass
        // per table, 680 ms single-threaded vs 165 on four.
        let build = |path: &str, mem_pattern: bool, threads: usize| -> Result<Session, String> {
            let builder = docling_onnx::session_builder()?
                .with_intra_threads(threads)
                .map_err(|e| e.to_string())?
                .with_memory_pattern(mem_pattern)
                .map_err(|e| e.to_string())?;
            let variant = if mem_pattern {
                "mem_pattern"
            } else {
                "no_mem_pattern"
            };
            docling_onnx::commit(docling_onnx::apply(builder)?, path, variant)
                .map_err(|e| format!("tableformer load {path}: {e}"))
        };
        match (
            build(&enc, true, intra),
            build(&dec, false, 1),
            build(&bbx, false, intra),
        ) {
            (Ok(encoder), Ok(decoder), Ok(bbox)) => {
                let has = |n: &str| decoder.inputs().iter().any(|i| i.name() == n);
                let style = if has("cache_kt_0") {
                    DecoderStyle::KvLayered
                } else if has("cross_kt_0") {
                    DecoderStyle::KvHoisted
                } else if has("cache_k") {
                    DecoderStyle::KvStacked
                } else {
                    DecoderStyle::Legacy
                };
                if style.hoisted() && !encoder.outputs().iter().any(|o| o.name() == "cross_kt_0") {
                    eprintln!(
                        "docling-pdf: tableformer decoder needs per-layer cross tensors \
                         (cross_kt_*) the encoder doesn't emit — re-download or re-export \
                         the model set (scripts/install/export_tableformer.py); \
                         falling back to geometric tables"
                    );
                    return None;
                }
                // Dynamic batch axis on `tag` ⇒ the export batches decode
                // steps across tables (ort reports a symbolic dim as -1).
                let batched = style.hoisted()
                    && decoder.inputs().iter().any(|i| {
                        i.name() == "tag"
                            && matches!(i.dtype(), ort::value::ValueType::Tensor { shape, .. }
                                if shape.first().is_some_and(|d| *d < 0))
                    });
                if crate::timing::enabled() && batched {
                    eprintln!("docling-pdf: tableformer decoder batches a page's tables per step");
                }
                Some(Self {
                    encoder,
                    decoder,
                    bbox,
                    style,
                    batched,
                })
            }
            _ => None,
        }
    }

    /// Run the image encoder and capture what the cached decoder loop needs: each
    /// decoder layer's cross-attention K/V (projected from the image memory once,
    /// shape `[N_LAYERS,1,H,S,head_dim]`) and `enc_out` for the bbox decoder.
    fn encode(&mut self, img: &RgbImage) -> Result<EncodeOut, String> {
        let input = crate::timing::timed("tf.preprocess", || preprocess(img))?;
        let mut enc_out = crate::timing::timed("tf.encoder", || {
            docling_onnx::run(&mut self.encoder, ort::inputs!["image" => input])
                .map_err(|e| format!("tableformer: encode: {e}"))
        })?;
        let mut per_layer = Vec::new();
        if self.style.hoisted() {
            for prefix in ["cross_kt_", "cross_v_"] {
                for i in 0.. {
                    let name = format!("{prefix}{i}");
                    match enc_out.remove(&name) {
                        Some(v) => per_layer.push((name, v)),
                        None => break,
                    }
                }
            }
            if per_layer.is_empty() {
                return Err("tableformer: encoder emitted no cross_kt_* outputs".into());
            }
        }
        let mut grab = |name: &str| -> Result<DynValue, String> {
            enc_out
                .remove(name)
                .ok_or_else(|| format!("tableformer: encoder output {name} missing"))
        };
        let hoisted = self.style.hoisted();
        Ok(EncodeOut {
            ck: if hoisted {
                None
            } else {
                Some(grab("cross_k")?)
            },
            cv: if hoisted {
                None
            } else {
                Some(grab("cross_v")?)
            },
            eo: grab("enc_out")?,
            per_layer,
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
        crate::timing::timed("tf.decode_step", || {
            self.decode_step_inner(tags, enc, cache, empty)
        })
    }

    fn decode_step_inner(
        &mut self,
        tags: &[i64],
        enc: &EncodeOut,
        cache: &mut DecodeCache,
        empty: &EmptyCache,
    ) -> Result<(i64, Vec<f32>), String> {
        if self.style.hoisted() {
            // #97 graph: one tag; the constant per-layer cross tensors are
            // borrowed views — the step pays nothing proportional to them.
            let last = *tags.last().expect("decode starts from <start>");
            let (raws, hidden) = self.step_kv(&[last], &enc.per_layer, cache, empty)?;
            return Ok((raws[0], hidden));
        }
        let (ck, cv) = match (enc.ck.as_ref(), enc.cv.as_ref()) {
            (Some(k), Some(v)) => (k, v),
            _ => return Err("tableformer: stacked cross K/V missing".into()),
        };
        let mut dout = match self.style {
            DecoderStyle::KvHoisted | DecoderStyle::KvLayered => unreachable!("handled above"),
            DecoderStyle::KvStacked => {
                // Pre-#97 KV graph: feed only the newly emitted tag; the projected
                // K/V for the whole prefix live in cache_k/cache_v and are fed
                // back as-is.
                let last = *tags.last().expect("decode starts from <start>");
                let tag_t = Tensor::from_array(([1usize, 1usize], vec![last]))
                    .map_err(|e| format!("tableformer: tag: {e}"))?;
                match (cache.a.as_ref(), cache.b.as_ref()) {
                    (Some(k), Some(v)) => docling_onnx::run(
                        &mut self.decoder,
                        ort::inputs![
                        "tag" => tag_t, "cross_k" => ck, "cross_v" => cv,
                        "cache_k" => k, "cache_v" => v],
                    ),
                    _ => docling_onnx::run(
                        &mut self.decoder,
                        ort::inputs![
                        "tag" => tag_t, "cross_k" => ck, "cross_v" => cv,
                        "cache_k" => &empty.0,
                        "cache_v" => empty.1.as_ref().expect("kv empty cache has both halves")],
                    ),
                }
            }
            DecoderStyle::Legacy => {
                let tags_t = Tensor::from_array(([tags.len(), 1usize], tags.to_vec()))
                    .map_err(|e| format!("tableformer: tags: {e}"))?;
                match cache.a.as_ref() {
                    None => docling_onnx::run(
                        &mut self.decoder,
                        ort::inputs![
                        "tags" => tags_t, "cross_k" => ck, "cross_v" => cv,
                        "cache" => &empty.0],
                    ),
                    Some(c) => docling_onnx::run(
                        &mut self.decoder,
                        ort::inputs![
                        "tags" => tags_t, "cross_k" => ck, "cross_v" => cv,
                        "cache" => c],
                    ),
                }
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
        if self.style != DecoderStyle::Legacy {
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

    /// One step of a one-tag-per-step graph over `tags.len()` rows, through
    /// whichever cache layout the loaded decoder has.
    fn step_kv(
        &mut self,
        tags: &[i64],
        per_layer: &[(String, DynValue)],
        cache: &mut DecodeCache,
        empty: &EmptyCache,
    ) -> Result<(Vec<i64>, Vec<f32>), String> {
        if self.style == DecoderStyle::KvLayered {
            self.step_kv_layered(tags, per_layer, cache)
        } else {
            self.step_kv_hoisted(tags, per_layer, cache, empty)
        }
    }

    /// One `KvLayered` step: [`Self::step_kv_hoisted`] with the caches read
    /// from and written into the [`LayerCache`] buffers — ONNX Runtime
    /// allocates nothing that grows with the table (#871).
    fn step_kv_layered(
        &mut self,
        tags: &[i64],
        per_layer: &[(String, DynValue)],
        cache: &mut DecodeCache,
    ) -> Result<(Vec<i64>, Vec<f32>), String> {
        let rows = tags.len();
        let lc = match &mut cache.layers {
            Some(lc) => lc,
            None => cache
                .layers
                .insert(LayerCache::new(self.decoder.allocator(), rows)?),
        };
        if lc.rows != rows || lc.past >= MAX_STEPS {
            return Err(format!(
                "tableformer: kv cache for {} rows at {} tokens, asked for {rows} rows one more",
                lc.rows, lc.past
            ));
        }
        let (past, cap) = (lc.past, lc.cap);
        let tag_t = Tensor::from_array(([rows, 1usize], tags.to_vec()))
            .map_err(|e| format!("tableformer: tag: {e}"))?;
        let [first, second] = &mut lc.bufs;
        let (src, dst) = if lc.cur == 0 {
            (&*first, second)
        } else {
            (&*second, first)
        };
        let mut inputs: Vec<(
            std::borrow::Cow<'_, str>,
            ort::session::SessionInputValue<'_>,
        )> = Vec::with_capacity(1 + 2 * N_LAYERS + per_layer.len());
        inputs.push(("tag".into(), tag_t.into()));
        let in_len = rows * KV_HEADS * past * KV_HEAD_DIM;
        for j in 0..2 * N_LAYERS {
            let name = LayerCache::input_name(j);
            if past == 0 {
                inputs.push((name.into(), (&lc.empty[j]).into()));
            } else {
                let view = TensorRef::from_array_view((
                    LayerCache::shape(j, rows, past),
                    &src[j * cap..j * cap + in_len],
                ))
                .map_err(|e| format!("tableformer: {name}: {e}"))?;
                inputs.push((name.into(), view.into()));
            }
        }
        for (name, v) in per_layer {
            inputs.push((name.as_str().into(), v.into()));
        }
        let alloc = self.decoder.allocator();
        let mut outputs = ort::session::OutputSelector::default();
        for (j, region) in dst.chunks_exact_mut(cap).enumerate() {
            // SAFETY: `dst` is borrowed mutably for this whole step and
            // nothing else touches it; the values over it live in `opts` and
            // `dout`, both dropped at the end of this function, before the
            // next step reads the buffer (or the cache frees it).
            let out = unsafe {
                docling_onnx::borrowed_f32(region, &LayerCache::shape(j, rows, past + 1), alloc)
            }
            .map_err(|e| format!("tableformer: kv cache output: {e}"))?;
            outputs = outputs.preallocate(format!("out_{}", LayerCache::input_name(j)), out);
        }
        let opts = docling_onnx::run_options_with_outputs(outputs)?;
        let dout = self
            .decoder
            .run_with_options(inputs, &opts)
            .map_err(|e| format!("tableformer: decode: {e}"))?;
        let (_, logits) = dout["logits"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: logits: {e}"))?;
        let vocab = logits.len() / rows;
        let raws: Vec<i64> = logits
            .chunks_exact(vocab)
            .map(|row| argmax(row) as i64)
            .collect();
        let (_, hidden) = dout["hidden"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: hidden: {e}"))?;
        let hidden = hidden.to_vec();
        drop(dout);
        drop(opts);
        lc.past += 1;
        lc.cur ^= 1;
        Ok((raws, hidden))
    }

    /// One `KvHoisted` step over `tags.len()` rows — one table per row. `tags`
    /// holds each row's last emitted tag, `per_layer` the cross tensors with a
    /// matching leading batch axis (the encoder's own `[1,…]` outputs for a
    /// single table, or [`Self::batch_cross`]'s concatenation), and the cache
    /// grows `[N_LAYERS, rows, H, past, hd]` in lockstep. Returns each row's raw
    /// argmax tag and the `[rows, EMBED_DIM]` hidden states, flattened.
    fn step_kv_hoisted(
        &mut self,
        tags: &[i64],
        per_layer: &[(String, DynValue)],
        cache: &mut DecodeCache,
        empty: &EmptyCache,
    ) -> Result<(Vec<i64>, Vec<f32>), String> {
        let rows = tags.len();
        let tag_t = Tensor::from_array(([rows, 1usize], tags.to_vec()))
            .map_err(|e| format!("tableformer: tag: {e}"))?;
        let mut inputs: Vec<(
            std::borrow::Cow<'_, str>,
            ort::session::SessionInputValue<'_>,
        )> = Vec::with_capacity(3 + per_layer.len());
        inputs.push(("tag".into(), tag_t.into()));
        match (cache.a.as_ref(), cache.b.as_ref()) {
            (Some(k), Some(v)) => {
                inputs.push(("cache_k".into(), k.into()));
                inputs.push(("cache_v".into(), v.into()));
            }
            _ => {
                inputs.push(("cache_k".into(), (&empty.0).into()));
                inputs.push((
                    "cache_v".into(),
                    empty
                        .1
                        .as_ref()
                        .expect("kv empty cache has both halves")
                        .into(),
                ));
            }
        }
        for (name, v) in per_layer {
            inputs.push((name.as_str().into(), v.into()));
        }
        let mut dout = docling_onnx::run(&mut self.decoder, inputs)
            .map_err(|e| format!("tableformer: decode: {e}"))?;
        let (_, logits) = dout["logits"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: logits: {e}"))?;
        let vocab = logits.len() / rows;
        let raws: Vec<i64> = logits
            .chunks_exact(vocab)
            .map(|row| argmax(row) as i64)
            .collect();
        let (_, hidden) = dout["hidden"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("tableformer: hidden: {e}"))?;
        let hidden = hidden.to_vec();
        cache.a = Some(
            dout.remove("out_cache_k")
                .ok_or_else(|| "tableformer: out_cache_k missing".to_string())?,
        );
        cache.b = Some(
            dout.remove("out_cache_v")
                .ok_or_else(|| "tableformer: out_cache_v missing".to_string())?,
        );
        Ok((raws, hidden))
    }

    /// Stack the per-layer cross tensors of several encoded tables along the
    /// batch axis (`[1,H,hd,S]` × B → `[B,H,hd,S]`, same for `cross_v`), index-
    /// aligned with the decoder's input names. One copy per page — ~20 MB per
    /// table, nothing next to the decode steps it lets the tables share.
    fn batch_cross(encs: &[EncodeOut]) -> Result<Vec<(String, DynValue)>, String> {
        let b = encs.len();
        let mut out = Vec::with_capacity(encs[0].per_layer.len());
        for j in 0..encs[0].per_layer.len() {
            let name = encs[0].per_layer[j].0.clone();
            let mut data: Vec<f32> = Vec::new();
            let mut dims = [b, 0, 0, 0];
            for enc in encs {
                let (shape, v) = enc.per_layer[j]
                    .1
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("tableformer: {name}: {e}"))?;
                if shape.len() != 4 || shape[0] != 1 {
                    return Err(format!("tableformer: {name}: unexpected shape {shape:?}"));
                }
                dims[1..].copy_from_slice(&[
                    shape[1] as usize,
                    shape[2] as usize,
                    shape[3] as usize,
                ]);
                data.reserve(v.len() * b);
                data.extend_from_slice(v);
            }
            let t = Tensor::from_array((dims, data))
                .map_err(|e| format!("tableformer: {name}: {e}"))?;
            out.push((name, t.into_dyn()));
        }
        Ok(out)
    }

    /// Decode `encs.len()` tables in lockstep: every step runs the decoder once
    /// over all of them (a step is 49 weight-streaming GEMMs over one token per
    /// row — B rows cost about what one does). The caches start empty for
    /// every row and grow together, so nothing is ever padded or masked; a
    /// table that emits `<end>` simply keeps its row (fed `END`, output
    /// ignored) until the last one finishes. Row b of every op is exactly the
    /// single-table computation, so each table's tokens and hidden states are
    /// bit-identical to decoding it alone (asserted by the export script's
    /// batching gate; the corpus snapshots pin it end-to-end).
    fn decode_batch(&mut self, encs: &[EncodeOut]) -> Result<Vec<BboxBook>, String> {
        let b = encs.len();
        let cross = Self::batch_cross(encs)?;
        let mut books: Vec<BboxBook> = (0..b).map(|_| BboxBook::new()).collect();
        let mut active = vec![true; b];
        let mut last = vec![START; b];
        let mut cache = DecodeCache::default();
        let empty = self.empty_cache(b)?;
        crate::timing::timed("tf.decode_loop", || -> Result<(), String> {
            // Each active table's `otsl` grows by one per step, so a shared
            // step counter is the per-table `otsl.len() < MAX_STEPS` bound.
            for _ in 0..MAX_STEPS {
                if !active.iter().any(|a| *a) {
                    break;
                }
                let (raws, hidden) = crate::timing::timed("tf.decode_step", || {
                    self.step_kv(&last, &cross, &mut cache, &empty)
                })?;
                for t in 0..b {
                    if !active[t] {
                        continue;
                    }
                    let h = &hidden[t * EMBED_DIM..(t + 1) * EMBED_DIM];
                    if books[t].step(raws[t], h) {
                        last[t] = *books[t].tags.last().expect("step pushed a tag");
                    } else {
                        active[t] = false;
                        last[t] = END;
                    }
                }
            }
            Ok(())
        })?;
        Ok(books)
    }

    /// The zero-`past` first-step cache(s) for `rows` tables, allocated through
    /// the session allocator (ort's array constructors reject a 0-length dim;
    /// the C API does allow it).
    fn empty_cache(&self, rows: usize) -> Result<EmptyCache, String> {
        let alloc = self.decoder.allocator();
        if self.style != DecoderStyle::Legacy {
            let mk = || {
                Tensor::<f32>::new(alloc, [N_LAYERS, rows, KV_HEADS, 0usize, KV_HEAD_DIM])
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
        // Structure corrections live in tf_core::correct (shared with the wasm
        // path); docling's line_num is never incremented, so xcel→lcel fires on
        // every row.
        let mut tags: Vec<i64> = vec![START];
        let mut out: Vec<i64> = Vec::new();
        let mut prev_ucel = false;
        let mut cache = DecodeCache::default();
        let empty = self.empty_cache(1)?;
        while out.len() < MAX_STEPS {
            let (raw, _hidden) = self.decode_step(&tags, &enc, &mut cache, &empty)?;
            let tag = correct(raw, prev_ucel);
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

        // The autoregressive loop's bbox bookkeeping lives in tf_core::BboxBook
        // (shared with the wasm path); this loop only steps the decoder.
        let mut book = BboxBook::new();
        let mut cache = DecodeCache::default();
        let empty = self.empty_cache(1)?;
        crate::timing::timed("tf.decode_loop", || -> Result<(), String> {
            while book.otsl.len() < MAX_STEPS {
                let (raw, hidden) = self.decode_step(&book.tags, &enc, &mut cache, &empty)?;
                if !book.step(raw, &hidden) {
                    break;
                }
            }
            Ok(())
        })?;
        self.finish_table(book, &enc.eo)
    }

    /// The bbox stage after a table's decode loop: run the bbox decoder over
    /// the collected per-cell hidden states, merge span boxes, lay the cells
    /// onto the OTSL grid.
    fn finish_table(
        &mut self,
        mut book: BboxBook,
        eo: &DynValue,
    ) -> Result<Vec<TableCell>, String> {
        if book.runaway() {
            docling_core::debug_log!(
                "docling-pdf: tableformer: no row break in {MAX_ROW_TAGS} tags; \
                 geometric table fallback"
            );
            return Ok(Vec::new());
        }
        if book.n == 0 {
            return Ok(Vec::new());
        }
        let tag_h = Tensor::from_array(([book.n, EMBED_DIM], std::mem::take(&mut book.hiddens)))
            .map_err(|e| format!("tableformer: tag_h: {e}"))?;
        let bout = crate::timing::timed("tf.bbox", || {
            docling_onnx::run(
                &mut self.bbox,
                ort::inputs!["enc_out" => eo, "tag_h" => tag_h],
            )
            .map_err(|e| format!("tableformer: bbox: {e}"))
        })?;
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
        let (merged, merged_classes) = merge_spans(&boxes, &classes, &book.merge);
        Ok(build_table_cells(&book.otsl, &merged, &merged_classes))
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
    ) -> Option<crate::tf_core::TableGrid> {
        let page1024 = Self::page_1024(page_image);
        self.predict_table_rows_on(page_image.height(), &page1024, region, words)
    }

    /// The page rendered at 1024 px height (cv2.INTER_AREA), the frame every
    /// table crop of that page is cut from. Computed once per page by the
    /// pipeline and shared across its tables — the resample is a full-page
    /// f64 box filter, 110–170 ms on the corpus pages, and it used to run
    /// again for every table on the page.
    pub fn page_1024(page_image: &RgbImage) -> RgbImage {
        let sf = 1024.0 / page_image.height() as f32;
        let pw = (page_image.width() as f32 * sf) as u32;
        crate::timing::timed("tableformer.inter_area", || {
            crate::resample::inter_area(page_image, pw, 1024)
        })
    }

    /// [`predict_table_rows`](Self::predict_table_rows) with the page's
    /// 1024-px frame already built ([`page_1024`](Self::page_1024));
    /// `page_h` is the source page image's pixel height.
    pub fn predict_table_rows_on(
        &mut self,
        page_h: u32,
        page1024: &RgbImage,
        region: [f32; 4],
        words: &[TextCell],
    ) -> Option<crate::tf_core::TableGrid> {
        let crop = Self::crop_region(page_h, page1024, region)?;
        let cells = crate::timing::timed("tableformer.structure", || {
            self.predict_table_structure(&crop)
        })
        .ok()?;
        if cells.is_empty() {
            return None;
        }
        // The ort-free tail (word matching + grid assembly) is shared with the
        // browser path in tf_core.
        crate::tf_core::table_rows(&cells, region, words)
    }

    /// Every table of a page at once: [`predict_table_rows_on`](Self::predict_table_rows_on)
    /// per region, except that with the dynamic-batch decoder the tables'
    /// decode steps are shared — each table is encoded on its own, then one
    /// decode loop steps all of them together ([`Self::decode_batch`]), then
    /// each runs its own bbox head. Per table the result is bit-identical to
    /// the one-at-a-time path; a page with a single table takes exactly that
    /// path. Should the batched run fail (an ort error), the tables are
    /// retried one by one so a page never loses all of its tables to one
    /// shared step.
    pub fn predict_tables_on(
        &mut self,
        page_h: u32,
        page1024: &RgbImage,
        regions: &[[f32; 4]],
        words: &[TextCell],
    ) -> Vec<Option<crate::tf_core::TableGrid>> {
        let mut out: Vec<Option<crate::tf_core::TableGrid>> = vec![None; regions.len()];
        let crops: Vec<(usize, RgbImage)> = regions
            .iter()
            .enumerate()
            .filter_map(|(i, r)| Self::crop_region(page_h, page1024, *r).map(|c| (i, c)))
            .collect();
        if self.batched && crops.len() > 1 {
            let batched = crate::timing::timed("tableformer.structure", || {
                self.predict_structures_batched(crops.iter().map(|(_, c)| c))
            });
            match batched {
                Ok(cells) => {
                    for ((i, _), cells) in crops.iter().zip(cells) {
                        if !cells.is_empty() {
                            out[*i] = crate::tf_core::table_rows(&cells, regions[*i], words);
                        }
                    }
                    return out;
                }
                Err(e) => docling_core::debug_log!(
                    "docling-pdf: tableformer batched decode failed ({e}); decoding tables one by one"
                ),
            }
        }
        for (i, crop) in &crops {
            let cells = crate::timing::timed("tableformer.structure", || {
                self.predict_table_structure(crop)
            });
            if let Ok(cells) = cells {
                if !cells.is_empty() {
                    out[*i] = crate::tf_core::table_rows(&cells, regions[*i], words);
                }
            }
        }
        out
    }

    /// [`predict_table_structure`](Self::predict_table_structure) for several
    /// crops with the decode steps shared across them.
    fn predict_structures_batched<'a>(
        &mut self,
        crops: impl Iterator<Item = &'a RgbImage>,
    ) -> Result<Vec<Vec<TableCell>>, String> {
        let mut encs = Vec::new();
        for crop in crops {
            encs.push(self.encode(crop)?);
        }
        let books = self.decode_batch(&encs)?;
        books
            .into_iter()
            .zip(&encs)
            .map(|(book, enc)| self.finish_table(book, &enc.eo))
            .collect()
    }

    /// Crop the table bbox out of the 1024px frame. docling's coordinate
    /// chain, rounding included: the cluster bbox is rounded to integer page
    /// points *first* (`round(cluster.bbox.l) * scale`, banker's rounding),
    /// scaled by 2 (its table-structure page scale), then by `1024 / <2x
    /// page-image height>`, and the crop indices round again. Rounding after
    /// scaling instead shifts some crops by a pixel — enough to change
    /// TableFormer's cell boxes on tall tables (redp5110's TOC). `None` for a
    /// region that collapses to an empty crop.
    fn crop_region(page_h: u32, page1024: &RgbImage, region: [f32; 4]) -> Option<RgbImage> {
        let k = 2.0 * 1024.0 / page_h as f64;
        let px = |v: f32| (v as f64).round_ties_even() * k;
        let x = (px(region[0]).round_ties_even()).max(0.0) as u32;
        let y = (px(region[1]).round_ties_even()).max(0.0) as u32;
        let x2 = (px(region[2]).round_ties_even() as u32).min(page1024.width());
        let y2 = (px(region[3]).round_ties_even() as u32).min(page1024.height());
        if x2 <= x || y2 <= y {
            return None;
        }
        Some(image::imageops::crop_imm(page1024, x, y, x2 - x, y2 - y).to_image())
    }
}

/// Note once per process that TableFormer's ONNX graphs weren't found, so tables
/// fall back to geometric reconstruction. The default paths are relative
/// (`.models/tableformer/*.onnx`), which only resolves when the process's current
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
    Tensor::from_array(([1usize, 3, SIDE, SIDE], preprocess_input(img)))
        .map_err(|e| format!("tableformer: input: {e}"))
}
