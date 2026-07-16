// Public type surface for the `docling.rs` npm package. Re-exports the native
// binding's option/result types and unguarded functions, and declares the
// JS-wrapped classes, the dependency API, and the streaming helper.

import type {
  ConverterOptions,
  OutputOptions,
  ConvertOptions,
  ConvertInput,
  ConvertResult,
  ChunkOptions,
  Chunk,
} from './native'

export type {
  ConverterOptions,
  OutputOptions,
  ConvertOptions,
  ConvertInput,
  ConvertResult,
  ChunkOptions,
  Chunk,
}

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

/**
 * Chunk a file with docling's chunkers: convert it, then run the hierarchical
 * (default) or hybrid (`chunker: 'hybrid'` + `tokenizer`) chunker over the
 * document. Throws for PDF/image/METS if deps aren't installed.
 */
export declare function chunkFile(path: string, options?: ChunkOptions | null): Array<Chunk>
/** Async (Promise) {@link chunkFile}; conversion + chunking run off the event loop. */
export declare function chunkFileAsync(path: string, options?: ChunkOptions | null): Promise<Array<Chunk>>
/** Chunk in-memory bytes (same input contract as {@link convert}). */
export declare function chunk(input: ConvertInput, options?: ChunkOptions | null): Array<Chunk>
/** Async (Promise) {@link chunk}. */
export declare function chunkAsync(input: ConvertInput, options?: ChunkOptions | null): Promise<Array<Chunk>>
/**
 * Chunk an already-converted document, passed as docling-core JSON (the
 * `content` of a `convert*` call with `to: 'json'`) — so a document converted
 * once (e.g. through the warm PDF {@link Pipeline}) chunks without re-converting.
 */
export declare function chunkDocument(documentJson: string, options?: ChunkOptions | null): Array<Chunk>
/** Async (Promise) {@link chunkDocument}. */
export declare function chunkDocumentAsync(documentJson: string, options?: ChunkOptions | null): Promise<Array<Chunk>>

/** A reusable converter holding config (strict / fetchImages / allowedFormats). */
export declare class DocumentConverter {
  constructor(options?: ConverterOptions | null)
  convertFile(path: string, options?: OutputOptions | null): ConvertResult
  convert(input: ConvertInput, options?: OutputOptions | null): ConvertResult
  convertFileAsync(path: string, options?: OutputOptions | null): Promise<ConvertResult>
  convertAsync(input: ConvertInput, options?: OutputOptions | null): Promise<ConvertResult>
  convertFileStreaming(path: string, callback: StreamCallback, options?: OutputOptions | null): void
}

/** Output options for {@link Pipeline.streamFileMarkdown} (streamable modes only). */
export interface PipelineStreamOptions {
  imageMode?: 'placeholder' | 'embedded'
  artifactsDir?: string
}

/**
 * A reusable PDF/image pipeline that keeps the ONNX models loaded across calls.
 * Use instead of the per-call functions when converting many PDFs/images — the
 * one-shot path reloads every model each call. Handles `pdf` and `image` inputs.
 *
 * The `*Async` variants run the conversion off the event loop; overlapping
 * calls on one instance queue (the models are mutable sessions), so batch
 * throughput comes from keeping the models warm, not from parallel calls.
 */
export declare class Pipeline {
  constructor(options?: ConverterOptions | null)
  convertFile(path: string, options?: OutputOptions | null): ConvertResult
  convert(input: ConvertInput, options?: OutputOptions | null): ConvertResult
  /** Async (Promise) file conversion on the warm pipeline, off the event loop. */
  convertFileAsync(path: string, options?: OutputOptions | null): Promise<ConvertResult>
  /** Async (Promise) bytes conversion on the warm pipeline, off the event loop. */
  convertAsync(input: ConvertInput, options?: OutputOptions | null): Promise<ConvertResult>
  /** Callback-form streaming (prefer {@link Pipeline.streamFileMarkdown}). */
  convertFileStreaming(path: string, callback: StreamCallback, options?: OutputOptions | null): void
  /**
   * Stream a PDF's Markdown in chunks through the warm pipeline, in document
   * order, as pages finish converting (an image arrives as a single chunk).
   * Concatenating the chunks reproduces the buffered Markdown byte-for-byte.
   */
  streamFileMarkdown(
    filePath: string,
    options?: PipelineStreamOptions,
  ): AsyncGenerator<string, void, unknown>
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
  /** Hybrid-chunker tokenizer (models/chunk/tokenizer.json) present. */
  chunkTokenizer: boolean
  /** True when the minimum for PDF (pdfium + layout) is present. */
  ready: boolean
  /** Human-readable list of the missing required assets. */
  missing: string[]
}

/**
 * Report which PDF/image dependencies are present on disk. Fetch them with
 * `scripts/install/download_dependencies.sh` (see the package README) — this function
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

/**
 * Stream a file's chunks as the chunkers produce them — the streaming
 * counterpart of {@link chunkFile}. The first chunk is ready (e.g. for
 * embedding) while the rest of the document is still being chunked, and no
 * all-chunks array is materialized. Abandoning the generator early (`break`)
 * cancels the background chunking. Throws for PDF/image/METS if deps aren't
 * installed.
 */
export declare function streamFileChunks(
  filePath: string,
  options?: ChunkOptions | null,
): AsyncGenerator<Chunk, void, unknown>

/** Streaming counterpart of {@link chunk} (same contract as {@link streamFileChunks}). */
export declare function streamChunks(
  input: ConvertInput,
  options?: ChunkOptions | null,
): AsyncGenerator<Chunk, void, unknown>

/**
 * Streaming counterpart of {@link chunkDocument}: chunk an already-converted
 * docling-core JSON document (same contract as {@link streamFileChunks}).
 * Touches no ML models.
 */
export declare function streamDocumentChunks(
  documentJson: string,
  options?: ChunkOptions | null,
): AsyncGenerator<Chunk, void, unknown>
