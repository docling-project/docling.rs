// Public type surface for the `docling.rs` npm package. Re-exports the native
// binding's option/result types and unguarded functions, and declares the
// JS-wrapped classes, the dependency API, and the streaming helper.

import type {
  ConverterOptions,
  OutputOptions,
  ConvertOptions,
  ConvertInput,
  ConvertResult,
} from './native'

export type { ConverterOptions, OutputOptions, ConvertOptions, ConvertInput, ConvertResult }

// Format helpers pass straight through from the native binding.
export { supportedFormats, formatFromName } from './native'

/** Callback form used by the native streaming API (prefer {@link streamFileMarkdown}). */
export type StreamCallback = (err: Error | null, chunk: string | undefined | null) => void

/** Convert a file on disk (format detected from the extension). Throws for PDF/image/METS if deps aren't installed. */
export declare function convertFile(path: string, options?: ConvertOptions | null): ConvertResult
/** Convert in-memory bytes. Throws for PDF/image/METS if deps aren't installed. */
export declare function convert(input: ConvertInput, options?: ConvertOptions | null): ConvertResult
/** Async (Promise) file conversion, off the event loop. Rejects for PDF/image/METS if deps aren't installed. */
export declare function convertFileAsync(path: string, options?: ConvertOptions | null): Promise<ConvertResult>
/** Async (Promise) bytes conversion, off the event loop. */
export declare function convertAsync(input: ConvertInput, options?: ConvertOptions | null): Promise<ConvertResult>

/** A reusable converter holding config (strict / fetchImages / allowedFormats). */
export declare class DocumentConverter {
  constructor(options?: ConverterOptions | null)
  convertFile(path: string, options?: OutputOptions | null): ConvertResult
  convert(input: ConvertInput, options?: OutputOptions | null): ConvertResult
  convertFileAsync(path: string, options?: OutputOptions | null): Promise<ConvertResult>
  convertAsync(input: ConvertInput, options?: OutputOptions | null): Promise<ConvertResult>
  convertFileStreaming(path: string, callback: StreamCallback, options?: OutputOptions | null): void
}

/**
 * A reusable PDF/image pipeline that keeps the ONNX models loaded across calls.
 * Use instead of the per-call functions when converting many PDFs/images — the
 * one-shot path reloads every model each call. Handles `pdf` and `image` inputs.
 */
export declare class Pipeline {
  constructor(options?: ConverterOptions | null)
  convertFile(path: string, options?: OutputOptions | null): ConvertResult
  convert(input: ConvertInput, options?: OutputOptions | null): ConvertResult
}

// --- dependency provisioning (PDF/image ML pipeline) -----------------------

/** Where installed dependencies live and which are present. */
export interface DependencyStatus {
  /** Install home directory. */
  home: string
  /** libpdfium present. */
  pdfium: boolean
  /** Layout model (layout_heron.onnx) present. */
  layout: boolean
  /** OCR model + dictionary present. */
  ocr: boolean
  /** TableFormer encoder/decoder/bbox present. */
  tableformer: boolean
  /** True when the minimum for PDF (pdfium + layout) is present. */
  ready: boolean
  /** Human-readable list of the missing required assets. */
  missing: string[]
}

/**
 * Report which PDF/image dependencies are present on disk. Fetch them with
 * `scripts/download_dependencies.sh` (see the package README) — this function
 * only reports status, it does not download anything.
 */
export declare function checkDependencies(options?: { dir?: string }): DependencyStatus

// --- streaming --------------------------------------------------------------

/** Options for {@link streamFileMarkdown} (converter config + streamable output). */
export interface StreamOptions {
  strict?: boolean
  fetchImages?: boolean
  allowedFormats?: string[]
  imageMode?: 'placeholder' | 'embedded'
  artifactsDir?: string
}

/**
 * Stream a file's Markdown in chunks, in document order, as conversion
 * progresses — the win for PDF, whose pages convert in parallel. Concatenating
 * the chunks reproduces the buffered `convertFile(path).content` byte-for-byte.
 */
export declare function streamFileMarkdown(
  filePath: string,
  options?: StreamOptions,
): AsyncGenerator<string, void, unknown>
