# CLAUDE.md — working notes for AI-assisted sessions

Rust port of [docling](https://github.com/docling-project/docling): document
conversion (PDF/Office/HTML/audio/video/…) to Markdown / docling-JSON / DCLX,
measured against upstream Python docling as the reference — not bound to
reproduce it byte for byte (see "Conformance & fixtures").

## Workflow rules

- **Every commit must be signed off by the author.** End each commit message
  with `Signed-off-by: name <email>`.
- **Releases are automatic on merge; only the `fix:` prefix matters.** CI
  (`scripts/ci/bump_version.sh`): a merge whose commits are all
  `fix:`/`perf:`/`revert:` cuts a patch (0.49.0 → 0.49.1); **any other
  commit** — whatever its prefix — bumps the 0.x "major" (0.49.0 → 0.50.0).
  So prefix bug fixes `fix(scope): …` and don't worry about the rest.
  Docs/CI-only merges still release nothing (release.sh only fires when a
  publishable crate's source changed — the bindings included: docling-py's
  `python/` and `src/`, docling-node's, docling-wasm's, docling-ffi's, since
  PyPI / npm / the binaries publish them from the tag). No automatic semver-major: majors are
  cut by hand (`force_version` dispatch input) — v1.0.0 marks the breaking
  `.models/` asset-dir rename. ci.yml bumps the version and tags it, builds
  nothing and cuts no GitHub Release; **everything is published by hand**,
  for a tag the maintainer picks: `gh workflow run release.yml` (the newest
  `vX.Y.Z` tag; `-f tag=vX.Y.Z` for another) first cuts the GitHub Release
  for the tag (notes from the tags, `scripts/ci/release_notes.sh` — the
  Releases page thus lists what was published, not every merge), then
  dispatches `crates-publish.yml` (the crates, in dependency
  order), `pypi-publish.yml` (`docling-rs` + `docling-rs-cuda`,
  the `docling-rs-mcp` launcher, then the MCP catalogs), `npm-publish.yml`
  (`docling.rs`, the platform packages, `docling.rs-cuda`,
  `docling.rs-wasm`), `docker-publish.yml` (GHCR) and `cli-binaries.yml` (the
  release's CLI/FFI assets), waits for them and marks the release latest
  (install.sh takes the newest release that has its binary; the asset-only
  `npm-cuda-v*` / `models-v*` releases are created `--latest=false`, #707).
  Each also runs alone (`-f
  tag=vX.Y.Z`) to repair a failed channel. The release job also comments
  "Shipped in vX.Y.Z" on every issue the release's commits and merged PRs
  reference (`scripts/ci/release_issue_comments.sh`: `Closes/Refs #NN` in
  commit messages and PR descriptions, GitHub's own PR→issue links) — one
  more reason to put `Closes #NN` / `Refs #NN` in the commit message. A release per merge outgrew PyPI's
  10 GiB per-project limit (v2.22.0 failed half-way);
  `scripts/ci/pypi_room.py` refuses an upload that would not fit.
- Claude Web: **Never open pull requests on `artiz/docling.rs`.** Push a `claude/<topic>`
  branch and hand back a compare link
  (`https://github.com/docling-project/docling.rs/compare/master...artiz:docling.rs:<branch>?expand=1`);
  the maintainer opens/merges PRs themself (usually into the upstream
  `docling-project/docling.rs`; `artiz/docling.rs` is their working fork).
- One feature = one branch off fresh `origin/master`. Don't stack unrelated
  work.
- Issue numbers (`#80`, `#138`, …) refer to `docling-project/docling.rs`
  issues; reference them in commit messages — `Closes #NN` when the
  change fully resolves the ticket (GitHub then auto-closes it on merge),
  `Refs #NN` for partial/related work.
- **Slack notifications** (when the Slack MCP connector is available): post to
  **#claude-code** (channel ID `C0BKAHN0BSM`) when (a) a question blocks the
  work and needs the maintainer's answer, (b) a long task finishes — include
  the outcome and the compare link, (c) idle/bored — the queue is empty;
  suggest what to pick up next. Keep it to these events; don't narrate
  routine progress there.

## Workspace map

| Crate | What it is |
| --- | --- |
| `crates/docling-core` | `DoclingDocument` model, Markdown/JSON/DCLX serializers, `MarkdownStreamer`, chunkers; `tree::ItemTree` — docling's item tree a backend can hand the JSON export when the flat nodes cannot express upstream's structure (HTML via `html_tree.rs`, DOCX via `docx_tree.rs`) |
| `crates/docling` | `DocumentConverter` (format routing), declarative backends (`src/backend/`), streaming (`src/stream.rs`), video (`src/video.rs`) |
| `crates/docling-pdf` | ML pipeline: lopdf object model + pure-Rust page renderer (`render/`) / raster (`raster/`) + RT-DETR layout + TableFormer + PP-OCRv3 + enrichment (`ml` feature; pdfium only behind the opt-in `pdfium` feature); pure-Rust text-layer path compiles for wasm without it |
| `crates/docling-onnx` | Shared ONNX Runtime execution-provider selection (`DOCLING_RS_EP`, `cuda`/`tensorrt`/`directml`/`coreml`/`xnnpack` features) and the one `session_builder()` every session starts from (`load-dynamic` feature, #504: ORT `dlopen`ed at run time — `ORT_DYLIB_PATH`, `.models/onnxruntime/`, search path — for s390x and self-built runtimes) for docling-pdf/docling-asr/docling-rag |
| `crates/docling-asr` | ASR: symphonia decode (audio + video containers) → Whisper (log-mel → ONNX encoder/decoder), or the `parakeet_tdt_0.6b_v3` preset (#508: NeMo 128-mel → FastConformer encoder → TDT greedy decoding, Silero VAD spans; a port of onnx-asr) |
| `crates/docling-cli` | `docling-rs` binary (also `serve` subcommand behind `--features serve`); its lib target holds the MCP server (`mcp.rs`, `docling-rs mcp`) so the Python wheel links the same one |
| `crates/docling-serve` | axum HTTP conversion API (+ Dockerfile with ffmpeg) |
| `crates/docling-py` / `docling-node` / `docling-wasm` | pyo3 / napi-rs / wasm-bindgen bindings — **node and wasm are ordinary workspace members; only docling-py sits outside the workspace and builds from its own directory (maturin)**; `python/docling_rs/markitdown.py`, `python/docling_rs/marker/` and `python/docling_rs/mineru/` are the MarkItDown-, Marker- and MinerU-compatible facades (`docs/MARKITDOWN.md`, `docs/MARKER.md`, `docs/MINERU.md`; numbers from `scripts/conformance/markitdown_compare.py` / `marker_compare.py` / `mineru_compare.py`); `crates/docling-py/mcp` is the pure-Python `docling-rs-mcp` launcher (`uvx docling-rs-mcp`; catalog metadata: `server.json`, the Smithery bundle `crates/docling-py/mcp/mcpb/` (+ `smithery.yaml`, its id), `glama.json`, `docs/MCP_REGISTRIES.md`) |
| `crates/docling-rag` | RAG subsystem (embedder, store, web UI) |

## Build & test

```bash
cargo test --lib --tests -p docling-core -p docling -p docling-asr -p docling-serve -p docling-pdf
cargo clippy --all-targets  <same -p list>          # keep it warning-free
# (CI lints examples too — `--all-targets`, not just `--lib --tests --bins`.)
cargo fmt --all
cargo check -p docling --no-default-features --features pdf-text \
  --target wasm32-unknown-unknown --locked           # the wasm CI gate
(cd crates/docling-py && cargo check)               # pyo3 binding (outside the workspace)
cargo test -p docling-pdf --features pdfium --lib raster:: pdfium_backend::  # the pdfium oracle tests (need .pdfium/lib)
```

- Prefer `--lib --tests` over bare `cargo test`: it skips example binaries,
  each of which statically links onnxruntime (~0.3–5 GB of `target/` churn).
- **Disk discipline (remote container!):** `target/debug` balloons past 15 GB.
  When "No space left on device" hits, delete `target/debug/examples`,
  `target/debug/incremental`, oldest `target/debug/deps` files — deletes work
  even at 0 free. `CARGO_INCREMENTAL=0` helps. Old rustup toolchains and
  `~/.cargo/registry/cache` are also safe to drop. Every `cargo test` rebuild
  leaves the previous test binary behind as a differently-hashed
  `target/debug/deps/<crate>-<hash>` (500–650 MB each for the ML crates); only
  the newest hash per stem is live, the rest is the first thing to delete.
- Tests run with CWD = the crate dir, but shared fixtures and runtime assets
  live at the **repo root**. Resolve fixtures via
  `Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")` and gate asset-needing
  tests with a skip (see `crates/docling/tests/scanned.rs::ml_stack_ready`,
  `crates/docling/src/video.rs::asr_models_ready`) — CI without models
  must stay green.

## Runtime assets & env

- `.models/` (repo root): layout, TableFormer, OCR (rec pairs + the optional
  `ocr_det.onnx` text detector, #429), ASR (`.models/asr/`,
  presets in subdirs), enrichment, embedder. No native PDF library: page
  count, geometry, `/Rotate` and links come from lopdf (`pdf_meta.rs`), the
  text layer from `textparse`, the model images from the pure-Rust page
  renderer (`render/` — tiny-skia paths, embedded/host fonts, shadings,
  patterns, images; measured against the shim) and, for image-only pages
  (scans), from `raster/` byte-identical to pdfium
  (`DOCLING_RS_SCAN_RASTER=pdfium` disables it), so `.models/` alone converts
  every PDF. Development oracles: the docling-parse shim
  `.docling-parse/lib/libdparse_render.so` (`download_dependencies.sh
  --with-docling-parse`; `DOCLING_RS_RENDERER=docling-parse` — what the
  conformance scripts run) and pdfium `.pdfium/lib/libpdfium.so`
  (`libpdfium.dylib` on macOS, #298/#299; only a build with docling-pdf's
  opt-in `pdfium` feature loads it — `DOCLING_RS_RENDERER=pdfium`, a file
  lopdf cannot read, the raster/object-model oracle tests).
  Fetch: `scripts/install/download_dependencies.sh`.
- Resolution is CWD-relative, then `$DOCLING_RS_MODELS_DIR` for `.models/…`
  paths (#285 — whole-dir override keeping the engine's own selection logic,
  e.g. the OCR en/ch pair; the py bindings point it at their cache), then
  exe-dir fallback; env overrides: `PDFIUM_DYNAMIC_LIB_PATH` (`pdfium` feature),
  `DOCLING_OCR_DET_ONNX` (text detector; its boxes are the recognizer's line
  crops inside layout regions since #570 — missing → the ink-projection strips
  and no text outside the regions), `DOCLING_RS_OCR_DET_MAX_SIDE` (cap on the
  detector input's longer side; default 2000 = RapidOCR's `max_side_len`,
  #570; `0` = uncapped, 960 = the pre-#570 PaddleOCR budget),
  `DOCLING_RS_OCR_LINES` (#570; `det` default | `projection` = the strips alone),
  `DOCLING_RS_OCR_TEXT_SCORE` (#570; RapidOCR's `text_score`, 0.5 — lines under
  it are dropped; `0` keeps all), `DOCLING_RS_PICTURE_OCR_MIN_SIDE` (#645; 32 —
  the smallest side in px an embedded picture must have for the opt-in
  picture-OCR enrichment `do_picture_ocr` / `--picture-ocr` to read it; the
  text lands on the picture as docling's `description` annotation),
  `DOCLING_RS_PICTURE_OCR_SCALE` (#666; the picture OCR's px per picture
  pixel under the 2000 px cap — `1.0` = RapidOCR's native pixels; unset =
  the image-input rule), `DOCLING_RS_OCR_TEXTLINE_ORIENTATION` (#666; off —
  RapidOCR's oriented line crops, 90° turn of tall lines and 0/180
  classifier, `.models/ocr_textline_ori.onnx` / `DOCLING_OCR_TEXTLINE_ORI_ONNX`),
  `DOCLING_RS_NER_DIR` (#621; `.models/ner` — the token-classification
  model + `tokenizer.json` + `config.json` the opt-in PII redaction pass
  `redact_pii` / `--redact-pii` reads names, organizations and locations
  with, `download_dependencies.sh --with-ner`; missing → one warning,
  pattern-only), `DOCLING_OCR_REC_ONNX` + `DOCLING_OCR_DICT`
  (the recognizer pair; unset → `.models/ocr_rec_v6.onnx` + `ocr_rec_v6_dict.txt`
  when present (#570, RapidOCR's PP-OCRv6), else the PP-OCRv3 en/ch pair),
  `DOCLING_ASR_{ENCODER,DECODER,VOCAB}` (Whisper), `DOCLING_RS_ASR_VAD`
  (Parakeet's Silero VAD, on when `.models/asr/vad/silero_vad.onnx` exists;
  `off` = the energy-based pause splitter) + `DOCLING_ASR_VAD_ONNX` (its path),
  `DOCLING_FFMPEG` (video frames — ffmpeg is a runtime binary, never a build
  dep), `DOCLING_RS_VIDEO_SCENE_THRESHOLD` / `DOCLING_RS_VIDEO_FRAME_MAX_SIDE` /
  `DOCLING_RS_VIDEO_FRAME_DEDUPE` (#647; the defaults of `video_scene_threshold`
  0.27, `video_frame_max_side` 0 = source size, `video_frame_dedupe` unset —
  frames stream one at a time through the picture-OCR hook, `video_frames=all`
  keeps every distinct cut), `DOCLING_RS_PDF_WORKERS/_THREADS/_INTRA`, `DOCLING_RS_TF_INTRA` (#262),
  `DOCLING_RS_NO_ARENA` (#263; serve and `docling-rs mcp` default it on —
  both also pin glibc's mmap threshold and trim after each conversion,
  `docling::memory` = `docling_core::memory`, #669; since #871 docling-onnx
  pins it with the ONNX Runtime environment, so every model-loading process
  has it, and the Node/Python/FFI bindings trim after each conversion; the
  TableFormer decoder `decoder_kv_layers.onnx` — `kv_layers.py`'s rewrite of
  `decoder_kv.onnx`, bit-identical — keeps its KV caches in loop-owned
  buffers, `docling_onnx::borrowed_f32`, so no per-step tensor grows), the CUDA arena knobs (#842: `DOCLING_RS_CUDA_DEVICE`,
  `DOCLING_RS_CUDA_MEM_LIMIT_MB`, `DOCLING_RS_CUDA_ARENA_EXTEND`
  `next_power_of_two`|`same_as_requested`, `DOCLING_RS_CUDA_ARENA_SHRINK` —
  on: every run hands its arena growth back, `docling_onnx::run` /
  `run_options`, which every session run goes through —
  `DOCLING_RS_CUDA_CONV_ALGO_SEARCH`, `DOCLING_RS_CUDA_CONV_MAX_WORKSPACE`;
  under a CUDA-class provider the page-worker pool and the OCR lanes default
  to one, `docling_onnx::prefers_single_worker`, decided by a probe session
  so an `auto` build without a device keeps its CPU defaults; DirectML,
  #840, takes the same one-worker defaults plus per-page layout and no
  memory pattern, `docling_onnx::is_directml` — named, or `auto` with a
  probe — and `DOCLING_RS_DML_DEVICE` pins its DXGI adapter, unset = the
  high-performance one) and serve's
  `--idle-unload-secs` / `DOCLING_RS_IDLE_UNLOAD_SECS` (drops the warm
  pipeline, GPU memory included, after N idle seconds),
  `DOCLING_RS_GRAPH_CACHE_DIR` / `DOCLING_RS_NO_GRAPH_CACHE` (ONNX Runtime
  optimized-graph cache, CPU provider only), `DOCLING_RS_OCR_SESSIONS`
  (parallel single-thread OCR lanes; byte-identical output),
  `DOCLING_RS_MAX_MEMORY_MB` + `DOCLING_RS_MEMORY_WATERMARK_PCT` (serve
  admission control), `DOCLING_RS_FP32`,
  `DOCLING_RS_EP` (GPU execution providers), `ORT_DYLIB_PATH` (an
  `ort-load-dynamic` build only, #504: the `libonnxruntime.so` to `dlopen`;
  unset → `.models/onnxruntime/<soname>` then the library search path; missing
  → the ML stages degrade like a missing model; the s390x library is
  `onnxruntime-linux-s390x.tar.gz` in the models release —
  `onnxruntime-s390x.yml` / `build_onnxruntime_s390x.sh` cross-build it,
  `download_dependencies.sh` fetches it on an s390x host), `DOCLING_RS_ASR_LANG`,
  `DOCLING_RS_OCR_LANG` (en default; `ch` = the docling-conformance OCR
  model, which conformance scripts pin themselves; BCP-47 tags for either
  language — `en-US`, `zh-Hans` — resolve to the same two, #388),
  `DOCLING_RS_OCR_ENGINE` (#460; `ppocr` default | `tesseract` = the system
  binary on the same layout-region crops — `DOCLING_TESSERACT` names it,
  `DOCLING_RS_TESSERACT_PSM` its page segmentation mode,
  `DOCLING_RS_TESSDATA_DIR` its data; under it `ocr_lang` is tessdata stems /
  BCP-47 tags), `DOCLING_RS_OCR_MODE` (#254; docling's `OcrMode` —
  `full_page`/`layout_regions` force-discard the text layer),
  `DOCLING_RS_OCR_SCALE` (#254; OCR input px/pt — resampled from the 2.0
  render; docling's default is 3; unset, an image input reads at docling's
  effective resolution — 3 px/pt shrunk to RapidOCR's 2000 px longer side,
  #570),
  `DOCLING_RS_OCR_ORIENTATION` (auto default; `off` disables content-based
  un-rotation of raster-rotated scans, #225 — probed on the detector's boxes
  with a confidence margin since #571), `DOCLING_CHUNK_TOKENIZER`,
  `DOCLING_RS_DEBUG` (re-enables quiet pipeline diagnostics, e.g. the
  int8→fp32 layout-retry notice), `DOCLING_RS_MAX_XML_DEPTH` (512; XML
  element nesting any input/part may reach — roxmltree recurses per level),
  `DOCLING_RS_SHEET_MAX_CELLS` (10M; a sheet's used area before it is
  skipped — calamine materializes a dense grid), `DOCLING_RS_MAX_TABLE_CELLS`
  / `DOCLING_RS_MAX_TOTAL_TABLE_CELLS` (#861; 1M per table, 5M per document —
  docling-core 2.103.1's caps; a docling-JSON, DocTags or DocLang table
  declaring more is rejected with the typed `LimitExceeded`,
  `ConversionError::limit()`, before any grid is materialized),
  `DOCLING_RS_MAX_IMAGE_BYTES` / `DOCLING_RS_MAX_IMAGES` /
  `DOCLING_RS_MAX_IMAGE_TOTAL_MB` / `DOCLING_RS_MIN_IMAGE_BYTES` (#646; 32 MiB /
  unlimited / unlimited / 0 — the per-document budget of the image resolvers
  under `image_sources` = `none` default | `embedded` | `local` | `remote`,
  `fetch_images` being `remote`'s alias; `image_hosts` confines `remote`),
  `DOCLING_RS_ZIP_MAX_ENTRIES` / `_MAX_ENTRY_MB` / `_MAX_TOTAL_MB` /
  `_MAX_RATIO` (10000 / 256 / 1024 / 200; what a ZIP input may make the CLI
  or serve inflate, #557 — `ArchiveLimits::from_env`), `DOCLING_RS_MAX_HTML_DEPTH`
  (2000; over-deep HTML is emitted as text from a linear tag scan, never
  parsed), `DOCLING_RS_MAX_RENDER_PIXELS` (15000; per-side cap on a rendered
  PDF-page bitmap — a crafted `MediaBox` otherwise forces a multi-GB
  allocation that pdfium rejects opaquely / the `image` crate panics on;
  parallels the standalone-image `DOCLING_RS_MAX_IMAGE_PIXELS` cap of 30000),
  `DOCLING_RS_DJVU_RENDER_PX` (2500; long-side box a scan-only DjVu page is
  rendered into for the OCR fallback, #434 — DjVu decodes in pure Rust via
  `djvu-rs`, no binary), `DOCLING_RS_RENDERER` (#478; `auto` default = the
  pure-Rust renderer for the model inputs; `docling-parse` = docling-parse's
  Blend2D renderer through the `dlopen`ed shim
  `.docling-parse/lib/libdparse_render.so` (`download_dependencies.sh
  --with-docling-parse` fetches it from the models release,
  `scripts/install/build_docling_parse_render.sh` builds it), missing → one
  warning + the Rust renderer — the conformance scripts set it, the baselines
  are its renders; `rust` = the default spelled out; `pdfium` = the library's
  render, docling's pypdfium2 chain, only with the `pdfium` cargo feature.
  `DOCLING_RS_FONT_DIRS`
  adds host font directories for fonts without a program (`.models/fonts`,
  Liberation/DejaVu/URW/Noto system dirs are scanned by default; a font-less
  host — slim container, bare runner — gets them from
  `download_dependencies.sh --with-fonts` or the `fonts-liberation` +
  `fonts-dejavu-core` packages, which the Dockerfiles install);
  `DOCLING_RS_SYSTEM_FONTS=0` (#633) drops the `$HOME`/system dirs from that
  search so the render depends only on `.models/fonts` + `DOCLING_RS_FONT_DIRS`;
  the Dockerfiles set it, pinning the two font packages they install.
  `DOCLING_PARSE_RENDER_LIB` /
  `DOCLING_PARSE_RESOURCES` override the library and `pdf_resources`
  locations. The PDF baselines — `tests/snapshots`, the groundtruth table —
  are docling-parse-rendered, and `tests/data/pdf/groundtruth` mirrors
  upstream's current files, which docling generates with `do_ocr=False`;
  `pdf_groundtruth.sh` runs `--skip-ocr` to match).

## Conformance & fixtures

- **docling is the reference, not the spec.** Correct, useful output comes
  first; Python docling's behavior is the default to follow where it is
  sensible, and the comparison against it is how changes are measured. A
  deliberate divergence — docling is wrong, lossy, or not worth the cost of
  reproducing — is fine when it is documented: say why in the code comment
  and record it in `docs/MIGRATION.md` (or `docs/PDF_CONFORMANCE.md`'s
  deviations) instead of chasing byte parity. A *difference* from docling is
  a signal to look at, not automatically a bug; an unexplained one is.
- `tests/data/<format>/sources/` + `groundtruth/` (+ `groundtruth_dclx/`,
  `-enriched/`): the corpus mirrored from upstream docling, and the
  regression net for the point above — declarative formats are compared
  against it (byte-for-byte where we do match, and that is the usual case),
  the ML pipeline is pinned by deterministic snapshots (`tests/snapshots/`,
  `scripts/conformance/`, see `docs/PDF_CONFORMANCE.md`). Keep the numbers in
  the docs honest when a change moves them, in either direction.
- Output-regression suite: `crates/docling/tests/regression.rs`, expected
  outputs under `crates/docling/tests/data/<format>/expected/`; regenerate
  intentional changes with `DOCLING_RS_REGEN=1`. **A source file lives in one
  place only:** an upstream fixture goes in the root `tests/data/<format>/sources/`
  and is covered by adding its name to `crates/docling/tests/data/<format>/mirror.txt`;
  only fixtures of our own (formats docling lacks, our regression cases) go in
  `crates/docling/tests/data/<format>/sources/`. The harness fails on a copy
  that exists in both.
- When touching serializers, keep the streaming and buffered paths
  byte-identical — `MarkdownStreamer` tests assert exactly that.
- A `docling-core` serializer change reaches the **PDF** baselines too. The
  main CI jobs never run them (both need the models + the docling-parse shim);
  `pdf-conformance.yml` does, on every PR that touches the PDF stack, the
  serializers, the fixtures or the baselines — it fails on snapshot drift and
  uploads the regenerated tree to refresh `tests/snapshots/` from. Still
  re-run `scripts/conformance/pdf_conformance.sh` (snapshots) and
  `scripts/conformance/pdf_groundtruth.sh` locally in the same PR when you
  can, and keep `docs/PDF_CONFORMANCE.md`'s numbers honest (the job only
  warns when its strict count disagrees with the doc's headline). The
  groundtruth `.md` is a serialization of the committed `groundtruth/*.json`,
  so a serializer-only change is refreshed by re-exporting that JSON — no new
  docling run needed.

## Conventions that keep recurring

- Env knobs go through `docling_core::env` — `flag` (truthy: anything except
  empty/`0`/`false`/`no`/`off`), `nonempty`, `parse::<T>` — and quiet
  diagnostics through `docling_core::debug_log!` (gated on `DOCLING_RS_DEBUG`,
  cached). Never hand-roll `std::env::var` dances in crate code.
- Options plumb through **every** surface in one PR: lib builder on
  `DocumentConverter` → CLI flag → serve option (multipart field + JSON body +
  query param) → Python kwarg → Node option struct. Grep `video_frames` or
  `page_range` for the full pattern.
- Degradation over failure: a missing optional tool/model (ffmpeg, enrichment
  model) warns and degrades; only "nothing convertible at all" errors.
- DOCX is walked twice — `docx.rs` (flat `Node` stream: Markdown, DocLang,
  LaTeX) and `docx_tree.rs` (item tree: JSON, HTML, Pandoc). A change to one
  walk's traversal goes with the same change in the other, and
  `backend/docx_chain_parity.rs` (#628) checks that both carry the same words
  on every DOCX fixture and on a generated wrapper × context matrix; add a
  payload or context there when a new wrapper is special-cased. The flat
  chain is to be retired for the tree eventually.
- An encrypted input raises the typed `docling_core::EncryptionError` on the
  error's `source()` chain (#636: `ConversionError::encrypted[_with_message]`,
  `PdfError::Encrypted`; read with `ConversionError::encryption()`) — never
  a bare `Parse(String)`, which callers would have to match by text.
- Docs: `README.md` is the overview — keep it at 10–12 screens (headline
  tables, one short section per surface with a link out); the detail lives
  in the crate READMEs (`crates/docling` = the library guide: API, output
  formats, chunking, OCR, VLM…; `crates/docling-cli` = CLI, batch, MCP;
  `crates/docling-serve`; the bindings') and `docs/` (`OPTIONS.md`,
  `MODELS.md`, `DEPLOYMENT.md`, `SECURITY.md`, `IBMZ.md`, `MARKITDOWN.md`).
  `docs/MIGRATION.md` holds the parity tables with real conformance numbers,
  every extension, and the deliberate divergences from docling — update it
  with behavior changes; `docs/PDF_CONFORMANCE.md` for pipeline/model
  changes.
- MSRV 1.92 (1.85 for docling-core), edition 2021; CI lints on the 1.96
  toolchain — `cargo fmt` + clippy clean; comments explain *why*
  (docling parity or a deliberate divergence from it, perf tradeoffs),
  matching the existing dense doc-comment
  style.
