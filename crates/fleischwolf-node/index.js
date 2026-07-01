// Public entry point for the `fleischwolf` npm package.
//
// Re-exports the native N-API binding (loaded by `native.js`, which picks the
// right prebuilt `.node` for the host platform) and adds one pure-JS
// convenience: `streamFileMarkdown`, an async generator over a document's
// Markdown chunks, wrapping the native callback-based streaming API.
//
// Works in Node.js and Bun (Bun implements N-API).

'use strict'

const native = require('./native.js')

const { DocumentConverter } = native

/**
 * Stream a file's Markdown in chunks, in document order, as conversion
 * progresses — the win for PDF, whose pages convert in parallel.
 *
 * Yields each Markdown chunk as a string; concatenating every chunk reproduces
 * the buffered `convertFile(path).content` byte-for-byte. Converter options
 * (`strict`, `fetchImages`, `allowedFormats`) and the streamable `imageMode`
 * (`placeholder` or `embedded`; `referenced` is not streamable) are all
 * accepted in a single options object.
 *
 * @param {string} filePath
 * @param {object} [options]
 * @returns {AsyncGenerator<string, void, unknown>}
 */
async function* streamFileMarkdown(filePath, options = {}) {
  const { strict, fetchImages, allowedFormats, imageMode, artifactsDir } = options
  const converter = new DocumentConverter({ strict, fetchImages, allowedFormats })

  // Bridge the native (err, chunk) callback into an async generator. The native
  // side delivers chunks on the event loop (via a threadsafe function) and
  // signals end-of-stream with a null chunk; a non-null err ends with a throw.
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

// Explicit re-exports (not a spread) so Node's CJS lexer can detect them and
// allow `import { convert } from 'fleischwolf'` from ESM, in Node and Bun alike.
module.exports.convert = native.convert
module.exports.convertFile = native.convertFile
module.exports.convertAsync = native.convertAsync
module.exports.convertFileAsync = native.convertFileAsync
module.exports.supportedFormats = native.supportedFormats
module.exports.formatFromName = native.formatFromName
module.exports.DocumentConverter = native.DocumentConverter
module.exports.streamFileMarkdown = streamFileMarkdown
