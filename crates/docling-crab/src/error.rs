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
        }
    }
}

impl std::error::Error for ConversionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConversionError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConversionError {
    fn from(e: std::io::Error) -> Self {
        ConversionError::Io(e)
    }
}
