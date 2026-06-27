//! Conversion result types.

use docling_crab_core::DoclingDocument;

use crate::format::InputFormat;

/// Outcome status of a conversion, mirroring
/// `docling.datamodel.base_models.ConversionStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionStatus {
    Success,
    PartialSuccess,
    Failure,
}

/// The result of converting one [`crate::SourceDocument`].
///
/// Mirrors `docling.datamodel.document.ConversionResult`. The converted
/// [`DoclingDocument`] is exposed directly as `document`, matching the target
/// API (`result.document.export_to_markdown()`).
#[derive(Debug, Clone)]
pub struct ConversionResult {
    pub document: DoclingDocument,
    pub status: ConversionStatus,
    pub input_name: String,
    pub format: InputFormat,
}
