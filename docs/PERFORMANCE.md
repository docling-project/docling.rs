# Performance

Where the PDF pipeline's time goes, what was measured, which optimizations are
validated (INT8, the TableFormer rounds, GPU execution providers) and the
backlog. Split out of [`PDF_CONFORMANCE.md`](PDF_CONFORMANCE.md), which keeps
the conformance numbers; nothing here changes the output (every optimization
is byte-identical or conformance-gated).

## Parallelism

Profiling a 14-page document (`DOCLING_RS_TIMING=1` prints an env-gated per-stage
wall-clock breakdown) shows ~80 % of the time is the two ONNX models (layout ~58 %,
TableFormer ~22 %) and ~16 % the page-image downsample — all per-page work that is
independent across pages. A multi-page PDF therefore renders on one thread (one
`Renderer` per document, its font and image caches shared across the pages)
and fans the pages out across a **pool of page-workers**, each
owning its own model set (`ort`'s `Session::run` is `&mut self`, so sessions can't
be shared), reassembled in page order. A bounded channel keeps only a handful of
page bitmaps resident, so the streaming memory profile is preserved; the output is
byte-identical to the serial path (verified across all PDF snapshots). Single-page /
image / METS inputs keep the serial path and load no helper models.

The layout model is **memory-bandwidth bound** (even one model at four intra-op
threads only reaches ~2.1× core utilisation), so the pool defaults to two intra-op
threads per worker with `workers ≈ cores / 2` (ceiling 16 — a memory bound of
~0.4 GB of model sessions per worker, not a performance cap; #324 follow-up
measured the old cap of 4 leaving ~1.2× on the table on a 16-core machine): two threads sharing one
in-cache copy of the weights beats both one fat model and many single-thread workers.
The speed-up scales with cores and memory bandwidth. Tune per machine with
`DOCLING_RS_PDF_WORKERS` (pool size) and `DOCLING_RS_PDF_INTRA` (intra-op threads
per worker). Each worker layout-detects up to `DOCLING_RS_PDF_LAYOUT_BATCH`
already-rendered pages per inference call (issue #73; default: per-page on
the CPU provider and under CoreML, 4 under CUDA/TensorRT/DirectML — CoreML
only takes static-shaped graphs and only the per-page graph is static,
#602; #338: every CPU
measurement favors per-page, 8.5 vs 9.3 s/conv on a 4-core x86 box and ~2×
on a 16-core M4 Max, while GPU dispatch overhead still amortizes). In
per-page mode the model's dynamic `batch` axis is pinned to 1 at session
creation (#339): the free dimension blocks ONNX Runtime's channels-last conv
transform (~1.4× on Apple-silicon CPU); x86 measured neutral. Output is
bit-identical at every batch size, so the knob is purely about throughput.

## Review & profiling notes

Post-migration review of the PDF processing path: where the time actually goes,
what was measured, which optimizations are validated, and a ranked backlog of
further ideas that do **not** trade away output quality.

### Results at a glance

Everything below was landed across two optimization rounds (PR #26, #27),
each change gated on corpus conformance — groundtruth distance unchanged or
better, byte-identical where the change is structural:

| Optimization | Measured effect |
|---|---|
| INT8 layout model (Conv-only static QDQ, calibrated; **default**) | layout inference **2.4×** faster; **1.83× end-to-end** on a 1913-page PDF (0.74 → 0.40 s/page) |
| INT8 TableFormer decoder (dynamic, **default**) | ~10% faster table decode, byte-identical |
| SIMD page downscale (`fast_image_resize`, same kernel; **default**) | `image.resize` stage **17×** faster (2607 → 152 ms / 16 pages) |
| TableFormer KV cache fed back as `ort` values (no per-step copy) | ~9% faster table-structure decode, byte-identical |
| One shared lazy TableFormer across the worker pool | peak RSS **3.8 → 1.9 GB** (4 workers); table-free docs 682 → 331 MB |
| Single shared line/word contraction pass | `--text-layer-only` (then `--no-ocr`) conversion ~1.25× faster, identical output |
| Per-document font + form caches in the text parser | 3–10% off `textparse` here; far more on CJK/form-heavy PDFs |
| True-KV-cache decoder export (`decoder_kv.onnx`, optional) | parity at corpus table sizes; O(past)/step for very large tables |
| Dynamic-batch `decoder_kv.onnx`: a page's tables decode in one lockstep loop | decode steps shared across tables; byte-identical (see round four below) |
| Per-layer caches in loop-owned buffers (`decoder_kv_layers.onnx`, #871) | no per-step allocation that grows with the table (the arm64 OOM); long tables ~24% faster; bit-identical (round six) |

Cumulative head-to-head vs Python docling (measured on an 8-thread desktop,
`scripts/test/performance.sh`): **4.3× faster warm conversion, 4.7× end-to-end,
2.3–2.6× less peak memory** on the PDF ML pipeline — up from ~1.2× warm
before this work. Model sizes: layout 172 → 68 MB, TF decoder 78 → 50 MB.

A re-measure on the final stack (hoisted-KV TableFormer decoder default per
#97; different desktop, Jul 2026) with `performance.sh
tests/data/pdf/sources/2305.03393v1-pg9.pdf` — a single table-dense page, so
the page-parallel worker pool sits mostly idle and this bounds the *low* end
of the speedup range: **3.0× end-to-end** (16.3 → 5.4 s avg over 5 runs),
**2.0× warm conversion** (9.1 → 4.7 s/doc), **1.9× less peak memory**
(1589 → 857 MB).
Also fixed along the way: the `"` show-text operator dropped its word/char
spacing operands (real spec violation), and OCR/TableFormer sub-stages are
now visible in `DOCLING_RS_TIMING` profiles.

Measured on a 4-core AVX-512(+VNNI/AMX) Xeon, release build (`lto = "thin"`),
models from `scripts/install/download_dependencies.sh`, `DOCLING_RS_TIMING=1`.

### Where the time goes

Per-stage wall-clock share (summed across workers):

| Stage | 1913-page text-heavy PDF¹ | 16-page table-heavy paper² | scanned page³ |
|---|---:|---:|---:|
| `layout.predict` (RT-DETR ONNX) | **80.3%** | 55.4% | 64.9% |
| `image.resize` (3×→2× CatmullRom) | 14.9% | 7.9% | 18.5% |
| `tableformer` | 2.8% | 32.1% | — |
| page render (then pdfium; the Rust renderer's share is of the same order, 50–200 ms a page) | 1.8% | 3.7% | 16.5% |
| `textparse` + assembly | ~0.2% | ~0.3% | ~0.1% |

¹ `tests/data/pdf/large/dotnet-csharp-language-reference.pdf` — 936 s wall, ~0.49 s/page.
² `tests/data/pdf/sources/2203.01017v2.pdf`.
³ `tests/data/scanned/sources/ocr_test.pdf`.

Two conclusions drive everything below:

1. **ONNX inference is ~85–95% of PDF conversion time.** All the Rust-side text
   extraction, parsing, and assembly work combined is under 1%. Rust-code
   micro-optimizations are irrelevant to PDF throughput until the models get
   faster; model-level and preprocessing-level changes are the only levers that
   matter.
2. Within TableFormer, the **autoregressive decode loop** dominates
   (`tableformer.structure` ≈ 96% of the stage; the per-table page resample
   `tableformer.inter_area` is ~1% of a conversion).

The worker-pool topology heuristic in `lib.rs` (`workers × intra ≈ cores`,
default 2×2 on 4 cores) was re-validated: 2×2 beat both 4×1 and 1×4 on the
16-page document (11.6 s vs 12.2 s vs 15.6 s; separate run from the INT8
table below, hence the ±0.1 s vs its 11.5 s).

### Validated win: INT8 quantization (quality-checked)

`scripts/install/quantize_models.py` produces two quantized models. Point
`DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_DECODER` at them to opt in.

**These are now the default:** when the `*_int8` files sit next to the fp32
models at the default paths, the pipeline loads them automatically.
`DOCLING_RS_FP32=1` forces full precision, and an explicit
`DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_DECODER` always wins (the
conformance/groundtruth scripts pin fp32 explicitly, so snapshots stay
deterministic).

#### TableFormer encoder: not quantized, but half its size (#374)

The encoder is a ~20-Conv ResNet backbone feeding a 6-layer transformer, and
the transformer Gemms — not the convs — are its cost. INT8 was measured and
rejected: conv-only static QDQ keeps fidelity (enc_out / cross-K/V cosine
≥ 0.995) but is not faster than ORT's fp32 conv kernels, and quantizing the
attention MatMuls collapses cross-attention fidelity to ~0.85 (garbled table
structure). What *was* wrong with the published 215 MiB `encoder.onnx` is
exporter waste: the explicit all-false attention mask handed to the
transformer encoder was materialized as a zero `[1,8,784,784]` fp32 constant
(18.8 MiB) baked into each of the six layers — 112.6 MiB of `x + 0` around
103 MiB of real weights (42.6 MiB Conv, 60 MiB MatMul/Gemm).
`scripts/install/strip_zero_masks.py` (run by the export) removes those `Add`
nodes; the stripped graph's outputs are bit-identical to the original's
(onnxruntime, max |diff| = 0), so the republished encoder changes nothing but
the download. On top of that, `encoder_fp16.onnx` (`quantize_models.py
tableformer-encoder-fp16`) stores the same graph's weights as fp16 behind a
`Cast` back to fp32 — ORT folds the cast at load, so compute, speed and
memory are those of the fp32 encoder and only the file shrinks, 103 → 54 MB
(4.2× below the original 226 MB). Fidelity gate on the 54 calibration inputs:
cosine ≥ 0.999999, relative L2 error ≤ 1.5e-3 per output tensor; and over the
full 101-file snapshot corpus the fp16 encoder's Markdown is **byte-identical**
to the fp32 encoder's (same machine, same binary, `diff -rq` empty). It is
therefore preferred when present, like the INT8 decoder; `DOCLING_RS_FP32=1`
or an explicit `DOCLING_TABLEFORMER_ENCODER` keeps the fp32 file. INT8 for
the encoder stays off the table.

#### Layout: static QDQ INT8, **Conv ops only** (~2.4× faster layout)

Calibrated on 42 real corpus pages preprocessed exactly like
`layout.rs::predict`. Only the HGNetv2 backbone convolutions are quantized;
the transformer decoder and detection-head MatMuls stay fp32.

| Configuration | layout.predict (16-page doc)¹ | end-to-end wall | model size |
|---|---:|---:|---:|
| fp32 baseline | 17.2 s | 16.6 s | 172 MB |
| **INT8 conv-only** | **7.2 s (2.4×)** | 11.5 s (1.45×) | 68 MB |
| + INT8 TableFormer decoder | — | 12.3 s² | — |

¹ `layout.predict` is summed across the parallel page workers, so it can
exceed the end-to-end wall.
² Separate run; within run-to-run noise of the 11.5 s conv-only wall — the
INT8 decoder's own win is per-table (~10 % faster tables, byte-identical;
see its section below), not end-to-end on this document.

On text-dominated documents (layout = 80% of time) the end-to-end gain
approaches ~1.7–2×; on table-heavy ones it is ~1.4×.

Full-scale run — the 1913-page `dotnet-csharp-language-reference.pdf`,
INT8 layout + INT8 TableFormer decoder vs fp32, same machine and binary,
back-to-back:

| | fp32 | INT8 | ratio |
|---|---:|---:|---:|
| wall clock | 1406 s (0.74 s/page) | **770 s (0.40 s/page)** | **1.83×** |
| `layout.predict` (summed) | 2667 s | 1350 s | 1.98× |
| output difference | — | 1199 of 52,615 Markdown lines (2.3%) | |

The 2.3% of differing lines are the same near-threshold classification flips
seen on the corpus (where groundtruth conformance measured *equal or slightly
better* under INT8 — 812 vs 833 summed diff-lines), not a systematic
degradation. With layout halved, `image.resize` becomes the next stage
(24.8% of the INT8 run), which is why backlog item 4 matters more after
quantization.

**Quality gate** (measured at INT8-selection time over the then-23-file
PDF+scanned corpus):

- Conv-only INT8: 12/23 byte-identical to fp32; remaining diffs are small
  region-classification flips. Against the committed groundtruth the summed
  diff-line distance is **812 (INT8) vs 833 (fp32)** — i.e. conformance-neutral
  (INT8 is marginally better on 3 fixtures, marginally worse on 2).
- Full INT8 (convs + MatMuls) was **rejected**: 3/23 exact, with clear quality
  loss (section headers demoted to plain text, page-footer text leaking into
  the output) — the RT-DETR head's class scores sit near the 0.3 threshold and
  cannot tolerate activation quantization.
- Dynamic (weights-only) INT8 of the whole layout model was also rejected: it
  is *slower* than fp32 (3.2 s vs 2.1 s per page-with-table) because inserted
  per-activation quantize ops outweigh the MatMul savings while the conv
  backbone stays fp32.

##### 7-bit weights: the same model on every x86 CPU

The quantizer emits **7-bit** weights (`reduce_range=True`). u8 activations
times s8 weights go through `VPMADDUBSW` on CPUs without VNNI, and that
instruction sums two products into one int16 slot: full-range weights reach
255·127·2 = 64770 and saturate at 32767, so a model that measures perfectly on
the machine that quantized it can drop whole regions on a plain AVX2 CPU.
Not hypothetical — a publish run's agreement gate lost **83 of 505** confident
detections (two of them tables) on a runner without VNNI, while the same recipe
over a structurally identical export lost **1** on a VNNI box. 7 bits caps the
pair product at 255·64·2 = 32640, below saturation.

It costs nothing. Same VNNI machine, same corpus, 4 intra-op threads (the
ratios are the point, not the absolute times — this is a shared container, not
the benchmark box the table above was measured on):

| layout_heron | agreement gate (505 confident fp32 detections) | 640×640 inference |
|---|---:|---:|
| fp32 | — | 548 ms |
| INT8, full-range weights | 1 lost (0.20%) | 290 ms |
| **INT8, 7-bit weights** | **0 lost (0.00%)** | **282 ms** |

`quantize_models.py` checks the weight range right after quantizing
(`check_weight_range`), before the accuracy gate. Reading weights is
hardware-independent, so a recipe that reintroduces full-range weights fails on
the publishing machine instead of on a user's laptop — the accuracy gate alone
cannot catch this, since it only ever exercises the ISA it happens to run on.
An int8 layout model fetched before this change is full-range: on a non-VNNI
CPU, refetch after the next models publish, or set `DOCLING_RS_FP32=1`.

##### BatchNorm folded before quantization (~1.4× faster int8 layout)

A re-profile (Sep 2026, 4-core Xeon, ORT node profiler on the int8 graph)
found ~40% of layout inference in fp32 elementwise ops that the fp32 path
never executes: `Mul` 13%, `Add` 12%, `DequantizeLinear` 12%,
`QuantizeLinear` 4%. The HGNetv2 export spells eval-mode BatchNorm as
`Conv → Mul(1,C,1,1) → Add(1,C,1,1)`; in an fp32 session ONNX Runtime's
ConvMulFusion/ConvAddFusion fold that into one FusedConv at load, but the
QDQ quantizer wraps the Conv in Quantize/Dequantize pairs that block the
fusion, so every one of the backbone's 110 normalizations ran as
`DQ → Mul → Add → Q` over the full activation tensor. `quantize_models.py`
now folds the 55 pairs into the conv weights and bias first
(`fold_conv_affine`: `(W∗x)·s + t == (W·s)∗x + t`, exact up to the fp32
rounding the runtime fusion performs as well) and the graph collapses to
back-to-back QLinearConvs.

Folding alone moved int8/fp32 agreement sideways (mean |Δscore| over the
1045 above-threshold fp32 detections on the calibration pages 0.049 → 0.051),
so the activation ranges now come from `CalibMovingAverage` — per-page
min/max averaged instead of corpus-wide extremes, so one outlier page no
longer sets the u8 grid for all. Entropy and percentile calibration were
tried on the same graph and landed in between (0.046 / 0.043), on top of
needing every activation sample in RAM (the 52-page set does not fit in
15 GB). Same box, same corpus, single-thread ORT for the output diffs:

| layout_heron_int8 recipe | 2 thr | 4 thr | mean \|Δscore\| vs fp32 | lost (of 1045) | Markdown diff-lines vs fp32 (24 docs) | vs groundtruth (17 docs) |
|---|---:|---:|---:|---:|---:|---:|
| previous (unfolded, MinMax) | 602 ms | 381 ms | 0.049 | 38 | 327 | 564 |
| folded, MinMax | 453 ms | 289 ms | 0.051 | 48 | 345 | 576 |
| folded, MinMax moving average | 424 ms | 289 ms | 0.037 | 28 | 290 | 563 |
| **+ stem convs `embedder.0/1` fp32** | **≈ same¹** | **≈ same¹** | **0.022** | **15** | **146** | **423** |

¹ Interleaved bench, 30 iterations: 350.3 vs 348.6 ms (2 thr), 247.4 vs
249.9 ms (4 thr) against the all-int8 row — inside run-to-run noise. The
whole stem in fp32 (`embedder.2` too) would have cost ~12% for 0.020 / 17;
`embedder.0` alone buys little (0.035 / 19).

The quantization error concentrates at the front of the backbone: the two
stem convs see the raw pixels at 320×320, the largest maps in the graph, and
their u8 rounding rides through every later stage. Leaving those two in fp32
takes the int8 model's distance to fp32 output from 327 to **146** diff-lines
(`redp5110_sampled` 99 → 2, `right_to_left_03` 46 → 0) and its groundtruth
distance from 564 to **423** — fp32 itself sits at 411, so the int8 default
now costs 3% of conformance instead of 37%. Entropy and percentile
calibration were also tried (in between MinMax and moving average, and they
need every activation sample in RAM). Both gates pass on every row (0/505
confident detections lost; weights within 7 bits). Pipeline effect,
back-to-back cold CLI runs, default 2×2 pool, with the two changes below:
the 60-page slice of the .NET reference 34.5–34.9 s → **21.0–21.4 s
(−39%)**; the table-heavy `2206.01062` 13.2–13.4 s → **9.1–9.8 s (−30%)**.

##### Text layer parsed per page, not up front

`for_each_page` used to run the pure-Rust text parser over *every* page of
the document before rendering the first one: a 6.5 s serial prefix on the
1913-page .NET reference that the page-worker pool sat idle through, and a
`--pages` window still paid for all 1913 pages (14% of a 60-page slice's
wall time). `PageTextParser` keeps the loaded document and the font/form
caches and parses a page when the render walk reaches it — 0.58 s to open
the document plus ~2.4 ms per selected page, overlapped with the workers'
inference. Same glyph walk, same shared caches, same contraction per page:
Markdown output is byte-identical over the PDF corpus, the scanned fixtures
and the 60-page slice (single-thread ORT, old vs new binary).
`DOCLING_RS_TIMING` now reports `textparse.open` plus a per-page `textparse`.

##### Pages parked on a busy TableFormer, not blocked

The pool shares one TableFormer behind a mutex (the memory win above), and a
worker whose page had a table used to block on it for the whole of another
worker's decode: on the 60-page slice ~9.5 s of the `tableformer` stage was
one worker idle (stage 22.5 s vs `tableformer.structure` 11.5 s +
`inter_area` 1.5 s), and pool topology could not buy it back — 2×2, 4×1 and
3×1 all landed within noise. `finish_page` is now `prepare_page` → table
stage → `complete_page`, and the pool path (`Worker::run_pool`, shared by
the buffered and streaming conversions) tries the slot without blocking:
when it is held, the prepared page is parked and the worker keeps pulling
from the render channel, retrying parked pages before every pull. While
pages are parked the pull is non-blocking, so an empty channel means "wait
for the TableFormer", never "hold work while waiting for the renderer". At
most two parked pages per worker (their bitmaps are the memory cost); past
that, or once the channel closes, the worker waits as before. Output does
not depend on completion order — both callers reassemble by page index —
verified byte-identical over 23 documents on a 2-worker pool with
single-thread sessions. On `2206.01062` the stage's wait fell from ~10 s to
~1.9 s (stage 8.3 s vs structure 6.0 s + inter_area 0.4 s).

##### Pillow-exact resize over rows

`pil_resize` (the docling-parity 1.5×→1× layout image on the render thread,
and the 640×640 layout input in each worker) went through
`get_pixel`/`put_pixel` per tap and walked the vertical pass by column:
~30 ms per page, slower than the SIMD 3×→2× downscale of a bigger image.
Both passes now run over raw rows with one i32 accumulator row for the
vertical pass. The arithmetic is unchanged and purely integer, so the bytes
are identical (the Pillow reference hashes pin that); `image.resize_layout`
is 11–12 ms per page. The render thread's remaining per-page work — the two
renders plus the two downscales — is not the bottleneck at 4 cores but caps a
many-core pool at roughly 9 pages/s.

##### Round two (Sep 2026): the fixed costs

With inference trimmed, a second profile of the merged stack looked at what a
conversion pays regardless of page count. All four changes below are
output-neutral: Markdown over the PDF corpus, the scanned/rotated fixtures and
the 60-page slice is byte-identical with and without them, on the serial path
(single-thread ORT) and on a 2-worker pool.

- **The page window loaded every page.** `for_each_page` walked
  `pages.iter()` from page 0 and skipped to `first`, so every page before the
  window was loaded and closed — ~0.7 ms each. A one-page `--text-layer-only`
  window over the 1913-page .NET reference took 3.1 s, of which the parser
  accounted for 0.4 s; indexing the window with `pages.get(i)` brings it to
  **0.85 s** (the full pipeline on that page: 4.1 → 2.1 s, `--pages 1-60`
  no-ocr 2.7 → 1.0 s). The parser's teardown — 250 ms of lopdf objects on
  that document — now happens on a detached thread (`textparse.close` still
  reports it) instead of delaying the last page.
- **Session creation is graph optimization.** Creating the int8 layout
  session costs ~0.8 s (4 threads) and the TableFormer encoder ~1 s; each
  process, each pool worker. ONNX Runtime can serialize the optimized graph
  and load it with the optimizer off in ~0.15 s, running the identical
  kernels. `docling_onnx::commit` keeps such a cache
  (`$XDG_CACHE_HOME/docling-rs/graphs`, `DOCLING_RS_GRAPH_CACHE_DIR`,
  `DOCLING_RS_NO_GRAPH_CACHE=1`), keyed on the model file, the session
  options that shape the graph, the ONNX Runtime API version and the CPU's
  SIMD features (the saved graph carries hardware-specific NCHWc kernels);
  CPU provider only, and every failure on the cached path falls back to the
  ordinary load. A one-page digital PDF: **1.26 → 0.68 s** wall (the first
  run after a model or machine change pays ~0.16 s extra to write the cache).
- **OCR ran on one core while three idled.** Recognition is pinned to a
  single intra-op thread for determinism (multi-threaded float reductions
  flip CTC argmaxes on low-confidence characters), and it is linear in line
  width — ~0.17 ms per pixel column, no batching benefit, no new-shape
  penalty (measured). Lines are independent, so `OcrModel` now holds one
  single-thread session per lane (the worker's thread budget;
  `DOCLING_RS_OCR_SESSIONS` overrides) and deals same-width batches across
  them by index; each line still meets exactly the single-thread kernel
  path. On `ocr_test.pdf`, 4 lanes: `ocr.rec` 374 → 188 ms, `orient.score`
  429 → 225 ms, wall 1.62 → 1.33 s; `nemotron_multipage` 6.5 → 4.1 s.
- **The orientation probe was a second OCR pass.** Sub-stage timers showed
  `orient.detect` on a scan is ~95% recognition: it reads the six *widest*
  lines, and on a small scan those are most of the page's text — the probe
  (467 ms) cost more than `ocr.rec` for the whole page (393 ms). The lanes
  halve it; the remaining lever is a smaller probe budget, not taken here
  because it changes the evidence the decision is made on.

Pool-path effect of this round, back-to-back runs: `2203.01017v2` 9.8 →
8.0–8.1 s (its in-picture table OCR rides the lanes), `2206.01062` 9.1–9.8 →
8.2–9.6 s, the 60-page slice 21.0–21.4 → 19.4–21.6 s.
##### Round three (Sep 2026): inside a table

Sub-stage timers (`tf.preprocess`, `tf.encoder`, `tf.decode_loop`,
`tf.bbox`, alongside `tf.decode_step`) split the ~1 s per table on
`2206.01062` (4 threads) into: decode loop 0.53 s (~107 steps, ~5 ms each),
bbox head 0.26 s, encoder 0.20 s, page→1024 resample 0.11 s, preprocessing
5 ms. Three exact fixes came out of that, one dead end, and one wall.

- **The bbox head fought the memory-pattern planner.** Its `tag_h` input is
  `[ncells, 512]` and every table has a different cell count, so ONNX
  Runtime re-planned buffer reuse on every run — and on this graph the plan
  is worse than none: 290 ms vs 54 ms for a 100-cell table, 560 vs 94 ms
  for 200 cells (repeat runs, 4 threads; the decoder session already ran
  without the planner for the same reason). Off now; the head still pays
  first-shape allocation per table because every table *is* a new shape.
- **One 1024-px frame per page.** The page→1024 INTER_AREA resample (a
  full-page f64 box filter, 110–170 ms) ran once per *table*;
  `TableFormer::page_1024` builds it once per page and every table crops
  from it. `2203.01017v2`: 8 → 4 resamples.
- **The decoder runs on one intra-op thread.** A step is 49 small GEMMs
  over a single token: it streams the layer weights (~70 MB per step) rather
  than computing, so threads only add synchronisation — isolated, 4.1 ms per
  step on 1 thread vs 5.5 on 4 (4.9 vs 7.1 once the KV cache is 100+ long).
  In the pool a table decode also stops taking every core from the other
  workers' layout inference, and a single-thread session has a fixed
  reduction order, so table structure no longer varies run-to-run on
  near-tie tokens (the conformance scripts pinned one thread for exactly
  that; the default matches them now). The encoder keeps the shared budget:
  680 ms single-threaded vs 165 on four.
- **Dead end: graph simplification.** The exported step graph has 449 nodes,
  ~415 executed per step, 275 of them Reshape/Transpose/Squeeze/Unsqueeze/
  Concat. `onnxsim` takes it to 383 nodes, bit-identical — and no faster
  (5.2 → 5.2 ms): ONNX Runtime already folds what can be folded at load, and
  the shape ops that remain are dispatch noise next to the GEMMs (42% of
  node time).
- **The wall.** What is left per table is the encoder's real compute (a
  448×448 CNN + transformer pass, 165 ms on 4 threads) and the decoder's
  weight streaming. Both move only with the model: INT8/fp16 weights for the
  step (the dynamic-INT8 `decoder_kv` was already tried and rejected — it
  flips near-tie tokens on redp5110's TOC), or batching several tables'
  steps through one run so the weights stream once for all of them. The
  first is not byte-exact by construction; the second turned out to be —
  round four below.

Back-to-back, same machine, default pool: `2206.01062` 9.4–9.6 → 8.8 s,
`2203.01017v2` 8.0–8.1 → 7.9 s, `2305.03393v1-pg9` (one 55-step table on
one page) 2.4 s. Output is byte-identical to the round-two references on the
serial path and on a 2-worker pool over the same 32 documents.

#### Round four (Sep 2026): a page's tables decode together

The decode step streams ~70 MB of layer weights for one token, so the
obvious lever left after round three was to push several tables' tokens
through the same step. It needed a dynamic-batch `decoder_kv.onnx` and a
loop that keeps the tables' KV caches aligned — and, unlike the earlier
guess, no masking at all.

- **The export** (`export_tableformer.py`, `DecodeKVHoistedBatched`): `tag`
  is `[B,1]`, the caches `[L,B,H,past,hd]`, every cross tensor `[B,…]`; B=1
  is the same graph, so the artifact stays a drop-in for a one-table loop.
  Two things had to be exact. First, ORT must fuse the batched graph the
  way it fuses the B=1 one: with a symbolic batch axis a 3-D activation
  stays MatMul-then-Add (a different rounding of the bias, ~1e-6 in the
  logits) while the B=1 graph gets MatMul+Add→Gemm through constant-shape
  Reshapes, and a fully 2-D module loses the Add+LayerNorm→
  SkipLayerNormalization fusion instead. The module therefore runs each
  linear on an explicit 2-D view and keeps the residual stream 3-D — the
  optimized graph then has the published one's kernel mix (49 Gemm, 18
  SkipLayerNorm, 12 FusedMatMul), and B=1 reproduces the published
  `decoder_kv.onnx` **bit for bit** over 3 random tables × 48 greedy steps.
  Second, the script's batching gate asserts that two tables decoded in one
  batch give, step by step, the logits and hidden states each gives alone
  (row b of an `[B,K]·[K,N]` GEMM is the `[1,K]` result in MLAS).
- **The loop** (`tableformer.rs::decode_batch`): every table of a page is
  encoded, the per-layer cross tensors are stacked along the batch axis
  (one ~20 MB copy per table per page), and one loop steps all of them. The
  caches start at `past=0` for every row and grow in lockstep, so nothing
  is padded or masked; a table that emits `<end>` keeps its row (fed `END`,
  output ignored) until the last one finishes, and each table then runs its
  own bbox head. Detected from the decoder's `tag` input having a symbolic
  batch axis, so the older fixed-`[1,1]` export keeps decoding one table at
  a time; a page with one table takes exactly that path; a failing batched
  run falls back to it.
- **What it buys, and what it can't.** Per-op profile of one step (1
  thread, past=50): the 49 Gemm total **6.8 ms at B=1 and 6.8 ms at B=2** —
  the weights do stream once for all rows — but each table brings its own
  cross-attention: q·Kᵀ and softmax·V over 784 image positions × 6 layers
  read ~19 MB of that table's `cross_kt`/`cross_v` per step (FusedMatMul
  1.7 → 3.2 ms, MatMul 1.5 → 2.6 ms for B 1 → 2). So a step costs 10.5 ms
  alone, 13.5 for two tables, 19.6 for four, 34 for eight: **−35% per table
  at two tables per page, −55% at four**, and that is the bound — the
  cross-attention reads are the model's, not the loop's.
- **Measured** (`DOCLING_RS_TIMING=1`, same binary, published vs
  dynamic-batch decoder): `2203.01017v2` (two tables on each of its pages)
  decode loop 3.1 → 2.5 s (297 single steps → 178 shared ones), serial wall
  25.2 → 24.6 s, default pool 10.0 → 9.4 s; `2206.01062` decode 5.6 →
  4.7 s, pool 10.4 → 9.8 s; `redp5110_sampled` (one table per page) flat,
  as it must be. Pool wall moves less than the decode does because the
  deferral machinery of round two already overlaps a page's tables with
  other pages' layout. Output byte-identical to the round-three references
  on the serial path and on a 2-worker pool over the same 32 documents.
- **Memory:** a page's tables are now all held encoded at once (per-layer
  cross tensors ~19 MB each) plus their stacked copy, against which the
  hoisted decoder's never-read stacked `cross_k`/`cross_v` (2×9.6 MB per
  table) are dropped at encode time now instead of living for the table's
  lifetime. Net on `2203.01017v2` (default pool): peak RSS 1.74 → 1.81 GB.

The dynamic-batch `decoder_kv.onnx` reaches users through the models
release (`publish-models.yml` re-run); until then the shipped decoder simply
takes the one-table-at-a-time path.

#### Round five (Sep 2026): the page→1024 resample, and a cache that cannot be poisoned

Two small items left over from round four's profile of master.

- **`inter_area`** (the page → 1024 px `cv2.INTER_AREA` box filter every
  table crop is cut from; `tableformer.inter_area`, ~0.16 s per table page
  in the pool, 43 ms in isolation) kept its addition order — horizontal
  taps in increasing source column, then vertical taps in increasing source
  row, f64 — and changed only its shape: the shrunk source rows are now
  computed on demand as the vertical pass reaches them and kept in a ring
  of a few rows (a source row feeds at most two output rows) instead of a
  30 MB `sh × dw` intermediate written and re-read, the horizontal taps run
  over the contiguous byte span they cover, and the vertical pass is a flat
  `f64` axpy. **43 → 18 ms** per page render (1224×1584 → 791×1024, one
  thread); a test asserts the bytes equal the naive per-pixel form on six
  geometries, and the 32-document corpus is byte-identical on the serial
  path and the 2-worker pool.
- **Graph cache guard** (`docling_onnx::commit`). ONNX Runtime serializes
  the optimized graph as a side effect of session creation and does not
  fail the session when that write comes up short: a full disk truncates
  the file and the session still initializes from memory. The cache then
  published the prefix, and every later process tried it, failed, removed
  it and rebuilt — a ~50 s cold start for the TableFormer graphs, seen once
  right after a disk-full episode in the round-four session. The commit
  path now checks the file's protobuf skeleton before renaming it into the
  cache (each top-level field's declared length must end within the file;
  a truncation lands inside the multi-megabyte `graph` field), a handful of
  reads and seeks, never a parse of the weights.

#### Round six (Oct 2026): per-layer caches in buffers the loop owns (#871)

Not a speed round — an out-of-memory one. On linux/arm64 (ONNX Runtime 1.31)
with the CPU arena off (`DOCLING_RS_NO_ARENA`, serve's default) a PDF with a
~700-step table ran a 6 GiB container out of memory: 4.3 GB of glibc heap
was *free* 608 steps into the first table, 530 MB in use. Every step of
`decoder_kv.onnx` made the KV cache one token longer and freed the old one —
and copied it several times over on the way: ONNX Runtime's optimized graph
splits the stacked `[L,B,H,past,hd]` input into the six layers, each layer
concatenates its token, K is transposed for q·Kᵀ, and the layers are stacked
and concatenated again into the outputs. Each of those buffers is a
`malloc` one token larger than its predecessor, and on arm64 glibc reused
25% of the freed blocks (87% on amd64), so they piled up.

- **The graph** (`scripts/install/kv_layers.py` → `decoder_kv_layers.onnx`):
  a rewrite of `decoder_kv.onnx`, not a re-export. The cache comes in and
  goes out per layer — `cache_kt_<i>` `[B,H,hd,past]` (K already transposed)
  and `cache_v_<i>` `[B,H,past,hd]` — so a step's only cache-sized work is
  one `Concat` per layer and tensor. The attention reads exactly the floats
  it read before (Kᵀ in the same layout, the same `FusedMatMul` with
  `alpha=s²`), and the rewrite keeps the exporter's `value_info` so ONNX
  Runtime still fuses Add+LayerNorm→SkipLayerNormalization (without it the
  residual stream rounds differently, ~5e-6 in the logits). Bit-identical
  to `decoder_kv.onnx` — logits, hidden states and caches — over the whole
  1024-step range, one table and two batched (`kv_layers.py --verify`, run
  by `export_tableformer.py`). Weights untouched: the new 0.25 MB graph
  reads `decoder_kv.onnx.data`.
- **The loop** (`tableformer.rs`, `DecoderStyle::KvLayered`): the caches live
  in two buffers per decode (a table, or a page's batch), each holding the
  12 tensors in regions sized for `MAX_STEPS` tokens — `calloc`'d, so pages
  become resident only as far as the table reaches. A step reads its caches
  from one buffer and binds the outputs to the other
  (`OutputSelector::preallocate` over `docling_onnx::borrowed_f32`, the one
  `unsafe` in it: an owned ONNX Runtime value over a slice the loop keeps
  borrowed for the step), so ONNX Runtime writes the next caches in place
  and allocates nothing that grows with the table; the next step swaps the
  buffers.
- **Measured** (x86_64, release CLI, `DOCLING_RS_NO_ARENA=1`; an
  `LD_PRELOAD` counter of allocations ≥ 64 KiB, three 79-row tables of
  ~790 steps): TableFormer's large allocations **44,600 → 620** (36.1 →
  8.0 GiB allocated over the run, 843 → 27 distinct sizes; what is left is
  the encoder, once per table); the same PDF converts in **69.0 → 52.2 s**
  — the per-step Split/Transpose/re-stack copies were real work too. Peak
  RSS on x86 is unchanged (~2.1 GB; amd64 reused the blocks), Markdown and
  JSON byte-identical on the 8 table-bearing corpus PDFs and that one.
  The same change pins glibc's mmap threshold in every process that loads
  a model (`docling_core::memory`, until then serve and MCP only), which on
  the reporter's machine alone avoided the OOM, at 0.66–1.28 GB above
  2.22.0's peak.

`decoder_kv_layers.onnx` reaches users through the models release: an
installation without it keeps decoding with `decoder_kv.onnx`.

#### TableFormer decoder: dynamic INT8 (~10% faster tables, byte-identical)

The autoregressive tag decoder is MatMul-only; weights-only dynamic INT8
produced **byte-identical corpus output** and ~10% faster table decode
(784 → 695 ms/table), 78 → 50 MB. Small but free.

The decoder speed is *not* weight-bound — it is per-step overhead (see backlog
item 2), which is why quantization helps so little there.

### GPU execution providers (#74) — validated on GPU (#108)

The ONNX sessions (layout, TableFormer×3, OCR recognition, both enrichment
models) accept alternative ONNX Runtime execution providers behind cargo
features: `cuda`, `tensorrt`, `directml` (Windows), `coreml` (macOS). CPU
stays the default in every configuration — the features only compile a
provider in; `DOCLING_RS_EP` selects one at runtime:

| `DOCLING_RS_EP` | behavior |
|---|---|
| unset | `auto` in a build with a CUDA-class (or XNNPACK) feature compiled in (a GPU build should use the GPU); CPU in a default build. CoreML is opt-in (#602): a `coreml` build stays on CPU, and the implicit `auto` leaves CoreML out |
| `cpu` | CPU, byte-for-byte the pre-#74 code path (no EP registered) |
| `cuda` \| `tensorrt` \| `directml` \| `coreml` | that provider, **error-on-failure**: an explicitly requested accelerator that can't initialize fails the conversion instead of silently degrading to a 10×-slower CPU run; requesting one that isn't compiled in warns once and stays on CPU |
| `auto` | every compiled-in provider registered in order TensorRT → CUDA → CoreML → DirectML; ONNX Runtime falls back down the list to CPU at session creation (for images deployed on mixed fleets). CoreML only when `auto` is set by name |

When a GPU provider is selected the model resolution skips the int8 defaults
in favor of fp32 (`decoder_kv.onnx` stays preferred): the int8 exports are
QDQ graphs calibrated for CPU kernels — on GPU they add de-quantize traffic
and their conformance was only ever validated on CPU. An explicit
`DOCLING_*_ONNX` path override still wins over this policy. (Until #602 the
Python bindings set `DOCLING_LAYOUT_ONNX` to the cached int8 graph
themselves, so a source-built GPU wheel ran int8 layout; they now hand the
cache over as `DOCLING_RS_MODELS_DIR` only.)

CoreML specifics (#602, M4 Max, 25-page manual): the `NeuralNetwork` model
format is the default — byte-identical to the CPU provider (3.5 s vs 3.7 s,
~41% GPU) — while `MLProgram` (`DOCLING_RS_COREML_FORMAT=mlprogram`, 2.05 s)
changes the layout detections on 14 of 25 pages and is an opt-in with a
notice; the layout batch stays per-page, since CoreML takes static-shaped
partitions only and a batched session leaves the batch axis free (the old
batch-4 default ran 0% on the GPU, 8.4 s).

Verified without GPU hardware (this is what CI's `ep-features` matrix
covers): default/`cpu`/`auto`/unknown/uncompiled-request configurations all
produce byte-identical corpus output on a CPU-only build; on a
`--features cuda` build with no usable CUDA, `auto` falls back to CPU with
fp32 models selected (output byte-identical to `DOCLING_RS_FP32=1`) and
`DOCLING_RS_EP=cuda` fails loudly at the first session load. In CLI batch
mode (`--input`/`--output`) that first failure aborts the whole batch —
every remaining PDF would fail identically, so they are reported as
`skipped` instead of producing one error line per file.

#### Measured on real hardware (issue #108)

`scripts/test/gpu_benchmark.sh` — every corpus PDF (+ the scanned set)
under `cpu` and `cuda`, best of 3 cold CLI runs each, outputs
byte-compared. Machine: **NVIDIA GeForce RTX 3080 Laptop (16 GB), driver
566.07 · AMD Ryzen 9 5900HX, 16 logical cores** (both providers on the
fp32 models, per the policy above).

**Output equivalence:** 21 of 22 fixtures byte-identical to CPU; one
(`2203.01017v2`, the heaviest layout) differs by 2 markdown lines — fp32
CUDA kernels are not bit-identical to fp32 CPU kernels, so a borderline
detection can flip; groundtruth-distance parity is the standard here, and
byte-parity on 21/22 exceeds it. The entire 2-line diff is one label flip
on one borderline region: the caption fragment
`c. Structure predicted by TableFormer:` comes out as a `list_item`
(`- c. …`) on CPU and as plain `text` on CUDA — same content, same
position, one class score straddling the 0.3 threshold.

**Corpus total (best-of-3): CPU 124.5 s · CUDA 101.2 s → 1.23×** — but the
aggregate hides a clean size split:

| segment | speedup (best) |
|---|---|
| multi-page digital (9–39 pages: arXiv papers, redp5110) | **1.5–2.1×** (`2305.03393v1`: 13.6 s → 7.0 s) |
| mid-size digital (4–5 pages) | 1.1–1.3× |
| 1–2-page digital | 0.75–1.0× — CUDA EP init + host↔device traffic never amortizes |
| scanned/OCR-heavy | 0.65–0.85× — dominated by the page render + OCR pre/post on CPU |

The corpus is small-document-biased; on a genuinely large document the
init noise vanishes and the ONNX stages dominate — that is the regime the
GPU features exist for. The 1913-page .NET C# language reference (same
machine, single cold run each):

| provider | wall time | speedup |
|---|---|---|
| `cpu` | 15 min 13 s (767 % CPU) | — |
| `cuda` | **1 min 45 s** (321 % CPU) | **8.7×** |

Practical guidance: the break-even for a cold CLI run sits around 3–4
pages. Below that, or for OCR-heavy scans, stay on CPU; for batches or
services use the warm `Pipeline` / `docling-serve`, which pays EP
initialization once per process instead of once per file and moves the
break-even to roughly "any document with a table". The cold-vs-best gap on
the CUDA column (~1.5–2.5 s) is that per-process EP initialization made
visible. (Timing methodology: the script reads the monotonic clock —
wall-clock `date` proved able to step backwards under NTP mid-benchmark.)

<details>
<summary>Per-file results (seconds, best of 3; cold = run 1 incl. model/EP init)</summary>

| file | cpu cold | cpu best | cuda cold | cuda best | speedup (best) | output |
|---|---|---|---|---|---|---|
| 2203.01017v2 | 19.93 | 18.13 | 13.31 | 10.38 | 1.75x | 2 diff lines |
| 2206.01062 | 15.34 | 14.84 | 10.86 | 9.70 | 1.53x | identical |
| 2305.03393v1-pg9 | 3.87 | 3.87 | 6.25 | 3.98 | 0.97x | identical |
| 2305.03393v1 | 13.55 | 13.55 | 9.94 | 7.01 | 1.93x | identical |
| amt_handbook_sample | 2.63 | 2.50 | 4.74 | 3.26 | 0.77x | identical |
| code_and_formula | 3.13 | 3.13 | 5.14 | 3.09 | 1.01x | identical |
| multi_page | 5.12 | 5.12 | 6.24 | 4.30 | 1.19x | identical |
| normal_4pages | 7.67 | 6.34 | 7.04 | 4.76 | 1.33x | identical |
| picture_classification | 2.56 | 2.56 | 5.24 | 2.95 | 0.87x | identical |
| redp5110_sampled | 16.53 | 16.53 | 11.73 | 8.05 | 2.05x | identical |
| right_to_left_01 | 1.94 | 1.94 | 5.01 | 2.59 | 0.75x | identical |
| right_to_left_02 | 2.03 | 2.03 | 4.64 | 2.68 | 0.76x | identical |
| right_to_left_03 | 3.26 | 3.26 | 5.95 | 4.09 | 0.80x | identical |
| skipped_1page | 3.62 | 3.38 | 4.79 | 2.96 | 1.14x | identical |
| skipped_2pages | 3.99 | 3.75 | 5.89 | 3.32 | 1.13x | identical |
| table_mislabeled_as_picture | 4.67 | 4.48 | 6.71 | 4.51 | 0.99x | identical |
| nemotron_multipage | 4.83 | 4.76 | 9.16 | 6.18 | 0.77x | identical |
| ocr_test | 2.53 | 2.50 | 5.22 | 3.34 | 0.75x | identical |
| ocr_test_rotated_180 | 2.64 | 2.64 | 4.27 | 3.10 | 0.85x | identical |
| ocr_test_rotated_270 | 2.35 | 2.29 | 3.50 | 3.50 | 0.65x | identical |
| ocr_test_rotated_90 | 2.37 | 2.35 | 5.46 | 3.48 | 0.68x | identical |
| sample_with_rotation_mismatch | 4.54 | 4.54 | 4.74 | 3.92 | 1.16x | identical |

</details>

### Backlog

Landed from the earlier ranked list (each measured above): the int8 layout
model as the CPU default, the KV-cache TableFormer decoder with hoisted
cross-attention (#97) and the dynamic-batch decode of a page's tables, layout
batching in the pool (#73 — dynamic-batch export with the position embedding
folded offline, bit-identical at every batch size; default per-page on CPU,
4 on GPU, #338), the SIMD page downscale (`fast_image_resize`, same
Catmull-Rom kernel, ±1/255 — `DOCLING_RS_SLOW_RESIZE=1` and the conformance
scripts pin the scalar path the snapshots were generated with), per-document
font / form caches and one shared line/word contraction in the text parser,
same-width OCR batching and single-thread OCR lanes.

Still open, none of them large:

* `decode_code` / `decompose_ligatures` allocate a `String` per glyph
  (`textparse.rs`); decompose once at font-parse time and return borrowed
  `&str`.
* The RTL merge is O(n²) (string prepend in `merge_with`, `dp_lines.rs`);
  accumulate reversed and flip once per line.
* Same-width OCR buckets could run across the page-worker pool's idle
  threads (one extra session per worker).
* The orientation probe reads the six widest lines — most of a small scan's
  text; a smaller budget would halve it again but changes the evidence the
  decision is made on.
* Padded OCR batches (PaddleOCR-style) were measured and rejected: padding
  perturbs the valid region's probabilities through the model's
  global-attention blocks and changes the decoded text on 16/20 lines.

### Memory

Each pool worker used to own a full model set, so peak RSS scaled with the
pool: on a 4-worker machine ~0.4 GB of TableFormer weights+arenas were
duplicated four times even though tables appear on a minority of pages. The
pool now shares **one lazily-loaded TableFormer** behind a mutex (loaded with
the full intra-op budget, since tables serialise on it anyway; prediction is
independent of which worker runs it). Measured on the 16-page table-heavy
paper, INT8 stack:

| pool | per-worker TF (before) | shared TF (after) |
|---|---:|---:|
| 4 workers | 3816 MB | **1880 MB** |
| 2 workers | 2183 MB | **1517 MB** |
| 4 workers, table-free doc | 682 MB | **331 MB** (TableFormer never loads) |

`DOCLING_RS_PDF_WORKERS` remains the coarse memory knob on top.

### Determinism note (pre-existing, worth knowing)

Multi-threaded ONNX Runtime float reductions are **not deterministic
run-to-run**: on `2203.01017v2.pdf` two identical invocations of the same
binary can differ in a handful of borderline table cells (measured 0–20
Markdown diff-lines between repeat runs, before any of this branch's
changes). `ocr.rs` already pins its session to one thread for exactly this
reason. Regression checks for structural changes should therefore compare
outputs under `DOCLING_RS_PDF_THREADS=1` (single-thread inference is
deterministic and byte-stable); multi-threaded corpus diffs of a few lines on
table-dense fixtures are thread-scheduling jitter, not necessarily a real
change.

### Reproducing

```bash
scripts/install/download_dependencies.sh
cargo build --release

# stage timing
DOCLING_RS_TIMING=1 ./target/release/docling-rs input.pdf > /dev/null

# build the int8 models (used automatically once present)
uv venv .venv-quant && uv pip install --python .venv-quant/bin/python \
    onnx onnxruntime sympy pypdfium2 pillow numpy
.venv-quant/bin/python scripts/install/quantize_models.py

# force full precision for a run
DOCLING_RS_FP32=1 ./target/release/docling-rs input.pdf > /dev/null
```

Integration points: `scripts/install/download_dependencies.sh` fetches the
pre-quantized assets by default (`--no-int8` skips; published by
`.github/workflows/publish-models.yml`, which quantizes after export);
`scripts/install/pdf_setup.sh` quantizes locally unless `DOCLING_RS_FP32=1`;
`scripts/test/performance.sh` benchmarks whatever the pipeline default resolves to
(int8 when present, `DOCLING_RS_FP32=1` for fp32); `examples/Dockerfile`
bakes both precisions and defaults to int8 (`--build-arg INT8=0` for fp32).
### Where the time goes

For the declarative formats (everything but PDF/images), `cargo run --release
-p docling --example profile_declarative` times parse, Markdown export and
JSON export separately for every file in the corpus and lists the slowest.
Parsing is milliseconds per document and Markdown export is negligible; JSON
export dominates on table-heavy documents because docling's schema repeats
every table cell in the `grid`, and it builds a `serde_json::Value` tree
before printing. The table builder now fills that grid from an index instead
of a hash map of cloned cells: on the corpus's heaviest JSON (a 13 MB patent
export) the export went from 256 ms to 179 ms, on an EBCDIC table dump from
123 ms to 66 ms, byte-identical output. Printing the tree is ~10 % of the
remaining cost; the rest is the `Value` allocation itself, so a further
step would be serializing straight from the document.

For the PDF/image ML pipeline, `scripts/test/profile_pdf.sh` runs the release
binary over the PDF corpus with `DOCLING_RS_TIMING=1` and sums the pipeline's
per-stage wall-clock (`crates/docling-pdf/src/timing.rs`) into one table. On
the 88-page corpus the cost is almost entirely model inference: TableFormer
structure recognition (the autoregressive OTSL decode — ~1400 decode steps
across the corpus's tables) and the per-page layout model together account for
~85 % of it, with a one-time ONNX session/graph init paid on the first table
page. Everything outside the models — both page renders, the two resamples,
text-layer parsing and assembly — was under ~6 % combined on the pdfium
chain; the pure-Rust renderer that replaced it costs more (on the 18-file
corpus of the MarkItDown comparison, 80 pages, the two renders of every page
sum to a third of the CPU time the models take), so since #682 the renders
are scheduled off the model's path rather than before it: a page's 2× bitmap
draws on a helper thread while layout reads its 1× image, the serial path
renders the next page while the worker processes the current one, and the
worker pool — two-thread workers, which the layout model's weak intra-op
scaling makes faster than one four-thread worker — takes every document of
four pages or more. Output is byte-identical; the model cost itself is bounded
by the layout and TableFormer models, so the levers there stay the INT8
models, the KV-cached decoder and GPU execution providers.

Off the model's path is not free, though: #682 still drew every page's 2×
bitmap — the 3× supersample, four times the 1.5× layout render's pixels —
where only TableFormer, picture crops, the code/formula enrichment, page
images and OCR read it, and a page of prose has none of those (#897: render
was 42–55 % of the summed engine time on an M-series machine). Since #897 the
walk schedules that render adaptively (`BitmapRender::Adaptive`): a page's
layout detections decide whether it reads the bitmap, and while most of the
last eight pages did, the render starts during layout as before; while most
did not, it is drawn only for a page that reads it, after layout — and a
render started for a page that then declines it is skipped if the helper has
not begun it. On a 60-page text-only PDF (4-core container) the renders drop
from 120 to 69 and their CPU from ~11 s to ~3.7 s, wall time −6 %; on the
corpus papers (a table or picture on most pages) the render count and wall
time are unchanged. Output is byte-identical in every schedule — the bitmap,
when drawn, is the same one. `DOCLING_RS_PAGE_BITMAP=eager` restores the #682
schedule and `on-demand` draws only the bitmaps read (least CPU; a paper's
pages then wait for their render after layout, +15 % wall there). With
`DOCLING_RS_TIMING=1`, `docling_pdf::timing::snapshot()` / `reset()` read
and clear the per-stage totals per document.

What a TableFormer decode step costs, measured on the same box with
`DOCLING_RS_ORT_PROFILE` (ORT's per-operator profile; see
`scripts/dev/ort_profile_summary.py`) and a per-step timer: 4.4 ms with an
empty cache, 5.7 ms once 100–150 tokens are cached, 6.2 ms past 150 — about
10 µs per cached token — and 6.0 ms for two tables batched. The floor is
memory traffic, not arithmetic: a step streams the decoder's ~90 MB of fp32
weights through 49 single-token GEMMs (40–50 % of kernel time) plus 19 MB of
per-table cross-attention K/V (about a quarter); the cache-dependent part is
the `Split`/`Concat`/`Transpose` copies the exported graph makes of the
growing cache (≈10 % at 85 cached tokens), and the rest is ~330 nodes of
shape plumbing and per-node executor overhead. Three things were measured
not to pay and are not in the tree: ONNX Runtime IO binding for the step
(cross tensors bound once, outputs pre-bound: 5.41 vs 5.34 ms per step,
byte-identical output — the copies are inside the graph, not in the
`Run()` call); a decoder session per pool worker instead of the shared slot
(wall time unchanged on 9-, 18- and 60-page documents, +0.5 GB RSS — two
weight-streaming decodes share one memory bus); and more intra-op threads
(the step does not scale). What would: re-exporting the decoder with
per-layer caches that need no `Split`/`Concat`/`Transpose` (bit-exact in
principle, new model files), narrower weights (INT8/fp16 — a numerics
change), or a GPU execution provider.


## ONNX Runtime 1.31 (#523)

Since ort 2.0.0-rc.14 the default builds — CLI, wheels, npm, containers,
crates — statically link ONNX Runtime **1.31** (pyke's `ms@1.31.0`
binaries; `docling-rs --version` prints the linked version). Measured
against the 1.28 build of the same commit on a 4-vCPU x86-64 box (release,
INT8 layout, `--jobs 1`, same models, same graph cache rebuilt per runtime):

| | ORT 1.28 | ORT 1.31 |
|---|---:|---:|
| 18-PDF corpus (80 pages), wall | 38.9 s | 40.0 s |
| `2206.01062.pdf` (9 pages, 5 tables), wall | 6.9–7.5 s | 8.4 s |
| `redp5110_sampled.pdf` (18 pages, scans + tables), wall | 10.0–10.9 s | 10.1 s |
| `layout.predict`, 9 pages (2 threads) | 3.98 s | 3.80–3.97 s |
| `tf.encoder`, 5 tables | 1.25 s | 1.17 s |
| `tf.bbox`, 5 tables | 1.48 s | 0.47 s |
| `tf.decode_step`, 458 steps | 2.34 s (5.1 ms/step) | 3.0–3.4 s (6.5–7.5 ms/step) |

The one regression is the TableFormer decode step: its 49 single-token
fp32 GEMMs run at ~100 µs each on 1.31 against ~56 µs on 1.28 (ORT's own
per-operator profile, `DOCLING_RS_ORT_PROFILE`; the same 331-node graph,
the same 1-thread session), so a table-heavy page is ~10–15 % slower and
the corpus ~3 %. 2 intra-op threads recover about half of it at the
price of run-to-run determinism, so the session stays single-threaded.
The bbox head got three times faster and layout, the encoder and OCR are
unchanged.

Output: the 18-PDF corpus converts to byte-identical Markdown on both
runtimes; `--to json` differs in one file (`redp5110_sampled`): two table
cells' box edges move by 0.01 pt, two table regions' edges by 1.2–1.5 pt,
and one picture crop is 1 px wider — the float-accumulation noise of a
different GEMM kernel, below every tolerance the DocLang comparison uses.
`scripts/conformance/pdf_groundtruth.sh` and `pdf_conformance.sh` report
the same files with the same line counts on both runtimes (9/18 strict and
74/98 snapshots exact with the pure-Rust renderer — the committed
snapshots are pinned to the docling-parse renderer, that drift predates
the bump and is not the runtime's). The picture classifier runs fully
optimized (the #517 cap only fires under `ort-load-dynamic` with an older
library) and matches docling on the enrichment fixtures; the ASR tests
pass on the Whisper tiny fixtures (the Parakeet int8 preset was not
re-measured here).
