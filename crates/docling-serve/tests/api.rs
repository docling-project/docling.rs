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

#[tokio::test]
async fn health_is_ok() {
    let response = app()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("ok"));
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
