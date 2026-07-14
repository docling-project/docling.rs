# docling-py — Python bindings (PyO3)

A **strangler-fig drop-in** for Python docling's common conversion path,
backed by the Rust [docling.rs](https://github.com/docling-project/docling.rs) engine:
same call shape, no torch, ~4× faster PDF conversion at a fraction of the
memory (see [`PDF_CONFORMANCE.md`](../../PDF_CONFORMANCE.md)).

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

## Try it locally

Needs a Rust toolchain (1.82+) and Python ≥ 3.9.

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
| `DocumentConverter(format_options=None, *, allowed_formats=None, do_ocr=True, do_table_structure=True, fetch_images=False, use_web_browser=False, artifacts_path=None)` | `DocumentConverter(allowed_formats=…, format_options=…)` | Pass `{InputFormat.PDF: PdfFormatOption(pipeline_options=PdfPipelineOptions(…))}` or the shorthand kwargs; `allowed_formats` restricts conversion; `artifacts_path` overrides the model cache dir. |
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
| `docling_rs.download_models()` | `docling-tools models download` | idempotent; `~/.cache/docling.rs` or `$DOCLING_RS_CACHE_DIR`; INT8 models fetched when hosted and preferred automatically (`DOCLING_RS_FP32=1` opts out) |

Model/env resolution order: explicit `DOCLING_*` env vars → the cache dir set
by `ensure_env()` (called by the constructor) → the process CWD (`models/`,
`.pdfium/`, matching the CLI). pdfium is Linux x64 from the release; on other
platforms set `PDFIUM_DYNAMIC_LIB_PATH` to a local build.

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

The Rust engine acts on `do_ocr`, `do_table_structure`, and
`accelerator_options.num_threads` (→ ONNX Runtime intra-op threads via
`DOCLING_RS_PDF_THREADS`). The remaining `PdfPipelineOptions` fields
(`images_scale`, `generate_page_images`, `table_structure_options.mode`, …) are
accepted for API compatibility but do not change the pipeline. `InputFormat`,
`DocumentStream` and `ImageRefMode` are re-exported too (the last straight from
`docling_core`, for `export_to_markdown(image_mode=…)`). A GPU
`accelerator_options.device` (`CUDA`/`MPS`) is accepted but warns and falls back
to CPU — the engine runs ONNX Runtime on the CPU execution provider.

## Chunking

`docling_rs.chunking` ships the **Rust-native** ports of docling's chunkers
(`docling::chunker`), API-shaped like `docling.chunking`:

```python
from docling_rs import DocumentConverter
from docling_rs.chunking import HierarchicalChunker, HybridChunker

doc = DocumentConverter().convert("report.docx").document

for chunk in HierarchicalChunker().chunk(doc):        # structure-driven
    print(chunk.meta.headings, chunk.text)

chunker = HybridChunker(tokenizer="tokenizer.json", max_tokens=256)
for chunk in chunker.chunk(doc):                       # tokenization-aware
    embed_me = chunker.contextualize(chunk)            # heading path + text
```

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

VLM/enrichment pipelines, GPU accelerator devices (the engine is ONNX Runtime on
CPU), and per-format *backend* selection. The document carries rendered text for
inline formatting rather than structured `formatting` fields — see
`MIGRATION.md` §4 for the documented divergences.

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
docling-core — no API token is stored or rotated. This requires a **one-time
setup on PyPI by the project owner** (below); re-runs are idempotent
(`skip-existing`). macOS wheels are omitted (no hosted runners here); macOS users
install the sdist, which compiles from source. The ONNX runtime is bundled in the
wheel; pdfium is fetched at runtime by `download_models()`.

### First-time PyPI setup (project owner)

Trusted Publishing lets this repo's workflow upload to PyPI without any password
or API token — GitHub mints a short-lived OIDC token per run and PyPI verifies it
against a *trusted publisher* you register once. Until that publisher exists, the
`publish` job fails with `invalid-publisher: valid token, but no corresponding
publisher`.

Because the `docling-rs` project does not exist on PyPI yet, register a **pending
publisher** (it both authorizes the workflow and lets the first run create the
project):

1. Sign in to PyPI as the account that will own `docling-rs`, then open
   **<https://pypi.org/manage/account/publishing/>**.
2. Under **“Add a new pending publisher”**, choose **GitHub** and fill in
   **exactly** these values:

   | Field | Value |
   |---|---|
   | PyPI Project Name | `docling-rs` |
   | Owner | `docling-project` |
   | Repository name | `docling.rs` |
   | Workflow name | `pypi-publish.yml` |
   | Environment name | `pypi` |

3. Click **Add**. Then re-run the **pypi publish** workflow (Actions tab → Run
   workflow) — the `publish` job will now succeed and create the project on its
   first upload.

Notes:
- The Git branch does not matter: PyPI matches on repository + workflow +
  environment, not the branch, so publishing works from any branch once the
  publisher is registered.
- **Environment name must be `pypi`** — it has to match the `environment: pypi`
  the `publish` job runs in.
- After the first successful publish the project exists; the *pending* publisher
  automatically becomes a regular trusted publisher (manage it later at
  *Project → Manage → Publishing*). To rotate/add publishers there, use the same
  five values above.
- To rehearse without touching production PyPI, register the equivalent pending
  publisher on **TestPyPI** (<https://test.pypi.org/manage/account/publishing/>)
  and point the workflow's upload at `https://test.pypi.org/legacy/`.

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
