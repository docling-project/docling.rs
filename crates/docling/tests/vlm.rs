//! e2e for issue #77: the remote-VLM pipeline against a local mock of an
//! OpenAI-compatible `chat/completions` endpoint.
//!
//! The PDF test renders real pages (pdfium, no ONNX models) and skips cleanly
//! when the pdfium library isn't around, like `tests/pages.rs`. The mock
//! asserts the wire shape (model name, data-URI page image) and returns
//! DocLang wrapped the way real models wrap answers (fences, full roots),
//! exercising the normalization path end to end.
#![cfg(feature = "vlm")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use docling::vlm::{convert_vlm, VlmOptions};
use docling::{InputFormat, SourceDocument};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn pdfium_ready() -> bool {
    let lib = repo_root().join(".pdfium/lib");
    if lib.join("libpdfium.so").exists()
        || lib.join("libpdfium.dylib").exists()
        || lib.join("pdfium.dll").exists()
    {
        std::env::set_var("PDFIUM_DYNAMIC_LIB_PATH", &lib);
        return true;
    }
    std::env::var("PDFIUM_DYNAMIC_LIB_PATH").is_ok()
}

/// Minimal HTTP/1.1 server: for each expected request, read head + body,
/// run the assertions, respond with a canned chat completion whose `content`
/// comes from `answers`. Returns (base_url, served-count handle, join handle).
fn mock_openai(answers: Vec<String>) -> (String, Arc<AtomicUsize>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&served);
    let handle = std::thread::spawn(move || {
        for answer in answers {
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
            let payload = serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": answer } }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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
    if !pdfium_ready() {
        eprintln!("skipping: pdfium library not found");
        return;
    }
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

#[test]
fn vlm_converts_an_image_without_pdfium() {
    // An image input goes straight to the endpoint — no pdfium, no models —
    // so this leg runs everywhere, keeping the wire format pinned in CI.
    let (endpoint, served, handle) = mock_openai(vec!["<text>From an image.</text>".into()]);
    let source = SourceDocument::from_bytes("page.png", InputFormat::Image, image_bytes());
    let doc = convert_vlm(&source, &opts(endpoint)).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(served.load(Ordering::SeqCst), 1);
    assert!(doc.export_to_markdown().contains("From an image."));
}

/// A granite-style DocTags answer routes through docling-core's DocTags
/// parser end to end (image leg: no pdfium needed, runs everywhere).
#[test]
fn vlm_parses_doctags_answers() {
    let (endpoint, served, handle) = mock_openai(vec![
        "<doctag><section_header_level_1><loc_1><loc_2><loc_3><loc_4>Section</section_header_level_1>\
<text><loc_1><loc_5><loc_3><loc_6>Body text.</text>\
<otsl><loc_1><loc_7><loc_3><loc_9><ched>H<nl><fcel>v<nl></otsl></doctag>"
            .into(),
    ]);
    let source = SourceDocument::from_bytes("page.png", InputFormat::Image, image_bytes());
    let doc = convert_vlm(&source, &opts(endpoint)).expect("vlm conversion");
    handle.join().expect("mock server");
    assert_eq!(served.load(Ordering::SeqCst), 1);
    let md = doc.export_to_markdown();
    assert!(md.contains("## Section"), "md: {md:?}");
    assert!(md.contains("Body text."), "md: {md:?}");
    assert!(md.contains("| H"), "md: {md:?}");
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
