// Public entry point for the `fleischwolf` npm package.
//
// Wraps the native N-API binding (loaded by `native.js`, which picks the right
// prebuilt `.node` for the host platform) with two things:
//   1. dependency guards — converting a PDF/image/METS input throws a clear
//      error unless the ML models + pdfium are on disk (see
//      scripts/download_dependencies.sh);
//   2. a `streamFileMarkdown` async generator over Markdown chunks.
//
// Works in Node.js and Bun (Bun implements N-API).

'use strict'

const native = require('./native.js')
const { checkDependencies, assertMlReady } = require('./deps.js')

// Resolve the format id of an input for the ML guard. Uses the native
// extension→format map; falls back to an explicitly-passed format string.
function mlFormatOf(name, format) {
  if (format) {
    return native.formatFromName(`x.${String(format).replace(/^\./, '')}`) || String(format)
  }
  return native.formatFromName(name || '') || ''
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

  // Bridge the native (err, chunk) callback into an async generator. Chunks are
  // delivered on the event loop (via a threadsafe function); a null chunk ends
  // the stream, a non-null err ends it with a throw.
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

  converter.convertFileStreaming(
    filePath,
    (err, chunk) => {
      if (err) {
        failure = err
        done = true
      } else if (chunk === null || chunk === undefined) {
        done = true
      } else {
        queue.push(chunk)
      }
      wake()
    },
    { imageMode, artifactsDir },
  )

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

// --- exports (explicit, so ESM named imports work in Node and Bun) ----------

module.exports.convert = convert
module.exports.convertFile = convertFile
module.exports.convertAsync = convertAsync
module.exports.convertFileAsync = convertFileAsync
module.exports.DocumentConverter = DocumentConverter
module.exports.Pipeline = Pipeline
module.exports.streamFileMarkdown = streamFileMarkdown
module.exports.checkDependencies = checkDependencies
module.exports.supportedFormats = native.supportedFormats
module.exports.formatFromName = native.formatFromName
