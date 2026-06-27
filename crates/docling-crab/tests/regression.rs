//! Output regression suite.
//!
//! Every supported source under `tests/data/<format>/sources/` is converted to
//! legacy Markdown, strict Markdown and docling JSON, and compared against the
//! committed fixtures in the sibling `expected/` directory. This pins the Rust
//! converter's output so any unintended change is caught.
//!
//! The ML formats (PDF, images, METS) need pdfium + the ONNX models, so they are
//! covered by the deterministic snapshot harness (`scripts/pdf_conformance.sh`)
//! instead of this pure-Rust test.
//!
//! Regenerate the fixtures after an *intentional* output change:
//!
//! ```bash
//! DOCLING_CRAB_REGEN=1 cargo test -p docling-crab --test regression
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use docling_crab::{DocumentConverter, SourceDocument};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Every file under `tests/data/*/sources/`, in a stable order.
fn sources() -> Vec<PathBuf> {
    let mut formats: Vec<PathBuf> = fs::read_dir(data_dir())
        .expect("tests/data missing")
        .flatten()
        .map(|e| e.path())
        .collect();
    formats.sort();

    let mut out = Vec::new();
    for fmt in formats {
        let sources = fmt.join("sources");
        if !sources.is_dir() {
            continue;
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&sources)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        out.extend(files);
    }
    out
}

/// `<fmt>/sources/<file>` → `<fmt>/expected/<file><suffix>`.
fn expected_path(src: &Path, suffix: &str) -> PathBuf {
    let fmt_dir = src.parent().unwrap().parent().unwrap();
    let name = src.file_name().unwrap().to_string_lossy();
    fmt_dir.join("expected").join(format!("{name}{suffix}"))
}

fn convert(src: &Path, strict: bool) -> Result<docling_crab::DoclingDocument, String> {
    let source = SourceDocument::from_file(src).map_err(|e| e.to_string())?;
    DocumentConverter::new()
        .strict(strict)
        .convert(source)
        .map(|r| r.document)
        .map_err(|e| e.to_string())
}

#[test]
fn outputs_match_fixtures() {
    let regen = std::env::var_os("DOCLING_CRAB_REGEN").is_some();
    let srcs = sources();
    assert!(!srcs.is_empty(), "no sources under {}", data_dir().display());

    let mut failures = Vec::new();
    for src in &srcs {
        let rel = src.strip_prefix(data_dir()).unwrap().display().to_string();

        let legacy = match convert(src, false) {
            Ok(d) => d,
            Err(e) => {
                failures.push(format!("{rel}: convert error: {e}"));
                continue;
            }
        };
        let strict = match convert(src, true) {
            Ok(d) => d,
            Err(e) => {
                failures.push(format!("{rel}: strict convert error: {e}"));
                continue;
            }
        };

        let outputs = [
            (".md", legacy.export_to_markdown()),
            (".strict.md", strict.export_to_markdown()),
            (".json", legacy.export_to_json()),
        ];
        for (suffix, got) in outputs {
            let path = expected_path(src, suffix);
            if regen {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, got).unwrap();
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(want) if want == got => {}
                Ok(_) => failures.push(format!(
                    "{rel}{suffix}: output changed (run DOCLING_CRAB_REGEN=1 to update)"
                )),
                Err(_) => failures.push(format!("{rel}{suffix}: missing fixture {}", path.display())),
            }
        }
    }

    if regen {
        eprintln!("regenerated fixtures for {} sources", srcs.len());
        return;
    }
    assert!(
        failures.is_empty(),
        "{} regression failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
