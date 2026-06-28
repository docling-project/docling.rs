//! Generate the PDF snapshot baseline.
//!
//! `cargo run --release -p fleischwolf-pdf --example snapshot -- <root> <outdir>`
//!
//! Recursively converts every `*.pdf` under `<root>` and writes its Markdown to
//! `<outdir>/<relative-path>.md` (the pipeline's deterministic output, committed
//! as the conformance baseline). Conversion errors are written verbatim as
//! `ERROR: …` so failures are captured, never silently skipped.

use std::path::{Path, PathBuf};

use fleischwolf_pdf::Pipeline;

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "tif", "tiff", "bmp", "gif", "webp"];

fn is_supported(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some(e) if e == "pdf" || e == "gz" || IMAGE_EXTS.contains(&e)
    )
}

fn find_pdfs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            // Skip `large/` — big perf-test inputs with no conformance baseline.
            if p.file_name().is_some_and(|n| n == "large") {
                continue;
            }
            find_pdfs(&p, out);
        } else if is_supported(&p) {
            out.push(p);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: snapshot <root> <outdir>"));
    let outdir = PathBuf::from(args.next().expect("usage: snapshot <root> <outdir>"));

    let mut pdfs = Vec::new();
    find_pdfs(&root, &mut pdfs);

    let mut pipeline = Pipeline::new().expect("load pipeline");
    let (mut ok, mut err) = (0u32, 0u32);
    for pdf in &pdfs {
        let rel = pdf.strip_prefix(&root).unwrap_or(pdf);
        let name = pdf.file_name().unwrap().to_string_lossy().to_string();
        let md = match std::fs::read(pdf)
            .map_err(|e| format!("read: {e}"))
            .and_then(|bytes| {
                let ext = pdf.extension().and_then(|e| e.to_str()).unwrap_or("");
                let result = if ext == "gz" {
                    fleischwolf_pdf::convert_mets_gbs(&bytes, &name)
                } else if IMAGE_EXTS.contains(&ext) {
                    pipeline.convert_image(&bytes, &name)
                } else {
                    pipeline.convert(&bytes, None, &name)
                };
                result
                    .map(|d| d.export_to_markdown())
                    .map_err(|e| e.to_string())
            }) {
            Ok(md) => {
                ok += 1;
                md
            }
            Err(e) => {
                err += 1;
                eprintln!("ERR {}: {e}", rel.display());
                format!("ERROR: {e}\n")
            }
        };
        // Groundtruth naming keeps the source extension: `<file>.<ext>.md`.
        let mut dest = outdir.join(rel).into_os_string();
        dest.push(".md");
        let dest = PathBuf::from(dest);
        std::fs::create_dir_all(dest.parent().unwrap()).expect("mkdir");
        std::fs::write(&dest, md).expect("write snapshot");
    }
    eprintln!("snapshots: {} ok, {} error, {} total", ok, err, pdfs.len());
}
