// Converting PDFs/images: installing the ML dependencies and reusing a warm
// Pipeline. Run with:
//
//   node examples/pdf-pipeline.mjs path/to/document.pdf
//
// The PDF/image path needs native assets that aren't bundled in the addon
// (pdfium + the layout/OCR/TableFormer ONNX models). `installDependencies()`
// provisions them; without it, converting a PDF throws a clear error.

import { installDependencies, checkDependencies, convertFileAsync, Pipeline } from 'fleischwolf'

const file = process.argv[2] ?? 'document.pdf'

// 1. Without the models, PDF conversion throws — the guard points you here.
console.log('deps before:', checkDependencies())
try {
  await convertFileAsync(file)
} catch (err) {
  console.log('\nexpected (models not installed yet):\n ', err.message.split('.')[0], '…\n')
}

// 2. Install them. pdfium + OCR download automatically; the layout + TableFormer
//    ONNX come from a base URL you host (or set FLEISCHWOLF_MODELS_URL). If you
//    exported the models locally, set DOCLING_LAYOUT_ONNX etc. instead and this
//    call just validates + wires them up.
await installDependencies({
  modelsUrl: process.env.FLEISCHWOLF_MODELS_URL, // e.g. https://you.example/fleischwolf-models
  onProgress: (m) => console.log('  ·', m),
})
console.log('deps after:', checkDependencies(), '\n')

// 3. Convert. For a single file, convertFileAsync is fine (runs off the event loop):
const res = await convertFileAsync(file, { to: 'markdown' })
console.log(res.content.slice(0, 500))

// 4. For MANY PDFs, reuse a warm Pipeline so the models load once instead of per call:
const pipeline = new Pipeline({ strict: true })
for (const path of process.argv.slice(2)) {
  const r = pipeline.convertFile(path, { to: 'json' })
  const doc = JSON.parse(r.content)
  console.log(`${path}: ${doc.texts.length} text nodes, ${doc.tables.length} tables`)
}
