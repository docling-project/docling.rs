//! #253 (docling-core 2.88/2.89 parity): DCLX round-trip robustness. The
//! writer-side fixes (XML-illegal markers, CDATA splitting, heading clamp)
//! are unit-tested in docling-core; these tests pin the full write → read
//! loop through the archive, including the reader behaviors docling fixed in
//! docling-core#689/#695 — our single-pass roxmltree reader never re-parses
//! text fragments as XML, so tag-shaped literals must survive as text.

use docling::{DocumentConverter, SourceDocument};
use docling_core::{DoclingDocument, Node, Table};

fn roundtrip(doc: &DoclingDocument) -> DoclingDocument {
    let bytes = docling::dclx::to_dclx_bytes(doc);
    let src = SourceDocument::from_bytes("t", docling::InputFormat::Dclx, bytes);
    DocumentConverter::new()
        .convert(src)
        .expect("dclx round-trip")
        .document
}

/// A literal `]]>` in body text survives the CDATA-section split
/// (docling-core#689's writer half) and reads back verbatim.
#[test]
fn cdata_delimiter_text_round_trips() {
    let mut doc = DoclingDocument::new("t");
    doc.push(Node::Paragraph {
        text: "a]]>b & c <t>".into(),
    });
    let back = roundtrip(&doc);
    assert_eq!(back.export_to_markdown(), doc.export_to_markdown());
}

/// Cell text that merely looks like DocLang/OTSL markup — docling-core#695's
/// literal `<fcel>`, plus a `<location .../>`-shaped string — stays text
/// through the archive instead of being interpreted as structure.
#[test]
fn tag_shaped_cell_text_round_trips() {
    let mut doc = DoclingDocument::new("t");
    doc.push(Node::Table(Table {
        footnotes: Vec::new(),
        rows: vec![
            vec!["h1".into(), "h2".into()],
            vec!["<fcel>".into(), "<location value=\"3\"/> x".into()],
        ],
        location: None,
        structure: None,
        cell_blocks: None,
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    }));
    let back = roundtrip(&doc);
    let md = back.export_to_markdown();
    assert!(md.contains("&lt;fcel&gt;") || md.contains("<fcel>"), "{md}");
    assert_eq!(back.export_to_markdown(), doc.export_to_markdown());
}

/// XML-illegal control characters render as visible `[U+XXXX]` markers
/// (docling-core#687) — and, crucially, the archive stays parseable: the
/// round-trip must succeed rather than fail on invalid XML.
#[test]
fn xml_illegal_characters_round_trip_as_markers() {
    let mut doc = DoclingDocument::new("t");
    doc.push(Node::Paragraph {
        text: "break\u{0B}here".into(),
    });
    let back = roundtrip(&doc);
    assert!(
        back.export_to_markdown().contains("break[U+000B]here"),
        "{}",
        back.export_to_markdown()
    );
}

/// Deep headings clamp to level 6 (docling-core#688) and read back at 6.
#[test]
fn deep_headings_round_trip_clamped() {
    let mut doc = DoclingDocument::new("t");
    doc.push(Node::Heading {
        level: 42,
        text: "Deeper".into(),
    });
    let back = roundtrip(&doc);
    assert!(
        back.export_to_markdown().contains("###### Deeper"),
        "{}",
        back.export_to_markdown()
    );
}

/// Picture assets travel inside the archive as docling stores them: one
/// `assets/image_NNNNNN_<sha256>.png` part per `<src>`, PNG whatever the
/// source encoding (a JPEG re-encoded from its libjpeg pixels, a GIF through
/// the `image` crate), written between `_rels/` and `document.xml` — and the
/// reader resolves them back into the pictures.
#[test]
fn picture_assets_are_packaged_and_read_back() {
    let jpg = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../docling-pdf/tests/data/jpeg/rgb_420.jpg"),
    )
    .expect("libjpeg fixture");
    let mut gif = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(3, 2, image::Rgb([9, 99, 199])))
        .write_to(&mut gif, image::ImageFormat::Gif)
        .unwrap();
    let mut doc = DoclingDocument::new("t");
    for (mimetype, data) in [("image/jpeg", jpg), ("image/gif", gif.into_inner())] {
        let img = image::load_from_memory(&data).unwrap();
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image: Some(docling_core::PictureImage {
                mimetype: mimetype.into(),
                width: img.width(),
                height: img.height(),
                data,
                dpi: docling_core::PictureImage::DEFAULT_DPI,
                display_size: None,
            }),
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
    }
    let bytes = docling::dclx::to_dclx_bytes(&doc);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let names: Vec<String> = zip.file_names().flatten().map(|n| n.into_owned()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "lexicographic part order");
    let assets: Vec<&String> = names.iter().filter(|n| n.starts_with("assets/")).collect();
    assert_eq!(assets.len(), 2, "{names:?}");
    // rgb_420.jpg's digest is Pillow's: sha256(Image.open(f).tobytes()).
    assert_eq!(
        assets[0].as_str(),
        "assets/image_000000_50690f45a9c80a48b5e2e0e38008cd3ac26b8d92af24f2acddbdabcb6ec0c4c1.png"
    );
    assert!(assets[1].starts_with("assets/image_000001_"));
    for name in &assets {
        let mut part = Vec::new();
        std::io::Read::read_to_end(&mut zip.by_name(name).unwrap(), &mut part).unwrap();
        assert!(
            part.starts_with(b"\x89PNG\r\n\x1a\n"),
            "{name} is not a PNG"
        );
    }

    // The reader resolves `<src>` into the item tree (docling's
    // `media_root`): both pictures carry their PNG payload in the JSON.
    let json = roundtrip(&doc).export_to_json();
    assert_eq!(
        json.matches("data:image/png;base64,").count(),
        2,
        "assets resolve back into the pictures"
    );
}
