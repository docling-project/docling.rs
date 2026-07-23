# CLAUDE.md — working notes for AI-assisted sessions

Rust port of [docling](https://github.com/docling-project/docling): document
conversion (PDF/Office/HTML/audio/video/…) to Markdown / docling-JSON / DCLX,
validated for byte-for-byte conformance against upstream Python docling.

## Workflow rules

- **Every commit must be signed off by the author.** End each commit message
  with `Signed-off-by: name <email>`.
- Claude Web: **Never open pull requests on `artiz/docling.rs`.** Push a `claude/<topic>`
  branch and hand back a compare link
  (`https://github.com/docling-project/docling.rs/compare/master...artiz:docling.rs:<branch>?expand=1`);
  the maintainer opens/merges PRs themself (usually into the upstream
  `docling-project/docling.rs`; `artiz/docling.rs` is their working fork).
- One feature = one branch off fresh `origin/master`. Don't stack unrelated
  work.
- Issue numbers (`#80`, `#138`, …) refer to `docling-project/docling.rs`
  issues; reference them in commit messages (`Refs #NN`).

## Workspace map

| Crate | What it is |
|---|---|
| `crates/docling-core` | `DoclingDocument` model, Markdown/JSON/DCLX serializers, `MarkdownStreamer`, chunkers |
| `crates/docling` | `DocumentConverter` (format routing), declarative backends (`src/backend/`), streaming (`src/stream.rs`), video (`src/video.rs`) |
| `crates/docling-pdf` | ML pipeline: pdfium + RT-DETR layout + TableFormer + PP-OCRv3 + enrichment (`ml` feature); pure-Rust text-layer path compiles for wasm without it |
| `crates/docling-asr` | Whisper ASR: symphonia decode (audio + video containers) → log-mel → ONNX encoder/decoder |
| `crates/docling-cli` | `docling-rs` binary (also `serve` subcommand behind `--features serve`) |
| `crates/docling-serve` | axum HTTP conversion API (+ Dockerfile with ffmpeg) |
| `crates/docling-py` / `docling-node` / `docling-wasm` | pyo3 / napi-rs / wasm-bindgen bindings — **excluded from the workspace default-members; py and wasm build from their own directories** |
| `crates/docling-rag` | RAG subsystem (embedder, store, web UI) |

## Build & test

```bash
cargo test --lib --tests -p docling-core -p docling -p docling-asr -p docling-serve -p docling-pdf
cargo clippy --lib --tests --bins <same -p list>   # keep it warning-free
cargo fmt --all
cargo check -p docling --no-default-features        # pdf-text/wasm path
(cd crates/docling-py && cargo check)               # pyo3 binding
(cd crates/docling-wasm && cargo check)             # wasm binding
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

- `models/` (repo root): layout, TableFormer, OCR, ASR (`models/asr/`,
  presets in subdirs), enrichment, embedder. `.pdfium/lib/libpdfium.so` for
  page rendering. Fetch: `scripts/install/download_dependencies.sh`.
- Resolution is CWD-relative with exe-dir fallback; env overrides:
  `PDFIUM_DYNAMIC_LIB_PATH`, `DOCLING_ASR_{ENCODER,DECODER,VOCAB}`,
  `DOCLING_FFMPEG` (video frames — ffmpeg is a runtime binary, never a build
  dep), `DOCLING_RS_PDF_WORKERS/_THREADS/_INTRA`, `DOCLING_RS_FP32`,
  `DOCLING_RS_EP` (GPU execution providers), `DOCLING_RS_ASR_LANG`,
  `DOCLING_CHUNK_TOKENIZER`.

## Conformance & fixtures

- `tests/data/<format>/sources/` + `groundtruth/` (+ `groundtruth_dclx/`,
  `-enriched/`): the corpus mirrored from upstream docling. Declarative
  formats must match Python docling **byte-for-byte**; the ML pipeline is
  pinned by deterministic snapshots (`tests/snapshots/`,
  `scripts/conformance/`, see `docs/PDF_CONFORMANCE.md`).
- Output-regression suite: `crates/docling/tests/regression.rs` over
  `crates/docling/tests/data`; regenerate intentional changes with
  `DOCLING_RS_REGEN=1`.
- When touching serializers, keep the streaming and buffered paths
  byte-identical — `MarkdownStreamer` tests assert exactly that.

## Conventions that keep recurring

- Options plumb through **every** surface in one PR: lib builder on
  `DocumentConverter` → CLI flag → serve option (multipart field + JSON body +
  query param) → Python kwarg → Node option struct. Grep `video_frames` or
  `page_range` for the full pattern.
- Degradation over failure: a missing optional tool/model (ffmpeg, enrichment
  model) warns and degrades; only "nothing convertible at all" errors.
- Docs live in `README.md` (user-facing) + `docs/MIGRATION.md` (parity table
  with real conformance numbers) — update both with behavior changes;
  `docs/PDF_CONFORMANCE.md` for pipeline/model changes.
- Rust 1.96, edition 2021, `cargo fmt` + clippy clean; comments explain *why*
  (docling parity, perf tradeoffs), matching the existing dense doc-comment
  style.
