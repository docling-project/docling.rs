//! `export_to_html` against the HTML groundtruth upstream docling ships for
//! its fixtures (#492): every `tests/data/<fmt>/groundtruth/<name>.html` in
//! the mirrored corpus is byte-for-byte what `DoclingDocument.export_to_html(
//! image_mode=EMBEDDED)` produced for `sources/<name>`, so the port is scored
//! the way the Markdown and JSON exports are. The content-layer selection
//! (#499) is scored the same way against `tests/data/html_layers/` in this
//! crate: docling-core 2.103.1's `export_to_html(included_content_layers=…)`
//! run over the upstream groundtruth JSON of the same fixtures.
use std::fs;
use std::path::{Path, PathBuf};

use docling::{ContentLayers, DocumentConverter, HtmlExportOptions, ImageMode, SourceDocument};
use rayon::prelude::*;

/// `src="data:<mime>;base64,<payload>"` → `src="data:…"`: the payload and
/// its media type both come from the JSON export's picture bytes.
fn mask_data_uris(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find("\"data:") {
        let start = i + "\"data:".len();
        let end = rest[start..]
            .find('"')
            .map(|e| start + e)
            .unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        out.push('…');
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Drop each masked `<img src="data:…">` and the `<figure></figure>` left
/// empty by it, with the line break the serializer put before it.
fn drop_embedded_images(html: &str) -> String {
    html.replace("<img src=\"data:…\">", "")
        .replace("\n<figure></figure>", "")
        .replace("<figure></figure>", "")
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data")
}

#[test]
fn html_matches_upstream_groundtruth() {
    let corpus = corpus();
    let mut cases: Vec<(PathBuf, PathBuf)> = Vec::new();
    for fmt in fs::read_dir(&corpus).expect("corpus") {
        let fmt = fmt.expect("entry").path();
        let gt_dir = fmt.join("groundtruth");
        let Ok(entries) = fs::read_dir(&gt_dir) else {
            continue;
        };
        for e in entries.flatten() {
            let gt = e.path();
            if gt.extension().and_then(|x| x.to_str()) != Some("html") {
                continue;
            }
            let stem = gt.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let src = fmt.join("sources").join(stem);
            if src.is_file() {
                cases.push((src, gt));
            }
        }
    }
    cases.sort();
    assert!(
        !cases.is_empty(),
        "no HTML groundtruth under {}",
        corpus.display()
    );

    let converter = DocumentConverter::new();
    // Every fixture with HTML groundtruth, converted once more here (the
    // regression suite pins our own HTML): in parallel, the verdicts in
    // the cases' sorted order.
    let failures: Vec<String> = cases
        .par_iter()
        .filter_map(|(src, gt)| {
            let name = src.file_name().unwrap().to_string_lossy().into_owned();
            let source = SourceDocument::from_file(src).expect("read source");
            let doc = match converter.convert(source) {
                Ok(r) => r.document,
                Err(e) => return Some(format!("{name}: convert error: {e}")),
            };
            let want = fs::read_to_string(gt).expect("read groundtruth");
            let (got, _) = doc.export_to_html_with_images(ImageMode::Embedded, "artifacts");
            // Picture payloads are the JSON export's business (upstream's
            // `save_as_html` re-encodes every picture through PIL as PNG, so
            // neither its bytes nor its media type are the file's): compare the
            // markup with the embedded `<img>`s dropped, the trailing newline
            // aside. Dropped, not masked: references written since docling-core
            // 2.103 leave out the image of a picture its local-image opt-in did
            // not load (`docx_rich_cells`' two cell pictures, whose JSON
            // carries them), so only the markup around a picture is compared.
            let (got, want) = (
                drop_embedded_images(&mask_data_uris(got.trim_end())),
                drop_embedded_images(&mask_data_uris(want.trim_end())),
            );
            if got != want {
                if let Some(dir) = std::env::var_os("DOCLING_RS_HTML_DUMP") {
                    let dir = Path::new(&dir);
                    fs::create_dir_all(dir).unwrap();
                    fs::write(dir.join(format!("{name}.got.html")), &got).unwrap();
                    fs::write(dir.join(format!("{name}.want.html")), &want).unwrap();
                }
                let first = got
                    .lines()
                    .zip(want.lines())
                    .enumerate()
                    .find(|(_, (a, b))| a != b)
                    .map(|(i, (a, b))| {
                        format!("line {}:\n  got:  {:.200}\n  want: {:.200}", i + 1, a, b)
                    })
                    .unwrap_or_else(|| {
                        format!("lengths differ: got {} want {}", got.len(), want.len())
                    });
                return Some(format!("{name}: {first}"));
            }
            None
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} HTML groundtruth mismatch(es):\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// `export_to_html_with_layers` (#499) against docling-core's own output for
/// the extra layers: every `tests/data/html_layers/<source>.<layers>.html`
/// (`<layers>` = `+`-joined layer names, or `all`) is what docling-core 2.99's
/// `DoclingDocument.export_to_html(image_mode=EMBEDDED,
/// included_content_layers={…})` produced over the upstream groundtruth
/// JSON of the DOCX fixture `<source>`, whose body-parented page headers /
/// footers (furniture) and reviewer comments (notes) the default export
/// leaves out.
#[test]
fn html_layers_match_docling_core() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/html_layers");
    let corpus = corpus();
    let mut cases: Vec<(PathBuf, PathBuf, ContentLayers)> = Vec::new();
    for e in fs::read_dir(&dir).expect("html_layers fixtures").flatten() {
        let want = e.path();
        let name = want.file_name().unwrap().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".html") else {
            continue;
        };
        let (source, layers) = stem.rsplit_once('.').expect("<source>.<layers>.html");
        let layers = ContentLayers::parse_list(&layers.replace('+', ",")).expect("layer names");
        let ext = Path::new(source)
            .extension()
            .and_then(|x| x.to_str())
            .expect("source extension");
        let src = corpus.join(ext).join("sources").join(source);
        assert!(src.is_file(), "no upstream source for {name}");
        cases.push((src, want, layers));
    }
    cases.sort_by(|a, b| a.1.cmp(&b.1));
    assert!(!cases.is_empty(), "no fixtures under {}", dir.display());

    let converter = DocumentConverter::new();
    let mut failures = Vec::new();
    for (src, want_path, layers) in &cases {
        let name = want_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let source = SourceDocument::from_file(src).expect("read source");
        let doc = converter.convert(source).expect("convert").document;
        let (got, _) = doc.export_to_html_with(&HtmlExportOptions {
            image_mode: ImageMode::Embedded,
            layers: *layers,
            ..HtmlExportOptions::default()
        });
        let want = fs::read_to_string(want_path).expect("read expected");
        let (got, want) = (
            mask_data_uris(got.trim_end()),
            mask_data_uris(want.trim_end()),
        );
        if got != want {
            let first = got
                .lines()
                .zip(want.lines())
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .map(|(i, (a, b))| {
                    format!("line {}:\n  got:  {:.200}\n  want: {:.200}", i + 1, a, b)
                })
                .unwrap_or_else(|| {
                    format!("lengths differ: got {} want {}", got.len(), want.len())
                });
            failures.push(format!("{name}: {first}"));
        }
        // The default export of the same document stays body-only.
        if *layers != ContentLayers::BODY {
            let body_only = doc.export_to_html();
            assert_ne!(
                mask_data_uris(&body_only),
                got,
                "{name}: the extra layers changed nothing"
            );
            assert_eq!(
                body_only,
                doc.export_to_html_with_layers(ContentLayers::BODY)
            );
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} layer export mismatch(es):\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

/// #860: a hyperlink with an active scheme keeps its text and loses the
/// link on every export but the JSON (docling-core 2.104's
/// `is_safe_hyperlink`); a live destination is percent-escaped in Markdown
/// and attribute-escaped in HTML, so neither can end the link early.
#[test]
fn unsafe_hyperlinks_are_text_and_destinations_are_escaped() {
    let html = r#"<p>One <a href="javascript:alert(1)">click</a> two
<a href=" JAVA&#10;SCRIPT:alert(2)">spaced</a> three
<a href="/wiki/Duck_(film)?a=1&amp;b=2 c">paren</a> four
<a href="data:text/html;base64,PHNjcmlwdD4=">data</a>.</p>"#;
    let source = SourceDocument::from_bytes("links.html", docling::InputFormat::Html, html.into());
    let doc = DocumentConverter::new()
        .convert(source)
        .expect("convert")
        .document;
    let md = doc.export_to_markdown();
    assert!(md.contains("One click two"), "{md}");
    assert!(md.contains("spaced three"), "{md}");
    assert!(
        md.contains("[paren](/wiki/Duck_%28film%29?a=1&b=2%20c)"),
        "{md}"
    );
    assert!(md.contains("four data"), "{md}");
    assert!(!md.contains("javascript:") && !md.contains("data:"), "{md}");
    let out = doc.export_to_html();
    assert!(
        !out.contains("<a href=\"javascript") && !out.contains("<a href=\"data:"),
        "{out}"
    );
    assert!(!out.contains("<a href=\" JAVA"), "{out}");
    assert!(
        out.contains("<a href=\"/wiki/Duck_(film)?a=1&amp;b=2 c\">paren</a>"),
        "{out}"
    );
    // The JSON keeps the raw destination, as docling's model does.
    let json = doc.export_to_json();
    assert!(json.contains("javascript:alert(1)"), "{json}");
}
