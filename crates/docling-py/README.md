# docling-py — Python bindings (PyO3)

A **strangler-fig drop-in** for Python docling's common conversion path,
backed by the Rust [docling.rs](https://github.com/docling-project/docling.rs) engine:
same call shape, no torch, ~4× faster PDF conversion at a fraction of the
memory (see [`docs/PDF_CONFORMANCE.md`](../../docs/PDF_CONFORMANCE.md)).

```python
# was:  from docling.document_converter import DocumentConverter
from docling_rs import DocumentConverter

result = DocumentConverter().convert("document.pdf")
print(result.document.export_to_markdown())
data = result.document.export_to_dict()     # docling-core JSON wire format (schema 1.10.0)
```

**Only the document processor is Rust.** The engine parses the input and returns
docling-core's JSON wire format; this package validates it into a genuine
[`docling_core.types.doc.DoclingDocument`](https://github.com/docling-project/docling-core).
So `result.document` **is** the docling object — `export_to_markdown()`,
`export_to_dict()`, `export_to_doctags()`, the serializers, and the
[chunkers](https://github.com/docling-project/docling-core) are docling's own
Python code, unchanged. `docling-core` is a runtime dependency; nothing else from
docling is required for the declarative path.

> **Status: experimental.** The PyPI distribution name is `docling-rs`.
> Releases are cut manually (like the npm package) via the
> [`pypi-publish`](../../.github/workflows/pypi-publish.yml) workflow — see
> [Publishing](#publishing) below. The crate is intentionally outside the repo's
> Cargo workspace and its crates.io publish flow. For development, build and
> install locally as shown next.

## Migrating from Python docling

The package is designed so that a typical docling script moves over by
changing **the install and the imports** — the code below the imports stays
as-is, because `result.document` is a genuine `docling_core` `DoclingDocument`
and all config objects are re-exported docling-shaped.

**1. Swap the package.** `docling-rs` and `docling` can coexist in one
environment (different module names), so you can A/B them during the
transition; drop `docling` once nothing imports it. For the GPU build install
`docling-rs-cuda` **instead of** `docling-rs` (same `docling_rs` module —
never both):

```bash
pip install docling-rs            # CPU wheels: Linux x86-64/arm64, Windows; sdist elsewhere
# or, with an NVIDIA GPU (Linux x86_64, CUDA 12 + cuDNN 9, glibc ≥ 2.38):
pip install docling-rs-cuda       # converts on the GPU automatically, CPU fallback
pip uninstall docling             # optional — only when you no longer import it
```

**2. Rewrite the imports** — everything comes from `docling_rs` /
`docling_rs.chunking`:

| Python docling import | docling.rs import |
|---|---|
| `from docling.document_converter import DocumentConverter, PdfFormatOption` | `from docling_rs import DocumentConverter, PdfFormatOption` |
| `from docling.datamodel.base_models import InputFormat, DocumentStream` | `from docling_rs import InputFormat, DocumentStream` |
| `from docling.datamodel.pipeline_options import PdfPipelineOptions, AcceleratorOptions, TableFormerMode` | `from docling_rs import PdfPipelineOptions, AcceleratorOptions, TableFormerMode` |
| `from docling.exceptions import ConversionError` | `from docling_rs import ConversionError` |
| `from docling.chunking import HybridChunker, HierarchicalChunker` | `from docling_rs.chunking import HybridChunker, HierarchicalChunker` |
| `from docling_core.types.doc import …` (types, `ImageRefMode`, serializers) | unchanged — `docling_core` stays a dependency and `result.document` is its `DoclingDocument` |

A minimal script, before and after:

```python
# before                                         # after
from docling.document_converter import (         from docling_rs import DocumentConverter
    DocumentConverter,
)
conv = DocumentConverter()                       conv = DocumentConverter()
result = conv.convert("report.pdf")              result = conv.convert("report.pdf")
md = result.document.export_to_markdown()        md = result.document.export_to_markdown()
```

**3. Fetch the models once** (PDF/image path only — declarative formats need
none): `python -c "import docling_rs; docling_rs.download_models()"`
(~700 MB to `~/.cache/docling.rs`, idempotent). docling's own model cache is
not reused; torch, transformers and CUDA-for-python are no longer needed —
the engine bundles ONNX Runtime.

**4. Check the divergences** if your code goes beyond the common path:
the full-VLM pipeline (SmolDocling) and per-format backend selection are not
ported; some `PdfPipelineOptions` fields are accepted for compatibility but
inert (`images_scale`, `generate_page_images`, …); inline formatting is
rendered into the text rather than structured `formatting` fields. The
[API surface](#api-surface-docling-shaped) table below lists what acts, and
[`docs/MIGRATION.md`](../../docs/MIGRATION.md) §4 the documented output
divergences. `HybridChunker(tokenizer=…)` takes a `tokenizer.json` **path**
(no `transformers`) instead of a HF model name.

**5. GPU** (`docling-rs-cuda`): no code changes — the wheel defaults to
`auto` (GPU when usable, CPU fallback). `DOCLING_RS_EP=cpu` or
`AcceleratorOptions(device="cpu")` forces CPU; `DOCLING_RS_EP=cuda` /
`device="cuda"` pins the GPU and fails loudly instead of falling back. See
[GPU wheel](#gpu-wheel-docling-rs-cuda) for the runtime requirements.

## Try it locally

Needs a Rust toolchain (1.88+, the workspace MSRV) and Python ≥ 3.9.

```bash
cd crates/docling-py

# 1. Build + install into the CURRENT virtualenv (create one first):
python -m venv .venv && source .venv/bin/activate
pip install maturin
maturin develop --release          # compiles the Rust engine, installs `docling.rs`

# 2. One-time model download (~700 MB → ~/.cache/docling.rs), pure Python —
#    fetched from the repo's models-v1 GitHub release, like docling fetches
#    its artifacts. Declarative formats (DOCX/HTML/XLSX/…) skip this entirely.
python -c "import docling_rs; docling_rs.download_models()"

# 3. Convert:
python - <<'PY'
from docling_rs import DocumentConverter

conv = DocumentConverter()
result = conv.convert("../../tests/data/pdf/sources/2305.03393v1-pg9.pdf")
print(result.status)                            # "success"
print(result.document.export_to_markdown()[:400])
PY
```

## API surface (docling-shaped)

| docling.rs | docling counterpart | notes |
|---|---|---|
| `DocumentConverter(format_options=None, *, allowed_formats=None, do_ocr=True, do_table_structure=True, do_picture_classification=False, do_code_enrichment=False, do_formula_enrichment=False, fetch_images=False, use_web_browser=False, artifacts_path=None)` | `DocumentConverter(allowed_formats=…, format_options=…)` | Pass `{InputFormat.PDF: PdfFormatOption(pipeline_options=PdfPipelineOptions(…))}` or the shorthand kwargs; `allowed_formats` restricts conversion; `artifacts_path` overrides the model cache dir. |
| `.convert(path \| DocumentStream) -> ConversionResult` | `.convert(source)` | str / `pathlib.Path` / `DocumentStream`. Releases the GIL during conversion. |
| `.convert_all(sources, raises_on_error=True) -> Iterator[ConversionResult]` | same | lazily converts many sources; `raises_on_error=False` yields a `failure` result instead of raising |
| `.initialize_pipeline(format=None)` | same | pre-loads the PDF/image ML models so the first conversion isn't slow and later PDFs reuse the warm pipeline (no-op for non-ML formats; needs the models available) |
| `.convert_bytes(name, data)` | `DocumentStream` | extension of `name` drives format detection |
| `InputFormat`, `PdfPipelineOptions`, `PdfFormatOption`, `AcceleratorOptions`, `TableFormerMode`, `DocumentStream`, `ImageRefMode` | same modules | docling-shaped config re-exported from `docling_rs` (see below) |
| `ConversionError` | `docling.exceptions.ConversionError` | raised on a failed conversion; caught by `convert_all(..., raises_on_error=False)` |
| `result.status` / `result.document` / `result.input.file` | same | `.status` is a `ConversionStatus` str-enum (`"success" / "partial_success" / "failure"`); `.document` is a genuine `docling_core` `DoclingDocument` |
| `document.export_to_markdown(...)` | same | docling-core's own method — all of docling's params (`image_placeholder`, `page_break_placeholder`, …) apply |
| `document.export_to_dict()` / `export_to_json()` / `export_to_doctags()` | same | docling-core's own serializers over the wire format |
| `document.save_as_markdown(p)` / `save_as_json(p)` / chunkers | same | anything `docling_core` offers on a `DoclingDocument` works, since it *is* one |
| `docling_rs.download_models()` | `docling-tools models download` | idempotent; `~/.cache/docling.rs` or `$DOCLING_RS_CACHE_DIR`; INT8 models fetched when hosted and preferred automatically (`DOCLING_RS_FP32=1` opts out); `force=True` re-downloads a stale cache after a model re-publish |

Model/env resolution order: explicit `DOCLING_*` env vars → the process CWD
(`models/`, `.pdfium/`, matching the CLI — so a repo checkout uses its own
exports) → the cache dir set by `ensure_env()` (called by the constructor).
pdfium is Linux x64 from the release; on other platforms set
`PDFIUM_DYNAMIC_LIB_PATH` to a local build.

## Configuration (docling-shaped)

`docling_rs` re-exports docling-shaped config objects — same names and fields, so
docling code reads unchanged:

```python
from docling_rs import DocumentConverter, InputFormat, PdfFormatOption, PdfPipelineOptions, AcceleratorOptions

opts = PdfPipelineOptions(
    do_ocr=False,                                   # skip OCR on scanned pages
    do_table_structure=True,                        # TableFormer table recovery
    accelerator_options=AcceleratorOptions(num_threads=4),
)
conv = DocumentConverter(format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)})
# shorthand: DocumentConverter(do_ocr=False, do_table_structure=True)
```

The Rust engine acts on `do_ocr`, `do_table_structure`, the opt-in enrichment
flags `do_picture_classification` / `do_code_enrichment` /
`do_formula_enrichment` (the picture classifier is fetched by the default
`scripts/install/download_dependencies.sh` run; the code/formula models need
its `--enrich` flag), and
`accelerator_options.num_threads` (→ ONNX Runtime intra-op threads via
`DOCLING_RS_PDF_THREADS`). The remaining `PdfPipelineOptions` fields
(`images_scale`, `generate_page_images`, `table_structure_options.mode`, …) are
accepted for API compatibility but do not change the pipeline. `InputFormat`,
`DocumentStream` and `ImageRefMode` are re-exported too (the last straight from
`docling_core`, for `export_to_markdown(image_mode=…)`). A GPU
`accelerator_options.device` (`CUDA`/`MPS`) is accepted but warns and falls back
to CPU: the prebuilt PyPI wheels ship ONNX Runtime with the CPU execution
provider only. The engine itself supports CUDA / TensorRT / DirectML / CoreML
behind cargo features (issue #74) — build the wheel from source with e.g.
`maturin build --features cuda` and select the provider per process with
`DOCLING_RS_EP=cuda` (see the workspace README).

## Chunking

`docling_rs.chunking` ships the **Rust-native** ports of docling's chunkers
(`docling::chunker`), API-shaped like `docling.chunking`:

```python
from docling_rs import DocumentConverter
from docling_rs.chunking import HierarchicalChunker, HybridChunker, WindowChunker

doc = DocumentConverter().convert("report.docx").document

for chunk in HierarchicalChunker().chunk(doc):        # structure-driven
    print(chunk.meta.headings, chunk.text)

chunker = HybridChunker(tokenizer="tokenizer.json", max_tokens=256)
for chunk in chunker.chunk(doc):                       # tokenization-aware
    embed_me = chunker.contextualize(chunk)            # heading path + text

chunker = WindowChunker(max_words=300, overlap=0.05)   # word-window, no tokenizer
for chunk in chunker.chunk(doc):                       # docling-rag's window chunker
    embed_me = chunker.contextualize(chunk)            # '# path' line + body
```

`WindowChunker` is **docling-rag's window chunker**: the document's Markdown is
cut into heading-bounded sections of plain words (markup stripped), and a
fixed window of `max_words` words (default 300) slides over each section with
`overlap` fractional overlap (default 0.05 = 5%). A chunk never crosses a
heading, `chunk.meta.headings` carries the heading path, and
`contextualize(chunk)` renders rag-style — a `# Outer > Inner` context line, a
blank line, then the body. No tokenizer and no ML models are involved, making
it the zero-dependency choice when an approximate chunk size is enough
(`meta.doc_items` is empty — it works on the rendered Markdown, not the
document tree).

Two deltas from docling: `HybridChunker(tokenizer=...)` takes a **path to a
HuggingFace `tokenizer.json`** (loaded natively — no `transformers` install),
and `chunk.meta.doc_items` holds the items' JSON-pointer refs. With no
`tokenizer` argument it falls back to MiniLM's tokenizer at
`models/chunk/tokenizer.json` (the download script's location) or the package
cache — `docling_rs.download_models()` fetches it with the other assets. Since
`result.document` is a genuine `docling_core` `DoclingDocument`, docling's own
Python chunkers (`pip install "docling-core[chunking]"`) also keep working on
it — the native classes are the faster, dependency-free path.

### Streaming

`chunk()` **streams natively**: it returns a lazy iterator fed by a Rust
background thread, which hands each chunk to Python as the chunkers produce
it. The full chunk list is never materialized on either side of the FFI
boundary — the first chunk is ready for embedding while the rest of the
document is still being chunked, and a slow consumer throttles the producer
through a bounded queue instead of buffering unboundedly.

```python
from itertools import islice

from docling_rs import DocumentConverter
from docling_rs.chunking import HybridChunker

doc = DocumentConverter().convert("large.html").document
chunker = HybridChunker(tokenizer="tokenizer.json", max_tokens=512)

# Chunks arrive one by one; embed each as soon as it is produced.
for chunk in chunker.chunk(doc):
    index.add(embed(chunker.contextualize(chunk)))

# Laziness composes: this chunks only far enough to produce 10 chunks.
preview = list(islice(chunker.chunk(doc), 10))
```

Abandoning the iterator early (`break`, `islice`, dropping the generator)
cancels the background chunking, and Ctrl-C interrupts a pending `next()`.
Errors (a bad tokenizer path, malformed document JSON) surface on the first
`next()`, not at `chunk()` call time.

## Not covered (yet)

The full-VLM conversion pipeline (SmolDocling) and per-format *backend*
selection. GPU inference is engine-side only: compiled in via cargo features
(#74) and selected with `DOCLING_RS_EP`, not via `accelerator_options.device`,
and absent from the prebuilt CPU wheels. The document carries rendered text for
inline formatting rather than structured `formatting` fields — see
`docs/MIGRATION.md` §4 for the documented divergences.

## Publishing

Releases are **manual**, mirroring the npm package: the
[`pypi-publish`](../../.github/workflows/pypi-publish.yml) GitHub Actions workflow
(`workflow_dispatch`) builds an `abi3` wheel per platform (Linux x86-64/arm64 as
`manylinux_2_28`, Windows x86-64 — one wheel covers every Python ≥ 3.9) plus an
sdist, and uploads them to PyPI.

```bash
# From the Actions tab, or:
gh workflow run pypi-publish.yml                 # version from pyproject.toml
gh workflow run pypi-publish.yml -f version=0.16.0
```

No secrets: it publishes via PyPI **Trusted Publishing** (OIDC), like
docling-core — no API token is stored or rotated (the trusted publisher is
registered on PyPI; manage it at *Project → Manage → Publishing*). Re-runs are
idempotent (`skip-existing`). macOS wheels are omitted (no hosted runners here);
macOS users install the sdist, which compiles from source. The ONNX runtime is
bundled in the wheel; pdfium is fetched at runtime by `download_models()`.

### GPU wheel: `docling-rs-cuda`

The workflow's `cuda_wheel` input additionally builds a **Linux x86_64** wheel
published as **`docling-rs-cuda`**: the same crate compiled with
`--features cuda`, ONNX Runtime's CUDA provider libraries bundled next to the
native module (found via an `$ORIGIN` rpath — no import-time preload).
It installs the same `docling_rs` module — install *either* `docling-rs` *or*
`docling-rs-cuda`, not both:

```bash
pip install docling-rs-cuda
python -c "import docling_rs; ..."   # GPU used automatically when present
```

The GPU wheel defaults to `auto`: it converts on the GPU when one is usable
and falls back to CPU when not — no environment setup needed.
`DOCLING_RS_EP=cpu` (or `AcceleratorOptions(device="cpu")`) forces CPU;
`DOCLING_RS_EP=cuda` / `device="cuda"` pins the GPU and fails loudly if it
can't initialize. The fp32 models are preferred automatically on GPU, and
**CUDA 12 + cuDNN 9 must be installed on the system** — the wheel ships the
ONNX Runtime provider, not the CUDA toolkit. The wheel is tagged **`manylinux_2_38`**
(glibc ≥ 2.38 at runtime, i.e. Ubuntu 24.04+ / Debian 13+): the CUDA ONNX
Runtime static binaries carry glibc-2.38 symbols, so this floor is inherent —
it is also why the CI job builds on plain `ubuntu-24.04` instead of the
manylinux_2_28 container the CPU wheels use (linking there fails on
`__isoc23_*`). Measured end-to-end on an RTX 3080 Laptop: 1.5–2.1× on
multi-page digital PDFs, 8.7× on a 1913-page manual (see
[`PDF_CONFORMANCE.md`](../../docs/PDF_CONFORMANCE.md#measured-on-real-hardware-issue-108)).

Local build mirroring the CI wheel (order matters — the provider libraries
must exist *and* sit inside `python/docling_rs/` before the wheel is
assembled, or the wheel silently ships without them):

```bash
cd crates/docling-py
export RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN'
cargo build --release --features cuda            # ort fetches CUDA ONNX Runtime + drops the provider libs
cp target/release/libonnxruntime_providers_{shared,cuda}.so python/docling_rs/
maturin build --release --features cuda          # wheel now includes them (expect ~hundreds of MB)
```

PyPI setup (one-time): `docling-rs-cuda` is a separate PyPI project — register
the same workflow as a trusted publisher there too, and if the wheel exceeds
PyPI's default file-size limit, request a per-project bump (the
`onnxruntime-gpu` package is the precedent).

### Test the release build locally

Reproduce what CI does — build the wheel + sdist and verify both install and run
— before (or instead of) triggering the workflow. Needs a Rust toolchain and
Python ≥ 3.9.

```bash
cd crates/docling-py
python -m venv .venv && source .venv/bin/activate
pip install maturin

# 1. Build the same two artifacts the workflow builds.
maturin build --release --out dist      # dist/docling_rs-<v>-cp39-abi3-<platform>.whl
maturin sdist            --out dist      # dist/docling_rs-<v>.tar.gz  (vendors all crates)

# 2. Smoke-test the WHEEL in a clean env — pip pulls docling-core from the
#    wheel's declared dependency, exactly as an end user would get it.
python -m venv /tmp/wheel-test
/tmp/wheel-test/bin/pip install dist/docling_rs-*.whl
/tmp/wheel-test/bin/python - <<'PY'
from docling_rs import DocumentConverter
r = DocumentConverter().convert("../../tests/data/html/sources/hyperlink_03.html")
assert r.status == "success"
assert type(r.document).__module__.startswith("docling_core")   # the real DoclingDocument
print("wheel OK:", len(r.document.export_to_markdown()), "md chars")
PY

# 3. Verify the SDIST is self-contained: pip compiles the Rust engine from source
#    (this is the exact unpack-and-build path cibuildwheel runs in the manylinux
#    containers, so a green result here means the CI wheel build will work too).
python -m venv /tmp/sdist-test
/tmp/sdist-test/bin/pip install dist/docling_rs-*.tar.gz
/tmp/sdist-test/bin/python -c "import docling_rs; print('sdist build OK')"

# 4. Run the declarative-path test suite (no ML models needed).
pip install pytest docling-core
pytest tests/
```

To exercise the full manylinux wheel build (what `pypa/cibuildwheel` runs) you
need Docker; with a daemon available:

```bash
pipx run cibuildwheel==2.21.3 --platform linux --output-dir wheelhouse .
# env: CIBW_BUILD=cp39-* CIBW_SKIP=*-musllinux*  CIBW_BEFORE_ALL_LINUX="curl … rustup … -y"
```

An optional final rehearsal uploads to **TestPyPI** (needs a TestPyPI token or a
pending publisher there): `pip install twine && twine upload --repository testpypi dist/*`.
