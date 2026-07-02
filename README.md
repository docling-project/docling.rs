# Fleischwolf (meat grinder in German, [ˈflaɪ̯ˌʃvɔlf])

```
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢻⣿⣿⣿⣿⣿⣿⣿⣿⠇⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⢿⣿⣿⣿⣿⣿⣿⠏⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠻⣿⣿⣿⡿⠋⣀⣀⣀⣀⣀⣀⢰⣶⡆⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢸⣿⣦⣄⣉⣉⣤⣾⣿⣿⣿⣿⣿⣿⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⢠⣤⣤⠀⡇⢸⣿⣿⣿⣿⣿⣿⣿⣟⣛⣛⣛⣛⡋⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⠈⢉⡉⠀⠇⢸⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠈⢉⣉⡉⠉⠉⠉⠛⠛⠛⠛⠛⠛⠛⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠀⢸⣿⡇⠀⠀⠸⠿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠀⢸⣿⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠀⢸⣿⡇⠀⠀⢠⣤⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠸⠇⠀⠀⠀⢸⣿⡇⠀⠀⢠⣤⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⣴⣶⣾⣿⣿⣷⣶⣦⠄⠀⠀⠀⠸⣿⣧⣤⣤⣾⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠉⠉⠉⠉⠉⠁⠀⠀⠀⠀⠀⠀⠈⠉⠉⠉⢉⣉⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀
```

A Rust port of [docling](https://github.com/docling-project/docling): convert
documents into a unified `DoclingDocument` for downstream AI workflows.

This is an **early, in-progress** port. See [`MIGRATION.md`](./MIGRATION.md) for
the full architecture, the Python → Rust mapping, and the phased plan.

## Status

The public API works end to end across **Markdown, CSV, HTML, AsciiDoc, DOCX,
PPTX, XLSX, EPUB, ODF, WebVTT, Email, MHTML, JATS, USPTO, XBRL, LaTeX, JSON,
PDF, images, METS and audio** — plus Markdown / docling-JSON output and image
extraction. MHTML is a fleischwolf-only extension (docling has no MHTML
backend): saved-webpage `.mhtml`/`.mht` archives are parsed as a MIME message
with [`mail-parser`](https://crates.io/crates/mail-parser) (which conforms to
[RFC 2557](https://datatracker.ietf.org/doc/html/rfc2557), the MHTML spec) and
routed through the HTML backend, with embedded images resolved from the
archive by `Content-Location`/`cid:`. The discriminative PDF/image pipeline
lives in `fleischwolf-pdf`: a pure-Rust PDF text parser, pdfium for page
rasterization, and an ONNX layout/TableFormer/OCR stack. TableFormer is ported
to ONNX and run on every detected table region to recover its structure;
geometric reconstruction from cell positions remains only as the fallback when
the TableFormer graphs aren't present (see `PDF_CONFORMANCE.md`).

**Audio/ASR** (docling's Whisper pipeline) lives in `fleischwolf-asr`, and it is
Rust all the way down: [`symphonia`](https://crates.io/crates/symphonia)
demuxes/decodes the container in-process (wav, mp3, flac, ogg, aac, m4a — plus
the audio track of mp4/mov; no ffmpeg), a ported log-mel front-end feeds a
**Whisper tiny** encoder/decoder exported to ONNX (run on `ort`, greedy with
OpenAI's timestamp rules — docling's ASR defaults), and each segment becomes a
`[time: start-end] text` paragraph. `FLEISCHWOLF_ASR_LANG` picks the language
(default `en`). AVI is the one container symphonia cannot demux.

Output is checked against upstream Python docling — declarative formats
byte-for-byte against live docling, the ML pipeline against a deterministic
snapshot baseline. See [`COMPARING.md`](./COMPARING.md) and
`scripts/conformance.sh`.

## RAG subsystem

[`crates/fleischwolf-rag`](./crates/fleischwolf-rag) builds a pluggable
Retrieval-Augmented-Generation layer on top of the converter: it turns documents
into Markdown, chunks them (configurable size / overlap), embeds the chunks, and
stores them in a vector database for semantic search. Every external dependency is
a swappable trait — embedders (**Ollama**/Gemini/local-ONNX), vector stores
(**SQLite+sqlite-vec**/PostgreSQL+pgvector), LLM (**OpenRouter**, DeepSeek-V3 by default),
document sources (**folder**/FTP/SFTP), and message queues
(**in-process**/RabbitMQ/Redis). It ships Hybrid, Multi-Query fusion and HyDE
retrieval plus an evaluation harness to compare configurations and an
API-key-protected REST API (`fleischwolf-rag serve`) for document info and
search. Configure it via [`.env`](./.env.example); see the
[crate README](./crates/fleischwolf-rag/README.md) for a quickstart on any
documents folder.

## The API

```rust
use fleischwolf::{DocumentConverter, SourceDocument};

let converter = DocumentConverter::new();
let result = converter
    .convert(SourceDocument::from_file("input.md").unwrap())
    .unwrap();

println!("{}", result.document.export_to_markdown()); // Markdown
println!("{}", result.document.export_to_json());     // docling DoclingDocument JSON
```

### JSON output

`export_to_json()` emits docling-core's native `DoclingDocument` wire format
(schema `1.10.0`) — the same shape Python docling's `export_to_dict()` /
`save_as_json()` produce: a `body` tree of `$ref`s into `texts` / `groups` /
`tables` / `pictures`, with labels (`title`, `section_header`, `list_item`,
`code`, `formula`, …), list grouping, and table grids. The output loads straight
back into Python docling-core (`DoclingDocument.load_from_json(...)`) and
round-trips to the same Markdown.

> Note: Fleischwolf's model bakes inline formatting (bold, links, inline math)
> into the text, so for those spans the JSON carries the rendered text rather
> than docling's structured `formatting` / `hyperlink` fields. Block structure,
> headings, lists, tables, code and display equations match.

### Image extraction

Backends that have the image populate `Node::Picture { image }`: the PDF/image
pipeline crops figure regions, the DOCX / PPTX / MHTML backends pull embedded
image blobs (MHTML resolves `<img src>` against the archive's own MIME parts —
no network/filesystem access needed, so it's on by default), and — opt-in —
the HTML / EPUB backends fetch `<img src>` (see below).
Pick how pictures render with an [`ImageMode`] — the analogue of docling's
`image_mode`:

```rust
use fleischwolf::ImageMode;

// self-contained Markdown: ![Image](data:image/png;base64,…)
let (md, _) = result.document.export_to_markdown_with_images(ImageMode::Embedded, "artifacts");

// referenced: ![Image](artifacts/image_000000.png) + the bytes to write
let (md, files) = result.document.export_to_markdown_with_images(ImageMode::Referenced, "artifacts");
for (path, bytes) in files { std::fs::write(path, bytes).unwrap(); }
```

`export_to_json()` always embeds extracted images as docling `ImageRef`s
(`data:` URIs + size). The default `export_to_markdown()` stays
`<!-- image -->`, like docling.

> The cropped/extracted pixels are real, but the base64 won't be byte-identical
> to docling's (different PNG encoder). HTML/EPUB pictures stay placeholders by
> default (like docling); enable fetching with `--fetch-images` /
> `DocumentConverter::fetch_images(true)` to resolve `<img src>` — `data:` URIs,
> local files, remote `http(s)` URLs, and EPUB archive entries — and embed the
> bytes. Remote URLs are fetched over the network, so enable it only for input
> you trust.

### `strict` Markdown (Rust-only)

By default `export_to_markdown()` reproduces docling's output byte-for-byte,
quirks included (`***x*** .`, dropped code-fence languages, `\_` escaping). Set
`strict(true)` for cleaner, more conformant Markdown:

```rust
let converter = DocumentConverter::new().strict(true);
let result = converter.convert(source).unwrap();
println!("{}", result.document.export_to_markdown()); // ```rust kept, no `***x*** .`, `_` not escaped
```

```text
legacy:  Foo ***both*** .   |   ``` (lang dropped)   |   Name: \_\_\_
strict:  Foo ***both***.    |   ```rust (lang kept)  |   Name: ___
```

`result.document.export_to_markdown_with(strict)` overrides the mode per call.
Python docling has no such switch.

### Streaming Markdown

For embedding in real apps, `convert_streaming` returns the document's Markdown
as an iterator of chunks instead of one big string — handy for piping a long
document straight to stdout, an HTTP response, or a socket as it is produced:

```rust
use std::io::Write;
use fleischwolf::{DocumentConverter, SourceDocument};

let source = SourceDocument::from_file("input.pdf").unwrap();
let mut out = std::io::stdout();
for chunk in DocumentConverter::new().convert_streaming(source).unwrap() {
    out.write_all(chunk.unwrap().as_bytes()).unwrap();
}
```

The headline win is PDF. The ML pipeline already processes pages **in parallel**;
streaming emits each page's Markdown **in document order, as soon as it is ready**
(with a one-page look-ahead so paragraphs that wrap across a page break still
merge), so output starts flowing before the last page is done. The conversion
runs on a background thread and the chunk iterator applies backpressure; dropping
it cancels the work. Concatenating every chunk is **byte-identical** to the
buffered `export_to_markdown()`.

Streaming is Markdown-only — JSON serializes docling-core's reference-based tree
and needs every node up front. Picture placeholders and `embedded` data-URI
images stream; the `referenced` mode writes sidecar files, so it stays on the
buffered `export_to_markdown_with_images` path. Use
`convert_streaming_images(source, ImageMode::Embedded)` to pick the image mode.

The CLI streams Markdown by default (`--no-stream` opts back into buffering;
`--to json` and `--images referenced` always buffer). `--no-table-former` skips
loading/running the TableFormer table-structure model, falling back to simple
geometric table reconstruction from cell positions — no model load, no
per-table inference, which can noticeably speed up parsing (especially in
streaming mode) at the cost of table fidelity. `--no-ocr` goes further and
skips layout detection, OCR, and TableFormer entirely — no ML inference at
all, only the PDF's embedded text cells grouped into flat paragraphs by
reading order (no headings/lists/tables/pictures). It's the fastest PDF path
by a wide margin, but a scanned/image-only PDF (no embedded text layer) comes
back empty rather than erroring, so a caller can detect that and re-convert
without the flag.

### Headless-browser HTML pre-render (optional)

Almost everything in the HTML backend is pure Rust, but one thing a static
parse can't do is resolve the **CSS cascade** — whether a stylesheet- or
class-driven rule makes an element `display:none` (e.g. a collapsed nav menu).
The optional `--use-web-browser` flag renders the page in the system Chromium
first, drops every element the browser computes as hidden, then feeds the
cleaned HTML through the normal Rust backend (so all structure/table/KVP/
formatting logic still runs in Rust — the browser only decides visibility). It
applies to every HTML-routing input: direct HTML, plus MHTML and EPUB (which
assemble HTML from their archives). It's driven straight from Rust over the
DevTools protocol via
[`headless_chrome`](https://crates.io/crates/headless_chrome) — no Node,
Playwright, or other runtime.

It's gated behind the off-by-default `web-browser` Cargo feature, so the standard
build stays browser-dependency-free:

```bash
cargo run -p fleischwolf-cli --features web-browser -- --use-web-browser page.html
```

Chromium is located via `$FLEISCHWOLF_CHROME`/`$CHROME`, then
`$PLAYWRIGHT_BROWSERS_PATH/chromium`, else autodetected. The page's CSS must be
reachable for the cascade to resolve — inline `<style>` works offline, but a
saved page that links external stylesheets needs those fetchable (with a base
host). Without the feature, `--use-web-browser` is a clear error rather than a
silent no-op.

## Node.js / Bun bindings

Fleischwolf ships as an npm package, [**`fleischwolf`**](https://www.npmjs.com/package/fleischwolf)
— native TypeScript bindings (built with [napi-rs](https://napi.rs)) that live in
[`crates/fleischwolf-node`](./crates/fleischwolf-node). It's a real `.node` addon
that loads in both Node.js and Bun (Bun implements N-API — same binary, no
rebuild), exposing the converter with the same knobs as the Rust API: Markdown /
docling JSON output, `strict` mode, image modes, allowed-format restriction,
`fetchImages`, sync + async (`Promise`) calls, and a `streamFileMarkdown` async
generator.

Install — no Rust toolchain needed, the prebuilt binary for your platform (Linux
x64/arm64, Windows x64) is pulled in automatically:

```bash
npm install fleischwolf   # or: bun add fleischwolf
```

```ts
import { convert, convertFile, convertFileAsync } from 'fleischwolf'

// in-memory bytes → Markdown
const md = convert({ name: 'notes.md', data: Buffer.from('# Hello\n\nWorld **bold**') })
console.log(md.content)

// a file → Markdown or docling JSON (format detected from the extension)
const { content } = convertFile('report.docx')
const json = await convertFileAsync('report.docx', { to: 'json' })
```

Declarative formats (Markdown, HTML, DOCX, XLSX, …) work out of the box. The
PDF/image pipeline needs pdfium + the ONNX models (not bundled), so it throws
until you fetch them with `scripts/download_dependencies.sh` — see
[Getting the ML models](#getting-the-ml-models) below.

A reusable `Pipeline` keeps those models warm across many PDFs.

Runnable Node + Bun examples are in
[`crates/fleischwolf-node/examples`](./crates/fleischwolf-node/examples)
(`npm install && node node-basic.mjs`). See
[`crates/fleischwolf-node/README.md`](./crates/fleischwolf-node/README.md) for
the full API.

## Getting the ML models

The PDF/image pipeline needs native assets that aren't bundled in the crate or
the npm addon: [pdfium](https://pdfium.googlesource.com/pdfium/) (text
extraction + page rendering) and three ONNX models — RT-DETR layout, PP-OCRv3
recognition, and TableFormer (optional; tables fall back to geometric
reconstruction without it). `scripts/download_dependencies.sh` fetches all of
them from this repo's [GitHub Releases](https://github.com/artiz/fleischwolf/releases)
(tag `models-v1`) straight into `./models` and `./.pdfium`, relative to the
current directory — both the Rust CLI/library and the Node.js/Bun bindings
look there by default, so no env vars or extra setup are needed afterwards:

```bash
# from a checkout of this repo, or any directory you'll run fleischwolf from:
scripts/download_dependencies.sh

# or, without a checkout — e.g. a container build step, or a fresh npm project:
curl -fsSL https://raw.githubusercontent.com/artiz/fleischwolf/master/scripts/download_dependencies.sh | sh
```

| Asset | Destination |
| --- | --- |
| pdfium (Linux x64) | `.pdfium/lib/libpdfium.so` |
| RT-DETR layout | `models/layout_heron.onnx` |
| PP-OCRv3 rec + dictionary | `models/ocr_rec.onnx`, `models/ppocr_keys_v1.txt` |
| TableFormer (optional) | `models/tableformer/{encoder,decoder,bbox}.onnx` (+ `.data` sidecars where the export needs them) |
| Whisper tiny (audio/ASR; skip with `--no-asr`) | `models/asr/{encoder_model,decoder_model}.onnx`, `models/asr/vocab.json` (+ `added_tokens.json` for language selection) |
| INT8 CPU models (optional; fetch with `--int8`) | `models/layout_heron_int8.onnx`, `models/tableformer/decoder_int8.onnx` |

Idempotent — safe to re-run; it skips files already on disk. Pass `--force` to
re-fetch everything, or set `$FLEISCHWOLF_MODELS_URL` to fetch from a
different host (your own export, an internal mirror, …); the Whisper assets
come from Hugging Face (`$FLEISCHWOLF_ASR_MODELS_URL` overrides, or point
`DOCLING_ASR_{ENCODER,DECODER,VOCAB}` at explicit files). pdfium is Linux x64
only for now — other platforms, or building the models from source, need
[`scripts/pdf_setup.sh`](#testing) instead.

### INT8 models (faster PDF conversion on CPU — the default)

The `*_int8` assets are post-training quantizations of the same models:
Conv-only static INT8 of the layout detector (calibrated on this repo's PDF
corpus) and dynamic INT8 of the TableFormer decoder. On CPUs with AVX-512
VNNI they make layout inference — the dominant PDF cost — **~2.4× faster**
(~1.4–1.8× end-to-end) at conformance validated as unchanged against the
corpus groundtruth; the TableFormer output is byte-identical. See
[`PDF_PERFORMANCE.md`](./PDF_PERFORMANCE.md) for the measurements.

**The pipeline uses them automatically** whenever they sit next to the fp32
files at the default paths (`download_dependencies.sh` fetches them by
default; `--no-int8` skips, or build them with `python
scripts/quantize_models.py`). To force full precision:

```bash
FLEISCHWOLF_FP32=1 fleischwolf input.pdf          # keep the int8 files, use fp32
# or pin a model explicitly — an explicit path always wins:
export DOCLING_LAYOUT_ONNX=$PWD/models/layout_heron.onnx
export DOCLING_TABLEFORMER_DECODER=$PWD/models/tableformer/decoder.onnx
```

(The [example Dockerfile](./examples/Dockerfile) bakes both precisions and
defaults to INT8; build with `--build-arg INT8=0` for pure fp32.)

Then either:

```bash
cargo run -p fleischwolf-cli -- document.pdf
```

or, in a Node.js/Bun app:

```bash
npm i fleischwolf
```

```js
import { convertFileAsync } from 'fleischwolf'
const { content } = await convertFileAsync('document.pdf', { to: 'markdown' })
console.log(content)
```

The layout model and TableFormer are PyTorch→ONNX exports of docling-project's
own models (Apache-2.0 / CDLA-Permissive-2.0 — see
[`MODELS_NOTICE.md`](./MODELS_NOTICE.md) for full attribution); pdfium and the
OCR model are re-hosted, unmodified, from their own public releases — all on
one host for convenience.

To point at files you exported or placed elsewhere instead, set the env vars
directly: `DOCLING_LAYOUT_ONNX`, `DOCLING_OCR_REC_ONNX`, `DOCLING_OCR_DICT`,
`DOCLING_TABLEFORMER_{ENCODER,DECODER,BBOX}`, `PDFIUM_DYNAMIC_LIB_PATH` — an
env var always wins over the `./models` / `./.pdfium` default.

## Testing

All commands run from the `fleischwolf/` workspace root.

```bash
# everything — unit tests + the output-regression suite (pure Rust; no Python/models)
cargo test

# just the regression suite: re-convert every source under
# crates/fleischwolf/tests/data/<fmt>/sources/ and assert that legacy Markdown,
# strict Markdown and docling JSON match the committed fixtures (catches drift)
cargo test -p fleischwolf --test regression

# refresh the fixtures after an *intentional* output change, then review `git diff`
FLEISCHWOLF_REGEN=1 cargo test -p fleischwolf --test regression

# a single crate / a single test (with output)
cargo test -p fleischwolf-core
cargo test outputs_match_fixtures -- --nocapture
```

The ML formats (PDF, images, METS) need pdfium + the ONNX models, so they are
covered by a separate **deterministic snapshot** harness rather than `cargo test`:

```bash
bash scripts/pdf_setup.sh           # one-time: fetch pdfium + export the ONNX models
                                    # (layout + TableFormer; needs a torch/docling Python)
# Updating an existing checkout after a model-format change (e.g. the cached
# TableFormer decoder): `rm -rf models/tableformer && bash scripts/pdf_setup.sh`,
# or re-run `python scripts/export_tableformer.py models/tableformer` directly.

export PDFIUM_DYNAMIC_LIB_PATH="$(pwd)/.pdfium/lib"
export DOCLING_LAYOUT_ONNX="$(pwd)/models/layout_heron.onnx"
export DOCLING_OCR_REC_ONNX="$(pwd)/models/ocr_rec.onnx"
export DOCLING_OCR_DICT="$(pwd)/models/ppocr_keys_v1.txt"
# Optional (falls back to geometric table reconstruction if unset/missing —
# but the fallback is *silent*, so set these to be sure TableFormer is used,
# especially if you invoke fleischwolf from anywhere but the repo root: the
# defaults baked into the binary are relative paths, so a different working
# directory makes them silently miss even when the files exist elsewhere).
export DOCLING_TABLEFORMER_ENCODER="$(pwd)/models/tableformer/encoder.onnx"
export DOCLING_TABLEFORMER_DECODER="$(pwd)/models/tableformer/decoder.onnx"
export DOCLING_TABLEFORMER_BBOX="$(pwd)/models/tableformer/bbox.onnx"
bash scripts/pdf_conformance.sh     # regenerate + diff the snapshot baseline (91/91)
```

## Try it

```bash
# convert a file from the CLI — Markdown to stdout (add --strict for cleaner MD)
cargo run -p fleischwolf-cli -- crates/fleischwolf/sample.html
cargo run -p fleischwolf-cli -- --strict crates/fleischwolf/sample.html

# emit docling's native DoclingDocument JSON instead (--to md is the default)
cargo run -p fleischwolf-cli -- --to json crates/fleischwolf/sample.html
cargo run -p fleischwolf-cli -- --to json crates/fleischwolf/sample.html > out.json

# PDF/image conversion needs the ML models — see "Getting the ML models" above.
scripts/download_dependencies.sh
cargo run -p fleischwolf-cli -- document.pdf

# transcribe audio (wav/mp3/flac/ogg/aac/m4a, or an mp4/mov audio track) — the
# Whisper models come from the same download script
cargo run -p fleischwolf-cli -- recording.mp3

# extract pictures (PDF/image inputs): embed as data URIs, or write ./artifacts/*.png
cargo run -p fleischwolf-cli -- --images embedded   document.pdf
cargo run -p fleischwolf-cli -- --images referenced document.pdf > out.md

# stream Markdown to stdout page by page (the CLI's default; --no-stream to buffer)
cargo run -p fleischwolf-cli -- document.pdf
cargo run -p fleischwolf-cli -- --no-stream document.pdf

# or via the examples
cargo run -p fleischwolf --example convert -- crates/fleischwolf/sample.md
cargo run -p fleischwolf --example stream  -- crates/fleischwolf/sample.md

# score HTML output against the latest published docling (installed from PyPI)
scripts/conformance.sh html

# diff Python docling vs Rust on one file (installs published docling from PyPI)
scripts/compare.sh tests/data/html/sources/example_03.html

# benchmark time / CPU / memory: Python docling vs Rust
scripts/performance.sh tests/data/html/sources/wiki_duck.html 10
```

The comparison scripts install the latest published Python `docling` from PyPI
into `.venv-compare` automatically on first run. See
[`COMPARING.md`](./COMPARING.md).

## Deploy in a container

For a real-world service, bake the binary, native libs, and models into one image
so the runtime needs no Python. [`examples/Dockerfile`](./examples/Dockerfile) is a
3-stage build that does exactly this — a `models` stage exports the layout +
**TableFormer** (KV-cached decoder) ONNX with torch and fetches the OCR model +
pdfium, a `builder` stage compiles the CLI, and a slim `runtime` stage carries just
the binary, `libonnxruntime`, pdfium, and the models, with the `DOCLING_*` env vars
preset:

```bash
docker build -f examples/Dockerfile -t fleischwolf .
docker run --rm -v "$PWD:/data" fleischwolf /data/input.pdf          # Markdown to stdout
docker run --rm -v "$PWD:/data" fleischwolf /data/input.pdf --to json
```

The image converts PDFs/images fully offline; the model export (torch +
`docling-ibm-models`) happens only at build time, never at runtime.

## Performance

`scripts/performance.sh` runs a representative fixture of each supported type
through both engines (published Python `docling` vs the Rust release binary) and
reports peak RSS, CPU utilization, and conversion time. Ratios below are
docling ÷ fleischwolf — bigger means Rust wins by more. The PDF row is the
**fp32** stack; the optional [INT8 models](#int8-models-faster-pdf-conversion-on-cpu)
roughly double layout-inference speed on top of it (measured 1.83× end-to-end
on a 1913-page document — see [`PDF_PERFORMANCE.md`](./PDF_PERFORMANCE.md)).

| File | Size | Peak-memory ratio | CPU ratio | Warm-conversion speedup |
|---|---:|---:|---:|---:|
| `picture_classification.pdf` (PDF) | 208 KB | **2.3× less** | 1.0× | 2.3× |
| `docx_rich_tables_01.docx` (DOCX) | 3.1 MB | **41× less** | 2.7× | 21× |
| `wiki_duck.html` (HTML) | 240 KB | **57× less** | 3.2× | 46× |
| `elife-56337.nxml` (JATS XML) | 180 KB | **61× less** | 2.9× | 10× |
| `xlsx_04_inflated.xlsx` (XLSX) | 168 KB | **59× less** | 2.9× | 12× |
| `powerpoint_with_image.pptx` (PPTX) | 80 KB | **57× less** | 2.8× | 4.4× |
| `wiki.md` (Markdown) | 8 KB | **58× less** | 2.9× | 1.3× |
| `csv-comma.csv` (CSV) | 4 KB | **66× less** | 2.9× | 0.6× † |

- **Peak memory** is where Rust wins decisively: a declarative conversion holds a
  few MB versus docling's ~750 MB (it imports torch even for non-ML formats). The
  PDF runs the full ML pipeline in both engines (torch vs ONNX), so the gap there
  is 2.3× rather than 50×+, but Rust still peaks at 0.77 GB vs docling's 1.75 GB —
  and the PDF converts **4.1× faster end-to-end** (docling re-pays its torch
  import + model load on every invocation).
- **CPU**: docling spreads across 2.7–3.2 cores for declarative work that Rust does
  on a single core (~100%); on the PDF both go multi-core (~330% each here).
- **Warm-conversion speedup** isolates the parse/convert work — it times docling
  *in-process* (excluding its ~3 s interpreter + import startup) against the Rust
  whole-process figure. Rust wins on substantial inputs (HTML 46×, DOCX 21×); the
  end-to-end figure, which re-pays docling's startup every invocation, is **377–
  1190× faster** for the declarative formats.
- † For trivial inputs (a 4 KB CSV) the conversion itself is microseconds, so Rust's
  own process startup dominates its number while warm-Python excludes startup — the
  warm metric understates Rust there. End-to-end, the CSV is **1190× faster** in Rust.

## Layout

| Crate | Role | Python analogue |
|---|---|---|
| `fleischwolf-core` | `DoclingDocument` model + serializers | `docling-core` |
| `fleischwolf` | `DocumentConverter`, source loading, backends | `docling` |
| `fleischwolf-pdf` | PDF/image ML pipeline (pdfium + ONNX layout/table/OCR) | `docling` PDF pipeline |
| `fleischwolf-cli` | command-line interface | `docling.cli` |
| `fleischwolf-node` | Node.js / Bun N-API bindings (npm package) | — |

## License

MIT, matching upstream docling.
