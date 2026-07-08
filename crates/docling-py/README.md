# docling-py — Python bindings (PyO3)

A **strangler-fig drop-in** for Python docling's common conversion path,
backed by the Rust [docling.rs](https://github.com/artiz/docling.rs) engine:
same call shape, no torch, ~4× faster PDF conversion at a fraction of the
memory (see [`PDF_PERFORMANCE.md`](../../PDF_PERFORMANCE.md)).

```python
# was:  from docling.document_converter import DocumentConverter
from docling_rs import DocumentConverter

result = DocumentConverter().convert("document.pdf")
print(result.document.export_to_markdown())
data = result.document.export_to_dict()     # docling-core JSON wire format (schema 1.10.0)
```

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
| `DocumentConverter(strict=False, fetch_images=False, artifacts_path=None)` | `DocumentConverter(...)` | `strict` = docling.rs-only cleaner Markdown; default output is docling-legacy byte parity. `artifacts_path` overrides the model cache dir. |
| `.convert(path) -> ConversionResult` | `.convert(source)` | str / `pathlib.Path`. Releases the GIL during conversion. |
| `.convert_bytes(name, data)` | `DocumentStream` | extension of `name` drives format detection |
| `result.status` / `result.document` | same | status: `"success" / "partial_success" / "failure"` |
| `document.export_to_markdown()` | same | plus `export_to_markdown(strict=True/False)` per-call override |
| `document.export_to_dict()` / `export_to_json()` | `export_to_dict()` | dict / JSON string of the docling wire format |
| `document.save_as_markdown(p)` / `save_as_json(p)` | same | |
| `docling_rs.download_models()` | `docling-tools models download` | idempotent; `~/.cache/docling.rs` or `$DOCLING_RS_CACHE_DIR`; INT8 models fetched when hosted and preferred automatically (`DOCLING_RS_FP32=1` opts out) |

Model/env resolution order: explicit `DOCLING_*` env vars → the cache dir set
by `ensure_env()` (called by the constructor) → the process CWD (`models/`,
`.pdfium/`, matching the CLI). pdfium is Linux x64 from the release; on other
platforms set `PDFIUM_DYNAMIC_LIB_PATH` to a local build.

## Not covered (yet)

Chunkers, VLM/enrichment pipelines, and docling's full options model
(`PdfPipelineOptions`, per-format backends selection). The document object
carries rendered text for inline formatting rather than structured
`formatting` fields — see `MIGRATION.md` §4 for the documented divergences.
