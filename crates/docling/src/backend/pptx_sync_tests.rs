//! The PPTX cases of the 2026-11 upstream sync (#1208), ported from
//! docling's `tests/test_backend_pptx*.py`: each test builds the deck
//! upstream's builds and asserts what upstream asserts, on the Markdown and
//! on docling's item tree (the JSON).

use serde_json::Value;

use std::io::Write;

use super::DeclarativeBackend;
use crate::format::InputFormat;
use crate::source::SourceDocument;

pub(super) const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
pub(super) const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
pub(super) const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const REL_SLIDE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";

/// A ZIP of `parts`, stored.
pub(super) fn package<C: AsRef<[u8]>>(parts: &[(&str, C)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, content) in parts {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(content.as_ref()).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

/// A one-slide deck: `shapes` is the slide's `p:spTree` content, `rels`
/// extra slide relationships, `parts` extra package parts.
pub(super) fn deck(shapes: &str, rels: &str, parts: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let presentation = format!(
        r#"<p:presentation xmlns:p="{P}" xmlns:r="{R}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>"#
    );
    let pres_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL_SLIDE}" Target="slides/slide1.xml"/></Relationships>"#
    );
    let slide = format!(
        r#"<p:sld xmlns:p="{P}" xmlns:a="{A}" xmlns:r="{R}"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{shapes}</p:spTree></p:cSld></p:sld>"#
    );
    let slide_rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#
    );
    let content_types = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="png" ContentType="image/png"/><Default Extension="xml" ContentType="application/xml"/></Types>"#;
    let mut all: Vec<(&str, Vec<u8>)> = vec![
        ("[Content_Types].xml", content_types.as_bytes().to_vec()),
        ("ppt/presentation.xml", presentation.into_bytes()),
        ("ppt/_rels/presentation.xml.rels", pres_rels.into_bytes()),
        ("ppt/slides/slide1.xml", slide.into_bytes()),
        ("ppt/slides/_rels/slide1.xml.rels", slide_rels.into_bytes()),
    ];
    all.extend(parts.iter().map(|(n, c)| (*n, c.clone())));
    package(&all)
}

/// A text box at `(x, y)` EMU holding one paragraph per entry.
pub(super) fn text_box(id: u32, x: i64, y: i64, paras: &[&str], extra_nv: &str) -> String {
    let paras: String = paras
        .iter()
        .map(|t| format!("<a:p><a:r><a:t>{t}</a:t></a:r></a:p>"))
        .collect();
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="TextBox {id}">{extra_nv}</p:cNvPr><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="2000000" cy="400000"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/>{paras}</p:txBody></p:sp>"#
    )
}

pub(super) fn convert(bytes: Vec<u8>) -> docling_core::DoclingDocument {
    let src = SourceDocument::from_bytes("t.pptx", InputFormat::Pptx, bytes);
    super::PptxBackend.convert(&src).expect("pptx converts")
}

pub(super) fn json(doc: &docling_core::DoclingDocument) -> Value {
    serde_json::from_str(&doc.export_to_json()).unwrap()
}

/// docling#5384 (#1164, `test_malformed_shape_hyperlink_keeps_the_slide_text`
/// and `…_does_not_stop_the_next_file`): a shape link `urlsplit` / `AnyUrl`
/// reject is dropped and the shape's text kept; a valid one stays.
#[test]
fn malformed_shape_hyperlink_keeps_the_slide_text() {
    let link = r#"<a:hlinkClick r:id="rIdL"/>"#;
    let rel = |target: &str| {
        format!(
            r#"<Relationship Id="rIdL" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{target}" TargetMode="External"/>"#
        )
    };
    for (target, want) in [
        ("https://[broken", "Link text"),
        ("https://broken]", "Link text"),
        ("https://[not-ipv6]/", "Link text"),
        (
            "https://example.org/ok",
            "[Link text](https://example.org/ok)",
        ),
    ] {
        let shapes = text_box(2, 0, 0, &["Link text"], link);
        let doc = convert(deck(&shapes, &rel(target), &[]));
        let md = doc.export_to_markdown();
        assert!(md.contains(want), "{target}: {md}");
        if want == "Link text" {
            assert!(!md.contains("]("), "{target}: {md}");
            let v = json(&doc);
            assert!(
                v["texts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|t| t["hyperlink"].is_null()),
                "{target}"
            );
        }
    }
}

// ----- Office 2016 charts (docling#5371, #1165, `test_backend_pptx_chartex.py`)

const CX: &str = "http://schemas.microsoft.com/office/drawing/2014/chartex";
const EMU_PER_INCH: f64 = 914_400.0;

/// A `cx:lvl`; an empty point gets no `cx:pt`, as for an empty cell.
fn level(points: &[&str], extra: &str) -> String {
    let pts: String = points
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.is_empty())
        .map(|(i, t)| format!(r#"<cx:pt idx="{i}">{t}</cx:pt>"#))
        .collect();
    format!(
        r#"<cx:lvl ptCount="{}"{extra}>{pts}</cx:lvl>"#,
        points.len()
    )
}

/// A category dimension; the levels go from the inner to the outer one.
fn categories(levels: &[&[&str]]) -> String {
    let lvls: String = levels.iter().map(|l| level(l, "")).collect();
    format!(r#"<cx:strDim type="cat">{lvls}</cx:strDim>"#)
}

fn values(values: &[&str], dim_type: &str) -> String {
    format!(
        r#"<cx:numDim type="{dim_type}">{}</cx:numDim>"#,
        level(values, r#" formatCode="General""#)
    )
}

fn series(layout: &str, name: Option<&str>, data_id: Option<&str>) -> String {
    let tx = name.map_or(String::new(), |n| {
        format!("<cx:tx><cx:txData><cx:v>{n}</cx:v></cx:txData></cx:tx>")
    });
    let data = data_id.map_or(String::new(), |d| format!(r#"<cx:dataId val="{d}"/>"#));
    format!(r#"<cx:series layoutId="{layout}">{tx}{data}</cx:series>"#)
}

fn chart_space(series: &str, dimensions: &str, title: Option<&str>, axes: &str) -> String {
    let title = title.map_or(String::new(), |t| {
        format!("<cx:title><cx:tx><cx:txData><cx:v>{t}</cx:v></cx:txData></cx:tx></cx:title>")
    });
    format!(
        r#"<cx:chartSpace xmlns:cx="{CX}" xmlns:a="{A}"><cx:chartData><cx:data id="0">{dimensions}</cx:data></cx:chartData><cx:chart>{title}<cx:plotArea><cx:plotAreaRegion>{series}</cx:plotAreaRegion>{axes}</cx:plotArea></cx:chart></cx:chartSpace>"#
    )
}

fn waterfall() -> String {
    chart_space(
        &series("waterfall", Some("Change"), Some("0")),
        &(categories(&[&["Opening", "Sales", "Costs", "Closing"]])
            + &values(&["100", "40.000000000000001", "-15", "125"], "val")),
        Some("Profit bridge"),
        r#"<cx:axis id="0"><cx:catScaling/><cx:title><cx:tx><cx:txData><cx:v>Step</cx:v></cx:txData></cx:tx></cx:title></cx:axis><cx:axis id="1"><cx:valScaling/></cx:axis>"#,
    )
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(w, h, image::Rgb([255, 0, 0]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[derive(Default)]
struct ChartDeck<'a> {
    fallback_picture: bool,
    group_xfrm: Option<&'a str>,
    hidden: bool,
    relationship_id: Option<&'a str>,
}

/// upstream's `_save_chartex_deck`: a text box, an Office 2016 chart at
/// (1 in, 1.5 in) sized 6 × 4 in in the `mc:Choice` of an
/// `mc:AlternateContent`, and a second text box.
fn chartex_deck(chart: &str, opts: ChartDeck) -> Vec<u8> {
    let inch = |v: f64| (v * EMU_PER_INCH) as i64;
    let hidden = if opts.hidden { r#" hidden="1""# } else { "" };
    let fallback = if opts.fallback_picture {
        r#"<p:pic><p:nvPicPr><p:cNvPr id="21" name="Picture"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rIdP"/></p:blipFill><p:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="381000" cy="190500"/></a:xfrm></p:spPr></p:pic>"#.to_string()
    } else {
        String::new()
    };
    let rid = opts.relationship_id.unwrap_or("rIdC");
    let alt = format!(
        r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice xmlns:cx1="http://schemas.microsoft.com/office/drawing/2015/9/8/chartex" Requires="cx1"><p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="20" name="Chart"{hidden}/><p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr><p:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></p:xfrm><a:graphic><a:graphicData uri="{CX}"><cx:chart xmlns:cx="{CX}" r:id="{rid}"/></a:graphicData></a:graphic></p:graphicFrame></mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent>"#,
        inch(1.0),
        inch(1.5),
        inch(6.0),
        inch(4.0),
    );
    let alt = match opts.group_xfrm {
        Some(xfrm) => format!(
            r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="30" name="Group"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr>{xfrm}</p:grpSpPr>{alt}</p:grpSp>"#
        ),
        None => alt,
    };
    let shapes = text_box(2, inch(1.0), inch(0.3), &["Before"], "")
        + &alt
        + &text_box(3, inch(1.0), inch(6.0), &["After"], "");
    let rels = r#"<Relationship Id="rIdC" Type="http://schemas.microsoft.com/office/2014/relationships/chartEx" Target="../charts/chartEx1.xml"/><Relationship Id="rIdP" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>"#;
    deck(
        &shapes,
        rels,
        &[
            ("ppt/charts/chartEx1.xml", chart.as_bytes().to_vec()),
            ("ppt/media/image1.png", png(40, 20)),
        ],
    )
}

/// The one picture of a deck.
fn only_picture(v: &Value) -> &Value {
    let pics = v["pictures"].as_array().unwrap();
    assert_eq!(pics.len(), 1, "{pics:?}");
    &pics[0]
}

fn grid(picture: &Value) -> Vec<Vec<String>> {
    picture["meta"]["tabular_chart"]["chart_data"]["grid"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            row.as_array()
                .unwrap()
                .iter()
                .map(|c| c["text"].as_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

fn labels(picture: &Value) -> Vec<String> {
    picture["meta"]["classification"]["predictions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["class_name"].as_str().unwrap().to_string())
        .collect()
}

fn bbox(picture: &Value) -> [f64; 4] {
    let b = &picture["prov"][0]["bbox"];
    ["l", "t", "r", "b"].map(|k| b[k].as_f64().unwrap())
}

fn rows(table: &docling_core::Table) -> Vec<Vec<&str>> {
    table
        .rows
        .iter()
        .map(|r| r.iter().map(String::as_str).collect())
        .collect()
}

/// `test_chart_keeps_its_kind_title_data_and_place`.
#[test]
fn chartex_keeps_its_kind_title_data_and_place() {
    let doc = convert(chartex_deck(&waterfall(), ChartDeck::default()));
    let md = doc.export_to_markdown();
    let (before, chart, after) = (
        md.find("Before").unwrap(),
        md.find("Profit bridge").unwrap(),
        md.find("After").unwrap(),
    );
    assert!(before < chart && chart < after, "{md}");
    let v = json(&doc);
    let picture = only_picture(&v);
    let caption = &picture["captions"][0]["$ref"];
    let caption_text = v["texts"][caption
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap()]["text"]
        .clone();
    assert_eq!(caption_text, "Profit bridge");
    assert_eq!(labels(picture), ["bar_chart"]);
    assert_eq!(
        grid(picture),
        [
            ["Step", "Change"],
            ["Opening", "100"],
            ["Sales", "40"],
            ["Costs", "-15"],
            ["Closing", "125"],
        ]
    );
    assert!(picture["image"].is_null());
    assert_eq!(
        bbox(picture),
        [1.0, 1.5, 7.0, 5.5].map(|i| i * EMU_PER_INCH)
    );
}

/// `test_fallback_picture_becomes_the_chart_image`.
#[test]
fn chartex_fallback_picture_becomes_the_chart_image() {
    let doc = convert(chartex_deck(
        &waterfall(),
        ChartDeck {
            fallback_picture: true,
            ..Default::default()
        },
    ));
    let v = json(&doc);
    let picture = only_picture(&v);
    let size = &picture["image"]["size"];
    assert_eq!(
        (size["width"].as_f64(), size["height"].as_f64()),
        (Some(40.0), Some(20.0))
    );
    assert_eq!(labels(picture), ["bar_chart"]);
}

/// `test_chart_in_a_group_is_placed_on_the_slide`: the group draws its
/// 8 × 6 in child space at half size from (1 in, 1 in).
#[test]
fn chartex_in_a_group_is_placed_on_the_slide() {
    let inch = EMU_PER_INCH as i64;
    let xfrm = format!(
        r#"<a:xfrm><a:off x="{inch}" y="{inch}"/><a:ext cx="{}" cy="{}"/><a:chOff x="0" y="0"/><a:chExt cx="{}" cy="{}"/></a:xfrm>"#,
        4 * inch,
        3 * inch,
        8 * inch,
        6 * inch
    );
    let doc = convert(chartex_deck(
        &waterfall(),
        ChartDeck {
            group_xfrm: Some(&xfrm),
            ..Default::default()
        },
    ));
    let v = json(&doc);
    let picture = only_picture(&v);
    assert_eq!(
        bbox(picture),
        [1.5, 1.75, 4.5, 3.75].map(|i| i * EMU_PER_INCH)
    );
    assert_eq!(grid(picture)[1], ["Opening", "100"]);
}

/// `test_hidden_chart_goes_to_invisible_layer`.
#[test]
fn chartex_hidden_goes_to_the_invisible_layer() {
    let v = json(&convert(chartex_deck(
        &waterfall(),
        ChartDeck {
            hidden: true,
            ..Default::default()
        },
    )));
    assert_eq!(only_picture(&v)["content_layer"], "invisible");
}

/// `test_unreadable_chart_still_gives_a_picture[broken-xml|missing-part]`.
#[test]
fn unreadable_chartex_still_gives_a_picture() {
    for (chart, rid) in [
        ("<cx:chartSpace".to_string(), None),
        (waterfall(), Some("rId99")),
    ] {
        let v = json(&convert(chartex_deck(
            &chart,
            ChartDeck {
                relationship_id: rid,
                ..Default::default()
            },
        )));
        let picture = only_picture(&v);
        assert_eq!(labels(picture), ["other_chart"], "{rid:?}");
        assert!(picture["meta"]["tabular_chart"].is_null(), "{rid:?}");
    }
}

/// `test_classification_follows_the_series_layouts`.
#[test]
fn chartex_classification_follows_the_series_layouts() {
    for (layouts, want) in [
        (&["waterfall"][..], &["bar_chart"][..]),
        (&["funnel"], &["bar_chart"]),
        (&["clusteredColumn"], &["bar_chart"]),
        (
            &["clusteredColumn", "paretoLine"],
            &["bar_chart", "line_chart"],
        ),
        (&["boxWhisker"], &["box_plot"]),
        (&["sunburst"], &["pie_chart"]),
        (&["treemap"], &["other_chart"]),
        (&["regionMap"], &["geographical_map"]),
        (&["someFutureLayout"], &["other_chart"]),
    ] {
        let ser: String = layouts
            .iter()
            .map(|l| series(l, Some("Sales"), Some("0")))
            .collect();
        let xml = chart_space(
            &ser,
            &(categories(&[&["A"]]) + &values(&["1"], "val")),
            None,
            "",
        );
        let chart = super::pptx_chartex::parse(&xml).unwrap();
        assert_eq!(chart.labels, want, "{layouts:?}");
    }
}

/// `test_hierarchical_categories_get_one_column_per_level`.
#[test]
fn chartex_hierarchical_categories_get_one_column_per_level() {
    let xml = chart_space(
        &series("treemap", Some("Revenue"), Some("0")),
        &(categories(&[&["Delhi", "Punjab", "Kerala"], &["North", "North", "South"]])
            + &values(&["4860", "1980", "1870"], "size")),
        None,
        "",
    );
    let table = super::pptx_chartex::parse(&xml).unwrap().table.unwrap();
    assert_eq!(
        rows(&table),
        [
            ["", "", "Revenue"],
            ["North", "Delhi", "4860"],
            ["North", "Punjab", "1980"],
            ["South", "Kerala", "1870"],
        ]
    );
    assert_eq!(
        table.structure.as_ref().unwrap().row_header[1],
        [true, true, false]
    );
}

/// `test_sparse_parent_labels_are_filled_from_the_row_above`.
#[test]
fn chartex_sparse_parent_labels_are_filled_from_the_row_above() {
    let xml = chart_space(
        &series("sunburst", Some("Sales"), Some("0")),
        &(categories(&[
            &["", "Wk.1", "Wk.2", "", ""],
            &["Jan", "Feb", "", "Mar", ""],
            &["1st", "", "", "", "2nd"],
        ]) + &values(&["3.5", "1.2", "0.8", "2", "5"], "size")),
        None,
        "",
    );
    let table = super::pptx_chartex::parse(&xml).unwrap().table.unwrap();
    assert_eq!(
        rows(&table),
        [
            ["", "", "", "Sales"],
            ["1st", "Jan", "", "3.5"],
            ["1st", "Feb", "Wk.1", "1.2"],
            ["1st", "Feb", "Wk.2", "0.8"],
            ["1st", "Mar", "", "2"],
            ["2nd", "", "", "5"],
        ]
    );
}

/// `test_values_without_categories_get_no_category_column`,
/// `test_derived_series_gets_no_column`, `test_point_indexes_do_not_size_the_table`
/// and `test_part_without_a_chart_has_no_data`.
#[test]
fn chartex_tables_are_sized_by_their_data() {
    let parse = |ser: &str, dims: &str| {
        super::pptx_chartex::parse(&chart_space(ser, dims, None, "")).unwrap()
    };
    let histogram = parse(
        &series("clusteredColumn", Some("Usage"), Some("0")),
        &values(&["17.399999999999999", "33", "9.0999999999999996"], "val"),
    );
    assert_eq!(
        rows(&histogram.table.unwrap()),
        [["Usage"], ["17.4"], ["33"], ["9.1"]]
    );

    let pareto = parse(
        &(series("clusteredColumn", Some("Complaints"), Some("0"))
            + &series("paretoLine", None, None)),
        &(categories(&[&["Coverage", "Billing"]]) + &values(&["412", "268"], "val")),
    );
    assert_eq!(
        rows(&pareto.table.unwrap()),
        [["", "Complaints"], ["Coverage", "412"], ["Billing", "268"]]
    );

    let lvl = r#"<cx:lvl ptCount="1000000000"><cx:pt idx="0">5</cx:pt><cx:pt idx="1">n/a</cx:pt><cx:pt idx="-1">7</cx:pt><cx:pt idx="x">8</cx:pt><cx:pt idx="999999999">9</cx:pt></cx:lvl>"#;
    let funnel = parse(
        &series("funnel", Some("Stage"), Some("0")),
        &format!(r#"<cx:numDim type="val">{lvl}</cx:numDim>"#),
    );
    assert_eq!(rows(&funnel.table.unwrap()), [["Stage"], ["5"], ["n/a"]]);

    let empty =
        super::pptx_chartex::parse(&format!(r#"<cx:chartSpace xmlns:cx="{CX}"/>"#)).unwrap();
    assert_eq!(empty.labels, ["other_chart"]);
    assert!(empty.title.is_none() && empty.table.is_none());
}

/// #1236: a deck whose main part is `ppt/deck.xml` — named by the
/// `officeDocument` relationship, its own relationships in
/// `ppt/_rels/deck.xml.rels` — converts like the conventional one, and is
/// detected as PPTX without an extension hint.
#[test]
fn presentation_main_part_named_by_relationship() {
    let conventional = deck(&text_box(2, 0, 0, &["Hello deck"], ""), "", &[]);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(conventional.clone())).unwrap();
    let mut parts: Vec<(String, Vec<u8>)> = (0..zip.len())
        .map(|i| {
            let mut f = zip.by_index(i).unwrap();
            let name = f.name().unwrap().replace("presentation.xml", "deck.xml");
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut f, &mut body).unwrap();
            (name, body)
        })
        .collect();
    parts.push((
        "_rels/.rels".into(),
        br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="R1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/deck.xml"/></Relationships>"#.to_vec(),
    ));
    let parts: Vec<(&str, Vec<u8>)> = parts.iter().map(|(n, c)| (n.as_str(), c.clone())).collect();
    let renamed = package(&parts);
    assert_eq!(crate::sniff::detect(&renamed), Some(InputFormat::Pptx));
    let md = convert(renamed).export_to_markdown();
    assert_eq!(md, convert(conventional).export_to_markdown());
    assert!(md.contains("Hello deck"), "{md}");
}
