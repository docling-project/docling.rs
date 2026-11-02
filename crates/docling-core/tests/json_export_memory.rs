#![cfg(feature = "preserve-order")] // asserts docling's field order (#970)
//! Peak heap of `export_to_json_value` on a long document.
//!
//! The export's `json!` constructors deep-copy every interpolated `Value`
//! (`to_value`): the top-level object copied the finished `texts` array whole,
//! so the export briefly held its result twice — 22 GB at peak for a 10 GB
//! value on a 3M-item PDF. The hot constructors now move their parts in.
//!
//! Measured with a counting allocator, per thread so the harness's other test
//! threads don't leak into the numbers.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use docling_core::{DoclingDocument, Node, Table, TableCell, TableStructure};

struct PerThread;

thread_local! {
    static CUR: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn track(delta: isize) {
    let _ = CUR.try_with(|c| {
        let v = c.get() + delta;
        c.set(v);
        let _ = PEAK.try_with(|p| p.set(p.get().max(v)));
    });
}

unsafe impl GlobalAlloc for PerThread {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        track(l.size() as isize);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        track(-(l.size() as isize));
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        track(n as isize - l.size() as isize);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static ALLOC: PerThread = PerThread;

/// `pages` pages of `per_page` short located paragraphs — the flat-node shape
/// the PDF text layer produces.
fn long_document(pages: usize, per_page: usize) -> DoclingDocument {
    let mut doc = DoclingDocument::new("long");
    for p in 1..=pages {
        doc.nodes.push(Node::PageInfo {
            page_no: p,
            width: 612.0,
            height: 792.0,
        });
        for i in 0..per_page {
            let (x, y) = ((i % 10) as f32 * 50.0, (i / 10) as f32 * 12.0);
            doc.nodes.push(Node::Prov {
                page_no: p,
                bbox: [x, y, x + 40.0, y + 9.0],
                charspan: [0, 6],
                bottom_left: false,
                seq: None,
                inner: Box::new(Node::Paragraph {
                    text: format!("{:06}", p * per_page + i),
                }),
            });
        }
    }
    doc
}

#[test]
fn export_does_not_hold_its_result_twice() {
    let doc = long_document(50, 200);
    let start = CUR.with(Cell::get);
    PEAK.with(|p| p.set(start));
    let json = doc.export_to_json_value();
    let peak = (PEAK.with(Cell::get) - start) as usize;
    let output = (CUR.with(Cell::get) - start) as usize;
    eprintln!("export peak {peak} B for a {output} B value");
    assert_eq!(json["texts"].as_array().map(Vec::len), Some(50 * 200));
    assert!(
        peak * 4 < output * 5,
        "export peaked at {peak} B for a {output} B value"
    );
}

#[test]
fn write_json_writes_the_value_without_building_it() {
    let doc = long_document(50, 200);
    let start = CUR.with(Cell::get);
    let value = doc.export_to_json_value();
    let value_size = (CUR.with(Cell::get) - start) as usize;
    let expected = serde_json::to_vec(&value).unwrap();
    let pretty = serde_json::to_vec_pretty(&value).unwrap();
    drop(value);

    let mut out = Vec::with_capacity(expected.len());
    let start = CUR.with(Cell::get);
    PEAK.with(|p| p.set(start));
    doc.write_json(&mut out).unwrap();
    let peak = (PEAK.with(Cell::get) - start) as usize;
    eprintln!("write_json peak {peak} B; the Value is {value_size} B");
    assert!(
        out == expected,
        "write_json differs from serializing the Value"
    );
    // The typed items cost a fraction of their `Value`s.
    assert!(
        peak * 4 < value_size,
        "write_json peaked at {peak} B; the Value it avoids is {value_size} B"
    );

    let mut out = Vec::new();
    doc.write_json_pretty(&mut out).unwrap();
    assert!(out == pretty);
    assert_eq!(doc.export_to_json().into_bytes(), pretty);
}

/// Items the compact path doesn't cover — an ASR segment's `source` (#614),
/// a heading, an unlocated paragraph — and a box past the page edge (the
/// clamp) come out of `write_json` exactly as from the `Value`.
#[test]
fn write_json_matches_the_value_off_the_compact_path() {
    use docling_core::tree::TreeTrack;
    let mut doc = DoclingDocument::new("mixed");
    doc.nodes.push(Node::PageInfo {
        page_no: 1,
        width: 612.0,
        height: 792.0,
    });
    doc.nodes.push(Node::Track {
        track: TreeTrack {
            start_time: 0.0,
            end_time: 1.5,
            identifier: Some("c1".into()),
            voice: Some("Ann".into()),
        },
        cue: "hello".into(),
        inner: Box::new(Node::Prov {
            page_no: 1,
            bbox: [10.0, 20.0, 700.0, 30.0],
            charspan: [0, 5],
            bottom_left: false,
            seq: None,
            inner: Box::new(Node::Paragraph {
                text: "[time: 0.0-1.5] hello".into(),
            }),
        }),
    });
    doc.nodes.push(Node::Located {
        location: [10, 10, 200, 20],
        inner: Box::new(Node::Paragraph {
            text: "a \\_b &amp; c".into(),
        }),
    });
    doc.nodes.push(Node::Heading {
        level: 2,
        text: "Heading".into(),
    });
    doc.nodes.push(Node::Paragraph {
        text: "unlocated".into(),
    });

    let value = doc.export_to_json_value();
    let mut out = Vec::new();
    doc.write_json(&mut out).unwrap();
    assert!(out == serde_json::to_vec(&value).unwrap());

    let first = &value["texts"][0];
    let keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "self_ref",
            "parent",
            "children",
            "content_layer",
            "label",
            "prov",
            "source",
            "orig",
            "text"
        ]
    );
    assert_eq!(first["prov"][0]["bbox"]["r"], 612.0);
}

fn cell(
    text: &str,
    r: usize,
    c: usize,
    spans: (usize, usize),
    bbox: Option<[f32; 4]>,
) -> TableCell {
    TableCell {
        text: text.into(),
        bbox,
        start_row: r,
        start_col: c,
        row_span: spans.0,
        col_span: spans.1,
        column_header: r == 0,
        row_header: false,
        row_section: false,
    }
}

/// Tables are written typed (#875) — each cell once, the grid as slots
/// pointing at it — and come out of `write_json` exactly as from the
/// `Value`: spans repeated over the grid, docling's filler cells, boxes
/// with digits an `f32` does not hold, the page clamp, a non-finite box.
#[test]
fn write_json_matches_the_value_for_tables() {
    let mut doc = DoclingDocument::new("tables");
    doc.nodes.push(Node::PageInfo {
        page_no: 1,
        width: 612.0,
        height: 792.0,
    });
    let first_class = Table {
        // Ragged: the grid comes from the cells, and (2, 1) is a filler.
        rows: vec![
            vec!["Head".into(), "Head".into()],
            vec!["a".into(), "b".into()],
            vec!["c".into()],
        ],
        cells: Some(vec![
            cell("Head", 0, 0, (1, 2), Some([10.1, 20.3, 700.7, 30.9])),
            cell("a", 1, 0, (1, 1), Some([1.0 / 3.0, 40.0, 50.0, 50.0])),
            cell(
                "b \\_x &amp; y",
                1,
                1,
                (1, 1),
                Some([60.0, f32::NAN, 90.0, 50.0]),
            ),
            cell("c", 2, 0, (1, 1), None),
        ]),
        ..Table::default()
    };
    doc.nodes.push(Node::Prov {
        page_no: 1,
        bbox: [10.0, 20.0, 700.0, 60.0],
        charspan: [0, 0],
        bottom_left: false,
        seq: None,
        inner: Box::new(Node::Table(first_class)),
    });
    // The dense-rows path: a horizontal span through the OTSL overlay.
    doc.nodes.push(Node::Table(Table {
        rows: vec![
            vec!["x".into(), "x".into(), "y".into()],
            vec!["1".into(), "2".into(), "3".into()],
        ],
        structure: Some(TableStructure {
            col_continuation: vec![vec![false, true, false], vec![false; 3]],
            ..TableStructure::default()
        }),
        ..Table::default()
    }));

    let value = doc.export_to_json_value();
    let mut out = Vec::new();
    doc.write_json(&mut out).unwrap();
    assert!(out == serde_json::to_vec(&value).unwrap());
    let mut out = Vec::new();
    doc.write_json_pretty(&mut out).unwrap();
    assert!(out == serde_json::to_vec_pretty(&value).unwrap());

    let data = &value["tables"][0]["data"];
    assert_eq!(
        (data["num_rows"].as_u64(), data["num_cols"].as_u64()),
        (Some(3), Some(2))
    );
    // The spanning header fills both of its grid positions, clamped to the
    // page like its `table_cells` record; the f32 box keeps its f64 digits.
    let head = &data["table_cells"][0];
    assert_eq!(data["grid"][0][0], *head);
    assert_eq!(data["grid"][0][1], *head);
    assert_eq!(head["bbox"]["r"], 612.0);
    assert_eq!(head["bbox"]["l"], 10.1);
    assert_eq!(data["table_cells"][2]["text"], "b _x & y");
    assert!(data["table_cells"][2]["bbox"]["t"].is_null());
    assert_eq!(
        data["grid"][2][1],
        serde_json::json!({
            "row_span": 1, "col_span": 1,
            "start_row_offset_idx": 2, "end_row_offset_idx": 3,
            "start_col_offset_idx": 1, "end_col_offset_idx": 2,
            "text": "", "column_header": false, "row_header": false,
            "row_section": false, "fillable": false,
        })
    );
    let dense = &value["tables"][1]["data"];
    assert_eq!(dense["table_cells"].as_array().map(Vec::len), Some(5));
    assert_eq!(dense["grid"][0][1]["col_span"], 2);
    assert_eq!(dense["grid"][0][0], dense["grid"][0][1]);
}

/// A CSV-shaped table — short cells, no boxes — as docling JSON: as maps,
/// a cell cost two `serde_json` objects of eleven owned keys (its
/// `table_cells` record and its grid copy, ~7.5 KB of export peak per cell
/// on #875's 640,000-cell CSV); typed, `write_json` holds the cells once.
#[test]
fn write_json_keeps_table_cells_typed() {
    let (rows, cols) = (2_000, 16);
    let mut doc = DoclingDocument::new("sheet");
    doc.nodes.push(Node::Table(Table {
        rows: (0..rows)
            .map(|r| (0..cols).map(|c| format!("{}", r * cols + c)).collect())
            .collect(),
        ..Table::default()
    }));
    let start = CUR.with(Cell::get);
    let value = doc.export_to_json_value();
    let value_size = (CUR.with(Cell::get) - start) as usize;
    let expected = serde_json::to_vec(&value).unwrap();
    drop(value);

    let mut out = Vec::with_capacity(expected.len());
    let start = CUR.with(Cell::get);
    PEAK.with(|p| p.set(start));
    doc.write_json(&mut out).unwrap();
    let peak = (PEAK.with(Cell::get) - start) as usize;
    eprintln!(
        "write_json peak {peak} B ({} B/cell); the Value is {value_size} B ({} B/cell)",
        peak / (rows * cols),
        value_size / (rows * cols)
    );
    assert!(
        out == expected,
        "write_json differs from serializing the Value"
    );
    assert!(
        peak * 8 < value_size,
        "write_json peaked at {peak} B; the Value it avoids is {value_size} B"
    );
}

/// docling-serve appends its confidence report to the document JSON: the
/// typed writer with `extras` is byte for byte the `Value` with the key set.
#[test]
fn write_json_pretty_with_appends_extras_like_the_value() {
    let doc = long_document(2, 3);
    let report = serde_json::json!({ "mean_grade": "good", "pages": { "1": 0.9 } });
    let mut value = doc.export_to_json_value();
    value["confidence"] = report.clone();
    let mut out = Vec::new();
    doc.write_json_pretty_with(&mut out, &[("confidence", &report)])
        .unwrap();
    assert!(out == serde_json::to_vec_pretty(&value).unwrap());
    let mut plain = Vec::new();
    doc.write_json_pretty_with(&mut plain, &[]).unwrap();
    assert_eq!(String::from_utf8(plain).unwrap(), doc.export_to_json());
}
