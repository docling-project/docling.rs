// Public entry point for the `docling.rs` npm package.
//
// Wraps the native N-API binding (loaded by `native.js`, which picks the right
// prebuilt `.node` for the host platform) with two things:
//   1. dependency guards — converting a PDF/image/METS input throws a clear
//      error unless the ML models + pdfium are on disk (see
//      scripts/install/download_dependencies.sh);
//   2. `streamFileMarkdown` async generators over Markdown chunks (module-level
//      and on the warm `Pipeline`).
//
// Works in Node.js and Bun (Bun implements N-API).

'use strict'

const native = require('./native.js')
const { checkDependencies, assertMlReady, defaultChunkTokenizer } = require('./deps.js')

// Resolve the format id of an input for the ML guard. Uses the native
// extension→format map; falls back to an explicitly-passed format string.
function mlFormatOf(name, format) {
  if (format) {
    return native.formatFromName(`x.${String(format).replace(/^\./, '')}`) || String(format)
  }
  return native.formatFromName(name || '') || ''
}

// Bridge a native (err, chunk) streaming callback into an async generator.
// `start` receives the callback and kicks off the native conversion. Chunks
// are delivered on the event loop (via a threadsafe function); a null chunk
// ends the stream, a non-null err ends it with a throw.
async function* chunkStream(start) {
  const queue = []
  let done = false
  let failure = null
  let notify = null
  const wake = () => {
    if (notify) {
      const n = notify
      notify = null
      n()
    }
  }

  start((err, chunk) => {
    if (err) {
      failure = err
      done = true
    } else if (chunk === null || chunk === undefined) {
      done = true
    } else {
      queue.push(chunk)
    }
    wake()
  })

  while (true) {
    if (queue.length > 0) {
      yield queue.shift()
      continue
    }
    if (failure) throw failure
    if (done) return
    await new Promise((resolve) => {
      notify = resolve
    })
  }
}

// --- guarded one-shot functions --------------------------------------------

function convertFile(path, options) {
  assertMlReady(mlFormatOf(path))
  return native.convertFile(path, options)
}

function convert(input, options) {
  assertMlReady(mlFormatOf(input && input.name, input && input.format))
  return native.convert(input, options)
}

// async so a guard failure surfaces as a rejected promise, not a sync throw.
async function convertFileAsync(path, options) {
  assertMlReady(mlFormatOf(path))
  return native.convertFileAsync(path, options)
}

async function convertAsync(input, options) {
  assertMlReady(mlFormatOf(input && input.name, input && input.format))
  return native.convertAsync(input, options)
}

// --- guarded chunking functions ---------------------------------------------

// For the hybrid chunker with no explicit tokenizer, resolve the default one
// (models/chunk/tokenizer.json) through the same install-home logic as the ML
// models — so DOCLING_RS_HOME / ~/.cache installs work, not only ./models. The
// native side keeps its own CWD-relative fallback as a backstop.
function withDefaultTokenizer(options) {
  if (!options || options.tokenizer) return options
  if (String(options.chunker || '').toLowerCase() !== 'hybrid') return options
  const tokenizer = defaultChunkTokenizer()
  return tokenizer ? { ...options, tokenizer } : options
}

function chunkFile(path, options) {
  assertMlReady(mlFormatOf(path))
  return native.chunkFile(path, withDefaultTokenizer(options))
}

function chunk(input, options) {
  assertMlReady(mlFormatOf(input && input.name, input && input.format))
  return native.chunk(input, withDefaultTokenizer(options))
}

// async so a guard failure surfaces as a rejected promise, not a sync throw.
async function chunkFileAsync(path, options) {
  assertMlReady(mlFormatOf(path))
  return native.chunkFileAsync(path, withDefaultTokenizer(options))
}

async function chunkAsync(input, options) {
  assertMlReady(mlFormatOf(input && input.name, input && input.format))
  return native.chunkAsync(input, withDefaultTokenizer(options))
}

function chunkDocument(documentJson, options) {
  return native.chunkDocument(documentJson, withDefaultTokenizer(options))
}

async function chunkDocumentAsync(documentJson, options) {
  return native.chunkDocumentAsync(documentJson, withDefaultTokenizer(options))
}

// --- guarded classes --------------------------------------------------------

class DocumentConverter {
  constructor(options) {
    this._inner = new native.DocumentConverter(options)
  }

  convertFile(path, options) {
    assertMlReady(mlFormatOf(path))
    return this._inner.convertFile(path, options)
  }

  convert(input, options) {
    assertMlReady(mlFormatOf(input && input.name, input && input.format))
    return this._inner.convert(input, options)
  }

  async convertFileAsync(path, options) {
    assertMlReady(mlFormatOf(path))
    return this._inner.convertFileAsync(path, options)
  }

  async convertAsync(input, options) {
    assertMlReady(mlFormatOf(input && input.name, input && input.format))
    return this._inner.convertAsync(input, options)
  }

  convertFileStreaming(path, callback, options) {
    assertMlReady(mlFormatOf(path))
    return this._inner.convertFileStreaming(path, callback, options)
  }
}

// The warm PDF/image pipeline is inherently ML — always guarded.
class Pipeline {
  constructor(options) {
    this._inner = new native.Pipeline(options)
  }

  convertFile(path, options) {
    assertMlReady(mlFormatOf(path))
    return this._inner.convertFile(path, options)
  }

  convert(input, options) {
    assertMlReady(mlFormatOf(input && input.name, input && input.format))
    return this._inner.convert(input, options)
  }

  // async so a guard failure surfaces as a rejected promise, not a sync throw.
  async convertFileAsync(path, options) {
    assertMlReady(mlFormatOf(path))
    return this._inner.convertFileAsync(path, options)
  }

  async convertAsync(input, options) {
    assertMlReady(mlFormatOf(input && input.name, input && input.format))
    return this._inner.convertAsync(input, options)
  }

  convertFileStreaming(path, callback, options) {
    assertMlReady(mlFormatOf(path))
    return this._inner.convertFileStreaming(path, callback, options)
  }

  /**
   * Stream a PDF's Markdown in chunks through the warm pipeline, in document
   * order, as pages finish converting (an image arrives as a single chunk).
   * Same contract as the module-level `streamFileMarkdown`, but reusing this
   * instance's loaded models — no per-call model reload.
   *
   * @param {string} filePath
   * @param {object} [options] output options (`imageMode`: `placeholder` or
   *   `embedded`; `referenced` is rejected)
   * @returns {AsyncGenerator<string, void, unknown>}
   */
  async *streamFileMarkdown(filePath, options = {}) {
    assertMlReady(mlFormatOf(filePath))
    const { imageMode, artifactsDir } = options
    yield* chunkStream((callback) =>
      this._inner.convertFileStreaming(filePath, callback, { imageMode, artifactsDir }),
    )
  }
}

// --- streaming --------------------------------------------------------------

/**
 * Stream a file's Markdown in chunks, in document order, as conversion
 * progresses — the win for PDF, whose pages convert in parallel.
 *
 * Yields each Markdown chunk; concatenating every chunk reproduces the buffered
 * `convertFile(path).content` byte-for-byte.
 *
 * @param {string} filePath
 * @param {object} [options]
 * @returns {AsyncGenerator<string, void, unknown>}
 */
async function* streamFileMarkdown(filePath, options = {}) {
  assertMlReady(mlFormatOf(filePath))
  const { strict, fetchImages, allowedFormats, imageMode, artifactsDir } = options
  const converter = new native.DocumentConverter({ strict, fetchImages, allowedFormats })
  yield* chunkStream((callback) =>
    converter.convertFileStreaming(filePath, callback, { imageMode, artifactsDir }),
  )
}

/**
 * Stream a file's chunks as the chunkers produce them — the streaming
 * counterpart of `chunkFile`. The first chunk is ready (e.g. for embedding)
 * while the rest of the document is still being chunked, and no all-chunks
 * array is materialized. Abandoning the generator early (`break`) cancels the
 * background chunking.
 *
 * @param {string} filePath
 * @param {object} [options] same `ChunkOptions` as `chunkFile`
 * @returns {AsyncGenerator<Chunk, void, unknown>}
 */
async function* streamFileChunks(filePath, options = {}) {
  assertMlReady(mlFormatOf(filePath))
  const opts = withDefaultTokenizer(options)
  yield* chunkStream((callback) => native.chunkFileStreaming(filePath, callback, opts))
}

/**
 * Streaming counterpart of `chunk`: chunk in-memory bytes, yielding each chunk
 * as it is produced (same contract as {@link streamFileChunks}).
 *
 * @param {object} input same `ConvertInput` as `chunk`
 * @param {object} [options] same `ChunkOptions` as `chunk`
 * @returns {AsyncGenerator<Chunk, void, unknown>}
 */
async function* streamChunks(input, options = {}) {
  assertMlReady(mlFormatOf(input && input.name, input && input.format))
  const opts = withDefaultTokenizer(options)
  yield* chunkStream((callback) => native.chunkStreaming(input, callback, opts))
}

/**
 * Streaming counterpart of `chunkDocument`: chunk an already-converted
 * docling-core JSON document, yielding each chunk as it is produced (same
 * contract as {@link streamFileChunks}). Touches no ML models — unguarded.
 *
 * @param {string} documentJson
 * @param {object} [options] same `ChunkOptions` as `chunkDocument`
 * @returns {AsyncGenerator<Chunk, void, unknown>}
 */
async function* streamDocumentChunks(documentJson, options = {}) {
  const opts = withDefaultTokenizer(options)
  yield* chunkStream((callback) => native.chunkDocumentStreaming(documentJson, callback, opts))
}

// --- exports (explicit, so ESM named imports work in Node and Bun) ----------

module.exports.convert = convert
module.exports.convertFile = convertFile
module.exports.convertAsync = convertAsync
module.exports.convertFileAsync = convertFileAsync
module.exports.chunk = chunk
module.exports.chunkFile = chunkFile
module.exports.chunkAsync = chunkAsync
module.exports.chunkFileAsync = chunkFileAsync
// Chunking an already-converted JSON document touches no ML models — unguarded.
module.exports.chunkDocument = chunkDocument
module.exports.chunkDocumentAsync = chunkDocumentAsync
module.exports.DocumentConverter = DocumentConverter
module.exports.Pipeline = Pipeline
module.exports.streamFileMarkdown = streamFileMarkdown
module.exports.streamFileChunks = streamFileChunks
module.exports.streamChunks = streamChunks
module.exports.streamDocumentChunks = streamDocumentChunks
module.exports.checkDependencies = checkDependencies
module.exports.supportedFormats = native.supportedFormats
module.exports.formatFromName = native.formatFromName
