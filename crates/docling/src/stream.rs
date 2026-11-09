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

use docling_core::{ImageMode, MarkdownExportOptions, MarkdownStreamer};

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
            //
            // A producer that *panicked* also drops its sender, and reading
            // that as a clean end of stream is how a backend bug turned into a
            // silent empty document (#396: a 200 with no body where the panic
            // should have been a 500). Report it as this stream's last item
            // instead; the panic message and backtrace have already gone to
            // stderr from the worker's own unwind.
            Err(_) => {
                let panicked = self.handle.take().is_some_and(|h| h.join().is_err());
                // Nothing more to read either way — a further `next()` ends.
                self.rx = None;
                panicked.then(|| {
                    Err(ConversionError::Panic(
                        "the conversion worker panicked (its message and backtrace are on stderr)"
                            .into(),
                    ))
                })
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
    pub no_text_panels: bool,
    pub text_layer_only: crate::TextLayerOnly,
    pub no_ocr: bool,
    pub password: Option<String>,
    pub force_full_page_ocr: bool,
    pub replace_broken_text_layer: bool,
    pub enrich: docling_pdf::EnrichmentOptions,
    pub page_range: Option<(usize, usize)>,
    pub ocr_lang: Option<docling_pdf::OcrLang>,
    pub ocr_engine: Option<docling_pdf::OcrEngine>,
    pub tesseract_lang: Option<String>,
    pub ocr_mode: Option<docling_pdf::OcrMode>,
    pub ocr_scale: Option<f32>,
    pub word_gap: Option<docling_pdf::WordGap>,
    pub keep_typography: Option<bool>,
    /// [`DocumentConverter::ocr_textline_orientation`] (#666); `None` keeps
    /// the pipeline's environment default.
    pub ocr_textline_orientation: Option<bool>,
    /// Picture-crop scale (#520); page images have no place in a stream.
    pub images_scale: Option<f32>,
    /// [`DocumentConverter::use_embedded_picture_images`] (#1007).
    pub embedded_pictures: bool,
    /// [`DocumentConverter::keep_picture_images`] (#645): off, each page
    /// batch loses its pictures' bytes before it is serialized (#896), as
    /// the buffered path's document does.
    pub keep_picture_images: bool,
    /// [`DocumentConverter::page_break_placeholder`] for the streamer — the
    /// PDF path never builds a `DoclingDocument` to carry it.
    pub page_break_placeholder: Option<String>,
    /// [`DocumentConverter::compact_tables`] for the streamer, for the same
    /// reason: the buffered PDF path sets it on the document it builds, the
    /// streaming path has no document.
    pub compact_tables: bool,
    /// [`DoclingDocument::picture_alt_text`] for the stream (#676).
    pub picture_alt_text: bool,
    /// Where the metadata goes (#679): read up front on the streaming
    /// path, written as front matter ahead of the first chunk.
    pub metadata_output: docling_core::MetadataOutput,
    /// [`DocumentConverter::document_timeout`] (#497).
    pub document_timeout: Option<std::time::Duration>,
    /// The PII pass (#621), applied to each page batch before it is
    /// serialized, so the stream matches the buffered redacted Markdown.
    pub redact: Option<docling_core::RedactionOptions>,
}

/// Spawn the background conversion and return the chunk iterator.
///
/// [`ImageMode::Referenced`] is memory-bounded (issue #80): each page's image
/// files are written under `options.artifacts_dir` as that page's chunk is
/// emitted, then dropped, instead of accumulating until export.
pub(crate) fn spawn(
    converter: DocumentConverter,
    source: SourceDocument,
    options: MarkdownExportOptions,
) -> MarkdownStream {
    let (tx, rx) = sync_channel::<Result<String, ConversionError>>(CHANNEL_DEPTH);
    let handle = std::thread::spawn(move || match source.format {
        // PDF is the format with internal page-level parallelism, so it gets the
        // true streaming path: emit each page's Markdown in order as it completes.
        // Exception: the heading-hierarchy stage (#302) rewrites levels across
        // the *whole* assembled document (and the script-position stage,
        // #1005, runs after it on the assembled items), so those conversions run buffered
        // (one chunk) — streamed output stays byte-identical to buffered.
        // A `.pdf` without the PDF header is something else saved under that
        // name: the buffered path converts it, retrying as its content's
        // format (#556). The header check reads the first KiB the PDF parser
        // reads anyway.
        // A registered backend (#680) claiming the source takes it buffered.
        InputFormat::Pdf
            if !converter.heading_hierarchy_enabled()
                && !converter.script_positions_enabled()
                && !converter.claims(&source)
                && crate::sniff::looks_like_pdf(&source.bytes) =>
        {
            // The stream's progress events (#783): the converter's
            // callbacks, as document 1, bracketing the page walk below.
            let reporter = std::sync::Arc::new(converter.reporter(1, &source.name, None));
            run_pdf(
                converter.stream_settings(),
                &source,
                &options,
                &tx,
                &reporter,
            )
        }
        // Every other backend builds the whole `DoclingDocument` synchronously, so
        // there is no latency to stream away; serialize it through the same chunk
        // API for a uniform interface (one chunk plus the trailing newline).
        _ => run_buffered(converter, source, &options, &tx),
    });
    MarkdownStream {
        rx: Some(rx),
        handle: Some(handle),
    }
}

/// The streamer for `options` (#599's layers, picture traversal, escaping,
/// placeholder — byte-identical to the buffered
/// `export_to_markdown_with_options`), with the converter's table style and
/// page-break placeholder.
fn streamer(
    strict: bool,
    compact_tables: bool,
    picture_alt_text: bool,
    page_break_placeholder: Option<String>,
    metadata: Option<&docling_core::DocumentMetadata>,
    options: &MarkdownExportOptions,
) -> MarkdownStreamer {
    let streamer = MarkdownStreamer::with_artifacts(
        strict,
        ImageMode::Placeholder,
        compact_tables,
        &options.artifacts_dir,
    )
    .with_export_options(options)
    .with_page_break_placeholder(page_break_placeholder)
    .with_picture_alt_text(picture_alt_text);
    match metadata {
        Some(m) => streamer.with_metadata(m),
        None => streamer,
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
    options: &MarkdownExportOptions,
    tx: &std::sync::mpsc::SyncSender<Result<String, ConversionError>>,
    reporter: &std::sync::Arc<crate::progress::ProgressReporter>,
) {
    reporter.document_started();
    // The PII pass (#621) on the streaming path: one `Redactor` across the
    // page batches, so pseudonym numbering follows document order exactly
    // as the buffered pass numbers it. `BoxOut` needs the OCR pipeline,
    // which is busy converting here — the images are dropped instead.
    let redact_opts = settings.redact.clone().map(|mut o| {
        if o.images == docling_core::ImageRedaction::BoxOut {
            crate::redact::warn_box_out_unavailable("the streaming path drops them");
            o.images = docling_core::ImageRedaction::Drop;
        }
        o
    });
    let detector = match redact_opts.as_ref().map(crate::redact::detector) {
        Some(Ok(d)) => Some(d),
        Some(Err(e)) => {
            let _ = tx.send(Err(e));
            return;
        }
        None => None,
    };
    let mut redactor = match (redact_opts.as_ref(), detector.as_ref()) {
        (Some(o), Some(d)) => Some(docling_core::redact::Redactor::new(o, d)),
        _ => None,
    };
    // The file's metadata (#679) when an export shows it: redacted with the
    // rest, written as front matter ahead of the first page.
    let mut metadata = settings
        .metadata_output
        .in_json()
        .then(|| crate::metadata::read(source, settings.password.as_deref()));
    if let (Some(m), Some(r)) = (metadata.as_mut(), redactor.as_mut()) {
        r.redact_metadata(m);
    }
    // Tables use the padded GitHub serializer unless the converter asked for
    // compact ones — the same knob the buffered PDF path carries on its
    // document (`--compact-tables` used to reach only `--no-stream`).
    let mut streamer = streamer(
        settings.strict,
        settings.compact_tables,
        settings.picture_alt_text,
        settings.page_break_placeholder.clone(),
        metadata
            .as_ref()
            .filter(|_| settings.metadata_output.front_matter()),
        options,
    );
    let mut pipeline = match docling_pdf::Pipeline::new().map(|p| {
        let p = match settings.ocr_textline_orientation {
            Some(enable) => p.ocr_textline_orientation(enable),
            None => p,
        };
        p.no_table_former(settings.no_table_former)
            .no_text_panels(settings.no_text_panels)
            // docling-pdf's pre-2.0 names (see `DocumentConverter::ml_pipeline`).
            .no_ocr(settings.text_layer_only == crate::TextLayerOnly::On)
            .text_layer_auto(settings.text_layer_only == crate::TextLayerOnly::Auto)
            .skip_ocr(settings.no_ocr)
            .force_full_page_ocr(settings.force_full_page_ocr)
            .replace_broken_text_layer(settings.replace_broken_text_layer)
            .ocr_lang(settings.ocr_lang)
            .ocr_engine(settings.ocr_engine)
            .tesseract_lang(settings.tesseract_lang.clone())
            .ocr_mode(settings.ocr_mode)
            .ocr_scale(settings.ocr_scale)
            .word_gap(settings.word_gap)
            .keep_typography(settings.keep_typography)
            .images_scale(settings.images_scale)
            .use_embedded_picture_images(settings.embedded_pictures)
            .enrichments(settings.enrich)
            .pages(settings.page_range)
            .document_timeout(settings.document_timeout)
    }) {
        Ok(p) => p,
        Err(e) => {
            reporter.document_completed(crate::ConversionStatus::Failure);
            let _ = tx.send(Err(ConversionError::Parse(e.to_string())));
            return;
        }
    };
    pipeline.set_progress(reporter.page_hook());

    let result = pipeline.convert_streaming_outcome(
        &source.bytes,
        settings.password.as_deref(),
        &source.name,
        |mut nodes, mut links| {
            if let Some(r) = redactor.as_mut() {
                r.redact_nodes(&mut nodes);
                r.redact_links(&mut links);
            }
            if !settings.keep_picture_images {
                crate::converter::strip_picture_images_in(&mut nodes);
            }
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
        },
    );

    match result {
        Ok(completion) => {
            let tail = streamer.finish();
            if !tail.is_empty() {
                let _ = tx.send(Ok(tail));
            }
            // The pages the budget left out are failed pages (#783).
            if let docling_pdf::Completion::TimedOut { pages_selected, .. } = &completion {
                reporter.fail_pages_cut_off(
                    settings.page_range.map_or(1, |(first, _)| first),
                    *pages_selected,
                );
            }
            // A spent document budget (#497) is the stream's last item, after
            // every chunk of the partial document: the consumer has the pages
            // that fit and learns the document was cut — docling's
            // PARTIAL_SUCCESS, which a chunk stream has no status field for.
            if let Some(message) = completion.message() {
                reporter.document_completed(crate::ConversionStatus::PartialSuccess);
                let _ = tx.send(Err(ConversionError::Timeout(message)));
            } else {
                reporter.document_completed(crate::ConversionStatus::Success);
            }
        }
        // A consumer-drop abort and a real parse error both end here; the send below
        // is a no-op if the consumer is already gone, so only genuine errors surface.
        // The `PdfError` stays on the chain (same text: it names `pdf:` itself), so
        // an encrypted PDF is typed on the streaming path too (#636).
        Err(e) => {
            reporter.document_completed(crate::ConversionStatus::Failure);
            let _ = tx.send(Err(ConversionError::with_source("pdf", e)));
        }
    }
}

fn run_buffered(
    converter: DocumentConverter,
    source: SourceDocument,
    options: &MarkdownExportOptions,
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
    let mut streamer = streamer(
        settings.strict,
        doc.compact_tables,
        settings.picture_alt_text,
        doc.page_break_placeholder.clone(),
        doc.metadata_output.front_matter().then_some(&doc.metadata),
        options,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #395/#396: a producer that panics drops its sender exactly like one that
    /// finished, so reading the closed channel as a clean end of stream turned a
    /// backend bug into a silent empty document (an HTTP 200 with no body). The
    /// stream reports it as its own last item instead. The worker's panic
    /// message still goes to stderr — that is the noise this test prints.
    #[test]
    fn a_panicking_producer_ends_the_stream_with_an_error() {
        let (tx, rx) = sync_channel::<Result<String, ConversionError>>(CHANNEL_DEPTH);
        let handle = std::thread::spawn(move || {
            tx.send(Ok("a chunk".into())).unwrap();
            panic!("a backend bug");
        });
        let mut stream = MarkdownStream {
            rx: Some(rx),
            handle: Some(handle),
        };
        assert_eq!(stream.next().unwrap().unwrap(), "a chunk");
        let err = stream
            .next()
            .expect("the panic is reported, not swallowed")
            .expect_err("as an error");
        assert!(matches!(err, ConversionError::Panic(_)), "got {err}");
        assert!(
            stream.next().is_none(),
            "the stream ends after reporting the panic"
        );
    }

    /// The ordinary end of a stream stays a plain `None`.
    #[test]
    fn a_finished_producer_just_ends() {
        let (tx, rx) = sync_channel::<Result<String, ConversionError>>(CHANNEL_DEPTH);
        let handle = std::thread::spawn(move || {
            tx.send(Ok("only chunk".into())).unwrap();
        });
        let mut stream = MarkdownStream {
            rx: Some(rx),
            handle: Some(handle),
        };
        assert_eq!(stream.next().unwrap().unwrap(), "only chunk");
        assert!(stream.next().is_none());
    }
}
