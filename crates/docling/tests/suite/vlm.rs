//! e2e for issue #77: the remote-VLM pipeline against a local mock of an
//! OpenAI-compatible `chat/completions` endpoint.
//!
//! The PDF test renders real pages (the pure-Rust renderer, no ONNX models),
//! so it runs everywhere. The mock
//! asserts the wire shape (model name, data-URI page image) and returns
//! DocLang wrapped the way real models wrap answers (fences, full roots),
//! exercising the normalization path end to end.
#![cfg(feature = "vlm")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use docling::vlm::{convert_vlm, convert_vlm_result, VlmOptions};
use docling::{ConversionStatus, InputFormat, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One canned answer of [`mock_replies`]: an HTTP status, and for a 200 the
/// completion's `content` and `finish_reason`.
struct Reply {
    status: u16,
    content: String,
    finish_reason: &'static str,
}

impl Reply {
    fn ok(content: impl Into<String>) -> Self {
        Reply {
            status: 200,
            content: content.into(),
            finish_reason: "stop",
        }
    }
}

/// Minimal HTTP/1.1 server: for each expected request, read head + body,
/// run the assertions, respond with a canned chat completion whose `content`
/// comes from `answers`. Returns (base_url, served-count handle, join handle).
fn mock_openai(answers: Vec<String>) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
    mock_replies(answers.into_iter().map(Reply::ok).collect())
}

/// [`mock_openai`] with a status and finish reason per answer.
fn mock_replies(replies: Vec<Reply>) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&served);
    let handle = std::thread::spawn(move || {
        for reply in replies {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // Read until the full body arrived (Content-Length framing).
            let body_len = loop {
                let n = conn.read(&mut tmp).expect("read request");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(head_end) = find(&buf, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let need: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .expect("content-length");
                    if buf.len() >= head_end + 4 + need {
                        break need;
                    }
                }
            };
            let head_end = find(&buf, b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let body =
                String::from_utf8_lossy(&buf[head_end + 4..head_end + 4 + body_len]).into_owned();
            // Wire-shape assertions: right path, model, prompt, page image.
            assert!(
                head.starts_with("POST /v1/chat/completions"),
                "path: {head}"
            );
            assert!(body.contains("\"model\":\"mock-docling\""), "model missing");
            assert!(
                body.contains("Convert this page to docling."),
                "prompt missing"
            );
            assert!(
                body.contains("data:image/png;base64,"),
                "page image missing"
            );
            let payload = if reply.status == 200 {
                serde_json::json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": reply.content },
                        "finish_reason": reply.finish_reason,
                    }]
                })
                .to_string()
            } else {
                serde_json::json!({ "error": { "message": "bad page" } }).to_string()
            };
            let resp = format!(
                "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                reply.status,
                payload.len(),
                payload
            );
            conn.write_all(resp.as_bytes()).expect("write response");
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
    (format!("http://{addr}/v1"), served, handle)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn opts(endpoint: String) -> VlmOptions {
    VlmOptions {
        endpoint,
        model: "mock-docling".into(),
        prompt: None,
        api_key: None,
        page_range: None,
        max_tokens: 8192,
    }
}

#[test]
fn vlm_converts_pdf_pages_through_the_endpoint() {
    // Two pages, two differently-wrapped answers — fenced and full-root — so
    // normalization is exercised on real responses, then stitched in order.
    let (endpoint, served, handle) = mock_openai(vec![
        "```xml\n<heading level=\"1\">Page One</heading>\n<text>First body.</text>\n```".into(),
        "<doclang version=\"0.7\"><text>Second body.</text></doclang>".into(),
    ]);
    let source =
        SourceDocument::from_file(repo_root().join("tests/data/pdf/sources/2206.01062.pdf"))
            .expect("pdf fixture");
    let mut o = opts(endpoint);
    o.page_range = Some((1, 2));
    let doc = convert_vlm(&source, &o).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(served.load(Ordering::SeqCst), 2, "one request per page");
    let md = doc.export_to_markdown();
    assert!(md.contains("# Page One"), "markdown: {md:?}");
    assert!(md.contains("First body."), "markdown: {md:?}");
    assert!(md.contains("Second body."), "markdown: {md:?}");
}

/// #762 (docling#4561): a page whose request fails is recorded and left
/// out; the pages around it are kept and the result is a partial success.
#[test]
fn vlm_failed_page_is_a_page_error() {
    let (endpoint, served, handle) = mock_replies(vec![
        Reply::ok("<text>First body.</text>"),
        Reply {
            status: 400,
            content: String::new(),
            finish_reason: "stop",
        },
        Reply::ok("<text>Third body.</text>"),
    ]);
    let source =
        SourceDocument::from_file(repo_root().join("tests/data/pdf/sources/2206.01062.pdf"))
            .expect("pdf fixture");
    let mut o = opts(endpoint);
    o.page_range = Some((1, 3));
    let result = convert_vlm_result(&source, &o).expect("partial vlm conversion");
    handle.join().expect("mock server");
    // A 4xx is not retried: one request per page.
    assert_eq!(served.load(Ordering::SeqCst), 3);
    assert_eq!(result.status, ConversionStatus::PartialSuccess);
    let md = result.document.export_to_markdown();
    assert!(md.contains("First body."), "markdown: {md:?}");
    assert!(md.contains("Third body."), "markdown: {md:?}");
    assert_eq!(result.errors.len(), 1, "{:?}", result.errors);
    let e = &result.errors[0];
    assert_eq!(e.page_no, Some(2));
    assert_eq!(
        (e.component_type.as_str(), e.module_name.as_str()),
        ("pipeline", "VlmPipeline")
    );
    assert!(
        e.error_message.starts_with("VLM inference failed: ")
            && e.error_message.contains("HTTP 400"),
        "{}",
        e.error_message
    );
}

/// Only a document where no page produced output fails.
#[test]
fn vlm_every_page_failed_is_an_error() {
    let (endpoint, _, handle) = mock_replies(vec![Reply {
        status: 400,
        content: String::new(),
        finish_reason: "stop",
    }]);
    let source = SourceDocument::from_bytes("page.png", InputFormat::Image, image_bytes());
    let err = convert_vlm_result(&source, &opts(endpoint)).unwrap_err();
    handle.join().expect("mock server");
    assert!(
        err.to_string().contains("page 1: ") && err.to_string().contains("HTTP 400"),
        "{err}"
    );
}

/// #763 (docling#4778): an answer cut off at `max_tokens` (or by a content
/// filter) keeps its text and is recorded as incomplete.
#[test]
fn vlm_truncated_page_is_a_page_error() {
    let (endpoint, _, handle) = mock_replies(vec![
        Reply {
            status: 200,
            content: "<text>Cut off mid</text>".into(),
            finish_reason: "length",
        },
        Reply {
            status: 200,
            content: "<text>Filtered</text>".into(),
            finish_reason: "content_filter",
        },
    ]);
    let source =
        SourceDocument::from_file(repo_root().join("tests/data/pdf/sources/2206.01062.pdf"))
            .expect("pdf fixture");
    let mut o = opts(endpoint);
    o.page_range = Some((1, 2));
    let result = convert_vlm_result(&source, &o).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(result.status, ConversionStatus::PartialSuccess);
    let md = result.document.export_to_markdown();
    assert!(
        md.contains("Cut off mid") && md.contains("Filtered"),
        "markdown: {md:?}"
    );
    let got: Vec<_> = result
        .errors
        .iter()
        .map(|e| (e.page_no, e.error_message.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (Some(1), "VLM output incomplete (stop_reason=length)."),
            (
                Some(2),
                "VLM output incomplete (stop_reason=content_filter)."
            ),
        ]
    );
}

/// A complete answer is a plain success.
#[test]
fn vlm_complete_answer_is_a_success() {
    let (endpoint, _, handle) = mock_openai(vec!["<text>Whole page.</text>".into()]);
    let source = SourceDocument::from_bytes("page.png", InputFormat::Image, image_bytes());
    let result = convert_vlm_result(&source, &opts(endpoint)).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(result.status, ConversionStatus::Success);
    assert!(result.errors.is_empty());
}

/// #322: a Chandra-grammar answer routes to the layout-HTML parser — blocks,
/// furniture exclusion and the Form-table rule all through the wire path.
#[test]
fn vlm_parses_chandra_layout_html() {
    let answer = concat!(
        r#"<div data-bbox="10 10 990 40" data-label="Page-Header">Running header</div>"#,
        r#"<div data-bbox="10 60 990 120" data-label="Section-Header"><h2>Results</h2></div>"#,
        r#"<div data-bbox="10 140 990 200" data-label="Text"><p>Hello<br/>World</p></div>"#,
        r#"<div data-bbox="10 220 990 400" data-label="Form"><table><tr><td>Name</td><td>Value</td></tr></table></div>"#,
    );
    let (endpoint, served, handle) = mock_openai(vec![answer.into()]);
    let source = SourceDocument::from_bytes("page.png", InputFormat::Image, image_bytes());
    let doc = convert_vlm(&source, &opts(endpoint)).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(served.load(Ordering::SeqCst), 1);
    let md = doc.export_to_markdown();
    assert!(md.contains("## Results"), "markdown: {md:?}");
    assert!(md.contains("Hello World"), "br spacing: {md:?}");
    assert!(md.contains("| Name"), "form table: {md:?}");
    assert!(!md.contains("Running header"), "furniture leaked: {md:?}");
}

/// #322: an Unlimited-OCR grounding answer normalizes into the DeepSeek-OCR
/// shape and parses as labelled blocks.
#[test]
fn vlm_parses_unlimited_ocr_grounding() {
    let answer = "<|det|>title [52, 40, 816, 63]<|/det|>Report 1481\n\
                  <|det|>text [52, 80, 816, 120]<|/det|>First paragraph.";
    let (endpoint, served, handle) = mock_openai(vec![answer.into()]);
    let source = SourceDocument::from_bytes("page.png", InputFormat::Image, image_bytes());
    let doc = convert_vlm(&source, &opts(endpoint)).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(served.load(Ordering::SeqCst), 1);
    let md = doc.export_to_markdown();
    assert!(md.contains("# Report 1481"), "markdown: {md:?}");
    assert!(md.contains("First paragraph."), "markdown: {md:?}");
}

/// Conformance lock for #153: the `picture_classification` corpus fixture
/// scored 1.3% similarity in the live VLM run — a divergence that (per the
/// harness's own note) is render/model variance, *not* a parser defect. Feed
/// the model's exact DocTags for that document through `convert_vlm` and assert
/// the Markdown matches docling's groundtruth byte-for-byte, so our side of the
/// comparison stays provably correct (pictures + captions before the image
/// placeholder, multi-page `<page_break>`, reading order). Image leg → no
/// pdfium, runs everywhere.
#[test]
fn vlm_doctags_matches_groundtruth_picture_classification() {
    let doctags = std::fs::read_to_string(
        repo_root().join("tests/data/pdf/groundtruth/picture_classification.doctags.txt"),
    )
    .expect("doctags fixture");
    let want = std::fs::read_to_string(
        repo_root().join("tests/data/pdf/groundtruth/picture_classification.md"),
    )
    .expect("markdown groundtruth");
    // The model emits one DocTags blob per document (root wrapper + internal
    // <page_break>); the image leg is a single request → single answer.
    let (endpoint, served, handle) = mock_openai(vec![doctags]);
    let source = SourceDocument::from_bytes("page.png", InputFormat::Image, image_bytes());
    let doc = convert_vlm(&source, &opts(endpoint)).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(served.load(Ordering::SeqCst), 1);
    assert_eq!(
        doc.export_to_markdown().trim(),
        want.trim(),
        "VLM DocTags → Markdown must match docling's groundtruth for picture_classification"
    );
}

#[test]
fn vlm_rejects_non_visual_formats() {
    let source = SourceDocument::from_bytes("x.md", InputFormat::Md, b"# hi".to_vec());
    let err = convert_vlm(&source, &opts("http://127.0.0.1:1/v1".into())).unwrap_err();
    assert!(err
        .to_string()
        .contains("vlm pipeline converts PDF and image"));
}

/// A tiny valid PNG via the `image` crate (same trick as docling-pdf's tests).
fn image_bytes() -> Vec<u8> {
    use std::io::Cursor;
    let img = image::RgbImage::new(8, 8);
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

/// One DocTags page as granite-docling answers it: a header and a text,
/// each with its `loc_` box.
fn doctags_page(n: usize) -> String {
    format!(
        "<doctag><page_header><loc_10><loc_10><loc_490><loc_30>Header</page_header>\
         <text><loc_10><loc_50><loc_490><loc_70>Request {n}</text></doctag>"
    )
}

/// The page numbers of the JSON's `pages`, and of every item's `prov`.
fn json_pages(doc: &docling::DoclingDocument) -> (Vec<u64>, Vec<(String, u64)>) {
    let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    let mut pages: Vec<u64> = json["pages"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.parse().unwrap())
        .collect();
    pages.sort_unstable();
    let provs = json["texts"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| {
            let text = t["text"].as_str().unwrap_or("").to_string();
            t["prov"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(move |p| (text.clone(), p["page_no"].as_u64().unwrap()))
        })
        .collect();
    (pages, provs)
}

/// docling#4498 (#1146, `test_complete_page_ranges_equal_whole_vlm_conversion`):
/// a VLM conversion carries its pages and its items' provenance — the whole
/// `multi_page.pdf` gives pages 1–5, `--pages 3-4` exactly pages 3 and 4 with
/// every prov on them, and a failed page leaves the numbers after it
/// unchanged. A Markdown answer, which has no boxes, still gets its pages.
#[test]
fn vlm_output_carries_its_pages_and_provenance() {
    let source =
        SourceDocument::from_file(repo_root().join("tests/data/pdf/sources/multi_page.pdf"))
            .expect("pdf fixture");

    let (endpoint, _, handle) = mock_openai((1..=5).map(doctags_page).collect());
    let whole = convert_vlm(&source, &opts(endpoint)).expect("whole conversion");
    handle.join().expect("mock server");
    let (pages, provs) = json_pages(&whole);
    assert_eq!(pages, [1, 2, 3, 4, 5]);
    assert_eq!(provs.len(), 10, "{provs:?}");
    for n in 1..=5u64 {
        assert!(provs.contains(&(format!("Request {n}"), n)), "{provs:?}");
    }

    let (endpoint, _, handle) = mock_openai(vec![doctags_page(3), doctags_page(4)]);
    let mut o = opts(endpoint);
    o.page_range = Some((3, 4));
    let part = convert_vlm(&source, &o).expect("range conversion");
    handle.join().expect("mock server");
    let (pages, provs) = json_pages(&part);
    assert_eq!(pages, [3, 4]);
    assert!(provs.iter().all(|(_, p)| *p == 3 || *p == 4), "{provs:?}");
    assert!(provs.contains(&("Request 3".into(), 3)) && provs.contains(&("Request 4".into(), 4)));

    // A failed middle page: page 3's items stay on page 3.
    let (endpoint, _, handle) = mock_replies(vec![
        Reply::ok(doctags_page(1)),
        Reply {
            status: 400,
            content: String::new(),
            finish_reason: "stop",
        },
        Reply::ok(doctags_page(3)),
    ]);
    let mut o = opts(endpoint);
    o.page_range = Some((1, 3));
    let result = convert_vlm_result(&source, &o).expect("partial conversion");
    handle.join().expect("mock server");
    let (pages, provs) = json_pages(&result.document);
    assert_eq!(pages, [1, 3]);
    assert!(provs.contains(&("Request 3".into(), 3)), "{provs:?}");

    // Markdown answers: no boxes, but the pages are the converted ones.
    let (endpoint, _, handle) =
        mock_openai(vec!["# Page three\n\nBody.".into(), "Body four.".into()]);
    let mut o = opts(endpoint);
    o.page_range = Some((3, 4));
    let md = convert_vlm(&source, &o).expect("markdown answers");
    handle.join().expect("mock server");
    assert_eq!(json_pages(&md).0, [3, 4]);
}
