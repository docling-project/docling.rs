# fleischwolf (Node.js / Bun bindings)

Native [Node.js](https://nodejs.org) / [Bun](https://bun.sh) bindings for
[Fleischwolf](https://github.com/artiz/fleischwolf) — a Rust port of
[docling](https://github.com/docling-project/docling). Convert Markdown, HTML,
DOCX, PPTX, XLSX, EPUB, ODF, LaTeX, email, PDF, images and more into a unified
`DoclingDocument`, and export it as **Markdown** or docling-core **JSON**.

Built with [napi-rs](https://napi.rs), so it ships a real native addon (`.node`)
that loads in both Node.js and Bun (Bun implements N-API) — the same binary, no
rebuild between runtimes.

## Install & build

This package lives in the Fleischwolf Cargo workspace and builds the addon from
the Rust source. You need a Rust toolchain (1.82+) and Node.js 14+ (or Bun).

```bash
cd crates/fleischwolf-node
npm install          # installs @napi-rs/cli
npm run build        # release build → index.<platform>.node + native.js/.d.ts
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

- [`examples/node-basic.mjs`](examples/node-basic.mjs) — Node.js (ESM).
- [`examples/bun-basic.ts`](examples/bun-basic.ts) — Bun + TypeScript, with async and streaming.

```bash
npm run build          # once
node examples/node-basic.mjs
bun run examples/bun-basic.ts
node test/smoke.mjs    # or: bun test/smoke.mjs
```

## License

MIT, same as the rest of Fleischwolf.
