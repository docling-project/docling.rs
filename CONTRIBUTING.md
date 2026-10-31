# Contributing

Thanks for helping out. docling.rs is a Rust port of
[docling](https://github.com/docling-project/docling); the guiding constraint is
**conformance**: output is validated against upstream Python docling, byte-for-byte
where the format allows it.

## Setup

```bash
git clone https://github.com/docling-project/docling.rs
cd docling.rs
cargo build

# Optional — only for the ML pipeline (PDF layout/OCR/tables, ASR, enrichment):
scripts/install/download_dependencies.sh   # models/ + .pdfium/
```

Without models and pdfium the declarative converters (DOCX, HTML, XLSX, PPTX, EPUB, …)
and text-layer PDFs work fine, and the test suite stays green — tests that need runtime
assets skip themselves.

## Build & test

```bash
cargo test --lib --tests -p docling-core -p docling -p docling-asr -p docling-serve -p docling-pdf
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Prefer `--lib --tests` over a bare `cargo test`: it skips the example binaries, each of
which statically links onnxruntime and adds gigabytes to `target/`.

CI additionally checks:

- MSRV: `docling-core` on 1.85, the workspace on 1.88;
- `wasm32-unknown-unknown`: `cargo check -p docling --no-default-features [--features pdf-text]`,
  a release build of `docling-wasm`, and its host-side tests;
- GPU feature combinations for `docling-cli` / `docling-rag` / `docling-pdf`;
- `cargo test --workspace --locked`.

If you touch the Python or Node bindings, check them from their own directories
(`cd crates/docling-py && cargo check`) — they are excluded from the workspace default
members.

## Conformance

The corpus lives in `tests/data/<format>/sources/` with `groundtruth/` produced by
published Python docling. Declarative formats must match **byte-for-byte**; the ML
pipeline is pinned by deterministic snapshots.

```bash
scripts/conformance/pdf_groundtruth.sh      # PDF vs the committed groundtruth (no docling install)
scripts/conformance/conformance.sh <format> # installs docling and diffs against it
DOCLING_RS_REGEN=1 cargo test -p docling --test regression   # regenerate intentional output changes
```

A change that moves conformance numbers must say so in the pull request and update
`docs/PDF_CONFORMANCE.md` / `docs/MIGRATION.md` with the real measured numbers.

## Test suites in detail

All commands run from the repo workspace root.

```bash
# everything — unit tests + the output-regression suite (pure Rust; no Python/models)
cargo test

# just the regression suite: re-convert every covered source — the upstream
# fixtures each crates/docling/tests/data/<fmt>/mirror.txt lists from the root
# tests/data/<fmt>/sources/ corpus, plus our own under
# crates/docling/tests/data/<fmt>/sources/ — and assert that legacy Markdown,
# strict Markdown, docling JSON and LaTeX match the committed fixtures
cargo test -p docling --test regression

# refresh the fixtures after an *intentional* output change, then review `git diff`
DOCLING_RS_REGEN=1 cargo test -p docling --test regression

# a single crate / a single test (with output)
cargo test -p docling-core
cargo test outputs_match_fixtures -- --nocapture

# one module of docling's integration suite (each tests/suite/*.rs file)
cargo test -p docling --test suite pptx_list_cascade::
```

Most of a test run is building, not testing: every test binary links the
whole crate with all backends. Two things keep that short.

- **One test binary**: `crates/docling`'s integration tests are modules of
  `tests/suite/main.rs`, linked once (`--test suite`). The files that change
  the process environment, and `regression`, stay binaries of their own —
  add a new test to the suite unless it calls `std::env::set_var`.
- **One pass over the corpus**: `regression`'s three tests (the fixtures,
  streaming vs buffered Markdown, streaming with options) share a single
  parallel pass that converts each source once per mode and keeps the
  verdicts, so running one of them by name still converts the whole corpus
  once — restrict with `DOCLING_RS_REGRESSION_FORMATS=<fmt>,…` instead.
  A test that walks a corpus (the `*_upstream` suites, `docx_chain_parity`,
  docling-pdf's `renders_the_corpus`) converts its files in parallel; keep
  a new one that way — a serial corpus walk in a debug build is a minute
  of CI per hundred files. A walk that *streams* (`convert_streaming`)
  runs on plain threads, not rayon's pool: the stream's producer is a
  thread of its own that may itself fan out on that pool (PPTX slides,
  XLSX sheets), and a pool whose every worker waits on such a producer
  is a deadlock (`regression`'s `par_map`). The rule behind both: a
  rayon worker must never block on a plain thread that may need the
  pool — and a conversion may, through PPTX/XLSX's `par_iter` or lopdf's
  (every PDF read, a LaTeX paper's figures included). Work a worker
  fans out goes on the pool itself (nested rayon is deadlock-free);
  only a thread that is not a worker may spawn and join plain threads.
- **`--profile fasttest`**: optimized like `--release` (the PDF/ONNX tests
  need it) but without the thin LTO the release binaries get, and with
  incremental builds — use it instead of `--release` while iterating:

```bash
cargo test --profile fasttest --workspace
cargo test --profile fasttest -p docling --test suite pptx_list_cascade::
```

The ML formats (PDF, images, METS) need the ONNX models, so they are
covered by a separate **deterministic snapshot** harness rather than `cargo test`:

```bash
bash scripts/install/pdf_setup.sh           # one-time: export the ONNX models
                                    # (layout + TableFormer; needs a torch/docling Python)
# Updating an existing checkout after a model-format change (e.g. the cached
# TableFormer decoder): `rm -rf .models/tableformer && bash scripts/install/pdf_setup.sh`,
# or re-run `python scripts/install/export_tableformer.py .models/tableformer` directly.

export DOCLING_LAYOUT_ONNX="$(pwd)/models/layout_heron.onnx"
export DOCLING_OCR_REC_ONNX="$(pwd)/models/ocr_rec.onnx"
export DOCLING_OCR_DICT="$(pwd)/models/ppocr_keys_v1.txt"
# Optional (falls back to geometric table reconstruction if unset/missing —
# but the fallback is *silent*, so set these to be sure TableFormer is used,
# especially if you invoke docling.rs from anywhere but the repo root: the
# defaults baked into the binary are relative paths, so a different working
# directory makes them silently miss even when the files exist elsewhere).
export DOCLING_TABLEFORMER_ENCODER="$(pwd)/models/tableformer/encoder.onnx"
export DOCLING_TABLEFORMER_DECODER="$(pwd)/models/tableformer/decoder.onnx"
export DOCLING_TABLEFORMER_BBOX="$(pwd)/models/tableformer/bbox.onnx"
bash scripts/conformance/pdf_conformance.sh     # regenerate + diff the snapshot baseline (94 outputs)
```

## Trying it from a checkout

```bash
# convert a file from the CLI — Markdown to stdout (add --strict for cleaner MD)
cargo run -p docling-cli -- crates/docling/sample.html
cargo run -p docling-cli -- --strict crates/docling/sample.html

# emit docling's native DoclingDocument JSON instead (--to md is the default)
cargo run -p docling-cli -- --to json crates/docling/sample.html
cargo run -p docling-cli -- --to json crates/docling/sample.html > out.json

# PDF/image conversion needs the ML models — see "Getting the ML models" below.
scripts/install/download_dependencies.sh
cargo run -p docling-cli -- document.pdf

# transcribe audio (wav/mp3/flac/ogg/aac/m4a, or an mp4/mov audio track) — the
# Whisper models come from the same download script
cargo run -p docling-cli -- recording.mp3
# …with a named preset (fetch it first: download_dependencies.sh --asr-model=whisper_tiny_en)
cargo run -p docling-cli -- --asr-model whisper_tiny_en recording.mp3

# extract pictures: embed as data URIs, or write ./artifacts/*.png — for any
# input that carries images, docling-JSON included (a `data:` URI or a
# referenced file next to the JSON is read back, #403)
cargo run -p docling-cli -- --images embedded   document.pdf
cargo run -p docling-cli -- --images referenced document.pdf > out.md
cargo run -p docling-cli -- --images referenced document.json > out.md

# stream Markdown to stdout page by page (the CLI's default; --no-stream to buffer)
cargo run -p docling-cli -- document.pdf
cargo run -p docling-cli -- --no-stream document.pdf

# sources need not be files (#668): stdin, http(s), data: and file: URLs —
# the format from the name, the Content-Type or the bytes, or say it
cat report.docx | docling-rs --from docx -
docling-rs https://example.com/whitepaper.pdf > whitepaper.md
docling-rs --mime-type text/markdown https://raw.example.com/README
docling-rs "data:text/html;base64,PGgxPkhpPC9oMT4="

# or via the examples
cargo run -p docling --example convert -- crates/docling/sample.md
cargo run -p docling --example stream  -- crates/docling/sample.md

# score HTML output against the latest published docling (installed from PyPI)
scripts/conformance/conformance.sh html

# the full declarative sweep behind the "Conformance with Python docling" table
# (Markdown byte-for-byte + JSON structural, every format or the ones you name)
.venv-compare/bin/python scripts/conformance/full_conformance.py [docx html …]

# diff Python docling vs Rust on one file (installs published docling from PyPI)
scripts/conformance/compare.sh tests/data/html/sources/example_03.html

# benchmark time / CPU / memory: Python docling vs Rust
scripts/test/performance.sh tests/data/html/sources/wiki_duck.html 10
```

The comparison scripts install the latest published Python `docling` from PyPI
into `.venv-compare` automatically on first run. See
[`docs/MIGRATION.md`](./docs/MIGRATION.md) (§7 “Testing” for the differential
and performance scripts, §9 for keeping up with upstream releases).

## Workspace layout

| Crate | Role | Python analogue |
|---|---|---|
| `docling-core` | `DoclingDocument` model + serializers | `docling-core` |
| `docling` | `DocumentConverter`, source loading, backends | `docling` |
| `docling-pdf` | PDF/image ML pipeline (pure-Rust text layer + renderer, ONNX layout/table/OCR) | `docling` PDF pipeline |
| `docling-asr` | audio/ASR pipeline (symphonia + ONNX Whisper) | `docling` ASR pipeline |
| `docling-onnx` | shared ONNX Runtime execution-provider selection (`DOCLING_RS_EP`; `cuda` / `tensorrt` / `directml` / `coreml` / `xnnpack` features) for the ML crates | — |
| `docling-cli` | command-line interface (`docling-rs`, plus the `serve` subcommand behind `--features serve`) | `docling.cli` |
| `docling-serve` | HTTP conversion API over a warm pipeline (`docling-serve` binary, `ghcr.io/docling-project/docling-rs-serve` image) | `docling-serve` |
| `docling-ffi` | C ABI (`docling.h` + shared/static library) for C, C++, C#, Go, Java, Swift embedders | — |
| `docling-node` | Node.js / Bun N-API bindings | https://www.npmjs.com/package/docling.rs |
| `docling-py` | Python bindings (strangler-fig drop-in over docling-core) | https://pypi.org/project/docling-rs |
| `docling-wasm` | WebAssembly bindings (declarative converters + PDF text layer + browser OCR) | https://www.npmjs.com/package/docling.rs-wasm |
| `docling-rag` | RAG layer: chunking, embeddings, vector search, REST API | — |

## Conventions

- **Options plumb through every surface in one pull request**: library builder on
  `DocumentConverter` → CLI flag → serve option (multipart field + JSON body + query
  param) → Python kwarg → Node option. Grep an existing option (`page_range`,
  `video_frames`) for the full pattern.
- **Degradation over failure**: a missing optional tool or model warns and degrades; only
  "nothing convertible at all" is an error.
- **Comments explain *why*** — docling parity, a performance trade-off, a corpus fixture
  that forced the shape. What the code does should be readable from the code.
- **Docs are part of the change**: `README.md` for user-facing behavior,
  `docs/MIGRATION.md` for the parity table, `docs/PDF_CONFORMANCE.md` for pipeline/model
  changes.
- New behavior needs a test — ideally a corpus fixture, otherwise a unit test that pins
  the specific case.

## Commits and pull requests

- One feature per branch, branched off fresh `master`. Don't stack unrelated work.
- **Sign off every commit**: `Signed-off-by: Name <email>` (`git commit -s`).
- Reference issues in the message (`Refs #123`).
- Commit subjects drive releases: a merge whose commits are all `fix:` / `perf:` /
  `revert:` cuts a patch version; anything else bumps the minor. Prefix bug fixes
  accordingly and don't worry about the rest. Docs- or CI-only merges publish nothing.
- Describe in the pull request what you ran: which suites, which conformance numbers
  moved.

## Reporting bugs

Include the input format, the exact command or API call, the docling.rs version, and —
where the output is wrong rather than absent — what Python docling produces for the same
file. A minimal reproducing file is worth more than a description of one.

## License

MIT. By contributing you agree your work is licensed under it.
