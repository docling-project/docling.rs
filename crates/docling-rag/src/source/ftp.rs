//! FTP document source (feature `remote-sources`).
//!
//! `suppaftp`'s `FtpStream` is blocking and not easily held across `.await`, so a
//! fresh connection is opened inside `spawn_blocking` for each operation. Fine for
//! batch ingestion; compile-checked here, exercised against a live FTP server.

use super::{DocumentSource, SourceRef};
use crate::{RagConfig, RagError, Result};
use async_trait::async_trait;
use suppaftp::FtpStream;

/// Connection parameters for an FTP source.
#[derive(Debug, Clone)]
pub struct FtpSource {
    addr: String,
    dir: String,
    user: String,
    password: String,
}

impl FtpSource {
    /// Build from config (`RAG_SOURCE_URL`, `RAG_SOURCE_PATH`, `RAG_SOURCE_USER`,
    /// `RAG_SOURCE_PASSWORD`).
    pub fn from_config(cfg: &RagConfig) -> Result<Self> {
        let url = cfg
            .source_url
            .clone()
            .ok_or_else(|| RagError::config("RAG_SOURCE_URL is required for the ftp source"))?;
        let hostport = url.trim_start_matches("ftp://").trim_end_matches('/');
        let addr = if hostport.contains(':') {
            hostport.to_string()
        } else {
            format!("{hostport}:21")
        };
        Ok(FtpSource {
            addr,
            dir: cfg.source_path.clone(),
            user: cfg
                .source_user
                .clone()
                .unwrap_or_else(|| "anonymous".into()),
            password: cfg.source_password.clone().unwrap_or_default(),
        })
    }

    fn connect(&self) -> Result<FtpStream> {
        let mut ftp = FtpStream::connect(&self.addr)
            .map_err(|e| RagError::Source(format!("ftp connect {}: {e}", self.addr)))?;
        ftp.login(&self.user, &self.password)
            .map_err(|e| RagError::Source(format!("ftp login: {e}")))?;
        Ok(ftp)
    }
}

#[async_trait]
impl DocumentSource for FtpSource {
    async fn list(&self) -> Result<Vec<SourceRef>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut ftp = this.connect()?;
            let names = ftp
                .nlst(Some(&this.dir))
                .map_err(|e| RagError::Source(format!("ftp nlst: {e}")))?;
            let _ = ftp.quit();
            Ok(names
                .into_iter()
                .map(|name| {
                    let file = name.rsplit('/').next().unwrap_or(&name).to_string();
                    let rel = name
                        .strip_prefix(this.dir.trim_end_matches('/'))
                        .map(|s| s.trim_start_matches('/').to_string())
                        .unwrap_or_else(|| name.clone());
                    SourceRef {
                        uri: format!("ftp://{}/{}", this.addr, name),
                        name: file,
                        rel_path: rel,
                    }
                })
                .collect())
        })
        .await
        .map_err(|e| RagError::Source(format!("ftp list join: {e}")))?
    }

    async fn fetch(&self, r: &SourceRef) -> Result<Vec<u8>> {
        let this = self.clone();
        let remote = r
            .uri
            .strip_prefix(&format!("ftp://{}/", self.addr))
            .unwrap_or(&r.uri)
            .to_string();
        tokio::task::spawn_blocking(move || {
            let mut ftp = this.connect()?;
            let cursor = ftp
                .retr_as_buffer(&remote)
                .map_err(|e| RagError::Source(format!("ftp retr {remote}: {e}")))?;
            let _ = ftp.quit();
            Ok(cursor.into_inner())
        })
        .await
        .map_err(|e| RagError::Source(format!("ftp fetch join: {e}")))?
    }
}
