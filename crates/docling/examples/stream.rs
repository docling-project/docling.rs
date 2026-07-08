//! Streaming Markdown, end to end.
//!
//! Prints a document's Markdown in chunks as the converter produces them — for a
//! PDF, that means page by page, in document order, as the parallel pipeline
//! finishes each page.
//!
//! Run with:  cargo run -p docling.rs --example stream -- path/to/file.pdf

use std::io::{self, Write};

use docling::{DocumentConverter, SourceDocument};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sample.md".to_string());

    let source = SourceDocument::from_file(&path).expect("load input");
    let stream = DocumentConverter::new()
        .convert_streaming(source)
        .expect("start streaming conversion");

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for chunk in stream {
        match chunk {
            Ok(md) => {
                out.write_all(md.as_bytes()).expect("write stdout");
                // Flush each chunk so output is visible as it streams.
                out.flush().expect("flush stdout");
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}
