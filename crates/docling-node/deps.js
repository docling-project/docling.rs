// Dependency *resolution* for the PDF/image ML pipeline.
//
// The declarative backends (Markdown, HTML, DOCX, XLSX, …) are pure Rust and
// need nothing. The PDF/image path needs ONNX models that are NOT bundled in
// the addon (they're large and licensed separately from docling.rs's own MIT
// code); the PDF pages themselves are parsed and rendered in pure Rust:
//
//   - RT-DETR layout model (.models/layout_heron.onnx)                 — required for PDF & image
//   - PP-OCR rec + dict    (.models/ocr_rec.onnx, ppocr_keys_v1.txt)   — used for pages with no text layer
//   - PP-OCR text detector (.models/ocr_det.onnx)                      — optional; reads lines outside layout regions (#429)
//   - Text-line orientation (.models/ocr_textline_ori.onnx)            — optional; ocrTextlineOrientation's 0/180 check (#666)
//   - TableFormer          (.models/tableformer/{encoder,decoder,bbox}.onnx) — optional; geometric fallback otherwise
//
// This module does NOT download anything — `scripts/install/download_dependencies.sh`
// does that, fetching everything from this repo's GitHub Releases straight
// into `./.models` (see docs/MODELS_NOTICE.md for attribution: the
// layout model and TableFormer are PyTorch→ONNX exports of docling-project's
// own models, re-hosted here as a convenience). This module just resolves
// where those files (or an explicit `DOCLING_*` override) should live, reports whether they're present, and wires the
// matching env vars in-process so the native pipeline finds them — mirroring
// the CWD-relative defaults already baked into the Rust pipeline itself, so a
// plain `convertFileAsync(...)` call needs no explicit setup once
// `download_dependencies.sh` has run.

'use strict'

const fs = require('fs')
const os = require('os')
const path = require('path')

// Formats whose conversion requires the ML models above.
const ML_FORMATS = new Set(['pdf', 'image', 'mets_gbs'])

// Of those, the ones the remote VLM pipeline can convert (#77) — `convert_vlm`
// handles PDF and image only.
const VLM_FORMATS = new Set(['pdf', 'image'])

/**
 * Resolve the install home directory (absolute), and which models layout it
 * uses. Precedence: an explicit `dir` > `$DOCLING_RS_HOME`
 * > the current directory, *if* it already has a local `.models/`
 * (the layout `scripts/install/download_dependencies.sh` and `scripts/install/pdf_setup.sh`
 * both produce, and the one the native Rust pipeline's own env-var-less
 * defaults already resolve — `.models/layout_heron.onnx` — relative to *its*
 * CWD) > `~/.cache/docling.rs`. This lets a plain
 * `convertFileAsync(...)` call succeed with zero setup (no env vars) whenever
 * the app is run from a directory that already has the dependencies
 * downloaded next to it.
 */
// Which models layout a home directory uses: `.models/` (what
// download_dependencies.sh writes into a checkout / app dir) or the plain
// `models/` internal to the shared ~/.cache/docling.rs home that docling-py's
// download_models() populates. Probed per directory, so an explicit `dir` /
// $DOCLING_RS_HOME pointing at either layout resolves.
function dotModelsIn(home) {
  return fs.existsSync(path.join(home, '.models'))
}

function homeDir(dir) {
  if (dir) {
    const home = path.resolve(dir)
    return { home, dotModels: dotModelsIn(home) }
  }
  if (process.env.DOCLING_RS_HOME) {
    const home = path.resolve(process.env.DOCLING_RS_HOME)
    return { home, dotModels: dotModelsIn(home) }
  }
  const cwd = process.cwd()
  if (dotModelsIn(cwd)) {
    return { home: cwd, dotModels: dotModelsIn(cwd) }
  }
  return { home: path.join(os.homedir(), '.cache', 'docling.rs'), dotModels: false }
}

/**
 * The resolved on-disk location of each dependency: an existing `DOCLING_*`
 * environment variable wins (so a local Python export is honored), else the
 * path under the install home directory.
 */
// `DOCLING_RS_FP32` truthiness, docling_core::env::flag's rule (anything but
// empty / 0 / false / no / off).
function fp32Forced() {
  const v = (process.env.DOCLING_RS_FP32 || '').trim().toLowerCase()
  return !['', '0', 'false', 'no', 'off'].includes(v)
}

// The first existing candidate, else the last one (the fp32 default, whose
// absence the readiness check then reports).
function firstExisting(candidates) {
  return candidates.find((p) => fs.existsSync(p)) || candidates[candidates.length - 1]
}

function resolvePaths(dir) {
  const { home, dotModels } = homeDir(dir)
  const models = path.join(home, dotModels ? '.models' : 'models')
  const tf = (name) => path.join(models, 'tableformer', name)

  return {
    home,
    models,
    layout: process.env.DOCLING_LAYOUT_ONNX || path.join(models, 'layout_heron.onnx'),
    ocrRec: process.env.DOCLING_OCR_REC_ONNX || path.join(models, 'ocr_rec.onnx'),
    ocrDict: process.env.DOCLING_OCR_DICT || path.join(models, 'ppocr_keys_v1.txt'),
    // The text detector (#429) is optional: without it OCR stays region-scoped.
    ocrDet: process.env.DOCLING_OCR_DET_ONNX || path.join(models, 'ocr_det.onnx'),
    // The text-line orientation classifier (#666), optional likewise.
    ocrTextlineOri:
      process.env.DOCLING_OCR_TEXTLINE_ORI_ONNX || path.join(models, 'ocr_textline_ori.onnx'),
    // The fp16-weight encoder repack (#374; fp32 compute, half the download)
    // ranks ahead of the fp32 file, like the Rust pipeline's own chain,
    // unless full precision is forced.
    tfEncoder:
      process.env.DOCLING_TABLEFORMER_ENCODER ||
      firstExisting(fp32Forced() ? [tf('encoder.onnx')] : [tf('encoder_fp16.onnx'), tf('encoder.onnx')]),
    // The decoder follows the Rust pipeline's own preference chain
    // (`tableformer::resolved_paths`): the per-layer-cache graph (#871), the
    // KV-cache one, then the legacy graphs. Pinning `decoder.onnx`, as this
    // did, ran every Node conversion on the slowest graph, whose growing
    // per-step tensors are what #871's heap fragmentation came from.
    tfDecoder:
      process.env.DOCLING_TABLEFORMER_DECODER ||
      firstExisting(
        fp32Forced()
          ? [tf('decoder_kv_layers.onnx'), tf('decoder_kv.onnx'), tf('decoder.onnx')]
          : [
              tf('decoder_kv_int8.onnx'),
              tf('decoder_kv_layers.onnx'),
              tf('decoder_kv.onnx'),
              tf('decoder_int8.onnx'),
              tf('decoder.onnx'),
            ],
      ),
    tfBbox: process.env.DOCLING_TABLEFORMER_BBOX || path.join(models, 'tableformer', 'bbox.onnx'),
    chunkTokenizer:
      process.env.DOCLING_CHUNK_TOKENIZER || path.join(models, 'chunk', 'tokenizer.json'),
    // IBM Z (#504): the prebuilt addon has no ONNX Runtime linked in — pyke
    // ships none for s390x — and dlopens libonnxruntime.so from
    // `ORT_DYLIB_PATH`, else `<models>/onnxruntime/`, else the library search
    // path; download_dependencies.sh fetches it there on an s390x host.
    onnxRuntime: process.env.ORT_DYLIB_PATH || path.join(models, 'onnxruntime', 'libonnxruntime.so'),
  }
}

// Whether this process needs a separately installed ONNX Runtime library for
// the ML stages (the s390x addon; every other prebuilt links it in).
function needsOnnxRuntime() {
  return process.arch === 's390x'
}

/**
 * The hybrid chunker's default tokenizer (all-MiniLM-L6-v2's tokenizer.json,
 * fetched by `scripts/install/download_dependencies.sh` into `.models/chunk/`), resolved
 * through the same install-home logic as the ML models. Returns `null` when not
 * installed — the native side then reports a clear error with the download hint.
 */
function defaultChunkTokenizer(dir) {
  const p = resolvePaths(dir)
  return fs.existsSync(p.chunkTokenizer) ? p.chunkTokenizer : null
}

/**
 * Report which dependencies are present on disk. `ready` is true when the
 * minimum for PDF and image input (the layout model) is present.
 */
function checkDependencies(options = {}) {
  const p = resolvePaths(options.dir)
  const has = (f) => fs.existsSync(f)
  const status = {
    home: p.home,
    layout: has(p.layout),
    ocr: has(p.ocrRec) && has(p.ocrDict),
    ocrDet: has(p.ocrDet),
    ocrTextlineOri: has(p.ocrTextlineOri),
    tableformer: has(p.tfEncoder) && has(p.tfDecoder) && has(p.tfBbox),
    chunkTokenizer: has(p.chunkTokenizer),
    // true where the runtime is linked in; on s390x whether the library is present.
    onnxRuntime: !needsOnnxRuntime() || has(p.onnxRuntime),
  }
  status.ready = status.layout && status.onnxRuntime
  status.missing = [
    !status.layout && 'layout_heron.onnx',
    !status.onnxRuntime && 'onnxruntime/libonnxruntime.so',
  ].filter(Boolean)
  return status
}

/** Point the current process at installed assets (so the native pipeline finds them). */
function exportEnv(p) {
  if (fs.existsSync(p.layout)) process.env.DOCLING_LAYOUT_ONNX = p.layout
  if (fs.existsSync(p.ocrRec)) process.env.DOCLING_OCR_REC_ONNX = p.ocrRec
  if (fs.existsSync(p.ocrDict)) process.env.DOCLING_OCR_DICT = p.ocrDict
  if (fs.existsSync(p.ocrDet)) process.env.DOCLING_OCR_DET_ONNX = p.ocrDet
  if (fs.existsSync(p.ocrTextlineOri)) process.env.DOCLING_OCR_TEXTLINE_ORI_ONNX = p.ocrTextlineOri
  if (fs.existsSync(p.tfEncoder)) process.env.DOCLING_TABLEFORMER_ENCODER = p.tfEncoder
  if (fs.existsSync(p.tfDecoder)) process.env.DOCLING_TABLEFORMER_DECODER = p.tfDecoder
  if (fs.existsSync(p.tfBbox)) process.env.DOCLING_TABLEFORMER_BBOX = p.tfBbox
  // The native loader looks under `.models/` (CWD) and `$DOCLING_RS_MODELS_DIR`
  // only; a cache-layout home (`~/.cache/docling.rs/models/onnxruntime/`) is
  // handed over as the explicit path.
  if (needsOnnxRuntime() && !process.env.ORT_DYLIB_PATH && fs.existsSync(p.onnxRuntime)) {
    process.env.ORT_DYLIB_PATH = p.onnxRuntime
  }
}

/**
 * A copy-pasteable next step, shown when a PDF/image/METS conversion is
 * attempted without the dependencies on disk.
 */
function downloadGuide() {
  return [
    'Run this once from your app\'s directory (fetches the ONNX models —',
    'layout, OCR, TableFormer — from this repo\'s GitHub Releases straight',
    'into ./.models, which this package looks for by default; no env vars',
    'needed afterwards):',
    '',
    '  curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh',
    '',
    'or, from a checkout of the repo:',
    '',
    '  scripts/install/download_dependencies.sh',
    '',
    'On IBM Z (s390x) the same script also fetches the ONNX Runtime library',
    '(onnxruntime/libonnxruntime.so) the prebuilt addon loads at run time;',
    'ORT_DYLIB_PATH points at one installed elsewhere.',
    '',
    'TableFormer is optional (tables fall back to geometric reconstruction',
    'without it). To use your own export/host instead, point the DOCLING_*',
    'env vars at it directly: DOCLING_LAYOUT_ONNX, DOCLING_OCR_REC_ONNX,',
    'DOCLING_OCR_DICT, DOCLING_TABLEFORMER_{ENCODER,DECODER,BBOX} — see',
    'docs/MODELS_NOTICE.md for licensing.',
    '',
    'Declarative formats (md, html, docx, xlsx, …) need none of this — only',
    'PDF, image and METS conversion do. The remote VLM pipeline needs no ONNX',
    "models either (PDF pages are rendered in pure Rust) — but it is a",
    "convert* option (pipeline: 'vlm'); the chunk* functions have no VLM path",
    'and always need the models.',
  ].join('\n')
}

/**
 * Throw a clear, actionable error if `format` needs the ML pipeline but its
 * dependencies aren't installed. Called before ML conversions; also wires up
 * the `DOCLING_*` env vars for whatever is present,
 * so a checkout with `scripts/install/download_dependencies.sh` already run just works.
 *
 * `options` are the caller's convert options, read only for `pipeline` (#77):
 * under `pipeline: 'vlm'` a remote endpoint replaces the ONNX stack, so the
 * layout model is not required — PDF pages are rendered locally in pure Rust
 * before being sent, so `vlm` requires nothing on disk.
 */
function assertMlReady(format, dir, options) {
  if (!ML_FORMATS.has(format)) return
  const pipeline = options && options.pipeline
  // Validated here, not left to the native side: this guard runs first, so an
  // unrecognized name would otherwise fall through to the strict ONNX
  // requirement — telling someone who typed 'VLM' to download hundreds of
  // megabytes of models to fix a capitalization slip.
  if (pipeline != null && pipeline !== 'standard' && pipeline !== 'vlm') {
    throw new Error(`unknown pipeline '${pipeline}' (expected: standard, vlm)`)
  }
  const vlm = pipeline === 'vlm'
  // METS-GBS is an ML format with no VLM path at all, so relaxing the model
  // requirement for it would only mislead. Say what's actually wrong instead.
  if (vlm && !VLM_FORMATS.has(format)) {
    throw new Error(
      `the 'vlm' pipeline converts PDF and image inputs; '${format}' needs the standard pipeline.`,
    )
  }
  const p = resolvePaths(dir)
  exportEnv(p)
  const status = checkDependencies({ dir })
  const missing = [!vlm && !status.layout && 'layout_heron.onnx'].filter(Boolean)
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
  defaultChunkTokenizer,
}
