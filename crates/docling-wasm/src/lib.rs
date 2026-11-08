//! Browser/edge (wasm32) bindings for docling.rs's **declarative** converters
//! (issue #79): DOCX, HTML, Markdown, XLSX, PPTX, CSV, AsciiDoc, EPUB, ODF,
//! RTF, WebVTT, Email, MHTML, JATS, USPTO, XBRL, LaTeX, JSON, DocLang →
//! Markdown or docling JSON — fully client-side, no server round-trip.
//!
//! Built on `docling` with `default-features = false` plus `pdf-text`: a PDF's
//! **embedded text layer** converts too (pure-Rust parser, same extraction as
//! the native `--text-layer-only` flag — flat paragraphs, no headings/tables/pictures).
//! The ML pipelines (ONNX Runtime) and the HTTP image fetcher are
//! compiled out — scanned PDFs, images, and audio are rejected at convert time
//! with a clear message.
//!
//! ```js
//! import init, { convert } from "./pkg/docling_wasm.js";
//! await init();
//! const md = convert(new Uint8Array(await file.arrayBuffer()), file.name, "md");
//! ```

use docling::{ConvertOptions, DocumentConverter, ImageMode, InputFormat, SourceDocument};
use serde::Deserialize;
use wasm_bindgen::prelude::*;

#[cfg(feature = "ocr")]
mod digital;
#[cfg(feature = "ocr")]
mod ocr;
#[cfg(feature = "ocr")]
mod scanned;
#[cfg(feature = "ocr")]
mod tableformer;
#[cfg(feature = "ocr")]
pub use digital::DigitalConverter;
#[cfg(feature = "ocr")]
pub use ocr::ocr_image;
#[cfg(feature = "ocr")]
pub use scanned::{convert_scanned_image, ScannedConverter};

#[wasm_bindgen(start)]
fn start() {
    // Panics surface as readable messages in the browser console instead of
    // an opaque `unreachable executed`.
    console_error_panic_hook::set_once();
}

/// One request: the output selection plus the conversion options every
/// surface shares ([`docling::ConvertOptions`], #577) — the JSON object
/// [`convert_with_options`] takes.
#[derive(Debug, Default, Deserialize)]
struct Request {
    /// `md` (default) | `json` | `text` | `doclang` | `latex` | `html` | `pandoc` |
    /// `vtt`.
    to: Option<String>,
    /// `placeholder` (default) | `embedded` — see [`image_mode`].
    images: Option<String>,
    #[serde(flatten)]
    convert: ConvertOptions,
}

/// Parse the options JSON; unknown keys are rejected (serde's
/// `deny_unknown_fields` cannot see through the `flatten`), so a misspelled
/// key fails loudly instead of silently doing nothing.
fn parse_request(options_json: Option<&str>) -> Result<Request, String> {
    let Some(json) = options_json.filter(|j| !j.trim().is_empty()) else {
        return Ok(Request::default());
    };
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("options: {e}"))?;
    let Some(map) = value.as_object() else {
        return Err("options: expected a JSON object".into());
    };
    let unknown = ConvertOptions::unknown_keys(map.keys().map(String::as_str), &["to", "images"]);
    if let Some(key) = unknown.first() {
        return Err(format!("options: unknown field `{key}`"));
    }
    serde_json::from_value(value).map_err(|e| format!("options: {e}"))
}

/// The positional [`convert`] arguments as a [`Request`]: `max_pages` is the
/// window `1-N` (issue #80's window with first pinned to 1).
fn positional_request(
    to: Option<&str>,
    images: Option<&str>,
    max_pages: Option<u32>,
    page_break_placeholder: Option<&str>,
) -> Request {
    Request {
        to: to.map(str::to_owned),
        images: images.map(str::to_owned),
        convert: ConvertOptions {
            pages: max_pages.filter(|&n| n > 0).map(|n| format!("1-{n}")),
            page_break_placeholder: page_break_placeholder.map(str::to_owned),
            ..ConvertOptions::default()
        },
    }
}

/// The whole conversion body, host-testable (`JsError` can only be
/// constructed on the wasm target, so the JS boundary stays a thin shim).
fn convert_impl(
    bytes: &[u8],
    filename: &str,
    to: Option<&str>,
    images: Option<&str>,
    max_pages: Option<u32>,
    page_break_placeholder: Option<&str>,
) -> Result<String, String> {
    convert_request(
        bytes,
        filename,
        positional_request(to, images, max_pages, page_break_placeholder),
    )
}

/// Convert per a parsed [`Request`]: the shared options validated and applied
/// by [`ConvertOptions::apply`] (the ML-only ones are inert here — the formats
/// they affect are rejected at convert time), then the export `to` names.
fn convert_request(bytes: &[u8], filename: &str, request: Request) -> Result<String, String> {
    let ext = filename.rsplit('.').next().unwrap_or_default();
    let format = InputFormat::from_extension(ext)
        .ok_or_else(|| format!("unknown or unsupported extension: {filename:?}"))?;
    let source = SourceDocument::from_bytes(filename.to_string(), format, bytes.to_vec());
    let converter = request
        .convert
        .apply(DocumentConverter::new())
        .map_err(|e| e.to_string())?;
    let result = converter.convert(source).map_err(|e| e.to_string())?;
    let to = request.to.as_deref().unwrap_or("md");
    let image_mode = image_mode(request.images.as_deref(), to)?;
    match to {
        // `Referenced` is deliberately unreachable here: it hands the caller
        // loose image files to write next to the Markdown, which a page with no
        // filesystem cannot do — the browser equivalent is `embedded`.
        "md" | "markdown" => Ok(result
            .document
            .export_to_markdown_with_images(image_mode, "artifacts")
            .0),
        "json" => Ok(result.document.export_to_json()),
        "doclang" => Ok(result.document.export_to_doclang()),
        "latex" => Ok(result.document.export_to_latex()),
        // #613: docling's plain text (`export_to_text`).
        "text" => Ok(result.document.export_to_text()),
        // #492: like Markdown, pictures embed (`referenced` is unreachable
        // here for the same reason).
        "html" => Ok(result
            .document
            .export_to_html_with_images(image_mode, "artifacts")
            .0),
        // #515: Pandoc's JSON AST (`pandoc -f json`); pictures as above.
        "pandoc" => pandoc_json(&result.document, image_mode),
        // #614: docling's `--to vtt` — a `.vtt` input's cues round-trip.
        "vtt" => Ok(result.document.export_to_vtt()),
        other => Err(format!(
            "unknown output format {other:?} (expected \"md\", \"json\", \"text\", \"doclang\", \"latex\", \"html\", \"pandoc\" or \"vtt\")"
        )),
    }
}

/// Picture rendering for Markdown output, mirroring docling-serve's `images`
/// option: `placeholder` (docling's default `<!-- image -->`) or `embedded`
/// (`![Image](data:…;base64,…)`, self-contained — the only way to carry pixels
/// out of a page that cannot write files).
///
/// Unset is `placeholder`, except `embedded` for the Pandoc AST (#537): it
/// feeds `pandoc -t docx`, whose writers drop a picture without a target.
fn image_mode(images: Option<&str>, to: &str) -> Result<ImageMode, String> {
    let default = if to == "pandoc" {
        "embedded"
    } else {
        "placeholder"
    };
    match images.unwrap_or(default) {
        "placeholder" => Ok(ImageMode::Placeholder),
        "embedded" => Ok(ImageMode::Embedded),
        other => Err(format!(
            "unknown images={other:?} (expected \"placeholder\" or \"embedded\")"
        )),
    }
}

/// Pandoc's JSON AST (#515) with `image_mode`'s pictures — the one output
/// every conversion path here (declarative, OCR, layout) shares.
pub(crate) fn pandoc_json(
    doc: &docling_core::DoclingDocument,
    image_mode: ImageMode,
) -> Result<String, String> {
    let opts = docling_core::pandoc::PandocExportOptions {
        image_mode,
        ..Default::default()
    };
    doc.export_to_pandoc_json_with(&opts)
        .map(|(json, _)| json)
        .map_err(|e| e.to_string())
}

/// Convert a document (as bytes + filename, the extension drives format
/// detection) to `to`: `"md"` (Markdown, default), `"json"` (docling-core's
/// `DoclingDocument` wire format, schema 1.11.0), `"text"` (plain text,
/// docling's `export_to_text`, #613), `"doclang"` (docling's
/// DocLang XML serialization), `"latex"` (docling 2.124's LaTeX document,
/// #317), `"html"`, `"pandoc"` (Pandoc's JSON AST for `pandoc -f json`,
/// #515) or `"vtt"` (WebVTT, docling's `--to vtt`, #614).
///
/// `images` controls how pictures render in Markdown — `"placeholder"`
/// (default) or `"embedded"` (base64 data URIs), the same option
/// docling-serve exposes. `max_pages` converts only a PDF's first N pages
/// (issue #80's window, first pinned to 1); other formats ignore it.
/// `page_break_placeholder` is docling's `export_to_markdown` option of the
/// same name: text inserted between pages in the Markdown (e.g.
/// `"<!-- page break -->"`); unset, the Markdown carries no page breaks.
#[wasm_bindgen]
pub fn convert(
    bytes: &[u8],
    filename: &str,
    to: Option<String>,
    images: Option<String>,
    max_pages: Option<u32>,
    page_break_placeholder: Option<String>,
) -> Result<String, JsError> {
    convert_impl(
        bytes,
        filename,
        to.as_deref(),
        images.as_deref(),
        max_pages,
        page_break_placeholder.as_deref(),
    )
    .map_err(|e| JsError::new(&e))
}

/// Convert with the full option set as one JSON object (#577): `to` and
/// `images` as in [`convert`], plus every conversion option the CLI,
/// docling-serve, the C ABI and the Python/Node bindings share — the keys of
/// `docling::ConvertOptions` (`strict`, `compact_tables`,
/// `page_break_placeholder`, `pages`, `encoding`, `skip_empty_cells`,
/// `list_attachments`, `xbrl_taxonomy`, …), validated by the same rules.
/// Options of the ML pipeline are accepted and inert in this build (the
/// formats they apply to are rejected at convert time). Unknown keys are an
/// error. `undefined` / `""` means defaults.
///
/// ```js
/// convert_with_options(bytes, file.name, JSON.stringify({ to: "md", strict: true, pages: "1-3" }));
/// ```
#[wasm_bindgen]
pub fn convert_with_options(
    bytes: &[u8],
    filename: &str,
    options_json: Option<String>,
) -> Result<String, JsError> {
    let request = parse_request(options_json.as_deref()).map_err(|e| JsError::new(&e))?;
    convert_request(bytes, filename, request).map_err(|e| JsError::new(&e))
}

/// The file extensions this build can convert, as a JSON string array —
/// handy for an `<input accept=…>` filter: the engine's own
/// [`InputFormat::supported_extensions`] (what `docling-rs
/// --list-input-formats` prints), which leaves out what this build lacks —
/// the ML formats (images, audio, video, METS), as the `asr` and full `pdf`
/// features are not compiled in; PDF stays, converting via its text layer
/// (`pdf-text`). A format added to the engine is listed here with no edit.
#[wasm_bindgen]
pub fn supported_extensions() -> String {
    let exts = InputFormat::supported_extensions();
    serde_json::to_string(exts.as_slice()).expect("static array serializes")
}

/// The docling.rs version this module was built from.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    // Host-side (`cargo test -p docling-wasm`) sanity of the conversion body —
    // the wasm-bindgen layer is exercised by the browser demo.
    use super::*;

    #[test]
    fn markdown_roundtrip() {
        let md = b"# Title\n\nHello *world*\n";
        let out = convert_impl(md, "note.md", None, None, None, None).unwrap();
        assert!(out.contains("# Title"));
        let json = convert_impl(md, "note.md", Some("json"), None, None, None).unwrap();
        assert!(json.contains("\"schema_name\""));
        let pandoc = convert_impl(md, "note.md", Some("pandoc"), None, None, None).unwrap();
        assert!(pandoc.starts_with("{\"pandoc-api-version\":[1,23,1,1]"));
        assert!(pandoc.contains("\"t\":\"Header\""));
    }

    #[test]
    fn ml_formats_rejected() {
        // Images still need the full ML pipeline.
        let err = convert_impl(
            &[0x89, b'P', b'N', b'G'],
            "scan.png",
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("unknown or unsupported") || err.contains("pdf"),
            "should reject the ML-only format: {err}"
        );
    }

    #[test]
    fn pdf_text_layer_converts() {
        // A text-layer PDF converts via the pure-Rust `pdf-text` path (the
        // exact `--text-layer-only` extraction: flat paragraphs in reading order).
        // Under `cargo test --workspace`, feature unification swaps in the
        // full ML pipeline (which needs the models) — this test is about
        // the text-layer arm, so it only runs in the real wasm feature set.
        if docling::PDF_ML_COMPILED {
            return;
        }
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/pdf/sources/code_and_formula.pdf"
        ))
        .expect("corpus pdf");
        let out = convert_impl(&bytes, "code_and_formula.pdf", None, None, None, None).unwrap();
        assert!(!out.trim().is_empty(), "text layer should extract");
    }

    #[test]
    fn scanned_pdf_reports_missing_text_layer() {
        // A PDF with no embedded text (an image-only scan) should explain
        // that OCR needs a native build, not return "".
        // Text-layer-arm-only, same as `pdf_text_layer_converts`.
        if docling::PDF_ML_COMPILED {
            return;
        }
        let scan = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/scanned/sources/ocr_test_raster.pdf"
        ))
        .unwrap();
        let err = convert_impl(&scan, "scan.pdf", None, None, None, None).unwrap_err();
        assert!(
            err.contains("text layer") || err.contains("OCR"),
            "should point at the missing text layer: {err}"
        );
        // Bytes no reader opens are an error about the file instead.
        let err = convert_impl(b"%PDF-1.4\n%%EOF", "x.pdf", None, None, None, None).unwrap_err();
        assert!(err.contains("not a readable PDF"), "{err}");
    }

    #[test]
    fn docx_converts() {
        // A real corpus DOCX through the wasm entry path on the host.
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/docx/sources/docx_lists.docx"
        ))
        .expect("corpus docx");
        let out = convert_impl(&bytes, "docx_lists.docx", None, None, None, None).unwrap();
        assert!(!out.trim().is_empty());
    }

    /// `images=embedded` inlines picture bytes as data URIs (docling-serve's
    /// option, and the only way a page with no filesystem can carry pixels
    /// out); the default stays docling's `<!-- image -->` placeholder.
    #[test]
    fn embedded_images_inline_as_data_uris() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/docx/sources/word_image_anchors.docx"
        ))
        .expect("corpus docx with images");
        let placeholder =
            convert_impl(&bytes, "word_image_anchors.docx", None, None, None, None).unwrap();
        assert!(placeholder.contains("<!-- image -->"), "{placeholder}");
        let embedded = convert_impl(
            &bytes,
            "word_image_anchors.docx",
            None,
            Some("embedded"),
            None,
            None,
        )
        .unwrap();
        assert!(embedded.contains("](data:image/"), "expected a data URI");
        let err = convert_impl(
            &bytes,
            "word_image_anchors.docx",
            None,
            Some("nope"),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("unknown images="), "{err}");
    }

    /// `convert_with_options` reads the shared option set: a `pages` window
    /// and `strict` land, a typo is rejected, and the positional `convert`
    /// arguments are the same request.
    #[test]
    fn options_json_is_the_shared_option_set() {
        let md = b"# Title\n\nHello *world*\n";
        let req = parse_request(Some(r#"{"to": "json", "strict": true, "pages": "1-2"}"#)).unwrap();
        assert_eq!(req.to.as_deref(), Some("json"));
        assert_eq!(req.convert.strict, Some(true));
        assert_eq!(req.convert.page_range().unwrap(), Some((1, 2)));
        let out = convert_request(md, "note.md", req).unwrap();
        assert!(out.contains("\"schema_name\""));
        let err = parse_request(Some(r#"{"strictness": true}"#)).unwrap_err();
        assert!(err.contains("unknown field `strictness`"), "{err}");
        let err = convert_request(
            md,
            "note.md",
            parse_request(Some(r#"{"pages": "0-2"}"#)).unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("pages"), "{err}");
        assert_eq!(
            positional_request(None, None, Some(3), Some("<!-- p -->"))
                .convert
                .pages
                .as_deref(),
            Some("1-3")
        );
    }

    #[test]
    fn extensions_json_parses() {
        let v: Vec<String> = serde_json::from_str(&supported_extensions()).unwrap();
        for ext in ["docx", "ipynb", "jsonl", "py", "log", "pages", "djvu"] {
            assert!(v.contains(&ext.to_string()), "{ext}");
        }
        assert!(v.contains(&"pdf".to_string()), "pdf-text is compiled in");
        // The ML formats follow the features this test binary was built
        // with: absent in the wasm build (`pdf-text` only), present when a
        // workspace-wide `cargo test` unifies docling's default features
        // into this crate — the list says what `is_compiled_in` says.
        let has = |ext: &str| v.contains(&ext.to_string());
        assert_eq!(has("png"), InputFormat::Image.is_compiled_in());
        assert_eq!(has("mp3"), InputFormat::Audio.is_compiled_in());
        assert_eq!(has("mp4"), InputFormat::Video.is_compiled_in());
        assert!(
            !has("heic"),
            "heic needs the heif feature, which no build here has"
        );
    }
}
