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
//! Options are one JSON object whose keys mirror docling-serve's request
//! options (`to`, `strict`, `images`, `no_ocr`, `force_full_page_ocr`,
//! `no_table_former`, `no_text_panels`, `fetch_images`, `asr_model`,
//! `asr_lang`, `encoding`, `video_frames`, `pages`, `ocr_lang`); unknown keys fail the
//! conversion with a clear message rather than silently doing nothing — an
//! embedder's typo should not go unnoticed. `NULL` or `""` means defaults.
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

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use docling::{DocumentConverter, ImageMode, InputFormat, SourceDocument};
use serde::Deserialize;

/// Conversion options, one JSON object — the same keys as docling-serve's
/// request options. Unknown keys are rejected so misspellings fail loudly.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    /// Output format: `md` (default) | `json` | `dclx` | `latex`.
    to: Option<String>,
    /// Strict (docling-faithful) Markdown instead of the readable default.
    strict: Option<bool>,
    /// Picture rendering in Markdown: `placeholder` (default) | `embedded`
    /// (base64 data URIs — the only way to carry pixels through one buffer).
    images: Option<String>,
    no_ocr: Option<bool>,
    force_full_page_ocr: Option<bool>,
    no_table_former: Option<bool>,
    no_text_panels: Option<bool>,
    /// Resolve external `<img src>` for HTML/EPUB. Unlike docling-serve there
    /// is no server-side gate — the embedder owns its network policy.
    fetch_images: Option<bool>,
    asr_model: Option<String>,
    asr_lang: Option<String>,
    /// Character encoding of text inputs (docling's
    /// `TextBackendOptions.encoding`, a WHATWG label); unset = detect.
    encoding: Option<String>,
    video_frames: Option<usize>,
    /// PDF page window, `"A-B"` or a single `"N"` (1-based inclusive, #80).
    pages: Option<String>,
    /// OCR recognition language for scanned pages: `en` (default) | `ch`.
    ocr_lang: Option<String>,
    /// Keep layout + TableFormer, never OCR (docling's independent
    /// `do_ocr=False`, #244).
    skip_ocr: Option<bool>,
    /// Infer section-header levels (docling's HeadingHierarchyModel, #302).
    heading_hierarchy: Option<bool>,
    /// Which regions feed the OCR (docling's `OcrMode`, #254): `default` |
    /// `full_page` | `layout_regions` | `pdf_aware_layout_regions`.
    ocr_mode: Option<String>,
    /// OCR render scale in px per PDF point (docling's `OcrOptions.scale`,
    /// #254); unset reads the pipeline's own 2.0 px/pt render.
    ocr_scale: Option<f32>,
    /// Unpadded `| a | b |` Markdown tables (#271, docling.rs extension).
    compact_tables: Option<bool>,
    /// Omit empty cells from sparse XLSX/XLS grids (#271, docling.rs extension).
    skip_empty_cells: Option<bool>,
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

fn convert_impl(bytes: &[u8], filename: &str, options_json: &str) -> Result<Vec<u8>, String> {
    let options: Options = if options_json.trim().is_empty() {
        Options::default()
    } else {
        serde_json::from_str(options_json).map_err(|e| format!("options: {e}"))?
    };

    let ext = filename.rsplit('.').next().unwrap_or_default();
    let format = InputFormat::from_extension(ext)
        .ok_or_else(|| format!("unknown or unsupported extension: {filename:?}"))?;
    let source = SourceDocument::from_bytes(filename.to_string(), format, bytes.to_vec());

    let mut converter = DocumentConverter::new()
        .strict(options.strict.unwrap_or(false))
        .fetch_images(options.fetch_images.unwrap_or(false))
        .asr_model(options.asr_model.clone())
        .asr_lang(options.asr_lang.clone())
        .encoding(options.encoding.clone())
        .video_frames(
            options
                .video_frames
                .unwrap_or(docling::DEFAULT_VIDEO_FRAMES),
        )
        .no_ocr(options.no_ocr.unwrap_or(false))
        .force_full_page_ocr(options.force_full_page_ocr.unwrap_or(false))
        .no_table_former(options.no_table_former.unwrap_or(false))
        .no_text_panels(options.no_text_panels.unwrap_or(false))
        .skip_ocr(options.skip_ocr.unwrap_or(false))
        .heading_hierarchy(options.heading_hierarchy.unwrap_or(false))
        .compact_tables(options.compact_tables.unwrap_or(false))
        .skip_empty_cells(options.skip_empty_cells.unwrap_or(false));
    if let Some(mode) = &options.ocr_mode {
        converter = converter.ocr_mode(mode.clone());
    }
    if let Some(scale) = options.ocr_scale {
        converter = converter.ocr_scale(scale);
    }
    if let Some(pages) = &options.pages {
        let (first, last) = docling::parse_page_range(pages).map_err(|e| format!("pages: {e}"))?;
        converter = converter.page_range(first, last);
    }
    if let Some(lang) = &options.ocr_lang {
        docling::OcrLang::parse(lang).ok_or_else(|| {
            format!(
                "ocr_lang {lang:?} is not a supported OCR language ({})",
                docling::OcrLang::ACCEPTED
            )
        })?;
        converter = converter.ocr_lang(lang.clone());
    }

    let result = converter.convert(source).map_err(|e| e.to_string())?;
    let document = result.document;

    let image_mode = match options.images.as_deref().unwrap_or("placeholder") {
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
        other => Err(format!(
            "unknown to={other:?} (expected: md, json, dclx, latex)"
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
        match convert_impl(data, filename, options) {
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
/// (readable as a C string for `to` = `md` / `json` / `latex`); for binary output
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
