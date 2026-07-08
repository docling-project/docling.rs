# docling.rs (Node.js / Bun bindings)

Native [Node.js](https://nodejs.org) / [Bun](https://bun.sh) bindings for
[docling.rs](https://github.com/artiz/docling.rs) — a Rust port of
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

### PDF / images: getting the ML models

Declarative formats (Markdown, HTML, DOCX, XLSX, …) are pure Rust and need
nothing. The **PDF/image** path needs native assets that are *not* bundled in the
addon — pdfium plus the ONNX models (layout, OCR, TableFormer). Converting a
PDF/image/METS input **throws** until they're on disk. Fetch them with a
one-liner from your app's directory (where you'll `npm install docling.rs`):

```bash
curl -fsSL https://raw.githubusercontent.com/artiz/docling.rs/master/scripts/download_dependencies.sh | sh
```

```js
import { convertFileAsync } from 'docling.rs'

const res = await convertFileAsync('paper.pdf', { to: 'markdown' }) // ✅ works
```

`scripts/download_dependencies.sh` fetches everything from this repo's
[GitHub Releases](https://github.com/artiz/docling.rs/releases) straight into
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
> [`MODELS_NOTICE.md`](../../MODELS_NOTICE.md) for full attribution), not
> docling.rs's own weights — docling.rs hosts the converted `.onnx` as a
> GitHub Release purely so you don't need a local Python/torch toolchain.
> pdfium and the OCR model are re-hosted, unmodified, from their own public
> releases, on the same host for convenience.
>
> Run it from wherever your app lives — the script only writes to `./models`
> and `./.pdfium` under the current directory, e.g. in a container build step:
> ```bash
> cd /path/to/your/app && curl -fsSL https://raw.githubusercontent.com/artiz/docling.rs/master/scripts/download_dependencies.sh | sh
> ```
>
> To use your own export/host instead, point the env vars at it directly:
> `DOCLING_LAYOUT_ONNX`, `DOCLING_OCR_REC_ONNX`, `DOCLING_OCR_DICT`,
> `DOCLING_TABLEFORMER_{ENCODER,DECODER,BBOX}`, `PDFIUM_DYNAMIC_LIB_PATH` — an
> env var always wins over the `./models` / `./.pdfium` default.

```js
checkDependencies() // { home, pdfium, layout, ocr, tableformer, ready, missing }
```

### Reusing a warm `Pipeline` (many PDFs)

The one-shot `convertFile` / `convertFileAsync` rebuild the pipeline — reloading
every ONNX model — on each call. To convert many PDFs/images, reuse a `Pipeline`
so the models load **once**:

```js
import { Pipeline } from 'docling.rs'

const pipeline = new Pipeline({ strict: true })
for (const path of pdfPaths) {
  const { content } = pipeline.convertFile(path, { to: 'markdown' }) // warm models
}
```

`Pipeline` handles `pdf` and `image` inputs (the ML pipeline) and is synchronous
— reuse one instance behind a job queue.

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

## API

### Functions

| Function | Returns | Notes |
| --- | --- | --- |
| `convertFile(path, options?)` | `ConvertResult` | Detects format from the extension. |
| `convert(input, options?)` | `ConvertResult` | In-memory bytes (`{ name, data, format? }`). |
| `convertFileAsync(path, options?)` | `Promise<ConvertResult>` | Off the event loop. |
| `convertAsync(input, options?)` | `Promise<ConvertResult>` | Off the event loop. |
| `streamFileMarkdown(path, options?)` | `AsyncGenerator<string>` | Markdown chunks in document order. |
| `supportedFormats()` | `string[]` | Supported input format ids. |
| `formatFromName(name)` | `string \| null` | Detect a format id from a filename/extension. |
| `checkDependencies(options?)` | `DependencyStatus` | Report which PDF/image deps are present. |

`Pipeline` is the reusable warm PDF/image converter: `new Pipeline(converterOptions)`
then `convertFile` / `convert`.

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
node pdf-pipeline.mjs       # warm Pipeline for PDFs (run scripts/download_dependencies.sh first)
```

- [`examples/node-basic.mjs`](examples/node-basic.mjs) — Node.js (ESM): file, bytes, JSON, reuse.
- [`examples/bun-basic.ts`](examples/bun-basic.ts) — Bun + TypeScript, with async and streaming.
- [`examples/pdf-pipeline.mjs`](examples/pdf-pipeline.mjs) — warm `Pipeline` for PDFs.

The smoke test exercises the locally-built addon instead: `npm run build` once at
the package root, then `node test/smoke.mjs` (or `bun test/smoke.mjs`).

## License

MIT, same as the rest of docling.rs.
