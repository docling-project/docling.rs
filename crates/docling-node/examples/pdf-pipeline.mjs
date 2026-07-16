// Converting PDFs/images, and reusing a warm Pipeline. Run with:
//
//   ../../../scripts/install/download_dependencies.sh   # once, from the repo root
//   node examples/pdf-pipeline.mjs path/to/document.pdf
//
// The PDF/image path needs native assets that aren't bundled in the addon
// (pdfium + the layout/OCR/TableFormer ONNX models).
// `scripts/install/download_dependencies.sh` fetches them straight into ./models and
// ./.pdfium, which this package looks for by default — no env vars, no setup
// call needed. Without it, converting a PDF throws a clear error pointing here.

import { checkDependencies, convertFileAsync, Pipeline } from 'docling.rs'

const file = process.argv[2] ?? 'document.pdf'

console.log('deps:', checkDependencies())

// A single file: convertFileAsync is fine (runs off the event loop).
const res = await convertFileAsync(file, { to: 'markdown' })
console.log(res.content.slice(0, 500))

// For MANY PDFs, reuse a warm Pipeline so the models load once instead of per
// call. The *Async variants run off the event loop (calls on one instance
// queue — the models are mutable sessions):
const pipeline = new Pipeline({ strict: true })
for (const path of process.argv.slice(2)) {
  const r = await pipeline.convertFileAsync(path, { to: 'json' })
  const doc = JSON.parse(r.content)
  console.log(`${path}: ${doc.texts.length} text nodes, ${doc.tables.length} tables`)
}

// Stream a PDF's Markdown through the warm pipeline as pages finish converting:
for await (const chunk of pipeline.streamFileMarkdown(file)) {
  process.stdout.write(chunk)
}
