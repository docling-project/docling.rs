// Dependency *resolution* for the PDF/image ML pipeline.
//
// The declarative backends (Markdown, HTML, DOCX, XLSX, …) are pure Rust and
// need nothing. The PDF/image path needs native assets that are NOT bundled in
// the addon (they're large and licensed separately from docling.rs's own MIT
// code):
//
//   - libpdfium            (PDF text extraction + page rasterization) — required for PDF
//   - RT-DETR layout model (models/layout_heron.onnx)                 — required for PDF & image
//   - PP-OCR rec + dict    (models/ocr_rec.onnx, ppocr_keys_v1.txt)   — used for pages with no text layer
//   - TableFormer          (models/tableformer/{encoder,decoder,bbox}.onnx) — optional; geometric fallback otherwise
//
// This module does NOT download anything — `scripts/download_dependencies.sh`
// does that, fetching everything from this repo's GitHub Releases straight
// into `./models` and `./.pdfium` (see MODELS_NOTICE.md for attribution: the
// layout model and TableFormer are PyTorch→ONNX exports of docling-project's
// own models, re-hosted here as a convenience). This module just resolves
// where those files (or an explicit `DOCLING_*` / `PDFIUM_DYNAMIC_LIB_PATH`
// override) should live, reports whether they're present, and wires the
// matching env vars in-process so the native pipeline finds them — mirroring
// the CWD-relative defaults already baked into the Rust pipeline itself, so a
// plain `convertFileAsync(...)` call needs no explicit setup once
// `download_dependencies.sh` has run.

'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')

// Formats whose conversion requires the ML models + native libs above.
const ML_FORMATS = new Set(['pdf', 'image', 'mets_gbs'])

// pdfium's shared-library filename, by platform.
function pdfiumLibName() {
  switch (process.platform) {
    case 'linux':
      return 'libpdfium.so'
    case 'darwin':
      return 'libpdfium.dylib'
    case 'win32':
      return 'pdfium.dll'
    default:
      throw new Error(`unsupported platform for pdfium: ${process.platform}/${process.arch}`)
  }
}

/**
 * Resolve the install home directory (absolute), and which `pdfium/`-vs-
 * `.pdfium/` layout it uses. Precedence: an explicit `dir` > `$DOCLING_RS_HOME`
 * > the current directory, *if* it already has a local `models/` or `.pdfium/`
 * (the layout `scripts/download_dependencies.sh` and `scripts/pdf_setup.sh`
 * both produce, and the one the native Rust pipeline's own env-var-less
 * defaults already resolve — `models/layout_heron.onnx`, `.pdfium/lib/…` —
 * relative to *its* CWD) > `~/.cache/docling.rs`. This lets a plain
 * `convertFileAsync(...)` call succeed with zero setup (no env vars) whenever
 * the app is run from a directory that already has the dependencies
 * downloaded next to it.
 */
function homeDir(dir) {
  if (dir) return { home: path.resolve(dir), dotPdfium: false }
  if (process.env.DOCLING_RS_HOME) return { home: path.resolve(process.env.DOCLING_RS_HOME), dotPdfium: false }
  const cwd = process.cwd()
  const hasLocal =
    fs.existsSync(path.join(cwd, 'models', 'layout_heron.onnx')) ||
    fs.existsSync(path.join(cwd, '.pdfium', 'lib', pdfiumLibName()))
  if (hasLocal) return { home: cwd, dotPdfium: true }
  return { home: path.join(os.homedir(), '.cache', 'docling.rs'), dotPdfium: false }
}

/**
 * The resolved on-disk location of each dependency: an existing `DOCLING_*` /
 * `PDFIUM_DYNAMIC_LIB_PATH` environment variable wins (so a local Python export
 * is honored), else the path under the install home directory.
 */
function resolvePaths(dir) {
  const { home, dotPdfium } = homeDir(dir)
  const models = path.join(home, 'models')

  const pdfiumLibDir =
    process.env.PDFIUM_DYNAMIC_LIB_PATH || path.join(home, dotPdfium ? '.pdfium' : 'pdfium', 'lib')
  return {
    home,
    models,
    pdfiumLibDir,
    pdfiumLib: path.join(pdfiumLibDir, pdfiumLibName()),
    layout: process.env.DOCLING_LAYOUT_ONNX || path.join(models, 'layout_heron.onnx'),
    ocrRec: process.env.DOCLING_OCR_REC_ONNX || path.join(models, 'ocr_rec.onnx'),
    ocrDict: process.env.DOCLING_OCR_DICT || path.join(models, 'ppocr_keys_v1.txt'),
    tfEncoder:
      process.env.DOCLING_TABLEFORMER_ENCODER || path.join(models, 'tableformer', 'encoder.onnx'),
    tfDecoder:
      process.env.DOCLING_TABLEFORMER_DECODER || path.join(models, 'tableformer', 'decoder.onnx'),
    tfBbox: process.env.DOCLING_TABLEFORMER_BBOX || path.join(models, 'tableformer', 'bbox.onnx'),
  }
}

/**
 * Report which dependencies are present on disk. `ready` is true when the
 * minimum for PDF (pdfium + layout) is present.
 */
function checkDependencies(options = {}) {
  const p = resolvePaths(options.dir)
  const has = (f) => fs.existsSync(f)
  const status = {
    home: p.home,
    pdfium: has(p.pdfiumLib),
    layout: has(p.layout),
    ocr: has(p.ocrRec) && has(p.ocrDict),
    tableformer: has(p.tfEncoder) && has(p.tfDecoder) && has(p.tfBbox),
  }
  status.ready = status.pdfium && status.layout
  status.missing = [
    !status.pdfium && 'pdfium',
    !status.layout && 'layout_heron.onnx',
  ].filter(Boolean)
  return status
}

/** Point the current process at installed assets (so the native pipeline finds them). */
function exportEnv(p) {
  if (fs.existsSync(p.pdfiumLib)) process.env.PDFIUM_DYNAMIC_LIB_PATH = p.pdfiumLibDir
  if (fs.existsSync(p.layout)) process.env.DOCLING_LAYOUT_ONNX = p.layout
  if (fs.existsSync(p.ocrRec)) process.env.DOCLING_OCR_REC_ONNX = p.ocrRec
  if (fs.existsSync(p.ocrDict)) process.env.DOCLING_OCR_DICT = p.ocrDict
  if (fs.existsSync(p.tfEncoder)) process.env.DOCLING_TABLEFORMER_ENCODER = p.tfEncoder
  if (fs.existsSync(p.tfDecoder)) process.env.DOCLING_TABLEFORMER_DECODER = p.tfDecoder
  if (fs.existsSync(p.tfBbox)) process.env.DOCLING_TABLEFORMER_BBOX = p.tfBbox
}

/**
 * A copy-pasteable next step, shown when a PDF/image/METS conversion is
 * attempted without the dependencies on disk.
 */
function downloadGuide() {
  return [
    'Run this once from your app\'s directory (fetches pdfium + the ONNX',
    'models — layout, OCR, TableFormer — from this repo\'s GitHub Releases',
    'straight into ./models and ./.pdfium, which this package looks for by',
    'default; no env vars needed afterwards):',
    '',
    '  curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/download_dependencies.sh | sh',
    '',
    'or, from a checkout of the repo:',
    '',
    '  scripts/download_dependencies.sh',
    '',
    'TableFormer is optional (tables fall back to geometric reconstruction',
    'without it). To use your own export/host instead, point the DOCLING_*',
    'env vars at it directly: DOCLING_LAYOUT_ONNX, DOCLING_OCR_REC_ONNX,',
    'DOCLING_OCR_DICT, DOCLING_TABLEFORMER_{ENCODER,DECODER,BBOX},',
    'PDFIUM_DYNAMIC_LIB_PATH — see MODELS_NOTICE.md for licensing.',
    '',
    'Declarative formats (md, html, docx, xlsx, …) need none of this — only',
    'PDF, image and METS conversion do.',
  ].join('\n')
}

/**
 * Throw a clear, actionable error if `format` needs the ML pipeline but its
 * dependencies aren't installed. Called before ML conversions; also wires up
 * the `DOCLING_*` / `PDFIUM_DYNAMIC_LIB_PATH` env vars for whatever is present,
 * so a checkout with `scripts/download_dependencies.sh` already run just works.
 */
function assertMlReady(format, dir) {
  if (!ML_FORMATS.has(format)) return
  const p = resolvePaths(dir)
  exportEnv(p)
  const status = checkDependencies({ dir })
  // Image needs layout (+OCR), but not pdfium; PDF/METS need both.
  const needPdfium = format !== 'image'
  const missing = [!status.layout && 'layout_heron.onnx', needPdfium && !status.pdfium && 'pdfium'].filter(
    Boolean,
  )
  if (missing.length === 0) return
  throw new Error(
    `Converting '${format}' requires the PDF/ML dependencies, which are not installed: ` +
      `${missing.join(', ')}.\n\n${downloadGuide()}`,
  )
}

module.exports = {
  ML_FORMATS,
  checkDependencies,
  assertMlReady,
  resolvePaths,
  exportEnv,
}
