//! Local folder document source. Recursively lists files under a root directory;
//! works over any mounted filesystem (local disk, NFS, FUSE, …).

use super::{DocumentSource, SourceRef};
use crate::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Reads documents from a directory tree.
#[derive(Debug, Clone)]
pub struct FolderSource {
    root: PathBuf,
}

impl FolderSource {
    /// Create a source rooted at `root`.
    pub fn new(root: impl AsRef<Path>) -> Self {
        FolderSource {
            root: root.as_ref().to_path_buf(),
        }
    }
}

/// Recursively collect regular files under `dir` (depth-first, sorted for
/// determinism). Unreadable directories are skipped rather than failing the walk.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

#[async_trait]
impl DocumentSource for FolderSource {
    async fn list(&self) -> Result<Vec<SourceRef>> {
        let root = self.root.clone();
        // Directory walking is blocking I/O; keep it off the async reactor.
        let files = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            walk(&root, &mut out);
            out
        })
        .await
        .map_err(|e| crate::RagError::Source(format!("walk join: {e}")))?;

        Ok(files
            .into_iter()
            .map(|p| SourceRef {
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                rel_path: p
                    .strip_prefix(&self.root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned(),
                uri: format!("file://{}", p.display()),
            })
            .collect())
    }

    async fn fetch(&self, r: &SourceRef) -> Result<Vec<u8>> {
        let path = r.uri.strip_prefix("file://").unwrap_or(&r.uri);
        Ok(tokio::fs::read(path).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_and_fetches_files_recursively() {
        let dir = std::env::temp_dir().join(format!("rag-folder-{}", crate::model::new_id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.md"), b"# A\n\nalpha").unwrap();
        std::fs::write(dir.join("sub/b.md"), b"# B\n\nbeta").unwrap();

        let src = FolderSource::new(&dir);
        let refs = src.list().await.unwrap();
        assert_eq!(refs.len(), 2);
        let bytes = src.fetch(&refs[0]).await.unwrap();
        assert!(bytes.starts_with(b"# "));

        std::fs::remove_dir_all(&dir).ok();
    }
}
