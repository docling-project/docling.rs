//! Error type for conversion.

use std::fmt;

use crate::format::InputFormat;

/// Anything that can go wrong while loading or converting a source document.
#[derive(Debug)]
pub enum ConversionError {
    /// Reading the input from disk failed.
    Io(std::io::Error),
    /// The file extension (or content) did not map to a known format.
    UnknownFormat { hint: String },
    /// The format is known but no backend is wired up for it yet.
    UnsupportedFormat(InputFormat),
    /// The backend recognized the format but failed to parse the content.
    Parse(String),
    /// The requested streaming conversion is not supported (e.g. JSON, or the
    /// referenced image mode, which both need the whole document up front).
    Streaming(String),
    /// The headless-browser pre-render (`--use-web-browser`) failed, or the crate
    /// was built without the `web-browser` feature.
    Browser(String),
    /// A dependency failed during conversion. Unlike [`ConversionError::Parse`]
    /// the underlying error is kept alive (not flattened into a string), so
    /// callers can walk [`std::error::Error::source`] and downcast to the
    /// original type — e.g. to tell a truncated archive from malformed XML.
    WithSource {
        /// What was being converted when the failure happened (backend prefix,
        /// mirroring the `Parse` message style: "xlsx", "docling-json", …).
        context: String,
        /// The error that caused the failure, preserved on the chain.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl ConversionError {
    /// Wrap a dependency error, keeping it reachable via
    /// [`std::error::Error::source`] instead of stringifying it.
    pub fn with_source(
        context: impl Into<String>,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        ConversionError::WithSource {
            context: context.into(),
            source: source.into(),
        }
    }
}

impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConversionError::Io(e) => write!(f, "i/o error: {e}"),
            ConversionError::UnknownFormat { hint } => {
                write!(f, "could not determine input format (hint: {hint})")
            }
            ConversionError::UnsupportedFormat(fmt) => {
                write!(
                    f,
                    "no backend implemented yet for format '{}'",
                    fmt.as_str()
                )
            }
            ConversionError::Parse(msg) => write!(f, "parse error: {msg}"),
            ConversionError::Streaming(msg) => write!(f, "streaming not supported: {msg}"),
            ConversionError::Browser(msg) => write!(f, "web-browser render error: {msg}"),
            ConversionError::WithSource { context, source } => {
                write!(f, "parse error: {context}: {source}")
            }
        }
    }
}

impl std::error::Error for ConversionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConversionError::Io(e) => Some(e),
            ConversionError::WithSource { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConversionError {
    fn from(e: std::io::Error) -> Self {
        ConversionError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn with_source_keeps_the_cause_on_the_chain() {
        let cause = serde_json::from_str::<i32>("boom").unwrap_err();
        let err = ConversionError::with_source("docling-json", cause);

        assert!(err.to_string().starts_with("parse error: docling-json: "));
        let source = err.source().expect("source is chained");
        assert!(
            source.downcast_ref::<serde_json::Error>().is_some(),
            "chained source downcasts to the original type"
        );
    }

    #[test]
    fn stringly_variants_have_no_source() {
        assert!(ConversionError::Parse("x".into()).source().is_none());
    }
}
