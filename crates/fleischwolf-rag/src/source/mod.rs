//! Pluggable document sources. The default is a local [`folder`]; FTP and SFTP
//! are available behind the `remote-sources` feature.

pub mod folder;

#[cfg(feature = "remote-sources")]
pub mod ftp;
#[cfg(feature = "remote-sources")]
pub mod sftp;

use crate::config::SourceKind;
use crate::{RagConfig, RagError, Result};
use async_trait::async_trait;
use std::sync::Arc;

/// A handle to one document available from a source.
#[derive(Debug, Clone)]
pub struct SourceRef {
    /// A fully-qualified URI (`file:///…`, `ftp://host/…`, `sftp://host/…`).
    pub uri: String,
    /// A short display name, typically the file name.
    pub name: String,
}

/// A place documents are read from.
#[async_trait]
pub trait DocumentSource: Send + Sync {
    /// Enumerate the documents currently available.
    async fn list(&self) -> Result<Vec<SourceRef>>;

    /// Fetch the raw bytes of one document.
    async fn fetch(&self, r: &SourceRef) -> Result<Vec<u8>>;
}

/// Build the document source selected by `cfg.source`.
pub fn from_config(cfg: &RagConfig) -> Result<Arc<dyn DocumentSource>> {
    match cfg.source {
        SourceKind::Folder => Ok(Arc::new(folder::FolderSource::new(&cfg.source_path))),
        SourceKind::Ftp => {
            #[cfg(feature = "remote-sources")]
            {
                Ok(Arc::new(ftp::FtpSource::from_config(cfg)?))
            }
            #[cfg(not(feature = "remote-sources"))]
            {
                Err(RagError::FeatureDisabled(
                    "ftp".into(),
                    "remote-sources".into(),
                ))
            }
        }
        SourceKind::Sftp => {
            #[cfg(feature = "remote-sources")]
            {
                Ok(Arc::new(sftp::SftpSource::from_config(cfg)?))
            }
            #[cfg(not(feature = "remote-sources"))]
            {
                Err(RagError::FeatureDisabled(
                    "sftp".into(),
                    "remote-sources".into(),
                ))
            }
        }
    }
}
