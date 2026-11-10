//! CSV backend.
//!
//! Mirrors `docling.backend.csv_backend.CsvDocumentBackend`: sniff the delimiter
//! among `, ; \t | :` from the first line (falling back to comma), parse the
//! whole file with RFC-4180 quote handling, and emit one table whose width is
//! the widest row (ragged rows are padded). Row 0 is the header.

use csv::ReaderBuilder;
use docling_core::{DoclingDocument, Node, Table, TableCell};

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct CsvBackend;

impl DeclarativeBackend for CsvBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let text: &str = &text;
        let (text, delimiter) = match excel_sep_line(text) {
            Some((sep, rest)) => {
                // docling raises on a delimiter outside its candidates, a
                // named one included.
                let delimiter = u8::try_from(sep)
                    .ok()
                    .filter(|b| DELIMITERS.contains(b))
                    .ok_or_else(|| {
                        ConversionError::Parse(format!(
                            "csv: cannot convert csv with unknown delimiter {sep}"
                        ))
                    })?;
                (rest, delimiter)
            }
            None => (text, detect_delimiter(text)),
        };
        // docling reads with `csv.reader(strict=True)` and, since 2.129
        // (docling#4260), turns its `csv.Error` into a load error rather
        // than letting it escape: a closing quote followed by anything but
        // the delimiter or a line end, or a quote still open at EOF. The
        // `csv` crate is lenient about both, so they are checked up front.
        check_strict_quoting(text, delimiter)
            .map_err(|e| ConversionError::Parse(format!("csv: malformed quoting — {e}")))?;

        let mut reader = ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(text.as_bytes());

        let mut rows: Vec<Vec<String>> = Vec::new();
        for record in reader.records() {
            let record = record.map_err(|e| ConversionError::with_source("csv", e))?;
            // Cell escaping (newlines, pipes) happens centrally in the serializer.
            rows.push(record.iter().map(str::to_string).collect());
        }

        let mut doc = DoclingDocument::new(&source.name);
        if !rows.is_empty() {
            let num_cols = rows.iter().map(Vec::len).max().unwrap_or(0);
            // A ragged file's short rows hold only the cells they have, as
            // docling adds a `TableCell` per value read (its grid fills the
            // rest); the text grid is padded for the serializers.
            let cells = rows.iter().any(|r| r.len() != num_cols).then(|| {
                rows.iter()
                    .enumerate()
                    .flat_map(|(r, row)| {
                        row.iter().enumerate().map(move |(c, text)| TableCell {
                            text: text.clone(),
                            bbox: None,
                            start_row: r,
                            start_col: c,
                            row_span: 1,
                            col_span: 1,
                            column_header: r == 0,
                            row_header: false,
                            row_section: false,
                        })
                    })
                    .collect()
            });
            for row in &mut rows {
                row.resize(num_cols, String::new());
            }
            doc.push(Node::Table(Table {
                footnotes: Vec::new(),
                rows,
                location: None,
                structure: None,
                cell_blocks: None,
                cells,
                caption: None,
                caption_parent: Default::default(),
                caption_location: None,
                document_index: false,
            }));
        }
        Ok(doc)
    }
}

/// Python `csv`'s strict-mode errors: `'<delimiter>' expected after '"'`
/// when a quoted field's closing quote is followed by anything other than
/// the delimiter, a line end or EOF (`""` inside the field is an escaped
/// quote), and `unexpected end of data` when EOF arrives inside a quoted
/// field. A quote inside an *unquoted* field is ordinary text, as in Python.
fn check_strict_quoting(text: &str, delimiter: u8) -> Result<(), String> {
    let d = delimiter as char;
    let mut chars = text.chars().peekable();
    let mut field_start = true;
    let mut line = 1usize;
    while let Some(c) = chars.next() {
        if field_start && c == '"' {
            // Inside a quoted field.
            loop {
                match chars.next() {
                    None => {
                        return Err(format!(
                            "unexpected end of data in a quoted field (line {line})"
                        ))
                    }
                    Some('"') => match chars.peek() {
                        Some('"') => {
                            chars.next();
                        }
                        Some(&n) if n == d => {
                            chars.next();
                            field_start = true;
                            break;
                        }
                        Some('\n') | Some('\r') | None => {
                            field_start = true;
                            break;
                        }
                        Some(n) => {
                            return Err(format!(
                                "'{d}' expected after '\"' (line {line}), found {n:?}"
                            ))
                        }
                    },
                    Some('\n') => line += 1,
                    Some(_) => {}
                }
            }
            continue;
        }
        if c == '\n' {
            line += 1;
        }
        field_start = c == d || c == '\n' || c == '\r';
    }
    Ok(())
}

/// The delimiters docling accepts (`_DELIMITERS`).
const DELIMITERS: [u8; 5] = *b",;\t|:";

/// Excel's `sep=<char>` line (docling#5200): when the first non-blank line
/// is exactly `sep=` plus one character, that character is the delimiter
/// and the line is no row — Excel writes it for files whose values hold the
/// delimiter the sniffer would pick (decimal commas in a `;` file). Returns
/// the delimiter and the text after the line, from which the strict-quoting
/// check and the reader start.
fn excel_sep_line(text: &str) -> Option<(char, &str)> {
    let mut rest = text;
    loop {
        let end = rest.find('\n').map_or(rest.len(), |i| i + 1);
        let line = &rest[..end];
        let body = line.trim_end_matches(['\r', '\n']);
        // Python's `readline` splits a lone `\r` too; a blank line is any of
        // `\n`, `\r\n`, `\r` — skipped, as docling's sniffing loop does.
        if body.is_empty() && !line.is_empty() {
            rest = &rest[end..];
            continue;
        }
        let mut chars = body.strip_prefix("sep=")?.chars();
        let sep = chars.next()?;
        return chars.next().is_none().then_some((sep, &rest[end..]));
    }
}

/// Sniff the delimiter from the first line: the candidate (`, ; \t | :`) that
/// occurs most often wins. When the first line carries none — a quoted field
/// spanning several lines cuts it mid-quote (docling#3985, 2.123) — retry
/// over the first 4 KiB, which closes the quote; comma remains the default
/// when nothing matches at all. The first line stays authoritative otherwise:
/// widening the sample unconditionally would change the delimiter picked for
/// ragged files.
fn detect_delimiter(text: &str) -> u8 {
    const CANDIDATES: [u8; 5] = DELIMITERS;
    let count_best = |sample: &[u8]| {
        let mut best = b',';
        let mut best_count = 0usize;
        for &c in &CANDIDATES {
            let n = sample.iter().filter(|&&b| b == c).count();
            if n > best_count {
                best_count = n;
                best = c;
            }
        }
        (best, best_count)
    };
    let first = text.lines().next().unwrap_or("");
    // docling#4896: a header holding several candidates — a comma inside a
    // semicolon file's `Weight (kg, net)`, the `:` of a comma file's
    // `Time (hh:mm:ss)` — is read by the occurrences outside brackets, where
    // a column name rarely splits; only a header with none there falls back
    // to every occurrence.
    match count_best(&outside_brackets(first)) {
        (best, n) if n > 0 => best,
        _ => match count_best(first.as_bytes()) {
            (best, n) if n > 0 => best,
            _ => {
                let bytes = text.as_bytes();
                count_best(&bytes[..bytes.len().min(4096)]).0
            }
        },
    }
}

/// `line`'s bytes outside `()`, `[]` and `{}` — docling's
/// `_count_outside_brackets` (an unbalanced closer is ignored).
fn outside_brackets(line: &str) -> Vec<u8> {
    let mut depth = 0usize;
    let mut out = Vec::with_capacity(line.len());
    for b in line.bytes() {
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(b),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(bytes: &[u8]) -> DoclingDocument {
        let src = SourceDocument::from_bytes("data", InputFormat::Csv, bytes.to_vec());
        CsvBackend.convert(&src).unwrap()
    }

    /// docling#4896's cases (`test_header_with_second_delimiter_uses_following_rows`):
    /// a header holding two candidate delimiters — a comma inside a
    /// semicolon file's column name, a `:` in `Time (hh:mm)` of a ragged
    /// comma file — still splits on the file's delimiter, and the quoting
    /// (`'n/a'`, an unclosed `'90s music`) stays literal.
    #[test]
    fn header_with_a_second_delimiter_splits_on_the_files_one() {
        for (payload, expected) in [
            (
                "Product;Weight (kg, net);Price\nApple;0,2;1,50\nPear;0,35;2\n",
                &[
                    "Product",
                    "Weight (kg, net)",
                    "Price",
                    "Apple",
                    "0,2",
                    "1,50",
                    "Pear",
                    "0,35",
                    "2",
                ][..],
            ),
            (
                "Product,Weight (kg; net),Price\nApple,0.2,1.50\nPear,0.35,2\n",
                &[
                    "Product",
                    "Weight (kg; net)",
                    "Price",
                    "Apple",
                    "0.2",
                    "1.50",
                    "Pear",
                    "0.35",
                    "2",
                ][..],
            ),
            (
                "Date,Time (hh:mm),Value,Note\n2024-01-01,10:30,5,a\n2024-01-02,11:45,6\n",
                &[
                    "Date",
                    "Time (hh:mm)",
                    "Value",
                    "Note",
                    "2024-01-01",
                    "10:30",
                    "5",
                    "a",
                    "2024-01-02",
                    "11:45",
                    "6",
                ][..],
            ),
            (
                "id,ratio (a;b),x,y\n1,2;3,4,5\n2,6;7,8\n",
                &[
                    "id",
                    "ratio (a;b)",
                    "x",
                    "y",
                    "1",
                    "2;3",
                    "4",
                    "5",
                    "2",
                    "6;7",
                    "8",
                ][..],
            ),
            (
                "id,ratio (a;b),x,y\n1,0.5,2,3\n2,0.7,4\n",
                &[
                    "id",
                    "ratio (a;b)",
                    "x",
                    "y",
                    "1",
                    "0.5",
                    "2",
                    "3",
                    "2",
                    "0.7",
                    "4",
                ][..],
            ),
            (
                "Time (hh:mm:ss),Value\n10:30:00,5\n11:45:00\n",
                &["Time (hh:mm:ss)", "Value", "10:30:00", "5", "11:45:00"][..],
            ),
            (
                "Time (hh:mm),Value\n10:30,5\n11:45\n",
                &["Time (hh:mm)", "Value", "10:30", "5", "11:45"][..],
            ),
            (
                "Name,Time hh:mm:ss,Value\nA,10:30:00,5\nB,11:45:00\n",
                &[
                    "Name",
                    "Time hh:mm:ss",
                    "Value",
                    "A",
                    "10:30:00",
                    "5",
                    "B",
                    "11:45:00",
                ][..],
            ),
            (
                "Start: time,Value\n10:30,5\n11:45\n",
                &["Start: time", "Value", "10:30", "5", "11:45"][..],
            ),
            (
                "Date,Time (hh:mm),Note\n2024-01-01,10:30,'n/a'\n2024-01-02,11:45,'90s music\n",
                &[
                    "Date",
                    "Time (hh:mm)",
                    "Note",
                    "2024-01-01",
                    "10:30",
                    "'n/a'",
                    "2024-01-02",
                    "11:45",
                    "'90s music",
                ][..],
            ),
            (
                "id,ratio (a;b),Note\n1,0.5,'n/a'\n2,0.7,'90s music\n",
                &[
                    "id",
                    "ratio (a;b)",
                    "Note",
                    "1",
                    "0.5",
                    "'n/a'",
                    "2",
                    "0.7",
                    "'90s music",
                ][..],
            ),
            (
                "Date,Time (hh:mm),Note\n2024-01-01,10:30,'tbd'\n2024-01-02,11:45,'tbd'\n",
                &[
                    "Date",
                    "Time (hh:mm)",
                    "Note",
                    "2024-01-01",
                    "10:30",
                    "'tbd'",
                    "2024-01-02",
                    "11:45",
                    "'tbd'",
                ][..],
            ),
            (
                "Product;Weight (kg, net);Note\nApple;0,2;'n/a'\nPear;0,35;'90s music\n",
                &[
                    "Product",
                    "Weight (kg, net)",
                    "Note",
                    "Apple",
                    "0,2",
                    "'n/a'",
                    "Pear",
                    "0,35",
                    "'90s music",
                ][..],
            ),
            (
                "Product;Weight (kg, net);Note\nApple;0,2;'tbd'\nPear;0,35;'tbd'\n",
                &[
                    "Product",
                    "Weight (kg, net)",
                    "Note",
                    "Apple",
                    "0,2",
                    "'tbd'",
                    "Pear",
                    "0,35",
                    "'tbd'",
                ][..],
            ),
            (
                "Product;Weight (kg, net);Note\nApple;0,2;\"a; b\"\nPear;0,35;c\n",
                &[
                    "Product",
                    "Weight (kg, net)",
                    "Note",
                    "Apple",
                    "0,2",
                    "a; b",
                    "Pear",
                    "0,35",
                    "c",
                ][..],
            ),
        ] {
            let json = convert(payload.as_bytes()).export_to_json_value();
            let cells: Vec<&str> = json["tables"][0]["data"]["table_cells"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["text"].as_str().unwrap())
                .collect();
            assert_eq!(cells, expected, "{payload:?}");
        }
    }

    #[test]
    fn converts_csv_to_table() {
        let doc = convert(b"name,age\nAlice,30\nBob,25\n");
        assert_eq!(
            doc.export_to_markdown(),
            "| name   |   age |\n|--------|-------|\n| Alice  |    30 |\n| Bob    |    25 |\n"
        );
    }

    /// docling#4260 (2.129): Python's strict reader rejects a closing quote
    /// followed by text and a quote left open at EOF; both are load errors
    /// here too, while an escaped quote and a quote inside an unquoted field
    /// still read.
    #[test]
    fn malformed_quoting_is_a_load_error() {
        let bad = |s: &str| {
            let src = SourceDocument::from_bytes("t.csv", InputFormat::Csv, s.as_bytes().to_vec());
            CsvBackend.convert(&src).is_err()
        };
        assert!(bad("a,\"b\"c,d\n"));
        assert!(bad("a,\"open\n"));
        assert!(!bad("a,\"x \"\"y\"\" z\",c\n"));
        assert!(!bad("a,b\"c,d\n"));
        assert!(!bad("\"a\",\"b\"\n\"c\",\"d\""));
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

    /// docling#5200 (`test_excel_sep_line_sets_the_delimiter`, #1026): a
    /// first line `sep=<char>` names the delimiter and is not a row — LF,
    /// CRLF, a tab, and behind a UTF-8 BOM.
    #[test]
    fn excel_sep_line_sets_the_delimiter() {
        for (csv, want) in [
            (
                "sep=;\nName;Price\nApple;1,50\n",
                ["Name", "Price", "Apple", "1,50"],
            ),
            (
                "sep=,\r\nName,Price\r\nApple,1.50\r\n",
                ["Name", "Price", "Apple", "1.50"],
            ),
            (
                "sep=\t\nName\tPrice\nApple\t1.50\n",
                ["Name", "Price", "Apple", "1.50"],
            ),
            (
                "\u{feff}sep=|\nName|Price\nApple|1.50\n",
                ["Name", "Price", "Apple", "1.50"],
            ),
            (
                "\n\nsep=;\nName;Price\nApple;1,50\n",
                ["Name", "Price", "Apple", "1,50"],
            ),
        ] {
            let doc = convert(csv.as_bytes());
            let Node::Table(t) = &doc.nodes[0] else {
                panic!("no table for {csv:?}");
            };
            let cells: Vec<&str> = t.rows.iter().flatten().map(String::as_str).collect();
            assert_eq!(cells, want, "{csv:?}");
        }
    }

    /// The quoting check starts after the `sep=` line, and a `sep=` that is
    /// not exactly one character is an ordinary row; an unknown named
    /// delimiter is docling's load error.
    #[test]
    fn excel_sep_line_edges() {
        let doc = convert(b"sep=;\n\"a;b\";c\n1;2\n");
        let Node::Table(t) = &doc.nodes[0] else {
            panic!()
        };
        assert_eq!(t.rows[0], ["a;b", "c"]);
        let doc = convert(b"sep=;;\nx;y\n");
        let Node::Table(t) = &doc.nodes[0] else {
            panic!()
        };
        assert_eq!(t.rows[0], ["sep=", "", ""]);
        let src = SourceDocument::from_bytes("data", InputFormat::Csv, b"sep=#\na#b\n".to_vec());
        assert!(CsvBackend.convert(&src).is_err());
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
