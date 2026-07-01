// Fleischwolf from Node.js (ESM). Run with:
//
//   node examples/node-basic.mjs
//
// Shows the four common paths: convert a file, convert in-memory bytes, emit
// docling-core JSON, and reuse a converter. See bun-basic.ts for TypeScript and
// streaming.

import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

import {
  convertFile,
  convert,
  DocumentConverter,
  supportedFormats,
} from 'fleischwolf'

const here = dirname(fileURLToPath(import.meta.url))

// 1. Convert a file on disk — format detected from the extension.
const html = join(here, 'inputs', 'sample.html')
const doc = convertFile(html)
console.log('--- Markdown from sample.html ---')
console.log(doc.content)

// 2. Convert in-memory bytes (e.g. an upload) — pass the format explicitly.
const md = '# Notes\n\nInline math $E = mc^2$ and a [link](https://example.com).\n'
const fromBytes = convert({ name: 'notes', data: Buffer.from(md), format: 'md' })
console.log('--- Round-tripped Markdown ---')
console.log(fromBytes.content)

// 3. Emit docling-core's native JSON wire format instead of Markdown.
const asJson = convertFile(html, { to: 'json' })
const model = JSON.parse(asJson.content)
console.log('--- docling JSON ---')
console.log('schema:', model.schema_name, model.version)
console.log('text nodes:', model.texts.length, '| tables:', model.tables.length)

// 4. Reuse a converter across many documents (config parsed once). `strict`
//    yields cleaner, more conformant Markdown.
const converter = new DocumentConverter({ strict: true })
for (const [name, body] of [
  ['a.md', '# First\n'],
  ['b.md', '# Second\n'],
]) {
  const r = converter.convert({ name, data: Buffer.from(body) })
  console.log(`converted ${name}: ${r.content.trim()}`)
}

console.log('\nsupported formats:', supportedFormats().join(', '))
