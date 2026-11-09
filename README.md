# docling.rs

<p align="center">
  <img src="https://raw.githubusercontent.com/docling-project/docling.rs/refs/heads/master/docs/assets/logo.svg" alt="docling.rs — a duck feeding a document into a meat grinder" width="240">
</p>

<p align="center">
  <a href="https://github.com/docling-project/docling.rs/actions/workflows/ci.yml"><img src="https://github.com/docling-project/docling.rs/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://docling-project.github.io/docling.rs/"><img src="https://img.shields.io/badge/demo-live-brightgreen" alt="Live browser demo"></a>
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/github/license/docling-project/docling.rs" alt="License MIT"></a>
  <a href="https://crates.io/crates/docling"><img src="https://img.shields.io/crates/v/docling?logo=rust" alt="crates.io version"></a>
  <a href="https://pypi.org/project/docling-rs/"><img src="https://img.shields.io/pypi/v/docling-rs?logo=pypi&logoColor=fff" alt="PyPI version"></a>
  <a href="https://www.npmjs.com/package/docling.rs"><img src="https://img.shields.io/npm/v/docling.rs?logo=npm&logoColor=fff" alt="npm version"></a>
  <a href="https://github.com/docling-project/docling.rs/releases"><img src="https://img.shields.io/github/v/release/docling-project/docling.rs?label=release&logo=github" alt="GitHub release"></a>
  <a href="https://crates.io/crates/docling"><img src="https://img.shields.io/crates/d/docling?label=crates.io%20downloads" alt="crates.io downloads"></a>
  <a href="https://pepy.tech/projects/docling-rs"><img src="https://static.pepy.tech/badge/docling-rs/month" alt="PyPI downloads/month"></a>
  <a href="https://www.npmjs.com/package/docling.rs"><img src="https://img.shields.io/npm/dm/docling.rs?label=npm%20downloads" alt="npm downloads/month"></a>
</p>

**docling.rs** converts documents — PDF, Office, HTML, scans, audio — into
Markdown, docling JSON and the other shapes an LLM or a RAG pipeline can
read. It is a Rust port of IBM's
[docling](https://github.com/docling-project/docling): the same output,
byte-for-byte, from one static binary. No Python, no PyTorch.

Pulling text out of a PDF is easy. Keeping its *structure* — headings in
order, tables as tables, scanned pages read — takes layout analysis, table
recognition and OCR. docling has the models for that, and a Python stack to
match. docling.rs runs the same models from a single binary: PDF 4.3× faster
than Python docling at under half the RAM, Office and HTML 20–60× faster, and
tables that survive — cell recall 0.94 where MarkItDown gets 0.31
([numbers](#conformance-with-microsoft-markitdown),
[method](./docs/PDF_CONFORMANCE.md#performance--review--profiling-notes)).

**Coming from [MarkItDown](https://github.com/microsoft/markitdown)?**
`pip install docling-rs`, swap one import —
`from docling_rs.markitdown import MarkItDown` — and keep its API, every
format it converts included, with real layout analysis, tables and OCR
behind it: [guide](./docs/MARKITDOWN.md),
[measured side by side](#conformance-with-microsoft-markitdown).

**Coming from [Marker](https://github.com/datalab-to/marker)?**
`pip install docling-rs`, swap the imports —
`from docling_rs.marker.converters.pdf import PdfConverter` — and keep its
API: `create_model_dict`, `text_from_rendered`, the JSON block tree, chunks,
`marker_single`'s CLI and output layout, with docling's layout analysis,
TableFormer and OCR behind it and no PyTorch or inference server:
[guide](./docs/MARKER.md), [measured side by side](#conformance-with-marker).

**Coming from [MinerU](https://github.com/opendatalab/MinerU)?**
`pip install docling-rs`, swap the imports —
`from docling_rs.mineru.parser import parse` — and keep MinerU 4's SDK:
`ParseResult`, `markdown()`, the Middle JSON, Content Lists and Structured
Content, `save()`'s layout and `mineru-kit parse`, with docling's layout
analysis, TableFormer and OCR behind it and no DocVortex or llama.cpp:
[guide](./docs/MINERU.md), [measured side by side](#conformance-with-mineru).


## Install

```bash
# CLI + models (Linux x64/arm64/s390x + macOS arm64 prebuilt; builds from source elsewhere)
curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/install.sh | bash
docling-rs example.pdf          # Markdown on stdout

# or from crates.io; --cli puts the models where the installed binary looks
cargo install docling-cli
curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh -s -- --cli

pip install docling-rs          # Python — drop-in for docling's, MarkItDown's, Marker's and MinerU's APIs
npm install docling.rs          # Node.js / Bun
cargo add docling               # Rust library

# containers, models baked in (-cuda variants for NVIDIA)
docker run --rm -v "$PWD:/data" ghcr.io/docling-project/docling-rs paper.pdf
docker run -p 127.0.0.1:5001:5001 ghcr.io/docling-project/docling-rs-serve
```

Office, HTML, Markdown and e-mail need no models. PDF, images and audio need
the ONNX models (layout, OCR, TableFormer, Whisper; under 1 GB): `install.sh`,
`--cli` and the containers bring them along; with `pip`, run
`python -c "import docling_rs; docling_rs.download_models()"` once; with `npm`
or a checkout, run `download_dependencies.sh` where you convert
([details](#getting-the-ml-models)). Windows: [`docs/WINDOWS.md`](./docs/WINDOWS.md).

**▶ [Try it in your browser](https://docling-project.github.io/docling.rs/)**
first — the converter as wasm, OCR included; nothing leaves your device.

## Use

```bash
docling-rs report.pdf > report.md               # Markdown (streamed page by page)
docling-rs report.pdf --to json                 # docling JSON; also html, text, latex, dclx, chunks, images
docling-rs --input ./docs --output ./out        # a directory tree; --jobs N in parallel
docling-rs https://example.com/paper.pdf        # URLs, data: URIs and stdin (`-`) too
docling-rs https://www.youtube.com/watch?v=ID   # a YouTube video: title, description, caption transcript (--to vtt keeps the timing)
docling-rs --help                               # every flag: OCR, page ranges, images, enrichment, redaction …
```

```python
from docling_rs import DocumentConverter       # was: from docling.document_converter import …
doc = DocumentConverter().convert("report.pdf").document
print(doc.export_to_markdown())
```

```js
import { convertFileAsync } from 'docling.rs'
const { content } = await convertFileAsync('report.pdf', { to: 'markdown' })
```

```rust
use docling::{DocumentConverter, SourceDocument};
let result = DocumentConverter::new().convert(SourceDocument::from_file("report.pdf")?)?;
println!("{}", result.document.export_to_markdown());
```

```bash
curl -F file=@report.pdf 'localhost:5001/v1/convert?to=md'   # the server; OpenAPI at /openapi.yaml
```

Same options, same outputs on every surface — every option in
[`docs/OPTIONS.md`](./docs/OPTIONS.md).

## Highlights

- **From Python docling:** `from docling_rs import DocumentConverter`;
  `result.document` is a genuine `DoclingDocument`
  ([migration notes](./crates/docling-py/README.md#migrating-from-python-docling)).
- **Every docling format, and a few it lacks** — the table under
  [Status](#status), each validated against live docling
  ([Conformance](#conformance-with-python-docling)).
- **Every output docling has:** Markdown (docling's, or a stricter dialect),
  docling JSON, HTML, plain text, DocLang, LaTeX, Pandoc AST (→ DOCX, EPUB,
  Typst… through pandoc), WebVTT subtitles, RAG chunks and page images
  ([output formats](./crates/docling/README.md#output-formats)).
- **From any language:** Rust, Python, Node.js / Bun, the browser (wasm,
  fully client-side), C, C++, C#, Go, Java and Swift through one C header,
  any HTTP client, the shell
  ([Integration](#integration--using-doclingrs-from-your-language)).
- **A drop-in server for docling-serve:** Open WebUI uses it as its Docling
  content-extraction engine — point *Docling Server URL* at
  `http://<host>:5001` — and n8n, Dify and LangChain clients work unchanged
  ([`docling-rs serve`](#http-conversion-api--docling-rs-serve),
  [Open WebUI setup](./crates/docling-serve/README.md#open-webui)).
- **AI agents (MCP):** `docling-rs mcp` gives Claude Desktop, Claude Code,
  Cursor or VS Code MarkItDown's `convert_to_markdown` tool, plus `convert`
  for JSON, chunks and the rest — `uvx docling-rs-mcp`, or
  `claude mcp add docling -- docling-rs mcp` ([MCP server](#mcp-server--docling-rs-mcp)).
- **LangChain:** `pip install "docling-rs[langchain]"` — langchain-docling's
  `DoclingLoader` on the Rust engine, one import to swap
  ([details](./crates/docling-py/README.md#langchain)).
- **AutoGen:** `pip install "docling-rs[autogen]"` — `DoclingIndexer` fills
  any AutoGen memory with chunks that carry their pages, `DoclingConvertTool`
  lets an agent read documents ([details](./crates/docling-py/README.md#autogen)).
- **Microsoft Agent Framework:** `pip install "docling-rs[agent-framework]"` —
  AutoGen's successor: `DoclingIndexer` fills any Agent Framework vector
  store, `DoclingContextProvider` puts the relevant chunks with their pages
  into each turn ([details](./crates/docling-py/README.md#microsoft-agent-framework)).
- **Your own formats:** register a backend for a format docling.rs does not
  read — or to read one of its formats your way — from Rust, Python
  (`@docling_rs.register_backend(extensions=["rtf2"])`, and MarkItDown's
  `register_converter` and plugins) or Node.js, ahead of the built-in formats
  ([pluggable backends](./crates/docling/README.md#pluggable-backends)).
- **RAG:** `docling-rag` — chunk, embed and search with a REST API and a web
  UI ([RAG subsystem](#rag-subsystem)).
- **Runs where you are:** Linux x64 / arm64 / s390x, macOS (Apple Silicon)
  and Windows prebuilt (CLI, wheels, npm), Intel macs from source, containers
  for amd64 and arm64; NVIDIA
  CUDA and TensorRT, DirectML (a prebuilt Windows CLI), CoreML and WebGPU in the browser as opt-in
  accelerators ([GPU](./docs/MODELS.md#gpu-execution-providers-optional-off-by-default),
  [IBM Z](./docs/IBMZ.md)).

Developed with **Claude Code** and _[TENET](https://github.com/artiz/tenet/tree/master)_ (minimalistic AI-driven development framework).

## Status

Every format in docling's pipeline converts — plus a few docling lacks. A
file with no or an unknown extension, stdin, an upload or a `data:` URL is
typed by its content (`InputFormat::detect`, `docling_rs.detect_format`,
`detectFormat`: the packages, the OLE files, PDF, images, audio and video,
iWork, XML, JSON, notebooks, messages, …), and a file that fails under its
extension is checked by content and converted as what it really is, with a
warning.

| Category | Formats |
|---|---|
| Text & markup | Markdown, AsciiDoc, HTML, MHTML, LaTeX (multi-file arXiv projects, math, tables); plain text and source code (`.py`, `.yaml`, `.log`, … — any text file, as a code block or paragraphs), generic JSON / JSON Lines and XML (kept as code blocks); RSS / Atom / RDF feeds (`.rss`, `.atom`, `.rdf` or a bare `.xml`: the feed's title, each item's heading, date and body); Jupyter notebooks (`.ipynb`: markdown cells, code cells in the kernel's language, outputs on request) |
| Word processing | DOCX, Word 97–2004 `.doc` (and Word 6/95, 1.x/2.0), ODT, OpenOffice / StarWriter, AbiWord, WordPerfect, MS Works, Hancom HWP / HWPX, Lotus Word Pro, MacWrite / Word for Mac / ClarisWorks, T602, Palm e-texts, print files (`.prn`), EPUB, RTF |
| Presentations | PPTX, PowerPoint 97–2003, ODP, OpenOffice / StarImpress |
| Spreadsheets | XLSX, XLSB, Excel 97–2004, ODS, CSV / TSV, dBase, DIF, SYLK, Lotus 1-2-3, Quattro Pro, MS Works, Kingsoft `.et` |
| Diagrams · Apple iWork | Visio, SVG, CGM (its text) · Pages, Numbers, Keynote |
| PDF & images | PDF (layout, TableFormer, OCR), PNG, JPEG, TIFF, BMP, WebP, GIF, HEIC (opt-in), METS/GBS scans, DjVu |
| XML · docling native | JATS, USPTO, XBRL, RIS (Austrian legal XML), DocLang · docling JSON, DocTags, DCLX |
| Email · subtitles · mainframe | `.eml`, Outlook `.msg` · WebVTT · EBCDIC records through a COBOL copybook |
| Audio · video | WAV, MP3, M4A, AAC, OGG, FLAC; MP4, AVI, MOV, MKV, WebM, MPEG — local Whisper or Parakeet, plus scene frames from video; a YouTube watch URL — the video's title, description and caption transcript as timed items (`--asr-lang` picks the language) |

Every extension, with what each backend does:
[`docs/MIGRATION.md`](./docs/MIGRATION.md#every-extension-by-category).

<details>
<summary><b>Installing ffmpeg</b> (optional — only for video frame sampling)</summary>

Any ffmpeg ≥ 4.x on `PATH` works; docling.rs shells out to the binary and
parses its output, so no dev headers/libraries are needed.

- **Debian/Ubuntu**: `sudo apt-get install ffmpeg`
- **Fedora**: `sudo dnf install ffmpeg-free` (or `ffmpeg` from RPM Fusion)
- **Alpine**: `apk add ffmpeg`
- **macOS**: `brew install ffmpeg`
- **Windows**: `winget install ffmpeg` (or `choco install ffmpeg`, or
  `scoop install ffmpeg`). Installing from a downloaded zip
  ([gyan.dev](https://www.gyan.dev/ffmpeg/builds/) /
  [BtbN](https://github.com/BtbN/FFmpeg-Builds/releases)) also works — either
  add the extracted `bin\` folder to `PATH`, or skip `PATH` entirely and point
  `DOCLING_FFMPEG` at the exe:
  `set DOCLING_FFMPEG=C:\tools\ffmpeg\bin\ffmpeg.exe`

Check with `ffmpeg -version`. `DOCLING_FFMPEG` overrides the binary used on
any OS; the docling-rs-serve Docker image ships ffmpeg preinstalled.

A trimmed, decode-only build (`--disable-everything --enable-…`) works too
(#853); docling.rs needs only: the `file` protocol; the demuxers of your
containers (`mov` for mp4/mov/m4v, `matroska` for mkv/webm, `avi`, `mpeg`);
the video decoders of your tracks (`h264`, `hevc`, `vp8`, `vp9`, `av1`,
`mpeg4`, `mpeg2video`, …) with their parsers; the filters `select`,
`metadata`, `fps`, `scale`, `showinfo`; the encoders `png` and
`wrapped_avframe`; the muxers `image2` and `null`. No audio component is
used — the transcript is decoded in Rust — and the scene scan maps the
video stream alone. Missing a component, ffmpeg fails the scene scan
(`Encoder not found`, `Invalid argument`) and the video converts as
transcript only; `DOCLING_RS_DEBUG=1` shows the message.
</details>

## Conformance with Microsoft MarkItDown

[MarkItDown](https://github.com/microsoft/markitdown) converts the same kinds
of documents to Markdown for LLMs. Measured side by side — MarkItDown 0.1.9
(`markitdown[all]`) and docling.rs master after 2.51.3 (release build), 4 vCPU,
no GPU, 2026-10-25 — on this repository's test corpus and on MarkItDown's own
OCR test set
([`scripts/conformance/markitdown_compare.py`](./scripts/conformance/markitdown_compare.py)):

| Corpus (files) | MarkItDown, warm | docling.rs, warm | Cold CLI call, MarkItDown / docling.rs | Word F1 | Table-cell recall | Heading recall |
|---|---|---|---|---|---|---|
| DOCX (38) | 9.32 s | **0.57 s** | 1360 / **31** ms | 0.93 / **1.00** | 0.81 / **1.00** | 0.78 / **1.00** |
| PPTX (8) | 0.28 s | **0.04 s** | 1277 / **11** ms | 0.81 / **0.97** | 0.62 / **1.00** | 0.86 / **1.00** |
| XLSX (14) | 2.44 s | **0.30 s** | 1472 / **14** ms | 0.60 / **0.95** | 0.78 / **0.98** | — ¹ |
| HTML (32) | 0.53 s | **0.05 s** | 1248 / **11** ms | 0.98 / **1.00** | 0.88 / **1.00** | 0.96 / **1.00** |
| CSV (9) | 0.07 s | **0.01 s** | 1250 / **6** ms | 1.00 / 1.00 | 0.78 / **1.00** | — |
| EPUB (1) | 0.07 s | **0.02 s** | 1473 / **17** ms | 0.99 / **1.00** | — | 1.00 / 1.00 |
| PDF (18), full layout + tables + OCR | **19.0 s** | 60.3 s | **1442** / 1652 ms | 0.63 / **0.997** | 0.31 / **0.94** | 0.00 / **0.91** |
| PDF (18), text layer only (`--text-layer-only`) | 19.0 s | **0.8 s** | — | 0.63 / **0.97** | 0.31 / 0.00 | 0.00 / 0.00 |

Accuracy is scored against Python docling's groundtruth, which favours
docling.rs by construction; the timings do not depend on it. Text drawn into
images (MarkItDown's own OCR test set) reads at 1.00 with docling.rs' local
OCR, against 0.00–0.33 for MarkItDown without an LLM. Method, caveats and the full
OCR comparison: [`docs/MARKITDOWN.md`](./docs/MARKITDOWN.md#how-the-comparison-is-scored).

## Conformance with Marker

[Marker](https://github.com/datalab-to/marker) (datalab's `marker-pdf`)
converts PDFs and, through a PDF render, Office files to Markdown for LLMs
with a layout model and OCR. Measured side by side — Marker 2.0.0
(`marker-pdf[full]`, its CPU default `fast` mode, llama.cpp for its OCR) and
docling.rs master after 2.51.3 (release build), 4 vCPU, no GPU, 2026-10-25 ³ —
on this repository's test corpus
([`scripts/conformance/marker_compare.py`](./scripts/conformance/marker_compare.py)):

| Corpus (files) | Marker, warm | docling.rs, warm | Cold CLI call, Marker / docling.rs | Peak RSS, Marker (client + servers) / docling.rs | Word F1 | Table-cell recall | Heading recall |
|---|---|---|---|---|---|---|---|
| PDF (18), full layout + tables + OCR | 1356 s | **64 s** | 32.6 s / **1.5 s** | 10.3 GB / **2.8 GB** | 0.93 / **0.997** | 0.41 / **0.94** | 0.66 / **0.91** |
| PDF (18), text layer only (`--text-layer-only`) | 1356 s | **0.75 s** | — | — / 74 MB | 0.93 / **0.97** | 0.41 / 0.00 | 0.66 / 0.00 |
| DOCX (38) | 707 s (1 failed) | **0.54 s** | 36.3 s / **23 ms** | 7.7 GB / **41 MB** | 0.89 / **1.00** | 0.57 / **1.00** | 0.73 / **1.00** |
| PPTX (8) | 4.6 s (3 failed) | **0.03 s** | 33.7 s / **13 ms** | 2.7 GB / **20 MB** | 0.49 / **0.97** | 0.41 / **1.00** | 0.43 / **1.00** |
| XLSX (14) | 379 s (2 failed) | **0.33 s** | 34.4 s / **12 ms** | 17.2 GB / **53 MB** | 0.66 / **0.95** | 0.58 / **0.98** | — ¹ |
| HTML (32) ³ | 1514 s | **0.06 s** | 29 min ² / **8 ms** | 8.8 GB / **21 MB** | 0.94 / **1.00** | 0.50 / **1.00** | 0.84 / **1.00** |
| EPUB (1) | 13.5 s | **0.08 s** | 69.9 s / **21 ms** | 6.0 GB / **19 MB** | 0.98 / **1.00** | — | 1.00 / 1.00 |

A failed file scores 0. ¹ Only one XLSX groundtruth carries a heading.
² The median HTML file needs OCR in Marker: a cold call starts its llama.cpp
server and loads the 1.2 GB OCR model first.
³ The HTML row is the earlier run's (docling.rs 2.46.2, another host): its
re-run here was stopped after 3 h 45 min — its warm pass done, the first cold
`marker_single` call had spent 97 minutes on one 855-byte file, Marker's OCR model
looping on one render (5,000+ tokens for a single block).

Accuracy is scored against Python docling's groundtruth, which favours
docling.rs by construction; the timings and memory do not depend on it.
Marker reads Office files, HTML and EPUB by rendering them to PDF and
running its vision models on the render; on CPU every page its text check
flags goes through a 1.2 GB vision-language OCR model in a llama.cpp server
that `pip install marker-pdf` does not bring (without it 4 PDFs, 9 DOCX, 10
HTML and the EPUB fail outright). Peak RSS counts Marker's client and its
inference servers. Marker's `balanced` mode (its published benchmarks) needs
a GPU. Method, caveats and the API map:
[`docs/MARKER.md`](./docs/MARKER.md#how-the-comparison-is-scored).

## Conformance with MinerU

[MinerU](https://github.com/opendatalab/MinerU) (OpenDataLab) converts PDFs,
images and Office files to Markdown and JSON for LLMs in four tiers — `flash`
(text layer, no models), `basic` (ONNX layout / table / OCR models),
`standard` and `advanced` (a vision-language model through llama.cpp).
Measured side by side — MinerU 4.0.12 and docling.rs master after 2.51.3
(release build), 4 vCPU, no GPU, 2026-10-25 — on this repository's test corpus
([`scripts/conformance/mineru_compare.py`](./scripts/conformance/mineru_compare.py)):

| Corpus (files) | MinerU `flash`, warm | MinerU `basic`, warm | docling.rs, warm | Cold CLI call, MinerU `flash` / `basic` / docling.rs | Peak RSS, MinerU `flash` / `basic` / docling.rs | Word F1 (`flash` / `basic` / docling.rs) | Table-cell recall | Heading recall |
|---|---|---|---|---|---|---|---|---|
| PDF (18), full layout + tables + OCR | 38.8 s | 190 s (+ 78 s model load) | **62.7 s** | 5.7 s / 11.1 s / **1.7 s** | 1.4 GB / 4.4 GB / 2.9 GB | 0.87 / 0.74 / **0.997** | 0.28 / 0.44 / **0.94** | 0.57 / 0.58 / **0.91** |
| PDF (18), text layer only (`--text-layer-only`) | 38.8 s | — | **0.82 s** | — / — / — | — / — / 73 MB | 0.87 / — / **0.97** | 0.28 / — / 0.00 | 0.57 / — / 0.00 |
| DOCX (38) | 4.36 s | flash only | **0.70 s** | 3331 ms / — / **23 ms** | 164 MB / — / **41 MB** | 0.97 / — / **1.00** | 0.82 / — / **1.00** | 0.96 / — / **1.00** |
| PPTX (8) | 0.26 s (1 failed) | flash only | **0.02 s** | 3340 ms / — / **11 ms** | 110 MB / — / **20 MB** | 0.79 / — / **0.97** | 0.91 / — / **1.00** | 1.00 / — / 1.00 |
| XLSX (14) | 1.00 s | flash only | **0.20 s** | 1519 ms / — / **13 ms** | 130 MB / — / **55 MB** | 0.83 / — / **0.95** | 0.77 / — / **0.98** | — ¹ |
| HTML (32) | 1.07 s | flash only | **0.07 s** | 3039 ms / — / **9 ms** | 141 MB / — / **22 MB** | 0.94 / — / **1.00** | 0.80 / — / **1.00** | 0.96 / — / **1.00** |
| EPUB (1) | 0.12 s | flash only | **0.03 s** | 3144 ms / — / **24 ms** | 89 MB / — / **19 MB** | 1.00 / 1.00 | — | 1.00 / 1.00 |

A failed file scores 0. ¹ Only one XLSX groundtruth carries a heading.

Accuracy is scored against Python docling's groundtruth, which favours
docling.rs by construction; the timings and memory do not depend on it.
MinerU parses Office files, HTML and EPUB at `flash` only; `basic` is its
model tier on CPU; its `standard` tier (a 1.2 B vision-language model through llama.cpp) took 138 s for one page on a 4 vCPU CPU in the earlier run and is not in the table. Method, caveats and the API map:
[`docs/MINERU.md`](./docs/MINERU.md#how-the-comparison-is-scored).

## Conformance with Python docling

Every declarative format is compared against the live library — Markdown
byte for byte, JSON structurally (every key and value but `origin`/`version`
and re-encoded image bytes). Latest sweep, docling **2.142.1** with the
docling-core 2.103.1 it locks, over its mirrored test corpus
(`scripts/conformance/full_conformance.py`; ✅ all = every file matches):

| Format | Files | Markdown | JSON | Format | Files | Markdown | JSON |
|---|---|---|---|---|---|---|---|
| DOCX | 38 | ✅ all | ✅ all | iWork Pages | 1 | ✅ all | ✅ all |
| HTML | 32 | 30 ³ | ✅ all | iWork Keynote | 5 | ✅ all | ✅ all |
| PPTX | 8 | 7 ⁶ | 7 ⁶ | EBCDIC | 3 | ✅ all | ✅ all |
| XLSX | 14 | 13 ² | 13 ² | JATS | 7 | 4 ⁴ | ✅ all |
| ODF | 7 | ✅ all | ✅ all | DocLang | 15 | ✅ all | ✅ all |
| CSV | 9 | ✅ all | ✅ all | AsciiDoc | 5 | ✅ all | ✅ all |
| Markdown | 11 | ✅ all | 10 ⁵ | USPTO | 9 | ✅ all | ✅ all |
| DeepSeek-OCR Markdown | 3 | ✅ all | n/a | EPUB | 1 | ✅ all | ✅ all |
| WebVTT | 4 | ✅ all | ✅ all | LaTeX | 8 | ✅ all | 7 ¹ |
| Email (`.eml`) | 2 | ✅ all | ✅ all |  |  |  |  |

¹ Deliberate: in `1706.03762` a `\multirow` / `\multicolumn` cell's `row_span` / `col_span` agree with its offsets, where upstream leaves them at 1 (docling#4817's LaTeX rewrite is ported in full, PDF figures rendered at 144 dpi like upstream's).

² `xlsx_02_sample_sales_data`: 20 date cells print by docling PR #4628's number-format rules (#634), which the released docling does not apply yet.

³ Deliberate: a table inside a list item stays a table after the item (upstream prints its grid right under the item line); two Wikipedia links keep docling-core 2.104's `%28`/`%29` escaping.

⁴ Whitespace only: a one-run footnote prints on its bullet, where docling 2.142 leaves the bullet empty and puts the text on the next line.

⁵ Deliberate: a code span keeps its literal text (`&amp;`), which upstream decodes when a raw HTML block sends the file through its HTML backend.

⁶ Ahead of the release: `pptx_chart`'s 3-D column chart is read as a bar chart with its data, as docling `main` reads it since docling#4811 (after 2.142.1).

The residuals, with the exact files: [`docs/MIGRATION.md`](./docs/MIGRATION.md).
The PDF/image pipeline is pinned by a deterministic snapshot baseline and
scored against docling's groundtruth in
[`docs/PDF_CONFORMANCE.md`](./docs/PDF_CONFORMANCE.md).

## Performance

One fixture per format through published Python docling and the Rust release
binary (`scripts/test/performance.sh`); docling ÷ docling.rs, so bigger means
Rust wins by more. Measured 2026-10-25: docling 2.142.1 (the default PyPI
install, torch included) against docling.rs master after 2.51.3, 4 vCPU, no
GPU.

| File | Size | Peak-memory ratio | CPU ratio | Warm-conversion speedup |
|---|---:|---:|---:|---:|
| `picture_classification.pdf` (PDF) | 208 KB | **4.8× less** | 0.7× | 11.6× |
| `docx_rich_tables_01.docx` (DOCX) | 3.1 MB | **28× less** | 1.1× | 12× |
| `wiki_duck.html` (HTML) | 240 KB | **33× less** | 1.1× | 27× |
| `elife-56337.nxml` (JATS XML) | 180 KB | **39× less** | 1.2× | 11× |
| `xlsx_04_inflated.xlsx` (XLSX) | 168 KB | **32× less** | 0.9× | 11× |
| `powerpoint_with_image.pptx` (PPTX) | 80 KB | **36× less** | 1.1× | 2.7× |
| `wiki.md` (Markdown) | 8 KB | **43× less** | 1.1× | 2.3× |
| `csv-comma.csv` (CSV) | 4 KB | **44× less** | 1.3× | 0.4× † |

† A 4 KB CSV converts in microseconds, so Rust's own process startup dominates
its number; end to end it is 780× faster.

Declarative formats hold 17–28 MB against docling's 750–780 MB (it imports
torch even for them) and finish 200–890× sooner end to end; the PDF peaks at
0.46 GB against 2.2 GB and converts 19× faster end to end. Where the time
goes, and what was measured not to help:
[`docs/PDF_CONFORMANCE.md`](./docs/PDF_CONFORMANCE.md#where-the-time-goes).

## Integration — using docling.rs from your language

One engine, several front doors, the same options and outputs everywhere.

| You write… | Use | Install |
|---|---|---|
| Rust | the [`docling`](./crates/docling/README.md) crate | `cargo add docling` |
| a shell / CI job | the [`docling-rs` CLI](./crates/docling-cli/README.md), batch mode included | release binaries · `cargo install docling-cli` · `ghcr.io/docling-project/docling-rs` |
| anything that speaks HTTP | [`docling-serve`](./crates/docling-serve/README.md), OpenAPI 3.1 | `ghcr.io/docling-project/docling-rs-serve` · `cargo install docling-serve` |
| Node.js / Bun / Electron | [`docling.rs`](./crates/docling-node/README.md), an N-API addon | `npm i docling.rs` (`docling.rs-cuda`) |
| Python | [`docling-rs`](./crates/docling-py/README.md): docling's, MarkItDown's, Marker's and MinerU's APIs | `pip install docling-rs` (`docling-rs-cuda`) |
| LangChain / LlamaIndex | [`DoclingLoader`](./crates/docling-py/README.md#langchain) (langchain-docling's API) · [`DoclingReader`](./crates/docling-py/README.md#llamaindex) (llama-index-readers-docling's API) | `pip install langchain-docling-rs` · `pip install llama-index-readers-docling-rs` |
| the browser / Tauri / a PWA | [`docling.rs-wasm`](./crates/docling-wasm/README.md), fully client-side | `npm i docling.rs-wasm` |
| C, C++, C#, Go, Java, Swift, Zig | [`docling-ffi`](./crates/docling-ffi/README.md): one `docling.h` | release archives `docling-ffi-<tag>-<target>` |

**Rust** when you're already in Cargo; the **CLI** for scripts and batch jobs;
**serve** when several services share one warm pipeline; **Node / Python**
in-process; **wasm** when the document must not leave the user's machine;
**FFI** for anything that can call C.

## Rust API

```rust
use docling::{DocumentConverter, SourceDocument};

let converter = DocumentConverter::new();
let result = converter.convert(SourceDocument::from_file("input.md")?)?;

println!("{}", result.document.export_to_markdown()); // Markdown
println!("{}", result.document.export_to_json());     // docling DoclingDocument JSON
```

Every conversion knob is one serializable struct, `docling::ConvertOptions`,
filled the same way by every surface ([`docs/OPTIONS.md`](./docs/OPTIONS.md)).
The [library guide](./crates/docling/README.md) covers the output formats,
error handling, chunking, image extraction, streaming, OCR, the PDF switches,
the remote VLM pipeline and registering your own backends (#680).

## HTTP conversion API — `docling-rs serve`

[`docling-serve`](./crates/docling-serve/README.md) exposes the converter over
HTTP with one warm PDF/image pipeline shared across requests (repeat PDFs skip
the model load), streamed Markdown, batches, ZIP inputs, async jobs, an
interactive test form at `GET /` and an OpenAPI 3.1 description at
`GET /openapi.yaml`. It also serves Python docling-serve's routes, so Open
WebUI, n8n, Dify and LangChain clients work unchanged.

```bash
docker run -p 127.0.0.1:5001:5001 ghcr.io/docling-project/docling-rs-serve
curl -F file=@paper.pdf localhost:5001/v1/convert                # Markdown
curl -F file=@report.docx 'localhost:5001/v1/convert?to=json'    # docling JSON
curl -F file=@a.pdf -F file=@b.docx localhost:5001/v1/convert    # a batch
```

Request options, server flags, async jobs, the docling-serve routes and the
Open WebUI setup: [`crates/docling-serve/README.md`](./crates/docling-serve/README.md);
containers and sizing: [`docs/DEPLOYMENT.md`](./docs/DEPLOYMENT.md).

## MCP server — `docling-rs mcp`

`docling-rs mcp` is a [Model Context Protocol](https://modelcontextprotocol.io)
server in the default `docling-rs` binary and image. Agents get MarkItDown's
`convert_to_markdown(uri)` tool — same name and contract, so a
`markitdown-mcp` setup switches by changing the command — plus
`convert(uri, to, options)` and `list_input_formats()`.

```json
{
  "mcpServers": {
    "docling": { "command": "docling-rs", "args": ["mcp", "--root", "/Users/me/Documents"] }
  }
}
```

```bash
claude mcp add docling -- docling-rs mcp --root ~/Documents     # Claude Code
uvx docling-rs-mcp --root ~/Documents                           # the same server from PyPI
```

Local reads stay under `--root`; URLs need `--allow-url-fetch`. Listed in the
[official MCP registry](https://registry.modelcontextprotocol.io/v0/servers?search=io.github.docling-project/docling-rs)
and on [Smithery](https://smithery.ai/servers/artem-kustikov/docling-rs). The PyPI
launcher, Docker, Streamable HTTP (`--http`), the security model and memory
behaviour:
[`crates/docling-cli/README.md`](./crates/docling-cli/README.md#mcp-server--docling-rs-mcp).

## RAG subsystem

[`crates/docling-rag`](./crates/docling-rag/README.md) turns documents into
chunks, embeds them (Ollama, Gemini or a local ONNX model), stores them
(SQLite + sqlite-vec or PostgreSQL + pgvector) and searches them — hybrid,
multi-query and HyDE retrieval, an evaluation harness, a REST API and a
built-in search UI.

## In the browser — `docling-wasm`

The declarative converters and the PDF text layer compile to WebAssembly:
`npm i docling.rs-wasm`, then `convert(bytes, filename, to)` runs **fully
client-side** — no server, a ~3.4 MB gzipped module, optional in-browser OCR.
Details and the demo page: [`crates/docling-wasm`](./crates/docling-wasm/README.md).

## Node.js, Python and C

- **Node.js / Bun** — [`docling.rs`](./crates/docling-node/README.md) on npm, a
  prebuilt N-API addon: sync and async conversion, streaming, chunking, ZIP
  batches, a warm `Pipeline`.
- **Python** — [`docling-rs`](./crates/docling-py/README.md) on PyPI
  (`docling-rs-cuda` for NVIDIA): docling's `DocumentConverter` with a genuine
  docling-core `DoclingDocument`, MarkItDown's API, and the LangChain,
  AutoGen and Microsoft Agent Framework integrations.
- **C ABI** — [`docling-ffi`](./crates/docling-ffi/README.md): one library and
  one `docling.h` for C, C++, C#/.NET, Go, Java, Swift and Zig, prebuilt in
  every published release.

## Deploy in a container

| Image | What it is |
|---|---|
| [`ghcr.io/docling-project/docling-rs-serve`](https://github.com/docling-project/docling.rs/pkgs/container/docling-rs-serve) | the HTTP API, models baked in (`linux/amd64`, `linux/arm64`) |
| [`ghcr.io/docling-project/docling-rs`](https://github.com/docling-project/docling.rs/pkgs/container/docling-rs) | the CLI, entrypoint `docling-rs`, working directory `/data` |
| `…-serve-cuda` · `…-rs-cuda` | the same on CUDA 13 + cuDNN 9 (`linux/amd64`) |

Compose files (standalone, Caddy TLS, CUDA), environment variables, sizing
and observability: [`docs/DEPLOYMENT.md`](./docs/DEPLOYMENT.md).

## Getting the ML models

The PDF/image pipeline needs ONNX models — RT-DETR layout, PP-OCR, and
TableFormer — and audio needs Whisper. `download_dependencies.sh` fetches
them from this repository's
[releases](https://github.com/docling-project/docling.rs/releases) into
`./.models`, where every surface looks (`--cli`: beside a cargo-installed
binary instead):

```bash
scripts/install/download_dependencies.sh                       # from a checkout
curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh         # anywhere
```

Every asset and option (ASR presets, enrichment models, NER for PII
redaction, INT8, fonts), GPU execution providers and the environment
overrides: [`docs/MODELS.md`](./docs/MODELS.md). IBM Z and other targets
without a prebuilt ONNX Runtime: [`docs/IBMZ.md`](./docs/IBMZ.md).

## Contributing

Bug reports and pull requests are welcome — see
[CONTRIBUTING.md](./CONTRIBUTING.md) for the build and test commands, the
conformance workflow, and the conventions a change is expected to follow.

## License

MIT, matching upstream docling.

The CID → Unicode tables in
[`crates/docling-pdf/data/mapping-resources-pdf/`](./crates/docling-pdf/data/mapping-resources-pdf/)
are Adobe's, under the BSD-3-Clause
[`LICENSE.txt`](./crates/docling-pdf/data/mapping-resources-pdf/LICENSE.txt)
there. The native builds compile them in (docling-pdf's `cid-collections`
feature), so the published binaries carry that licence as
`LICENSE-mapping-resources-pdf.txt`: the CLI and FFI archives and the Node.js
packages next to the binary, the wheels in their `.dist-info/licenses/`
(maturin includes `crates/docling-py/LICENSE-*`), the container images in
`/usr/share/doc/docling.rs/`, and `install.sh` in its prefix. The wasm package
and demo leave the tables out. A binary you build and redistribute yourself
with the feature on needs the licence beside it too.
