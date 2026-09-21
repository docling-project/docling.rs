//! Router-level tests over `tower::ServiceExt::oneshot` — no sockets, no ML
//! models: the conversions exercised here are declarative (Markdown/HTML/CSV
//! uploads), so the suite runs in plain CI.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use docling_serve::{router, ServeConfig};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app() -> axum::Router {
    router(ServeConfig::default())
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// A multipart body with one `file` part and optional extra text parts.
fn multipart(file_name: &str, content: &[u8], fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = "docling-serve-test-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    for (k, v) in fields {
        body.extend_from_slice(
            format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn convert_request(content_type: &str, body: Vec<u8>, query: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/convert{query}"))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap()
}

/// Shared fixtures live at the repo root (see CLAUDE.md); tests run with CWD =
/// the crate dir.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Skip-gate for tests that render PDFs: point pdfium resolution at the
/// repo-root `.pdfium/lib` when present (same pattern as
/// `crates/docling/tests/pages.rs`) — CI without the runtime assets stays
/// green.
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

/// The layout/OCR/TableFormer model files, resolved from the repo-root
/// `.models` into the env overrides (tests run with CWD = the crate dir, so
/// CWD-relative resolution can't find them) — same pattern as
/// `crates/docling/tests/pages.rs`.
fn ml_models_ready() -> bool {
    let m = repo_root().join(".models");
    let layout = ["layout_heron_int8.onnx", "layout_heron.onnx"]
        .iter()
        .map(|f| m.join(f))
        .find(|p| p.exists());
    let rec = ["ocr_rec_en.onnx", "ocr_rec.onnx"]
        .iter()
        .map(|f| m.join(f))
        .find(|p| p.exists());
    let dict = ["en_dict.txt", "ppocr_keys_v1.txt"]
        .iter()
        .map(|f| m.join(f))
        .find(|p| p.exists());
    let tf = m.join("tableformer");
    let dec = ["decoder_kv.onnx", "decoder_int8.onnx", "decoder.onnx"]
        .iter()
        .map(|f| tf.join(f))
        .find(|p| p.exists());
    match (layout, rec, dict, dec) {
        (Some(l), Some(r), Some(di), Some(de))
            if tf.join("encoder.onnx").exists() && tf.join("bbox.onnx").exists() =>
        {
            std::env::set_var("DOCLING_LAYOUT_ONNX", l);
            std::env::set_var("DOCLING_OCR_REC_ONNX", r);
            std::env::set_var("DOCLING_OCR_DICT", di);
            std::env::set_var("DOCLING_TABLEFORMER_ENCODER", tf.join("encoder.onnx"));
            std::env::set_var("DOCLING_TABLEFORMER_DECODER", de);
            std::env::set_var("DOCLING_TABLEFORMER_BBOX", tf.join("bbox.onnx"));
            true
        }
        _ => false,
    }
}

#[tokio::test]
async fn health_is_ok() {
    let response = app()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("ok"));
}

/// The playground's two static assets: the logo the header renders and the
/// OpenAPI description it links to. Both are baked into the binary, so a
/// server needs no static-file directory — and the spec must stay parseable,
/// since clients generate against it.
#[tokio::test]
async fn serves_its_logo_and_openapi_description() {
    let response = app()
        .oneshot(Request::get("/logo.svg").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "image/svg+xml",
        "the browser must render it, not download it"
    );
    assert!(body_string(response).await.contains("<svg"));

    let response = app()
        .oneshot(Request::get("/openapi.yaml").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/yaml");
    let spec = body_string(response).await;
    // Every route the router answers is described, and every conversion option
    // the API accepts appears — the spec drifting from `ConvertOptions` is the
    // failure mode worth catching here.
    for path in [
        "/v1/convert",
        "/v1/convert/async",
        "/v1/status/{id}",
        "/v1/result/{id}",
        "/v1/config",
        "/health",
        "/ready",
    ] {
        assert!(spec.contains(path), "{path} missing from openapi.yaml");
    }
    // The #182/#183 additions stay described.
    for schema in ["TaskStatus", "ConfidenceReport", "X-Docling-Confidence"] {
        assert!(spec.contains(schema), "{schema} missing from openapi.yaml");
    }
    for opt in [
        "to:",
        "strict:",
        "images:",
        "no_ocr:",
        "skip_ocr:",
        "force_full_page_ocr:",
        "no_table_former:",
        "fetch_images:",
        "skip_empty_cells:",
        "compact_tables:",
        "list_attachments:",
        "ebcdic_layout:",
        "pages:",
        "ocr_lang:",
        "ocr_mode:",
        "ocr_scale:",
        "scale:",
        "asr_model:",
        "encoding:",
        "video_frames:",
    ] {
        assert!(spec.contains(opt), "option {opt} missing from openapi.yaml");
    }
}

#[tokio::test]
async fn ready_without_warmup_is_immediate() {
    let response = app()
        .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn converts_markdown_upload_to_markdown() {
    let (ct, body) = multipart("note.md", b"# Title\n\nHello *world*.\n", &[]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/markdown; charset=utf-8"
    );
    let out = body_string(response).await;
    assert!(out.contains("# Title"), "unexpected body: {out}");
}

#[tokio::test]
async fn converts_csv_to_docling_json() {
    let (ct, body) = multipart("t.csv", b"a,b\n1,2\n", &[("to", "json")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(v["schema_name"], "DoclingDocument");
}

#[tokio::test]
async fn query_options_apply_and_body_wins() {
    // Query says json, body field says chunks — body wins.
    let (ct, body) = multipart("t.csv", b"a,b\n1,2\n", &[("to", "chunks")]);
    let response = app()
        .oneshot(convert_request(&ct, body, "?to=json"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(v.get("hierarchical").is_some(), "chunks shape expected");
}

/// docling's `TextBackendOptions.encoding`: the `encoding` option names the
/// text input's character encoding (multipart field or query parameter);
/// bytes it cannot decode fail the request instead of being guessed.
#[tokio::test]
async fn encoding_option_decodes_text_inputs() {
    // "# 日本" in Shift-JIS — detection would read it as windows-1252 mojibake.
    let sjis = b"# \x93\xfa\x96\x7b\n".to_vec();
    let (ct, body) = multipart("t.md", &sjis, &[("encoding", "shift_jis")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("# 日本"));

    let (ct, body) = multipart("t.md", &sjis, &[]);
    let response = app()
        .oneshot(convert_request(&ct, body, "?encoding=shift_jis"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("# 日本"));

    let (ct, body) = multipart("t.md", &sjis, &[("encoding", "utf-8")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_ne!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn dclx_download_has_attachment_headers() {
    let (ct, body) = multipart("sheet.csv", b"a,b\n1,2\n", &[("to", "dclx")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"sheet.dclx\""
    );
    // A dclx archive is a ZIP: PK magic.
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..2], b"PK");
}

#[tokio::test]
async fn unknown_format_is_422() {
    let (ct, body) = multipart("data.xyz", b"?", &[]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn missing_file_part_is_400() {
    let (ct, body) = multipart("x.md", b"x", &[]);
    // Rewrite the part name so no `file` part arrives.
    let body = String::from_utf8(body)
        .unwrap()
        .replace("name=\"file\"", "name=\"data\"");
    let response = app()
        .oneshot(convert_request(&ct, body.into_bytes(), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn bad_to_value_is_400() {
    let (ct, body) = multipart("x.md", b"x", &[("to", "pdf")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// `to=images` (#243) is PDF-only — a non-PDF upload answers 422 before any
/// pdfium work, so this runs in plain CI.
#[tokio::test]
async fn images_output_requires_a_pdf_input() {
    let (ct, body) = multipart("x.md", b"# hi", &[("to", "images")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_string(response).await.contains("PDF inputs only"));
}

/// #254: an unknown `ocr_mode` and a non-positive `ocr_scale` are rejected up
/// front (400), before any model work — the shared converter builder
/// validates them, so a plain-CI markdown upload exercises it.
#[tokio::test]
async fn ocr_mode_and_scale_are_validated() {
    let (ct, body) = multipart("x.md", b"# hi", &[("ocr_mode", "easyocr")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("ocr_mode"));

    let (ct, body) = multipart("x.md", b"# hi", &[("ocr_scale", "0")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("ocr_scale"));

    // Valid values pass straight through on a non-OCR format.
    let (ct, body) = multipart(
        "x.md",
        b"# hi",
        &[("ocr_mode", "full_page"), ("ocr_scale", "3")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// A `scale` outside 0.1–4.0 is rejected before pdfium is touched.
#[tokio::test]
async fn images_scale_is_validated() {
    let (ct, body) = multipart("x.pdf", b"%PDF-1.4", &[("to", "images"), ("scale", "9")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("scale"));
}

/// End-to-end rasterization over a real one-page fixture — runs only where
/// `.pdfium/lib` is installed (`pdfium_ready` gate).
#[tokio::test]
async fn rasterizes_pdf_pages_to_png() {
    if !pdfium_ready() {
        eprintln!("skipping: pdfium not installed");
        return;
    }
    let pdf = std::fs::read(repo_root().join("tests/data/pdf/sources/base14_fonts.pdf")).unwrap();
    let (ct, body) = multipart(
        "base14_fonts.pdf",
        &pdf,
        &[("to", "images"), ("scale", "1")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let pages = json["pages"].as_array().expect("pages array");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["page"], 1);
    assert!(pages[0]["width"].as_u64().unwrap() > 0);
    assert!(pages[0]["height"].as_u64().unwrap() > 0);
    // The payload decodes to a real PNG (magic bytes).
    let b64 = pages[0]["png_base64"].as_str().unwrap();
    let bytes = docling::base64::decode(b64).expect("valid base64");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
}

/// #246 end-to-end: a `no_ocr=true` request must not degrade the shared warm
/// pipeline for later default requests. The table fixture makes the difference
/// decisive — the full pipeline emits a Markdown table (pipes), the no-OCR
/// text-layer path emits flat paragraphs. Needs pdfium + the layout/OCR/
/// TableFormer models, so it gates like the rasterization test.
#[tokio::test]
async fn no_ocr_request_does_not_stick_to_the_warm_pipeline() {
    if !pdfium_ready() || !ml_models_ready() {
        eprintln!("skipping: pdfium or the ML models are not present");
        return;
    }
    let pdf =
        std::fs::read(repo_root().join("tests/data/pdf/sources/2305.03393v1-pg9.pdf")).unwrap();
    let app = app();
    let run = |no_ocr: &'static str, app: axum::Router| {
        let (ct, body) = multipart("t.pdf", &pdf, &[("to", "md"), ("no_ocr", no_ocr)]);
        async move {
            let response = app.oneshot(convert_request(&ct, body, "")).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            body_string(response).await
        }
    };
    let full1 = run("false", app.clone()).await;
    assert!(full1.contains('|'), "full pipeline must emit the table");
    let reduced = run("true", app.clone()).await;
    assert!(!reduced.contains('|'), "no_ocr path has no TableFormer");
    // The bug: this third request reused the degraded pipeline and returned
    // the flat no_ocr output until the server restarted.
    let full2 = run("false", app).await;
    assert_eq!(
        full1, full2,
        "default request after no_ocr must fully recover"
    );
}

/// #263: with the RSS already past the watermark of a tiny ceiling, both
/// endpoints shed the request with 503 + Retry-After instead of accepting
/// work that would push the process into the OOM killer. (The test process's
/// real RSS is far above a 1 MB ceiling on any platform where rss_mb()
/// reads, so no mocking is needed; off-Linux the check is inert and the
/// request converts — skip there.)
#[tokio::test]
async fn memory_ceiling_sheds_requests_with_503() {
    if docling_core::env::rss_mb().is_none() {
        eprintln!("skipping: no RSS reading on this platform");
        return;
    }
    let cfg = ServeConfig {
        max_memory_mb: Some(1),
        ..ServeConfig::default()
    };
    let app = router(cfg);
    let (ct, body) = multipart("x.md", b"# hi", &[]);
    let response = app
        .clone()
        .oneshot(convert_request(&ct, body, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "5");
    assert!(body_string(response).await.contains("ceiling"));
    // Async submissions are shed the same way.
    let (ct, body) = multipart("x.md", b"# hi", &[]);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/convert/async")
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// A ceiling of 0 (explicitly disabled) admits everything.
#[tokio::test]
async fn memory_ceiling_zero_disables_admission_control() {
    let cfg = ServeConfig {
        max_memory_mb: Some(0),
        ..ServeConfig::default()
    };
    let (ct, body) = multipart("note.md", b"# T", &[]);
    let response = router(cfg)
        .oneshot(convert_request(&ct, body, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn url_fetch_can_be_disabled() {
    let cfg = ServeConfig {
        allow_url_fetch: false,
        ..ServeConfig::default()
    };
    let response = router(cfg)
        .oneshot(convert_request(
            "application/json",
            br#"{"url": "https://example.com/x.md"}"#.to_vec(),
            "",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn wrong_content_type_is_400() {
    let response = app()
        .oneshot(convert_request("text/plain", b"hello".to_vec(), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn strict_field_changes_markdown_dialect() {
    // Legacy docling output escapes the underscore in `x_y`; strict mode
    // doesn't. The exact difference doesn't matter here, only that the switch
    // reaches the converter.
    let md = b"x_y and 5*6\n";
    let (ct1, b1) = multipart("p.md", md, &[]);
    let (ct2, b2) = multipart("p.md", md, &[("strict", "true")]);
    let legacy = body_string(app().oneshot(convert_request(&ct1, b1, "")).await.unwrap()).await;
    let strict = body_string(app().oneshot(convert_request(&ct2, b2, "")).await.unwrap()).await;
    assert_ne!(legacy, strict, "strict flag had no effect");
}

/// `fetch_images` is outbound fetch (SSRF surface), so it's gated behind the
/// same `--allow-url-fetch` as URL inputs: honored only when the flag is on,
/// silently ignored otherwise. Proven against a local image server that counts
/// the requests it receives — the gate must let *zero* through when off.
/// Serializes the tests that toggle `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH`
/// (process-global): without it, one test's `remove_var` can strip the escape
/// hatch out from under another's in-flight loopback fetch/PUT. Tokio's mutex
/// because the guard is held across the tests' awaits.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn fetch_images_is_gated_behind_allow_url_fetch() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let _env = ENV_LOCK.lock().await;

    // 1×1 red PNG — a real image so the resolved bytes decode and embed.
    const RED_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x6e, 0x2c, 0xdc, 0x33, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            server_hits.fetch_add(1, Ordering::Relaxed);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                RED_PNG.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(RED_PNG);
            let _ = stream.flush();
        }
    });
    let html = format!("<html><body><p>hi</p><img src=\"http://{addr}/x.png\"></body></html>");
    let fields = [("to", "json"), ("fetch_images", "true")];

    // Gate OFF (secure default): fetch_images asked for, but --allow-url-fetch
    // is off → no outbound fetch, the picture stays a placeholder (no bytes).
    let (ct, body) = multipart("p.html", html.as_bytes(), &fields);
    let cfg = ServeConfig {
        allow_url_fetch: false,
        ..ServeConfig::default()
    };
    let response = router(cfg)
        .oneshot(convert_request(&ct, body, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let out = body_string(response).await;
    assert!(
        !out.contains("data:image/"),
        "gate off must not embed: {out}"
    );
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "gate off must not fetch the image"
    );

    // Gate ON: --allow-url-fetch set (plus the private-IP opt-in for 127.0.0.1)
    // → the image is fetched and embedded as a data URI.
    std::env::set_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", "1");
    let (ct, body) = multipart("p.html", html.as_bytes(), &fields);
    let cfg = ServeConfig {
        allow_url_fetch: true,
        ..ServeConfig::default()
    };
    let response = router(cfg)
        .oneshot(convert_request(&ct, body, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let out = body_string(response).await;
    std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    assert!(
        hits.load(Ordering::Relaxed) >= 1,
        "gate on must fetch the image"
    );
    assert!(
        out.contains("data:image/"),
        "gate on must embed the fetched image"
    );
}

/// A multipart body with several `file` parts — a #182 batch.
fn multipart_files(files: &[(&str, &[u8])], fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = "docling-serve-test-boundary";
    let mut body = Vec::new();
    for (file_name, content) in files {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(b"\r\n");
    }
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Several `file` parts in one request convert as a batch (#182): the
/// response is a JSON results array with per-item status — and one bad file
/// fails only its own item.
#[tokio::test]
async fn batch_upload_returns_results_array() {
    let (ct, body) = multipart_files(
        &[
            ("a.md", b"# A\n"),
            ("b.csv", b"x,y\n1,2\n"),
            ("broken.xyz", b"?"),
        ],
        &[("to", "md")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["status"], "success");
    assert!(results[0]["md"].as_str().unwrap().contains("# A"));
    assert_eq!(results[1]["status"], "success");
    assert_eq!(results[2]["status"], "failure");
    assert!(results[2]["error"].as_str().unwrap().contains("xyz"));
}

/// Async job lifecycle (#182): submit returns a task id, status reaches
/// `success`, and the result endpoint replays the sync response (here: the
/// converted Markdown). Unknown ids 404.
#[tokio::test]
async fn async_job_roundtrip() {
    let app = app();
    let (ct, body) = multipart("note.md", b"# Async\n\nhello.\n", &[]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/convert/async")
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let id = v["task_id"].as_str().expect("task id").to_string();
    assert_eq!(v["task_status"], "pending");

    // Poll until the job finishes (a declarative conversion takes milliseconds;
    // the deadline only bounds a hung test).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/status/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        match v["task_status"].as_str().unwrap() {
            "success" => break,
            "failure" => panic!("job failed: {v}"),
            _ if std::time::Instant::now() > deadline => panic!("job never finished"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }

    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/v1/result/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/markdown; charset=utf-8"
    );
    let md = body_string(response).await;
    assert!(md.contains("# Async"), "unexpected result body: {md}");

    // The result stays fetchable (until the TTL): a retried GET must not 404.
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/v1/result/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::get("/v1/status/no-such-task")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// A bad async submission fails synchronously (nothing queues) and a full
/// queue answers 429 instead of growing without bound.
#[tokio::test]
async fn async_rejects_bad_requests_and_full_queue() {
    let (ct, body) = multipart("x.md", b"x", &[("to", "pdf")]);
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/convert/async")
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // queue_size 0 admits nothing — the first submission is already refused.
    let cfg = ServeConfig {
        queue_size: 0,
        ..ServeConfig::default()
    };
    let (ct, body) = multipart("x.md", b"# x\n", &[]);
    let response = router(cfg)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/convert/async")
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// Declarative conversions have no ML stages, so no confidence surfaces
/// (#183): no `X-Docling-Confidence` header, no `confidence` key in JSON.
/// (The positive case — real scores from the PDF pipeline — is covered by the
/// docling-pdf tests; here the router-level suite stays model-free.)
#[tokio::test]
async fn declarative_conversions_carry_no_confidence() {
    let (ct, body) = multipart("t.csv", b"a,b\n1,2\n", &[("to", "json")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-docling-confidence").is_none());
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(v.get("confidence").is_none());
}

#[tokio::test]
async fn index_serves_docs_and_form() {
    let response = app()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("/v1/convert") && body.contains("<form") || body.contains("Convert"));
}

#[tokio::test]
async fn chunker_hierarchical_returns_only_that_chunker() {
    // #256: an explicit chunker selects a single record set.
    let (ct, body) = multipart(
        "t.csv",
        b"a,b\n1,2\n",
        &[("to", "chunks"), ("chunker", "hierarchical")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(v.get("hierarchical").is_some());
    assert!(v.get("hybrid").is_none(), "hierarchical only: {v}");
}

#[tokio::test]
async fn unknown_chunker_is_a_400() {
    let (ct, body) = multipart(
        "t.csv",
        b"a,b\n1,2\n",
        &[("to", "chunks"), ("chunker", "nope")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chunk_tokenizer_path_escape_is_a_400() {
    // #256: the request-side tokenizer must stay a server-local relative
    // path — traversal is rejected before any conversion work.
    for bad in ["../secrets/tok.json", "/etc/passwd"] {
        let (ct, body) = multipart(
            "t.csv",
            b"a,b\n1,2\n",
            &[("to", "chunks"), ("chunk_tokenizer", bad)],
        );
        let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "for {bad:?}");
    }
}

#[tokio::test]
async fn explicit_hybrid_without_tokenizer_is_a_400() {
    // Legacy both-chunkers mode silently skips hybrid when no tokenizer is
    // installed; asking for hybrid outright must fail loudly instead. (The
    // test env has no .models/chunk/tokenizer.json and no
    // DOCLING_CHUNK_TOKENIZER; if one is installed, the request succeeds —
    // accept both, but never a silent hierarchical-only 200.)
    let (ct, body) = multipart(
        "t.csv",
        b"a,b\n1,2\n",
        &[("to", "chunks"), ("chunker", "hybrid")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    if response.status() == StatusCode::OK {
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert!(v.get("hybrid").is_some(), "hybrid requested: {v}");
        assert!(v.get("hierarchical").is_none());
    } else {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

// --- #139: docling sources/target passthrough ------------------------------

fn json_convert(body: &str, allow_fetch: bool) -> Request<Body> {
    let _ = allow_fetch;
    convert_request("application/json", body.as_bytes().to_vec(), "")
}

#[tokio::test]
async fn file_sources_convert_without_any_gate() {
    // kind=file is base64 in the body — no outbound access, no gate.
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [
            {{"kind": "file", "base64_string": "{b64}", "filename": "one.csv"}},
            {{"kind": "file", "base64_string": "{b64}", "filename": "two.csv"}}
        ], "to": "json"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let results = v["results"].as_array().expect("batch shape");
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r["status"] == "success"), "{v}");
}

fn docling_b64(text: &str) -> String {
    // Tiny local base64 (no test dep): RFC 4648 standard alphabet.
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[tokio::test]
async fn url_and_sources_together_is_a_400() {
    let b64 = docling_b64("x");
    let body = format!(
        r#"{{"url": "https://example.com/a.md",
            "sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "a.csv"}}]}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_source_kind_is_a_400() {
    let body = r#"{"sources": [{"kind": "google_drive", "document_id": "x"}]}"#;
    let response = app().oneshot(json_convert(body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let msg = body_string(response).await;
    assert!(
        msg.contains("unknown variant") && msg.contains("google_cloud_storage"),
        "should list the supported kinds: {msg}"
    );
}

#[tokio::test]
async fn cloud_source_and_target_are_gated_behind_allow_url_fetch() {
    // Both directions of cloud passthrough sit behind --allow-url-fetch.
    let s3 = r#"{"kind": "s3", "endpoint": "s3.us-east-2.amazonaws.com",
                 "access_key": "k", "secret_key": "s", "bucket": "b"}"#;
    let body = format!(r#"{{"sources": [{s3}]}}"#);
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "a.csv"}}],
            "target": {s3}}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[cfg(not(feature = "cloud"))]
#[tokio::test]
async fn cloud_kind_without_the_feature_names_the_rebuild() {
    // Gate open, feature absent: the error must say how to get the feature,
    // not fail as a mystery.
    let cfg = ServeConfig {
        allow_url_fetch: true,
        ..ServeConfig::default()
    };
    let body = r#"{"sources": [{"kind": "s3", "endpoint": "s3.us-east-2.amazonaws.com",
                   "access_key": "k", "secret_key": "s", "bucket": "b"}]}"#;
    let response = router(cfg).oneshot(json_convert(body, true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let msg = body_string(response).await;
    assert!(msg.contains("--features cloud"), "{msg}");
}

#[tokio::test]
async fn explicit_inbody_target_answers_in_the_body() {
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "a.csv"}}],
            "target": {{"kind": "inbody"}}, "to": "json"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(v["schema_name"], "DoclingDocument");
}

/// Scrape `/metrics` and read one metric line's value by its exact prefix
/// (name + labels).
async fn metric_value(line_prefix: &str) -> f64 {
    let response = app()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    text.lines()
        .find(|l| l.starts_with(line_prefix))
        .unwrap_or_else(|| panic!("no metric line starts with {line_prefix:?} in:\n{text}"))
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let response = app()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let text = body_string(response).await;
    for needle in [
        "# TYPE docling_serve_requests_total counter",
        "# TYPE docling_serve_requests_in_flight gauge",
        "# TYPE docling_serve_request_duration_seconds histogram",
        "docling_serve_request_duration_seconds_bucket{le=\"+Inf\"}",
        "# TYPE docling_serve_conversions_total counter",
        "docling_serve_conversions_total{outcome=\"success\"}",
        "docling_serve_conversions_total{outcome=\"failure\"}",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
}

/// The registry is process-global and tests run in parallel, so this asserts
/// monotonic growth caused by its own requests rather than absolute values.
#[tokio::test]
async fn requests_and_conversions_are_counted() {
    let requests_before = metric_value("docling_serve_requests_total{class=\"2xx\"}").await;
    let ok_before = metric_value("docling_serve_conversions_total{outcome=\"success\"}").await;
    let failed_before = metric_value("docling_serve_conversions_total{outcome=\"failure\"}").await;

    // `to=json` buffers through `convert_document`, so both counters have
    // moved by the time the response exists (no streaming race).
    let (ct, body) = multipart("a.csv", b"a,b\n1,2\n", &[("to", "json")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // An unconvertible upload counts as a failed conversion.
    let (ct, body) = multipart("broken.docx", b"not a zip", &[("to", "json")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let requests_after = metric_value("docling_serve_requests_total{class=\"2xx\"}").await;
    let ok_after = metric_value("docling_serve_conversions_total{outcome=\"success\"}").await;
    let failed_after = metric_value("docling_serve_conversions_total{outcome=\"failure\"}").await;
    assert!(requests_after >= requests_before + 1.0);
    assert!(ok_after >= ok_before + 1.0);
    assert!(failed_after >= failed_before + 1.0);
}

// --- #304: remote VLM pipeline ---------------------------------------------

/// 1×1 red PNG (the same bytes as the fetch_images test) — the VLM image leg
/// needs no pdfium and no models, so these tests run in plain CI.
const VLM_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x00, 0x03, 0x00, 0x01, 0x6e, 0x2c, 0xdc, 0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A one-shot OpenAI-compatible stub (the counterpart of
/// `crates/docling/tests/vlm.rs::mock_openai`): serves `answer` as
/// `choices[0].message.content` and records that it was hit.
fn mock_vlm(
    answer: &str,
) -> (
    String,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&served);
    let answer = answer.to_string();
    let handle = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        // Read until the full body arrived (Content-Length framing).
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = conn.read(&mut tmp).expect("read request");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                let need: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .expect("content-length");
                if buf.len() >= head_end + 4 + need {
                    break;
                }
            }
        }
        let body = String::from_utf8_lossy(&buf).into_owned();
        assert!(body.contains("\"model\":\"mock-docling\""), "model missing");
        let payload = serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": answer } }]
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        conn.write_all(resp.as_bytes()).expect("write response");
        count.fetch_add(1, Ordering::SeqCst);
    });
    (format!("http://{addr}/v1"), served, handle)
}

#[tokio::test]
async fn vlm_pipeline_converts_via_the_operator_pinned_endpoint() {
    // Pinned mode (#304's safer default): the endpoint comes from the
    // server's DOCLING_RS_VLM_* environment, so the request needs neither a
    // vlm_endpoint nor --allow-url-fetch — and a loopback model server (the
    // common deployment) is the operator's own choice, exempt from the SSRF
    // block-list. The other VLM tests below pass explicit endpoints, so these
    // process-global vars can't steer them even while set.
    let (endpoint, served, handle) = mock_vlm("Hello from the VLM");
    std::env::set_var("DOCLING_RS_VLM_ENDPOINT", &endpoint);
    let fields = [("pipeline", "vlm"), ("vlm_model", "mock-docling")];
    let (ct, body) = multipart("page.png", VLM_PNG, &fields);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    let status = response.status();
    let out = body_string(response).await;
    std::env::remove_var("DOCLING_RS_VLM_ENDPOINT");
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(out.contains("Hello from the VLM"), "markdown: {out}");
    // Join before reading the counter: the stub bumps it *after* writing the
    // response, so the client can observe the reply first — asserting before
    // the join raced the mock thread (flaky `0 == 1`).
    handle.join().unwrap();
    assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn vlm_rejects_non_visual_formats_instead_of_converting_them() {
    // pipeline=vlm on a Markdown upload must surface the VLM's format error,
    // not silently run the standard conversion the caller didn't ask for —
    // including on the to=md streaming path this request takes. The endpoint
    // is never contacted (the format check precedes any HTTP), so a public
    // literal that passes the SSRF block-list suffices — and keeps this test
    // off the env vars the pinned test above toggles.
    let fields = [
        ("pipeline", "vlm"),
        ("vlm_endpoint", "http://8.8.8.8:9/v1"),
        ("vlm_model", "m"),
    ];
    let cfg = ServeConfig {
        allow_url_fetch: true,
        ..ServeConfig::default()
    };
    let (ct, body) = multipart("d.md", b"# hi", &fields);
    let response = router(cfg)
        .oneshot(convert_request(&ct, body, ""))
        .await
        .unwrap();
    let status = response.status();
    let out = body_string(response).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert!(out.contains("vlm pipeline converts PDF and image"), "{out}");
}

#[tokio::test]
async fn request_supplied_vlm_endpoint_needs_allow_url_fetch() {
    // Secure default: without --allow-url-fetch a caller must not point the
    // server's outbound traffic anywhere — 422 before anything is contacted.
    let fields = [
        ("pipeline", "vlm"),
        ("vlm_endpoint", "http://127.0.0.1:9/v1"),
        ("vlm_model", "m"),
    ];
    let (ct, body) = multipart("page.png", VLM_PNG, &fields);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let out = body_string(response).await;
    assert!(out.contains("--allow-url-fetch"), "{out}");
    assert!(out.contains("DOCLING_RS_VLM_ENDPOINT"), "{out}");
}

#[tokio::test]
async fn vlm_endpoint_resolving_to_private_address_is_rejected() {
    let _env = ENV_LOCK.lock().await;
    std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    // Even with the gate open, a request endpoint may not reach back into the
    // server's own network (same block-list as URL inputs).
    let cfg = ServeConfig {
        allow_url_fetch: true,
        ..ServeConfig::default()
    };
    let fields = [
        ("pipeline", "vlm"),
        ("vlm_endpoint", "http://127.0.0.1:9/v1"),
        ("vlm_model", "m"),
    ];
    let (ct, body) = multipart("page.png", VLM_PNG, &fields);
    let response = router(cfg)
        .oneshot(convert_request(&ct, body, ""))
        .await
        .unwrap();
    let out = body_string(response).await;
    assert!(out.contains("private/loopback"), "{out}");
}

#[tokio::test]
async fn unknown_pipeline_is_a_400_and_fails_async_submissions_fast() {
    // Query-param spelling on the sync endpoint…
    let (ct, body) = multipart("d.md", b"# hi", &[]);
    let response = app()
        .oneshot(convert_request(&ct, body, "?pipeline=magic"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("unknown pipeline"));

    // …and the async endpoint validates synchronously: nothing is queued.
    let (ct, body) = multipart("d.md", b"# hi", &[("pipeline", "magic")]);
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/convert/async")
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn vlm_options_ride_the_json_body_and_zero_max_tokens_is_a_400() {
    // JSON-body spelling (the serde flatten path) — validated before any
    // conversion or outbound traffic.
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "t.csv"}}],
            "pipeline": "vlm", "vlm_endpoint": "http://example.com/v1",
            "vlm_model": "m", "vlm_max_tokens": 0}}"#
    );
    let cfg = ServeConfig {
        allow_url_fetch: true,
        ..ServeConfig::default()
    };
    let response = router(cfg)
        .oneshot(json_convert(&body, true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("vlm_max_tokens"));
}

// --- #303: zip and put output targets --------------------------------------

#[tokio::test]
async fn zip_target_answers_with_an_archive_of_rendered_outputs() {
    // No gate: the archive stays in the response body, nothing goes outbound.
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [
            {{"kind": "file", "base64_string": "{b64}", "filename": "one.csv"}},
            {{"kind": "file", "base64_string": "{b64}", "filename": "two.csv"}},
            {{"kind": "file", "base64_string": "{b64}", "filename": "two.csv"}}
        ], "target": {{"kind": "zip"}}, "to": "md"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(content_type, "application/zip");
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(disposition, "attachment; filename=\"converted.zip\"");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..2], b"PK", "zip magic");
    // Entry names are stored verbatim in the local file headers; the
    // duplicate stem gets the `-2` de-dup, same as the cloud targets.
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(has(b"one.md"), "one.md entry missing");
    assert!(has(b"two.md"), "two.md entry missing");
    assert!(has(b"two-2.md"), "deduped two-2.md entry missing");
}

#[tokio::test]
async fn zip_target_single_source_names_the_archive_and_propagates_errors() {
    // A single input names its archive after itself…
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "sheet.csv"}}],
            "target": {{"kind": "zip"}}, "to": "json"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let disposition = response
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(disposition, "attachment; filename=\"sheet.zip\"");

    // …and a single failing input propagates its error instead of answering
    // with an archive of one error file (matching the inbody semantics).
    let b64 = docling_b64("x");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "wat.unknown"}}],
            "target": {{"kind": "zip"}}, "to": "md"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
}

#[tokio::test]
async fn zip_target_batch_converts_around_a_bad_item() {
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [
            {{"kind": "file", "base64_string": "{b64}", "filename": "good.csv"}},
            {{"kind": "file", "base64_string": "{b64}", "filename": "bad.unknown"}}
        ], "target": {{"kind": "zip"}}, "to": "md"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    assert!(has(b"good.md"), "good entry missing");
    assert!(has(b"bad.md.error.txt"), "error entry missing");
}

#[tokio::test]
async fn zip_target_does_not_combine_with_to_images() {
    let b64 = docling_b64("a,b\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "s.csv"}}],
            "target": {{"kind": "zip"}}, "to": "images"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("output target"));
}

#[tokio::test]
async fn put_target_is_gated_behind_allow_url_fetch() {
    let b64 = docling_b64("a,b\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "s.csv"}}],
            "target": {{"kind": "put", "url": "http://127.0.0.1:9/up"}}, "to": "md"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_string(response).await.contains("--allow-url-fetch"));
}

#[tokio::test]
async fn put_target_uploads_each_output_and_acknowledges() {
    let _env = ENV_LOCK.lock().await;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = Arc::clone(&hits);
    let handle = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = conn.read(&mut tmp).expect("read request");
                buf.extend_from_slice(&tmp[..n]);
                if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let need: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .expect("content-length");
                    if buf.len() >= head_end + 4 + need {
                        break;
                    }
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            assert!(head.starts_with("PUT /up?sig=abc "), "request line: {head}");
            server_hits.fetch_add(1, Ordering::SeqCst);
            let _ = conn
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });

    std::env::set_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", "1");
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [
            {{"kind": "file", "base64_string": "{b64}", "filename": "one.csv"}},
            {{"kind": "file", "base64_string": "{b64}", "filename": "two.csv"}}
        ], "target": {{"kind": "put", "url": "http://{addr}/up?sig=abc"}}, "to": "md"}}"#
    );
    let cfg = ServeConfig {
        allow_url_fetch: true,
        ..ServeConfig::default()
    };
    let response = router(cfg)
        .oneshot(json_convert(&body, true))
        .await
        .unwrap();
    let status = response.status();
    let out = body_string(response).await;
    std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    assert_eq!(status, StatusCode::OK, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["kind"], "RemoteTargetResult");
    // The signature-carrying query string must not be echoed back.
    assert_eq!(v["target"], format!("http://{addr}/up"));
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r["status"] == "success"), "{v}");
    assert_eq!(results[0]["key"], "one.md");
    assert_eq!(results[1]["key"], "two.md");
    // Same join-before-assert rule as the VLM stub: the counter is bumped
    // after the response goes out.
    handle.join().unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn put_target_refuses_a_private_address_without_the_escape_hatch() {
    let _env = ENV_LOCK.lock().await;
    std::env::remove_var("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    let b64 = docling_b64("a,b\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "s.csv"}}],
            "target": {{"kind": "put", "url": "http://127.0.0.1:9/up"}}, "to": "md"}}"#
    );
    let cfg = ServeConfig {
        allow_url_fetch: true,
        ..ServeConfig::default()
    };
    let response = router(cfg)
        .oneshot(json_convert(&body, true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("private/loopback"));
}

// --- #317: LaTeX output --------------------------------------------------

#[tokio::test]
async fn latex_output_is_a_complete_document() {
    let (ct, body) = multipart(
        "note.md",
        b"# Title\n\nHello 100% & more.\n",
        &[("to", "latex")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/x-tex; charset=utf-8"
    );
    let out = body_string(response).await;
    assert!(out.starts_with("\\documentclass"), "{out}");
    assert!(out.contains("\\title{Title}"), "{out}");
    assert!(out.contains("Hello 100\\% \\& more."), "escaping: {out}");
    assert!(out.ends_with("\\end{document}"), "{out}");
}

#[tokio::test]
async fn latex_batch_items_and_zip_target_carry_tex() {
    // Batch: inline `latex` key per item.
    let (ct, body) = multipart_files(
        &[("a.md", b"# A\n"), ("b.md", b"# B\n")],
        &[("to", "latex")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let results = v["results"].as_array().expect("batch shape");
    assert_eq!(results.len(), 2);
    assert!(
        results[0]["latex"].as_str().unwrap().contains("\\title{A}"),
        "{v}"
    );

    // Zip target: `<stem>.tex` entries (OutputNames knows the extension).
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "one.csv"}}],
            "target": {{"kind": "zip"}}, "to": "latex"}}"#
    );
    let response = app().oneshot(json_convert(&body, false)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        bytes.windows(7).any(|w| w == b"one.tex"),
        "tex entry missing"
    );
}
