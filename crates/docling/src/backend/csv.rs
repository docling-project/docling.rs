//! CSV backend.
//!
//! Mirrors `docling.backend.csv_backend.CsvDocumentBackend`: sniff the delimiter
//! among `, ; \t | :` from the first line (falling back to comma), parse the
//! whole file with RFC-4180 quote handling, and emit one table whose width is
//! the widest row (ragged rows are padded). Row 0 is the header.

use csv::ReaderBuilder;
use docling_core::{DoclingDocument, Node, Table};

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct CsvBackend;

impl DeclarativeBackend for CsvBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let delimiter = detect_delimiter(text);

        let mut reader = ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(text.as_bytes());

        let mut rows: Vec<Vec<String>> = Vec::new();
        for record in reader.records() {
            let record = record.map_err(|e| ConversionError::Parse(format!("csv: {e}")))?;
            // Cell escaping (newlines, pipes) happens centrally in the serializer.
            rows.push(record.iter().map(str::to_string).collect());
        }

        let mut doc = DoclingDocument::new(&source.name);
        if !rows.is_empty() {
            let num_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
            for row in &mut rows {
                row.resize(num_cols, String::new());
            }
            doc.push(Node::Table(Table {
                rows,
                location: None,
                structure: None,
            }));
        }
        Ok(doc)
    }
}

/// Sniff the delimiter from the first line: the candidate (`, ; \t | :`) that
/// occurs most often wins; comma is the default when none appear.
fn detect_delimiter(text: &str) -> u8 {
    const CANDIDATES: [u8; 5] = [b',', b';', b'\t', b'|', b':'];
    let first = text.lines().next().unwrap_or("");
    let mut best = b',';
    let mut best_count = 0usize;
    for &c in &CANDIDATES {
        let n = first.bytes().filter(|&b| b == c).count();
        if n > best_count {
            best_count = n;
            best = c;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(bytes: &[u8]) -> DoclingDocument {
        let src = SourceDocument::from_bytes("data", InputFormat::Csv, bytes.to_vec());
        CsvBackend.convert(&src).unwrap()
    }

    #[test]
    fn converts_csv_to_table() {
        let doc = convert(b"name,age\nAlice,30\nBob,25\n");
        assert_eq!(
            doc.export_to_markdown(),
            "| name   |   age |\n|--------|-------|\n| Alice  |    30 |\n| Bob    |    25 |\n"
        );
    }

    #[test]
    fn handles_quoted_comma() {
        // The quoted field keeps its embedded comma instead of splitting.
        let doc = convert(b"a,b\n1,\"Lozano, Dr\"\n");
        let Node::Table(table) = &doc.nodes[0] else {
            panic!("expected a table");
        };
        assert_eq!(
            table.rows[1],
            vec!["1".to_string(), "Lozano, Dr".to_string()]
        );
    }

    #[test]
    fn sniffs_semicolon_delimiter() {
        let doc = convert(b"a;b;c\n1;2;3\n");
        let Node::Table(table) = &doc.nodes[0] else {
            panic!("expected a table");
        };
        assert_eq!(table.rows[0], vec!["a", "b", "c"]);
    }
}
