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
data = result.document.export_to_dict()     # docling-core JSON wire format (schema 1.11.0)
```

**Coming from [MarkItDown](https://github.com/microsoft/markitdown)?** The same
package speaks its API — change the import, keep the code:

```python
# was:  from markitdown import MarkItDown
from docling_rs.markitdown import MarkItDown

print(MarkItDown().convert("report.pdf").markdown)
```

Layout analysis, TableFormer tables and local OCR replace pdfminer and LLM
calls: PDF table-cell recall 0.31 → 0.94, Office/HTML 5–10× faster
([measured](https://github.com/docling-project/docling.rs/blob/master/docs/MARKITDOWN.md);
[migration notes](#migrating-from-markitdown)).

**Coming from [Marker](https://github.com/datalab-to/marker)?** Same thing —
swap the imports, keep the code:

```python
# was:  from marker.converters.pdf import PdfConverter
#       from marker.models import create_model_dict
#       from marker.output import text_from_rendered
from docling_rs.marker.converters.pdf import PdfConverter
from docling_rs.marker.models import create_model_dict
from docling_rs.marker.output import text_from_rendered

text, _, images = text_from_rendered(PdfConverter(artifact_dict=create_model_dict())("paper.pdf"))
```

No PyTorch, no inference server, every format natively
([measured](https://github.com/docling-project/docling.rs/blob/master/docs/MARKER.md);
[migration notes](#migrating-from-marker)).

**Coming from [MinerU](https://github.com/opendatalab/MinerU)?** Same thing —
swap the imports, keep MinerU 4's SDK:

```python
# was:  from mineru.parser import parse
from docling_rs.mineru.parser import parse

result = parse("paper.pdf", tier="standard", page_range="1-3")
print(result.markdown())                        # or result.to_json(), result.structured_content()
```

No DocVortex, no llama.cpp, every format at every tier
([measured](https://github.com/docling-project/docling.rs/blob/master/docs/MINERU.md);
[migration notes](#migrating-from-mineru)).

**Only the document processor is Rust.** The engine parses the input and returns
docling-core's JSON wire format; this package validates it into a genuine
[`docling_core.types.doc.DoclingDocument`](https://github.com/docling-project/docling-core).
So `result.document` **is** the docling object — `export_to_markdown()`,
`export_to_dict()`, `export_to_doctags()`, the serializers, and the
[chunkers](https://github.com/docling-project/docling-core) are docling's own
Python code, unchanged. `docling-core` is a runtime dependency; nothing else from
docling is required for the declarative path.

> **Status: experimental.** The PyPI distribution name is `docling-rs`.
> Releases reach PyPI when the maintainer runs the
> [`pypi-publish`](../../.github/workflows/pypi-publish.yml) workflow for them —
> not every GitHub Release is published there; see [Publishing](#publishing) below. The crate is intentionally outside the repo's
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
pip install docling-rs            # CPU wheels: Linux x86-64/arm64/s390x, Windows; sdist elsewhere
# or, with an NVIDIA GPU (Linux x86_64, CUDA 13 + cuDNN 9, glibc ≥ 2.38):
pip install docling-rs-cuda       # converts on the GPU automatically, CPU fallback
pip uninstall docling             # optional — only when you no longer import it
```

**2. Rewrite the imports: replace `docling` with `docling_rs`** — the package
mirrors docling's module layout one-to-one, so it's a mechanical rename
(`sed -i 's/\bdocling\./docling_rs./g'` on the import lines does it):

| Python docling import | docling.rs import |
|---|---|
| `from docling.document_converter import DocumentConverter, PdfFormatOption` | `from docling_rs.document_converter import DocumentConverter, PdfFormatOption` |
| `from docling.datamodel.base_models import InputFormat, DocumentStream` | `from docling_rs.datamodel.base_models import InputFormat, DocumentStream` |
| `from docling.datamodel.pipeline_options import PdfPipelineOptions, AcceleratorOptions, TableFormerMode` | `from docling_rs.datamodel.pipeline_options import …` (same names) |
| `from docling.datamodel.accelerator_options import AcceleratorDevice, AcceleratorOptions` | `from docling_rs.datamodel.accelerator_options import …` (same names) |
| `from docling.datamodel.document import ConversionResult` | `from docling_rs.datamodel.document import ConversionResult` |
| `from docling.exceptions import ConversionError` | `from docling_rs.exceptions import ConversionError` |
| `from docling.chunking import HybridChunker, HierarchicalChunker, BaseChunk` | `from docling_rs.chunking import HybridChunker, HierarchicalChunker, BaseChunk` |
| `from docling.utils.model_downloader import download_models` | `from docling_rs.utils.model_downloader import download_models` |
| `from docling_core.types.doc import …` (types, `ImageRefMode`, serializers) | unchanged — `docling_core` stays a dependency and `result.document` is its `DoclingDocument` |

Every name is *also* exported flat from the package root
(`from docling_rs import DocumentConverter, PdfPipelineOptions, …`) — the
submodules are aliases of the same objects, kept for drop-in parity. An import
docling.rs has no equivalent for (e.g. `docling.backend.*`, VLM pipeline
options) raises `ModuleNotFoundError` at the import line — the honest signal
that the code touches a not-ported area (see step 4).

A minimal script, before and after:

```python
# before                                         # after
from docling.document_converter import (         from docling_rs.document_converter import (
    DocumentConverter,                               DocumentConverter,
)                                                )
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
inert (`table_structure_options.mode`, …); inline formatting is
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

## Migrating from MarkItDown

[Microsoft MarkItDown](https://github.com/microsoft/markitdown) code moves over
by changing the import (#674):

```python
# was:  from markitdown import MarkItDown, StreamInfo
from docling_rs.markitdown import MarkItDown, StreamInfo

md = MarkItDown()
result = md.convert("report.docx")      # path, Path, http(s)/file/data URI,
print(result.markdown)                  # binary stream or requests.Response
print(result.title)

with open("scan.bin", "rb") as fh:      # no extension? hint it — or let the
    md.convert_stream(fh, stream_info=StreamInfo(extension=".pdf"))  # magic bytes decide
```

`convert` / `convert_local` / `convert_stream` / `convert_uri` /
`convert_url` / `convert_response`, `StreamInfo`, `DocumentConverterResult`
(`.markdown`, `.text_content`, `.title`, `str(result)`) and the exceptions
(`UnsupportedFormatException`, `FileConversionException`, …) behave as
MarkItDown's do. `python -m docling_rs.markitdown FILE [-o OUT] [-x EXT]
[-m MIME] [-c CHARSET] [--keep-data-uris]` stands in for the `markitdown`
CLI (stdin when no file is given).

What you get instead of MarkItDown's converters is docling.rs's: layout
analysis, TableFormer tables and OCR for PDFs and scans, the declarative
Office/HTML/EPUB/email backends — measured side by side in the main README's
[MarkItDown comparison](../../README.md#conformance-with-microsoft-markitdown)
(5–10× faster on Office/HTML, higher word/table/heading recall). What differs:

| MarkItDown | docling.rs facade |
|---|---|
| its own Markdown dialect | docling's Markdown (`<!-- image -->` placeholders, `- ` bullets, compact tables); byte-identical output is a non-goal |
| `keep_data_uris=True` | same: pictures as `data:` URIs |
| `llm_client` / `llm_model` / `llm_prompt` | same request (one `chat.completions.create` per picture); every embedded picture is described, the text lands in the Markdown and in `picture.meta.description`; an image input gets MarkItDown's `# Description:` section |
| XLSX `## <sheet>` headings | same (XLS not yet: its document carries no sheet groups) |
| PDFs via pdfminer (text layer) | the full pipeline once `docling_rs.download_models()` ran; until then the text layer, with a one-time warning |
| `magika` / `charset_normalizer` guesses | magic bytes + container structure; `charset_normalizer` when installed (it comes with `requests`), else the engine's own detection — BOM, UTF-8, a statistical guess for a non-Latin encoding (`chardetng`, #675), else windows-1252 |
| `exiftool_path`, `style_map`, `docintel_*`, `cu_*` | accepted, ignored with a warning (EXIF/metadata: #679) |
| plugins, `register_converter` | same (#680): `enable_plugins()` loads the `markitdown.plugin` entry points; a converter at `PRIORITY_SPECIFIC_FILE_FORMAT` or below runs before the engine, a generic one when the engine has no format for the input or fails on it. `docling_rs.register_backend` ([below](#pluggable-backends)) adds a format outside the facade too |
| picture alt text (`![alt](…)`) | kept (#676): the facade renders it — the `![alt](…)` of an image line, the text line above a `<!-- image -->` placeholder; `DocumentConverter(picture_alt_text=True)` elsewhere, and the document carries it always as the picture's description (`created_by="alt_text"`) |

`MarkItDown(docling_options={...})` passes options through to
`DocumentConverter` — `{"text_layer_only": True}` for pdfminer-speed PDFs (or `"auto"`: that path when the text layer carries the document, OCR when it does not),
`{"ocr_lang": "ch"}`, `{"do_picture_ocr": True}` — and `result.document` is
the converted `DoclingDocument` for anything Markdown cannot carry.

## Migrating from Marker

[Marker](https://github.com/datalab-to/marker) code moves over by changing
the imports — `docling_rs.marker` mirrors Marker 2.0's module layout:

```python
# was:  from marker.converters.pdf import PdfConverter
#       from marker.config.parser import ConfigParser
#       from marker.models import create_model_dict
#       from marker.output import text_from_rendered, save_output
from docling_rs.marker.converters.pdf import PdfConverter
from docling_rs.marker.config.parser import ConfigParser
from docling_rs.marker.models import create_model_dict
from docling_rs.marker.output import text_from_rendered, save_output

config_parser = ConfigParser({"output_format": "json", "page_range": "0-4", "paginate_output": True})
converter = PdfConverter(
    config=config_parser.generate_config_dict(),
    artifact_dict=create_model_dict(),
    processor_list=config_parser.get_processors(),
    renderer=config_parser.get_renderer(),
    llm_service=config_parser.get_llm_service(),
)
rendered = converter("report.pdf")            # or any DOCX / PPTX / XLSX / HTML / EPUB …
text, ext, images = text_from_rendered(rendered)
save_output(rendered, "out/report", "report")  # report.json + report_meta.json + the images
```

`PdfConverter` / `TableConverter` / `OCRConverter`, `create_model_dict`,
`ConfigParser`, the renderers and their output models (`MarkdownOutput`,
`HTMLOutput`, `JSONOutput`, `ChunkOutput`, `OCRJSONOutput`),
`text_from_rendered` / `save_output` / `output_exists`, `BlockTypes` and
`settings` behave as Marker's do; `python -m docling_rs.marker FILE|FOLDER`
stands in for `marker_single` and `marker` with the same options and output
layout. The full table of what is honoured, mapped and ignored is in
[docs/MARKER.md](https://github.com/docling-project/docling.rs/blob/master/docs/MARKER.md#the-api);
the numbers in the main README's
[Marker comparison](../../README.md#conformance-with-marker). What differs:

| Marker | docling.rs facade |
|---|---|
| Surya layout + OCR on PyTorch, a resident inference server per model, `fast` / `balanced` modes | docling's layout model, TableFormer and PP-OCR on ONNX Runtime in one process; `mode` accepted and ignored |
| LLM post-processing (`use_llm`, `llm_service`, …) | accepted, ignored with a warning: no LLM runs |
| Office / HTML / EPUB rendered to PDF (WeasyPrint) and OCR'd | parsed natively — real headings, tables, lists, no models needed |
| its own Markdown dialect | docling's Markdown; `![](_page_0_Picture_3.jpeg)` references and the `{page}----` separators as Marker writes them; byte-identical output is a non-goal |
| JSON block tree | same shape: Marker's ids, polygons in PDF points from the top-left, `section_hierarchy`, `<content-ref>` placeholders, base64 picture images; a figure with a caption is a `PictureGroup`, a table with one a `TableGroup` |
| `OCRConverter`: lines and characters | one `Line` per text block, no characters |
| `create_model_dict()` downloads Surya's models | downloads docling.rs's (~700 MB, once) when they are missing |

`PdfConverter(config={"docling_options": {...}})` passes options through to
`DocumentConverter` — `{"text_layer_only": True}` for Marker's
`--disable_ocr` speed class, `{"ocr_lang": "ch"}` — and `rendered.document`
is the converted `DoclingDocument` for anything the Marker shapes cannot carry.

## Migrating from MinerU

[MinerU 4](https://github.com/opendatalab/MinerU) code moves over by changing
the imports — `docling_rs.mineru` mirrors its `mineru.parser` / `mineru.render`
/ `mineru.types` modules:

```python
# was:  from mineru.parser import parse, ParseResult
#       from mineru.parser.writer import FileBasedDataWriter
#       from mineru.render import render, RenderFormat
from docling_rs.mineru.parser import parse, ParseResult
from docling_rs.mineru.parser.writer import FileBasedDataWriter
from docling_rs.mineru.render import render, RenderFormat

result = parse("report.pdf", tier="standard", ocr_mode="auto", page_range="1-5,r1")
md = result.markdown(add_markers=True)              # MinerU's Markdown, `---` between pages
content_list = render(result.middle_json, RenderFormat.CONTENT_LIST)
result.save(FileBasedDataWriter("out/report"))      # markdown.md, middle_json.json, structured_content.json, images/
restored = ParseResult.from_json(result.to_json())
```

`parse` / `parse_async`, `ParseResult`, `MinerUParser`, the writers, the
page-range grammar (`'1-5,8'`, `'r3-r1'`, `'all'`), the `render` targets
with their option classes, `BlockType` / `MiddleJson` / `PageInfo` and
`python -m docling_rs.mineru parse` (for `mineru-kit parse`) behave as
MinerU's do. The full map of what is honoured, mapped and left out is in
[docs/MINERU.md](https://github.com/docling-project/docling.rs/blob/master/docs/MINERU.md#the-api);
the numbers in the main README's
[MinerU comparison](../../README.md#conformance-with-mineru). What differs:

| MinerU | docling.rs facade |
|---|---|
| four tiers: `flash` (text layer), `basic` (ONNX small models), `standard` / `advanced` (+ a 1.2 B VLM through llama.cpp) | `flash` = the text layer; the other three = docling's one pipeline (layout, TableFormer, PP-OCR on ONNX Runtime), no VLM |
| Office / HTML / EPUB at `flash` only | every format at any tier, parsed natively |
| DocVortex's Middle JSON, validated strictly | the same shape (`docvortex.middle` 2.0: pages, indexed blocks, normalized boxes, inline spans, visual parents with body / caption / footnote children), producer `docling.rs`; round-trips through `from_dict` / `to_dict`, no strict validator |
| its own Markdown | the same conventions (`#` titles, HTML tables, `$$` equations, fenced code, `![](images/…)`, `---` pages in `FULL`); byte-identical output is a non-goal |
| LaTeX / DOCX / EPUB / PDF renders | `NotImplementedError` — `docling-rs --to` has docling's formats |
| `MinerUApiParser`, `mineru server`, the document library (`scan`, `search`, `read`) | not provided; the service is docling-serve, the MCP server `docling-rs mcp` |
| `basic` / `standard` models from Hugging Face or ModelScope on first use | docling.rs's models (~700 MB, once) — `docling_rs.download_models()`; a PDF converts from its text layer until then |

`parse(..., docling_options={...})` passes options through to
`DocumentConverter` — `{"ocr_lang": "ch"}`, `{"do_picture_ocr": True}` —
and `result.document` is the converted `DoclingDocument` for anything the
Middle JSON cannot carry.

## Try it locally

Needs a Rust toolchain (1.88+, the workspace MSRV) and Python ≥ 3.9.

```bash
cd crates/docling-py

# 1. Build + install into the CURRENT virtualenv (create one first):
python -m venv .venv && source .venv/bin/activate
pip install maturin
maturin develop --release          # compiles the Rust engine, installs `docling-rs`

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
| `DocumentConverter(format_options=None, *, allowed_formats=None, do_ocr=True, do_table_structure=True, force_full_page_ocr=False, no_text_panels=False, heading_hierarchy=False, do_picture_classification=False, do_code_enrichment=False, do_formula_enrichment=False, do_picture_ocr=False, picture_ocr_classes=None, picture_ocr_min_side=None, picture_ocr_scale=None, keep_picture_images=True, redact_pii=False, redact_mode=None, redact_kinds=None, redact_pattern=None, redact_images=None, fetch_images=False, use_web_browser=False, artifacts_path=None, ocr_lang=None, ocr_textline_orientation=None, asr_model=None, asr_lang=None, pipeline=None, vlm_endpoint=None, vlm_model=None, vlm_api_key=None, vlm_prompt=None, vlm_max_tokens=None)` | `DocumentConverter(allowed_formats=…, format_options=…)` | Pass `{InputFormat.PDF: PdfFormatOption(pipeline_options=PdfPipelineOptions(…))}` or the shorthand kwargs; `allowed_formats` restricts conversion; `artifacts_path` overrides the model cache dir. |
| `.convert(path \| url \| DocumentStream) -> ConversionResult` | `.convert(source)` | str / `pathlib.Path` / `http(s)://` URL (downloaded first; a YouTube watch URL becomes the video's title, description and caption transcript as timed items, #681; the format comes from the file name, else the response's `Content-Type`) / `DocumentStream`. Releases the GIL during conversion. |
| `.convert_all(sources, raises_on_error=True) -> Iterator[ConversionResult]` | same | lazily converts many sources; `raises_on_error=False` yields a `failure` result instead of raising |
| `.initialize_pipeline(format=None)` | same | pre-loads the PDF/image ML models so the first conversion isn't slow and later PDFs reuse the warm pipeline (no-op for non-ML formats; needs the models available) |
| `.convert_bytes(name, data)` | `DocumentStream` | extension of `name` drives format detection |
| `.convert_archive(source) -> Iterator[ArchiveItem]` | — (docling takes no archives) | every document inside a ZIP — a path, `bytes` or a `DocumentStream` — one `ArchiveItem` per entry: `outcome` `converted` (`result`) / `skipped` (`error` = the reason: unsupported type, nested archive, unsafe path, over a `DOCLING_RS_ZIP_MAX_*` limit) / `failed` (`error`); a broken document fails only itself (#557) |
| `InputFormat`, `PdfPipelineOptions`, `PdfFormatOption`, `AcceleratorOptions`, `TableFormerMode`, `DocumentStream`, `ImageRefMode` | same modules | docling-shaped config re-exported from `docling_rs` (see below) |
| `ConversionError` | `docling.exceptions.ConversionError` | raised on a failed conversion; caught by `convert_all(..., raises_on_error=False)` |
| `EncryptionError`, `PasswordRequiredError`, `WrongPasswordError` | — (docling.rs extension, #636) | subclasses of `ConversionError` an encrypted document raises: no password given / the given one is wrong (prompt for one) / a scheme docling.rs cannot decrypt (`EncryptionError` itself); `from docling_rs.exceptions import …` |
| `result.status` / `result.document` / `result.input.file` / `result.confidence` | same | `.status` is a `ConversionStatus` str-enum (`"success" / "partial_success" / "failure"`); `.document` is a genuine `docling_core` `DoclingDocument`; `.confidence` is a `ConfidenceReport` with real 1-based page-number keys for PDF/image conversions, and an empty report otherwise |
| `document.export_to_markdown(...)` | same | docling-core's own method — all of docling's params (`image_placeholder`, `page_break_placeholder`, …) apply |
| `document.export_to_dict()` / `export_to_json()` / `export_to_doctags()` | same | docling-core's own serializers over the wire format |
| `document.save_as_markdown(p)` / `save_as_json(p)` / chunkers | same | anything `docling_core` offers on a `DoclingDocument` works, since it *is* one |
| `docling_rs.email_attachments(source, *, max_entries=None, max_entry_size=None, max_total_size=None) -> list[EmailAttachment]` | — (docling lists attachment names only) | the attachments of an `.eml` / `.msg` (#561) — a path, the message bytes or a `DocumentStream` — as `EmailAttachment(index, name, content_type, format, size, inline, skipped, data)`: `data` the payload when kept (`None` over a limit or without a payload — a reference, an OLE object), `format` the `InputFormat` it converts as (`None` with `skipped` saying why: unsupported type, nested archive, …; the bytes stay available), `name` a safe base name, a forwarded message an `.eml` entry carrying the nested message. `converter.convert(att.as_stream())` converts one. Limits are byte counts (defaults 10 000 attachments, 256 MiB each, 1 GiB in all). |
| `docling_rs.detect_format(source) -> InputFormat \| str \| None` | — (docling routes by name; MarkItDown asks magika) | the format the bytes evidently are (#672: PDF, the Office / ODF / EPUB / iWork packages, the OLE files, RTF, HTML, images, DjVu, audio and video containers, WordPerfect, WebVTT, DocTags, SVG, AbiWord, XML, JSON and notebooks, MIME messages, LaTeX) from bytes, a path or a `DocumentStream`; `None` for text without a signature. What the converter itself does for a file with no or an unknown extension. |
| `docling_rs.download_models()` | `docling-tools models download` | idempotent; `~/.cache/docling.rs` or `$DOCLING_RS_CACHE_DIR`; INT8 models fetched when hosted and preferred automatically (`DOCLING_RS_FP32=1` opts out); `force=True` re-downloads a stale cache after a model re-publish; `asr_model="parakeet_tdt_0.6b_v3"` (or a list, or `"whisper_tiny"` / the Whisper presets — `docling_rs.models.ASR_MODELS`) adds speech-recognition models for audio/video, none by default; `pdf_models=False` skips the PDF/image models |

Model/env resolution order: explicit `DOCLING_*` env vars → the process CWD
(`models/`, matching the CLI — so a repo checkout uses its own exports) →
the cache dir set by `ensure_env()` (called by the constructor). PDF pages
are parsed and rendered in pure Rust — no native PDF library is fetched.

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

`document_timeout=90.0` (docling's `PipelineOptions.document_timeout`, #497;
also accepted as `pipeline_options.document_timeout`) is a per-document
budget in seconds for the PDF pipeline, checked between pages: once spent, the
pages done so far are the document, `result.status` is `PARTIAL_SUCCESS` and
`result.errors` holds the reason as docling's `ErrorItem`.

`progress_callback=` / `show_progress=True` are docling's conversion
progress events (docling 2.139, #783): the callback receives every
conversion's events — document start and end, one `PageCompletedProgress`
per PDF / image page with `completed_pages` / `total_pages` (a page the
budget cut off comes with `success=False`), the picture passes'
`EnrichmentProgress` — as the `docling_rs.progress` classes, docling's
names; `convert` / `convert_all` take a per-call `progress_callback` too,
`convert_all` numbering the events' `document_index`. `show_progress=True`
prints the engine's progress bars to stderr with no code:

```python
from docling_rs import DocumentConverter
from docling_rs.progress import PageCompletedProgress   # was: docling.datamodel.progress

def on_progress(event):
    if isinstance(event, PageCompletedProgress):
        print(f"{event.document_name}: page {event.completed_pages}/{event.total_pages}")

DocumentConverter(progress_callback=on_progress).convert("report.pdf")
```

For scanned pages, `ocr_lang="en"|"ch"` picks the OCR recognition model (`en`
is the default — proper Latin word spacing; `ch` is the multilingual
docling-conformance model). BCP-47 tags for either language resolve to the
same two models — `"en-US"`, `"eng"`, `"zh"`, `"zh-Hans"`, `"zh-TW"`, with or
without docling's `iso:` prefix (#388; script and region subtags are ignored,
other languages raise `ValueError`). docling-shaped `ocr_options.lang` lists
map onto the same switch the same way (`["english"]` → `en`, `["chinese"]` /
`["iso:zh-Hans"]` → `ch`; the first entry wins).

The Rust engine acts on `do_ocr`, `do_table_structure`, the opt-in enrichment
flags `do_picture_classification` / `do_code_enrichment` /
`do_formula_enrichment` (the picture classifier is fetched by the default
`scripts/install/download_dependencies.sh` run; the code/formula models need
its `--enrich` flag), `do_picture_ocr` (#645: OCR the pictures embedded in
DOCX/PPTX/HTML/… and video frames with the engine's OCR models — the text
lands in `picture.meta.description` + the `description` annotation, exactly
where `do_picture_description` puts a VLM's; `picture_ocr_classes` (a list
or comma-separated string of DocumentFigureClassifier labels) and
`picture_ocr_min_side` filter the pictures, `picture_ocr_scale` sets their
OCR resolution in px per picture pixel under RapidOCR's 2000 px cap (#666;
`1.0` = native pixels), `ocr_textline_orientation=True` adds RapidOCR's
text-line orientation handling (oriented crops, tall lines turned 90°, the
0/180 classifier — pages too; docling's `RapidOcrOptions.use_cls` maps onto
it), `keep_picture_images=False` drops the image bytes afterwards; video frame sampling is tunable too, #647:
`video_frames="all"` / `docling_rs.VIDEO_FRAMES_ALL` keeps every distinct
cut, `video_scene_threshold` (0.27) sets the cut score, `video_frame_max_side`
downscales frames inside ffmpeg and `video_frame_dedupe` (4–6) collapses a
re-lit slide — frames stream one at a time through the OCR hook), and
its `--enrich` flag), `redact_pii` (#621, a docling.rs extension: personal
data — e-mail, phone, card numbers, IBANs, IPs, URL credentials, national
IDs, and names / organizations / locations with the NER model under
`.models/ner/` — is replaced in the document model before export, so
`export_to_markdown()`, `export_to_dict()` and the chunkers all see the
redacted text; `redact_mode` is `label` | `pseudonym` | `fixed:<text>`,
`redact_kinds` a list or comma-separated string, `redact_pattern` a list or
newline-joined string of `NAME=REGEX`, `redact_images` `drop` | `box_out` |
`keep`; `result.redaction` is the counts per label), and
`accelerator_options.num_threads` (→ ONNX Runtime intra-op threads via
`DOCLING_RS_PDF_THREADS`), and the image outputs (#520):
`generate_page_images` keeps each page's render as `document.pages[n].image`
(so docling-core's `TableItem.get_image(doc)` works), and `images_scale` sets
the picture-crop / page-image resolution once `generate_picture_images` or
`generate_page_images` is on — docling renders images only then; picture
crops are always extracted here, at the engine's 2.0 px/pt otherwise. Every
picture's `image.dpi` is 72·scale (#519). `convert(source, page_range=(a, b))`
narrows a PDF to that page window per call, as in docling (#518). The
remaining `PdfPipelineOptions` fields (`table_structure_options.mode`, …) are
accepted for API compatibility but do not change the pipeline. `InputFormat`,
`DocumentStream` and `ImageRefMode` are re-exported too (the last straight from
`docling_core`, for `export_to_markdown(image_mode=…)`). A GPU
`accelerator_options.device` (`CUDA`/`MPS`) on a wheel without that provider
warns and falls back to CPU: the prebuilt PyPI wheels ship ONNX Runtime with
the CPU execution provider only (`docling-rs-cuda` adds CUDA). The engine
itself supports CUDA / TensorRT / DirectML / CoreML behind cargo features
(issue #74) — build the wheel from source with e.g. `maturin build --features
cuda` (or `--features coreml` on macOS) and select the provider per process
with `DOCLING_RS_EP=cuda` (see the workspace README). CoreML is opt-in (#602):
a CoreML wheel converts on CPU until `DOCLING_RS_EP=coreml` is set or
`accelerator_options.device` is `MPS`, which maps to it (an explicit
`DOCLING_RS_EP` still wins). Under a GPU provider the engine picks the fp32 models over the int8
ones on its own: `ensure_env()` hands the model cache over as
`DOCLING_RS_MODELS_DIR` and pins no file (#602).

## Pluggable backends

A format docling.rs does not read, or one you want read your way, without
forking (#680): register a function that turns the bytes into Markdown or a
`DoclingDocument`.

```python
import docling_rs
from docling_rs import DocumentConverter

@docling_rs.register_backend(extensions=["rtf2"], media_types=["application/x-rtf2"],
                             sniff=lambda data: data.startswith(b"{\\rtf2}"))
def read_rtf2(data: bytes, name: str) -> str:
    return "# " + name + "\n\n" + data.decode().split("\n", 1)[1]

result = DocumentConverter().convert("notes.rtf2")
```

- `fn(data, name)` returns Markdown (`str`, read by the engine's Markdown
  backend) or a docling-core `DoclingDocument` (or its dict).
- A backend claims a source by extension, media type (`convert_bytes(...,
  media_type=)`, the facade's `StreamInfo`), *or* content (`sniff`). Registered
  backends are tried before the built-in formats, the highest `priority`
  first, so one can also take a format over (`extensions=["csv"]`).
- `docling_rs.register_backend` applies to converters created afterwards;
  `DocumentConverter().register_backend(fn, extensions=[...])` to one
  converter. `registered_backends()` and `supported_extensions()` list them.
- The document goes through the converter's usual steps: output options,
  enrichment, picture descriptions, PII redaction, chunking.
- A backend's exception fails the conversion (`ConversionError`, naming the
  backend); it is not retried as a built-in format.
- Not routed: entries of a `.zip` (the archive is classified by the engine's
  extension table), and the CLI, serve and MCP binaries, which have no
  Python to call. In Rust: `DocumentConverter::register_backend`.

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
and `chunk.meta.doc_items` holds the items' JSON-pointer refs (`"#/texts/12"`)
rather than the item objects — refs into *your* document: the engine chunks a
re-imported copy whose numbering drifts (empty items dropped, HTML/DOCX item
trees, inline runs), so each chunk item is mapped back by kind, reading order
and text; an item that cannot be placed is left out rather than mis-pointed.
`chunk.meta.export_json_dict()` gives docling's `DocMeta` JSON shape. With no
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

## LangChain

`docling_rs.langchain` is the port of docling's
[langchain-docling](https://github.com/docling-project/docling-langchain)
integration — same classes, same parameters, same output — running on the
Rust engine (no PyTorch):

```bash
pip install "docling-rs[langchain]"      # or: pip install langchain-docling-rs
```

```python
# was:  from langchain_docling import DoclingLoader
from docling_rs.langchain import DoclingLoader, ExportType
# the same classes under LangChain's package naming: from langchain_docling_rs import DoclingLoader

docs = DoclingLoader(file_path=["https://arxiv.org/pdf/2408.09869", "notes.docx"]).load()
docs[0].page_content                      # heading path + chunk text, ready to embed
docs[0].metadata["source"]                # the input
docs[0].metadata["dl_meta"]               # docling's chunk meta: items + page/bbox provenance, headings, origin

DoclingLoader(file_path="report.pdf", export_type=ExportType.MARKDOWN).load()  # one Document per input
```

| Parameter | Default | |
|---|---|---|
| `file_path` | — | path or URL, or an iterable of them |
| `export_type` | `ExportType.DOC_CHUNKS` | one LangChain `Document` per chunk; `ExportType.MARKDOWN` = one per input |
| `chunker` | `docling_rs.chunking.HybridChunker()` | any chunker with `chunk()` / `contextualize()` — the native ones or docling's own |
| `converter` | `docling_rs.DocumentConverter()` | configure it as usual (`do_ocr=False`, `ocr_lang=…`, …); docling's `DocumentConverter` / `DoclingServiceClient` work too |
| `convert_kwargs` | `{}` | extra kwargs for `converter.convert(source=…)` |
| `md_export_kwargs` | `{"image_placeholder": ""}` | kwargs for `export_to_markdown()` (Markdown mode) |
| `meta_extractor` | `MetaExtractor()` | subclass `BaseMetaExtractor` to shape `metadata` |

It reproduces langchain-docling's own test expectations exactly (its
fixtures run in `tests/test_langchain.py`). The PyPI distribution
[`langchain-docling-rs`](./langchain/README.md) is the same loader under the
`langchain-<provider>` name LangChain's integration listing expects: it pins
this package's wheel and re-exports `docling_rs.langchain`. The default chunker is the native
hybrid chunker with docling's defaults (all-MiniLM-L6-v2 tokenizer, 256
tokens); its `tokenizer.json` (~0.5 MB) is fetched into the model cache on
the first load when not already there — docling likewise pulls it from the
Hugging Face Hub. `lazy_load()` converts and chunks one input at a time.

## LlamaIndex

`docling_rs.llamaindex` is the port of LlamaIndex's
[`llama-index-readers-docling`](https://github.com/run-llama/llama_index/tree/main/llama-index-integrations/readers/llama-index-readers-docling)
— the same `DoclingReader`, on the Rust engine:

```bash
pip install "docling-rs[llamaindex]"     # or: pip install llama-index-readers-docling-rs
```

```python
# was:  from llama_index.readers.docling import DoclingReader
from docling_rs.llamaindex import DoclingReader
# the same class under LlamaIndex's package naming: from llama_index.readers.docling_rs import DoclingRsReader

reader = DoclingReader()                                    # Markdown per input (default)
docs = reader.load_data(file_path=["https://arxiv.org/pdf/2408.09869", "notes.docx"])

reader = DoclingReader(export_type=DoclingReader.ExportType.JSON)   # docling's native JSON, for a Docling node parser
```

```python
from llama_index.core import SimpleDirectoryReader

docs = SimpleDirectoryReader(input_dir="docs/", file_extractor={".pdf": reader, ".docx": reader}).load_data()
```

| Parameter | Default | |
|---|---|---|
| `export_type` | `ExportType.MARKDOWN` | one `Document` per input holding its Markdown; `ExportType.JSON` = its `DoclingDocument` as JSON |
| `doc_converter` | `docling_rs.DocumentConverter()` | configure it as usual (`do_ocr=False`, `ocr_lang=…`, `text_layer_only=True`, …) |
| `md_export_kwargs` | `{"image_placeholder": ""}` | kwargs for `export_to_markdown()` |
| `id_func` | a random UUID per document | `(doc, file_path) -> str` |

`load_data` / `lazy_load_data(file_path, extra_info=None)` take a path or URL
or an iterable of them; `extra_info` becomes each document's `metadata` (a
copy per document). `SimpleDirectoryReader` adds its own file metadata. The
PyPI distribution [`llama-index-readers-docling-rs`](./llamaindex/README.md)
is the same reader under the `llama_index.readers.<name>` namespace every
reader package uses: it pins this package's wheel and re-exports
`docling_rs.llamaindex` (`DoclingRsReader`, and `DoclingReader` as the
upstream name).

**Picture descriptions with any LangChain chat model** — langchain-docling's
`PictureDescriptionLangChainOptions`, set on the pipeline options as in
docling (`do_picture_description`; `allow_external_plugins` is accepted but
not needed):

```python
from langchain_openai import ChatOpenAI
from docling_rs import DocumentConverter, InputFormat, PdfFormatOption, PdfPipelineOptions
from docling_rs.langchain import PictureDescriptionLangChainOptions

opts = PdfPipelineOptions(do_picture_description=True)
opts.picture_description_options = PictureDescriptionLangChainOptions(
    llm=ChatOpenAI(model="gpt-4o-mini"), prompt="Describe the image in three sentences.", provenance="gpt-4o-mini"
)
doc = DocumentConverter(format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)}).convert("paper.pdf").document
[p.meta.description.text for p in doc.pictures if p.meta and p.meta.description]
```

The engine already embeds every picture's crop, so the description pass runs
on the converted document (`docling_rs.picture_description`, docling's
selection rules: pictures under 5% of their page skipped,
`classification_allow` / `classification_deny` filters, batches of 8) and
applies to every input format the converter handles, not only PDF.

Runnable examples — loader basics, an agentic RAG with page citations, picture
descriptions — are in [`examples/langchain/`](./examples/langchain/).

## AutoGen

> AutoGen is in maintenance mode; Microsoft's successor is Agent Framework —
> new projects want [`docling_rs.agent_framework`](#microsoft-agent-framework).

`docling_rs.autogen` puts documents in front of
[AutoGen](https://github.com/microsoft/autogen) agents — the indexing half
of AutoGen's RAG recipe and a document-reading tool, both on the Rust
engine:

```bash
pip install "docling-rs[autogen]"
```

```python
from autogen_agentchat.agents import AssistantAgent
from autogen_core.memory import ListMemory            # or autogen-ext's ChromaDBVectorMemory, …
from docling_rs.autogen import DoclingIndexer, DoclingConvertTool

memory = ListMemory()
await DoclingIndexer(memory).index_documents(["report.pdf", "https://arxiv.org/pdf/2408.09869"])
agent = AssistantAgent("assistant", model_client=client, memory=[memory], tools=[DoclingConvertTool()])
```

`DoclingIndexer(memory, converter=…, chunker=…, mime_type=…, metadata=…)`
converts each source, chunks it (the native `HybridChunker` with docling's
defaults, or any chunker with `chunk()` / `contextualize()`) and adds one
`MemoryContent` per chunk: the heading path plus the chunk text, and flat
metadata a vector store accepts — `source`, `headings` (`A > B`) and `pages`
(`3,4`) — or whatever a `metadata(source, chunk)` function returns.
`indexer.chunks(source)` yields them without adding, for batching into a
store yourself.

`DoclingConvertTool(converter=…, max_chars=40_000)` is a `FunctionTool`
named `convert_document(source)` that returns the document as Markdown
(cut with a `[… N more characters]` tail past `max_chars`, so a manual does
not flood the context); the conversion runs on a worker thread, as
`FunctionTool` runs every synchronous function. The example in
[`examples/autogen/`](./examples/autogen/) indexes a paper into a Chroma
memory and answers questions with page citations.

## Microsoft Agent Framework

`docling_rs.agent_framework` does the same for
[Microsoft Agent Framework](https://github.com/microsoft/agent-framework),
AutoGen's successor (AutoGen is in maintenance mode) — documents into its
vector stores, retrieval for an agent, and the document-reading tool:

```bash
pip install "docling-rs[agent-framework]" agent-framework-openai
```

```python
from agent_framework import Agent, InMemoryCollection        # or a connector's collection
from agent_framework.openai import OpenAIChatClient, OpenAIEmbeddingClient
from docling_rs.agent_framework import (
    DoclingChunk, DoclingContextProvider, DoclingConvertTool, DoclingIndexer,
)

collection = InMemoryCollection(DoclingChunk, embedding_generator=OpenAIEmbeddingClient(model="text-embedding-3-small"))
await DoclingIndexer(collection).index_documents(["report.pdf", "https://arxiv.org/pdf/2408.09869"])
agent = Agent(
    client=OpenAIChatClient("gpt-5.4-mini"),
    context_providers=[DoclingContextProvider(collection)],   # relevant chunks, each turn
    tools=[DoclingConvertTool()],                              # read any other document
)
```

`DoclingChunk` is a `@vectorstoremodel` record — `id`, `text` (the heading
path plus the chunk text, full-text indexed), `source`, `headings` (`A > B`),
`pages` (`3,4`), `chunk_index` and the `embedding` the collection generates
from the text on upsert. It is sized for 1536-dimensional embeddings;
`docling_chunk_model(768)` builds the same record for another embedding
model. The key is a UUID derived from the source and the chunk's position,
so indexing a document again overwrites its chunks rather than duplicating
them (a document that shrank keeps its old tail until you delete it).

`DoclingIndexer(collection, converter=…, chunker=…, metadata=…, record=…,
batch_size=64)` converts each source on a worker thread, chunks it (the
native `HybridChunker` with docling's defaults, or any chunker with
`chunk()` / `contextualize()`) and upserts the records in batches — one
embedding request each; `indexer.records(source)` yields them without
upserting, and `record=` maps the fields onto a record type of your own.

`DoclingContextProvider(collection, top=4, score_threshold=…, filter=…)`
searches the collection with the user's messages before every run and adds
the best chunks to the context, each under a citation line
(`[report.pdf, pp. 3,4 — Results > Tables]`) — what an AutoGen
`AssistantAgent(memory=[…])` did. For agentic retrieval instead,
`docling_search_tool(collection)` is a `search_documents(query)` tool with
the same citations (Agent Framework's `create_vector_search_tool`).
Agent Framework marks its vector-store API experimental, and warns when it
is used.

`DoclingConvertTool(converter=…, max_chars=40_000, approval_mode=…)` is the
AutoGen tool's counterpart: `convert_document(source)` returns the document
as Markdown, run on a worker thread. It reads any path or URL the model
names with the process's file access — where the conversation carries
untrusted text, set `approval_mode="always_require"`. The example in
[`examples/agent_framework/`](./examples/agent_framework/) indexes a paper
and answers with page citations.

## MCP server

The wheel also carries docling.rs's Model Context Protocol server (#669) —
the one `docling-rs mcp` runs, MarkItDown's `convert_to_markdown(uri)` tool
included — as `python -m docling_rs.mcp --root ~/Documents`. The PyPI
launcher [`docling-rs-mcp`](./mcp/README.md) pins this package and names the
command, so agents start it with `uvx docling-rs-mcp --root ~/Documents`;
`--download-models` fetches the layout, OCR and table models into this
package's cache once. Configuration, tools and the security model: the
[CLI README](../docling-cli/README.md#mcp-server--docling-rs-mcp).

## Not covered (yet)

The *local in-process* full-VLM conversion pipeline (SmolDocling) and
per-format *backend* selection. The **remote** VLM pipeline (#304) *is*
covered: `DocumentConverter(pipeline="vlm", vlm_endpoint=…, vlm_model=…)`
(plus optional `vlm_api_key` / `vlm_prompt` / `vlm_max_tokens`, all falling
back to the `DOCLING_RS_VLM_*` environment) sends each PDF page / image to
any OpenAI-compatible vision endpoint instead of running the local ML stack —
no models needed; a bad configuration raises `ValueError` at construction and
a failed conversion raises the catchable `ConversionError`. GPU inference is engine-side only: compiled in via cargo features
(#74) and selected with `DOCLING_RS_EP`, not via `accelerator_options.device`,
and absent from the prebuilt CPU wheels. The document carries rendered text for
inline formatting rather than structured `formatting` fields — see
`docs/MIGRATION.md` §4 for the documented divergences.

## Publishing

A release reaches PyPI when the maintainer publishes it — the
[`release`](../../.github/workflows/release.yml) workflow (`gh workflow run
release.yml -f tag=v<version>`) runs every channel: PyPI, npm, the images and
the CLI binaries. Its PyPI channel is the
[`pypi-publish`](../../.github/workflows/pypi-publish.yml) GitHub Actions
workflow, which builds an `abi3` wheel per platform (Linux x86-64/arm64 as
`manylinux_2_28`, Linux s390x cross-compiled with maturin — it loads ONNX
Runtime at run time, see the repo README's IBM Z section — and Windows x86-64;
one wheel covers every Python ≥ 3.9) plus an
sdist and the `docling-rs-cuda` wheel, and uploads them to PyPI at the release's
version — then the pure-Python [`docling-rs-mcp`](./mcp/) launcher, pinned to
that version, as its own PyPI project, and the MCP registry entry
(`docs/MCP_REGISTRIES.md`). It runs **only by hand** (`workflow_dispatch`):
a version is tagged on every merge, which would outgrow PyPI's per-project
size limit (below) within weeks, so the maintainer publishes the releases
worth it. Run alone, it publishes PyPI only — e.g. to repair a failed PyPI
channel. Files already on PyPI are skipped, so re-runs are safe — within 14
days of a release's first upload, after which PyPI accepts no new files for it:

```bash
# From the Actions tab, or:
gh workflow run release.yml                                         # every channel, the newest release
gh workflow run release.yml -f tag=v2.23.0                          # every channel, a given release
gh workflow run pypi-publish.yml -f tag=v2.23.0                     # PyPI only, with docling-rs-cuda
gh workflow run pypi-publish.yml -f tag=v2.23.0 -f cuda_wheel=false # without it
gh workflow run pypi-publish.yml                                    # the dispatched ref at the workspace version
```

No secrets: it publishes via PyPI **Trusted Publishing** (OIDC), like
docling-core — no API token is stored or rotated (the trusted publisher is
registered on PyPI; manage it at *Project → Manage → Publishing*). Re-runs are
idempotent (`skip-existing`). The macOS wheel is Apple Silicon only (arm64,
macOS 13+; #816); Intel macs install the sdist, which compiles from source. The ONNX runtime is
bundled in the wheel; the ONNX models are fetched at runtime by `download_models()`.

**PyPI's size limit.** PyPI caps the total of all files a project keeps at
10 GiB unless it [raises it on request](https://docs.pypi.org/project-management/storage-limits#requesting-a-project-size-limit-increase);
a release adds ~100 MB to `docling-rs` and ~63 MB to `docling-rs-cuda`. An
upload past the cap is refused file by file after the earlier ones went
through (v2.22.0 got its aarch64 wheel only), so the publish job first runs
`scripts/ci/pypi_room.py`: it fails before uploading anything when a project
would overflow and warns past 90 %. To make room, a project owner deletes
superseded releases — permanently; pins to them stop resolving — e.g. all of
0.x and 1.x but their last versions, with
[`pypi-cleanup`](https://github.com/arcivanov/pypi-cleanup) (a dry run
without `--do-it`; `--query-only` lists the versions without logging in):

```bash
uvx pypi-cleanup -u <pypi-user> -p docling-rs -r '^0\.(?!53\.3$)' -r '^1\.(?!104\.3$)' --do-it
```

then re-run the failed publish job. When PyPI raises a limit, record it in the
repository variable `PYPI_SIZE_LIMIT_GIB` (`docling-rs=20,docling-rs-cuda=15`).

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
**CUDA 13 + cuDNN 9 must be installed on the system** — the wheel ships the
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
