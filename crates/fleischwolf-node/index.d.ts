// Public type surface for the `fleischwolf` npm package: everything the native
// binding exports, plus the `streamFileMarkdown` async-generator helper.

export * from './native'

/** Options for {@link streamFileMarkdown} (converter config + streamable output). */
export interface StreamOptions {
  /** Cleaner, more conformant Markdown instead of docling-legacy output. */
  strict?: boolean
  /** Fetch and embed external `<img src>` for HTML/EPUB (network for http(s)). */
  fetchImages?: boolean
  /** Restrict to these format ids/extensions; anything else is rejected. */
  allowedFormats?: Array<string>
  /** Streamable picture handling: `"placeholder"` (default) or `"embedded"`. */
  imageMode?: 'placeholder' | 'embedded'
  /** Directory name used in referenced image links (unused while streaming). */
  artifactsDir?: string
}

/**
 * Stream a file's Markdown in chunks, in document order, as conversion
 * progresses — the win for PDF, whose pages convert in parallel.
 *
 * Yields each Markdown chunk; concatenating every chunk reproduces the buffered
 * `convertFile(path).content` byte-for-byte.
 *
 * @example
 * for await (const chunk of streamFileMarkdown('paper.pdf')) {
 *   process.stdout.write(chunk)
 * }
 */
export function streamFileMarkdown(
  filePath: string,
  options?: StreamOptions,
): AsyncGenerator<string, void, unknown>
