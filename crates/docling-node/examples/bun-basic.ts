// docling.rs from Bun, in TypeScript. Run with:
//
//   bun run examples/bun-basic.ts
//
// Bun implements N-API, so the exact same native addon loads — no rebuild. This
// example leans on the TypeScript types and shows async conversion and the
// streaming async generator. It also runs unchanged under `tsx`/`ts-node` on Node.

import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

import {
  convertFileAsync,
  streamFileMarkdown,
  DocumentConverter,
  type ConvertResult,
} from 'docling.rs'

const here = dirname(fileURLToPath(import.meta.url))
const html = join(here, 'inputs', 'sample.html')

// 1. Async conversion — the CPU-bound work runs off the event loop, so this
//    scales to PDF/image without blocking. Fully typed: `res` is ConvertResult.
const res: ConvertResult = await convertFileAsync(html, { to: 'markdown' })
console.log(`converted "${res.inputName}" (${res.format}) → ${res.status}`)
console.log(res.content)

// 2. Streaming Markdown: chunks arrive in document order as conversion
//    progresses. For PDF, pages stream out as each finishes — output starts
//    before the whole document is done. Concatenating the chunks equals the
//    buffered `content`.
console.log('--- streamed ---')
let streamed = ''
for await (const chunk of streamFileMarkdown(html)) {
  process.stdout.write(chunk)
  streamed += chunk
}
console.log(`\n(streamed ${streamed.length} bytes)`)

// 3. Embedded images: pictures become inline base64 data URIs, so the Markdown
//    is self-contained. (This HTML has none; the option is a no-op here, but
//    the same call handles DOCX/PPTX/PDF figures.)
const embedded = new DocumentConverter().convertFile(html, { imageMode: 'embedded' })
console.log('embedded-image Markdown length:', embedded.content.length)
