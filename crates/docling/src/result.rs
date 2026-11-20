//! Conversion result types.

use docling_core::DoclingDocument;

use crate::format::InputFormat;

/// Outcome status of a conversion, mirroring
/// `docling.datamodel.base_models.ConversionStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionStatus {
    Success,
    PartialSuccess,
    Failure,
}

impl ConversionStatus {
    /// docling's spelling: `success`, `partial_success`, `failure` — what
    /// the bindings, serve and the progress events (#783) report.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::PartialSuccess => "partial_success",
            Self::Failure => "failure",
        }
    }
}

/// One recorded problem of a conversion that still produced a document —
/// docling's `ErrorItem` (`component_type`, `module_name`, `error_message`).
/// A result with any of these is a [`ConversionStatus::PartialSuccess`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorItem {
    /// docling's `DocItemLabel`-free component tag: `"document_backend"`,
    /// `"model"`, `"doc_assembler"`, `"user_input"`, `"pipeline"`.
    pub component_type: String,
    /// The stage that recorded it (`"pipeline"` for the document budget,
    /// `"VlmPipeline"` for a VLM page).
    pub module_name: String,
    pub error_message: String,
    /// docling's `page_no`: the 1-based page the problem belongs to, `None`
    /// for a document-scoped one.
    pub page_no: Option<usize>,
}

impl ErrorItem {
    /// The document budget ran out (docling's `document_timeout`, #497):
    /// the pipeline stopped between pages and the document holds the pages
    /// processed.
    pub fn timeout(message: impl Into<String>) -> Self {
        ErrorItem {
            component_type: "document_backend".into(),
            module_name: "pipeline".into(),
            error_message: message.into(),
            page_no: None,
        }
    }

    /// A VLM page that came back with no or cut-off output (#762, #763):
    /// docling's `VlmPipeline._determine_status` records it under the
    /// `pipeline` component with the page number.
    pub fn vlm_page(page_no: usize, message: impl Into<String>) -> Self {
        ErrorItem {
            component_type: "pipeline".into(),
            module_name: "VlmPipeline".into(),
            error_message: message.into(),
            page_no: Some(page_no),
        }
    }

    /// What a reader ran into and read around (#912:
    /// [`docling_core::Notice`]) — a limit it hit, named with its effective
    /// value — as the `document_backend` component's error.
    pub fn notice(notice: &docling_core::Notice) -> Self {
        let (module, page_no) = match notice.kind {
            docling_core::NoticeKind::DepthLimit { variable, .. } => (variable, None),
            // docling's backend error for a page it could not parse.
            docling_core::NoticeKind::PageSkipped { page_no } => ("page", Some(page_no)),
        };
        ErrorItem {
            component_type: "document_backend".into(),
            module_name: module.into(),
            error_message: notice.message.clone(),
            page_no,
        }
    }

    /// The message with its page in front, the way the CLI warns about it.
    pub fn describe(&self) -> String {
        match self.page_no {
            Some(page) => format!("page {page}: {}", self.error_message),
            None => self.error_message.clone(),
        }
    }
}

/// The result of converting one [`crate::SourceDocument`].
///
/// Mirrors `docling.datamodel.document.ConversionResult`. The converted
/// [`DoclingDocument`] is exposed directly as `document`, matching the target
/// API (`result.document.export_to_markdown()`); `errors` is non-empty
/// exactly when `status` is [`ConversionStatus::PartialSuccess`] — a spent
/// document budget, or a VLM page that failed or came back cut off.
#[derive(Debug, Clone)]
pub struct ConversionResult {
    pub document: DoclingDocument,
    pub status: ConversionStatus,
    pub input_name: String,
    pub format: InputFormat,
    pub errors: Vec<ErrorItem>,
    /// What the PII redaction pass removed (#621), when
    /// [`DocumentConverter::redact_pii`](crate::DocumentConverter::redact_pii)
    /// ran: counts per label, and the mapping only when asked for.
    pub redaction: Option<docling_core::RedactionReport>,
    /// What the PDF reader repaired to read the input (#934), when
    /// [`DocumentConverter::pdf_report_repairs`](crate::DocumentConverter::pdf_report_repairs)
    /// asked: empty for a well-formed file. `None` when not asked, for
    /// non-PDF input and on the VLM pipeline.
    pub pdf_repairs: Option<Vec<docling_core::PdfRepair>>,
    /// Whether the conversion could OCR what it needed to (#1210): `None`
    /// when nothing asked for OCR (a born-digital PDF, an Office file
    /// without picture OCR, OCR switched off), `Available` when the engine
    /// read it, `Unavailable` — with the load error — when the OCR model
    /// could not be loaded and the conversion went on without it (#244):
    /// the status stays `Success`, but scanned pages and text inside images
    /// are missing from the document. The stderr warning says the same
    /// once per process; this says it on every conversion it applies to.
    pub ocr_status: Option<docling_core::OcrStatus>,
}
