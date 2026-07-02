//! SFTP document source (feature `remote-sources`).
//!
//! Uses the blocking `ssh2` (libssh2) client inside `spawn_blocking`. A fresh
//! session is opened per operation. Compile-checked here; exercised against a live
//! SSH/SFTP server.

use super::{DocumentSource, SourceRef};
use crate::{RagConfig, RagError, Result};
use async_trait::async_trait;
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;

/// Connection parameters for an SFTP source.
#[derive(Debug, Clone)]
pub struct SftpSource {
    addr: String,
    dir: String,
    user: String,
    password: String,
}

impl SftpSource {
    /// Build from config (`RAG_SOURCE_URL`, `RAG_SOURCE_PATH`, `RAG_SOURCE_USER`,
    /// `RAG_SOURCE_PASSWORD`).
    pub fn from_config(cfg: &RagConfig) -> Result<Self> {
        let url = cfg
            .source_url
            .clone()
            .ok_or_else(|| RagError::config("RAG_SOURCE_URL is required for the sftp source"))?;
        let hostport = url.trim_start_matches("sftp://").trim_end_matches('/');
        let addr = if hostport.contains(':') {
            hostport.to_string()
        } else {
            format!("{hostport}:22")
        };
        Ok(SftpSource {
            addr,
            dir: cfg.source_path.clone(),
            user: cfg.source_user.clone().unwrap_or_default(),
            password: cfg.source_password.clone().unwrap_or_default(),
        })
    }

    fn session(&self) -> Result<ssh2::Session> {
        let tcp = TcpStream::connect(&self.addr)
            .map_err(|e| RagError::Source(format!("sftp connect {}: {e}", self.addr)))?;
        let mut sess = ssh2::Session::new().map_err(|e| RagError::Source(e.to_string()))?;
        sess.set_tcp_stream(tcp);
        sess.handshake()
            .map_err(|e| RagError::Source(format!("sftp handshake: {e}")))?;
        sess.userauth_password(&self.user, &self.password)
            .map_err(|e| RagError::Source(format!("sftp auth: {e}")))?;
        Ok(sess)
    }
}

#[async_trait]
impl DocumentSource for SftpSource {
    async fn list(&self) -> Result<Vec<SourceRef>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let sess = this.session()?;
            let sftp = sess.sftp().map_err(|e| RagError::Source(e.to_string()))?;
            let entries = sftp
                .readdir(Path::new(&this.dir))
                .map_err(|e| RagError::Source(format!("sftp readdir: {e}")))?;
            Ok(entries
                .into_iter()
                .filter(|(_, stat)| stat.is_file())
                .map(|(path, _)| {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    SourceRef {
                        uri: format!("sftp://{}{}", this.addr, path.display()),
                        name,
                    }
                })
                .collect())
        })
        .await
        .map_err(|e| RagError::Source(format!("sftp list join: {e}")))?
    }

    async fn fetch(&self, r: &SourceRef) -> Result<Vec<u8>> {
        let this = self.clone();
        let remote = r
            .uri
            .strip_prefix(&format!("sftp://{}", self.addr))
            .unwrap_or(&r.uri)
            .to_string();
        tokio::task::spawn_blocking(move || {
            let sess = this.session()?;
            let sftp = sess.sftp().map_err(|e| RagError::Source(e.to_string()))?;
            let mut f = sftp
                .open(Path::new(&remote))
                .map_err(|e| RagError::Source(format!("sftp open {remote}: {e}")))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)
                .map_err(|e| RagError::Source(e.to_string()))?;
            Ok(buf)
        })
        .await
        .map_err(|e| RagError::Source(format!("sftp fetch join: {e}")))?
    }
}
