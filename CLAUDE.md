# CLAUDE.md — working notes for AI-assisted sessions

Rust port of [docling](https://github.com/docling-project/docling): document
conversion (PDF/Office/HTML/audio/video/…) to Markdown / docling-JSON / DCLX,
validated for byte-for-byte conformance against upstream Python docling.

## Workflow rules

- **Every commit must be signed off by the author.** End each commit message
  with `Signed-off-by: name <email>`.
- **Releases are automatic on merge; only the `fix:` prefix matters.** CI
  (`scripts/ci/bump_version.sh`): a merge whose commits are all
  `fix:`/`perf:`/`revert:` cuts a patch (0.49.0 → 0.49.1); **any other
  commit** — whatever its prefix — bumps the 0.x "major" (0.49.0 → 0.50.0).
  So prefix bug fixes `fix(scope): …` and don't worry about the rest.
  Docs/CI-only merges still release nothing (release.sh only fires when a
  publishable crate's source changed). No automatic semver-major: majors are
  cut by hand (`force_version` dispatch input) — v1.0.0 marks the breaking
  `.models/` asset-dir rename. The GitHub Release ci.yml creates then fans
  out (`release: published`) to `pypi-publish.yml` (PyPI `docling-rs` +
  `docling-rs-cuda`), `npm-publish.yml` (`docling.rs`, the platform packages,
  `docling.rs-cuda`, `docling.rs-wasm`), `cli-binaries.yml` and
  `docker-publish.yml` — nothing to click; re-run one by hand only to repair
  a skipped/failed publish.
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
| `crates/docling-pdf` | ML pipeline: pdfium + RT-DETR layout + TableFormer + PP-OCRv3 + enrichment (`ml` feature); pure-Rust text-layer path compiles for wasm without it |
| `crates/docling-onnx` | Shared ONNX Runtime execution-provider selection (`DOCLING_RS_EP`, `cuda`/`tensorrt`/`directml`/`coreml`/`xnnpack` features) for docling-pdf/docling-asr/docling-rag |
| `crates/docling-asr` | Whisper ASR: symphonia decode (audio + video containers) → log-mel → ONNX encoder/decoder |
| `crates/docling-cli` | `docling-rs` binary (also `serve` subcommand behind `--features serve`) |
| `crates/docling-serve` | axum HTTP conversion API (+ Dockerfile with ffmpeg) |
| `crates/docling-py` / `docling-node` / `docling-wasm` | pyo3 / napi-rs / wasm-bindgen bindings — **node and wasm are ordinary workspace members; only docling-py sits outside the workspace and builds from its own directory (maturin)** |
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
```

- Prefer `--lib --tests` over bare `cargo test`: it skips example binaries,
  each of which statically links onnxruntime (~0.3–5 GB of `target/` churn).
- **Disk discipline (remote container!):** `target/debug` balloons past 15 GB.
  When "No space left on device" hits, delete `target/debug/examples`,
  `target/debug/incremental`, oldest `target/debug/deps` files — deletes work
  even at 0 free. `CARGO_INCREMENTAL=0` helps. Old rustup toolchains and
  `~/.cargo/registry/cache` are also safe to drop.
- Tests run with CWD = the crate dir, but shared fixtures and runtime assets
  live at the **repo root**. Resolve fixtures via
  `Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")` and gate asset-needing
  tests with a skip (see `crates/docling/tests/pages.rs::pdfium_ready`,
  `crates/docling/src/video.rs::asr_models_ready`) — CI without models/pdfium
  must stay green.

## Runtime assets & env

- `.models/` (repo root): layout, TableFormer, OCR (rec pairs + the optional
  `ocr_det.onnx` text detector, #429), ASR (`.models/asr/`,
  presets in subdirs), enrichment, embedder. `.pdfium/lib/libpdfium.so`
  (`libpdfium.dylib` on macOS, #298/#299) for page rendering. Fetch:
  `scripts/install/download_dependencies.sh`.
- Resolution is CWD-relative, then `$DOCLING_RS_MODELS_DIR` for `.models/…`
  paths (#285 — whole-dir override keeping the engine's own selection logic,
  e.g. the OCR en/ch pair; the py bindings point it at their cache), then
  exe-dir fallback; env overrides: `PDFIUM_DYNAMIC_LIB_PATH`,
  `DOCLING_OCR_DET_ONNX` (text detector; missing → recognition-only OCR),
  `DOCLING_RS_OCR_DET_MAX_SIDE` (cap on the detector input's longer side;
  default 960 = PaddleOCR's, ~⅓ the uncapped detection time; `0` = RapidOCR's
  uncapped rule),
  `DOCLING_ASR_{ENCODER,DECODER,VOCAB}`,
  `DOCLING_FFMPEG` (video frames — ffmpeg is a runtime binary, never a build
  dep), `DOCLING_RS_PDF_WORKERS/_THREADS/_INTRA`, `DOCLING_RS_TF_INTRA` (#262),
  `DOCLING_RS_NO_ARENA` (#263; serve defaults it on),
  `DOCLING_RS_GRAPH_CACHE_DIR` / `DOCLING_RS_NO_GRAPH_CACHE` (ONNX Runtime
  optimized-graph cache, CPU provider only), `DOCLING_RS_OCR_SESSIONS`
  (parallel single-thread OCR lanes; byte-identical output),
  `DOCLING_RS_MAX_MEMORY_MB` + `DOCLING_RS_MEMORY_WATERMARK_PCT` (serve
  admission control), `DOCLING_RS_FP32`,
  `DOCLING_RS_EP` (GPU execution providers), `DOCLING_RS_ASR_LANG`,
  `DOCLING_RS_OCR_LANG` (en default; `ch` = the docling-conformance OCR
  model, which conformance scripts pin themselves; BCP-47 tags for either
  language — `en-US`, `zh-Hans` — resolve to the same two, #388),
  `DOCLING_RS_OCR_MODE` (#254; docling's `OcrMode` —
  `full_page`/`layout_regions` force-discard the text layer),
  `DOCLING_RS_OCR_SCALE` (#254; OCR input px/pt — resampled from the 2.0
  render; docling's default is 3),
  `DOCLING_RS_OCR_ORIENTATION` (auto default; `off` disables content-based
  un-rotation of raster-rotated scans, #225), `DOCLING_CHUNK_TOKENIZER`,
  `DOCLING_RS_DEBUG` (re-enables quiet pipeline diagnostics, e.g. the
  int8→fp32 layout-retry notice), `DOCLING_RS_MAX_XML_DEPTH` (512; XML
  element nesting any input/part may reach — roxmltree recurses per level),
  `DOCLING_RS_SHEET_MAX_CELLS` (10M; a sheet's used area before it is
  skipped — calamine materializes a dense grid), `DOCLING_RS_MAX_HTML_DEPTH`
  (2000; over-deep HTML is emitted as text from a linear tag scan, never
  parsed), `DOCLING_RS_MAX_RENDER_PIXELS` (15000; per-side cap on a rendered
  PDF-page bitmap — a crafted `MediaBox` otherwise forces a multi-GB
  allocation that pdfium rejects opaquely / the `image` crate panics on;
  parallels the standalone-image `DOCLING_RS_MAX_IMAGE_PIXELS` cap of 30000),
  `DOCLING_RS_DJVU_RENDER_PX` (2500; long-side box a scan-only DjVu page is
  rendered into for the OCR fallback, #434 — DjVu decodes in pure Rust via
  `djvu-rs`, no binary).

## Conformance & fixtures

- `tests/data/<format>/sources/` + `groundtruth/` (+ `groundtruth_dclx/`,
  `-enriched/`): the corpus mirrored from upstream docling. Declarative
  formats must match Python docling **byte-for-byte**; the ML pipeline is
  pinned by deterministic snapshots (`tests/snapshots/`,
  `scripts/conformance/`, see `docs/PDF_CONFORMANCE.md`).
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
- A `docling-core` serializer change reaches the **PDF** baselines too, and
  neither runs in CI (both need pdfium + models): re-run
  `scripts/conformance/pdf_conformance.sh` (snapshots) and
  `scripts/conformance/pdf_groundtruth.sh` in the same PR, or the next person
  reads the stale baseline as a pipeline regression. The groundtruth `.md` is a
  serialization of the committed `groundtruth/*.json`, so a serializer-only
  change is refreshed by re-exporting that JSON — no new docling run needed.

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
- Docs live in `README.md` (user-facing) + `docs/MIGRATION.md` (parity table
  with real conformance numbers) — update both with behavior changes;
  `docs/PDF_CONFORMANCE.md` for pipeline/model changes.
- MSRV 1.88 (1.85 for docling-core), edition 2021; CI lints on the 1.96
  toolchain — `cargo fmt` + clippy clean; comments explain *why*
  (docling parity, perf tradeoffs), matching the existing dense doc-comment
  style.
