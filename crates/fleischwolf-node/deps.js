// Dependency provisioning for the PDF/image ML pipeline.
//
// The declarative backends (Markdown, HTML, DOCX, XLSX, …) are pure Rust and
// need nothing. The PDF/image path needs native assets that are NOT bundled in
// the addon (they're large and licensed separately), mirroring how Python
// docling downloads its models on first use:
//
//   - libpdfium            (PDF text extraction + page rasterization) — required for PDF
//   - RT-DETR layout model (models/layout_heron.onnx)                 — required for PDF & image
//   - PP-OCR rec + dict    (models/ocr_rec.onnx, ppocr_keys_v1.txt)   — used for pages with no text layer
//   - TableFormer          (models/tableformer/{encoder,decoder,bbox}.onnx) — optional; geometric fallback otherwise
//
// pdfium and the OCR assets have public download URLs and are fetched
// automatically. The layout and TableFormer models are exported from PyTorch
// (docling-project/docling-layout-heron and docling_ibm_models) and have no
// public prebuilt `.onnx`, so they are downloaded from a base URL you provide
// via `installDependencies({ modelsUrl })` or the `FLEISCHWOLF_MODELS_URL`
// environment variable (point it at a host serving `layout_heron.onnx` and
// `tableformer/*.onnx`). If you exported them locally, set `DOCLING_LAYOUT_ONNX`
// etc. and they'll be detected as already installed.
//
// Everything is installed under a single home directory (default
// `~/.cache/fleischwolf`, overridable via `FLEISCHWOLF_HOME` or the `dir`
// option), and the corresponding `DOCLING_*` / `PDFIUM_DYNAMIC_LIB_PATH`
// environment variables are set in-process so the native pipeline finds them.

'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')
const http = require('http')
const https = require('https')
const { execFileSync } = require('child_process')

// Formats whose conversion requires the ML models + native libs above.
const ML_FORMATS = new Set(['pdf', 'image', 'mets_gbs'])

const PDFIUM_RELEASE =
  'https://github.com/bblanchon/pdfium-binaries/releases/latest/download'
const OCR_REC_URL =
  'https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx'
const OCR_DICT_URL =
  'https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt'

// pdfium-binaries platform tag + shared-library filename, by (platform, arch).
function pdfiumPlatform() {
  const arch = process.arch === 'arm64' ? 'arm64' : process.arch === 'x64' ? 'x64' : process.arch
  switch (process.platform) {
    case 'linux':
      return { tag: `linux-${arch}`, lib: 'libpdfium.so' }
    case 'darwin':
      return { tag: `mac-${arch}`, lib: 'libpdfium.dylib' }
    case 'win32':
      return { tag: `win-${arch}`, lib: 'pdfium.dll' }
    default:
      throw new Error(`unsupported platform for pdfium: ${process.platform}/${process.arch}`)
  }
}

/** Resolve the install home directory (absolute). */
function homeDir(dir) {
  if (dir) return path.resolve(dir)
  if (process.env.FLEISCHWOLF_HOME) return path.resolve(process.env.FLEISCHWOLF_HOME)
  return path.join(os.homedir(), '.cache', 'fleischwolf')
}

/**
 * The resolved on-disk location of each dependency: an existing `DOCLING_*` /
 * `PDFIUM_DYNAMIC_LIB_PATH` environment variable wins (so a local Python export
 * is honored), else the path under the install home directory.
 */
function resolvePaths(dir) {
  const home = homeDir(dir)
  const models = path.join(home, 'models')
  const { lib } = pdfiumPlatform()

  const pdfiumLibDir = process.env.PDFIUM_DYNAMIC_LIB_PATH || path.join(home, 'pdfium', 'lib')
  return {
    home,
    models,
    pdfiumLibDir,
    pdfiumLib: path.join(pdfiumLibDir, lib),
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
 * Report which dependencies are present on disk, without downloading anything.
 * `ready` is true when the minimum for PDF (pdfium + layout) is present.
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
 * Throw a clear, actionable error if `format` needs the ML pipeline but its
 * dependencies aren't installed. Called before ML conversions.
 */
function assertMlReady(format, dir) {
  if (!ML_FORMATS.has(format)) return
  const status = checkDependencies({ dir })
  // Image needs layout (+OCR), but not pdfium; PDF/METS need both.
  const needPdfium = format !== 'image'
  const missing = [!status.layout && 'layout_heron.onnx', needPdfium && !status.pdfium && 'pdfium'].filter(
    Boolean,
  )
  if (missing.length === 0) return
  throw new Error(
    `Converting '${format}' requires the PDF/ML dependencies, which are not installed: ` +
      `${missing.join(', ')}. Call \`await installDependencies()\` before converting ` +
      `(pass { modelsUrl } or set FLEISCHWOLF_MODELS_URL so the layout/TableFormer ONNX can be ` +
      `fetched; or set DOCLING_LAYOUT_ONNX / PDFIUM_DYNAMIC_LIB_PATH to local files). ` +
      `Declarative formats (md, html, docx, xlsx, …) need none of this.`,
  )
}

// --- downloading -----------------------------------------------------------

function download(url, dest, onProgress) {
  return new Promise((resolve, reject) => {
    const tmp = `${dest}.download`
    const client = url.startsWith('http://') ? http : https
    const req = client.get(url, { headers: { 'User-Agent': 'fleischwolf-node' } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume()
        return download(res.headers.location, dest, onProgress).then(resolve, reject)
      }
      if (res.statusCode !== 200) {
        res.resume()
        return reject(new Error(`GET ${url} → HTTP ${res.statusCode}`))
      }
      fs.mkdirSync(path.dirname(dest), { recursive: true })
      const out = fs.createWriteStream(tmp)
      res.pipe(out)
      out.on('finish', () => out.close(() => {
        fs.renameSync(tmp, dest)
        resolve(dest)
      }))
      out.on('error', reject)
    })
    req.on('error', reject)
  })
}

async function ensureFile(dest, url, force, onProgress, label) {
  if (!force && fs.existsSync(dest)) return false
  onProgress?.(`downloading ${label}`)
  await download(url, dest, onProgress)
  return true
}

async function installPdfium(p, force, onProgress) {
  if (!force && fs.existsSync(p.pdfiumLib)) return false
  if (process.env.PDFIUM_DYNAMIC_LIB_PATH) {
    // The user pointed us at a pdfium directory that doesn't contain the lib.
    throw new Error(
      `PDFIUM_DYNAMIC_LIB_PATH is set to '${p.pdfiumLibDir}' but no pdfium library was found there.`,
    )
  }
  const { tag } = pdfiumPlatform()
  const url = `${PDFIUM_RELEASE}/pdfium-${tag}.tgz`
  const home = p.home
  const pdfiumRoot = path.join(home, 'pdfium')
  fs.mkdirSync(pdfiumRoot, { recursive: true })
  const tgz = path.join(pdfiumRoot, 'pdfium.tgz')
  onProgress?.(`downloading pdfium (${tag})`)
  await download(url, tgz)
  onProgress?.('extracting pdfium')
  // pdfium-binaries ships a .tgz; use the system `tar` (present on Linux, macOS,
  // and Windows 10+). The archive lays out lib/<libpdfium> which matches pdfiumLibDir.
  execFileSync('tar', ['-xzf', tgz, '-C', pdfiumRoot])
  fs.rmSync(tgz, { force: true })
  if (!fs.existsSync(p.pdfiumLib)) {
    throw new Error(`pdfium extracted but ${p.pdfiumLib} is missing (unexpected archive layout)`)
  }
  return true
}

/**
 * Download and install everything the PDF/image pipeline needs, then point the
 * process at it. Idempotent: skips assets already present (pass `{ force: true }`
 * to re-download). Returns a status report.
 *
 * @param {object} [options]
 * @param {string} [options.dir]         install home (default ~/.cache/fleischwolf or $FLEISCHWOLF_HOME)
 * @param {string} [options.modelsUrl]   base URL serving layout_heron.onnx + tableformer/*.onnx
 * @param {boolean} [options.ocr=true]   also fetch the OCR model + dictionary
 * @param {boolean} [options.tableformer=true] also fetch TableFormer from modelsUrl (if provided)
 * @param {boolean} [options.force=false] re-download assets that already exist
 * @param {(msg: string) => void} [options.onProgress]
 */
async function installDependencies(options = {}) {
  const p = resolvePaths(options.dir)
  const onProgress = options.onProgress
  const installed = []
  const missing = []
  fs.mkdirSync(p.models, { recursive: true })

  // 1. pdfium (required for PDF).
  if (await installPdfium(p, options.force, onProgress)) installed.push('pdfium')

  // 2. OCR recognition model + dictionary (for pages without a text layer).
  if (options.ocr !== false) {
    if (await ensureFile(p.ocrRec, OCR_REC_URL, options.force, onProgress, 'OCR model'))
      installed.push('ocr_rec.onnx')
    if (await ensureFile(p.ocrDict, OCR_DICT_URL, options.force, onProgress, 'OCR dictionary'))
      installed.push('ppocr_keys_v1.txt')
  }

  // 3. Layout (required) + TableFormer (optional) — from the configured base URL.
  const base = (options.modelsUrl || process.env.FLEISCHWOLF_MODELS_URL || '').replace(/\/$/, '')
  if (!fs.existsSync(p.layout)) {
    if (base) {
      if (
        await ensureFile(p.layout, `${base}/layout_heron.onnx`, options.force, onProgress, 'layout model')
      )
        installed.push('layout_heron.onnx')
    } else {
      missing.push('layout_heron.onnx')
    }
  }
  if (options.tableformer !== false && base) {
    for (const [file, dest] of [
      ['tableformer/encoder.onnx', p.tfEncoder],
      ['tableformer/decoder.onnx', p.tfDecoder],
      ['tableformer/bbox.onnx', p.tfBbox],
    ]) {
      try {
        if (await ensureFile(dest, `${base}/${file}`, options.force, onProgress, file))
          installed.push(file)
      } catch (e) {
        // TableFormer is optional (geometric fallback); note but don't fail.
        onProgress?.(`skipped ${file}: ${e.message}`)
      }
    }
  }

  exportEnv(p)
  const status = checkDependencies(options)

  if (!status.ready) {
    const hint = base
      ? `layout_heron.onnx could not be fetched from ${base}.`
      : `no models URL configured — pass { modelsUrl } to installDependencies() or set ` +
        `FLEISCHWOLF_MODELS_URL to a host serving layout_heron.onnx (and tableformer/*.onnx). ` +
        `The layout model is a PyTorch→ONNX export (docling-project/docling-layout-heron) with no ` +
        `public prebuilt download; export it with the repo's scripts/export_layout.py and host it, ` +
        `or set DOCLING_LAYOUT_ONNX to the local file.`
    throw new Error(
      `installDependencies: PDF conversion is not ready. Missing: ${status.missing.join(', ')}. ${hint}`,
    )
  }

  return { ...status, installed, missing }
}

module.exports = {
  ML_FORMATS,
  installDependencies,
  checkDependencies,
  assertMlReady,
  resolvePaths,
  exportEnv,
}
