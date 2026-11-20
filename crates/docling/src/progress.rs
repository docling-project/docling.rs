//! Conversion progress events (#783) — docling's `docling.datamodel.progress`
//! (docling#4560, 2.139).
//!
//! A [`ProgressCallback`] on the converter
//! ([`DocumentConverter::progress_callback`](crate::DocumentConverter::progress_callback))
//! receives every conversion's events, one on a call
//! ([`convert_with_progress`](crate::DocumentConverter::convert_with_progress))
//! that call's; [`show_progress`](crate::DocumentConverter::show_progress)
//! attaches the built-in [`ProgressPrinter`], which is what the CLI's
//! `--progress` prints. Events of one document arrive from the threads that
//! convert it — the page events of a parallel PDF walk from its workers — so
//! a callback must be `Send + Sync` and cheap; several documents converting
//! at once report concurrently, told apart by their `document_index`.
//!
//! The sequence for one document is docling's: [`ProgressKind::DocumentStarted`],
//! then a [`ProgressKind::PageCompleted`] per page of a PDF / image / METS
//! conversion (`completed_pages` counts up to `total_pages`, the pages a
//! `page_range` selected; a page the `document_timeout` cut off is reported
//! with `success: false`, so the count still reaches the total), then a
//! [`ProgressKind::EnrichmentProgress`] per batch of the picture passes the
//! converter runs on the assembled document, and last
//! [`ProgressKind::DocumentCompleted`] with the result's status — a failed
//! conversion ends with [`ConversionStatus::Failure`]. Declarative formats
//! (Office, HTML, …) have no pages to report: they emit the start and the
//! completion only. docling's `phase_started` events are not ported: the
//! pipeline here has no separate build / assemble / enrich phases to time.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::result::ConversionStatus;

/// What a [`ProgressEvent`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressKind {
    /// Emitted once per input document, first.
    DocumentStarted,
    /// One page finished — PDF, image and METS conversions only. Pages of a
    /// parallel walk finish out of order, so `completed_pages`, not
    /// `page_no`, drives a progress bar; `total_pages` counts the pages the
    /// page window selected. `success: false` marks a page the document
    /// budget cut off, reported once the walk ended so the count reaches
    /// the total.
    PageCompleted {
        /// The page's 1-based number in the source document.
        page_no: usize,
        success: bool,
        completed_pages: usize,
        total_pages: usize,
    },
    /// Item-level progress of one enrichment pass on the assembled document
    /// (the picture description and picture OCR passes): `0` of the total
    /// first, then after every item, ending at `total_items`. Match on
    /// `step`; `label` is for display and may change.
    EnrichmentProgress {
        step: EnrichmentStep,
        label: &'static str,
        completed_items: usize,
        total_items: usize,
    },
    /// Emitted once as the document's last event, also when it failed.
    DocumentCompleted { status: ConversionStatus },
}

impl ProgressKind {
    /// docling's `kind` discriminator: `document_started`, `page_completed`,
    /// `enrichment_progress`, `document_completed`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::DocumentStarted => "document_started",
            Self::PageCompleted { .. } => "page_completed",
            Self::EnrichmentProgress { .. } => "enrichment_progress",
            Self::DocumentCompleted { .. } => "document_completed",
        }
    }
}

/// Stable identifier of an enrichment pass, safe to match on — docling's
/// `EnrichmentStep`. The passes the converter runs on the assembled
/// document report here: picture description (the VLM pass, #670) and
/// picture OCR (#645, a docling.rs extension whose text lands as the
/// picture's description); the per-page enrichments (code/formula, picture
/// classification) run inside the page walk and are covered by its page
/// events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EnrichmentStep {
    CodeFormula,
    PictureClassification,
    PictureDescription,
    ChartExtraction,
    Other,
}

impl EnrichmentStep {
    /// docling's spelling: `code_formula`, `picture_classification`, …
    pub fn name(&self) -> &'static str {
        match self {
            Self::CodeFormula => "code_formula",
            Self::PictureClassification => "picture_classification",
            Self::PictureDescription => "picture_description",
            Self::ChartExtraction => "chart_extraction",
            Self::Other => "other",
        }
    }
}

/// One progress event: which document it is about and what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    /// The document's 1-based position in the batch it converts in —
    /// always 1 for [`DocumentConverter::convert`](crate::DocumentConverter::convert),
    /// what the caller passed to
    /// [`convert_with_progress`](crate::DocumentConverter::convert_with_progress)
    /// (the CLI batch numbers its files). Tells apart documents with the
    /// same name and documents converting concurrently.
    pub document_index: usize,
    /// The source's name.
    pub document_name: String,
    pub kind: ProgressKind,
}

impl ProgressEvent {
    /// The event as docling's flat JSON object: `kind`, `document_index`,
    /// `document_name` and the kind's own fields (`page_no`, `success`,
    /// `completed_pages`, `total_pages`; `step`, `label`, `completed_items`,
    /// `total_items`; `status`) — what the bindings and the FFI hand out.
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "kind": self.kind.name(),
            "document_index": self.document_index,
            "document_name": self.document_name,
        });
        match &self.kind {
            ProgressKind::DocumentStarted => {}
            ProgressKind::PageCompleted {
                page_no,
                success,
                completed_pages,
                total_pages,
            } => {
                v["page_no"] = (*page_no).into();
                v["success"] = (*success).into();
                v["completed_pages"] = (*completed_pages).into();
                v["total_pages"] = (*total_pages).into();
            }
            ProgressKind::EnrichmentProgress {
                step,
                label,
                completed_items,
                total_items,
            } => {
                v["step"] = step.name().into();
                v["label"] = (*label).into();
                v["completed_items"] = (*completed_items).into();
                v["total_items"] = (*total_items).into();
            }
            ProgressKind::DocumentCompleted { status } => {
                v["status"] = status.as_str().into();
            }
        }
        v
    }
}

/// A progress callback: called with every event of the conversions it is
/// attached to, from the thread that produced the event.
pub type ProgressCallback = Arc<dyn Fn(&ProgressEvent) + Send + Sync>;

/// The callbacks a converter carries (#783): cloned with it, shown as a
/// count.
#[derive(Clone, Default)]
pub(crate) struct Callbacks(pub(crate) Vec<ProgressCallback>);

impl fmt::Debug for Callbacks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} callback(s)", self.0.len())
    }
}

/// The per-document sink between a conversion and the callbacks —
/// docling's `ProgressReporter`. Every surface that drives the PDF pipeline
/// itself (serve's warm pipeline, the CLI batch, the bindings) makes one per
/// document, hands [`page_hook`](Self::page_hook) to
/// `Pipeline::set_progress`, and brackets the conversion with
/// [`document_started`](Self::document_started) /
/// [`document_completed`](Self::document_completed).
///
/// Page numbers are de-duplicated, so a page reports once whichever walk
/// reports it, and `completed_pages` is the count of distinct pages seen.
/// A callback that panics is caught and warned about: a misbehaving
/// observer must not break the conversion.
pub struct ProgressReporter {
    callbacks: Vec<ProgressCallback>,
    document_index: usize,
    document_name: String,
    finished: Mutex<BTreeSet<usize>>,
    /// What the conversion's OCR requests met (#1210), folded in by every
    /// pipeline the conversion leases and by the picture-OCR pass; the
    /// result's `ocr_status` is read off it. The reporter is the one value
    /// every stage of one conversion already holds, so the fact rides it
    /// instead of a new parameter through each backend.
    ocr_status: Mutex<Option<docling_core::OcrStatus>>,
}

impl fmt::Debug for ProgressReporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressReporter")
            .field("document_index", &self.document_index)
            .field("document_name", &self.document_name)
            .field("callbacks", &self.callbacks.len())
            .finish()
    }
}

impl ProgressReporter {
    /// A reporter for document `document_index` named `document_name`,
    /// feeding `callbacks`. With no callbacks every method is a no-op and
    /// [`page_hook`](Self::page_hook) is `None`, so an unobserved
    /// conversion costs nothing.
    pub fn new(
        callbacks: Vec<ProgressCallback>,
        document_index: usize,
        document_name: impl Into<String>,
    ) -> Self {
        Self {
            callbacks,
            document_index,
            document_name: document_name.into(),
            finished: Mutex::new(BTreeSet::new()),
            ocr_status: Mutex::new(None),
        }
    }

    /// Fold what a stage's OCR requests met into the conversion's status
    /// (#1210, see [`docling_core::OcrStatus::merge`]); `None` — the stage
    /// asked for no OCR — leaves it as it is.
    pub fn note_ocr_status(&self, status: Option<docling_core::OcrStatus>) {
        if status.is_none() {
            return;
        }
        let mut slot = self.ocr_status.lock().unwrap_or_else(|p| p.into_inner());
        *slot = docling_core::OcrStatus::merge_opt(slot.take(), status);
    }

    /// The conversion's OCR status so far, cleared (#1210): `None` when
    /// nothing asked for OCR.
    pub fn take_ocr_status(&self) -> Option<docling_core::OcrStatus> {
        self.ocr_status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// Whether anyone listens.
    pub fn enabled(&self) -> bool {
        !self.callbacks.is_empty()
    }

    pub fn document_index(&self) -> usize {
        self.document_index
    }

    pub fn document_name(&self) -> &str {
        &self.document_name
    }

    fn emit(&self, kind: ProgressKind) {
        if self.callbacks.is_empty() {
            return;
        }
        let event = ProgressEvent {
            document_index: self.document_index,
            document_name: self.document_name.clone(),
            kind,
        };
        for callback in &self.callbacks {
            // docling logs and swallows a callback's exception; a panic in
            // a page-walk worker would otherwise take the conversion down.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                callback(&event);
            }));
            if outcome.is_err() {
                eprintln!(
                    "docling: warning: a progress callback panicked on {} of {}; ignored",
                    event.kind.name(),
                    event.document_name
                );
            }
        }
    }

    pub fn document_started(&self) {
        self.emit(ProgressKind::DocumentStarted);
    }

    /// Page `page_no` (1-based) finished, `success` or not. A page number
    /// below 1 or already reported is ignored.
    pub fn page_completed(&self, page_no: usize, total_pages: usize, success: bool) {
        if !self.enabled() || page_no < 1 {
            return;
        }
        let completed_pages = {
            let mut finished = self.finished.lock().unwrap_or_else(|p| p.into_inner());
            if !finished.insert(page_no) {
                return;
            }
            finished.len()
        };
        self.emit(ProgressKind::PageCompleted {
            page_no,
            success,
            completed_pages,
            total_pages,
        });
    }

    /// Report every page of `pages` that never completed — the pages a
    /// spent document budget left out — as failed, in page order, so the
    /// completed count still reaches `total_pages`.
    pub fn fail_unfinished_pages(
        &self,
        pages: impl IntoIterator<Item = usize>,
        total_pages: usize,
    ) {
        if !self.enabled() {
            return;
        }
        let mut pages: Vec<usize> = pages.into_iter().collect();
        pages.sort_unstable();
        for page_no in pages {
            self.page_completed(page_no, total_pages, false);
        }
    }

    /// `completed_items` of `total_items` of enrichment `step` done.
    pub fn enrichment_progress(
        &self,
        step: EnrichmentStep,
        label: &'static str,
        completed_items: usize,
        total_items: usize,
    ) {
        self.emit(ProgressKind::EnrichmentProgress {
            step,
            label,
            completed_items,
            total_items,
        });
    }

    pub fn document_completed(&self, status: ConversionStatus) {
        self.emit(ProgressKind::DocumentCompleted { status });
    }

    /// The hook for `docling_pdf::Pipeline::set_progress`: every page the
    /// walk finishes becomes a successful [`ProgressKind::PageCompleted`].
    /// `None` when nobody listens, so the pipeline skips the call.
    #[cfg(feature = "pdf")]
    pub fn page_hook(self: &Arc<Self>) -> Option<docling_pdf::PageProgressHook> {
        if !self.enabled() {
            return None;
        }
        let reporter = Arc::clone(self);
        Some(Arc::new(move |p: docling_pdf::PageProgress| {
            reporter.page_completed(p.page_no, p.pages_selected, true);
        }))
    }

    /// After a PDF walk ended by the budget: every selected page the walk
    /// never reported is failed. `first_page` is the window's first page
    /// (1 without a window), `pages_selected` the walk's count.
    #[cfg(feature = "pdf")]
    pub fn fail_pages_cut_off(&self, first_page: usize, pages_selected: usize) {
        self.fail_unfinished_pages(first_page..first_page + pages_selected, pages_selected);
    }
}

/// The pages / enrichment bar one document currently shows.
struct Bar {
    document_index: usize,
    /// What the bar counts: `"pages"`, or an enrichment step's label.
    label: &'static str,
    done: usize,
    total: usize,
    /// Whether the terminal line currently holds this bar.
    drawn: bool,
    last_draw: Instant,
}

/// Ready-made [`ProgressCallback`] that prints to stderr — docling's
/// `ProgressPrinter`, what `DocumentConverter::show_progress` and the CLI's
/// `--progress` attach.
///
/// One line per document (`[3/12] Converting report.pdf`), a bar for its
/// pages and one for each enrichment pass that has work
/// (`  pages 4/9 [████████░░░░░░░░░░░░]`), then `Finished report.pdf:
/// success`. On a terminal the bar redraws in place, in docling orange
/// unless `NO_COLOR` is set; elsewhere (a log file, a pipe) each bar is
/// written once, as a plain `done/total` count, when it closes. Documents
/// converting at once share the terminal line: the bar shows the document
/// that last made progress, labelled with its index.
pub struct ProgressPrinter {
    total_documents: Option<usize>,
    /// The `Converting` / `Finished` lines; off for a caller that logs each
    /// document itself (the CLI batch) and wants the bars alone.
    document_lines: bool,
    terminal: bool,
    color: bool,
    bar: Mutex<Option<Bar>>,
}

impl fmt::Debug for ProgressPrinter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProgressPrinter")
            .field("total_documents", &self.total_documents)
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl Default for ProgressPrinter {
    fn default() -> Self {
        Self::new()
    }
}

/// The shortest time between two in-place redraws of a bar: a 1000-page
/// document must not spend its time on the terminal.
const REDRAW_EVERY: Duration = Duration::from_millis(50);
const BAR_WIDTH: usize = 20;

impl ProgressPrinter {
    /// A printer for stderr, bars drawn in place when stderr is a terminal.
    pub fn new() -> Self {
        Self {
            total_documents: None,
            document_lines: true,
            terminal: Self::is_terminal(),
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
            bar: Mutex::new(None),
        }
    }

    /// Print `[3/12]` instead of `[3]`: the number of documents in the batch.
    pub fn with_total_documents(mut self, total: usize) -> Self {
        self.total_documents = Some(total);
        self
    }

    /// Whether to print the `Converting …` / `Finished …` lines (default
    /// yes); off, only the bars show — for a caller that logs each
    /// document itself.
    pub fn with_document_lines(mut self, enable: bool) -> Self {
        self.document_lines = enable;
        self
    }

    /// Whether stderr is a terminal — the CLI's default for `--progress`.
    pub fn is_terminal() -> bool {
        std::io::stderr().is_terminal()
    }

    /// This printer as a [`ProgressCallback`].
    pub fn callback(self: Arc<Self>) -> ProgressCallback {
        Arc::new(move |event| self.handle(event))
    }

    /// Print `event`.
    pub fn handle(&self, event: &ProgressEvent) {
        let mut bar = self.bar.lock().unwrap_or_else(|p| p.into_inner());
        match &event.kind {
            ProgressKind::DocumentStarted if !self.document_lines => {}
            ProgressKind::DocumentStarted => {
                let position = match self.total_documents {
                    Some(total) => format!("{}/{total}", event.document_index),
                    None => event.document_index.to_string(),
                };
                self.write_line(
                    &mut bar,
                    &format!("[{position}] Converting {}", event.document_name),
                );
            }
            ProgressKind::PageCompleted {
                completed_pages,
                total_pages,
                ..
            } => self.advance(
                &mut bar,
                event.document_index,
                "pages",
                *completed_pages,
                *total_pages,
            ),
            ProgressKind::EnrichmentProgress {
                label,
                completed_items,
                total_items,
                ..
            } => self.advance(
                &mut bar,
                event.document_index,
                label,
                *completed_items,
                *total_items,
            ),
            ProgressKind::DocumentCompleted { status } => {
                self.close(&mut bar, Some(event.document_index));
                if self.document_lines {
                    self.write_line(
                        &mut bar,
                        &format!("Finished {}: {}", event.document_name, status.as_str()),
                    );
                }
            }
        }
    }

    /// A line of its own, above a bar being drawn: the bar line is cleared,
    /// the text written, the bar drawn again below it.
    fn write_line(&self, bar: &mut Option<Bar>, text: &str) {
        let mut err = std::io::stderr().lock();
        let redraw = bar.as_ref().is_some_and(|b| b.drawn);
        if redraw {
            let _ = write!(err, "\r\x1b[2K");
        }
        let _ = writeln!(err, "{text}");
        if redraw {
            if let Some(b) = bar.as_mut() {
                let _ = err.write_all(self.render(b).as_bytes());
                b.last_draw = Instant::now();
            }
        }
        let _ = err.flush();
    }

    fn advance(
        &self,
        bar: &mut Option<Bar>,
        document_index: usize,
        label: &'static str,
        done: usize,
        total: usize,
    ) {
        let same = bar
            .as_ref()
            .is_some_and(|b| b.document_index == document_index && b.label == label);
        if !same {
            self.close(bar, None);
            *bar = Some(Bar {
                document_index,
                label,
                done,
                total,
                drawn: false,
                last_draw: Instant::now() - REDRAW_EVERY,
            });
        }
        let Some(b) = bar.as_mut() else { return };
        b.done = done;
        b.total = total;
        // Off a terminal the count is written once, when the bar closes.
        if !self.terminal {
            return;
        }
        let finished = done >= total;
        if !finished && b.drawn && b.last_draw.elapsed() < REDRAW_EVERY {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K");
        let _ = err.write_all(self.render(b).as_bytes());
        let _ = err.flush();
        b.drawn = true;
        b.last_draw = Instant::now();
    }

    /// Close the bar: the one of `document_index`, or whichever is shown.
    fn close(&self, bar: &mut Option<Bar>, document_index: Option<usize>) {
        let owned = bar
            .as_ref()
            .is_some_and(|b| document_index.is_none_or(|i| b.document_index == i));
        if !owned {
            return;
        }
        let Some(b) = bar.take() else { return };
        let mut err = std::io::stderr().lock();
        if self.terminal {
            // The finished bar stays, as its own line.
            let _ = writeln!(err, "\r\x1b[2K{}", self.render(&b));
        } else {
            let _ = writeln!(
                err,
                "  {}{} {}/{}",
                self.prefix(b.document_index),
                b.label,
                b.done,
                b.total
            );
        }
        let _ = err.flush();
    }

    /// `[3] ` when documents are told apart by index (a batch), nothing for
    /// a single document.
    fn prefix(&self, document_index: usize) -> String {
        match self.total_documents {
            Some(total) if total > 1 => format!("[{document_index}] "),
            _ => String::new(),
        }
    }

    /// The bar's terminal line, without a line break.
    fn render(&self, b: &Bar) -> String {
        // An empty total (nothing to do) shows a full bar.
        let filled = (b.done.min(b.total) * BAR_WIDTH)
            .checked_div(b.total)
            .unwrap_or(BAR_WIDTH);
        let (on, off) = if self.color {
            ("\x1b[38;5;208m", "\x1b[0m")
        } else {
            ("", "")
        };
        format!(
            "  {}{} {}/{} [{on}{}{off}{}]",
            self.prefix(b.document_index),
            b.label,
            b.done,
            b.total,
            "█".repeat(filled),
            "░".repeat(BAR_WIDTH - filled),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect() -> (ProgressCallback, Arc<Mutex<Vec<ProgressEvent>>>) {
        let seen: Arc<Mutex<Vec<ProgressEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let cb: ProgressCallback = Arc::new(move |e| sink.lock().unwrap().push(e.clone()));
        (cb, seen)
    }

    fn pages(events: &[ProgressEvent]) -> Vec<(usize, bool, usize, usize)> {
        events
            .iter()
            .filter_map(|e| match e.kind {
                ProgressKind::PageCompleted {
                    page_no,
                    success,
                    completed_pages,
                    total_pages,
                } => Some((page_no, success, completed_pages, total_pages)),
                _ => None,
            })
            .collect()
    }

    /// docling's reporter contract: pages de-duplicate, `completed_pages`
    /// counts distinct pages, the cut-off pages are failed in order and the
    /// count reaches the total.
    #[test]
    fn pages_dedupe_and_cut_off_pages_fail_in_order() {
        let (cb, seen) = collect();
        let r = ProgressReporter::new(vec![cb], 2, "x.pdf");
        r.document_started();
        r.page_completed(3, 4, true);
        r.page_completed(3, 4, true); // a second report of page 3: ignored
        r.page_completed(0, 4, true); // below 1: ignored
        r.page_completed(2, 4, true);
        r.fail_unfinished_pages([5, 4], 4);
        r.document_completed(ConversionStatus::PartialSuccess);
        let events = seen.lock().unwrap().clone();
        assert!(events
            .iter()
            .all(|e| e.document_index == 2 && e.document_name == "x.pdf"));
        assert_eq!(
            events.first().map(|e| e.kind.name()),
            Some("document_started")
        );
        assert_eq!(
            pages(&events),
            vec![
                (3, true, 1, 4),
                (2, true, 2, 4),
                (4, false, 3, 4),
                (5, false, 4, 4)
            ]
        );
        assert_eq!(
            events.last().map(|e| e.kind.clone()),
            Some(ProgressKind::DocumentCompleted {
                status: ConversionStatus::PartialSuccess
            })
        );
    }

    /// Without a listener nothing is built; with one, a panicking callback
    /// is contained and the later callbacks still run.
    #[test]
    fn silent_without_listeners_and_resilient_to_a_panic() {
        let quiet = ProgressReporter::new(Vec::new(), 1, "a");
        assert!(!quiet.enabled());
        quiet.page_completed(1, 1, true);
        assert!(quiet.finished.lock().unwrap().is_empty());

        let (cb, seen) = collect();
        let bad: ProgressCallback = Arc::new(|_| panic!("observer bug"));
        let r = ProgressReporter::new(vec![bad, cb], 1, "a");
        r.document_started();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn json_shape_is_doclings() {
        let e = ProgressEvent {
            document_index: 1,
            document_name: "a.pdf".into(),
            kind: ProgressKind::PageCompleted {
                page_no: 2,
                success: true,
                completed_pages: 1,
                total_pages: 3,
            },
        };
        assert_eq!(
            e.to_json(),
            serde_json::json!({
                "kind": "page_completed", "document_index": 1, "document_name": "a.pdf",
                "page_no": 2, "success": true, "completed_pages": 1, "total_pages": 3
            })
        );
        let e = ProgressEvent {
            document_index: 1,
            document_name: "a.pdf".into(),
            kind: ProgressKind::EnrichmentProgress {
                step: EnrichmentStep::PictureDescription,
                label: "picture description",
                completed_items: 0,
                total_items: 2,
            },
        };
        assert_eq!(e.to_json()["step"], "picture_description");
        let e = ProgressEvent {
            document_index: 1,
            document_name: "a.pdf".into(),
            kind: ProgressKind::DocumentCompleted {
                status: ConversionStatus::Failure,
            },
        };
        assert_eq!(e.to_json()["status"], "failure");
    }

    /// The printer's bar line and its plain (non-terminal) count.
    #[test]
    fn printer_renders_a_bar() {
        let p = ProgressPrinter {
            total_documents: Some(3),
            document_lines: true,
            terminal: false,
            color: false,
            bar: Mutex::new(None),
        };
        let b = Bar {
            document_index: 2,
            label: "pages",
            done: 5,
            total: 10,
            drawn: false,
            last_draw: Instant::now(),
        };
        assert_eq!(p.render(&b), "  [2] pages 5/10 [██████████░░░░░░░░░░]");
        let single = ProgressPrinter {
            total_documents: None,
            ..p
        };
        assert_eq!(single.render(&b), "  pages 5/10 [██████████░░░░░░░░░░]");
    }
}
