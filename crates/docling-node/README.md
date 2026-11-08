# docling.rs (Node.js / Bun bindings)

Native [Node.js](https://nodejs.org) / [Bun](https://bun.sh) bindings for
[docling.rs](https://github.com/docling-project/docling.rs) — a Rust port of
[docling](https://github.com/docling-project/docling). Convert Markdown, HTML,
DOCX, PPTX, XLSX, EPUB, ODF, LaTeX, email, PDF, images and more into a unified
`DoclingDocument`, and export it as **Markdown** or docling-core **JSON**.

Built with [napi-rs](https://napi.rs), so it ships a real native addon (`.node`)
that loads in both Node.js and Bun (Bun implements N-API) — the same binary, no
rebuild between runtimes.

## Install

Released versions ship **prebuilt** native binaries, so no Rust toolchain is
needed to use the package:

```bash
npm install docling.rs   # or: bun add docling.rs
```

Prebuilt platforms: Linux x64 / arm64 / s390x (glibc), macOS arm64 (Apple
Silicon, macOS 13+; #816) and Windows x64 — on IBM Z the addon loads ONNX
Runtime at run time from the library `download_dependencies.sh` fetches
(#504). Minimum glibc: 2.28 on x64 and s390x (RHEL 8 / Debian 10 era and
newer), 2.39 on arm64 (built on Ubuntu 24.04). Before #745 the x64 addon
loaded only on glibc 2.38+ with a GCC 13+ libstdc++. (Intel macs aren't
prebuilt — build from source, see below.) The right binary is pulled in
automatically as a platform-specific `optionalDependency` (`docling.rs-<triple>`). Releases reach npm
when the maintainer publishes them — `gh workflow run release.yml -f
tag=v<version>` runs every channel (PyPI, npm, the images, the CLI binaries);
not every GitHub Release is published. Its npm channel is the `npm publish`
workflow (`.github/workflows/npm-publish.yml`), which also runs alone (`gh
workflow run npm-publish.yml -f tag=v<version>`): it builds the release tag and
publishes it at its version, together with `docling.rs-cuda` (the `cuda`
input, on by default) and `docling.rs-wasm`; without a tag it builds the
latest master (the workspace version). Versions already on npm are skipped,
so re-runs are safe.

## Build from source

This package lives in the docling.rs Cargo workspace and can also build the
addon from Rust source — needed for local development or an unsupported
platform. You need a Rust toolchain (1.92+) and Node.js 14+ (or Bun).

```bash
cd crates/docling-node
npm install          # installs @napi-rs/cli
npm run build        # release build → docling.rs.<platform>.node + native.js/.d.ts
# npm run build:debug  # faster, unoptimized
```

> The addon statically links the ONNX runtime used by the PDF/image pipeline, so
> the built `.node` is large. Declarative formats (Markdown, HTML, DOCX, …) don't
> touch it; only PDF/image conversion loads the ML models (downloaded on first
> use, like the CLI) — and not even that under `pipeline: 'vlm'`, which converts
> through a remote endpoint.

### GPU (CUDA)

The regular `docling.rs` npm binaries are CPU-only. For NVIDIA GPU inference
in the PDF/image ML pipeline (issue #74 — same mechanism as the CLI and the
`docling-rs-cuda` Python wheel) there are two routes:

**Install the [`docling.rs-cuda`](https://www.npmjs.com/package/docling.rs-cuda)
package** (Linux x64). Same JS API; the npm tarball is a few KB and its
`postinstall` downloads the CUDA addon + ONNX Runtime provider libraries from
this repo's matching `npm-cuda-v<version>` GitHub release, verifying each
file against the release's sha256 manifest (the binaries are far past npm's
practical size limits — the same fetch-at-install model `onnxruntime-node`
uses). Requires glibc ≥ 2.38 (Ubuntu 24.04+ era), CUDA 13 + cuDNN 9 at
runtime, and `github.com` access at install time — or set
`DOCLING_RS_NPM_CUDA_URL` to a mirror base URL / local directory with the
same assets (air-gapped installs). To keep existing `require('docling.rs')`
code unchanged, install it under an npm alias:

```bash
npm install docling.rs-cuda
# or: npm install docling.rs@npm:docling.rs-cuda
```

The package is published by the `npm publish` workflow
(`.github/workflows/npm-publish.yml`, run by hand; its `cuda` input is on by
default), which also uploads the release assets (`crates/docling-node/cuda/`
holds the shim sources).

**Or build the addon from source** with the `cuda` feature:

```bash
cd crates/docling-node
export RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN'   # Linux: find provider libs next to the addon
npm run build:cuda    # = napi build ... --features cuda; fetches the CUDA ONNX Runtime (large)
cp ../../target/release/libonnxruntime_providers_{shared,cuda}.so .
```

The CUDA execution provider is two *separate* shared libraries that ONNX
Runtime dlopens at session start; the `$ORIGIN` rpath makes it look next to
the `.node` addon, so ship them alongside it (without them a CUDA build
falls back to CPU with a warning). CUDA 13 + cuDNN 9 must be installed on the
system. A GPU build defaults to `DOCLING_RS_EP=auto` — GPU when usable, CPU
fallback; set `DOCLING_RS_EP=cuda` to fail loudly instead of falling back, or
`DOCLING_RS_EP=cpu` to force CPU. `tensorrt` / `directml` (Windows) /
`coreml` (macOS) features exist too, matching the Rust crates.

## Quick start

```js
import { convertFile, convert, DocumentConverter } from 'docling.rs'

// Convert a file — format detected from the extension.
const { content } = convertFile('report.docx')
console.log(content) // Markdown

// Convert in-memory bytes (e.g. an upload) — pass the format explicitly.
const md = convert({ name: 'notes', data: Buffer.from('# Hi\n'), format: 'md' })

// docling-core JSON instead of Markdown.
const json = convertFile('report.docx', { to: 'json' })

// Reuse a converter across many documents.
const converter = new DocumentConverter({ strict: true })
const a = converter.convert({ name: 'a.md', data: Buffer.from('# A\n') })
```

CommonJS works too: `const { convertFile } = require('docling.rs')`.

### Async (off the event loop)

Conversion is CPU-bound; the `*Async` variants run it on the libuv thread pool
so the event loop stays free. Prefer these for PDF/image and for servers.

```js
import { convertFileAsync } from 'docling.rs'

const res = await convertFileAsync('paper.pdf', { to: 'json' })
```

### Streaming Markdown

`streamFileMarkdown` yields Markdown chunks in document order as conversion
progresses. For PDF (whose pages convert in parallel) output starts flowing
before the whole document is done; concatenating the chunks reproduces the
buffered `content` byte-for-byte.

```js
import { streamFileMarkdown } from 'docling.rs'

for await (const chunk of streamFileMarkdown('paper.pdf')) {
  process.stdout.write(chunk)
}
```

### Chunking (docling's chunkers, for RAG)

`chunkFile` / `chunk` / `chunkDocument` (each with an `…Async` variant) run
docling's chunkers over a converted document and return embedding-ready
records. The default is the structure-driven **hierarchical** chunker (one
chunk per document item — whole lists, triplet-serialized tables — with its
heading path); pass `chunker: 'hybrid'` to refine against a token budget
(split oversized chunks, merge undersized same-heading neighbours), matching
docling's `HybridChunker`. The hybrid token counts come from a HuggingFace
`tokenizer.json`: pass a path via `tokenizer`, or omit it to use
`models/chunk/tokenizer.json` (all-MiniLM-L6-v2's — fetched by
`scripts/install/download_dependencies.sh` alongside the ML models, resolved through
the same install-home logic).

```js
import { chunkFileAsync, Pipeline, chunkDocumentAsync } from 'docling.rs'

const chunks = await chunkFileAsync('report.docx', {
  chunker: 'hybrid',
  tokenizer: 'tokenizer.json', // e.g. all-MiniLM-L6-v2's
  maxTokens: 256,
})
for (const c of chunks) {
  await embed(c.contextualized) // heading path + text, ready for the embedder
}

// Chunk something you already converted (no re-conversion), e.g. a PDF
// that went through the warm Pipeline:
const { content } = new Pipeline().convertFile('paper.pdf', { to: 'json' })
const pdfChunks = await chunkDocumentAsync(content)
```

Each `Chunk` is `{ text, headings?, docItems, contextualized }` — `docItems`
holds the source items' JSON-pointer refs (`"#/texts/12"`), `contextualized`
is docling's `contextualize()` rendering to feed the embedding model.

#### Streaming chunks

`streamFileChunks` / `streamChunks` / `streamDocumentChunks` are the streaming
counterparts: async generators that yield each chunk **as the chunkers produce
it** — the first chunk is ready for embedding while the rest of the document
is still being chunked, and no all-chunks array is materialized. Abandoning
the generator early (`break`) cancels the background chunking.

```js
import { streamFileChunks } from 'docling.rs'

for await (const c of streamFileChunks('report.docx', {
  chunker: 'hybrid',
  tokenizer: 'tokenizer.json',
  maxTokens: 256,
})) {
  await embed(c.contextualized) // embedding overlaps the remaining chunking
}
```

### PDF / images: getting the ML models

Declarative formats (Markdown, HTML, DOCX, XLSX, …) are pure Rust and need
nothing. The **PDF/image** path needs the ONNX models (layout, OCR,
TableFormer), which are *not* bundled in the addon — the PDF pages themselves
are parsed and rendered in pure Rust. Converting a PDF/image/METS input
**throws** until the models are on disk. Fetch them with a
one-liner from your app's directory (where you'll `npm install docling.rs`):

> The exception is [`pipeline: 'vlm'`](#vlm-pipeline-remote-endpoint), which
> replaces the ONNX stack with a remote endpoint: it needs **nothing on disk**
> (PDF pages are rendered in pure Rust).

```bash
curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh
```

```js
import { convertFileAsync } from 'docling.rs'

const res = await convertFileAsync('paper.pdf', { to: 'markdown' }) // ✅ works
```

`scripts/install/download_dependencies.sh` fetches everything from this repo's
[GitHub Releases](https://github.com/docling-project/docling.rs/releases) straight into
`./.models` — which this package (and the Rust CLI) look for by
default, relative to the process's current directory, so no env vars or setup
call are needed afterwards:

| Asset | Destination |
| --- | --- |
| **layout** (`layout_heron.onnx`) | `models/layout_heron.onnx` |
| **OCR** rec model + dictionary | `models/ocr_rec.onnx`, `models/ppocr_keys_v1.txt` |
| **OCR** text detector (optional, #429 — lines outside layout regions on scans/images) | `models/ocr_det.onnx` |
| **TableFormer** | `models/tableformer/{encoder,decoder,bbox}.onnx` |

> **layout + TableFormer are PyTorch→ONNX exports**
> (`docling-project/docling-layout-heron`, Apache-2.0;
> `docling-project/docling-models`, CDLA-Permissive-2.0/Apache-2.0 — see
> [`docs/MODELS_NOTICE.md`](../../docs/MODELS_NOTICE.md) for full attribution), not
> docling.rs's own weights — docling.rs hosts the converted `.onnx` as a
> GitHub Release purely so you don't need a local Python/torch toolchain.
> The OCR model is re-hosted, unmodified, from its own public release, on
> the same host for convenience.
>
> Run it from wherever your app lives — the script only writes to `./.models`
> under the current directory, e.g. in a container build step:
> ```bash
> cd /path/to/your/app && curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh
> ```
>
> To use your own export/host instead, point the env vars at it directly:
> `DOCLING_LAYOUT_ONNX`, `DOCLING_OCR_REC_ONNX`, `DOCLING_OCR_DICT`,
> `DOCLING_TABLEFORMER_{ENCODER,DECODER,BBOX}` — an env var always wins over
> the `./.models` default.

```js
checkDependencies() // { home, layout, ocr, ocrDet, tableformer, chunkTokenizer, ready, missing }
```

`ready` describes the **standard** pipeline (the layout model); a VLM-only
install has `ready: false` and still converts.

### Reusing a warm `Pipeline` (many PDFs)

The one-shot `convertFile` / `convertFileAsync` rebuild the pipeline — reloading
every ONNX model — on each call. To convert many PDFs/images, reuse a `Pipeline`
so the models load **once**:

```js
import { Pipeline } from 'docling.rs'

const pipeline = new Pipeline({ strict: true })
for (const path of pdfPaths) {
  const { content } = await pipeline.convertFileAsync(path, { to: 'json' }) // warm models, off the event loop
}

// Or stream a PDF's Markdown as pages finish converting:
for await (const chunk of pipeline.streamFileMarkdown('paper.pdf')) {
  process.stdout.write(chunk)
}
```

`Pipeline` handles `pdf` and `image` inputs (the ML pipeline). The sync
`convertFile` / `convert` block the event loop; the `*Async` variants run on the
libuv thread pool, and `streamFileMarkdown` yields Markdown chunks in document
order as pages finish. Conversions on one instance run one at a time (the
models are mutable sessions) — overlapping `*Async` calls queue in submission
order, so batch throughput comes from keeping the models warm, not from
parallel calls.

The constructor takes the same PDF/image options as the one-shot calls —
`ocrEngine`, `ocrLang`, `ocrMode`, `ocrScale`, `pdfWordGap` (where the PDF
text layer splits words drawn without a space glyph, #921), `pdfKeepTypography`
(keep `„ “ ‚ ‘`, dashes and `…` as drawn, #921), `pdfReportRepairs` (the
result's `pdfRepairs`: what the PDF reader repaired to read the input,
#934), `ocrTextlineOrientation`
(RapidOCR's text-line orientation handling, #666), `noOcr` (docling's `--no-ocr`:
never OCR, keep layout and tables; `skipOcr` is its pre-2.0 name),
`textLayerOnly` (the text layer only, no models — what `noOcr` meant before
2.0, #611; `"auto"` = that path when the text layer carries the document,
the full pipeline with OCR when it does not), `password` (an encrypted PDF's or Office document's password,
#611/#625; `pdfPassword`, its earlier name, still works), `forceFullPageOcr`, `noTextPanels`, `headingHierarchy`, `pages`,
`imagesScale` / `pageImages`
(picture-crop resolution in px per point and the JSON page images, docling's
`images_scale` / `generate_page_images`, #520), `documentTimeout` (a per-document
budget in seconds, docling's `document_timeout`: once spent, the pages done so
far are the document, `status` is `"partial_success"` and `errors` says why,
#497), `showProgress` (progress bars on stderr, #783) and the enrichment
switches — and
applies them to every conversion on that instance, e.g.
`new Pipeline({ ocrEngine: 'tesseract', ocrLang: 'por+eng' })` for Portuguese
scans. They are validated on `new` exactly as `DocumentConverter` validates
them (an unknown `ocrLang` throws, #471). The per-call `OutputOptions` only
choose the output (`to`, `imageMode`, `artifactsDir`, `pageBreakPlaceholder`).

`Pipeline` rejects `pipeline: 'vlm'`: that pipeline loads no models, so there
is nothing to keep warm — use `convertFileAsync` or `DocumentConverter`.

### VLM pipeline (remote endpoint)

`pipeline: 'vlm'` (issue #77) replaces the whole ONNX stack — layout, OCR,
TableFormer — with a **Vision Language Model**. Each PDF page is rendered (in
pure Rust) and sent to any OpenAI-compatible vision endpoint (LM Studio, Ollama,
vLLM, or a hosted service) with docling's page-conversion prompt; the returned
DocLang/DocTags markup is parsed by the same reader the CLI uses. No ONNX model
is loaded, so a VLM-only install needs nothing on disk.

```js
import { convertFileAsync } from 'docling.rs'

const res = await convertFileAsync('paper.pdf', {
  to: 'markdown',
  pipeline: 'vlm',
  vlmEndpoint: 'http://localhost:11434/v1', // or the full …/chat/completions URL
  vlmModel: 'granite-docling',
  vlmApiKey: process.env.VLM_API_KEY,       // local servers need none
})
```

`vlmEndpoint` / `vlmModel` fall back to `DOCLING_RS_VLM_ENDPOINT` /
`DOCLING_RS_VLM_MODEL`, `vlmApiKey` and `vlmPrompt` to
`DOCLING_RS_VLM_API_KEY` / `DOCLING_RS_VLM_PROMPT`, matching the CLI's
`--pipeline vlm`. Selecting `pipeline: 'vlm'` explicitly is always required —
the environment supplies values, it never switches the pipeline on, so a stale
`DOCLING_RS_VLM_ENDPOINT` can't silently route your PDFs over the network.

> **The `vlm*` options are only read under `pipeline: 'vlm'`.** Passing
> `vlmEndpoint` / `vlmModel` without it is not an error and does not switch
> pipelines: the options are ignored and the conversion runs through the
> standard ONNX pipeline — the same way the CLI parses a stray
> `--vlm-endpoint` given without `--pipeline vlm` and drops it. An option
> configures the VLM; `pipeline` alone selects it. So if a call is loading the
> ML models (or failing on missing ones) when you expected the endpoint to be
> contacted, the missing piece is `pipeline: 'vlm'`.

`pages: 'A-B'` composes (only those pages are rendered and sent), `strict`,
`compactTables`, `allowedFormats` and `to: 'json'` work as usual, and
`streamFileMarkdown` yields the document as a single chunk — a VLM answers a
whole page per request, so there is nothing earlier to emit. PDF and image are
the only accepted inputs; any other format is rejected rather than silently
falling back to the standard pipeline.

The `chunk*` functions have **no** VLM path — `ChunkOptions` carries no
`pipeline`, and they always convert through the ONNX stack. To chunk a
VLM-converted document, convert to JSON first and chunk that:

```js
const { content } = await convertFileAsync('paper.pdf', { to: 'json', pipeline: 'vlm', … })
const chunks = chunkDocument(content, { chunker: 'hybrid' })
```

### Images

Pick how pictures render in Markdown with `imageMode`:

```js
// Inline, self-contained: ![Image](data:image/png;base64,…)
convertFile('slides.pptx', { imageMode: 'embedded' })

// Referenced: links + the image bytes to write yourself.
const res = convertFile('slides.pptx', { imageMode: 'referenced', artifactsDir: 'assets' })
for (const img of res.images) {
  await fs.writeFile(img.path, img.data) // e.g. assets/image_000000.png
}
```

JSON output always embeds extracted images as data URIs.

For scanned PDFs/images, `ocrLang: 'en' | 'ch'` picks the OCR recognition
model (`en` is the default — proper Latin word spacing; `ch` is the
multilingual docling-conformance model; BCP-47 tags for either language —
`'en-US'`, `'eng'`, `'zh'`, `'zh-Hans'`, `'zh-TW'` — resolve to the same two
models, #388), and `pages: 'A-B'` converts only that
1-based PDF page window.

### Pluggable backends

Read a format docling.rs does not know — or one of its formats your way — with
a JavaScript function (#680):

```js
import { convertFile, registerBackend } from 'docling.rs'

registerBackend({
  name: 'rtf2',
  extensions: ['rtf2'],                       // and/or
  mimeTypes: ['application/x-rtf2'],          // convert({ name, data, mimeType })
  sniff: (data) => data.subarray(0, 7).toString() === '{\\rtf2}',
  convert: (data, name) => `# ${name}\n\n${data.toString('utf8')}`,
})

const result = convertFile('notes.rtf2')   // result.format === 'plugin'
```

`convert(data, name)` returns Markdown (a string) or a DoclingDocument (the
docling JSON as an object), which the native side converts under the call's
options — `to`, `imageMode`, `redactPii` and the rest apply. Registered
backends come before the built-in formats, highest `priority` first, so one
can also take a format over (`extensions: ['csv']`; the result keeps
`format: 'csv'`). A claimed input needs no ML models. An async `convert` works
through the `*Async` functions; a thrown error fails the call with the
backend's name.

`registerBackend` covers the module functions (`convert*`, `chunk*`,
`streamFileMarkdown`) and every `DocumentConverter` created afterwards;
`converter.registerBackend(spec)` one converter. `registeredBackends()` and
`unregisterBackend(name)` manage them. Not routed: archive entries, and the
warm `Pipeline` (PDF/images only).

## API

### Functions

| Function | Returns | Notes |
| --- | --- | --- |
| `convertFile(path, options?)` | `ConvertResult` | Detects format from the extension. |
| `convert(input, options?)` | `ConvertResult` | In-memory bytes (`{ name, data, format? }`). |
| `convertFileAsync(path, options?, progress?)` | `Promise<ConvertResult>` | Off the event loop; `progress` hears the conversion's events (#783, below). |
| `convertAsync(input, options?, progress?)` | `Promise<ConvertResult>` | Off the event loop; `progress` as above. |
| `convertArchiveFile(path, options?)` | `ArchiveItem[]` | Every document inside a ZIP (#557): per entry `outcome` `converted` (`result`) / `skipped` (`error` = the reason) / `failed` (`error`); one broken document fails only itself. |
| `convertArchive(input, options?)` | `ArchiveItem[]` | Same, over in-memory bytes (`format` ignored). |
| `convertArchiveFileAsync` / `convertArchiveAsync` | `Promise<ArchiveItem[]>` | Off the event loop. |
| `streamFileMarkdown(path, options?)` | `AsyncGenerator<string>` | Markdown chunks in document order. |
| `chunkFile(path, options?)` | `Chunk[]` | Convert + run docling's hierarchical/hybrid chunker. |
| `chunk(input, options?)` | `Chunk[]` | Same, over in-memory bytes. |
| `chunkDocument(documentJson, options?)` | `Chunk[]` | Chunk an already-converted docling JSON document. |
| `chunkFileAsync` / `chunkAsync` / `chunkDocumentAsync` | `Promise<Chunk[]>` | Off the event loop. |
| `streamFileChunks` / `streamChunks` / `streamDocumentChunks` | `AsyncGenerator<Chunk>` | Chunks yielded as produced; `break` cancels. |
| `emailAttachmentsFile(path, options?)` | `EmailAttachment[]` | The attachments of an `.eml` / `.msg` with their payloads (#561): `{ index, name, contentType, format, size, inline, skipped, data }` — `data` a `Buffer` when kept (absent over a limit or without a payload), `format` the id it converts as (absent, with `skipped` saying why, when it will not), a forwarded message an `.eml` entry. Convert one with `convert({ name: att.name, data: att.data, format: att.format })` — `format` carries what the extension cannot (a `scan.bin` sent as `application/pdf`). `inline` is true for images of the body only; names are unique within the message (`report-2.pdf`). `options`: `maxEntries`, `maxEntrySize`, `maxTotalSize` (bytes; defaults the `DOCLING_RS_ZIP_MAX_*` limits: 10 000 / 256 MiB / 1 GiB). |
| `emailAttachmentsAsync` / `emailAttachmentsFileAsync` | `Promise<EmailAttachment[]>` | Off the event loop (a large `.msg` parses without blocking). |
| `emailAttachments(input, options?)` | `EmailAttachment[]` | Same, over in-memory message bytes (`{ name, data }`). |
| `supportedFormats()` | `string[]` | Supported input format ids. |
| `formatFromName(name)` | `string \| null` | Detect a format id from a filename/extension. |
| `detectFormat(data)` | `string \| null` | The format id a `Buffer` evidently is by its content (#672: PDF, the Office / ODF / EPUB / iWork packages, OLE files, RTF, HTML, images, DjVu, audio/video containers, WordPerfect, WebVTT, DocTags, SVG, AbiWord, XML, JSON and notebooks, MIME messages, LaTeX); `null` for text without a signature. What `convert({ name, data })` does for a name with no or an unknown extension. |
| `checkDependencies(options?)` | `DependencyStatus` | Report which PDF/image deps are present. |
| `registerBackend(spec)` | `string` | Read a format with a JavaScript function (#680): `{ name?, extensions?, mimeTypes?, sniff?, convert(data, name), priority? }`, see [Pluggable backends](#pluggable-backends). |
| `registeredBackends()` / `unregisterBackend(name)` | `RegisteredBackend[]` / `boolean` | List / remove the module-level backends. |

`Pipeline` is the reusable warm PDF/image converter: `new Pipeline(converterOptions)`
then `convertFile` / `convert` / `convertFileAsync` / `convertAsync` /
`convertFileStreaming` / `streamFileMarkdown`. It reads `strict`, the
enrichment switches and every PDF/image option (`ocrEngine`, `ocrLang`,
`ocrMode`, `ocrScale`, `pdfWordGap`, `pdfKeepTypography`, `pdfReportRepairs`, `ocrTextlineOrientation`, `noOcr`, `textLayerOnly`, `password`,
`forceFullPageOcr`, `noTextPanels`, `headingHierarchy`, `pages`, `imagesScale`,
`pageImages`) from
`converterOptions` (#471).

`DocumentConverter` is the reusable form: `new DocumentConverter(converterOptions)`
then `convert` / `convertFile` / `convertFileAsync` / `convertAsync` /
`convertFileStreaming` / `convertArchiveFile` / `convertArchive` (+ `*Async`),
plus `registerBackend` / `registeredBackends` for this converter's own backends.
Converter config (`strict`, `fetchImages`,
`allowedFormats`, `pipeline` + the `vlm*` options) is set once on the
constructor; output options (`to`, `imageMode`, `artifactsDir`) are per call.

### Options

- `to`: `"markdown"` (default), `"json"`, `"html"`, `"text"`, `"latex"`, `"vtt"`
  (WebVTT subtitles from an audio/video transcript or a `.vtt`, #614) or `"pandoc"`
  (Pandoc's JSON AST — pipe it to `pandoc -f json -t docx`; pictures follow
  `imageMode`).
- `imageMode`: `"placeholder"` (default), `"embedded"`, or `"referenced"`.
- `artifactsDir`: directory name used in `referenced` links (default `"artifacts"`).
- `pandocHeadingDepth`: the `Header` depth a section heading gets in the
  `"pandoc"` AST (#848) — `"docling"` (default: the title is `Header 1`, a
  level-*n* section heading `Header n+1`, docling's Markdown / HTML depth) or
  `"source"` (`Header n`: a DOCX `Heading 1` is `Header 1`, as `pandoc -f docx`
  reads it).
- `pageBreakPlaceholder`: text inserted between pages in Markdown output —
  docling's `export_to_markdown(page_break_placeholder=…)`, e.g.
  `"<!-- page break -->"`. Pages come from the PDF/image pipeline, slides,
  sheets and DjVu pages; a break lands only between two rendered blocks on
  different pages (never first or last). Unset: no page breaks, docling's
  default. Markdown only.
- `strict`: cleaner, more conformant Markdown instead of docling's byte-for-byte
  legacy output (Markdown only).
- `fetchImages`: for HTML/EPUB, resolve and embed external `<img src>`. Off by
  default; fetches http(s) URLs over the network — enable only for trusted input.
  The alias of `imageSources: "remote"`.
- `imageSources` (#646): which image references resolve — `"none"` (default),
  `"embedded"` (`data:` URIs and parts of the same container: EPUB/MHTML entries,
  an email's `cid:` attachments; no filesystem, no network — the tier for
  untrusted input), `"local"` (plus files under the source file's directory,
  never an absolute path or one that escapes it), `"remote"` (plus http(s)
  fetches). HTML, EPUB, MHTML, JATS, AsciiDoc, ODF, Markdown (`![…](data:…)`,
  inline `<img>`) and email bodies (`cid:` images in body order).
  `imageHosts` (exact names or `*.suffix`) confines `"remote"` to listed hosts,
  redirects included; `maxImageBytes` (32 MiB), `maxImages`, `maxImageTotalMb`
  and `minImageBytes` bound what a document may make the process hold — over a
  limit a picture stays a placeholder, the conversion never fails.
- `allowedFormats`: restrict the converter to these format ids/extensions.
- `pages`: convert only this PDF page window, `"A-B"` or `"N"` (1-based inclusive).
- `ocrLang`: OCR recognition language for scanned pages, `"en"` (default) or
  `"ch"` (the multilingual docling-conformance model), or a BCP-47 tag for
  either language (`"en-US"`, `"eng"`, `"zh"`, `"zh-Hans"`, `"zh-TW"`; docling's
  `iso:` prefix accepted). Other languages are rejected.
- `forceFullPageOcr`: OCR every PDF page even when it carries a text layer
  (docling's `force_full_page_ocr`).
- `replaceBrokenTextLayer`: OCR only the PDF pages whose text layer a broken
  font mapping produced (docling's `replace_broken_text_layer`, #1006).
- `noTextPanels`: keep every detected picture as a picture — disable the
  demotion of uncaptioned dense-text "picture" regions into paragraphs (#173).
- `headingHierarchy`: infer PDF/image section-header levels after assembly
  (docling's `HeadingHierarchyModel`, #302): PDF bookmarks are authoritative,
  then legal/outline numbering, then font style. Off by default — headings
  then keep the flat detected level.
- `scriptPositions`: mark PDF superscripts and subscripts (footnote markers,
  `1st`, `mc²`, `H₂O`) as `formatting.script` runs — docling's
  `script_position_options.enabled` (#1005). Off by default.
- `doPictureClassification` / `doCodeEnrichment` / `doFormulaEnrichment`: the
  opt-in enrichment models (docling's `PdfPipelineOptions` flags of the same
  names, #423) — classify pictures with DocumentFigureClassifier (26 classes
  on the JSON picture item), rewrite code blocks and detect their language
  with the CodeFormulaV2 VLM, decode display formulas to LaTeX. Off by
  default; each needs its model under `.models/` (`scripts/install/download_dependencies.sh --enrich`),
  a missing one warns and skips the pass. CodeFormula is an autoregressive VLM:
  expect seconds per code/formula region on CPU. Also read by `new Pipeline()`.
- `doPictureOcr`: OCR the pictures embedded in non-PDF documents (DOCX/PPTX
  screenshots, HTML figures, sampled video frames) with the OCR models and
  attach the text to the picture as docling's description annotation (#645):
  Markdown prints it between the caption and the image placeholder, the JSON
  picture item carries `meta.description` + the `description` annotation,
  chunks include it. `pictureOcrClasses` (comma-separated
  DocumentFigureClassifier labels, e.g.
  `"screenshot_from_computer,screenshot_from_manual"`) and
  `pictureOcrMinSide` (px, default 32) filter which pictures are read;
  `pictureOcrScale` sets their OCR resolution in px per picture pixel under
  RapidOCR's 2000 px cap (#666; `1.0` = native pixels);
  `ocrTextlineOrientation: true` reads vertical, skewed and upside-down lines;
  `keepPictureImages: false` drops the image bytes after the pass. Off by
  default — every default output is unchanged.
- `doPictureDescription`: describe every picture — any format, PDF figures
  and image inputs included — through an OpenAI-compatible vision endpoint
  (#670); the answer is the picture's description annotation, where
  `doPictureOcr` puts its text. `pictureDescriptionEndpoint` / `Model` /
  `ApiKey` fall back to the `vlm*` options, then `DOCLING_RS_VLM_*`;
  `pictureDescriptionMode` is `"caption"` (MarkItDown's caption prompt) or
  `"ocr"` (markitdown-ocr's text-extraction prompt); `pictureDescriptionPrompt`
  overrides it; `pictureDescriptionConcurrency` pictures are in flight at
  once. Endpoint errors warn; the conversion goes on.
- `redactPii`: redact personal data from the converted document before any
  output (#621, a docling.rs extension) — e-mail, phone, card numbers (Luhn),
  IBANs (mod-97), IP addresses, URL credentials, national IDs and, with the
  NER model under `.models/ner/`, names / organizations / locations; every
  string of the document model is rewritten, so Markdown, JSON, DCLX and
  chunks come out clean. `redactMode` (`"label"` | `"pseudonym"` |
  `"fixed:<text>"`), `redactKinds` (comma-separated), `redactPattern`
  (`NAME=REGEX` lines) and `redactImages` (`"drop"` | `"box_out"` | `"keep"`)
  tune it; the result's `redaction` is the counts per label. Also read by
  `new Pipeline()`.
- `asrModel` / `asrLang`: Whisper model preset and transcription language
  (`"auto"` default) for audio/video sources.
- `encoding`: character encoding of text inputs (Markdown, CSV, AsciiDoc,
  WebVTT, XML, …) — docling's `TextBackendOptions.encoding`, a WHATWG label
  such as `"shift_jis"` or `"koi8-r"`. Unset detects (BOM, UTF-8, a
  statistical guess for a non-Latin encoding — #675 — else windows-1252);
  bytes the named encoding cannot decode fail the conversion.
- `metadata`: document metadata (#679) — `"none"` (default: nowhere), `"json"`
  (a `metadata` object in the docling JSON: title, authors, dates, EXIF,
  audio tags, …) or `"frontmatter"` (YAML front matter at the top of the
  Markdown as well).
- `ipynbOutputs`: Jupyter notebooks (#673) — `"none"` (default: the notebook
  as written; markdown cells, code cells as code items in the kernel's
  language, raw cells as code blocks) or `"text"` (stream and `text/plain`
  results as code blocks after their cell, image outputs as pictures).
- `extractDiagrams`: DOCX DrawingML drawings (#858) — `"mermaid"` (the
  default) adds a `mermaid` code item after a drawing's picture with the
  flowchart its labelled shapes and connectors describe, only when every
  connector names its two end shapes (a drawing laid by eye gets no graph,
  never an invented one); `"none"` turns the extraction off.
- `useOutlineLevelForHeadings`: Word (DOCX / DOC) — a style's `w:outlineLvl`
  alone makes it a heading (docling's `use_outline_level_for_headings`,
  docling#4480; default `true`); `false` for templates whose body styles
  carry an outline level only for the table of contents.
- `videoFrames`: max frames sampled from a video input as timestamped pictures
  (`0` = transcript only; default 8, needs ffmpeg at runtime; `4294967295` =
  every distinct cut, #647).
- `videoSceneThreshold` / `videoFrameMaxSide` / `videoFrameDedupe` (#647):
  ffmpeg's scene score a frame must exceed to be a cut (0.27; 0.6 keeps hard
  cuts only), a cap on each frame's longer side applied inside ffmpeg (0 =
  source size), and the difference-hash distance (0–64) under which a frame
  is a duplicate of a kept one and dropped (unset = keep all; 4–6 collapses a
  re-lit slide). Frames stream one at a time through `doPictureOcr` +
  `keepPictureImages: false`, so a long lecture costs one frame of memory.
- `pipeline`: `"standard"` (default) or `"vlm"` — convert PDF/image pages
  through a remote vision endpoint instead of the ONNX stack (#77). See
  [VLM pipeline](#vlm-pipeline-remote-endpoint). The five `vlm*` options below
  take effect **only** under `"vlm"`; without it they are silently ignored.
- `vlmEndpoint`: the server's `/v1` base or the full `…/chat/completions` URL.
  Falls back to `DOCLING_RS_VLM_ENDPOINT`; required for `pipeline: 'vlm'`.
- `vlmModel`: model name as the server knows it. Falls back to
  `DOCLING_RS_VLM_MODEL`; required for `pipeline: 'vlm'`.
- `vlmApiKey`: Bearer token; local servers need none. Falls back to
  `DOCLING_RS_VLM_API_KEY`.
- `vlmPrompt`: overrides docling's per-page instruction. Falls back to
  `DOCLING_RS_VLM_PROMPT`.
- `vlmMaxTokens`: `max_tokens` per page completion (default `8192`).

### `ConvertResult`

```ts
interface ConvertResult {
  content: string          // Markdown or JSON, per `to`
  format: string           // detected input format id
  status: string           // "success" | "partial_success" | "failure"
  errors: { componentType: string; moduleName: string; errorMessage: string; pageNo?: number }[]
                           // non-empty exactly for "partial_success"
  inputName: string
  images: { path: string; data: Buffer }[] // for the `referenced` image mode
}
```

Full TypeScript types are generated into `index.d.ts` / `native.d.ts`.

### Progress

docling's conversion progress events (docling 2.139, #783): the async
conversions — the module-level `convertFileAsync` / `convertAsync` and the
same methods of `DocumentConverter` and `Pipeline` — take a third
`progress` argument, a `(event) => void` called with every event of that
conversion as it happens: `document_started`, one `page_completed` per PDF
/ image page (`completedPages` counts up to `totalPages`, the pages `pages`
selected; a page the `documentTimeout` cut off comes with `success: false`,
so the count still reaches the total), `enrichment_progress` for the picture
passes, and last `document_completed` with the result's `status`. The
events arrive on the event loop, so a sync conversion would deliver them
only after it returned — which is why the sync calls take none.
`showProgress: true` in the options prints the engine's progress bars to
stderr instead.

```js
import { convertFileAsync } from 'docling.rs'

const { content } = await convertFileAsync('report.pdf', { to: 'markdown' }, (event) => {
  if (event.kind === 'page_completed') {
    console.error(`${event.document_name}: page ${event.completed_pages}/${event.total_pages}`)
  }
})
```

## Examples

The [`examples/`](examples) folder is a self-contained project that depends on
the published `docling.rs` package — `npm install` there, then run any of them:

```bash
cd examples
npm install
node node-basic.mjs        # ESM: file, bytes, JSON, reuse
bun run bun-basic.ts       # Bun + TypeScript: async + streaming
node pdf-pipeline.mjs       # warm Pipeline for PDFs (run scripts/install/download_dependencies.sh first)
```

- [`examples/node-basic.mjs`](examples/node-basic.mjs) — Node.js (ESM): file, bytes, JSON, reuse.
- [`examples/bun-basic.ts`](examples/bun-basic.ts) — Bun + TypeScript, with async and streaming.
- [`examples/pdf-pipeline.mjs`](examples/pdf-pipeline.mjs) — warm `Pipeline` for PDFs.

The smoke test exercises the locally-built addon instead: `npm run build` once at
the package root, then `node test/smoke.mjs` (or `bun test/smoke.mjs`).

## License

MIT, same as the rest of docling.rs.
