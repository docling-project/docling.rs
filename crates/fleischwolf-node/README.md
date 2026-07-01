# fleischwolf (Node.js / Bun bindings)

Native [Node.js](https://nodejs.org) / [Bun](https://bun.sh) bindings for
[Fleischwolf](https://github.com/artiz/fleischwolf) — a Rust port of
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
npm install fleischwolf   # or: bun add fleischwolf
```

Prebuilt platforms: Linux x64 / arm64 (glibc), macOS x64 / arm64, Windows x64.
The right binary is pulled in automatically as a platform-specific
`optionalDependency` (`fleischwolf-<triple>`). Releases are published to npm by
manually running the `npm publish` workflow
(`.github/workflows/npm-publish.yml`) for a chosen release tag — decoupled from
the crates.io release.

## Build from source

This package lives in the Fleischwolf Cargo workspace and can also build the
addon from Rust source — needed for local development or an unsupported
platform. You need a Rust toolchain (1.82+) and Node.js 14+ (or Bun).

```bash
cd crates/fleischwolf-node
npm install          # installs @napi-rs/cli
npm run build        # release build → fleischwolf.<platform>.node + native.js/.d.ts
# npm run build:debug  # faster, unoptimized
```

> The addon statically links the ONNX runtime used by the PDF/image pipeline, so
> the built `.node` is large. Declarative formats (Markdown, HTML, DOCX, …) don't
> touch it; only PDF/image conversion loads the ML models (downloaded on first
> use, like the CLI).

## Quick start

```js
import { convertFile, convert, DocumentConverter } from 'fleischwolf'

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

CommonJS works too: `const { convertFile } = require('fleischwolf')`.

### Async (off the event loop)

Conversion is CPU-bound; the `*Async` variants run it on the libuv thread pool
so the event loop stays free. Prefer these for PDF/image and for servers.

```js
import { convertFileAsync } from 'fleischwolf'

const res = await convertFileAsync('paper.pdf', { to: 'json' })
```

### Streaming Markdown

`streamFileMarkdown` yields Markdown chunks in document order as conversion
progresses. For PDF (whose pages convert in parallel) output starts flowing
before the whole document is done; concatenating the chunks reproduces the
buffered `content` byte-for-byte.

```js
import { streamFileMarkdown } from 'fleischwolf'

for await (const chunk of streamFileMarkdown('paper.pdf')) {
  process.stdout.write(chunk)
}
```

### PDF / images: installing the ML models

Declarative formats (Markdown, HTML, DOCX, XLSX, …) are pure Rust and need
nothing. The **PDF/image** path needs native assets that are *not* bundled in the
addon — pdfium plus the ONNX models (layout, OCR, TableFormer) — the same way
Python docling downloads its models on first use. Converting a PDF/image/METS
input **throws** until they're installed:

```js
import { installDependencies, checkDependencies, convertFileAsync } from 'fleischwolf'

await convertFileAsync('paper.pdf') // ❌ throws: "requires the PDF/ML dependencies … call installDependencies()"

await installDependencies()          // provisions everything, then:
await convertFileAsync('paper.pdf')  // ✅ works
```

What `installDependencies()` fetches, into `~/.cache/fleischwolf` (override with
`dir` or `$FLEISCHWOLF_HOME`), wiring the matching `DOCLING_*` /
`PDFIUM_DYNAMIC_LIB_PATH` env vars in-process:

| Asset | Source | Required for |
| --- | --- | --- |
| **pdfium** | bblanchon prebuilt (auto, platform-detected) | PDF |
| **OCR** rec model + dictionary | HuggingFace / GitHub (auto) | scanned pages |
| **layout** (`layout_heron.onnx`) | your `modelsUrl` (see below) | PDF **and** image |
| **TableFormer** (`tableformer/*.onnx`) | your `modelsUrl` | tables (else geometric fallback) |

> **layout + TableFormer have no public prebuilt download.** They're PyTorch→ONNX
> exports (`docling-project/docling-layout-heron`, `docling_ibm_models`). Host the
> exported `.onnx` yourself and point `installDependencies` at the base URL via
> `{ modelsUrl }` or `FLEISCHWOLF_MODELS_URL` — it fetches `layout_heron.onnx` and
> `tableformer/{encoder,decoder,bbox}.onnx` from there. Or export them locally
> (repo `scripts/export_layout.py`, `scripts/export_tableformer.py`) and set
> `DOCLING_LAYOUT_ONNX` / `DOCLING_TABLEFORMER_*` — `installDependencies` detects
> those as already installed. Without a layout source it installs pdfium/OCR and
> throws, naming what's missing.

```js
await installDependencies({
  modelsUrl: 'https://you.example/fleischwolf-models', // serves layout_heron.onnx, tableformer/*.onnx
  onProgress: (m) => console.log(m),
})

checkDependencies() // { home, pdfium, layout, ocr, tableformer, ready, missing }
```

### Reusing a warm `Pipeline` (many PDFs)

The one-shot `convertFile` / `convertFileAsync` rebuild the pipeline — reloading
every ONNX model — on each call. To convert many PDFs/images, reuse a `Pipeline`
so the models load **once**:

```js
import { Pipeline } from 'fleischwolf'

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
| `installDependencies(options?)` | `Promise<DependencyStatus>` | Download/validate the PDF/image models + pdfium. |
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

- [`examples/node-basic.mjs`](examples/node-basic.mjs) — Node.js (ESM): file, bytes, JSON, reuse.
- [`examples/bun-basic.ts`](examples/bun-basic.ts) — Bun + TypeScript, with async and streaming.
- [`examples/pdf-pipeline.mjs`](examples/pdf-pipeline.mjs) — `installDependencies` + warm `Pipeline` for PDFs.

```bash
npm run build          # once
node examples/node-basic.mjs
bun run examples/bun-basic.ts
node test/smoke.mjs    # or: bun test/smoke.mjs
```

## License

MIT, same as the rest of Fleischwolf.
