//! Streaming Markdown conversion.
//!
//! [`DocumentConverter::convert_streaming`] produces Markdown in chunks as the
//! document is converted, rather than building the whole string up front. The
//! headline win is PDF: the ML pipeline processes pages in parallel, and this
//! emits each page's Markdown in document order *as it finishes*, so output starts
//! flowing before the last page is done — while staying byte-identical to the
//! buffered `result.document.export_to_markdown()`.
//!
//! Streaming is Markdown-only: JSON serializes docling-core's reference-based tree
//! and needs every node present, so there is nothing to emit incrementally.
//!
//! [`crate::DocumentConverter::convert_streaming`]: crate::DocumentConverter

use std::sync::mpsc::{sync_channel, Receiver};
use std::thread::JoinHandle;

use docling_core::{ImageMode, MarkdownStreamer};

use crate::converter::DocumentConverter;
use crate::error::ConversionError;
use crate::format::InputFormat;
use crate::source::SourceDocument;

/// Bounded chunk buffer: the producer blocks once this many chunks are unread, so
/// a slow consumer throttles the conversion (and, for PDF, the whole page
/// pipeline) instead of letting Markdown pile up in memory.
const CHANNEL_DEPTH: usize = 8;

/// An iterator over a document's Markdown, yielded in document order as
/// conversion progresses. Each item is a chunk to write as-is; concatenating
/// every `Ok` chunk reproduces the buffered Markdown byte-for-byte.
///
/// A conversion error surfaces as a single `Err` item, after which the iterator
/// ends. Dropping the stream early cancels the background conversion.
pub struct MarkdownStream {
    /// `None` only transiently inside `Drop`, to disconnect before joining.
    rx: Option<Receiver<Result<String, ConversionError>>>,
    handle: Option<JoinHandle<()>>,
}

impl Iterator for MarkdownStream {
    type Item = Result<String, ConversionError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.rx.as_ref()?.recv() {
            Ok(item) => Some(item),
            // Producer finished and dropped its sender: join it and end.
            Err(_) => {
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
                None
            }
        }
    }
}

impl Drop for MarkdownStream {
    fn drop(&mut self) {
        // Disconnect first so a producer blocked on a full channel sees its send
        // fail and unwinds, then wait for it so no detached thread keeps working.
        self.rx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The producer-side settings a stream needs off its converter, snapshotted by
/// [`DocumentConverter::stream_settings`].
pub(crate) struct StreamSettings {
    pub strict: bool,
    pub no_table_former: bool,
    pub no_ocr: bool,
    pub enrich: docling_pdf::EnrichmentOptions,
    pub page_range: Option<(usize, usize)>,
    pub ocr_lang: Option<docling_pdf::OcrLang>,
    pub artifacts_dir: String,
}

/// Spawn the background conversion and return the chunk iterator.
///
/// [`ImageMode::Referenced`] is memory-bounded (issue #80): each page's image
/// files are written under the converter's artifacts dir as that page's chunk
/// is emitted, then dropped, instead of accumulating until export.
pub(crate) fn spawn(
    converter: DocumentConverter,
    source: SourceDocument,
    image_mode: ImageMode,
) -> MarkdownStream {
    let (tx, rx) = sync_channel::<Result<String, ConversionError>>(CHANNEL_DEPTH);
    let handle = std::thread::spawn(move || match source.format {
        // PDF is the format with internal page-level parallelism, so it gets the
        // true streaming path: emit each page's Markdown in order as it completes.
        InputFormat::Pdf => run_pdf(converter.stream_settings(), &source, image_mode, &tx),
        // Every other backend builds the whole `DoclingDocument` synchronously, so
        // there is no latency to stream away; serialize it through the same chunk
        // API for a uniform interface (one chunk plus the trailing newline).
        _ => run_buffered(converter, source, image_mode, &tx),
    });
    MarkdownStream {
        rx: Some(rx),
        handle: Some(handle),
    }
}

/// Write one push's referenced-mode images (paths are relative to the process
/// working directory, prefixed with the artifacts dir). No-op on an empty list,
/// so non-referenced modes never touch the filesystem.
fn write_artifacts(artifacts: Vec<(String, Vec<u8>)>) -> Result<(), ConversionError> {
    for (rel, bytes) in artifacts {
        let path = std::path::Path::new(&rel);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConversionError::Streaming(format!("creating {parent:?}: {e}")))?;
        }
        std::fs::write(path, bytes)
            .map_err(|e| ConversionError::Streaming(format!("writing image {rel}: {e}")))?;
    }
    Ok(())
}

fn run_pdf(
    settings: StreamSettings,
    source: &SourceDocument,
    image_mode: ImageMode,
    tx: &std::sync::mpsc::SyncSender<Result<String, ConversionError>>,
) {
    // The PDF pipeline builds its document from `DoclingDocument::new` defaults, so
    // tables use the padded GitHub serializer (compact_tables = false), matching the
    // buffered PDF path.
    let mut streamer = MarkdownStreamer::with_artifacts(
        settings.strict,
        image_mode,
        false,
        &settings.artifacts_dir,
    );
    let mut pipeline = match docling_pdf::Pipeline::new().map(|p| {
        p.no_table_former(settings.no_table_former)
            .no_ocr(settings.no_ocr)
            .ocr_lang(settings.ocr_lang)
            .enrichments(settings.enrich)
            .pages(settings.page_range)
    }) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Err(ConversionError::Parse(e.to_string())));
            return;
        }
    };

    let result = pipeline.convert_streaming(&source.bytes, None, &source.name, |nodes, links| {
        let chunk = streamer.push(&nodes, &links);
        // Referenced mode: this push's images hit the disk as its Markdown is
        // emitted, keeping ~one page batch of image bytes resident. A write
        // failure aborts the pipeline and surfaces through the outer error arm.
        if let Err(e) = write_artifacts(streamer.take_artifacts()) {
            return Err(docling_pdf::PdfError::Pdfium(e.to_string()));
        }
        if !chunk.is_empty() && tx.send(Ok(chunk)).is_err() {
            // Consumer dropped the stream: abort the pipeline.
            return Err(docling_pdf::PdfError::Pdfium(
                "markdown stream consumer dropped".into(),
            ));
        }
        Ok(())
    });

    match result {
        Ok(()) => {
            let tail = streamer.finish();
            if !tail.is_empty() {
                let _ = tx.send(Ok(tail));
            }
        }
        // A consumer-drop abort and a real parse error both end here; the send below
        // is a no-op if the consumer is already gone, so only genuine errors surface.
        Err(e) => {
            let _ = tx.send(Err(ConversionError::Parse(e.to_string())));
        }
    }
}

fn run_buffered(
    converter: DocumentConverter,
    source: SourceDocument,
    image_mode: ImageMode,
    tx: &std::sync::mpsc::SyncSender<Result<String, ConversionError>>,
) {
    let settings = converter.stream_settings();
    let doc = match converter.convert(source) {
        Ok(result) => result.document,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let mut streamer = MarkdownStreamer::with_artifacts(
        settings.strict,
        image_mode,
        doc.compact_tables,
        &settings.artifacts_dir,
    );
    let chunk = streamer.push(&doc.nodes, &doc.links);
    if let Err(e) = write_artifacts(streamer.take_artifacts()) {
        let _ = tx.send(Err(e));
        return;
    }
    if !chunk.is_empty() && tx.send(Ok(chunk)).is_err() {
        return;
    }
    let tail = streamer.finish();
    if !tail.is_empty() {
        let _ = tx.send(Ok(tail));
    }
}
