//! AFP / MO:DCA (#1034, docling#4177): ports of upstream's
//! `tests/test_backend_afp.py` that go through the converter. The mirrored
//! `synthetic.afp` is pinned by the regression suite
//! (`tests/data/afp/mirror.txt`); here it is checked against upstream's
//! groundtruth itself. The structured-field and PTOCA unit cases live next
//! to the parser (`backend/afp.rs`).

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, InputFormat, SourceDocument};
use docling_core::Node;

fn corpus(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/data/afp")
        .join(rel)
}

const BDT: [u8; 3] = [0xD3, 0xA8, 0xA8];
const EDT: [u8; 3] = [0xD3, 0xA9, 0xA8];
const BPG: [u8; 3] = [0xD3, 0xA8, 0xAF];
const EPG: [u8; 3] = [0xD3, 0xA9, 0xAF];
const BPT: [u8; 3] = [0xD3, 0xA8, 0x9B];
const EPT: [u8; 3] = [0xD3, 0xA9, 0x9B];
const PTX: [u8; 3] = [0xD3, 0xEE, 0x9B];
const IPD: [u8; 3] = [0xD3, 0xEE, 0xFB];
const GAD: [u8; 3] = [0xD3, 0xEE, 0xBB];
const BPS: [u8; 3] = [0xD3, 0xA8, 0x5F];
const EPS: [u8; 3] = [0xD3, 0xA9, 0x5F];

/// One synthetic MO:DCA structured field — upstream's `_structured_field`.
fn sf(identifier: [u8; 3], data: &[u8], extension: &[u8], padding: usize) -> Vec<u8> {
    let mut flags = 0u8;
    let mut payload = Vec::new();
    if !extension.is_empty() {
        flags |= 0x01;
        payload.push(extension.len() as u8 + 1);
        payload.extend_from_slice(extension);
    }
    payload.extend_from_slice(data);
    if padding > 0 {
        flags |= 0x10;
        payload.extend(std::iter::repeat_n(0, padding - 1));
        payload.push(padding as u8);
    }
    let mut out = vec![0x5A];
    out.extend(((8 + payload.len()) as u16).to_be_bytes());
    out.extend(identifier);
    out.extend([flags, 0, 0]);
    out.extend(payload);
    out
}

fn field(identifier: [u8; 3]) -> Vec<u8> {
    sf(identifier, &[], &[], 0)
}

/// ASCII → cp500 for the letters, digits and punctuation the tests use.
fn cp500(text: &str) -> Vec<u8> {
    text.bytes()
        .map(|b| match b {
            b'a'..=b'i' => 0x81 + (b - b'a'),
            b'j'..=b'r' => 0x91 + (b - b'j'),
            b's'..=b'z' => 0xA2 + (b - b's'),
            b'A'..=b'I' => 0xC1 + (b - b'A'),
            b'J'..=b'R' => 0xD1 + (b - b'J'),
            b'S'..=b'Z' => 0xE2 + (b - b'S'),
            b'0'..=b'9' => 0xF0 + (b - b'0'),
            b' ' => 0x40,
            _ => panic!("no cp500 mapping in the test for {b:?}"),
        })
        .collect()
}

fn trn(text: &str) -> Vec<u8> {
    let encoded = cp500(text);
    let mut out = vec![0x2B, 0xD3, encoded.len() as u8 + 2, 0xDA];
    out.extend(encoded);
    out
}

fn amb(baseline: i16) -> Vec<u8> {
    let mut out = vec![0x2B, 0xD3, 0x04, 0xD2];
    out.extend(baseline.to_be_bytes());
    out
}

fn page(ptoca: &[Vec<u8>], include_image: bool) -> Vec<u8> {
    let mut out = [field(BPG), field(BPT)].concat();
    for part in ptoca {
        out.extend(sf(PTX, part, &[], 0));
    }
    if include_image {
        out.extend(sf(IPD, b"synthetic-image-payload", &[], 0));
    }
    out.extend(field(EPT));
    out.extend(field(EPG));
    out
}

fn document(parts: &[Vec<u8>]) -> Vec<u8> {
    [vec![field(BDT)], parts.to_vec(), vec![field(EDT)]]
        .concat()
        .concat()
}

fn convert(bytes: Vec<u8>) -> Result<docling_core::DoclingDocument, docling::ConversionError> {
    let source = SourceDocument::from_bytes("synthetic", InputFormat::Afp, bytes);
    DocumentConverter::new().convert(source).map(|r| r.document)
}

/// The text of every paragraph, with the page it sits on.
fn texts(doc: &docling_core::DoclingDocument) -> Vec<(usize, String)> {
    fn walk(nodes: &[Node], out: &mut Vec<(usize, String)>) {
        for node in nodes {
            match node {
                Node::Group { children, .. } => walk(children, out),
                Node::Prov { page_no, inner, .. } => {
                    if let Node::Paragraph { text } = inner.as_ref() {
                        out.push((*page_no, text.clone()));
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&doc.nodes, &mut out);
    out
}

/// `test_e2e_afp_conversion_matches_groundtruth`: Markdown and JSON against
/// upstream's groundtruth (the JSON without `origin` / `version`, which
/// carry the converter's own hash and schema version).
#[test]
fn synthetic_matches_upstream_groundtruth() {
    let Ok(source) = SourceDocument::from_file(corpus("sources/synthetic.afp")) else {
        return; // the corpus is not packaged with the crate
    };
    assert_eq!(source.format, InputFormat::Afp);
    let doc = DocumentConverter::new().convert(source).unwrap().document;
    let md = std::fs::read_to_string(corpus("groundtruth/synthetic.afp.md")).unwrap();
    assert_eq!(doc.export_to_markdown().trim_end(), md.trim_end());

    let strip = |mut v: serde_json::Value| {
        let o = v.as_object_mut().unwrap();
        o.remove("origin");
        o.remove("version");
        v
    };
    let gt: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(corpus("groundtruth/synthetic.afp.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(strip(doc.export_to_json_value()), strip(gt));
    // docling-core 2.106's own DocLang export divides by the `0 × 0` page
    // size and raises on this document; ours carries no locations, so it
    // prints the text with a page break between the two pages.
    let doclang = doc.export_to_doclang();
    for text in ["Hello AFP", "Second line", "Page two", "<page_break"] {
        assert!(doclang.contains(text), "{text}: {doclang}");
    }
}

/// `test_afp_is_detected_from_signature_without_extension`, plus the media
/// type and the extension.
#[test]
fn detected_by_signature_media_type_and_extension() {
    let Ok(bytes) = std::fs::read(corpus("sources/synthetic.afp")) else {
        return;
    };
    let by_bytes = SourceDocument::from_named_bytes("print-stream.bin", bytes.clone(), None);
    assert_eq!(by_bytes.unwrap().format, InputFormat::Afp);
    let by_bytes = SourceDocument::from_named_bytes("stdin", bytes.clone(), None);
    assert_eq!(by_bytes.unwrap().format, InputFormat::Afp);
    let by_mime =
        SourceDocument::from_named_bytes("stdin", bytes, Some("application/vnd.ibm.modcap"));
    assert_eq!(by_mime.unwrap().format, InputFormat::Afp);
    assert_eq!(InputFormat::from_extension("AFP"), Some(InputFormat::Afp));
    assert_eq!(InputFormat::from_id("afp"), Some(InputFormat::Afp));
    assert_eq!(InputFormat::Afp.as_str(), "afp");
}

/// `test_afp_page_range_keeps_original_page_number`.
#[test]
fn page_range_keeps_original_page_number() {
    let Ok(source) = SourceDocument::from_file(corpus("sources/synthetic.afp")) else {
        return;
    };
    let doc = DocumentConverter::new()
        .page_range(2, 2)
        .convert(source)
        .unwrap()
        .document;
    assert_eq!(texts(&doc), [(2, "Page two".to_string())]);
    let json = doc.export_to_json_value();
    let pages: Vec<&String> = json["pages"].as_object().unwrap().keys().collect();
    assert_eq!(pages, ["2"]);
}

/// `test_afp_structured_field_extension_and_padding_are_removed`.
#[test]
fn structured_field_extension_and_padding_are_removed() {
    let data = document(&[
        field(BPG),
        field(BPT),
        sf(PTX, &trn("Extended"), &[0xAA, 0xBB], 3),
        field(EPT),
        field(EPG),
    ]);
    assert_eq!(texts(&convert(data).unwrap()), [(1, "Extended".into())]);
}

/// `test_unsupported_afp_image_data_emits_clear_warning` /
/// `test_unsupported_warnings_are_aggregated_by_content_type`: skipped
/// content does not cost the text.
#[test]
fn unsupported_content_is_skipped() {
    let data = document(&[page(&[trn("Text remains")], true)]);
    assert_eq!(texts(&convert(data).unwrap()), [(1, "Text remains".into())]);
    let data = document(&[
        field(BPG),
        sf(IPD, b"one", &[], 0),
        sf(IPD, b"two", &[], 0),
        sf(GAD, b"graphics", &[], 0),
        field(EPG),
    ]);
    assert!(texts(&convert(data).unwrap()).is_empty());
}

/// `test_unsupported_afp_resource_emits_clear_warning` and
/// `test_presentation_text_outside_page_is_ignored`: presentation text
/// outside a page — a page segment's — is not page content.
#[test]
fn presentation_text_outside_a_page_is_ignored() {
    let data = document(&[
        field(BPS),
        field(BPT),
        sf(PTX, &trn("Resource text"), &[], 0),
        field(EPT),
        field(EPS),
        page(&[trn("Page text")], false),
    ]);
    assert_eq!(texts(&convert(data).unwrap()), [(1, "Page text".into())]);

    let data = document(&[sf(PTX, &trn("Outside"), &[], 0), field(BPG), field(EPG)]);
    let doc = convert(data).unwrap();
    assert!(texts(&doc).is_empty());
    assert_eq!(doc.export_to_json_value()["pages"]["1"]["page_no"], 1);
}

/// `test_afp_lines_placed_with_amb_become_separate_paragraphs`.
#[test]
fn lines_placed_with_amb_become_separate_paragraphs() {
    let data = document(&[page(
        &[[amb(100), trn("First"), amb(120), trn("Second")].concat()],
        false,
    )]);
    assert_eq!(
        texts(&convert(data).unwrap()),
        [(1, "First".into()), (1, "Second".into())]
    );
}

/// `test_malformed_structured_field_reports_offset` and the page-bracket
/// cases: malformed structure is a parse error naming where.
#[test]
fn malformed_structure_is_rejected() {
    for (data, message) in [
        (
            b"\x5a\x00\x20\xd3\xa8\xa8\x00\x00\x00".to_vec(),
            "byte 0 declares 32 bytes",
        ),
        (
            [field(BPG), field(BPG)].concat(),
            "before the preceding page ends",
        ),
        (field(EPG), "no matching Begin Page"),
        (field(BPG), "page 1 has no matching End Page"),
        (
            b"not an AFP stream".to_vec(),
            "valid MO:DCA structured field",
        ),
    ] {
        let err = convert(data).unwrap_err().to_string();
        assert!(err.contains(message), "{message}: {err}");
    }
}
