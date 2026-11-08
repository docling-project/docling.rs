//! Markdown against docling's own output, for the documents of upstream
//! tests that docling.rs ports: autolinks and GFM bare URLs keep their
//! hyperlink (#773, docling#4791); pictures survive a raw HTML block in the
//! same document (#760, docling#4777); GFM tables with rich cells and
//! `~~strikethrough~~` (docling#4488).
//!
//! `tests/data/md/upstream/<name>.{expected.md,json}` is **not** our output:
//! it is what docling 2.140.0's `MarkdownDocumentBackend` writes for
//! `<name>.md` (`.convert()`, then `export_to_markdown()` and
//! `export_to_dict()`). `autolinks.md` is upstream's
//! `test_autolinks_outside_table_cells` document, the `autolinks_*` others
//! probe the edges of marko's GFM URL rule, and the `pictures_*` documents
//! are `test_mixed_html_keeps_markdown_pictures`' (with upstream's red PNG
//! data URIs). `inline_and_formatting.md` is upstream's corpus document as of
//! 2.140 (its table of formatted cells), the `tables_*` documents probe where
//! marko's GFM tables start, end and sit and what makes a cell rich, and
//! `strikethrough.md` the edges of docling's `~~` rule. Re-create a reference
//! with upstream's backend, never from our own output. One byte moved by
//! hand: `autolinks_edge_cases`' `(https://example.com/a_%28b%29)` is
//! docling-core 2.104's `markdown_link_destination` (#860), which the
//! 2.140.0 capture predates.
//!
//! Recaptured with docling 2.148.0 / docling-core 2.106.0 for the inline
//! whitespace contract (#997): runs keep their boundary whitespace and
//! concatenate. One edit by hand: `tables_plain`'s paragraph right before
//! its table keeps marko's two trailing hard-break spaces upstream
//! (`"Line one  "`, a block edge marko leaves untrimmed there); ours trims
//! the block edge, as for every other paragraph.
use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};
use serde_json::Value;

/// Documents whose JSON matches docling's but whose Markdown keeps a known,
/// older difference of the flat Markdown path: a link inside emphasis
/// (`*see https://x.y/w now*`) — docling serializes each run of the inline
/// group as its own item (`*see* [*https://x.y/w*](https://x.y/w) *now*`),
/// the flat path keeps the emphasis whole around the link, for an ordinary
/// `[label](url)` as much as for an autolink.
///
/// And three of the flat path's tables: a formatted cell prints its plain
/// text, where docling serializes the rich cell's runs (`**C** Cadre`,
/// `` `c` ``, `~~s~~`, a link's URL normalized); a table inside a list item
/// is not printed at all.
const MARKDOWN_KNOWN_DIFFERENCE: &[&str] = &[
    "autolinks_emphasis.md",
    "inline_and_formatting.md",
    "tables_nested.md",
    "tables_rich.md",
];

/// Documents with a raw HTML block, whose Markdown matches docling's but
/// whose JSON does not as a whole: docling converts such a document through
/// its HTML backend, which nests everything under the HTML heading; here it
/// keeps the flat export (docs/MIGRATION.md), and a table cell's picture is
/// no item of its own. [`an_html_block_keeps_the_markdown_pictures`] checks
/// the pictures in it.
const JSON_KNOWN_DIFFERENCE: &[&str] = &[
    "pictures_html_heading.md",
    "pictures_html_no_caption.md",
    "pictures_html_table.md",
    "pictures_html_table_cell.md",
];

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/md/upstream")
}

/// Drop what never depends on the conversion: the file's origin (hash, path)
/// and the schema version stamp.
fn normalize(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("origin");
            m.remove("version");
            m.values_mut().for_each(normalize);
        }
        Value::Array(a) => a.iter_mut().for_each(normalize),
        _ => {}
    }
}

/// docling.rs keeps a picture's alt text as its description (#676,
/// `created_by: "alt_text"`); docling does not. a picture's alt text is the one
/// deliberate difference these documents can show.
fn drop_alt_text(doc: &mut Value) {
    for pic in doc["pictures"].as_array_mut().into_iter().flatten() {
        let alt = pic["meta"]["description"]["created_by"] == "alt_text";
        if alt {
            pic.as_object_mut().unwrap().remove("meta");
            pic["annotations"]
                .as_array_mut()
                .unwrap()
                .retain(|a| a["provenance"] != "alt_text");
        }
    }
}

fn convert(name: &str) -> docling_core::DoclingDocument {
    let path = dir().join(name);
    DocumentConverter::new()
        .convert(SourceDocument::from_file(&path).expect("document"))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .document
}

/// Every document's Markdown and JSON item tree equal docling's.
#[test]
fn documents_follow_docling() {
    let mut docs: Vec<String> = fs::read_dir(dir())
        .expect("md/upstream fixtures")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            name.ends_with(".md") && !name.ends_with(".expected.md")
        })
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    docs.sort();
    assert!(!docs.is_empty());
    let mut failures = Vec::new();
    for name in &docs {
        let document = convert(name);
        let base = dir().join(name);
        let want_md =
            fs::read_to_string(base.with_extension("expected.md")).expect("reference .expected.md");
        let got_md = document.export_to_markdown();
        if got_md.trim_end() != want_md.trim_end()
            && !MARKDOWN_KNOWN_DIFFERENCE.contains(&name.as_str())
        {
            failures.push(format!(
                "{name} md:\n--- docling\n{want_md}\n--- ours\n{got_md}"
            ));
        }
        let mut want: Value =
            serde_json::from_str(&fs::read_to_string(base.with_extension("json")).unwrap())
                .unwrap();
        let mut got: Value = serde_json::from_str(&document.export_to_json()).unwrap();
        normalize(&mut want);
        normalize(&mut got);
        drop_alt_text(&mut got);
        if got != want && !JSON_KNOWN_DIFFERENCE.contains(&name.as_str()) {
            failures.push(format!("{name} json: item tree differs from docling's"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The pictures of a converted document, in body order: each one's caption
/// text, image size (when it carries one), and the body positions of the
/// picture and of the texts around it.
fn pictures(json: &Value) -> Vec<(String, Option<(u64, u64)>)> {
    let texts = json["texts"].as_array().unwrap();
    json["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let caption = p["captions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    let k: usize = r["$ref"].as_str().unwrap()["#/texts/".len()..]
                        .parse()
                        .unwrap();
                    texts[k]["text"].as_str().unwrap().to_string()
                })
                .collect::<Vec<_>>()
                .join(" ");
            let size = p["image"]["size"].as_object().map(|s| {
                (
                    s["width"].as_f64().unwrap() as u64,
                    s["height"].as_f64().unwrap() as u64,
                )
            });
            (caption, size)
        })
        .collect()
}

/// docling#4777's `test_mixed_html_keeps_markdown_pictures` (#760): with a
/// raw HTML block (a heading, a table) or none, both Markdown pictures stay
/// pictures, in order, captioned by their titles; they carry their pixels
/// only when the image policy resolves `data:` URIs.
#[test]
fn an_html_block_keeps_the_markdown_pictures() {
    for (name, first) in [
        ("pictures_plain.md", "First picture"),
        ("pictures_html_heading.md", "First picture"),
        ("pictures_html_table.md", "First picture"),
        ("pictures_html_no_caption.md", ""),
    ] {
        for embedded in [false, true] {
            let sources = if embedded {
                docling::ImageSources::Embedded
            } else {
                docling::ImageSources::None
            };
            let doc = DocumentConverter::new()
                .image_sources(sources)
                .convert(SourceDocument::from_file(dir().join(name)).expect("document"))
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .document;
            let json: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
            let sizes = |a, b| {
                if embedded {
                    [Some(a), Some(b)]
                } else {
                    [None, None]
                }
            };
            let [one, two] = sizes((7, 5), (3, 2));
            assert_eq!(
                pictures(&json),
                [
                    (first.to_string(), one),
                    ("Second picture".to_string(), two)
                ],
                "{name}, embedded: {embedded}"
            );
            // Reading order: the texts around each picture.
            let md = doc.export_to_markdown();
            let at = |s: &str| {
                md.find(s)
                    .unwrap_or_else(|| panic!("{name}: {s:?} in\n{md}"))
            };
            let images: Vec<usize> = md.match_indices("<!-- image -->").map(|(k, _)| k).collect();
            if !embedded {
                assert_eq!(images.len(), 2, "{name}:\n{md}");
                assert!(
                    at("Before picture.") < images[0]
                        && images[0] < at("Between pictures.")
                        && at("Between pictures.") < images[1]
                        && images[1] < at("After picture."),
                    "{name}:\n{md}"
                );
            }
        }
    }
    // A picture in a table cell prints with its title caption.
    let md = convert("pictures_html_table_cell.md").export_to_markdown();
    assert!(md.contains("| cell picture  <!-- image --> |"), "{md}");
}
