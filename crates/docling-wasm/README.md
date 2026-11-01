# docling-wasm — in-browser document conversion

A `wasm32-unknown-unknown` build of docling.rs's **declarative** converters
(issue #79): DOCX, HTML, Markdown, Jupyter notebooks, XLSX, PPTX, CSV,
AsciiDoc, EPUB, ODF, WebVTT, Email, MHTML, JATS, USPTO, XBRL, LaTeX, JSON,
DocLang, plain text and source code — and the
**embedded text layer of PDFs** — converted to Markdown / docling JSON /
DocLang XML **entirely client-side** — no server, the file never leaves the
page. Python docling cannot do this.

The crate is `docling` with `default-features = false` plus `pdf-text`:
digital PDFs convert through docling-pdf's pure-Rust content-stream parser —
the exact extraction the native `--text-layer-only` flag does (flat, line-grouped
paragraphs in reading order; no headings/lists/tables/pictures, since those
need the layout model). The ML pipelines (pdfium + ONNX Runtime) and the HTTP
image fetcher are compiled out: scanned/image-only PDFs get a clear "no
embedded text layer … OCR needs a build with the `pdf` feature" error, and
images/audio/METS are rejected with a "rebuild with …" message. Remote
`<img src>` images stay placeholders (no network in the module); embedded
images work normally. Native builds are untouched: the feature slices behind
this (`pdf` / `asr` / `fetch-images`) all stay in the `docling` default set,
so a plain `cargo build` is unchanged.

**Size: ~11 MB raw, ~3.4 MB gzipped** (measured at 0.49.x — the module now
also carries the browser ML pipeline's pre/post-processing: layout decode, OCR
prep, TableFormer core; `--release` with the workspace's `lto = "thin"`, no
`wasm-opt` pass — one typically shaves another 10–15%). No models are involved
for the declarative converters and the PDF text parser: pure Rust.

## Install from npm

```bash
npm i docling.rs-wasm
```

```js
// Bundlers (Vite, webpack 5 with experiments.asyncWebAssembly):
import { convert } from "docling.rs-wasm";

// No bundler (plain <script type="module">): the /web target + explicit init.
import init, { convert } from "docling.rs-wasm/web";
await init();
```

The package ships both wasm-bindgen targets — `bundler` (the default export)
and `web` (self-initializing, fetches `docling_wasm_bg.wasm` next to the JS).
It is assembled by `scripts/ci/build_wasm_pkg.sh` and published by the
`npm publish` workflow alongside [`docling.rs`](https://www.npmjs.com/package/docling.rs)
(the native Node bindings — prefer those on servers: full ML pipeline, no wasm
limits).

## API

```ts
convert(
  bytes: Uint8Array,
  filename: string,
  to?: "md" | "json" | "doclang" | "latex" | "html" | "pandoc", // default "md"
  images?: "placeholder" | "embedded",     // default "placeholder"; Markdown, HTML, Pandoc
  max_pages?: number,                      // convert only the first N PDF pages
  page_break_placeholder?: string,         // text between pages in Markdown, e.g. "<!-- page break -->"
): string
convert_with_options(            // #577: the same options as the CLI / serve / C ABI, one JSON object
  bytes: Uint8Array,
  filename: string,
  options_json?: string,                   // JSON: { to, images, ...docling ConvertOptions }
): string
supported_extensions(): string   // JSON array, e.g. for <input accept=…>
version(): string

// With the default-on `ocr` feature (see the browser-OCR sections below):
new DigitalConverter()                       // reusable digital-PDF converter
ocr_image(...)                               // recognize one raster image
new ScannedConverter() / convert_scanned_image(...)  // scanned-page pipeline
```

The filename's extension drives format detection, same as the CLI. `images`
mirrors docling-serve's option: `placeholder` emits docling's `<!-- image -->`,
`embedded` inlines the picture as a `data:` URI so the Markdown is
self-contained (a page has no filesystem, so there is no `referenced` mode).

`convert_with_options` takes one JSON object instead of positional arguments:
`to` and `images` as above, plus every key of the shared
[`docling::ConvertOptions`](../docling/src/options.rs) — `strict`,
`compact_tables`, `page_break_placeholder`, `pages` (`"A-B"`), `encoding`,
`skip_empty_cells`, `list_attachments`, `xbrl_taxonomy`, … (the full table is
[`docs/OPTIONS.md`](../../docs/OPTIONS.md)). The ML-pipeline options are
accepted and inert in this build; an unknown key is an error.

```js
const md = convert_with_options(bytes, file.name, JSON.stringify({ strict: true, pages: "1-3" }));
```

### Convert a file the user picked

```js
import init, { convert } from "./pkg/docling_wasm.js";
await init();

const file = input.files[0];
const bytes = new Uint8Array(await file.arrayBuffer());
const markdown = convert(bytes, file.name, "md");
const json     = convert(bytes, file.name, "json");
const withPics = convert(bytes, file.name, "md", "embedded");
```

### Digital PDFs with structure (no OCR)

A PDF that carries a text layer needs no recognition at all — but headings,
tables and pictures are things the *layout* model finds, so the pure text-layer
path (`convert`) can only emit flat paragraphs. `DigitalConverter` closes that
gap: the text comes out of the file (exact, no recognition errors) and only the
layout model runs over the rendered pages. It is the same thing the native
pipeline does, which OCRs a page only when it has no text cells.

```js
const conv = new DigitalConverter(pdfBytes);   // throws when there is no text layer
conv.setDict(dictText);                        // optional: enables picture OCR below
for (let i = 0; i < conv.page_count(); i++) {
  // rasterize page i with pdf.js at 2 px/point, then:
  await conv.add_page(i, rgba, w, h, 2.0, layout, rec); // rec optional
}
const markdown = conv.finish("bill.pdf", "md", "embedded");
```

`addPageTf(..., tf, rec)` adds TableFormer, still only for the tables whose
geometric reconstruction looks unreliable.

With a dictionary (`setDict`, or the constructor's second argument) and a
recognition session, embedded raster pictures that carry no text cells are
OCR'd too — Python docling's `bitmap_area_threshold` behavior: a digital page's
images (a terms-and-conditions box exported as a bitmap) hold text its text
layer cannot see. Without them the path fetches no recognition model and keeps
its old text-layer-only behavior.

### Scanned pages (OCR)

Scanned PDFs and images need the ML models; everything else runs with no
network at all. The full wiring — model resolution, Web Worker, pdf.js
rasterization — is [`www/index.html`](./www/index.html); the short version:

```js
import { createOcr } from "./pipeline.js";

const ocr = createOcr({ onStatus: (msg) => console.log(msg) });
await ocr.boot();                        // wasm + layout model

// A standalone image is its own page.
const md = await ocr.convertImage(await file.arrayBuffer(), file.name, "en", "md");

// A scanned PDF: feed rasterized pages in order (2 px/point = the native
// pipeline's RENDER_SCALE), then finish.
await ocr.startDoc("en", /* TableFormer */ false);
for (const page of pages) await ocr.addPage(page.rgba, page.w, page.h, 2.0);
const doc = ocr.finishDoc(file.name, "md");
```

Scanned pictures are cropped out of the rendered page just like the native
pipeline, so `images: "embedded"` inlines real figure bytes on this path too
(`ocr.finishDoc(name, "md", "embedded")`).

Models resolve **device file → local `./.models/` → Hugging Face**, so a page can
ship with no models and still work: `ocr.setProvidedModels({ "layout_heron_int8.onnx": buf })`
takes files the user picked, and anything not provided is fetched.

## Build

```bash
rustup target add wasm32-unknown-unknown

# Either wasm-pack (bundles the JS glue + package.json):
wasm-pack build crates/docling-wasm --target web

# ...or plain cargo + wasm-bindgen (what CI and the demo below use):
cargo build -p docling-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir crates/docling-wasm/www/pkg \
    target/wasm32-unknown-unknown/release/docling_wasm.wasm
```

## Demo

**Live: <https://docling-project.github.io/docling.rs/>** — deployed from
`www/` by [`.github/workflows/pages.yml`](../../.github/workflows/pages.yml) on
every push to `master`. No models are published with it, so the declarative
converters and text-layer PDFs work straight away and OCR starts once you give
the page models (device picker or Hugging Face — see below).

[`www/index.html`](./www/index.html) is the whole thing on one page: drop a
file, pick the output (Markdown / JSON / DocLang / LaTeX / Pandoc AST), pick how images render, and
optionally turn on OCR for scanned pages. A **Force OCR** toggle (docling's
`force_full_page_ocr`) sends a PDF straight to the OCR pipeline, ignoring
whatever text layer it claims to have — for layers that exist but lie. A
**Pages** field converts only the first N pages (empty = all), and **Stop**
aborts a running conversion and clears the output — the OCR worker is
terminated and boots afresh on the next file. **GPU (WebGPU)** (opt-in,
#629) runs the heavy models on the GPU where the browser has WebGPU — see
[Performance & threading](#performance--threading). To run it locally, after the
`wasm-bindgen` step above:

```bash
python3 -m http.server -d crates/docling-wasm/www 8901
# open http://127.0.0.1:8901/
```

It is a plain static page — copy `www/` behind any web server (or into a mobile
app's webview) and it works as-is. The OCR half loads lazily, so a visitor who
only converts a DOCX never downloads ONNX Runtime, pdf.js or any model. PDFs
try their text layer first and fall back to OCR only when there isn't one.

Verified end-to-end in headless Chromium: Markdown/DOCX→md, DOCX→JSON, a
corpus PDF→md through the text-layer path, and the scanned-PDF error path all
exercised through the real wasm module.

## In-browser ML pipeline — OCR, layout, TableFormer (#157)

With the default `ocr` feature the module runs the **full scanned-document ML
pipeline** client-side. Every ONNX-free step — image preprocessing, line/word
segmentation, CTC decoding, RT-DETR box decoding, the TableFormer
autoregressive loop and its bbox bookkeeping, span merging, OTSL→grid layout,
cell matching, region refinement and reading-order assembly — runs in Rust,
**the exact same functions the native pipeline uses**
(`docling_pdf::{ocr_prep, layout, scanned, tf_core, tf_match, resample}`). Only
the four ONNX graphs are delegated to
[ONNX Runtime Web](https://onnxruntime.ai/docs/tutorials/web/) via small JS
wrappers around `ort.InferenceSession`. One implementation is what keeps the
wasm output matching native — drift can only come from the runtime kernels.

| Stage | What | Models (from the [`models-v1` release](https://github.com/docling-project/docling.rs/releases/tag/models-v1) — pin a build to one of its immutable `models-v1-<id>` snapshots, below) |
|---|---|---|
| **1** — `ocr_image` | OCR a single scanned **image** | PP-OCRv3 recognition (~10 MB) |
| **2** — `ScannedConverter` / `convert_scanned_image` | Scanned **PDF/image** → Markdown: layout + OCR + reading order, tables via geometric reconstruction | + RT-DETR layout (`layout_heron_int8.onnx`, ~68 MB) |
| **3** — `ScannedConverter.addPageTf` | Real **TableFormer** table structure, on the tables that need it | + `tableformer/{encoder,decoder_kv,bbox}.onnx` (~260 MB; ~380 MB with the pre-#374 encoder) |

**Pin the model bytes to a snapshot, not to `models-v1`.** `models-v1` is a
rolling release: every `publish-models.yml` run re-uploads its assets, and
a re-export of the same weights is not byte-identical (the fp32 graph's
folded BatchNorm scales differ in their last bits between runs, and the INT8
calibration infers on whichever CPU the runner has), which breaks a build
that checks the file's sha256 (#965). Every published
state is frozen into an immutable prerelease named
`models-v1-<12 hex of sha256(SHA256SUMS)>`; fetch from that URL and the
digest you pinned holds forever:

```js
// the snapshot's SHA256SUMS lists every asset's digest
const MODELS = "https://github.com/docling-project/docling.rs/releases/download/models-v1-df257987d1fc";
const layout = await fetch(`${MODELS}/layout_heron_int8.onnx`);
```

The snapshot ids are on the [Releases page](https://github.com/docling-project/docling.rs/releases)
(prereleases); `docs/MODELS.md` "Pinning and snapshots" has the native
downloaders' equivalent (`DOCLING_RS_MODELS_URL` + `DOCLING_RS_MODELS_SUMS`).

Stage 3 is *selective*. TableFormer's encoder runs once per table region and
costs seconds, so each table is first reconstructed geometrically (free, from
the OCR cell positions) and the model is invoked only when that grid looks
unreliable — `docling_pdf::assemble::geometric_table_is_reliable`. The tell is
how `reconstruct_table` derives columns: it clusters cell *left edges*, which is
exact on a clean grid but splits one real column into several when entries are
not left-aligned, leaving a wide, mostly-empty table. So a grid is trusted only
when it is dense (≥60% of cells carry text) and has no column that just one row
uses; anything else goes to TableFormer. Well-formed tables therefore cost
nothing extra, and the pages that used to show spurious empty columns still get
the model.

Stage 1 recognition wrapper:

```js
const rec = {
  run: async (n, h, w, data) => {
    const out = await session.run({ [session.inputNames[0]]:
      new ort.Tensor("float32", data, [n, 3, h, w]) });
    const t = out[session.outputNames[0]];
    return { data: t.data, dims: Array.from(t.dims) };
  },
};
const markdown = await ocr_image(imageBytes, dictText, rec, "md");
```

Stage 2 rasterizes PDF pages with pdf.js at `{scale: 2}` (2 px/point — the
native `RENDER_SCALE`) and feeds bitmaps page by page:

```js
const conv = new ScannedConverter(dictText);
for (let p = 1; p <= pdf.numPages; p++) {
  const viewport = page.getViewport({ scale: 2 });
  // … render to canvas, then:
  await conv.add_page(rgba, canvas.width, canvas.height, 2.0, layout, rec);
}
const markdown = conv.finish(file.name, "md");
```

Stage 3 is the same but `conv.addPageTf(rgba, w, h, 2.0, layout, rec, tf)`,
where `tf` is a stateful session over the three TableFormer graphs (the heavy
cross-attention K/V and the growing decoder KV-cache stay on the JS side, so
each decode step marshals only the last tag and gets back logits+hidden — see
[`www/pipeline.js`](./www/pipeline.js)'s `JsTfSession`).
[`www/index.html`](./www/index.html) wires all three stages up behind the OCR
language selector, the TableFormer toggle and the model picker.

## Run it on your phone

The demo is a static page — the models are the only heavy part, and they load
same-origin, from a CORS host, **or straight from files on your device**.

1. **Fetch the models** (once), on a desktop or the phone:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh
   ```
   This writes `.models/` (layout, OCR, TableFormer, …).

2. **Serve the page** from any static host that sends the right MIME types —
   GitHub Pages, `raw.githack.com`, or a local `python3 -m http.server -d
   crates/docling-wasm/www 8901`. (Plain `raw.githubusercontent.com` and most
   GitHub CDNs serve HTML as `text/plain`, so the page won't render — use
   Pages or githack.) Build the wasm `pkg/` first (see **Build** above).

3. **Provide the models to the page**, either:
   - **from your device** — tap the model picker under *"Scanned PDFs and
     images"* and select `layout_heron_int8.onnx` and, for TableFormer,
     `encoder.onnx`, `decoder_kv.onnx` (+`.data`), `bbox.onnx` (+`.data`). Each
     file is read to an `ArrayBuffer` on the client and used directly — no
     download, and a single allocation per file (a 100+ MB encoder over
     `fetch` can OOM a mobile tab; a `File` read does not); or
   - **from a CORS mirror** — a Hugging Face model repo serves
     `Access-Control-Allow-Origin` (GitHub Release assets do not); point
     `MODEL_BASE` in `pipeline.js` at yours. The recognition model already
     streams from Hugging Face.

Resolution order for every model is **device file → local `./.models/` →
`MODEL_BASE` (Hugging Face)**. Tables need no upload beyond the model files;
the image never leaves the page.

## Tauri and other desktop shells

Two ways to put docling.rs in a desktop app, and they are not equivalent:

**Native backend (recommended for Tauri).** A Tauri app's backend *is* Rust,
so skip wasm entirely and depend on the real crates — the webview only renders
the result. This gets the full native pipeline: pdfium rendering, the complete
ML stack (RT-DETR + TableFormer + OCR + enrichment), GPU execution providers,
multi-threading, memory-mapped models — none of which fit the wasm build.

```toml
# src-tauri/Cargo.toml
[dependencies]
docling = { version = "0.53" }        # full default features (ml pipeline)
```

```rust
// src-tauri/src/lib.rs
use docling::{DocumentConverter, SourceDocument};

#[tauri::command]
async fn convert_document(path: String) -> Result<String, String> {
    // The ML pipeline is CPU-heavy — keep it off the event loop.
    tauri::async_runtime::spawn_blocking(move || {
        let source = SourceDocument::from_file(&path).map_err(|e| e.to_string())?;
        let result = DocumentConverter::new()
            .convert(source)
            .map_err(|e| e.to_string())?;
        Ok(result.document.export_to_markdown())
    })
    .await
    .map_err(|e| e.to_string())?
}
```

Ship the `.models/` directory as a Tauri resource (or fetch on first run with
`scripts/install/download_dependencies.sh`'s URLs) and point the resolver at
it — model paths resolve CWD-relative with env overrides
(`PDFIUM_DYNAMIC_LIB_PATH`, `DOCLING_LAYOUT_ONNX`, …), so set the resource dir
as the working directory or export the variables before the first conversion.

**Wasm in the webview.** `docling.rs-wasm` runs unchanged inside Tauri's (or
Electron's) webview — worth it only when the same SPA must also ship as a
plain web page and you want one code path. The wasm limits apply (declarative
formats + PDF text layer; browser ML needs ONNX Runtime Web + models). One
Tauri-specific note: multi-threaded ONNX Runtime Web needs COOP/COEP headers —
on GitHub Pages the demo fakes them with a service worker (`www/coi.js`), but
in Tauri you control the protocol, so serve the app with
`Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` directly (e.g. via the
`app.security.headers` config or a custom protocol handler) and drop the
service-worker shim.

**Electron.** Same fork: prefer the native
[`docling.rs`](https://www.npmjs.com/package/docling.rs) N-API package in the
main process (`ipcMain.handle("convert", …)`), keep `docling.rs-wasm` for
renderer-only/sandboxed setups.

## Performance & threading

- **Threads.** ONNX Runtime Web runs single-threaded unless the page is
  [cross-origin isolated](https://web.dev/coop-coep/) (SharedArrayBuffer). A
  static host can't set the COOP/COEP headers, so [`www/coi.js`](./www/coi.js)
  is a service worker that adds them (`COEP: credentialless`, which keeps the
  CDN/HF fetches working); `pipeline.js` then sets
  `ort.env.wasm.numThreads` to the core count. The demo's ready line reports
  the thread count — if it says `1 thread`, isolation didn't take (some mobile
  browsers), and everything runs serially.
- **int8.** The layout model is the conv-only static-INT8 export (~68 MB,
  ~2.4× faster than fp32 at unchanged conformance). The TableFormer **encoder
  is fp32 (~103 MB since #374 stripped the exporter's six baked zero
  attention masks — the published 225 MB file was half `x + 0`; the stripped
  graph is bit-identical; the native pipeline additionally prefers the 54 MB
  `encoder_fp16.onnx` repack, byte-identical output on the snapshot corpus)
  and runs once per table region** — it dominates wall time
  on mobile (a multi-table page can take minutes). An int8 encoder is *not* the
  easy win it looks like: it's a ResNet backbone (~20 Conv) feeding a 6-layer
  transformer, and the transformer Gemms — not the convs — are the cost.
  Measured on a native x86 build over 15 real table crops: conv-only static
  QDQ keeps fidelity (enc_out/cross cosine ≥ 0.995) but is *not* faster than
  ORT's fp32 conv kernels, and quantizing the attention MatMuls collapses
  cross-attention fidelity to ~0.85 (garbled structure) for no speed gain. So
  the encoder stays fp32; the real mobile levers are running heavy tables on a
  desktop and batching multi-table pages (below), not weight quantization.
- **Where to run heavy tables.** TableFormer's ~260 MB of models and per-region
  encode make a desktop (more cores, more memory) far faster than a phone; the
  geometric table path (stage 2, no TableFormer) already captures all cell text
  and is the light option for mobile.
- **GPU (WebGPU), opt-in (#629).** The demo's *GPU (WebGPU)* box — or
  `?webgpu` in the URL — runs the layout model, the text detector and the
  TableFormer encoder on ONNX Runtime Web's native WebGPU execution provider;
  recognition and the TableFormer decoder/bbox stay on wasm. No Rust changes:
  the wasm module only sees the JS session objects, so this is
  `pipeline.js` choosing an EP per model. What the probe behind it found, in
  headless Chromium on a software WebGPU adapter, on real pages:
  - **Same output.** Recognition, detector, TableFormer and the fp32 layout
    model match the wasm EP (identical text, detection maps, detections and
    structure tags); the demo's Markdown is byte-identical between the GPU
    path and the CPU path with the same (fp32) layout model. It is not the
    default CPU output on every page: that one comes from the int8 layout
    model, whose detections differ from fp32's here and there (on the QR-bill
    scan each wins a region — fp32 pulls a closing sentence into the table,
    int8 merges the e-mail/web lines). Python docling runs the fp32 model.
  - **fp32 layout only.** The int8 layout graph runs on WebGPU but diverges
    (max logit diff 5–7, most detections relabelled) — CPU-calibrated QDQ,
    the same reason the native pipeline uses fp32 on GPU providers (#74). So
    the GPU path loads `layout_heron.onnx` (~172 MB vs ~69 MB); with only
    the int8 file present, layout stays on wasm.
  - **Native EP, not JSEP.** The page now loads a pinned ORT build — the
    wasm-only `ort.wasm.min.mjs` by default (a 3.5 MB gzipped runtime instead
    of the 6.3 MB the unpinned JSEP `ort.min.mjs` was; byte-identical output),
    `ort.webgpu.min.mjs` when WebGPU is on and an adapter exists. JSEP is
    deprecated since ORT 1.29, and its AveragePool lacks `ceil_mode`, so the
    heron layout model fails on it outright.
  - **Fallbacks.** No WebGPU in the browser (Firefox on Linux, say) or no
    adapter → CPU, said so on the ready line; a model whose WebGPU session
    will not start → that model on wasm. WebGPU works in the dedicated
    worker the pipeline runs in.
  - **Speed: measure it.** A software adapter says nothing about real GPUs,
    which is why the GPU path is opt-in. After every OCR run the status line
    lists the inference time per model and where it ran (`layout 1.2 s GPU,
    rec 0.8 s, …`) — toggle the box and compare on the device. Compare the
    second conversion after a toggle: the first one also pays the shader
    compiles. `?webgpu=layout,det,rec,tf_enc,tf_dec,tf_bbox` (or `all`)
    overrides which models go to the GPU — the defaults keep the small,
    shape-varying recognition batches and the per-token decoder loop on
    wasm, where GPU dispatch is expected to cost more than it saves.

## Host-side tests

`cargo test -p docling-wasm` runs the conversion body natively (the
`JsError` boundary only exists on wasm), including a real corpus DOCX and
PDF.
