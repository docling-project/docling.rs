//! Minimal C ABI over the document converter (issue #193).
//!
//! Four calls carry the whole workflow — convert, read the output, read the
//! error, free — plus a version probe:
//!
//! ```c
//! DoclingResult *r = docling_convert(bytes, len, "report.docx", "{\"to\":\"md\"}");
//! if (docling_result_error(r)) {
//!     fprintf(stderr, "%s\n", docling_result_error(r));
//! } else {
//!     fwrite(docling_result_output(r), 1, docling_result_output_len(r), stdout);
//! }
//! docling_result_free(r);
//! ```
//!
//! Options are one JSON object: the output selection (`to`, `images`) plus
//! every key of [`docling::ConvertOptions`] — the conversion options shared
//! with the CLI, docling-serve, the wasm module and the Python/Node bindings
//! (#577; `strict`, `no_ocr`, `pages`, `ocr_lang`, `list_attachments`,
//! `document_timeout`, `pipeline`/`vlm_*`, …), validated by the same
//! [`ConvertOptions::validate`](docling::ConvertOptions::validate) rules.
//! Unknown keys fail the conversion with a clear message rather than
//! silently doing nothing — an embedder's typo should not go unnoticed.
//! `NULL` or `""` means defaults.
//!
//! The output is NUL-terminated for the textual formats (`to`: `md` |
//! `json`), so `docling_result_output` reads as a plain C string there;
//! `dclx` is a binary zip archive, which is why `docling_result_output_len`
//! exists — always pair the pointer with the length when the requested
//! format can be binary.
//!
//! Thread-safety: every call is reentrant; results are independent
//! allocations owned by the caller until `docling_result_free`. Panics never
//! unwind across the FFI boundary — they surface as an error result.

use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use docling::{ConvertOptions, DocumentConverter, ImageMode, InputFormat, SourceDocument};
use serde::Deserialize;

/// The request: output selection plus the shared conversion options.
#[derive(Default, Deserialize)]
struct Options {
    /// Output format: `md` (default) | `json` | `text` | `dclx` | `latex` | `html` |
    /// `pandoc` (Pandoc's JSON AST, #515) | `vtt` (WebVTT, #614).
    to: Option<String>,
    /// Picture rendering in Markdown: `placeholder` (default) | `embedded`
    /// (base64 data URIs — the only way to carry pixels through one buffer).
    images: Option<String>,
    /// Everything that configures the conversion itself (#577).
    #[serde(flatten)]
    convert: ConvertOptions,
}

/// Parse the options JSON. Unknown keys are rejected here (serde's
/// `deny_unknown_fields` cannot see through the `flatten`), so a misspelled
/// key fails loudly.
fn parse_options(options_json: &str) -> Result<Options, String> {
    if options_json.trim().is_empty() {
        return Ok(Options::default());
    }
    let value: serde_json::Value =
        serde_json::from_str(options_json).map_err(|e| format!("options: {e}"))?;
    let Some(map) = value.as_object() else {
        return Err("options: expected a JSON object".into());
    };
    let unknown = ConvertOptions::unknown_keys(map.keys().map(String::as_str), &["to", "images"]);
    if let Some(key) = unknown.first() {
        return Err(format!("options: unknown field `{key}`"));
    }
    serde_json::from_value(value).map_err(|e| format!("options: {e}"))
}

/// An opaque conversion result. Exactly one of output/error is set; the
/// output buffer always carries a trailing NUL byte (not counted by `len`)
/// so textual formats read as C strings.
pub struct DoclingResult {
    output: Option<Vec<u8>>,
    error: Option<CString>,
}

fn err_result(message: String) -> *mut DoclingResult {
    let error = CString::new(message.replace('\0', " "))
        .unwrap_or_else(|_| CString::new("conversion failed").expect("static"));
    Box::into_raw(Box::new(DoclingResult {
        output: None,
        error: Some(error),
    }))
}

/// The C progress callback of [`docling_convert_with_progress`] (#783):
/// called with each progress event as a JSON object — docling's event shape,
/// `{"kind": "page_completed", "document_index": 1, "document_name": …,
/// "page_no": 3, "success": true, "completed_pages": 3, "total_pages": 9}`;
/// `document_started` and `document_completed` (`"status"`), and
/// `enrichment_progress` (`"step"`, `"label"`, `"completed_items"`,
/// `"total_items"`) — as a NUL-terminated string valid for the duration of
/// the call, and the `user_data` pointer the caller passed through.
///
/// The callback runs on the threads that convert — a parallel PDF walk
/// reports its pages from its workers, several at a time — so it must be
/// thread-safe, and it must not call back into docling on the same handle.
pub type DoclingProgressCallback =
    Option<unsafe extern "C" fn(event_json: *const c_char, user_data: *mut c_void)>;

/// The C callback and its `user_data`, carried into the converter's
/// `Send + Sync` callback: the embedder promised (the function's contract)
/// that both may be used from any thread.
struct ProgressBridge {
    callback: unsafe extern "C" fn(*const c_char, *mut c_void),
    user_data: *mut c_void,
}

// SAFETY: see `ProgressBridge` — the embedder's callback contract.
unsafe impl Send for ProgressBridge {}
unsafe impl Sync for ProgressBridge {}

impl ProgressBridge {
    fn call(&self, event: &docling::ProgressEvent) {
        let Ok(json) = CString::new(event.to_json().to_string()) else {
            return;
        };
        // SAFETY: the pointer outlives the call, per the callback's contract.
        unsafe { (self.callback)(json.as_ptr(), self.user_data) }
    }
}

fn convert_impl(
    bytes: &[u8],
    filename: &str,
    options_json: &str,
    progress: Option<ProgressBridge>,
) -> Result<Vec<u8>, String> {
    let options = parse_options(options_json)?;

    let ext = filename.rsplit('.').next().unwrap_or_default();
    let format = InputFormat::from_extension(ext)
        .ok_or_else(|| format!("unknown or unsupported extension: {filename:?}"))?;
    let source = SourceDocument::from_bytes(filename.to_string(), format, bytes.to_vec());

    // The shared validation + builder mapping (#577): the same rejections
    // and the same defaults as every other surface.
    let mut converter = options
        .convert
        .apply(DocumentConverter::new())
        .map_err(|e| e.to_string())?;
    if let Some(bridge) = progress {
        converter = converter.progress_callback(std::sync::Arc::new(move |e| bridge.call(e)));
    }

    let result = converter.convert(source).map_err(|e| e.to_string());
    // The heap the conversion freed goes back to the OS rather than staying
    // in glibc's arenas between calls (#871; `docling::memory`, as serve does).
    docling::memory::release_free_memory();
    let result = result?;
    let document = result.document;

    // #537: the Pandoc AST embeds its pictures unless `images` says otherwise.
    let default_images = if options.to.as_deref() == Some("pandoc") {
        "embedded"
    } else {
        "placeholder"
    };
    let image_mode = match options.images.as_deref().unwrap_or(default_images) {
        "placeholder" => ImageMode::Placeholder,
        "embedded" => ImageMode::Embedded,
        other => {
            return Err(format!(
                "unknown images={other:?} (expected: placeholder, embedded)"
            ))
        }
    };
    match options.to.as_deref().unwrap_or("md") {
        "md" | "markdown" => Ok(match image_mode {
            ImageMode::Placeholder => document.export_to_markdown(),
            _ => {
                document
                    .export_to_markdown_with_images(image_mode, "artifacts")
                    .0
            }
        }
        .into_bytes()),
        "json" => Ok(document.export_to_json().into_bytes()),
        // A binary OPC zip — read it through output + output_len, never as a
        // C string.
        "dclx" => Ok(docling::dclx::to_dclx_bytes(&document)),
        "latex" => Ok(document.export_to_latex().into_bytes()),
        // #613: docling's plain text (`export_to_text`).
        "text" => Ok(document.export_to_text().into_bytes()),
        "html" => Ok(document.export_to_html().into_bytes()),
        // #515: pictures follow `images` like Markdown.
        "pandoc" => document
            .export_to_pandoc_json_with(&docling::pandoc::PandocExportOptions {
                image_mode,
                ..Default::default()
            })
            .map(|(json, _)| json.into_bytes())
            .map_err(|e| e.to_string()),
        // #614: docling's `--to vtt`.
        "vtt" => Ok(document.export_to_vtt().into_bytes()),
        other => Err(format!(
            "unknown to={other:?} (expected: md, json, text, dclx, latex, html, pandoc, vtt)"
        )),
    }
}

/// Read a required C string argument; `None` (→ error result) when the
/// pointer is NULL or the bytes are not UTF-8.
unsafe fn read_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

/// Convert a document held in memory.
///
/// `bytes`/`len` is the raw file content, `filename` selects the input
/// format by its extension, and `options_json` is a JSON object of
/// conversion options (`NULL` or `""` for defaults) — see the crate docs for
/// the keys. The returned result is never NULL and must be released with
/// [`docling_result_free`]; inspect [`docling_result_error`] before reading
/// the output.
///
/// # Safety
///
/// `bytes` must point to `len` readable bytes (or be NULL with `len` 0);
/// `filename` must be a NUL-terminated string; `options_json` may be NULL.
#[no_mangle]
pub unsafe extern "C" fn docling_convert(
    bytes: *const u8,
    len: usize,
    filename: *const c_char,
    options_json: *const c_char,
) -> *mut DoclingResult {
    docling_convert_with_progress(
        bytes,
        len,
        filename,
        options_json,
        None,
        std::ptr::null_mut(),
    )
}

/// [`docling_convert`] that reports progress (#783): `callback`, when not
/// NULL, receives every progress event of the conversion as JSON (see
/// [`DoclingProgressCallback`]) with `user_data` passed through — the
/// document's start and end, one event per PDF / image page with the
/// completed count (a page the `document_timeout` cut off reported with
/// `"success": false`), the picture passes' item counts.
///
/// # Safety
///
/// As [`docling_convert`]; `callback` must be safe to call from any thread
/// for as long as this call runs, with `user_data` valid for that long.
#[no_mangle]
pub unsafe extern "C" fn docling_convert_with_progress(
    bytes: *const u8,
    len: usize,
    filename: *const c_char,
    options_json: *const c_char,
    callback: DoclingProgressCallback,
    user_data: *mut c_void,
) -> *mut DoclingResult {
    let progress = callback.map(|callback| ProgressBridge {
        callback,
        user_data,
    });
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let data: &[u8] = if bytes.is_null() {
            if len != 0 {
                return err_result("bytes is NULL but len is non-zero".into());
            }
            &[]
        } else {
            std::slice::from_raw_parts(bytes, len)
        };
        let Some(filename) = read_str(filename) else {
            return err_result("filename must be a valid UTF-8 C string".into());
        };
        let options = if options_json.is_null() {
            ""
        } else {
            match read_str(options_json) {
                Some(s) => s,
                None => return err_result("options_json must be valid UTF-8".into()),
            }
        };
        match convert_impl(data, filename, options, progress) {
            Ok(mut output) => {
                // Trailing NUL so textual outputs read as C strings; not
                // counted by docling_result_output_len.
                output.push(0);
                Box::into_raw(Box::new(DoclingResult {
                    output: Some(output),
                    error: None,
                }))
            }
            Err(message) => err_result(message),
        }
    }));
    outcome.unwrap_or_else(|_| err_result("internal panic during conversion".into()))
}

/// The converted output, or NULL when the conversion failed. NUL-terminated
/// (readable as a C string for `to` = `md` / `json` / `text` / `latex` / `html` /
/// `pandoc` / `vtt`); for binary output
/// (`dclx`) pair it with [`docling_result_output_len`]. Owned by the result —
/// valid until [`docling_result_free`].
///
/// # Safety
///
/// `result` must be a pointer returned by [`docling_convert`] (or NULL) that
/// has not been freed.
#[no_mangle]
pub unsafe extern "C" fn docling_result_output(result: *const DoclingResult) -> *const c_char {
    result
        .as_ref()
        .and_then(|r| r.output.as_ref())
        .map_or(std::ptr::null(), |o| o.as_ptr() as *const c_char)
}

/// The output's length in bytes (the trailing NUL is not counted); 0 when
/// the conversion failed.
///
/// # Safety
///
/// `result` must be a pointer returned by [`docling_convert`] (or NULL) that
/// has not been freed.
#[no_mangle]
pub unsafe extern "C" fn docling_result_output_len(result: *const DoclingResult) -> usize {
    result
        .as_ref()
        .and_then(|r| r.output.as_ref())
        .map_or(0, |o| o.len() - 1)
}

/// The error message, or NULL when the conversion succeeded. Owned by the
/// result — valid until [`docling_result_free`].
///
/// # Safety
///
/// `result` must be a pointer returned by [`docling_convert`] (or NULL) that
/// has not been freed.
#[no_mangle]
pub unsafe extern "C" fn docling_result_error(result: *const DoclingResult) -> *const c_char {
    result
        .as_ref()
        .and_then(|r| r.error.as_ref())
        .map_or(std::ptr::null(), |e| e.as_ptr())
}

/// Release a result. NULL is a no-op; freeing the same pointer twice is
/// undefined behavior (as with `free`).
///
/// # Safety
///
/// `result` must be NULL or a pointer returned by [`docling_convert`] that
/// has not already been freed.
#[no_mangle]
pub unsafe extern "C" fn docling_result_free(result: *mut DoclingResult) {
    if !result.is_null() {
        drop(Box::from_raw(result));
    }
}

/// The docling.rs version as a static NUL-terminated string — never freed.
#[no_mangle]
pub extern "C" fn docling_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(bytes: &[u8], filename: &str, options: &str) -> *mut DoclingResult {
        let filename = CString::new(filename).unwrap();
        let options = CString::new(options).unwrap();
        unsafe {
            docling_convert(
                bytes.as_ptr(),
                bytes.len(),
                filename.as_ptr(),
                options.as_ptr(),
            )
        }
    }

    fn output_string(r: *const DoclingResult) -> String {
        unsafe {
            assert!(docling_result_error(r).is_null(), "unexpected error");
            CStr::from_ptr(docling_result_output(r))
                .to_string_lossy()
                .into_owned()
        }
    }

    fn error_string(r: *const DoclingResult) -> String {
        unsafe {
            assert!(docling_result_output(r).is_null(), "unexpected output");
            assert_eq!(docling_result_output_len(r), 0);
            CStr::from_ptr(docling_result_error(r))
                .to_string_lossy()
                .into_owned()
        }
    }

    #[test]
    fn markdown_json_and_dclx_roundtrip() {
        let md = b"# Title\n\nHello *world*\n";
        let r = convert(md, "note.md", "");
        let out = output_string(r);
        assert!(out.contains("# Title"), "{out}");
        assert_eq!(
            unsafe { docling_result_output_len(r) },
            out.len(),
            "len matches the C-string view for text output"
        );
        unsafe { docling_result_free(r) };

        let r = convert(md, "note.md", r#"{"to":"json"}"#);
        assert!(output_string(r).contains("\"schema_name\""));
        unsafe { docling_result_free(r) };

        // DCLX is a binary zip: starts with the PK local-file magic and the
        // length accessor is the only safe way to read it.
        let r = convert(md, "note.md", r#"{"to":"latex"}"#);
        let tex = output_string(r);
        assert!(tex.starts_with("\\documentclass"), "{tex}");
        assert!(tex.ends_with("\\end{document}"), "{tex}");
        unsafe { docling_result_free(r) };
        let r = convert(md, "note.md", r#"{"to":"text"}"#);
        assert_eq!(output_string(r), "Title\n\nHello world");
        unsafe { docling_result_free(r) };
        // #614: no timed text → the bare header, titled.
        let r = convert(md, "note.md", r#"{"to":"vtt"}"#);
        assert_eq!(output_string(r), "WEBVTT Title");
        unsafe { docling_result_free(r) };
        let r = convert(md, "note.md", r#"{"to":"pandoc"}"#);
        let ast = output_string(r);
        assert!(
            ast.starts_with("{\"pandoc-api-version\":[1,23,1,1]"),
            "{ast}"
        );
        unsafe { docling_result_free(r) };
        let r = convert(md, "note.md", r#"{"to":"dclx"}"#);
        unsafe {
            assert!(docling_result_error(r).is_null());
            let len = docling_result_output_len(r);
            let bytes = std::slice::from_raw_parts(docling_result_output(r) as *const u8, len);
            assert!(bytes.starts_with(b"PK"), "dclx is a zip archive");
            docling_result_free(r);
        }
    }

    #[test]
    fn errors_are_results_not_crashes() {
        // Unknown extension.
        let r = convert(b"x", "file.unknown-ext", "");
        assert!(error_string(r).contains("extension"), "unknown ext");
        unsafe { docling_result_free(r) };

        // Malformed options JSON.
        let r = convert(b"x", "note.md", "{not json");
        assert!(error_string(r).starts_with("options:"));
        unsafe { docling_result_free(r) };

        // A misspelled option key fails loudly instead of doing nothing.
        let r = convert(b"# hi", "note.md", r#"{"stricd":true}"#);
        assert!(error_string(r).contains("stricd"), "typo surfaces");
        unsafe { docling_result_free(r) };

        // Unknown output / image modes.
        let r = convert(b"# hi", "note.md", r#"{"to":"pdf"}"#);
        assert!(error_string(r).contains("unknown to"));
        unsafe { docling_result_free(r) };
        let r = convert(b"# hi", "note.md", r#"{"images":"inline"}"#);
        assert!(error_string(r).contains("unknown images"));
        unsafe { docling_result_free(r) };
    }

    #[test]
    fn null_arguments_are_handled() {
        // NULL filename → error result, not a crash.
        let r = unsafe { docling_convert(b"x".as_ptr(), 1, std::ptr::null(), std::ptr::null()) };
        assert!(error_string(r).contains("filename"));
        unsafe { docling_result_free(r) };

        // NULL bytes with len 0 is an empty (convertible) text file.
        let name = CString::new("empty.md").unwrap();
        let r = unsafe { docling_convert(std::ptr::null(), 0, name.as_ptr(), std::ptr::null()) };
        unsafe {
            assert!(docling_result_error(r).is_null(), "empty md converts");
            docling_result_free(r);
        }

        // NULL bytes with a non-zero len must not be dereferenced.
        let r = unsafe { docling_convert(std::ptr::null(), 4, name.as_ptr(), std::ptr::null()) };
        assert!(error_string(r).contains("NULL"));
        unsafe { docling_result_free(r) };

        // Accessors and free tolerate NULL results.
        unsafe {
            assert!(docling_result_output(std::ptr::null()).is_null());
            assert_eq!(docling_result_output_len(std::ptr::null()), 0);
            assert!(docling_result_error(std::ptr::null()).is_null());
            docling_result_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn version_is_a_static_c_string() {
        let v = unsafe { CStr::from_ptr(docling_version()) };
        assert_eq!(v.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
    }
}
