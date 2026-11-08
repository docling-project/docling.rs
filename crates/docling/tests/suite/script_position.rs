//! #1005: superscript and subscript marking in the PDF pipeline — docling's
//! `tests/test_script_position_pdf.py`, on its fixture
//! (`tests/fixtures/script_position/script_position.pdf`, exported from
//! Word-style formatting by LibreOffice, with an embedded font) and on the
//! one-page Helvetica PDFs it writes for the rotated, cropped and invisible
//! cases. Needs the layout model; skips without it so CI without models stays
//! green.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, InputFormat, SourceDocument};
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn ml_stack_ready() -> bool {
    let root = repo_root();
    root.join(".models/layout_heron.onnx").exists() && std::env::set_current_dir(&root).is_ok()
}

fn fixture() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/script_position/script_position.pdf"),
    )
    .expect("fixture")
}

fn convert(bytes: Vec<u8>, enabled: bool) -> docling_core::DoclingDocument {
    DocumentConverter::new()
        .no_ocr(true)
        .no_table_former(true)
        .script_positions(enabled)
        .convert(SourceDocument::from_bytes(
            "page.pdf",
            InputFormat::Pdf,
            bytes,
        ))
        .expect("conversion")
        .document
}

/// The runs of every inline group in the JSON, as `(text, script)`.
fn runs(doc: &docling_core::DoclingDocument) -> Vec<Vec<(String, String)>> {
    let json: Value = serde_json::from_str(&doc.export_to_json()).expect("json");
    let texts = json["texts"].as_array().expect("texts");
    json["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .filter(|g| g["label"] == "inline")
        .map(|g| {
            g["children"]
                .as_array()
                .expect("children")
                .iter()
                .map(|c| {
                    let idx: usize = c["$ref"].as_str().unwrap()["#/texts/".len()..]
                        .parse()
                        .unwrap();
                    let item = &texts[idx];
                    let script = item["formatting"]["script"].as_str().unwrap_or("baseline");
                    (
                        item["text"].as_str().unwrap().to_string(),
                        script.to_string(),
                    )
                })
                .collect()
        })
        .collect()
}

/// The raised and lowered runs, trimmed.
fn scripts(runs: &[Vec<(String, String)>]) -> Vec<(String, String)> {
    runs.iter()
        .flatten()
        .filter(|(_, s)| s != "baseline")
        .map(|(t, s)| (t.trim().to_string(), s.clone()))
        .collect()
}

fn pairs(want: &[(&str, &str)]) -> Vec<(String, String)> {
    want.iter()
        .map(|(t, s)| (t.to_string(), s.to_string()))
        .collect()
}

#[test]
fn marks_superscript_and_subscript_runs() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let doc = convert(fixture(), true);
    let runs = runs(&doc);
    assert_eq!(
        scripts(&runs),
        pairs(&[
            ("2", "super"),
            ("2", "sub"),
            ("2", "sub"),
            ("st", "super"),
            ("rd", "super"),
            ("[12]", "super"),
        ])
    );
    // The PDF has no space before the "2", and a space after it.
    assert_eq!(runs[0][0].0, "Einstein wrote E = mc");
    assert_eq!(runs[0][1], ("2".to_string(), "super".to_string()));
    assert!(runs[0][2].0.starts_with(" in 1905."), "{:?}", runs[0][2]);
    // A line without scripts stays a plain text item.
    let md = doc.export_to_markdown();
    assert!(md.contains("Plain line"), "{md}");
}

#[test]
fn exports_keep_the_spaces_of_the_pdf() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let doc = convert(fixture(), true);
    let html = doc.export_to_html();
    assert!(
        html.contains("Water is H<sub>2</sub>O and carbon dioxide is CO<sub>2</sub>."),
        "{html}"
    );
    assert!(
        html.contains("On the 21<sup>st</sup> of June the 3<sup>rd</sup> team won."),
        "{html}"
    );
    let md = doc.export_to_markdown();
    assert!(
        md.contains("Water is H2O and carbon dioxide is CO2."),
        "{md}"
    );
    assert!(md.contains("On the 21st of June the 3rd team won."), "{md}");
}

#[test]
fn disabled_leaves_the_document_unchanged() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let doc = convert(fixture(), false);
    assert!(runs(&doc).is_empty());
    assert!(doc.export_to_markdown().contains("E = mc 2 in 1905."));
}

/// One paragraph in Helvetica, as (text, size, rise) runs per line.
const LINES: &[&[(&str, u32, i32)]] = &[
    &[
        ("Water is H", 11, 0),
        ("2", 7, -2),
        ("O and carbon dioxide is CO", 11, 0),
        ("2", 7, -2),
        (" in the air", 11, 0),
    ],
    &[
        ("we can see that E = mc", 11, 0),
        ("2", 7, 4),
        (" holds on the 21", 11, 0),
        ("st", 7, 4),
        (" of June", 11, 0),
    ],
    &[
        ("as reported before", 11, 0),
        ("12", 7, 4),
        (" and confirmed by the team today", 11, 0),
    ],
];

/// docling's `_write_pdf`: a one-page PDF of [`LINES`] with the given page
/// box entries and text rendering mode.
fn write_pdf(page: &str, render_mode: u32) -> Vec<u8> {
    let mut ops = vec![
        "BT".to_string(),
        format!("{render_mode} Tr"),
        "13 TL".into(),
        "72 700 Td".into(),
    ];
    for (n, line) in LINES.iter().enumerate() {
        if n > 0 {
            ops.push("T*".into());
        }
        for (text, size, rise) in line.iter() {
            ops.push(format!("/F1 {size} Tf {rise} Ts ({text}) Tj"));
        }
    }
    ops.push("0 Ts".into());
    ops.push("ET".into());
    let stream = ops.join("\n");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R {page} /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> >>"
        ),
        format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[test]
fn matches_characters_on_rotated_and_cropped_pages() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    for page in [
        "/MediaBox [0 0 612 792] /Rotate 90",
        // The visible page does not start at the origin of the PDF coordinates.
        "/MediaBox [0 0 700 900] /CropBox [40 60 652 852]",
    ] {
        let doc = convert(write_pdf(page, 0), true);
        assert_eq!(
            scripts(&runs(&doc)),
            pairs(&[
                ("2", "sub"),
                ("2", "sub"),
                ("2", "super"),
                ("st", "super"),
                ("12", "super"),
            ]),
            "{page}"
        );
    }
}

/// An invisible text layer, such as OCR over a scan, has no typeset
/// baseline.
#[test]
fn leaves_invisible_text_unchanged() {
    if !ml_stack_ready() {
        eprintln!("skipping: models not found");
        return;
    }
    let doc = convert(write_pdf("/MediaBox [0 0 612 792]", 3), true);
    assert!(doc
        .export_to_markdown()
        .contains("confirmed by the team today"));
    assert!(scripts(&runs(&doc)).is_empty());
}
