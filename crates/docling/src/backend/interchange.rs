//! Legacy spreadsheet-interchange backends (#216): DIF, SYLK and dBase —
//! the table relics that outlived their applications. All three parse
//! natively (docling has no readers for any of them):
//!
//! - **DIF** (`.dif`, Data Interchange Format): line pairs — a `type,number`
//!   line then a value line. Type 1 carries a quoted string, type 0 a number
//!   (the value line is just a validity marker), type −1 a directive (`BOT`
//!   starts a row, `EOD` ends the data).
//! - **SYLK** (`.slk`/`.sylk`, Symbolic Link): `;`-separated records; `C`
//!   records place a cell at `X`/`Y` (1-based, sticky — an omitted
//!   coordinate repeats the previous one) with the value in `K` (quoted
//!   string with `;;` escaping, or a bare number).
//! - **dBase** (`.dbf`): binary — a header with 32-byte field descriptors
//!   (name, type, width) and fixed-width records; the field names become
//!   the table's header row. Deleted records (`*` flag) are skipped, memo
//!   fields (their content lives in a `.dbt` sidecar) come out empty, and
//!   `D` dates render as ISO `YYYY-MM-DD`. High bytes decode as
//!   Windows-1252.
//!
//! DIF and SYLK are sheet snapshots, so they run through the same
//! flood-fill region splitting as ODS sheets — a `.dif`/`.slk` saved from a
//! spreadsheet converts to the same tables as the sheet itself. dBase is a
//! single database table and converts as one.

use std::collections::HashMap;

use crate::backend::odf::emit_sheet_regions;
use crate::backend::rtf::decode_byte;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node, Table};

pub struct InterchangeBackend;

impl DeclarativeBackend for InterchangeBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        // The three formats are trivially distinguishable by content, so the
        // extension only routes here and a misnamed file still converts. The
        // text signatures go first — they are exact, while the dBase magic is
        // only a version byte plus plausibility checks.
        let mut doc = DoclingDocument::new(&source.name);
        if let Ok(text) = source.text() {
            if text.trim_start().starts_with("ID;") {
                convert_sylk(&text, &mut doc)?;
                return Ok(doc);
            }
            if text.lines().next().map(str::trim) == Some("TABLE") {
                convert_dif(&text, &mut doc)?;
                return Ok(doc);
            }
        }
        if looks_like_dbf(&source.bytes) {
            convert_dbf(&source.bytes, &mut doc)?;
            return Ok(doc);
        }
        Err(ConversionError::Parse(
            "interchange: neither DIF (TABLE header), SYLK (ID; record) nor dBase".into(),
        ))
    }
}

/// Render a cell number the way spreadsheet display does: integers without a
/// decimal point, everything else through the shortest `f64` form.
fn number_text(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ----------------------------------------------------------------- DIF

fn convert_dif(text: &str, doc: &mut DoclingDocument) -> Result<(), ConversionError> {
    let mut lines = text.lines();
    // Header sections (TABLE/VECTORS/TUPLES/…) are name + "v,n" + "\"…\""
    // triples until DATA opens the cell stream.
    loop {
        let Some(name) = lines.next() else {
            return Err(ConversionError::Parse("dif: no DATA section".into()));
        };
        lines.next();
        lines.next();
        if name.trim() == "DATA" {
            break;
        }
    }

    let mut cells: HashMap<(usize, usize), String> = HashMap::new();
    let mut row = 0usize;
    let mut col = 0usize;
    let mut started = false;
    while let Some(head) = lines.next() {
        let value_line = lines.next().unwrap_or("");
        let (kind, number) = head.split_once(',').unwrap_or((head, ""));
        match kind.trim() {
            "-1" => match value_line.trim() {
                "BOT" => {
                    if started {
                        row += 1;
                    }
                    started = true;
                    col = 0;
                }
                _ => break, // EOD
            },
            // Numeric cell: the value rides the header line; the value line
            // is a validity marker (V / NA / ERROR).
            "0" => {
                let text = match value_line.trim() {
                    "NA" | "ERROR" | "TRUE" | "FALSE" => value_line.trim().to_string(),
                    _ => number
                        .trim()
                        .parse::<f64>()
                        .map(number_text)
                        .unwrap_or_else(|_| number.trim().to_string()),
                };
                if !text.is_empty() {
                    cells.insert((row, col), text);
                }
                col += 1;
            }
            // String cell: the value line, quotes stripped.
            "1" => {
                let text = value_line.trim();
                let text = text.strip_prefix('"').unwrap_or(text);
                let text = text.strip_suffix('"').unwrap_or(text);
                if !text.is_empty() {
                    cells.insert((row, col), text.to_string());
                }
                col += 1;
            }
            _ => {}
        }
    }
    emit_sheet_regions(&cells, doc);
    Ok(())
}

// ----------------------------------------------------------------- SYLK

fn convert_sylk(text: &str, doc: &mut DoclingDocument) -> Result<(), ConversionError> {
    let mut cells: HashMap<(usize, usize), String> = HashMap::new();
    let (mut x, mut y) = (1usize, 1usize);
    for line in text.lines() {
        let mut fields = split_sylk(line);
        let Some(kind) = fields.next() else { continue };
        if kind != "C" {
            continue;
        }
        let mut value: Option<String> = None;
        for field in fields {
            let (tag, rest) = field.split_at(1.min(field.len()));
            match tag {
                "X" => x = rest.parse().unwrap_or(x),
                "Y" => y = rest.parse().unwrap_or(y),
                "K" => {
                    let text = if let Some(quoted) = rest.strip_prefix('"') {
                        quoted.strip_suffix('"').unwrap_or(quoted).to_string()
                    } else {
                        rest.parse::<f64>()
                            .map(number_text)
                            .unwrap_or_else(|_| rest.to_string())
                    };
                    value = Some(text);
                }
                _ => {}
            }
        }
        if let Some(v) = value {
            if !v.is_empty() {
                // SYLK is 1-based; the shared region splitter trims margins.
                cells.insert((y, x), v);
            }
        }
    }
    emit_sheet_regions(&cells, doc);
    Ok(())
}

/// Split a SYLK record on `;` while honoring the `;;` escape inside values
/// (a doubled semicolon is a literal one, not a separator).
fn split_sylk(line: &str) -> impl Iterator<Item = String> {
    let mut fields: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ';' {
            if chars.peek() == Some(&';') {
                chars.next();
                current.push(';');
            } else {
                fields.push(std::mem::take(&mut current));
                // continue into the next field
            }
        } else {
            current.push(c);
        }
    }
    fields.push(current);
    fields.into_iter().filter(|f| !f.is_empty())
}

// ----------------------------------------------------------------- dBase

/// dBase plausibility: a known version byte (dBase II–7 / FoxPro families,
/// with or without the memo flags in the high bits) followed by a sane
/// last-update date — the header proper is validated during the parse.
fn looks_like_dbf(d: &[u8]) -> bool {
    let known = matches!(
        d.first(),
        Some(
            0x02 | 0x03
                | 0x04
                | 0x05
                | 0x30
                | 0x31
                | 0x32
                | 0x43
                | 0x63
                | 0x83
                | 0x8b
                | 0x8e
                | 0xcb
                | 0xf5
                | 0xfb
        )
    );
    known && d.len() >= 32 && (1..=12).contains(&d[2]) && (1..=31).contains(&d[3])
}

struct DbfField {
    name: String,
    kind: u8,
    width: usize,
}

fn convert_dbf(d: &[u8], doc: &mut DoclingDocument) -> Result<(), ConversionError> {
    let err = |m: &str| ConversionError::Parse(format!("dbf: {m}"));
    let nrecords = u32::from_le_bytes(
        d.get(4..8)
            .ok_or_else(|| err("truncated header"))?
            .try_into()
            .unwrap(),
    ) as usize;
    let header_len = u16::from_le_bytes(d[8..10].try_into().unwrap()) as usize;
    let record_len = u16::from_le_bytes(d[10..12].try_into().unwrap()) as usize;

    // 32-byte field descriptors from offset 32 up to the 0x0D terminator.
    let mut fields: Vec<DbfField> = Vec::new();
    let mut off = 32usize;
    while off + 32 <= header_len.min(d.len()) && d[off] != 0x0D {
        let desc = &d[off..off + 32];
        let name_len = desc[..11].iter().position(|&b| b == 0).unwrap_or(11);
        fields.push(DbfField {
            name: desc[..name_len]
                .iter()
                .map(|&b| decode_byte(b, 1252))
                .collect(),
            kind: desc[11],
            width: desc[16] as usize,
        });
        off += 32;
    }
    if fields.is_empty() {
        return Err(err("no field descriptors"));
    }
    if record_len != 1 + fields.iter().map(|f| f.width).sum::<usize>() {
        return Err(err("record size does not match the field widths"));
    }

    let mut rows: Vec<Vec<String>> = vec![fields.iter().map(|f| f.name.clone()).collect()];
    for i in 0..nrecords {
        let start = header_len + i * record_len;
        let Some(record) = d.get(start..start + record_len) else {
            break; // truncated file: keep what parsed
        };
        if record[0] == b'*' {
            continue; // deleted
        }
        let mut row = Vec::with_capacity(fields.len());
        let mut p = 1usize;
        for field in &fields {
            let raw = &record[p..p + field.width];
            p += field.width;
            let text: String = raw.iter().map(|&b| decode_byte(b, 1252)).collect();
            let text = text.trim();
            row.push(match field.kind {
                // Memo content lives in the .dbt sidecar; degrade to empty.
                b'M' => String::new(),
                // YYYYMMDD → ISO date.
                b'D' if text.len() == 8 && text.chars().all(|c| c.is_ascii_digit()) => {
                    format!("{}-{}-{}", &text[..4], &text[4..6], &text[6..8])
                }
                b'L' => match text {
                    "T" | "t" | "Y" | "y" => "true".into(),
                    "F" | "f" | "N" | "n" => "false".into(),
                    _ => String::new(),
                },
                _ => text.to_string(),
            });
        }
        rows.push(row);
    }
    doc.push(Node::Table(Table {
        footnotes: Vec::new(),
        rows,
        location: None,
        structure: None,
        cell_blocks: None,
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(name: &str, bytes: &[u8]) -> DoclingDocument {
        let source = SourceDocument::from_bytes(name.to_string(), InputFormat::Dif, bytes.to_vec());
        InterchangeBackend.convert(&source).unwrap()
    }

    #[test]
    fn dif_rows_numbers_and_strings() {
        let dif = "TABLE\n0,1\n\"S\"\nVECTORS\n0,2\n\"\"\nTUPLES\n0,2\n\"\"\nDATA\n0,0\n\"\"\n\
                   -1,0\nBOT\n1,0\n\"Year\"\n1,0\n\"Ducks\"\n\
                   -1,0\nBOT\n0,2019\nV\n0,1.5\nV\n\
                   -1,0\nEOD\n";
        let md = convert("t.dif", dif.as_bytes()).export_to_markdown();
        assert!(md.contains("Year"), "{md}");
        assert!(md.contains("2019"), "{md}");
        assert!(md.contains("1.5"), "integers bare, floats kept:\n{md}");
    }

    #[test]
    fn sylk_sticky_coordinates_and_escapes() {
        let slk = "ID;PCALCOOO32\nC;X1;Y1;K\"a;;b\"\nC;X2;K42\nC;X1;Y2;K\"c\"\nC;X2;K\"d\"\nE\n";
        let doc = convert("t.slk", slk.as_bytes());
        let tables: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1);
        // Y sticks from the previous record; ";;" is a literal semicolon.
        assert_eq!(
            tables[0].rows,
            vec![
                vec!["a;b".to_string(), "42".into()],
                vec!["c".into(), "d".into()]
            ]
        );
    }

    #[test]
    fn dbf_fields_types_and_deleted_records() {
        // Header: version 3, 3 records, two fields (NAME C5, BORN D8).
        let mut f = vec![0x03u8, 99, 1, 1];
        f.extend(3u32.to_le_bytes());
        let header_len = 32 + 2 * 32 + 1;
        f.extend((header_len as u16).to_le_bytes());
        f.extend((1u16 + 5 + 8).to_le_bytes());
        f.extend([0u8; 20]);
        let mut field = |name: &[u8], kind: u8, width: u8| {
            let mut desc = [0u8; 32];
            desc[..name.len()].copy_from_slice(name);
            desc[11] = kind;
            desc[16] = width;
            f.extend(desc);
        };
        field(b"NAME", b'C', 5);
        field(b"BORN", b'D', 8);
        f.push(0x0D);
        f.extend(b" Ana  19991231");
        f.extend(b"*Del  20000101"); // deleted, skipped
        f.extend(b" Bob  20010615");
        let doc = convert("t.dbf", &f);
        let Node::Table(t) = &doc.nodes[0] else {
            panic!("table expected")
        };
        assert_eq!(t.rows[0], vec!["NAME".to_string(), "BORN".into()]);
        assert_eq!(t.rows[1], vec!["Ana".to_string(), "1999-12-31".into()]);
        assert_eq!(t.rows[2], vec!["Bob".to_string(), "2001-06-15".into()]);
        assert_eq!(t.rows.len(), 3, "deleted record dropped");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let source = SourceDocument::from_bytes(
            "x.dif".to_string(),
            InputFormat::Dif,
            b"random text".to_vec(),
        );
        assert!(InterchangeBackend.convert(&source).is_err());
    }
}
