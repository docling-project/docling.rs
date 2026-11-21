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
        // #615: upstream docling-serve's routes.
        "/v1/convert/file",
        "/v1/convert/source",
        "/v1/convert/file/async",
        "/v1/convert/source/async",
        "/v1/status/poll/{id}",
        "ConvertDocumentResponse",
        "UpstreamOptions",
    ] {
        assert!(spec.contains(path), "{path} missing from openapi.yaml");
    }
    // The #182/#183 additions stay described.
    for schema in ["TaskStatus", "ConfidenceReport", "X-Docling-Confidence"] {
        assert!(spec.contains(schema), "{schema} missing from openapi.yaml");
    }
    // The options only this server has; every shared `ConvertOptions` field
    // is checked (query parameter and multipart key) by
    // `crates/docling/tests/options_inventory.rs`. Anchored on the
    // parameter definition, so `scale` is not satisfied by `ocr_scale`.
    for opt in ["to", "images", "scale", "pandoc_api_version"] {
        assert!(
            spec.contains(&format!("\n    {opt}:\n")),
            "parameter {opt} missing from openapi.yaml"
        );
    }
}

/// `openapi.yaml` parses as YAML and every local `$ref` in it resolves. The
/// text checks above and `options_inventory` pass on a spec no YAML reader
/// accepts — two mis-indented lines and an unquoted `: ` in a description
/// went unnoticed for several releases — so the spec is read as a client
/// generator reads it.
#[test]
fn openapi_spec_parses_and_every_ref_resolves() {
    use serde_norway::Value;
    let spec: Value = serde_norway::from_str(include_str!("../src/openapi.yaml"))
        .unwrap_or_else(|e| panic!("openapi.yaml is not valid YAML: {e}"));
    assert!(spec["openapi"]
        .as_str()
        .is_some_and(|v| v.starts_with("3.")));
    assert!(spec["paths"].as_mapping().is_some_and(|p| !p.is_empty()));

    fn refs<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::Mapping(m) => {
                for (k, v) in m {
                    match (k.as_str(), v.as_str()) {
                        (Some("$ref"), Some(r)) => out.push(r),
                        _ => refs(v, out),
                    }
                }
            }
            Value::Sequence(s) => s.iter().for_each(|v| refs(v, out)),
            Value::Tagged(t) => refs(&t.value, out),
            _ => {}
        }
    }
    let mut all = Vec::new();
    refs(&spec, &mut all);
    assert!(!all.is_empty());
    let dangling: Vec<&str> = all
        .into_iter()
        .filter(|r| {
            let Some(pointer) = r.strip_prefix("#/") else {
                return true; // only local references are written
            };
            let mut node = &spec;
            for part in pointer.split('/') {
                let part = part.replace("~1", "/").replace("~0", "~");
                match node.get(part.as_str()) {
                    Some(next) => node = next,
                    None => return true,
                }
            }
            false
        })
        .collect();
    assert!(
        dangling.is_empty(),
        "unresolved $ref in openapi.yaml: {dangling:?}"
    );
}

/// #889: `/v1/config` names Tesseract's installed languages — the web UI's
/// `ocr_lang` hints — or `null` without a runnable binary. Whatever the host
/// has: the list matches `tesseract --list-langs` when one is installed.
#[tokio::test]
async fn config_reports_tesseract_languages() {
    let response = app()
        .oneshot(Request::get("/v1/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let langs = &v["tesseract_languages"];
    match docling::tesseract_languages() {
        Ok(expected) => assert_eq!(langs, &serde_json::json!(expected), "{v}"),
        Err(_) => assert!(langs.is_null(), "{v}"),
    }
}

#[tokio::test]
async fn ready_without_warmup_is_immediate() {
    let response = app()
        .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Ready at once, and honest that the models load on first use (#548).
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(v["models"], "lazy", "{v}");
}

/// #548: `--warmup` really loads the models before `/ready` turns 200, and a
/// failed load keeps it 503 with the reason instead of reporting ready over
/// an empty pipeline. With the models present the warmup must succeed; on a
/// runner without them it must fail loudly — either way it must settle.
#[tokio::test(flavor = "multi_thread")]
async fn warmup_settles_ready_or_failed() {
    let models = ml_models_ready();
    let app = router(ServeConfig {
        warmup: true,
        ..ServeConfig::default()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let (status, v) = loop {
        let response = app
            .clone()
            .oneshot(Request::get("/ready").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        if v["status"] != "warming_up" {
            break (status, v);
        }
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert!(std::time::Instant::now() < deadline, "warmup never settled");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    if status == StatusCode::OK {
        assert_eq!(v, serde_json::json!({"status": "ready", "models": "warm"}));
    } else {
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert_eq!(v["status"], "warmup_failed", "{v}");
        assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()), "{v}");
        assert!(!models, "the models are present, yet warmup failed: {v}");
    }
}

/// #547: `compact_tables` reaches PDF (and image) input too — those convert
/// on the warm pipeline, which used to skip the document settings the
/// declarative converter applies, so a compact request came back padded.
#[tokio::test]
async fn compact_tables_applies_to_pdf_input() {
    if !ml_models_ready() {
        eprintln!("skipping: the ML models are not present");
        return;
    }
    let pdf = std::fs::read(repo_root().join("tests/data/pdf/sources/2305.03393v1-pg9.pdf"))
        .expect("fixture");
    let mut out = Vec::new();
    for compact in ["true", "false"] {
        let (ct, body) = multipart(
            "table.pdf",
            &pdf,
            &[("to", "md"), ("compact_tables", compact)],
        );
        let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        out.push(body_string(response).await);
    }
    let separator = |md: &str| {
        md.lines()
            .find(|l| l.starts_with('|') && l.trim_matches(|c| "|-: ".contains(c)).is_empty())
            .map(str::to_string)
    };
    let (compact, padded) = (separator(&out[0]), separator(&out[1]));
    assert!(
        compact.as_deref().is_some_and(|l| l.starts_with("| - |")),
        "compact table expected: {compact:?}"
    );
    assert!(
        padded.as_deref().is_some_and(|l| l.starts_with("|--")),
        "padded table expected by default: {padded:?}"
    );
}

/// #896: `keep_picture_images=false` reaches PDF (and image) input too —
/// those convert on the warm pipeline, which skipped the picture enrichment
/// pass `convert` runs for every other format, so every picture came back
/// with its bytes whatever the request said. Kept by default.
#[tokio::test]
async fn keep_picture_images_false_applies_to_pdf_input() {
    if !ml_models_ready() {
        eprintln!("skipping: the ML models are not present");
        return;
    }
    let pdf = std::fs::read(repo_root().join("tests/data/pdf/sources/picture_classification.pdf"))
        .expect("fixture");
    let mut counts = Vec::new();
    for keep in ["true", "false"] {
        let (ct, body) = multipart(
            "pictures.pdf",
            &pdf,
            &[
                ("to", "json"),
                ("no_ocr", "true"),
                ("keep_picture_images", keep),
            ],
        );
        let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        let pictures = v["pictures"].as_array().expect("pictures");
        assert!(!pictures.is_empty(), "the fixture has pictures");
        counts.push((
            pictures.len(),
            pictures.iter().filter(|p| p.get("image").is_some()).count(),
        ));
    }
    assert_eq!(counts[0].0, counts[0].1, "kept by default: {counts:?}");
    assert_eq!(counts[1].1, 0, "dropped on request: {counts:?}");
}

/// #892: `ui: Some(false)` serves the API without the page — `/` is a 404
/// naming the API, the logo is gone, while the API description, the probes
/// and conversions answer as before. On by default.
#[tokio::test]
async fn ui_can_be_switched_off_keeping_the_api() {
    let page = app()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK, "the UI is on by default");
    assert!(body_string(page).await.contains("<html"));

    let api_only = || {
        router(ServeConfig {
            ui: Some(false),
            ..ServeConfig::default()
        })
    };
    let get = |path: &'static str| async move {
        api_only()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    };
    let root = get("/").await;
    assert_eq!(root.status(), StatusCode::NOT_FOUND);
    let body = body_string(root).await;
    assert!(!body.contains("<html"), "{body}");
    assert!(body.contains("/openapi.yaml"), "{body}");
    assert_eq!(get("/logo.svg").await.status(), StatusCode::NOT_FOUND);
    for path in ["/openapi.yaml", "/health", "/ready", "/v1/config"] {
        assert_eq!(get(path).await.status(), StatusCode::OK, "{path}");
    }
    let (ct, body) = multipart("note.md", b"# Title\n\nHello.\n", &[]);
    let response = api_only()
        .oneshot(convert_request(&ct, body, ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("# Title"));
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
    // A declarative conversion carries no confidence report (#183): no
    // header, no `confidence` key.
    assert!(response.headers().get("x-docling-confidence").is_none());
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(v["schema_name"], "DoclingDocument");
    assert!(v.get("confidence").is_none());
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

/// docling-core's `formatting_fallback_mode` (#1205): `html` writes the
/// underline and sub/superscript of the Markdown as tags — as a multipart
/// field, a query parameter, or under the `md_` alias — and `none`, the
/// default, drops them.
#[tokio::test]
async fn formatting_fallback_mode_tags_the_markdown() {
    let page = b"<p>H<sub>2</sub>O and <u>under</u></p>";
    let md = |fields: &[(&str, &str)], query: &str| {
        let (ct, body) = multipart("p.html", page, fields);
        let request = convert_request(&ct, body, query);
        async move {
            let response = app().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            body_string(response).await
        }
    };
    let tagged = "H<sub>2</sub>O and <u>under</u>";
    assert!(md(&[("formatting_fallback_mode", "html")], "")
        .await
        .contains(tagged));
    assert!(md(&[], "?formatting_fallback_mode=html")
        .await
        .contains(tagged));
    assert!(md(&[("md_formatting_fallback_mode", "html")], "")
        .await
        .contains(tagged));
    assert!(md(&[], "").await.contains("H2O and under"));
    let (ct, body) = multipart("p.html", page, &[("formatting_fallback_mode", "tags")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
    // Binary bytes under an unknown extension (text ones are plain text
    // since #671).
    let (ct, body) = multipart("data.xyz", b"\x00\x01\x02\xff\xfe\x00", &[]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// #624: an encrypted Office file is a 422 naming the encryption — the
/// `.ppt` used to stream an empty 200, the `.pptx` failed as "bad zip".
#[tokio::test]
async fn encrypted_office_files_are_422() {
    for name in ["min_encrypted.ppt", "min_encrypted.pptx"] {
        let bytes = std::fs::read(
            repo_root()
                .join("crates/docling/tests/data/encrypted")
                .join(name),
        )
        .unwrap();
        for query in ["", "?to=json"] {
            let (ct, body) = multipart(name, &bytes, &[]);
            let response = app()
                .oneshot(convert_request(&ct, body, query))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{name}{query}"
            );
            let text = body_string(response).await;
            assert!(
                text.contains("document is encrypted"),
                "{name}{query}: {text}"
            );
            // #636: the body names the case, so a client prompts for a
            // password instead of matching the message.
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["code"], "password_required", "{name}{query}: {text}");
        }
    }
}

/// #636: an encrypted PDF carries the `code` too, on the pipeline route the
/// Office formats never take (it used to answer 500 there), with docling's
/// message; the password opens it. Text-layer only: no models needed.
#[tokio::test]
async fn an_encrypted_pdf_is_a_422_with_a_code() {
    let bytes =
        std::fs::read(repo_root().join("tests/data/pdf_password/sources/2206.01062_pg3.pdf"))
            .unwrap();
    for (fields, code, text) in [
        (
            vec![("text_layer_only", "true")],
            "password_required",
            "the PDF is encrypted: a password is required",
        ),
        (
            vec![("text_layer_only", "true"), ("password", "nope")],
            "wrong_password",
            "the PDF is encrypted and the password is wrong",
        ),
    ] {
        let (ct, body) = multipart("locked.pdf", &bytes, &fields);
        let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{code}"
        );
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(v["code"], code, "{v}");
        assert!(v["error"].as_str().unwrap().contains(text), "{v}");
    }
    let (ct, body) = multipart(
        "locked.pdf",
        &bytes,
        &[("text_layer_only", "true"), ("password", "1234")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("DocLayNet"));
}

/// #636: a damaged upload is a 422 *without* a `code` — only an encrypted
/// document carries one — and an async job's status and result carry the
/// same `code` as the sync endpoint.
#[tokio::test]
async fn the_encryption_code_is_only_on_encrypted_documents() {
    let (ct, body) = multipart("junk.docx", b"PK\x03\x04 not a zip", &[]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(v["error"].is_string());
    assert!(v.get("code").is_none(), "{v}");

    let app = app();
    let bytes =
        std::fs::read(repo_root().join("crates/docling/tests/data/encrypted/min_encrypted.docx"))
            .unwrap();
    let (ct, body) = multipart("min_encrypted.docx", &bytes, &[("password", "4321")]);
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/status/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        match v["task_status"].as_str().unwrap() {
            "failure" => break v,
            "success" => panic!("an encrypted file converted without its password: {v}"),
            _ if std::time::Instant::now() > deadline => panic!("job never finished"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    };
    assert_eq!(status["code"], "wrong_password", "{status}");
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .contains("the password is wrong"),
        "{status}"
    );
    let response = app
        .oneshot(
            Request::get(format!("/v1/result/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(v["code"], "wrong_password", "{v}");
}

/// #625: the request's `password` opens an encrypted Office file too (and
/// `pdf_password`, its earlier name, still reads);
/// a wrong one is a 422 saying so.
#[tokio::test]
async fn password_opens_an_encrypted_office_file() {
    let bytes =
        std::fs::read(repo_root().join("crates/docling/tests/data/encrypted/min_encrypted.pptx"))
            .unwrap();
    for field in ["password", "pdf_password"] {
        let (ct, body) = multipart("deck.pptx", &bytes, &[(field, "1234")]);
        let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{field}");
        assert!(body_string(response).await.contains("Hello"), "{field}");
    }
    let (ct, body) = multipart("deck.pptx", &bytes, &[]);
    let response = app()
        .oneshot(convert_request(&ct, body, "?password=1234"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "query parameter");
    let (ct, body) = multipart("deck.pptx", &bytes, &[("password", "4321")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let text = body_string(response).await;
    assert!(text.contains("the password is wrong"), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["code"], "wrong_password", "{text}");
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
/// render work, so this runs in plain CI.
#[tokio::test]
async fn images_output_requires_a_pdf_input() {
    let (ct, body) = multipart("x.md", b"# hi", &[("to", "images")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_string(response).await.contains("PDF inputs only"));
}

/// #460: `ocr_engine` is validated up front, and `ocr_lang` against the
/// engine it will drive — `deu` is a language to Tesseract only, `xx` to
/// neither.
#[tokio::test]
async fn ocr_engine_and_lang_are_validated_together() {
    // The rules are `ConvertOptions::validate`'s (options.rs); here: the
    // cross-field check reaches the request as a 400 naming the field…
    let (ct, body) = multipart(
        "x.md",
        b"# hi",
        &[("ocr_engine", "tesseract"), ("ocr_lang", "xx")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("ocr_lang"));

    // A Tesseract language list passes on a non-OCR format.
    let (ct, body) = multipart(
        "x.md",
        b"# hi",
        &[("ocr_engine", "tesseract"), ("ocr_lang", "deu+iso:fr")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
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

/// #497: `document_timeout` must be a positive number of seconds; it is
/// validated by the shared converter builder, so a plain Markdown upload
/// exercises it without any model work. A valid value passes through (the
/// budget only ever acts on the PDF pipeline).
#[tokio::test]
async fn document_timeout_is_validated() {
    let (ct, body) = multipart("x.md", b"# hi", &[("document_timeout", "0")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("document_timeout"));

    let (ct, body) = multipart("x.md", b"# hi", &[("document_timeout", "soon")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("document_timeout"));

    let (ct, body) = multipart("x.md", b"# hi", &[("document_timeout", "90")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // A whole conversion carries no partial-status header.
    assert!(response.headers().get("x-docling-status").is_none());
}

/// `--to images`' `scale` outside 0.1–4.0 is rejected before any page is rendered.
#[tokio::test]
async fn to_images_scale_is_validated() {
    let (ct, body) = multipart("x.pdf", b"%PDF-1.4", &[("to", "images"), ("scale", "9")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("scale"));
}

/// `images_scale` (#520) outside 0.1–4.0 is a 400 before any page renders.
#[tokio::test]
async fn images_scale_option_is_validated() {
    let (ct, body) = multipart("x.pdf", b"%PDF-1.4", &[("images_scale", "9")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("images_scale"));
}

/// End-to-end rasterization over a real one-page fixture — the pure-Rust
/// renderer, so it runs in plain CI.
#[tokio::test]
async fn rasterizes_pdf_pages_to_png() {
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

/// #246 end-to-end: a `text_layer_only=true` request (`no_ocr` before 2.0,
/// #611) must not degrade the shared warm pipeline for later default
/// requests. The table fixture makes the difference decisive — the full
/// pipeline emits a Markdown table (pipes), the text-layer path emits flat
/// paragraphs — and `no_ocr=true`, docling's `--no-ocr` since 2.0, keeps the
/// table. Needs the layout/OCR/TableFormer models, so it skips on a
/// model-free checkout.
#[tokio::test]
async fn no_ocr_request_does_not_stick_to_the_warm_pipeline() {
    if !ml_models_ready() {
        eprintln!("skipping: the ML models are not present");
        return;
    }
    let pdf =
        std::fs::read(repo_root().join("tests/data/pdf/sources/2305.03393v1-pg9.pdf")).unwrap();
    let app = app();
    let run = |text_layer_only: &'static str, app: axum::Router| {
        let (ct, body) = multipart(
            "t.pdf",
            &pdf,
            &[("to", "md"), ("text_layer_only", text_layer_only)],
        );
        async move {
            let response = app.oneshot(convert_request(&ct, body, "")).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            body_string(response).await
        }
    };
    let full1 = run("false", app.clone()).await;
    assert!(full1.contains('|'), "full pipeline must emit the table");
    let reduced = run("true", app.clone()).await;
    assert!(
        !reduced.contains('|'),
        "the text-layer path has no TableFormer"
    );
    // The bug: this third request reused the degraded pipeline and returned
    // the flat text-layer output until the server restarted.
    let full2 = run("false", app.clone()).await;
    assert_eq!(
        full1, full2,
        "default request after text_layer_only must fully recover"
    );
    // docling's `--no-ocr`: layout and TableFormer still run.
    let (ct, body) = multipart("t.pdf", &pdf, &[("to", "md"), ("no_ocr", "true")]);
    let response = app.oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body_string(response).await.contains('|'),
        "no_ocr keeps the table (docling's do_ocr=False)"
    );
}

/// #611: `skip_ocr` is `no_ocr`'s pre-2.0 name, still read — and a JSON body
/// carrying both spellings (a pre-2.0 client that sent both switches) is a
/// conversion, not a duplicate-field 400.
#[tokio::test]
async fn both_ocr_spellings_in_one_json_body_convert() {
    let b64 = docling::base64::encode(b"a,b\n1,2\n");
    let body = format!(
        r#"{{"no_ocr": false, "skip_ocr": true, "text_layer_only": false,
            "sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "t.csv"}}]}}"#
    );
    let response = app().oneshot(json_convert(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    assert!(text.contains("|   1 |   2 |"), "{text}");
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

/// Serializes the tests that toggle `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH`
/// (process-global): without it, one test's `remove_var` can strip the escape
/// hatch out from under another's in-flight loopback fetch/PUT. Tokio's mutex
/// because the guard is held across the tests' awaits.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `fetch_images` is outbound fetch (SSRF surface), so it's gated behind the
/// same `--allow-url-fetch` as URL inputs: honored only when the flag is on,
/// silently ignored otherwise. Proven against a local image server that counts
/// the requests it receives — the gate must let *zero* through when off.
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

/// The three uploads of the batch tests: two documents and, under an
/// unknown extension, binary that no backend reads (text would read as
/// plain text since #671).
const BATCH_FILES: [(&str, &[u8]); 3] = [
    ("a.md", b"# A\n"),
    ("b.csv", b"x,y\n1,2\n"),
    ("broken.xyz", b"\x00\x01\x02\xff\xfe\x00"),
];

/// Several `file` parts in one request convert as a batch (#182). For
/// Markdown the answer is Markdown, as for one file: the documents in
/// request order, the separator line between them, the bad file as one
/// `Error converting …` line in its place — a 200, streamed.
#[tokio::test]
async fn batch_markdown_is_one_separated_body() {
    let (ct, body) = multipart_files(&BATCH_FILES, &[("to", "md")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/markdown; charset=utf-8"
    );
    let out = body_string(response).await;
    let sep = format!("\n\n{}\n\n", docling_serve::BATCH_SEPARATOR);
    let parts: Vec<&str> = out.split(sep.as_str()).collect();
    assert_eq!(parts.len(), 3, "{out:?}");
    assert_eq!(parts[0], "# A");
    assert!(
        parts[1].starts_with("|   x |   y |") && parts[1].ends_with("|   1 |   2 |"),
        "{:?}",
        parts[1]
    );
    assert!(
        parts[2].starts_with("Error converting broken.xyz: "),
        "{:?}",
        parts[2]
    );
    assert!(
        parts[2].contains("xyz") && !parts[2].contains('\n'),
        "{:?}",
        parts[2]
    );
    // The same body buffered — what an async job stores — down to the byte.
    let (ct, body) = multipart_files(&BATCH_FILES, &[("to", "md")]);
    let stored = async_result(&ct, body).await;
    assert_eq!(
        stored.headers()[header::CONTENT_TYPE],
        "text/markdown; charset=utf-8"
    );
    assert_eq!(stored.headers()["x-docling-status"], "partial_success");
    assert!(stored.headers()["x-docling-errors"]
        .to_str()
        .unwrap()
        .contains("broken.xyz"));
    assert_eq!(body_string(stored).await, out);
}

/// Submit `body` to `/v1/convert/async`, wait for the job, fetch its result.
async fn async_result(ct: &str, body: Vec<u8>) -> axum::response::Response {
    let app = app();
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
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        match v["task_status"].as_str().unwrap() {
            "success" => break,
            "failure" => panic!("job failed: {v}"),
            _ if std::time::Instant::now() > deadline => panic!("job never finished"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    app.oneshot(
        Request::get(format!("/v1/result/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

/// An HTML batch is one page: the first document's skeleton holding every
/// document's `page` div, `<hr><hr><hr>` between them, the bad file an
/// error paragraph; a LaTeX batch one document, `\clearpage` between the
/// bodies, the later document's `\title` ahead of its `\maketitle`.
#[tokio::test]
async fn batch_html_and_latex_are_one_document() {
    let (ct, body) = multipart_files(&BATCH_FILES, &[("to", "html")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let out = body_string(response).await;
    assert_eq!(out.matches("<!DOCTYPE html>").count(), 1, "{out}");
    assert_eq!(out.matches("<body>").count(), 1, "{out}");
    assert_eq!(out.matches("<hr><hr><hr>").count(), 2, "{out}");
    assert_eq!(out.matches("<div class='page'>").count(), 2, "{out}");
    let a = out.find("<h1>A</h1>").expect("a.md");
    let table = out.find("<table").expect("b.csv");
    let err = out
        .find("<p>Error converting broken.xyz: ")
        .expect("error paragraph");
    assert!(a < table && table < err, "{out}");
    assert!(out.trim_end().ends_with("</body>\n</html>"), "{out}");

    let (ct, body) = multipart_files(&BATCH_FILES, &[("to", "latex")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/x-tex; charset=utf-8"
    );
    let out = body_string(response).await;
    assert_eq!(out.matches("\\documentclass").count(), 1, "{out}");
    assert_eq!(out.matches("\\begin{document}").count(), 1, "{out}");
    assert_eq!(out.matches("\\end{document}").count(), 1, "{out}");
    assert_eq!(out.matches("\\clearpage").count(), 2, "{out}");
    assert!(out.contains("\\title{A}"), "{out}");
    assert!(
        out.contains("\\textbf{Error converting broken.xyz: "),
        "{out}"
    );
    assert!(out.ends_with("\\end{document}"), "{out}");
}

/// Any other format answers a JSON results array with per-item status — one
/// bad file fails only its own item.
#[tokio::test]
async fn batch_upload_returns_results_array() {
    let (ct, body) = multipart_files(&BATCH_FILES, &[("to", "json")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["status"], "success");
    assert_eq!(results[0]["document"]["texts"][0]["text"], "A", "{v}");
    assert_eq!(results[1]["status"], "success");
    assert_eq!(results[2]["status"], "failure");
    assert!(results[2]["error"].as_str().unwrap().contains("xyz"));
}

/// Async job lifecycle (#182): submit returns a task id, status reaches
/// `success`, and the result endpoint replays the sync response (here: the
/// converted Markdown). Unknown ids 404.
/// #783: a job's status carries the conversion's progress — the document
/// under way and its page counts — and a finished job keeps the last ones.
#[tokio::test]
async fn async_job_status_reports_progress() {
    let app = app();
    let pdf = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/pdf/sources/skipped_1page.pdf"),
    )
    .expect("3-page PDF fixture");
    let (ct, body) = multipart("skipped_1page.pdf", &pdf, &[("text_layer_only", "true")]);
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
    // Pending: nothing has started, so no progress yet.
    assert!(v.get("progress").is_none(), "{v}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/status/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        match v["task_status"].as_str().unwrap() {
            "success" => break v,
            "failure" => panic!("job failed: {v}"),
            _ if std::time::Instant::now() > deadline => panic!("job never finished"),
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    };
    let progress = &status["progress"];
    assert_eq!(progress["documents_started"], 1, "{status}");
    assert_eq!(progress["documents_total"], 1, "{status}");
    assert_eq!(progress["document_index"], 1, "{status}");
    assert!(
        progress["document_name"]
            .as_str()
            .is_some_and(|n| n.starts_with("skipped_1page")),
        "{status}"
    );
    assert_eq!(progress["completed_pages"], 3, "{status}");
    assert_eq!(progress["total_pages"], 3, "{status}");
    assert!(progress.get("enrichment").is_none(), "{status}");
}

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

#[tokio::test]
async fn index_serves_docs_and_form() {
    let response = app()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains(r#"<input type="file""#), "the test form");
    assert!(body.contains("/v1/convert"), "the API docs");
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
async fn xbrl_taxonomy_path_escape_is_a_400() {
    // #466: the taxonomy directory is a server-local relative path, held to
    // the `chunk_tokenizer` rule — no traversal, no absolute paths.
    for bad in ["../taxonomies", "/etc"] {
        let (ct, body) = multipart("t.csv", b"a,b\n1,2\n", &[("xbrl_taxonomy", bad)]);
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

fn json_convert(body: &str) -> Request<Body> {
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
    let response = app().oneshot(json_convert(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let results = v["results"].as_array().expect("batch shape");
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r["status"] == "success"), "{v}");
}

fn docling_b64(text: &str) -> String {
    docling::base64::encode(text.as_bytes())
}

#[tokio::test]
async fn url_and_sources_together_is_a_400() {
    let b64 = docling_b64("x");
    let body = format!(
        r#"{{"url": "https://example.com/a.md",
            "sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "a.csv"}}]}}"#
    );
    let response = app().oneshot(json_convert(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_source_kind_is_a_400() {
    let body = r#"{"sources": [{"kind": "google_drive", "document_id": "x"}]}"#;
    let response = app().oneshot(json_convert(body)).await.unwrap();
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
    let response = app().oneshot(json_convert(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "a.csv"}}],
            "target": {s3}}}"#
    );
    let response = app().oneshot(json_convert(&body)).await.unwrap();
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
    let response = router(cfg).oneshot(json_convert(body)).await.unwrap();
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
    let response = app().oneshot(json_convert(&body)).await.unwrap();
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
/// needs no models, so these tests run in plain CI.
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
    let response = router(cfg).oneshot(json_convert(&body)).await.unwrap();
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
    let response = app().oneshot(json_convert(&body)).await.unwrap();
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
    let response = app().oneshot(json_convert(&body)).await.unwrap();
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
    // Binary bytes: text under an unknown extension is plain text since #671.
    let b64 = docling::base64::encode(b"\x00\x01\x02\xff\xfe\x00");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "wat.unknown"}}],
            "target": {{"kind": "zip"}}, "to": "md"}}"#
    );
    let response = app().oneshot(json_convert(&body)).await.unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
}

#[tokio::test]
async fn zip_target_batch_converts_around_a_bad_item() {
    let b64 = docling_b64("a,b\n1,2\n");
    // Binary bytes: text under an unknown extension is plain text since #671.
    let bad = docling::base64::encode(b"\x00\x01\x02\xff\xfe\x00");
    let body = format!(
        r#"{{"sources": [
            {{"kind": "file", "base64_string": "{b64}", "filename": "good.csv"}},
            {{"kind": "file", "base64_string": "{bad}", "filename": "bad.unknown"}}
        ], "target": {{"kind": "zip"}}, "to": "md"}}"#
    );
    let response = app().oneshot(json_convert(&body)).await.unwrap();
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
    let response = app().oneshot(json_convert(&body)).await.unwrap();
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
    let response = app().oneshot(json_convert(&body)).await.unwrap();
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
    let response = router(cfg).oneshot(json_convert(&body)).await.unwrap();
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
    let response = router(cfg).oneshot(json_convert(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(response).await.contains("private/loopback"));
}

// --- #515: Pandoc AST output ----------------------------------------------

#[tokio::test]
async fn pandoc_output_is_the_ast_as_json() {
    let (ct, body) = multipart(
        "note.md",
        b"# Title\n\nHello **world**.\n\n- one\n- two\n",
        &[("to", "pandoc")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(v["pandoc-api-version"], serde_json::json!([1, 23, 1, 1]));
    let kinds: Vec<&str> = v["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["t"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Header", "Para", "BulletList"], "{v}");
}

#[tokio::test]
async fn pandoc_batch_items_and_api_version_check() {
    // Batch: the AST inline as an object per item.
    let (ct, body) = multipart_files(
        &[("a.md", b"# A\n"), ("b.md", b"# B\n")],
        &[("to", "pandoc"), ("pandoc_api_version", "1.23")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let results = v["results"].as_array().expect("batch shape");
    assert_eq!(results[1]["pandoc"]["blocks"][0]["t"], "Header", "{v}");

    // An API version this export does not write is refused up front.
    let (ct, body) = multipart(
        "note.md",
        b"# A\n",
        &[("to", "pandoc"), ("pandoc_api_version", "1.22")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let msg = body_string(response).await;
    assert!(
        msg.contains("unsupported Pandoc API version '1.22'"),
        "{msg}"
    );
}

// --- #614: WebVTT output -------------------------------------------------

#[tokio::test]
async fn vtt_output_round_trips_cues_and_batches_inline() {
    let vtt = b"WEBVTT\n\n00:00:01.000 --> 00:00:02.500\n<v Ann>Hello & welcome\n";
    let (ct, body) = multipart("talk.vtt", vtt, &[("to", "vtt")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/vtt; charset=utf-8"
    );
    assert_eq!(
        body_string(response).await,
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\n<v Ann>Hello &amp; welcome"
    );
    // A document without timed text is the bare header; batch items carry
    // the file inline under `vtt`.
    let (ct, body) = multipart_files(&[("a.vtt", vtt), ("b.md", b"# B\n")], &[("to", "vtt")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(
        v["results"][0]["vtt"].as_str().unwrap().contains("<v Ann>"),
        "{v}"
    );
    assert_eq!(v["results"][1]["vtt"], "WEBVTT B", "{v}");
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
    // Batch: one document; the second file's title travels into the body,
    // ahead of its `\maketitle`.
    let (ct, body) = multipart_files(
        &[("a.md", b"# A\n"), ("b.md", b"# B\n")],
        &[("to", "latex")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let out = body_string(response).await;
    assert_eq!(out.matches("\\documentclass").count(), 1, "{out}");
    let title_a = out.find("\\title{A}").expect("title A");
    let begin = out.find("\\begin{document}").unwrap();
    let title_b = out.find("\\title{B}").expect("title B");
    assert!(title_a < begin && begin < title_b, "{out}");
    assert!(out.contains("\\title{B}\n\\maketitle"), "{out}");

    // Zip target: `<stem>.tex` entries (OutputNames knows the extension).
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "one.csv"}}],
            "target": {{"kind": "zip"}}, "to": "latex"}}"#
    );
    let response = app().oneshot(json_convert(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        bytes.windows(7).any(|w| w == b"one.tex"),
        "tex entry missing"
    );
}

// --- #613: plain-text output --------------------------------------------

#[tokio::test]
async fn text_output_is_undecorated_plain_text() {
    let (ct, body) = multipart(
        "note.md",
        b"# Title\n\nSome **bold** and [a link](https://example.com)\n\n- one\n- two\n",
        &[("to", "text")],
    );
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    let out = body_string(response).await;
    assert_eq!(out, "Title\n\nSome bold and a link\n\n- one\n- two");

    // A batch is one text body, the separator line between the documents;
    // a zip target names it `<stem>.txt`.
    let (ct, body) = multipart_files(&[("a.md", b"# A\n"), ("b.md", b"*B*\n")], &[("to", "text")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(
        body_string(response).await,
        format!("A\n\n{}\n\nB", docling_serve::BATCH_SEPARATOR)
    );
    let b64 = docling_b64("a,b\n1,2\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "one.csv"}}],
            "target": {{"kind": "zip"}}, "to": "text"}}"#
    );
    let response = app().oneshot(json_convert(&body)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        bytes.windows(7).any(|w| w == b"one.txt"),
        "txt entry missing"
    );
}

/// A `.zip` upload converts every document inside as a batch (#557): one
/// results item per convertible entry, a broken one failing only itself, an
/// unsupported one left out.
#[tokio::test]
async fn zip_upload_converts_each_document() {
    let zip = docling::dclx::zip_bytes(vec![
        ("docs/a.md", b"# A\n\nalpha".as_slice()),
        ("b.csv", b"x,y\n1,2\n".as_slice()),
        ("tool.exe", b"MZ".as_slice()),
        ("broken.docx", b"not a zip".as_slice()),
    ]);
    let (ct, body) = multipart_files(&[("bundle.zip", &zip)], &[("to", "json")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3, "{v}");
    assert_eq!(results[0]["status"], "success");
    assert_eq!(results[0]["document"]["texts"][1]["text"], "alpha", "{v}");
    assert_eq!(results[1]["status"], "success");
    assert_eq!(results[2]["status"], "failure");
    // As Markdown: one body, the documents separated, the broken one a line.
    let (ct, body) = multipart_files(&[("bundle.zip", &zip)], &[("to", "md")]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let out = body_string(response).await;
    assert_eq!(
        out.matches(docling_serve::BATCH_SEPARATOR).count(),
        2,
        "{out:?}"
    );
    assert!(out.starts_with("# A\n\nalpha"), "{out:?}");
    // Entries are named by stem, as the JSON items are.
    assert!(out.contains("Error converting broken: "), "{out:?}");

    // Nothing convertible inside: a client error, not an empty success.
    let only_exe = docling::dclx::zip_bytes(vec![("tool.exe", b"MZ".as_slice())]);
    let (ct, body) = multipart("bundle.zip", &only_exe, &[]);
    let response = app().oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// --- #615: upstream docling-serve's API ----------------------------------

/// A multipart body the way upstream's clients send it: every upload as a
/// `files` part, options as repeated text parts.
fn upload(files: &[(&str, &[u8])], fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    let boundary = "docling-serve-compat-boundary";
    let mut body = Vec::new();
    for (file_name, content) in files {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
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

fn post(uri: &str, content_type: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&body_string(response).await).unwrap()
}

/// Open WebUI's `DoclingLoader` request, replayed: `files` named after the
/// upload's path, `image_export_mode=placeholder`, a form-feed page-break
/// marker and a `DOCLING_PARAMS` full of upstream-only knobs. The answer is
/// docling's `ConvertDocumentResponse` with the pages split on `\f`.
#[tokio::test]
async fn open_webui_request_gets_a_convert_document_response() {
    let pptx =
        std::fs::read(repo_root().join("tests/data/pptx/sources/powerpoint_sample.pptx")).unwrap();
    let (ct, body) = upload(
        &[(
            "/app/backend/data/uploads/1b2c_powerpoint_sample.pptx",
            &pptx,
        )],
        &[
            ("image_export_mode", "placeholder"),
            ("md_page_break_placeholder", "\u{c}"),
            ("do_ocr", "true"),
            ("ocr_engine", "easyocr"),
            ("ocr_lang", "en"),
            ("pdf_backend", "dlparse_v4"),
            ("table_mode", "accurate"),
            ("do_picture_description", "false"),
        ],
    );
    let response = app()
        .oneshot(post("/v1/convert/file", &ct, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let v = json_body(response).await;
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["errors"], serde_json::json!([]), "{v}");
    assert!(v["processing_time"].as_f64().is_some(), "{v}");
    let doc = &v["document"];
    assert_eq!(doc["filename"], "1b2c_powerpoint_sample.pptx");
    let md = doc["md_content"].as_str().expect("md_content");
    assert!(md.split('\u{c}').count() >= 2, "pages split on \\f: {md:?}");
    assert!(!md.contains("data:image"), "placeholder images: {md}");
    assert!(doc["json_content"].is_null() && doc["text_content"].is_null());
}

#[tokio::test]
async fn to_formats_fill_their_content_fields() {
    let (ct, body) = upload(
        &[("note.md", b"# Title\n\nSome **bold** text.\n")],
        &[
            ("to_formats", "md"),
            ("to_formats", "json"),
            ("to_formats", "text"),
        ],
    );
    let response = app()
        .oneshot(post("/v1/convert/file", &ct, body))
        .await
        .unwrap();
    let v = json_body(response).await;
    let doc = &v["document"];
    assert_eq!(doc["json_content"]["schema_name"], "DoclingDocument", "{v}");
    assert_eq!(doc["text_content"], "Title\n\nSome bold text.");
    assert!(doc["md_content"].as_str().unwrap().contains("# Title"));
    assert!(doc["html_content"].is_null());
}

/// A document that does not convert is a `200` envelope with `status:
/// "failure"` and the reason in `errors[0].error_message` — upstream's
/// clients read failures there, not from the HTTP status.
#[tokio::test]
async fn a_corrupt_upload_is_a_failure_envelope() {
    let (ct, body) = upload(&[("broken.docx", b"not a zip at all")], &[]);
    let response = app()
        .oneshot(post("/v1/convert/file", &ct, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v = json_body(response).await;
    assert_eq!(v["status"], "failure", "{v}");
    assert!(
        !v["errors"][0]["error_message"].as_str().unwrap().is_empty(),
        "{v}"
    );
    assert!(v["document"]["md_content"].is_null());
    assert_eq!(v["document"]["filename"], "broken.docx");
}

/// Several documents answer docling-serve's zip of `<stem>.<ext>` outputs.
#[tokio::test]
async fn several_files_answer_a_zip() {
    let (ct, body) = upload(
        &[("a.md", b"# A\n"), ("b.csv", b"x,y\n1,2\n")],
        &[("to_formats", "md"), ("to_formats", "json")],
    );
    let response = app()
        .oneshot(post("/v1/convert/file", &ct, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/zip");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    for name in ["a.md", "a.json", "b.md", "b.json"] {
        assert!(
            bytes.windows(name.len()).any(|w| w == name.as_bytes()),
            "{name} missing"
        );
    }
}

#[tokio::test]
async fn convert_source_takes_base64_files_and_gates_http() {
    let b64 = docling_b64("# From base64\n");
    let body = format!(
        r#"{{"sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "doc.md"}}],
            "options": {{"to_formats": ["md", "text"], "do_ocr": false, "page_range": [1, 9223372036854775807]}}}}"#
    );
    let response = app()
        .oneshot(post(
            "/v1/convert/source",
            "application/json",
            body.into_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let v = json_body(response).await;
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["document"]["filename"], "doc.md");
    assert_eq!(v["document"]["md_content"], "# From base64\n");
    assert_eq!(v["document"]["text_content"], "From base64");

    // docling-serve 0.x's `file_sources`, on the `/v1alpha` alias.
    let body =
        format!(r#"{{"file_sources": [{{"base64_string": "{b64}", "filename": "old.md"}}]}}"#);
    let response = app()
        .oneshot(post(
            "/v1alpha/convert/source",
            "application/json",
            body.into_bytes(),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(response).await["status"], "success");

    // A URL source needs --allow-url-fetch; nothing is fetched without it.
    let body = r#"{"sources": [{"kind": "http", "url": "https://example.com/a.pdf"}]}"#;
    let response = app()
        .oneshot(post(
            "/v1/convert/source",
            "application/json",
            body.as_bytes().to_vec(),
        ))
        .await
        .unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
    assert!(body_string(response).await.contains("--allow-url-fetch"));
}

/// The async variants: a `TaskStatusResponse`, `/v1/status/poll/{id}` until
/// `success`, then the `ConvertDocumentResponse` from `/v1/result/{id}`.
#[tokio::test]
async fn async_file_conversion_polls_to_a_result() {
    let app = app();
    let (ct, body) = upload(&[("note.md", b"# Async\n")], &[]);
    let response = app
        .clone()
        .oneshot(post("/v1/convert/file/async", &ct, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let task = json_body(response).await;
    assert_eq!(task["task_type"], "convert", "{task}");
    let id = task["task_id"].as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/v1/status/poll/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = json_body(response).await;
        match status["task_status"].as_str().unwrap() {
            "success" => break,
            "pending" | "started" => {}
            other => panic!("task {other}: {status}"),
        }
        assert!(std::time::Instant::now() < deadline, "task never finished");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
    let v = json_body(response).await;
    assert_eq!(v["status"], "success", "{v}");
    assert_eq!(v["document"]["md_content"], "# Async\n");

    let response = app
        .oneshot(
            Request::get("/v1/status/poll/no-such-task")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `X-Api-Key` (docling-serve's `DOCLING_SERVE_API_KEY`): every `/v1` route
/// needs it once a key is configured; health and the docs page stay open.
#[tokio::test]
async fn api_key_guards_the_v1_routes() {
    let app = router(ServeConfig {
        api_key: Some("s3cret".into()),
        ..ServeConfig::default()
    });
    let (ct, body) = upload(&[("note.md", b"# Key\n")], &[]);
    let response = app
        .clone()
        .oneshot(post("/v1/convert/file", &ct, body.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let mut request = post("/v1/convert/file", &ct, body);
    request
        .headers_mut()
        .insert("x-api-key", "s3cret".parse().unwrap());
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// --- #619: the request body cap -------------------------------------------

/// A body over `--max-body-mb` is refused before conversion with 413 on
/// both the native and the upstream-compatible routes (docs/SECURITY.md's
/// resource-limit table names the status).
#[tokio::test]
async fn an_oversized_body_is_refused_with_413() {
    let app = router(ServeConfig {
        max_body_bytes: 1024,
        ..ServeConfig::default()
    });
    let big = vec![b'a'; 4096];
    for (uri, field) in [("/v1/convert", "file"), ("/v1/convert/file", "files")] {
        let boundary = "body-limit-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"a.md\"\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&big);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
    }
    // JSON bodies (URL / base64 sources) hit the same cap.
    let json = format!(r#"{{"url": "https://example.com/{}"}}"#, "a".repeat(4096));
    for uri in ["/v1/convert", "/v1/convert/source"] {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json.clone()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "{uri} (JSON)"
        );
    }
}

/// `text_layer_only=auto` over the wire — a query parameter, a multipart
/// field and a JSON string alike: a digital PDF converts from its text layer
/// (no model needed, so this runs on a model-free checkout), the same bytes
/// `text_layer_only=true` gives; a value that is neither a bool nor `auto`
/// is a 400 naming the option.
#[tokio::test]
async fn text_layer_only_auto_converts_a_digital_pdf() {
    let pdf = std::fs::read(repo_root().join("tests/data/pdf/sources/skipped_1page.pdf")).unwrap();
    let app = app();
    let run = |value: &'static str, app: axum::Router| {
        let (ct, body) = multipart("t.pdf", &pdf, &[("to", "md"), ("text_layer_only", value)]);
        async move {
            let response = app.oneshot(convert_request(&ct, body, "")).await.unwrap();
            let status = response.status();
            (status, body_string(response).await)
        }
    };
    let (status, on) = run("true", app.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, auto) = run("auto", app.clone()).await;
    assert_eq!(status, StatusCode::OK, "{auto}");
    assert!(!auto.trim().is_empty());
    assert_eq!(on, auto, "a digital PDF under auto is the text-layer path");
    let (status, err) = run("sometimes", app.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err.contains("text_layer_only"), "{err}");
    // The query-string spelling.
    let (ct, body) = multipart("t.pdf", &pdf, &[]);
    let response = app
        .clone()
        .oneshot(convert_request(&ct, body, "?to=md&text_layer_only=auto"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, on);
    // The JSON body, as the string.
    let b64 = docling::base64::encode(&pdf);
    let body = format!(
        r#"{{"to": "md", "text_layer_only": "auto",
            "sources": [{{"kind": "file", "base64_string": "{b64}", "filename": "t.pdf"}}]}}"#
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/convert/source")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// A one-page PDF whose content stream declares `extra` bytes more than it
/// holds (#934's overstated `/Length`; the reader recovers it).
fn pdf_with_stream_length_off_by(extra: usize) -> Vec<u8> {
    let content = b"BT /F1 12 Tf 20 100 Td (Repaired stream text) Tj ET\n";
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R \
           /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_vec(),
        [
            format!("<< /Length {} >>\nstream\n", content.len() + extra).as_bytes(),
            content,
            b"endstream",
        ]
        .concat(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
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

/// `pdf_report_repairs` (#934): off by default; on, the single response
/// carries the reader's repairs in `X-Docling-Pdf-Repairs` and a batch item
/// in `pdf_repairs` — the document itself is the same either way.
#[tokio::test]
async fn pdf_report_repairs_reports_a_recovered_stream() {
    let pdf = pdf_with_stream_length_off_by(7);
    let app = app();
    let convert = |fields: Vec<(&'static str, &'static str)>, app: axum::Router| {
        let (ct, body) = multipart("t.pdf", &pdf, &fields);
        async move { app.oneshot(convert_request(&ct, body, "")).await.unwrap() }
    };
    let plain = convert(vec![("to", "md"), ("text_layer_only", "true")], app.clone()).await;
    assert_eq!(plain.status(), StatusCode::OK);
    assert!(plain.headers().get("x-docling-pdf-repairs").is_none());
    let plain = body_string(plain).await;
    assert!(plain.contains("Repaired stream text"), "{plain}");

    let fields = vec![
        ("to", "md"),
        ("text_layer_only", "true"),
        ("pdf_report_repairs", "true"),
    ];
    let reported = convert(fields, app.clone()).await;
    assert_eq!(reported.status(), StatusCode::OK);
    let header: serde_json::Value = serde_json::from_str(
        reported.headers()["x-docling-pdf-repairs"]
            .to_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(header[0]["kind"], "stream_length");
    assert_eq!(header[0]["object"], serde_json::json!([4, 0]));
    assert_eq!(body_string(reported).await, plain);

    // A well-formed file: the header says so with an empty list.
    let good = pdf_with_stream_length_off_by(0);
    let (ct, body) = multipart(
        "t.pdf",
        &good,
        &[
            ("to", "md"),
            ("text_layer_only", "true"),
            ("pdf_report_repairs", "true"),
        ],
    );
    let response = app.oneshot(convert_request(&ct, body, "")).await.unwrap();
    assert_eq!(response.headers()["x-docling-pdf-repairs"], "[]");
}
