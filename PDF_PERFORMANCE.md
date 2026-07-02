# PDF pipeline — performance review & profiling notes

Post-migration review of the PDF processing path: where the time actually goes,
what was measured, which optimizations are validated, and a ranked backlog of
further ideas that do **not** trade away output quality.

Measured on a 4-core AVX-512(+VNNI/AMX) Xeon, release build (`lto = "thin"`),
models from `scripts/download_dependencies.sh`, `FLEISCHWOLF_TIMING=1`.

## Where the time goes

Per-stage wall-clock share (summed across workers):

| Stage | 1913-page text-heavy PDF¹ | 16-page table-heavy paper² | scanned page³ |
|---|---:|---:|---:|
| `layout.predict` (RT-DETR ONNX) | **80.3%** | 55.4% | 64.9% |
| `image.resize` (3×→2× CatmullRom) | 14.9% | 7.9% | 18.5% |
| `tableformer` | 2.8% | 32.1% | — |
| `pdfium.render` | 1.8% | 3.7% | 16.5% |
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
16-page document (11.6 s vs 12.2 s vs 15.6 s).

## Validated win: INT8 quantization (quality-checked)

`scripts/quantize_models.py` produces two quantized models. Point
`DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_DECODER` at them to opt in.

### Layout: static QDQ INT8, **Conv ops only** (~2.4× faster layout)

Calibrated on 42 real corpus pages preprocessed exactly like
`layout.rs::predict`. Only the HGNetv2 backbone convolutions are quantized;
the transformer decoder and detection-head MatMuls stay fp32.

| Configuration | layout.predict (16-page doc) | end-to-end wall | model size |
|---|---:|---:|---:|
| fp32 baseline | 17.2 s | 16.6 s | 172 MB |
| **INT8 conv-only** | **7.2 s (2.4×)** | 11.5 s (1.45×) | 68 MB |
| + INT8 TableFormer decoder | — | **12.3 s → see note** | — |

On text-dominated documents (layout = 80% of time) the end-to-end gain
approaches ~1.7–2×; on table-heavy ones it is ~1.4×.

**Quality gate.** Markdown diffed across the full PDF+scanned corpus (23 files):

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

### TableFormer decoder: dynamic INT8 (~10% faster tables, byte-identical)

The autoregressive tag decoder is MatMul-only; weights-only dynamic INT8
produced **byte-identical corpus output** and ~10% faster table decode
(784 → 695 ms/table), 78 → 50 MB. Small but free.

The decoder speed is *not* weight-bound — it is per-step overhead (see backlog
item 2), which is why quantization helps so little there.

## Ranked backlog of further ideas

Ordered by expected impact ÷ risk. Items 1–3 attack the 85–95%.

1. **Ship/document the INT8 layout model as the default CPU configuration**
   (done here as an opt-in; consider making `download_dependencies.sh` fetch a
   pre-quantized `layout_heron_int8.onnx` so users don't need the Python
   tooling). Biggest single validated win: ~1.4–2× end-to-end.
2. **TableFormer decode-loop overhead** (~800 ms/table, ~60–500 steps):
   - `decode_step` copies the whole KV cache out (`ocache.to_vec()`) and back
     in every step — O(steps²·6·512) float traffic. `ort` can feed a session
     *output* `Value` directly as the next run's input; keeping the cache as a
     `Value` avoids both copies.
   - The exported graph re-embeds the **full tag sequence** every step
     (`tags` grows each iteration) even though attention is KV-cached. Re-export
     the decoder to take only the last tag (the cache carries the history) —
     docling's own export keeps this shape; worth checking
     `scripts/export_tableformer.py`.
   - Batch the bbox decoder input copies (`enc.eo.clone()` per table is a full
     encoder-output copy; a `TensorRef` view suffices).
3. **Layout batching for the parallel path**: the pool currently runs batch-1
   inference per page. RT-DETR's 640×640 input is fixed-shape, so pages can be
   batched (e.g. batch-4) per worker with one session — better core utilization
   and less framework overhead on wide machines. Output is per-image, so
   quality is unaffected. (Needs a re-export with a dynamic batch dim.)
4. **Render → resize pipeline copies** (`pdfium_backend.rs:264-272`, ~15% on
   text-heavy docs): pdfium's BGRA bitmap goes through `as_image()` (copy +
   swizzle) then `.into_rgb8()` (second copy) before the 3×→2× CatmullRom
   downsample (third buffer). A single BGRA→RGB pass into the resize source
   removes one full-page copy + traversal per page. Keep the 1.5× supersample +
   CatmullRom itself — it is deliberate PIL-BICUBIC parity for model input.
   Also: when `layout.predict` is the only image consumer at 640×640, the
   2×-page intermediate is only needed for TableFormer crops and OCR — a page
   with no table regions and a text layer never needs it; rendering could be
   deferred/skipped per page in a `no_ocr`-like fast path decided *after*
   layout runs on a cheaper raster.
5. **textparse font caching** (marginal for PDFs — textparse is ≤1% — but
   real for `no_ocr` mode where it becomes the bottleneck):
   - fonts are fully re-parsed (ToUnicode CMap decompression + tokenization,
     Type1 program scan, width maps) for **every page** and every Form-XObject
     invocation (`textparse.rs:794`); cache parsed `Font`s per document keyed
     by the font dict's `ObjectId`, and cache decoded Form XObject content.
   - `line_cells` + `word_cells` run the identical build+contract twice per
     page (`textparse.rs:705-709`); one pass can emit both.
   - `decode_code`/`decompose_ligatures` allocate a `String` per glyph
     (`textparse.rs:94-145`); decompose once at font-parse time and return
     borrowed `&str`.
   - RTL merge is O(n²) (string prepend + `Vec::remove(0)`,
     `dp_lines.rs:87-155`); accumulate reversed and flip once per line.
6. **OCR line batching** (`ocr.rs::recognize`): lines are recognized one at a
   time on one thread (deliberately, for CTC determinism). Batching same-width
   buckets keeps determinism per line and would speed scanned documents
   several-fold; alternatively run multiple single-thread recognitions across
   the existing worker pool.
7. **ort session options**: sessions use default graph optimization; worth
   setting `GraphOptimizationLevel::Level3` explicitly plus
   `with_optimized_model_path` to cache the optimized graph across loads (saves
   a chunk of the per-worker model-load latency the pool pays on first use).

## Correctness notes found during review (quality, not speed)

- `textparse.rs` `"` operator: the `aw ac string "` form must set word/char
  spacing (`tw`/`tc`) from its first two operands before showing the string;
  they are currently ignored (`Tj | ' | "` share one arm), so documents using
  `"` get wrong inter-word advances. **Fixed in this branch.**
- `textparse.rs::page_size` ignores a non-zero MediaBox origin; a page with
  e.g. `[9 9 621 801]` offsets all parser cells relative to pdfium's raster.
  Rare, but cheap to guard: subtract the box origin when emitting glyph boxes.
- OCR recognition ran un-instrumented; `ocr.page` is now a timed stage (this
  branch), so scanned-corpus profiles attribute it correctly.

## Reproducing

```bash
scripts/download_dependencies.sh
cargo build --release

# stage timing
FLEISCHWOLF_TIMING=1 ./target/release/fleischwolf input.pdf > /dev/null

# quantize + opt in
uv venv .venv-quant && uv pip install --python .venv-quant/bin/python \
    onnx onnxruntime sympy pypdfium2 pillow numpy
.venv-quant/bin/python scripts/quantize_models.py
export DOCLING_LAYOUT_ONNX=$PWD/models/layout_heron_int8.onnx
export DOCLING_TABLEFORMER_DECODER=$PWD/models/tableformer/decoder_int8.onnx
```
