//! DocLang of the LaTeX corpus against docling's own (#884): the backend's
//! item tree serialized as docling-core's `DocLangDocSerializer` does it.
//!
//! `tests/data/latex/groundtruth/<name>.dclg` is docling 2.142.1's
//! `export_to_doclang()` of the source (docling-core's default placeholder
//! image mode, hence `export_to_doclang_with_images(Placeholder)` here) —
//! upstream's fixture, mirrored, never our output.
use std::fs;
use std::path::Path;

use docling::{DocumentConverter, ImageMode, SourceDocument};
use rayon::prelude::*;

#[test]
fn doclang_follows_docling() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.join("../../tests/data/latex");
    let mirror = fs::read_to_string(crate_dir.join("tests/data/latex/mirror.txt"))
        .expect("latex mirror list");
    let papers: Vec<&str> = mirror
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // A paper's conversion renders its PDF figures (matplotlib / TikZ vector
    // art, tens of MB of them in the corpus) in a debug build — a minute per
    // serial pass, so the papers convert in parallel.
    let failures: Vec<String> = papers
        .par_iter()
        .filter_map(|rel| {
            let name = rel.replace('/', "_");
            let want = fs::read_to_string(root.join("groundtruth").join(format!("{name}.dclg")))
                .unwrap_or_else(|e| panic!("{name}.dclg: {e}"));
            let got = DocumentConverter::new()
                .convert(SourceDocument::from_file(root.join("sources").join(rel)).expect("source"))
                .unwrap_or_else(|e| panic!("{rel}: {e}"))
                .document
                .export_to_doclang_with_images(ImageMode::Placeholder)
                .0;
            if got == want {
                return None;
            }
            let line = got
                .lines()
                .zip(want.lines())
                .position(|(g, w)| g != w)
                .unwrap_or(got.lines().count().min(want.lines().count()))
                + 1;
            Some(format!("{name} (first difference on line {line})"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "DocLang differs from docling: {failures:?}"
    );
}
