//! Input document loading.
//!
//! The Rust analogue of `docling.datamodel.document.InputDocument`. A
//! `SourceDocument` holds the raw bytes plus a resolved [`InputFormat`]; it is
//! what you hand to [`crate::DocumentConverter::convert`].

use std::path::{Path, PathBuf};

use crate::error::ConversionError;
use crate::format::InputFormat;

/// A loaded input document: its name, detected format, and raw bytes.
#[derive(Debug, Clone)]
pub struct SourceDocument {
    pub name: String,
    pub format: InputFormat,
    pub bytes: Vec<u8>,
    /// The filesystem path it was loaded from, if any (`from_file`). Used to
    /// resolve relative `<img src>` paths when image fetching is enabled; `None`
    /// for in-memory sources.
    pub path: Option<PathBuf>,
}

impl SourceDocument {
    /// Load a document from a filesystem path, detecting the format from the
    /// extension.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConversionError> {
        let path = path.as_ref();
        let ext = path.extension().and_then(|e| e.to_str()).ok_or_else(|| {
            ConversionError::UnknownFormat {
                hint: format!("no extension on {}", path.display()),
            }
        })?;
        let format =
            InputFormat::from_extension(ext).ok_or_else(|| ConversionError::UnknownFormat {
                hint: format!("unrecognized extension '.{ext}'"),
            })?;
        let bytes = std::fs::read(path)?;
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("document")
            .to_string();
        Ok(Self {
            name,
            format,
            bytes,
            path: Some(path.to_path_buf()),
        })
    }

    /// Construct directly from in-memory bytes (no disk access).
    pub fn from_bytes(name: impl Into<String>, format: InputFormat, bytes: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            format,
            bytes,
            path: None,
        }
    }

    /// The directory containing the source file, for resolving relative asset
    /// paths. `None` for in-memory sources.
    pub fn base_dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }

    /// View the bytes as UTF-8 text, for text-based backends.
    pub fn text(&self) -> Result<&str, ConversionError> {
        std::str::from_utf8(&self.bytes)
            .map_err(|e| ConversionError::Parse(format!("input is not valid UTF-8: {e}")))
    }
}
