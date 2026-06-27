//! The target API, end to end.
//!
//! Run with:  cargo run -p docling-crab --example convert -- path/to/file.md

use docling_crab::{DocumentConverter, SourceDocument};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sample.md".to_string());

    let converter = DocumentConverter::new();
    let result = converter
        .convert(SourceDocument::from_file(&path).unwrap())
        .unwrap();
    println!("{}", result.document.export_to_markdown());
}
