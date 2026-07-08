//! Parsing micro-benchmarks for the declarative backends (HTML, DOCX).
//!
//! Each backend is driven directly by reference (no per-iteration clone), so the
//! numbers reflect parse + serialize work, not input copying. Inputs come from
//! the committed corpus under `tests/data/`.
//!
//! Run:  cargo bench -p docling.rs --bench parse

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};
use docling::backend::{DeclarativeBackend, DocxBackend, HtmlBackend};
use docling::{InputFormat, SourceDocument};

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data");

fn load(rel: &str, fmt: InputFormat) -> SourceDocument {
    let bytes = std::fs::read(format!("{CORPUS}/{rel}")).unwrap_or_else(|e| panic!("{rel}: {e}"));
    SourceDocument::from_bytes("bench", fmt, bytes)
}

fn html(c: &mut Criterion) {
    let mut g = c.benchmark_group("html");
    for (name, file) in [
        // A large, realistic page: headings, lists, several tables, links.
        ("wiki_duck", "html/sources/wiki_duck.html"),
        // Table-cell heavy: exercises the per-cell rich/has_descendant path.
        (
            "rich_table_cells",
            "html/sources/html_rich_table_cells.html",
        ),
    ] {
        let src = load(file, InputFormat::Html);
        g.bench_function(name, |b| {
            b.iter(|| black_box(HtmlBackend.convert(black_box(&src)).unwrap()))
        });
    }
    g.finish();
}

fn docx(c: &mut Criterion) {
    let mut g = c.benchmark_group("docx");
    for (name, file) in [
        ("rich_tables", "docx/sources/docx_rich_tables_01.docx"),
        ("tables", "docx/sources/word_tables.docx"),
        (
            "numbered_headers",
            "docx/sources/unit_test_headers_numbered.docx",
        ),
    ] {
        let src = load(file, InputFormat::Docx);
        g.bench_function(name, |b| {
            b.iter(|| black_box(DocxBackend.convert(black_box(&src)).unwrap()))
        });
    }
    g.finish();
}

criterion_group!(benches, html, docx);
criterion_main!(benches);
