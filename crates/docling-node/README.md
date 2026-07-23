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

Prebuilt platforms: Linux x64 / arm64 (glibc) and Windows x64. (macOS isn't
prebuilt — build from source, see below.) The right binary is pulled in
automatically as a platform-specific `optionalDependency` (`docling.rs-<triple>`). Releases are published to npm by
manually running the `npm publish` workflow
(`.github/workflows/npm-publish.yml`) — by default it builds the latest master
(the workspace version); optionally pass a release tag to build that instead.
Decoupled from the crates.io release.

## Build from source

This package lives in the docling.rs Cargo workspace and can also build the
addon from Rust source — needed for local development or an unsupported
platform. You need a Rust toolchain (1.82+) and Node.js 14+ (or Bun).

```bash
cd crates/docling-node
npm install          # installs @napi-rs/cli
npm run build        # release build → docling.rs.<platform>.node + native.js/.d.ts
# npm run build:debug  # faster, unoptimized
```

> The addon statically links the ONNX runtime used by the PDF/image pipeline, so
> the built `.node` is large. Declarative formats (Markdown, HTML, DOCX, …) don't
> touch it; only PDF/image conversion loads the ML models (downloaded on first
> use, like the CLI).

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
uses). Requires glibc ≥ 2.38 (Ubuntu 24.04+ era), CUDA 12 + cuDNN 9 at
runtime, and `github.com` access at install time — or set
`DOCLING_RS_NPM_CUDA_URL` to a mirror base URL / local directory with the
same assets (air-gapped installs). To keep existing `require('docling.rs')`
code unchanged, install it under an npm alias:

```bash
npm install docling.rs-cuda
# or: npm install docling.rs@npm:docling.rs-cuda
```

The package is published by the `npm publish` workflow's `cuda` input
(`.github/workflows/npm-publish.yml`), which also uploads the release assets
(`crates/docling-node/cuda/` holds the shim sources).

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
falls back to CPU with a warning). CUDA 12 + cuDNN 9 must be installed on the
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
nothing. The **PDF/image** path needs native assets that are *not* bundled in the
addon — pdfium plus the ONNX models (layout, OCR, TableFormer). Converting a
PDF/image/METS input **throws** until they're on disk. Fetch them with a
one-liner from your app's directory (where you'll `npm install docling.rs`):

```bash
curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh
```

```js
import { convertFileAsync } from 'docling.rs'

const res = await convertFileAsync('paper.pdf', { to: 'markdown' }) // ✅ works
```

`scripts/install/download_dependencies.sh` fetches everything from this repo's
[GitHub Releases](https://github.com/docling-project/docling.rs/releases) straight into
`./models` and `./.pdfium` — which this package (and the Rust CLI) look for by
default, relative to the process's current directory, so no env vars or setup
call are needed afterwards:

| Asset | Destination |
| --- | --- |
| **pdfium** | `.pdfium/lib/libpdfium.so` |
| **layout** (`layout_heron.onnx`) | `models/layout_heron.onnx` |
| **OCR** rec model + dictionary | `models/ocr_rec.onnx`, `models/ppocr_keys_v1.txt` |
| **TableFormer** | `models/tableformer/{encoder,decoder,bbox}.onnx` |

> **layout + TableFormer are PyTorch→ONNX exports**
> (`docling-project/docling-layout-heron`, Apache-2.0;
> `docling-project/docling-models`, CDLA-Permissive-2.0/Apache-2.0 — see
> [`docs/MODELS_NOTICE.md`](../../docs/MODELS_NOTICE.md) for full attribution), not
> docling.rs's own weights — docling.rs hosts the converted `.onnx` as a
> GitHub Release purely so you don't need a local Python/torch toolchain.
> pdfium and the OCR model are re-hosted, unmodified, from their own public
> releases, on the same host for convenience.
>
> Run it from wherever your app lives — the script only writes to `./models`
> and `./.pdfium` under the current directory, e.g. in a container build step:
> ```bash
> cd /path/to/your/app && curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh
> ```
>
> To use your own export/host instead, point the env vars at it directly:
> `DOCLING_LAYOUT_ONNX`, `DOCLING_OCR_REC_ONNX`, `DOCLING_OCR_DICT`,
> `DOCLING_TABLEFORMER_{ENCODER,DECODER,BBOX}`, `PDFIUM_DYNAMIC_LIB_PATH` — an
> env var always wins over the `./models` / `./.pdfium` default.

```js
checkDependencies() // { home, pdfium, layout, ocr, tableformer, chunkTokenizer, ready, missing }
```

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
multilingual docling-conformance model), and `pages: 'A-B'` converts only that
1-based PDF page window.

## API

### Functions

| Function | Returns | Notes |
| --- | --- | --- |
| `convertFile(path, options?)` | `ConvertResult` | Detects format from the extension. |
| `convert(input, options?)` | `ConvertResult` | In-memory bytes (`{ name, data, format? }`). |
| `convertFileAsync(path, options?)` | `Promise<ConvertResult>` | Off the event loop. |
| `convertAsync(input, options?)` | `Promise<ConvertResult>` | Off the event loop. |
| `streamFileMarkdown(path, options?)` | `AsyncGenerator<string>` | Markdown chunks in document order. |
| `chunkFile(path, options?)` | `Chunk[]` | Convert + run docling's hierarchical/hybrid chunker. |
| `chunk(input, options?)` | `Chunk[]` | Same, over in-memory bytes. |
| `chunkDocument(documentJson, options?)` | `Chunk[]` | Chunk an already-converted docling JSON document. |
| `chunkFileAsync` / `chunkAsync` / `chunkDocumentAsync` | `Promise<Chunk[]>` | Off the event loop. |
| `streamFileChunks` / `streamChunks` / `streamDocumentChunks` | `AsyncGenerator<Chunk>` | Chunks yielded as produced; `break` cancels. |
| `supportedFormats()` | `string[]` | Supported input format ids. |
| `formatFromName(name)` | `string \| null` | Detect a format id from a filename/extension. |
| `checkDependencies(options?)` | `DependencyStatus` | Report which PDF/image deps are present. |

`Pipeline` is the reusable warm PDF/image converter: `new Pipeline(converterOptions)`
then `convertFile` / `convert` / `convertFileAsync` / `convertAsync` /
`convertFileStreaming` / `streamFileMarkdown`.

`DocumentConverter` is the reusable form: `new DocumentConverter(converterOptions)`
then `convert` / `convertFile` / `convertFileAsync` / `convertAsync` /
`convertFileStreaming`. Converter config (`strict`, `fetchImages`,
`allowedFormats`) is set once on the constructor; output options (`to`,
`imageMode`, `artifactsDir`) are per call.

### Options

- `to`: `"markdown"` (default) or `"json"`.
- `imageMode`: `"placeholder"` (default), `"embedded"`, or `"referenced"`.
- `artifactsDir`: directory name used in `referenced` links (default `"artifacts"`).
- `strict`: cleaner, more conformant Markdown instead of docling's byte-for-byte
  legacy output (Markdown only).
- `fetchImages`: for HTML/EPUB, resolve and embed external `<img src>`. Off by
  default; fetches http(s) URLs over the network — enable only for trusted input.
- `allowedFormats`: restrict the converter to these format ids/extensions.

### `ConvertResult`

```ts
interface ConvertResult {
  content: string          // Markdown or JSON, per `to`
  format: string           // detected input format id
  status: string           // "success" | "partial_success" | "failure"
  inputName: string
  images: { path: string; data: Buffer }[] // for the `referenced` image mode
}
```

Full TypeScript types are generated into `index.d.ts` / `native.d.ts`.

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
