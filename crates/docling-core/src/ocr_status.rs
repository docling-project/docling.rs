//! Whether a conversion could OCR (#1210) — the typed fact the ML pipeline
//! hands its result next to the document. A missing OCR model degrades
//! instead of failing (#244), so without this the only trace of it was a
//! once-per-process stderr warning: a scan that came back empty looked
//! exactly like a document that needed no OCR. It lives here, in the crate
//! every surface's result type already depends on, so the converter's
//! `ConversionResult` holds it whatever PDF features are compiled in.

use std::fmt;

use serde::Serialize;

/// The OCR engine's state for one conversion that asked for it — a scanned
/// page, an image input, a picture or region with no text layer. A
/// conversion that never needed OCR carries none (`None` on the result).
///
/// Serialized (serve, Node, Python) as an object tagged by `status`:
/// `{"status": "available"}`, `{"status": "unavailable", "reason": "…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OcrStatus {
    /// The engine loaded and read what the conversion handed it.
    Available,
    /// The engine could not be loaded — the model files are not installed,
    /// the `tesseract` binary is missing, … — and the conversion continued
    /// without OCR: text it would have read (scanned pages, text inside
    /// images) is missing from the document.
    Unavailable {
        /// Why the load failed, as the engine reported it.
        reason: String,
    },
}

impl OcrStatus {
    /// `"available"` / `"unavailable"` — the tag every surface reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    /// Whether the conversion wanted OCR and went without it.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// Fold another request's state into this conversion's: one request
    /// that went without OCR makes the conversion's status unavailable (the
    /// first reason kept) — a document is only as readable as its worst
    /// page.
    pub fn merge(self, other: OcrStatus) -> OcrStatus {
        match (self, other) {
            (Self::Unavailable { reason }, _) | (_, Self::Unavailable { reason }) => {
                Self::Unavailable { reason }
            }
            _ => Self::Available,
        }
    }

    /// [`merge`](Self::merge) over optional states: `None` (no OCR asked
    /// for) leaves the other side as it is.
    pub fn merge_opt(a: Option<OcrStatus>, b: Option<OcrStatus>) -> Option<OcrStatus> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.merge(b)),
            (a, b) => a.or(b),
        }
    }
}

impl fmt::Display for OcrStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Available => f.write_str("available"),
            Self::Unavailable { reason } => write!(f, "unavailable ({reason})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_wins_a_merge_and_keeps_the_first_reason() {
        let a = OcrStatus::Unavailable { reason: "a".into() };
        let b = OcrStatus::Unavailable { reason: "b".into() };
        assert_eq!(OcrStatus::Available.merge(a.clone()), a);
        assert_eq!(a.clone().merge(OcrStatus::Available), a);
        assert_eq!(a.clone().merge(b), a);
        assert_eq!(
            OcrStatus::merge_opt(None, Some(OcrStatus::Available)),
            Some(OcrStatus::Available)
        );
        assert_eq!(OcrStatus::merge_opt(None, None), None);
    }

    #[test]
    fn serializes_tagged_by_status() {
        assert_eq!(
            serde_json::to_string(&OcrStatus::Available).unwrap(),
            r#"{"status":"available"}"#
        );
        assert_eq!(
            serde_json::to_string(&OcrStatus::Unavailable {
                reason: "no model".into()
            })
            .unwrap(),
            r#"{"status":"unavailable","reason":"no model"}"#
        );
    }
}
