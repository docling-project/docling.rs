# docling-py — Python bindings (PyO3)

A **strangler-fig drop-in** for Python docling's common conversion path,
backed by the Rust [docling.rs](https://github.com/docling-project/docling.rs) engine:
same call shape, no torch, ~4× faster PDF conversion at a fraction of the
memory (see [`PDF_PERFORMANCE.md`](../../PDF_PERFORMANCE.md)).

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

> **Status: experimental, not published.** The PyPI distribution name
> (`docling.rs`) is tentative and may change; nothing is uploaded anywhere.
> Build and install locally as below. The crate is intentionally outside the
> repo's Cargo workspace and its crates.io publish flow.

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
| `DocumentConverter(fetch_images=False, artifacts_path=None)` | `DocumentConverter(...)` | `fetch_images` resolves remote/local `<img src>` (HTML/EPUB); `artifacts_path` overrides the model cache dir. |
| `.convert(path) -> ConversionResult` | `.convert(source)` | str / `pathlib.Path`. Releases the GIL during conversion. |
| `.convert_bytes(name, data)` | `DocumentStream` | extension of `name` drives format detection |
| `result.status` / `result.document` / `result.input.file` | same | `.status` is a `ConversionStatus` str-enum (`"success" / "partial_success" / "failure"`); `.document` is a genuine `docling_core` `DoclingDocument` |
| `document.export_to_markdown(...)` | same | docling-core's own method — all of docling's params (`image_placeholder`, `page_break_placeholder`, …) apply |
| `document.export_to_dict()` / `export_to_json()` / `export_to_doctags()` | same | docling-core's own serializers over the wire format |
| `document.save_as_markdown(p)` / `save_as_json(p)` / chunkers | same | anything `docling_core` offers on a `DoclingDocument` works, since it *is* one |
| `docling_rs.download_models()` | `docling-tools models download` | idempotent; `~/.cache/docling.rs` or `$DOCLING_RS_CACHE_DIR`; INT8 models fetched when hosted and preferred automatically (`DOCLING_RS_FP32=1` opts out) |

Model/env resolution order: explicit `DOCLING_*` env vars → the cache dir set
by `ensure_env()` (called by the constructor) → the process CWD (`models/`,
`.pdfium/`, matching the CLI). pdfium is Linux x64 from the release; on other
platforms set `PDFIUM_DYNAMIC_LIB_PATH` to a local build.

## Not covered (yet)

VLM/enrichment pipelines and docling's full options model
(`PdfPipelineOptions`, per-format backend selection). Chunkers **are** available
now — the returned object is a real `docling_core` `DoclingDocument`, so
`docling_core.transforms.chunker`'s `HierarchicalChunker` / `HybridChunker`
operate on it directly (install docling-core's own extras for those:
`pip install "docling-core[chunking]"`). The document carries rendered text for
inline formatting rather than structured `formatting` fields — see
`MIGRATION.md` §4 for the documented divergences.
