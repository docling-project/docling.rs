//! Apple Numbers input (#1031): docling's `IWorkNumbersDocumentBackend`
//! (docling#4113, docling#5087, docling#5101) pinned against upstream's own
//! corpus, mirrored in `tests/data/numbers/` (repo root), and the behaviour
//! `tests/test_backend_iwork_numbers.py` checks there, ported test by test.
//!
//! The two Tika fixtures are the same two-sheet checking register saved by
//! both generations — `numbers_2013` as `Index/*.iwa`, `numbers_iwork09` as an
//! iWork '09 `index.xml` — so the independent readers are checked against each
//! other; the numbers-parser fixtures fill cells with pictures.
//!
//! Left out: the tests of options this port does not have (`sheet_names`,
//! the archive size limits, LibreOffice chart rendering), and the ones that
//! transplant a Keynote photo onto a sheet (the picture readers are Keynote's
//! and Pages', pinned there).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, InputFormat, SourceDocument};
use serde_json::Value;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/numbers")
}

fn source(name: &str) -> PathBuf {
    corpus().join("sources").join(name)
}

const IWA: &str = "numbers_2013.numbers";
const IWORK09: &str = "numbers_iwork09.numbers";
const BOTH: [&str; 2] = [IWA, IWORK09];

fn convert_with(converter: DocumentConverter, source: SourceDocument) -> Value {
    let doc = converter
        .convert(source)
        .unwrap_or_else(|e| panic!("{e}"))
        .document;
    serde_json::from_str(&doc.export_to_json()).unwrap()
}

fn convert(name: &str) -> Value {
    convert_with(
        DocumentConverter::new(),
        SourceDocument::from_file(source(name)).unwrap(),
    )
}

fn convert_bytes(name: &str, bytes: Vec<u8>) -> Value {
    convert_with(
        DocumentConverter::new(),
        SourceDocument::from_bytes(name, InputFormat::Numbers, bytes),
    )
}

/// `RefItem.resolve`.
fn resolve<'a>(doc: &'a Value, reference: &Value) -> &'a Value {
    let path = reference["$ref"].as_str().expect("a $ref");
    path.trim_start_matches("#/")
        .split('/')
        .fold(doc, |item, key| match key.parse::<usize>() {
            Ok(i) => &item[i],
            Err(_) => &item[key],
        })
}

fn tables(doc: &Value) -> Vec<&Value> {
    doc["tables"].as_array().unwrap().iter().collect()
}

fn pictures(doc: &Value) -> Vec<&Value> {
    doc["pictures"].as_array().unwrap().iter().collect()
}

/// `caption_text`: the item's captions joined.
fn caption(doc: &Value, item: &Value) -> String {
    item["captions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| resolve(doc, c)["text"].as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn cells(table: &Value) -> BTreeMap<(u64, u64), &Value> {
    table["data"]["table_cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let at = |k: &str| c[k].as_u64().unwrap();
            ((at("start_row_offset_idx"), at("start_col_offset_idx")), c)
        })
        .collect()
}

fn grid(table: &Value) -> BTreeMap<(u64, u64), String> {
    cells(table)
        .into_iter()
        .map(|(k, c)| (k, c["text"].as_str().unwrap().to_string()))
        .collect()
}

fn prov_top(item: &Value) -> (u64, f64) {
    let prov = &item["prov"][0];
    (
        prov["page_no"].as_u64().unwrap(),
        prov["bbox"]["t"].as_f64().unwrap(),
    )
}

/// The texts of the notes layer: the sticky notes.
fn notes(doc: &Value) -> Vec<String> {
    doc["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["content_layer"] == "notes")
        .map(|t| t["text"].as_str().unwrap().to_string())
        .collect()
}

/// A chart picture's data read back as rows of text (`_chart_grid`).
fn chart_grid(picture: &Value) -> Vec<Vec<String>> {
    let data = &picture["meta"]["tabular_chart"]["chart_data"];
    let at: BTreeMap<(u64, u64), String> = data["table_cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                (
                    c["start_row_offset_idx"].as_u64().unwrap(),
                    c["start_col_offset_idx"].as_u64().unwrap(),
                ),
                c["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let (rows, cols) = (
        data["num_rows"].as_u64().unwrap(),
        data["num_cols"].as_u64().unwrap(),
    );
    (0..rows)
        .map(|r| {
            (0..cols)
                .map(|c| at.get(&(r, c)).cloned().unwrap_or_default())
                .collect()
        })
        .collect()
}

/// The items a rich cell's group holds, in order (`_cell_items`).
fn cell_items<'a>(doc: &'a Value, cell: &Value) -> Vec<&'a Value> {
    let group = resolve(doc, &cell["ref"]);
    group["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| resolve(doc, c))
        .collect()
}

fn is_rich(cell: &Value) -> bool {
    cell.get("ref").is_some_and(|r| !r.is_null())
}

fn image_size(item: &Value) -> (u64, u64) {
    let size = &item["image"]["size"];
    (
        size["width"].as_f64().unwrap() as u64,
        size["height"].as_f64().unwrap() as u64,
    )
}

/// `test_conversion_matches_the_groundtruth`: the Markdown byte for byte and
/// the JSON item for item. What is left out of the JSON comparison: the
/// schema version (ours is newer), the `origin` block (hard-coded for every
/// format), and a picture's bytes — upstream re-encodes every picture to PNG
/// through Pillow, we keep the bytes the document stores (MIGRATION.md).
#[test]
fn conversion_matches_the_groundtruth() {
    fn normalize(v: &mut Value) {
        match v {
            Value::Object(map) => {
                map.remove("version");
                map.remove("origin");
                if map.contains_key("mimetype") && map.contains_key("uri") {
                    map.remove("mimetype");
                    map.remove("uri");
                }
                map.values_mut().for_each(normalize);
            }
            Value::Array(items) => items.iter_mut().for_each(normalize),
            _ => {}
        }
    }
    for name in [
        IWA,
        IWORK09,
        "numbers_cell_pictures.numbers",
        "numbers_cell_fills.numbers",
    ] {
        let doc = DocumentConverter::new()
            .convert(SourceDocument::from_file(source(name)).unwrap())
            .unwrap()
            .document;
        let groundtruth = corpus().join("groundtruth").join(name);
        let md = fs::read_to_string(groundtruth.with_extension("numbers.md")).unwrap();
        assert_eq!(
            doc.export_to_markdown().trim_end_matches('\n'),
            md.trim_end_matches('\n'),
            "{name}: Markdown drifted from upstream groundtruth"
        );
        let mut ours: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        let mut theirs: Value = serde_json::from_str(
            &fs::read_to_string(groundtruth.with_extension("numbers.json")).unwrap(),
        )
        .unwrap();
        normalize(&mut ours);
        normalize(&mut theirs);
        assert!(
            ours == theirs,
            "{name}: JSON drifted from upstream groundtruth"
        );
    }
}

#[test]
fn each_sheet_becomes_a_page_and_a_sheet_group() {
    for name in BOTH {
        let doc = convert(name);
        let sheets: Vec<&str> = doc["groups"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|g| g["label"] == "sheet")
            .map(|g| g["name"].as_str().unwrap())
            .collect();
        assert_eq!(sheets, ["Checking", "Second sheet"], "{name}");
        let pages: Vec<&String> = doc["pages"].as_object().unwrap().keys().collect();
        assert_eq!(pages, ["1", "2"], "{name}");
    }
}

#[test]
fn tables_keep_their_names_geometry_and_headers() {
    for name in BOTH {
        let doc = convert(name);
        let tables = tables(&doc);
        let names: Vec<String> = tables.iter().map(|t| caption(&doc, t)).collect();
        assert_eq!(
            names,
            ["Account Categories", "Transactions", "Table 1"],
            "{name}"
        );
        let shape = |t: &Value| {
            (
                t["data"]["num_rows"].as_u64().unwrap(),
                t["data"]["num_cols"].as_u64().unwrap(),
            )
        };
        assert_eq!(shape(tables[0]), (7, 2), "{name}");
        assert_eq!(shape(tables[1]), (14, 6), "{name}");
        // Transactions declares two header rows and one header column.
        let transactions = cells(tables[1]);
        let header_rows: Vec<u64> = transactions
            .iter()
            .filter(|(_, c)| c["column_header"] == true)
            .map(|((row, _), _)| *row)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(header_rows, [0, 1], "{name}");
        assert!(
            transactions
                .iter()
                .filter(|(_, c)| c["row_header"] == true)
                .all(|((_, col), _)| *col == 0),
            "{name}"
        );
    }
}

/// Numbers keeps a sheet's drawables in z-order; the checking sheet stores its
/// two tables the other way up, so they are sorted down the sheet.
#[test]
fn tables_are_ordered_down_their_sheet() {
    for name in BOTH {
        let doc = convert(name);
        let mut by_sheet: BTreeMap<u64, Vec<f64>> = BTreeMap::new();
        for table in tables(&doc) {
            let (page, top) = prov_top(table);
            by_sheet.entry(page).or_default().push(top);
        }
        assert_eq!(by_sheet[&1].len(), 2, "{name}");
        for tops in by_sheet.values() {
            assert!(tops.windows(2).all(|w| w[0] <= w[1]), "{name}: {tops:?}");
        }
    }
}

#[test]
fn typed_cells_are_rendered_as_the_sheet_shows_them() {
    for name in BOTH {
        let doc = convert(name);
        let tables = tables(&doc);
        let register = grid(tables[1]);
        assert_eq!(register[&(1, 1)], "Date", "{name}");
        assert_eq!(register[&(2, 1)], "2009-10-01 00:00:00", "{name}");
        assert_eq!(register[&(2, 2)], "Rent", "{name}");
        assert_eq!(register[&(2, 4)], "-775", "{name}");
        // A running total: the cached result of a formula.
        assert_eq!(register[&(2, 5)], "3875", "{name}");
        // No trailing ".0", no binary floating-point noise.
        assert_eq!(register[&(3, 4)], "-97.4", "{name}");
        let totals = grid(tables[0]);
        assert_eq!(totals[&(6, 0)], "Total", "{name}");
        assert_eq!(totals[&(6, 1)], "-2575.9", "{name}");
    }
}

/// The two readers agree on the register except the pop-up menu column: an
/// iWork '09 document stores the label, a 2013+ one only the menu index.
#[test]
fn both_generations_agree_on_the_spreadsheet() {
    let modern = grid(tables(&convert(IWA))[1]);
    let legacy = grid(tables(&convert(IWORK09))[1]);
    let popup = 3;
    let shared = |g: &BTreeMap<(u64, u64), String>| -> BTreeMap<(u64, u64), String> {
        g.iter()
            .filter(|((_, col), _)| *col != popup)
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    };
    assert_eq!(shared(&modern), shared(&legacy));
    assert_eq!(legacy[&(2, popup)], "Home");
    assert_eq!(modern[&(2, popup)], "2");
}

/// An iWork '09 datasource stores only the cells a row uses, without
/// coordinates; the grid's occupancy counts place them.
#[test]
fn sparse_rows_land_in_the_columns_the_spreadsheet_shows() {
    let modern = grid(tables(&convert(IWA))[2]);
    let legacy = grid(tables(&convert(IWORK09))[2]);
    assert_eq!(modern, legacy);
    let want: BTreeMap<(u64, u64), String> = [
        ((1, 1), "Test"),
        ((2, 2), "0.5"),
        ((2, 3), "0.1"),
        ((3, 3), "0.6"),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_string()))
    .collect();
    assert_eq!(modern, want);
}

#[test]
fn page_range_selects_sheets() {
    let doc = convert_with(
        DocumentConverter::new().page_range(2, 2),
        SourceDocument::from_file(source(IWA)).unwrap(),
    );
    let pages: Vec<&String> = doc["pages"].as_object().unwrap().keys().collect();
    assert_eq!(pages, ["2"]);
    let names: Vec<String> = tables(&doc).iter().map(|t| caption(&doc, t)).collect();
    assert_eq!(names, ["Table 1"]);
}

#[test]
fn zip_without_a_numbers_index_is_rejected() {
    let err = DocumentConverter::new()
        .convert(SourceDocument::from_bytes(
            "not_really.numbers",
            InputFormat::Numbers,
            zip_with(&[("word/document.xml", b"<w:document/>")]),
        ))
        .unwrap_err();
    assert!(
        err.to_string().contains("not a Numbers document"),
        "unexpected error: {err}"
    );
}

/// Numbers keeps no image of a chart, so the data it plots is what is read:
/// the summary table on the same sheet says whether it was read right.
#[test]
fn charts_carry_the_data_they_plot() {
    for name in BOTH {
        let doc = convert(name);
        let pictures = pictures(&doc);
        assert_eq!(pictures.len(), 1, "{name}");
        let mut plotted: Vec<String> = chart_grid(pictures[0])
            .into_iter()
            .flatten()
            .filter(|t| !t.is_empty())
            .collect();
        plotted.sort();
        let mut want = vec![
            "Amount",
            "Home",
            "Food",
            "Gas",
            "Credit Card",
            "Entertainment",
            "-872.4",
            "-226",
            "-137.5",
            "-1095",
            "-245",
        ];
        want.sort();
        assert_eq!(plotted, want, "{name}");
    }
}

/// The pie plots its rows as series in both generations: a wedge per row.
#[test]
fn both_generations_orient_the_chart_the_same_way() {
    let modern = chart_grid(pictures(&convert(IWA))[0]);
    let legacy = chart_grid(pictures(&convert(IWORK09))[0]);
    assert_eq!(modern, legacy);
    assert_eq!(
        legacy,
        [
            vec!["", "Home", "Food", "Gas", "Credit Card", "Entertainment"],
            vec!["Amount", "-872.4", "-226", "-137.5", "-1095", "-245"],
        ]
    );
}

#[test]
fn a_chart_is_classified_by_its_kind_and_captioned_with_its_title() {
    for name in BOTH {
        let doc = convert(name);
        let chart = pictures(&doc)[0];
        assert_eq!(
            chart["meta"]["classification"]["predictions"][0]["class_name"], "pie_chart",
            "{name}"
        );
        assert_eq!(caption(&doc, chart), "Expenditure by Category", "{name}");
    }
}

/// `sf:chart-type` follows Keynote '09's scripting dictionary, and
/// `sf:chart-direction` says which way round a chart plots: the 3D area chart
/// (direction 1) has a series per column, the column chart (0) per row.
#[test]
fn iwork09_charts_follow_their_kind_and_direction() {
    let doc = convert("numbers_iwork09_charts.numbers");
    let charts: Vec<(String, String)> = pictures(&doc)
        .into_iter()
        .map(|p| {
            (
                caption(&doc, p),
                p["meta"]["classification"]["predictions"][0]["class_name"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect();
    assert_eq!(
        charts,
        [
            ("Expenditure by Category".into(), "pie_chart".into()),
            ("Currency Chart name".into(), "other_chart".into()),
            ("Chart 2".into(), "bar_chart".into()),
        ]
    );
    let charts = pictures(&doc);
    let area = chart_grid(charts[1]);
    assert_eq!(area.len(), 10, "nine categories under the header row");
    assert_eq!(area[0].len(), 5, "four series beside the category column");
    assert_eq!(area[2][0], "average pay");
    assert_eq!(area[3][0], "maximum wage");
    assert_eq!(area[2][1..], ["", "0.5", "0.1", ""]);
    assert_eq!(area[3][1..], ["", "", "0.6", ""]);
    assert_eq!(
        chart_grid(charts[2]),
        [
            vec!["", "Region 1", "Region 2"],
            vec!["2007", "17", "55"],
            vec!["2008", "26", "43"],
            vec!["2009", "53", "70"],
            vec!["2010", "96", "58"],
        ]
    );
}

/// A sheet is a canvas: tables and charts are read in the order they sit
/// down it, whatever their kind.
#[test]
fn tables_and_charts_are_interleaved_down_the_sheet() {
    for name in BOTH {
        let doc = convert(name);
        let mut drawn: Vec<(usize, u64, f64)> = Vec::new();
        for (kind, items) in [("tables", tables(&doc)), ("pictures", pictures(&doc))] {
            for item in items {
                let index = doc["body"]["children"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|sheet| {
                        resolve(&doc, sheet)["children"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                    })
                    .position(|c| c["$ref"] == item["self_ref"])
                    .unwrap_or_else(|| panic!("{name}: a {kind} item outside its sheet"));
                let (page, top) = prov_top(item);
                drawn.push((index, page, top));
            }
        }
        drawn.sort_by_key(|d| d.0);
        assert_eq!(drawn.iter().filter(|d| d.1 == 1).count(), 3, "{name}");
        for page in [1, 2] {
            let tops: Vec<f64> = drawn.iter().filter(|d| d.1 == page).map(|d| d.2).collect();
            assert!(tops.windows(2).all(|w| w[0] <= w[1]), "{name}: {tops:?}");
        }
    }
}

#[test]
fn sticky_notes_become_comments() {
    for name in BOTH {
        let doc = convert(name);
        let groups: Vec<&str> = doc["groups"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|g| g["name"].as_str())
            .filter(|n| n.starts_with("comment-"))
            .collect();
        assert_eq!(groups, ["comment-Checking-1"], "{name}");
        let notes = notes(&doc);
        assert_eq!(notes.len(), 1, "{name}");
        assert!(
            notes[0].contains("drag an OFX file to the table"),
            "{name}: {notes:?}"
        );
    }
}

/// The 2013 container attributes a sticky note to an author and a moment;
/// iWork '09 recorded neither.
#[test]
fn a_comment_records_who_left_it_and_when() {
    assert!(notes(&convert(IWA))[0].starts_with("[author: Author, time: 2016-05-04T13:08:26"));
    assert!(notes(&convert(IWORK09))[0].starts_with("Try adding your own account transactions"));
}

/// A cell filled with an image becomes a rich cell: its group holds the text,
/// if any, then the picture, in the frame of the table.
#[test]
fn a_cell_filled_with_a_picture_becomes_a_rich_cell() {
    let doc = convert("numbers_cell_pictures.numbers");
    let tables = tables(&doc);
    assert_eq!(tables.len(), 1);
    let cells = cells(tables[0]);
    let icon = cell_items(&doc, cells[&(0, 0)]);
    assert_eq!(icon.len(), 1);
    let text_and_laptop = cell_items(&doc, cells[&(0, 1)]);
    assert_eq!(text_and_laptop.len(), 2);
    assert_eq!(text_and_laptop[0]["text"], "text ");
    assert_eq!(cells[&(0, 1)]["text"], "text ");
    for (picture, size) in [(icon[0], (452, 512)), (text_and_laptop[1], (370, 244))] {
        assert!(picture["self_ref"]
            .as_str()
            .unwrap()
            .starts_with("#/pictures/"));
        assert_eq!(image_size(picture), size);
        assert_eq!(picture["prov"][0]["page_no"], 1);
        assert_eq!(picture["prov"][0]["bbox"], tables[0]["prov"][0]["bbox"]);
    }
    assert!(!is_rich(cells[&(0, 2)]));
    assert_eq!(cells[&(0, 2)]["text"], "no image");
}

/// Only an image fill makes a picture; a colour fill does not.
#[test]
fn a_cell_filled_with_a_colour_holds_no_picture() {
    let doc = convert("numbers_cell_fills.numbers");
    let tables = tables(&doc);
    let cells = cells(tables[0]);
    let cat = cell_items(&doc, cells[&(0, 0)]);
    assert_eq!(cat.len(), 1);
    assert!(cat[0]["image"].is_object());
    assert!(!is_rich(cells[&(1, 1)]));
    assert_eq!(cells[&(1, 1)]["text"], "No Dog");
    assert_eq!(pictures(&doc).len(), 1);
}

/// All 50 cells are filled with the same photo, stored under a member name
/// that is UTF-8 without the flag saying so — docling finds it under its
/// code page 437 reading.
#[test]
fn a_photo_that_fills_every_cell_is_in_every_cell() {
    let doc = convert("numbers_cell_pictures_repeated.numbers");
    let tables = tables(&doc);
    let cells = cells(tables[0]);
    assert_eq!(cells.len(), 50);
    for cell in cells.values() {
        let items = cell_items(&doc, cell);
        assert_eq!(items.len(), 1);
        assert_eq!(image_size(items[0]), (940, 940));
    }
    let pictures = pictures(&doc);
    assert_eq!(pictures.len(), 50);
    assert!(pictures.iter().all(|p| p["image"].is_object()));
}

/// A rich cell keeps the header flags a plain cell in its place gets: on the
/// Headers sheet, photos fill header-row, header-column and footer cells.
#[test]
fn a_picture_in_a_header_cell_keeps_the_header() {
    let doc = convert("numbers_cell_pictures_styled.numbers");
    let headers = doc["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["label"] == "sheet" && g["name"] == "Headers")
        .expect("the Headers sheet");
    let table = headers["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| resolve(&doc, c))
        .find(|item| item["self_ref"].as_str().unwrap().starts_with("#/tables/"))
        .expect("its table");
    let rich: BTreeMap<(u64, u64), (bool, bool)> = cells(table)
        .into_iter()
        .filter(|(_, c)| is_rich(c))
        .map(|(k, c)| {
            (
                k,
                (
                    c["column_header"].as_bool().unwrap(),
                    c["row_header"].as_bool().unwrap(),
                ),
            )
        })
        .collect();
    let want: BTreeMap<(u64, u64), (bool, bool)> = [
        ((1, 3), (true, false)),
        ((1, 4), (true, false)),
        ((6, 1), (false, true)),
        ((6, 2), (false, true)),
        ((9, 3), (false, false)),
        ((9, 4), (false, false)),
    ]
    .into_iter()
    .collect();
    assert_eq!(rich, want);
    for (key, cell) in cells(table).into_iter().filter(|(_, c)| is_rich(c)) {
        let items = cell_items(&doc, cell);
        assert_eq!(items.len(), 2, "{key:?}");
        assert_eq!(items[0]["text"], cell["text"], "{key:?}");
        assert!(items[1]["image"].is_object(), "{key:?}");
    }
}

/// A date outside the range a datetime holds, or NaN — only a corrupt or
/// hostile document records one — leaves its cell empty and every other cell
/// as it was.
#[test]
fn a_date_no_datetime_can_hold_leaves_its_cell_empty() {
    let original = fs::read(source(IWORK09)).unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&original)).unwrap();
    let mut members: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..archive.len() {
        use std::io::Read;
        let mut file = archive.by_index(i).unwrap();
        let name = file.name().unwrap().to_string();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        if name == "index.xml" {
            let xml = String::from_utf8(bytes).unwrap();
            assert_eq!(xml.matches(r#"cell-date="276048000""#).count(), 1);
            assert_eq!(xml.matches(r#"cell-date="277257600""#).count(), 1);
            bytes = xml
                .replace(r#"cell-date="276048000""#, r#"cell-date="1e308""#)
                .replace(r#"cell-date="277257600""#, r#"cell-date="nan""#)
                .into_bytes();
        }
        members.push((name, bytes));
    }
    let members: Vec<(&str, &[u8])> = members
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    let damaged = convert_bytes("dates_outside_range.numbers", zip_with(&members));

    let mut expected: Vec<_> = tables(&convert(IWORK09)).into_iter().map(grid).collect();
    assert_eq!(
        expected[1].remove(&(2, 1)).as_deref(),
        Some("2009-10-01 00:00:00")
    );
    assert_eq!(
        expected[1].remove(&(3, 1)).as_deref(),
        Some("2009-10-15 00:00:00")
    );
    let damaged: Vec<_> = tables(&damaged).into_iter().map(grid).collect();
    assert_eq!(damaged, expected, "only the two bogus dates should be lost");
}

fn zip_with(members: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        for (name, bytes) in members {
            zip.start_file::<_, ()>(*name, Default::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    buf
}
