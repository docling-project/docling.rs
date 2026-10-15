//! PDF backend for docling.rs.
//!
//! A port of docling's standard PDF pipeline: pdfium extracts the text layer
//! (cells with bounding boxes) and renders page images; a discriminative ONNX
//! stack (layout detection, table structure, OCR) classifies regions; the cells
//! are assembled in reading order into a [`DoclingDocument`].
//!
//! Current stages: pdfium text-cell extraction + page rendering ([`pdfium_backend`])
//! and the deterministic text/reading-order assembly ([`assemble`]). The layout,
//! table-structure and OCR ONNX stages land behind [`Pipeline`] next.

// Without `ml` only the text-layer path runs; the shared assembly/label
// helpers it doesn't exercise stay compiled for API stability (the full
// build still flags genuinely dead code).
#![cfg_attr(not(feature = "ml"), allow(dead_code))]

// Reading-order assembly. Public under `ocr-prep` so the browser pipeline can
// reuse the geometric table reconstruction and its reliability gate (#157).
#[cfg(feature = "ocr-prep")]
pub mod assemble;
#[cfg(not(feature = "ocr-prep"))]
mod assemble;
// Vector checkbox squares from the text parser's path walk (#609).
pub mod checkbox;
mod dp_lines;
#[cfg(feature = "ml")]
pub mod enrich;
// Public so sibling crates (e.g. docling-rag's ONNX embedder) can route their
// own `ort` sessions through the same `DOCLING_RS_EP` selection. Kept as a
// re-export after the logic moved to the shared `docling-onnx` crate —
// `docling_pdf::ep::…` remains the stable path downstream crates code against.
#[cfg(feature = "ml")]
pub use docling_onnx as ep;
// Heading-hierarchy stage (#302): PDF font-name style parsing, outline
// extraction (pure lopdf), and the level-assignment pass. No feature gate on
// the logic itself — only the glyph style pass needs pdfium (`ml`).
#[cfg(feature = "ml")]
pub mod dparse_render;
mod font_style;
mod heading_hierarchy;
pub mod layout;
#[cfg(feature = "ml")]
mod mets;
#[cfg(feature = "ner")]
pub mod ner;
#[cfg(feature = "ml")]
mod ocr;
#[cfg(feature = "ml")]
mod ocr_cls;
#[cfg(any(feature = "ml", feature = "ocr-prep"))]
pub mod ocr_det;
#[cfg(feature = "ocr-prep")]
pub mod ocr_prep;
#[cfg(feature = "ml")]
mod orient;
pub mod outline;
pub mod pdfium_backend;
#[cfg(feature = "ml")]
pub mod picture;
#[cfg(feature = "ml")]
pub mod quality;
mod reading_order;
#[cfg(feature = "ml")]
mod redact_ocr;
// Pure-Rust region resampling (page→1024px box-average, crop→448 bilinear) —
// available to the browser TableFormer path (#157 stage 3), not just `ml`.
#[cfg(feature = "ocr-prep")]
pub mod resample;
#[cfg(feature = "ocr-prep")]
pub mod scanned;
// Built-in standard-14 font metrics for the pure-Rust text parser (#187) —
// no feature gate: the wasm/pdf-text path needs them like the native one.
pub mod pdf_meta;
#[cfg(feature = "ml")]
pub mod raster;
#[cfg(feature = "ml")]
pub mod render;
mod std14;
#[cfg(feature = "ml")]
pub mod tableformer;
#[cfg(feature = "ml")]
mod tesseract;
pub mod textparse;
#[cfg(feature = "ocr-prep")]
pub mod tf_core;
// docling's TableFormer cell matcher — pure Rust, shared with the browser
// TableFormer path (#157 stage 3).
#[cfg(feature = "ocr-prep")]
pub mod tf_match;
pub mod timing;

#[cfg(feature = "ml")]
use std::collections::BTreeMap;
use std::fmt;
#[cfg(feature = "ml")]
use std::sync::mpsc::{sync_channel, Receiver};
#[cfg(feature = "ml")]
use std::sync::{Arc, Mutex};

// An execution provider only exists on its OS, and ort's prebuilt ONNX
// Runtime binaries follow suit — requesting an impossible pairing otherwise
// surfaces as a cryptic ort-sys linker error ("no builds available that
// satisfy the requested feature set"). Catch it at type-check time with an
// actionable message instead.
#[cfg(all(feature = "coreml", not(target_vendor = "apple")))]
compile_error!(
    "the `coreml` execution provider exists only on Apple targets (macOS/iOS). \
     On Linux use `--features cuda` or `--features tensorrt` (NVIDIA), on \
     Windows also `--features directml`, or build without EP features for CPU."
);
#[cfg(all(feature = "directml", not(target_os = "windows")))]
compile_error!(
    "the `directml` execution provider exists only on Windows. On Linux use \
     `--features cuda` or `--features tensorrt` (NVIDIA), on macOS \
     `--features coreml`, or build without EP features for CPU."
);
#[cfg(all(any(feature = "cuda", feature = "tensorrt"), target_vendor = "apple"))]
compile_error!(
    "the `cuda`/`tensorrt` execution providers have no Apple builds (no NVIDIA \
     support on macOS). Use `--features coreml` there, or build without EP \
     features for CPU."
);

use docling_core::DoclingDocument;
pub use docling_core::EncryptionError;
// The env-knob helpers only gate ML-pipeline diagnostics and tuning; the
// pure text-layer (wasm) build has no call sites.
#[cfg(feature = "ml")]
use docling_core::Node;
#[cfg(feature = "ml")]
use docling_core::{debug_log, env};

pub use heading_hierarchy::HeadingHierarchyOptions;
#[cfg(feature = "ml")]
pub use mets::{convert_mets_gbs, convert_mets_gbs_with_options, convert_mets_gbs_with_pipeline};
#[cfg(feature = "ml")]
pub use ocr::{OcrEngine, OcrLang, OcrMode};
#[cfg(feature = "ml")]
pub use pdfium_backend::PdfDocument;
pub use pdfium_backend::{PdfPage, TextCell};
#[cfg(feature = "ml")]
pub use tesseract::{lang_arg as tesseract_lang_arg, TesseractOptions};
// Plain page rasterization (#243) — pdfium only, no models.
#[cfg(feature = "ml")]
pub use pdfium_backend::{render_pages, RenderedPage};

/// Errors from the PDF backend. Detailed and surfaced (never silently skipped).
#[derive(Debug)]
pub enum PdfError {
    /// pdfium failed to bind, open, or read the document (and, historically,
    /// any pipeline error outside the models).
    Pdfium(String),
    /// The document itself: an object model lopdf cannot read, a page no
    /// renderer can draw, a page range outside it.
    Document(String),
    /// The layout ONNX model failed to load or run.
    Layout(String),
    /// The OCR ONNX model failed to load or run.
    Ocr(String),
    /// The document budget ([`Pipeline::document_timeout`]) ran out between
    /// two pages. Internally the sentinel that stops the page walk; a caller
    /// never sees it from [`Pipeline::convert`] — the conversion returns the
    /// pages it finished and reports the cut through [`Completion`].
    Timeout(String),
    /// The document is encrypted and the password given (or none) does not
    /// open it (#636) — the typed value a caller prompts for a password on,
    /// reachable through [`std::error::Error::source`] too. Its text is the
    /// one docling raises.
    Encrypted(EncryptionError),
}

impl fmt::Display for PdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfError::Pdfium(m) => write!(f, "pdf: pdfium error: {m}"),
            PdfError::Document(m) => write!(f, "pdf: {m}"),
            PdfError::Layout(m) => write!(f, "pdf: {m}"),
            PdfError::Ocr(m) => write!(f, "pdf: {m}"),
            PdfError::Timeout(m) => write!(f, "pdf: {m}"),
            PdfError::Encrypted(EncryptionError::WrongPassword) => {
                f.write_str("pdf: the PDF is encrypted and the password is wrong")
            }
            PdfError::Encrypted(EncryptionError::NeedPassword) => {
                f.write_str("pdf: the PDF is encrypted: a password is required")
            }
            PdfError::Encrypted(e) => write!(f, "pdf: the PDF is encrypted: {e}"),
        }
    }
}

/// How a conversion ended: every selected page, or the pages that fit the
/// document budget (docling's `document_timeout`, `PARTIAL_SUCCESS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// Every selected page was processed.
    Complete,
    /// The budget ran out: `pages_done` of `pages_selected` pages are in the
    /// document, the rest were never rendered (or never processed).
    TimedOut {
        pages_done: usize,
        pages_selected: usize,
        budget: std::time::Duration,
    },
}

impl Completion {
    pub fn timed_out(&self) -> bool {
        matches!(self, Completion::TimedOut { .. })
    }

    /// docling's `ErrorItem.error_message` for the cut, `None` when complete.
    pub fn message(&self) -> Option<String> {
        match self {
            Completion::Complete => None,
            Completion::TimedOut {
                pages_done,
                pages_selected,
                budget,
            } => Some(format!(
                "document timeout of {:.3}s exceeded after {pages_done} of {pages_selected} \
                 pages; the output holds the pages processed",
                budget.as_secs_f64()
            )),
        }
    }
}

/// A converted document and how its conversion ended ([`Completion`]).
#[derive(Debug, Clone)]
pub struct Converted {
    pub document: DoclingDocument,
    pub completion: Completion,
}

/// Has the document budget run out?
fn expired(deadline: Option<std::time::Instant>) -> bool {
    deadline.is_some_and(|d| std::time::Instant::now() >= d)
}

/// The sentinel the page walks stop on once the budget is gone.
fn timeout_sentinel() -> PdfError {
    PdfError::Timeout("document timeout exceeded".into())
}

impl std::error::Error for PdfError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PdfError::Encrypted(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(feature = "pdfium")]
impl From<pdfium_render::prelude::PdfiumError> for PdfError {
    fn from(e: pdfium_render::prelude::PdfiumError) -> Self {
        // A failed dlopen means the library this build was asked to use
        // (`pdfium` feature) is not there. Say what to do instead of leaking
        // the raw loader error.
        if matches!(e, pdfium_render::prelude::PdfiumError::LoadLibraryError(_)) {
            // The loader error pretty-prints over several lines; compact it.
            let detail = e
                .to_string()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            return PdfError::Pdfium(format!(
                "the pdfium library is not installed. This build's `pdfium` feature \
                 was asked for it (DOCLING_RS_RENDERER=pdfium, or a file the pure-Rust \
                 object model cannot read): point PDFIUM_DYNAMIC_LIB_PATH at a directory \
                 containing the pdfium library, or unset DOCLING_RS_RENDERER — the \
                 default renderer needs no library. Declarative formats (DOCX, HTML, \
                 Markdown, …) never need it. [{detail}]"
            ));
        }
        PdfError::Pdfium(e.to_string())
    }
}

/// Convert a PDF's **embedded text layer only** — no ONNX, no
/// threads: the pure-Rust content-stream parser ([`textparse`]) feeds the same
/// orphan-region assembly the `no_ocr` pipeline flag uses, so text-layer PDFs
/// come out identical to `--text-layer-only` (flat, line-grouped paragraphs in reading
/// order; no headings/lists/tables/pictures, and no hyperlink recovery).
///
/// This is the only conversion entry compiled without the `ml` feature (it is
/// what a wasm32 build runs). A scanned/image-only PDF (no embedded text
/// layer) yields an empty document rather than an error, same as `no_ocr` —
/// callers can detect that and fall back to an OCR-capable build.
pub fn convert_text_layer(bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
    convert_text_layer_pages(bytes, name, None)
}

/// The error for bytes no reader here can parse as a PDF — not a PDF at
/// all, or one damaged past the parser's repairs. Said about the file, not
/// about a missing renderer or text layer: no library would read it either.
pub(crate) const UNREADABLE: &str =
    "not a readable PDF: the file is damaged or not a PDF (its structure could not be parsed)";

/// [`convert_text_layer`] restricted to a **1-based inclusive** page window
/// (issue #80's `--pages`); `None` converts everything. The window is
/// validated the same way as [`Pipeline::pages`]: `first <= last`, 1-based,
/// and it must select at least one existing page.
pub fn convert_text_layer_pages(
    bytes: &[u8],
    name: &str,
    pages: Option<(usize, usize)>,
) -> Result<DoclingDocument, PdfError> {
    if let Some((first, last)) = pages {
        if first == 0 || last < first {
            return Err(PdfError::Pdfium(format!(
                "invalid page range {first}-{last} (pages are 1-based, first <= last)"
            )));
        }
    }
    let mut doc = DoclingDocument::new(name);
    // A file nothing can open is an error, not a PDF that merely has no
    // text layer (the caller's "scanned? needs OCR" hint).
    let Some(mut parser) = textparse::PageTextParser::open(bytes) else {
        return Err(PdfError::Document(UNREADABLE.into()));
    };
    // Pages are parsed, assembled and dropped one at a time: holding every
    // page's cells until the end made the peak scale with the whole file —
    // and a page window still paid for every page outside it.
    let total = parser.page_count();
    let mut tally = textparse::TextLayerTally::default();
    for i in 0..total {
        let selected = pages.is_none_or(|(first, last)| (first..=last).contains(&(i + 1)));
        // A page outside the window only feeds the vestigial verdict below,
        // so it is parsed only until that verdict is settled (on any real
        // text layer, by the first page or two).
        if !selected && tally.proves_text(total) {
            continue;
        }
        let page = parser.text_page(i);
        tally.add(&page);
        if !selected {
            continue;
        }
        let mut regions = Vec::new();
        assemble::add_orphan_regions(&mut regions, &page.cells);
        let table_rows = vec![None; regions.len()];
        let enrich_out = vec![None; regions.len()];
        let (mut nodes, links) =
            assemble::assemble_page(&page, regions, &table_rows, &enrich_out, None);
        assemble::stamp_page_no(&mut nodes, i + 1);
        doc.nodes.extend(nodes);
        doc.links.extend(links);
    }
    // A vestigial layer (a few typed-in form fields over scanned pages) is not
    // the document's text: return the empty document, which callers already
    // report as "no text layer" — so an OCR-capable caller falls back to OCR
    // instead of proudly extracting thirteen characters.
    if tally.is_vestigial() {
        return Ok(DoclingDocument::new(name));
    }
    if let Some((first, last)) = pages {
        if first > total {
            return Err(PdfError::Pdfium(format!(
                "page range {first}-{last} is outside the document ({total} page(s))"
            )));
        }
    }
    assemble::merge_continuations(&mut doc.nodes);
    // Grown a page at a time, the node vector can be up to half spare
    // capacity — hundreds of MB on a long document, held for its lifetime.
    doc.nodes.shrink_to_fit();
    Ok(doc)
}

/// Threads ONNX inference may use, capped by `DOCLING_RS_PDF_THREADS` if set.
/// Defaults to the available parallelism (ort otherwise picks a low number).
#[cfg(feature = "ml")]
pub(crate) fn intra_threads() -> usize {
    if let Some(n) = env::parse::<usize>("DOCLING_RS_PDF_THREADS").filter(|&n| n > 0) {
        return n;
    }
    env::cpu_budget()
}

#[cfg(feature = "ml")]
/// TableFormer's intra-op width (#262): `DOCLING_RS_TF_INTRA` explicitly,
/// else the shared [`intra_threads`] budget. The shared TF session used to
/// take the raw host width on top of the already-sized worker pools —
/// under a cgroup CPU limit that oversubscription showed up as constant
/// throttling and ~66% higher peak memory (each intra thread carries its own
/// arena slab); the reporter's 4-CPU/8-core case dropped from 2.2 GB to
/// 1.3 GB peak by capping this pool.
pub(crate) fn tf_intra() -> usize {
    if let Some(n) = env::parse::<usize>("DOCLING_RS_TF_INTRA").filter(|&n| n > 0) {
        return n;
    }
    intra_threads()
}

#[cfg(feature = "ml")]
/// True when `DOCLING_RS_FP32` forces the full-precision models even where
/// an INT8 variant sits next to the fp32 default.
pub(crate) fn fp32_forced() -> bool {
    env::flag("DOCLING_RS_FP32")
}

#[cfg(feature = "ml")]
/// Should the int8 model defaults be skipped in favor of fp32? Either the
/// user said so (`DOCLING_RS_FP32`), or a GPU execution provider is selected
/// (#74) — the int8 exports are QDQ graphs calibrated for CPU kernels and
/// only conformance-validated there. An explicit `DOCLING_*_ONNX` path
/// override still wins over this at every call site.
pub(crate) fn prefer_fp32() -> bool {
    fp32_forced() || docling_onnx::prefers_fp32()
}

#[cfg(feature = "ml")]
/// Resolve a default (CWD-relative) asset path — the shared chain in
/// [`docling_core::assets`]: CWD, then next to the executable and one level
/// above it (the `scripts/install/install.sh` layout).
pub(crate) fn resolve_asset(rel: &str) -> String {
    docling_core::assets::resolve(rel)
}

/// One resolved runtime asset — which file a stage would load right now,
/// given the CWD, the env overrides and the int8/fp32 preference.
#[cfg(feature = "ml")]
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// Pipeline stage, e.g. `layout`, `tableformer.decoder`, `ocr.rec`.
    pub stage: &'static str,
    /// The resolved path (absolute or CWD-relative, as it will be opened).
    pub path: String,
    /// Whether the file exists right now.
    pub found: bool,
    /// File size in bytes (0 when missing) — enough to tell an int8 quant
    /// from an fp32 graph, or a stale model from a re-published one, at a
    /// glance without hashing gigabytes per request.
    pub bytes: u64,
}

/// Resolve the whole runtime model set **without loading anything** — the
/// exact selection each stage performs at load time (layout honors the
/// int8/fp32 preference, TableFormer its decoder ranking, OCR the language
/// pair), plus the pdfium library. docling-serve exposes this at
/// `/v1/config` and logs it at startup, so "the server picked up different
/// models" is one `curl` away instead of a mystery of dissolved tables.
/// Resolution is CWD-relative with an exe-dir fallback, so the answer can
/// legitimately differ between two working directories.
#[cfg(feature = "ml")]
pub fn model_inventory() -> Vec<ModelEntry> {
    fn entry(stage: &'static str, path: String) -> ModelEntry {
        let meta = std::fs::metadata(&path).ok();
        ModelEntry {
            stage,
            found: meta.is_some(),
            bytes: meta.map(|m| m.len()).unwrap_or(0),
            path,
        }
    }
    let (enc, dec, bbx) = tableformer::resolved_paths();
    let (rec, dict) = ocr::resolve_rec_pair(ocr::OcrLang::from_env());
    let pdfium =
        env::nonempty("PDFIUM_DYNAMIC_LIB_PATH").unwrap_or_else(|| resolve_asset(".pdfium/lib"));
    vec![
        entry(
            "layout",
            model_path(
                "DOCLING_LAYOUT_ONNX",
                ".models/layout_heron.onnx",
                ".models/layout_heron_int8.onnx",
            ),
        ),
        entry("tableformer.encoder", enc),
        entry("tableformer.decoder", dec),
        entry("tableformer.bbox", bbx),
        entry("ocr.rec", rec),
        entry("ocr.dict", dict),
        entry("ocr.textline_ori", ocr_cls::resolve_cls_path()),
        entry("pdfium", pdfium),
    ]
}

/// Resolve a model path: an explicit env override always wins; otherwise the
/// INT8 variant of the default path when it exists on disk (the quantized
/// models are conformance-validated — see docs/PDF_CONFORMANCE.md — and load/run
/// markedly faster on CPU), unless `DOCLING_RS_FP32` opts back into full
/// precision; else the fp32 default.
#[cfg(feature = "ml")]
pub(crate) fn model_path(key: &str, fp32_default: &str, int8_default: &str) -> String {
    if let Some(p) = env::nonempty(key) {
        return p;
    }
    if !prefer_fp32() {
        let p = resolve_asset(int8_default);
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    resolve_asset(fp32_default)
}

/// Decode a standalone image with hard resource limits. A crafted image can
/// declare enormous dimensions in a few-KB file; `image::load_from_memory`
/// then tries to allocate the full pixel buffer (e.g. 60000×60000 → ~10 GB),
/// and allocation failure aborts the whole process, bypassing the per-request
/// panic catch. The 256 MiB alloc / 30000-px caps below turn that into a
/// recoverable decode error instead. `DOCLING_RS_MAX_IMAGE_PIXELS` overrides
/// the per-side pixel cap for the rare legitimately-huge scan.
///
/// Gated on `ml`: the only callers (`convert_image`, the METS backend) are
/// ML-only, and the `image` crate is an `ml`-feature dependency — the
/// text-layer wasm build has neither.
#[cfg(feature = "ml")]
pub(crate) fn decode_image_limited(bytes: &[u8]) -> Result<image::RgbImage, PdfError> {
    let max_side: u32 = env::parse("DOCLING_RS_MAX_IMAGE_PIXELS").unwrap_or(30_000);
    decode_image_with_max_side(bytes, max_side)
}

/// Whether `bytes` is an ISOBMFF HEIF/HEIC container (the `ftyp` brands
/// iPhones write). Checked by content, not extension — HEIC regularly
/// arrives misnamed `.jpg`.
#[cfg(feature = "ml")]
fn is_heif(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"hevm" | b"hevs" | b"mif1" | b"msf1"
        )
}

/// Decode a HEIF/HEIC primary image via libheif (#211). Behind the opt-in
/// `heif` feature — libheif is a native dependency the default build (and
/// wasm) must not carry.
#[cfg(all(feature = "ml", feature = "heif"))]
fn decode_heif(bytes: &[u8], max_side: u32) -> Result<image::RgbImage, PdfError> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};
    let err = |e: String| PdfError::Pdfium(format!("heif: {e}"));
    let ctx = HeifContext::read_from_bytes(bytes).map_err(|e| err(e.to_string()))?;
    let handle = ctx.primary_image_handle().map_err(|e| err(e.to_string()))?;
    if handle.width() > max_side || handle.height() > max_side {
        return Err(err(format!(
            "image dimensions {}x{} exceed the {max_side}px per-side cap \
             (DOCLING_RS_MAX_IMAGE_PIXELS overrides)",
            handle.width(),
            handle.height()
        )));
    }
    let lib = LibHeif::new();
    let img = lib
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
        .map_err(|e| err(e.to_string()))?;
    let (w, h) = (img.width(), img.height());
    let planes = img.planes();
    let plane = planes
        .interleaved
        .ok_or_else(|| err("no RGB plane".into()))?;
    let stride = plane.stride;
    let mut out = image::RgbImage::new(w, h);
    for (y, row) in out.rows_mut().enumerate() {
        let src = &plane.data[y * stride..y * stride + w as usize * 3];
        for (x, px) in row.enumerate() {
            px.0 = [src[x * 3], src[x * 3 + 1], src[x * 3 + 2]];
        }
    }
    Ok(out)
}

#[cfg(feature = "ml")]
fn decode_image_with_max_side(bytes: &[u8], max_side: u32) -> Result<image::RgbImage, PdfError> {
    use image::{ImageDecoder, ImageReader};
    use std::io::Cursor;

    if is_heif(bytes) {
        #[cfg(feature = "heif")]
        return decode_heif(bytes, max_side);
        #[cfg(not(feature = "heif"))]
        return Err(PdfError::Pdfium(
            "HEIC/HEIF input needs a build with the `heif` cargo feature \
             (rebuild with --features heif; links the system libheif)"
                .into(),
        ));
    }

    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_side);
    limits.max_image_height = Some(max_side);
    limits.max_alloc = Some(256 * 1024 * 1024);

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| PdfError::Pdfium(format!("image: {e}")))?;
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| PdfError::Pdfium(format!("image: {e}")))?;
    // docling#4247 (2.128, `ImageOps.exif_transpose`): a camera stores the
    // sensor readout plus an orientation tag rather than rotated pixels, so
    // a portrait photo would reach layout and OCR on its side unless the tag
    // is honoured. An unreadable tag is no orientation, not an error.
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder)
        .map_err(|e| PdfError::Pdfium(format!("image: {e}")))?;
    img.apply_orientation(orientation);
    Ok(img.into_rgb8())
}

/// Image outputs of the PDF/image pipeline (#519/#520): the scale picture
/// crops are delivered at and whether each page's render is kept as the
/// document's page image. See [`Pipeline::images_scale`] /
/// [`Pipeline::generate_page_images`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ImageOutput {
    /// Pixels per PDF point for picture crops and page images — docling's
    /// `images_scale`. `None` keeps the pipeline's own page render (2.0
    /// px/pt), resampling nothing.
    pub scale: Option<f32>,
    /// Keep every page's render as the document's page image — docling's
    /// `generate_page_images`.
    pub page_images: bool,
}

#[cfg(feature = "ml")]
/// One page's assembled output: typed nodes plus the page's hyperlinks (kept
/// separate so pages processed out of order can be stitched back in page
/// order) and its confidence scores (#183).
type PageOut = (
    Vec<Node>,
    Vec<(String, String)>,
    docling_core::confidence::PageConfidence,
    // The page image, when page images are requested (#520).
    Option<docling_core::PictureImage>,
);

#[cfg(feature = "ml")]
/// A page between region resolution and TableFormer: what `prepare_page`
/// produced and `complete_page` still needs, so a pool worker can park the
/// page while another worker holds the shared TableFormer (see `Staged`).
struct Prepared {
    regions: Vec<layout::Region>,
    ocr_confs: Vec<f32>,
    parse: Option<f64>,
}

#[cfg(feature = "ml")]
/// A pool worker's per-page outcome. `NeedsTables` is a page whose only
/// remaining stage is the shared TableFormer, which was busy when the worker
/// got there: rather than block on the mutex — one worker idle for the
/// whole of another worker's table decode, ~9.5 s of the 60-page .NET slice
/// on a 2-worker pool — the worker keeps the page aside, pulls the next one
/// off the render channel, and comes back once the slot is free. Results
/// are reassembled by page index anyway, so completion order is free.
enum Staged {
    Done(PageOut),
    NeedsTables(Prepared),
}

#[cfg(feature = "ml")]
/// How many pages a pool worker keeps parked on the TableFormer before it
/// falls back to waiting: each carries its ~5 MB bitmaps, so this bounds the
/// extra residency to two pages per worker on top of the render channel.
const MAX_DEFERRED_PAGES: usize = 2;

#[cfg(feature = "ml")]
/// The pool-wide TableFormer slot: one instance shared by every worker, loaded
/// lazily on the first table region any worker sees. Tables appear on a
/// minority of pages, so per-worker copies mostly multiplied ~0.4 GB of
/// weights+arenas by the pool size for nothing; a single shared instance keeps
/// the peak flat regardless of pool width, and a table's structure prediction
/// is independent of which worker runs it, so output is byte-identical. The
/// mutex serialises concurrent tables — the shared instance is loaded with the
/// full intra-op thread budget to compensate (one wide TableFormer instead of
/// several narrow ones).
enum TfSlot {
    /// Not attempted yet (no table seen so far).
    Unloaded,
    /// Load attempted, graphs absent — geometric fallback (warned once).
    Missing,
    Ready(tableformer::TableFormer),
}

#[cfg(feature = "ml")]
impl TfSlot {
    /// Load TableFormer on the first call: `Ready`, or `Missing` when its
    /// graphs are absent (the geometric fallback — degradation, not an
    /// error). Later calls are no-ops either way.
    fn load(&mut self) {
        if matches!(self, TfSlot::Unloaded) {
            *self = match tableformer::TableFormer::load_with(tf_intra()) {
                Some(tf) => TfSlot::Ready(tf),
                None => TfSlot::Missing,
            };
        }
    }
}

#[cfg(feature = "ml")]
type SharedTables = Arc<Mutex<TfSlot>>;

#[cfg(feature = "ml")]
/// The same lazy shared-slot pattern for the (rarer still) enrichment models:
/// one instance per pipeline, loaded on the first region that needs it.
enum EnrichSlot<T> {
    Unloaded,
    /// Load attempted, model files absent — enrichment skipped (warned once).
    Missing,
    Ready(T),
}

#[cfg(feature = "ml")]
type SharedClassifier = Arc<Mutex<EnrichSlot<enrich::PictureClassifier>>>;
#[cfg(feature = "ml")]
type SharedCodeFormula = Arc<Mutex<EnrichSlot<enrich::CodeFormula>>>;

#[cfg(feature = "ml")]
/// The opt-in enrichment passes, mirroring docling's `PdfPipelineOptions`
/// flags (`do_picture_classification`, `do_code_enrichment`,
/// `do_formula_enrichment`). All off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnrichmentOptions {
    /// Classify each picture with DocumentFigureClassifier (26 classes).
    pub picture_classification: bool,
    /// Rewrite code blocks (and detect their language) with CodeFormulaV2.
    pub code: bool,
    /// Decode display formulas to LaTeX with CodeFormulaV2.
    pub formula: bool,
}

#[cfg(feature = "ml")]
impl EnrichmentOptions {
    fn any(&self) -> bool {
        self.picture_classification || self.code || self.formula
    }
}

#[cfg(feature = "ml")]
/// The layout model's input for a page: the pypdfium2-exact scale-1.0 page
/// image (#478) when the renderer produced one, else the legacy stretch of the 2×
/// bitmap (browser / METS paths) — see [`layout::LayoutSrc`]. Public so the
/// diagnostic examples feed [`layout::LayoutModel::predict`] the same input
/// the pipeline does.
pub fn layout_src(page: &PdfPage) -> layout::LayoutSrc<'_> {
    match &page.image_layout {
        Some(img) => layout::LayoutSrc::PageImage(img),
        None => layout::LayoutSrc::Raw(&page.image),
    }
}

#[cfg(feature = "ml")]
/// RapidOCR's longest side (`Global.max_side_len`, 2000): docling renders the
/// OCR input at `OcrOptions.scale` = 3 and RapidOCR then shrinks what exceeds
/// this before detecting and recognizing, so that is the resolution its
/// models actually see.
pub(crate) const RAPIDOCR_MAX_SIDE: f32 = 2000.0;

#[cfg(feature = "ml")]
/// The px/pt the OCR reads a page at: the caller's `ocr_scale` when set; else,
/// for a standalone bitmap input — its own scale-1.0 page, a resolution the
/// pipeline did not choose — docling's rule (#570): three pixels per point
/// (`OcrOptions.scale`), shrunk so the longer side stays within RapidOCR's
/// 2000 px (a 754 × 1000 scanned form reads at 2.0; a 3000 px photo at 0.67,
/// as RapidOCR would downsample it). `None` for a rendered page (PDF, hOCR),
/// which keeps its own render — the 2.0 px/pt the conformance baselines are
/// pinned to. Measured on FUNSD at 1.0 / 2.0 / 3.0 px/pt: 0.825 / 0.856 /
/// 0.836 word recall — the recognizer likes its crops at the resolution
/// RapidOCR hands it, no more.
fn page_ocr_scale(ocr_scale: Option<f32>, width: f32, height: f32, scale: f32) -> Option<f32> {
    if ocr_scale.is_some() || scale != 1.0 {
        return ocr_scale;
    }
    let longest = width.max(height);
    if longest <= 0.0 {
        return None;
    }
    let s = 3.0f32.min(RAPIDOCR_MAX_SIDE / longest);
    ((s - 1.0).abs() > 1e-3).then_some(s)
}

#[cfg(feature = "ml")]
/// The bitmap + px/pt scale the OCR reads (#254, docling#3877's
/// `OcrOptions.scale`): the page's own render unless `ocr_scale` asks for a
/// different resolution, where a PIL-bicubic resample of that render is built
/// once per page (cached in `cache`) and shared by every OCR pass. Resampling
/// — rather than a second native pdfium render — keeps one code path across
/// PDF, image, and hOCR inputs and leaves the layout/TableFormer pixels (and
/// with them the conformance baseline) untouched; the 144-dpi base render is
/// itself supersampled down from 216 dpi, so an upscaled OCR view loses
/// little against a native render.
fn ocr_input<'a>(
    cache: &'a mut Option<image::RgbImage>,
    image: &'a image::RgbImage,
    scale: f32,
    ocr_scale: Option<f32>,
) -> (&'a image::RgbImage, f32) {
    match ocr_scale {
        Some(s) if (s - scale).abs() > 1e-3 && image.width() > 1 => {
            let f = s / scale;
            let img = cache.get_or_insert_with(|| {
                let dw = ((image.width() as f32 * f).round() as u32).max(1);
                let dh = ((image.height() as f32 * f).round() as u32).max(1);
                resample::pil_resize(image, dw, dh, resample::PilFilter::Bicubic)
            });
            (img, s)
        }
        _ => (image, scale),
    }
}

#[cfg(feature = "ml")]
/// A self-contained set of the per-page models (layout, OCR). Each parallel
/// page-worker owns its own `Worker` so inference runs concurrently without
/// sharing an ONNX session (`ort`'s `Session::run` is `&mut self`); only the
/// rarely-hit TableFormer is shared (see [`TfSlot`]).
struct Worker {
    /// `None` when `no_ocr` skips layout entirely — no model load, no inference.
    layout: Option<layout::LayoutModel>,
    ocr: OcrSlot,
    det: DetSlot,
    /// Text-detector results computed ahead of a page's OCR pass (#429), keyed
    /// by page index: the detector reads nothing but the page image, so it
    /// runs on its own thread while the layout model predicts — see
    /// [`Self::detect_alongside`] — and the OCR block collects the boxes here
    /// instead of paying for detection serially.
    pending_det: BTreeMap<usize, Result<Vec<ocr_det::DetBox>, String>>,
    /// This worker's intra-op thread budget — also the OCR lane count (see
    /// [`ocr::OcrModel::load_with`]): a pool worker with two threads runs two
    /// single-thread recognisers, the primary as many as its cores.
    intra: usize,
    /// Shared TableFormer slot; `None` when `no_table_former`/`no_ocr` skip it.
    tables: Option<SharedTables>,
    /// Shared enrichment slots; `None` unless the corresponding flag is on.
    classifier: Option<SharedClassifier>,
    code_formula: Option<SharedCodeFormula>,
    enrich: EnrichmentOptions,
    /// Skip layout, OCR, and TableFormer; reconstruct text purely from the PDF's
    /// embedded text layer. See [`Pipeline::no_ocr`].
    no_ocr: bool,
    /// Discard the embedded text layer and OCR every page. See
    /// [`Pipeline::force_full_page_ocr`].
    force_full_page_ocr: bool,
    /// Keep text-panel pictures as pictures instead of demoting them to
    /// paragraphs. See [`Pipeline::no_text_panels`].
    no_text_panels: bool,
    /// Never run OCR, but keep layout + TableFormer (#244) — docling's
    /// `do_ocr=False`. See [`Pipeline::skip_ocr`].
    skip_ocr: bool,
    /// Which recognition model [`Self::ocr`] loads. See [`Pipeline::ocr_lang`].
    ocr_lang: ocr::OcrLang,
    /// Which engine [`Self::ocr`] is (#460). See [`Pipeline::ocr_engine`].
    ocr_engine: ocr::OcrEngine,
    /// Tesseract's `-l` argument (#460). See [`Pipeline::tesseract_lang`].
    tesseract_lang: Option<String>,
    /// OCR render scale override (px/pt, #254). See [`Pipeline::ocr_scale`].
    ocr_scale: Option<f32>,
    /// RapidOCR's line orientation handling (#666), handed to the PP-OCR
    /// recognizer. See [`Pipeline::ocr_textline_orientation`].
    textline_orientation: bool,
    /// Picture-crop scale and page images (#519/#520). See [`ImageOutput`].
    images: ImageOutput,
}

#[cfg(feature = "ml")]
/// The worker's lazily-loaded OCR recognition model. `Missing` records a
/// failed load (#244: degradation over failure — a deployment without the OCR
/// model still gets layout + TableFormer, and OCR-dependent regions stay
/// empty) so the load isn't retried per page.
enum OcrSlot {
    Unloaded,
    Ready(Recognizer),
    Missing,
}

#[cfg(feature = "ml")]
/// The loaded OCR engine (#460): PP-OCRv3 via ONNX Runtime or the
/// `tesseract` binary. Both answer the two questions the page assembly asks
/// — line cells for text regions, word cells for table regions — in page
/// points with a 0–1 confidence, so the call sites never know which runs.
pub(crate) enum Recognizer {
    PpOcr(ocr::OcrModel),
    Tesseract(tesseract::TesseractOcr),
}

#[cfg(feature = "ml")]
impl Recognizer {
    /// See [`ocr::OcrModel::ocr_page_with`].
    fn ocr_page(
        &mut self,
        img: &image::RgbImage,
        regions: &[layout::Region],
        scale: f32,
    ) -> Result<Vec<(TextCell, f32)>, String> {
        self.ocr_page_with(img, regions, scale, None)
    }

    /// [`Self::ocr_page`] with the detector's boxes (#570). Tesseract segments its
    /// own lines and ignores the detector's boxes.
    fn ocr_page_with(
        &mut self,
        img: &image::RgbImage,
        regions: &[layout::Region],
        scale: f32,
        detected: Option<&[ocr_det::DetBox]>,
    ) -> Result<Vec<(TextCell, f32)>, String> {
        match self {
            Self::PpOcr(m) => m.ocr_page_with(img, regions, scale, detected),
            Self::Tesseract(t) => t.ocr_page(img, regions, scale),
        }
    }

    /// See [`ocr::OcrModel::ocr_table_words`].
    fn ocr_table_words(
        &mut self,
        img: &image::RgbImage,
        regions: &[layout::Region],
        scale: f32,
        detected: Option<&[ocr_det::DetBox]>,
    ) -> Result<Vec<(TextCell, f32)>, String> {
        match self {
            Self::PpOcr(m) => m.ocr_table_words(img, regions, scale, detected),
            Self::Tesseract(t) => t.ocr_table_words(img, regions, scale),
        }
    }
}

#[cfg(feature = "ml")]
/// The text detector (#429), loaded on the first OCR'd page like the
/// recognizer; `Missing` keeps the pipeline recognition-only.
enum DetSlot {
    Unloaded,
    Ready(ocr_det::DetModel),
    Missing,
}

#[cfg(feature = "ml")]
impl Worker {
    #[allow(clippy::too_many_arguments)] // mirrors the Pipeline's option set
    fn load(
        intra: usize,
        tables: Option<SharedTables>,
        enrich_slots: (Option<SharedClassifier>, Option<SharedCodeFormula>),
        enrich: EnrichmentOptions,
        no_ocr: bool,
        skip_ocr: bool,
        force_full_page_ocr: bool,
        no_text_panels: bool,
        ocr_lang: ocr::OcrLang,
        ocr_engine: ocr::OcrEngine,
        tesseract_lang: Option<String>,
        ocr_scale: Option<f32>,
        textline_orientation: bool,
        images: ImageOutput,
    ) -> Result<Self, PdfError> {
        Ok(Self {
            images,
            layout: if no_ocr {
                None
            } else {
                Some(layout::LayoutModel::load_with(intra).map_err(PdfError::Layout)?)
            },
            ocr: OcrSlot::Unloaded,
            det: DetSlot::Unloaded,
            pending_det: BTreeMap::new(),
            intra,
            tables,
            classifier: enrich_slots.0,
            code_formula: enrich_slots.1,
            enrich,
            no_ocr,
            skip_ocr,
            force_full_page_ocr,
            no_text_panels,
            ocr_lang,
            ocr_engine,
            tesseract_lang,
            ocr_scale,
            textline_orientation,
        })
    }

    /// Set [`Pipeline::ocr_textline_orientation`] here and on a loaded
    /// PP-OCR recognizer.
    fn set_textline_orientation(&mut self, enable: bool) {
        self.textline_orientation = enable;
        if let OcrSlot::Ready(Recognizer::PpOcr(m)) = &mut self.ocr {
            m.set_textline_orientation(enable);
        }
    }

    /// The OCR model, or `None` when this conversion must not (or cannot) OCR:
    /// `skip_ocr` short-circuits, and a failed model load degrades to `None`
    /// with a one-time warning instead of failing the conversion (#244) —
    /// unless `force_full_page_ocr` demanded OCR explicitly, where a missing
    /// model stays a hard error (the text layer was deliberately discarded, so
    /// degrading would silently emit an empty document).
    fn ocr_model(&mut self) -> Result<Option<&mut Recognizer>, PdfError> {
        if self.skip_ocr {
            return Ok(None);
        }
        if matches!(self.ocr, OcrSlot::Unloaded) {
            let loaded = match self.ocr_engine {
                ocr::OcrEngine::PpOcr => {
                    ocr::OcrModel::load_with(self.ocr_lang, self.intra).map(|mut m| {
                        m.set_textline_orientation(self.textline_orientation);
                        Recognizer::PpOcr(m)
                    })
                }
                ocr::OcrEngine::Tesseract => tesseract::TesseractOcr::load(
                    tesseract::TesseractOptions::from_env(self.tesseract_lang.clone()),
                    self.intra,
                )
                .map(Recognizer::Tesseract),
            };
            match loaded {
                Ok(model) => self.ocr = OcrSlot::Ready(model),
                Err(e) if self.force_full_page_ocr => return Err(PdfError::Ocr(e)),
                Err(e) => {
                    static WARNED: std::sync::Once = std::sync::Once::new();
                    let hint = match self.ocr_engine {
                        ocr::OcrEngine::PpOcr => {
                            "run scripts/install/download_dependencies.sh for the model"
                        }
                        ocr::OcrEngine::Tesseract => {
                            "install tesseract-ocr with the language pack, or use --ocr-engine ppocr"
                        }
                    };
                    WARNED.call_once(|| {
                        eprintln!(
                            "warning: OCR model unavailable ({e}); continuing without OCR — \
                             scanned pages and text inside images will come back empty \
                             ({hint})"
                        );
                    });
                    self.ocr = OcrSlot::Missing;
                }
            }
        }
        Ok(match &mut self.ocr {
            OcrSlot::Ready(model) => Some(model),
            _ => None,
        })
    }

    /// The text detector (#429): loaded on first use, `None` with `skip_ocr`
    /// or when the model is not installed — recognition stays region-scoped
    /// then, exactly the pre-#429 behavior, and `DOCLING_RS_DEBUG` says why.
    fn det_model(&mut self) -> Option<&mut ocr_det::DetModel> {
        if self.skip_ocr {
            return None;
        }
        if matches!(self.det, DetSlot::Unloaded) {
            match ocr_det::DetModel::load(self.intra) {
                Ok(model) => self.det = DetSlot::Ready(model),
                Err(e) => {
                    docling_core::debug_log!(
                        "docling-pdf: text detection unavailable ({e}); OCR stays region-scoped"
                    );
                    self.det = DetSlot::Missing;
                }
            }
        }
        match &mut self.det {
            DetSlot::Ready(model) => Some(model),
            _ => None,
        }
    }

    /// Run layout (+ OCR for cell-less pages) + TableFormer and assemble page `n`
    /// into its nodes and links. Pure given the page (mutates only the worker's
    /// lazily-loaded OCR model), so it is safe to run concurrently across pages.
    fn process(&mut self, n: usize, page: &mut PdfPage) -> Result<PageOut, PdfError> {
        if self.no_ocr {
            // Fastest path: no layout/OCR/TableFormer inference at all. The PDF's
            // embedded text cells (if any) become flat, line-grouped paragraphs in
            // reading order via the same orphan-region machinery that normally
            // rescues text the detector missed — here it rescues *all* of it.
            // Pages with no embedded text layer (scanned/image-only) yield nothing;
            // convert those without `no_ocr`.
            let parse = quality::parse_score(&page.cells);
            let mut regions = Vec::new();
            assemble::add_orphan_regions(&mut regions, &page.cells);
            let table_rows = vec![None; regions.len()];
            let enrich_out = vec![None; regions.len()];
            let conf = quality::page_confidence(parse, &regions, &[]);
            let (nodes, links) = timing::timed("assemble_page", || {
                assemble::assemble_page(page, regions, &table_rows, &enrich_out, self.images.scale)
            });
            return Ok((nodes, links, conf, self.page_image(n, page)));
        }
        self.normalize_orientation(n, page)?;
        let det_pages = self.det_candidates(std::slice::from_ref(&(n, &*page)))?;
        let regions = {
            let Self {
                layout,
                det,
                pending_det,
                ocr_scale,
                ..
            } = self;
            let layout = layout.as_mut().expect("layout model loaded unless no_ocr");
            let page: &PdfPage = &*page;
            Self::detect_alongside(det, pending_det, *ocr_scale, &det_pages, || {
                timing::timed("layout.predict", || {
                    layout.predict(layout_src(page), page.width, page.height)
                })
            })
        }
        .map_err(|e| PdfError::Layout(format!("page {}: {e}", n + 1)))?;
        // The 2x bitmap, rendered meanwhile (#682): everything from here on
        // reads it.
        page.resolve_image()?;
        self.finish_page(n, page, regions)
    }

    /// The page's render as docling's `PageItem.image` (#520), at
    /// [`ImageOutput::scale`] — `None` unless page images were requested, or
    /// when the page has no bitmap (the text-layer-only `no_ocr` path never
    /// renders one).
    fn page_image(&self, n: usize, page: &PdfPage) -> Option<docling_core::PictureImage> {
        if !self.images.page_images {
            return None;
        }
        let image = assemble::page_image(page, self.images.scale);
        if image.is_none() {
            docling_core::debug_log!("page {}: no page bitmap for the page image", n + 1);
        }
        image
    }

    /// The pages of a batch the text detector should sweep (#429): bitmap
    /// pages (no text layer) on a run with OCR and the detector available —
    /// loading both lazily here so the concurrent run below needs no `self`.
    fn det_candidates<'p>(
        &mut self,
        pages: &[(usize, &'p PdfPage)],
    ) -> Result<Vec<(usize, &'p PdfPage)>, PdfError> {
        // Pages the orientation pass already detected (#571) are skipped:
        // their boxes wait in `pending_det`.
        let scanned: Vec<(usize, &PdfPage)> = pages
            .iter()
            .filter(|(n, p)| {
                p.cells.is_empty() && p.image.width() > 1 && !self.pending_det.contains_key(n)
            })
            .copied()
            .collect();
        if scanned.is_empty() || self.ocr_model()?.is_none() || self.det_model().is_none() {
            return Ok(Vec::new());
        }
        Ok(scanned)
    }

    /// Run `layout` on the calling thread while the text detector sweeps
    /// `det_pages` on another (#429): the detector only reads each page's
    /// image, layout only reads the pages too, and the two sessions are
    /// independent, so on a scanned page the ~0.7 s detection hides behind
    /// layout instead of adding to it. Results land in `pending_det` for the
    /// OCR block; a detector failure is recorded per page and surfaces there.
    fn detect_alongside<T>(
        det: &mut DetSlot,
        pending_det: &mut BTreeMap<usize, Result<Vec<ocr_det::DetBox>, String>>,
        ocr_scale: Option<f32>,
        det_pages: &[(usize, &PdfPage)],
        layout: impl FnOnce() -> T,
    ) -> T {
        let DetSlot::Ready(det) = det else {
            return layout();
        };
        if det_pages.is_empty() {
            return layout();
        }
        std::thread::scope(|s| {
            let handle = s.spawn(move || {
                det_pages
                    .iter()
                    .map(|&(n, page)| {
                        let mut view = None;
                        let (img, _) = ocr_input(
                            &mut view,
                            &page.image,
                            page.scale,
                            page_ocr_scale(ocr_scale, page.width, page.height, page.scale),
                        );
                        (n, timing::timed("ocr.det", || det.detect(img)))
                    })
                    .collect::<Vec<_>>()
            });
            let out = layout();
            match handle.join() {
                Ok(results) => pending_det.extend(results),
                Err(_) => {
                    for &(n, _) in det_pages {
                        pending_det.insert(n, Err("ocr-det: detection thread panicked".into()));
                    }
                }
            }
            out
        })
    }

    /// Content-based orientation normalization (#225), before any inference:
    /// a physically rotated scan (sideways phone photo, landscape-fed sheet)
    /// has `/Rotate 0`, so the metadata pass in `extract_page` never fires and
    /// layout+OCR would read a sideways raster. Only pages with no text layer
    /// at all are probed (a digital page's raster is upright by construction,
    /// and its cells — not its pixels — carry the text); the detected angle
    /// composes with any `/Rotate` normalization through the same
    /// [`PdfPage::unrotate`] + display-space assembly mapping. Detection is
    /// evidence-gated and degrades to a no-op — see [`orient`].
    fn normalize_orientation(&mut self, n: usize, page: &mut PdfPage) -> Result<(), PdfError> {
        let scanned =
            page.cells.is_empty() && page.word_cells.is_empty() && page.code_cells.is_empty();
        if self.no_ocr || self.skip_ocr || !scanned || page.image.width() <= 1 || !orient::enabled()
        {
            return Ok(());
        }
        // The probe reads text through the OCR model; without one (missing —
        // #244 degradation) the page stays as rendered.
        if self.ocr_model()?.is_none() {
            return Ok(());
        }
        // The text detector's lines are the probe's crops (#571). Detected on
        // the page image as rendered; an upright page keeps them for the OCR
        // pass (`pending_det`) when that pass reads the same bitmap, so the
        // detector runs once per page as before — a rotated page detects
        // again on the un-rotated image, alongside layout.
        let boxes: Vec<ocr_det::DetBox> = match self.det_model() {
            Some(det) => timing::timed("orient.det", || det.detect(&page.image)).unwrap_or_else(|e| {
                debug_log!("docling-pdf: page {}: text detection failed ({e}); probing projection strips", n + 1);
                Vec::new()
            }),
            None => Vec::new(),
        };
        let ocr_scale = page_ocr_scale(self.ocr_scale, page.width, page.height, page.scale);
        let Some(ocr) = self.ocr_model()? else {
            return Ok(());
        };
        let deg = timing::timed("orient.detect", || {
            orient::detect(&page.image, ocr, page.scale, &boxes)
        });
        if deg != 0 {
            debug_log!(
                "docling-pdf: page {}: content rotated {deg}° in the raster; \
                 un-rotating before layout/OCR",
                n + 1
            );
            page.unrotate(deg);
        } else if !boxes.is_empty() && ocr_scale.is_none_or(|s| (s - page.scale).abs() <= 1e-3) {
            self.pending_det.insert(n, Ok(boxes));
        }
        Ok(())
    }

    /// Layout-detect a whole batch of pages with one inference call (issue #73),
    /// then run each page's remaining stages (OCR / TableFormer / enrichment /
    /// assembly) per page. Index-aligned with `items`; a layout failure fails
    /// every page in the batch (they shared the one inference call).
    fn process_batch(&mut self, items: &mut [(usize, PdfPage)]) -> Vec<Result<Staged, PdfError>> {
        if self.no_ocr {
            // No layout model to batch — the text-layer-only path is per page.
            return items
                .iter_mut()
                .map(|(n, page)| {
                    let n = *n;
                    self.process(n, page).map(Staged::Done)
                })
                .collect();
        }
        // Orientation-normalize every scanned page before the shared layout
        // call — the batched inference must see upright bitmaps too (#225).
        for (n, page) in items.iter_mut() {
            let n = *n;
            if let Err(e) = self.normalize_orientation(n, page) {
                // Model-load failure — every page in the batch needs the same
                // model, so they all fail alike (mirrors the layout-error arm).
                let msg = e.to_string();
                return items
                    .iter()
                    .map(|_| Err(PdfError::Ocr(msg.clone())))
                    .collect();
            }
        }
        let inputs: Vec<(layout::LayoutSrc<'_>, f32, f32)> = items
            .iter()
            .map(|(_, page)| (layout_src(page), page.width, page.height))
            .collect();
        let refs: Vec<(usize, &PdfPage)> = items.iter().map(|(n, page)| (*n, page)).collect();
        let det_pages = match self.det_candidates(&refs) {
            Ok(p) => p,
            Err(e) => {
                let msg = e.to_string();
                return items
                    .iter()
                    .map(|_| Err(PdfError::Ocr(msg.clone())))
                    .collect();
            }
        };
        let batched = {
            let Self {
                layout,
                det,
                pending_det,
                ocr_scale,
                ..
            } = self;
            let layout = layout.as_mut().expect("layout model loaded unless no_ocr");
            Self::detect_alongside(det, pending_det, *ocr_scale, &det_pages, || {
                timing::timed("layout.predict", || layout.predict_batch(&inputs))
            })
        };
        match batched {
            Ok(all) => items
                .iter_mut()
                .zip(all)
                .map(|((n, page), regions)| {
                    // The 2x bitmap, rendered meanwhile (#682).
                    page.resolve_image()?;
                    self.stage_page(*n, page, regions)
                })
                .collect(),
            Err(e) => items
                .iter()
                .map(|(n, _)| Err(PdfError::Layout(format!("page {}: {e}", n + 1))))
                .collect(),
        }
    }

    /// A pool worker's main loop: pull rendered pages off the shared channel
    /// (whatever is already there, up to the layout batch size), process them,
    /// and hand each finished page — or its error — to `deliver`, which returns
    /// `false` to stop early (the streaming consumer went away). Returns when
    /// the channel is closed and every page this worker took is delivered.
    ///
    /// Pages whose TableFormer turn would have to wait are parked (see
    /// `Staged`) and retried before every new pull; while any are parked the
    /// pull is non-blocking, so an empty channel means the worker waits for
    /// the TableFormer rather than for the renderer. The parking budget
    /// (`MAX_DEFERRED_PAGES`) bounds resident bitmaps; past it the worker waits
    /// like the serial path. Output is independent of completion order — the
    /// callers reassemble by page index.
    fn run_pool(
        &mut self,
        work_rx: &Mutex<Receiver<(usize, PdfPage)>>,
        layout_batch: usize,
        deadline: Option<std::time::Instant>,
        mut deliver: impl FnMut(usize, Result<PageOut, PdfError>) -> bool,
    ) {
        use std::sync::mpsc::TryRecvError;
        let mut deferred: std::collections::VecDeque<(usize, PdfPage, Prepared)> =
            std::collections::VecDeque::new();
        loop {
            // The document budget (#497), checked between pages: once it is
            // spent, every page still queued or parked is reported as timed
            // out instead of processed — the renderer stops feeding at the
            // same check, so the queue drains quickly.
            if expired(deadline) {
                while let Some((idx, _, _)) = deferred.pop_front() {
                    if !deliver(idx, Err(timeout_sentinel())) {
                        return;
                    }
                }
                let rx = work_rx.lock().unwrap();
                loop {
                    match rx.recv() {
                        Ok((idx, _page)) => {
                            if !deliver(idx, Err(timeout_sentinel())) {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
            }
            // Any parked page the slot has since freed up for, oldest first.
            let mut i = 0;
            while i < deferred.len() {
                let (_, page, prepared) = &deferred[i];
                match self.table_rows_try(page, &prepared.regions) {
                    Some(rows) => {
                        let (idx, mut page, prepared) = deferred.remove(i).expect("index in range");
                        if !deliver(idx, self.complete_page(idx, &mut page, prepared, rows)) {
                            return;
                        }
                    }
                    None => i += 1,
                }
            }
            // Hold the receiver lock only for the recv (plus a non-blocking drain
            // up to the layout batch size); release before the (long) per-page
            // work so other workers can pull concurrently.
            let mut batch: Vec<(usize, PdfPage)> = Vec::new();
            let mut closed = false;
            {
                let rx = work_rx.lock().unwrap();
                let first = if deferred.is_empty() {
                    rx.recv().map_err(|_| TryRecvError::Disconnected)
                } else {
                    rx.try_recv()
                };
                match first {
                    Ok(item) => {
                        batch.push(item);
                        while batch.len() < layout_batch {
                            match rx.try_recv() {
                                Ok(item) => batch.push(item),
                                Err(_) => break,
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => closed = true,
                }
            }
            if batch.is_empty() {
                // Nothing new rendered (or the channel is closed): wait our turn
                // on the oldest parked page instead.
                match deferred.pop_front() {
                    Some((idx, mut page, prepared)) => {
                        let rows = self.table_rows_blocking(&page, &prepared.regions);
                        if !deliver(idx, self.complete_page(idx, &mut page, prepared, rows)) {
                            return;
                        }
                        continue;
                    }
                    None if closed => return,
                    None => continue,
                }
            }
            let outs = self.process_batch(&mut batch);
            for ((idx, mut page), out) in batch.into_iter().zip(outs) {
                let delivered = match out {
                    Ok(Staged::Done(out)) => deliver(idx, Ok(out)),
                    Ok(Staged::NeedsTables(prepared)) => {
                        if deferred.len() < MAX_DEFERRED_PAGES {
                            deferred.push_back((idx, page, prepared));
                            true
                        } else {
                            let rows = self.table_rows_blocking(&page, &prepared.regions);
                            deliver(idx, self.complete_page(idx, &mut page, prepared, rows))
                        }
                    }
                    Err(e) => deliver(idx, Err(e)),
                };
                if !delivered {
                    return;
                }
            }
        }
    }

    /// Everything after layout detection: per-label confidence thresholds,
    /// overlap resolution, orphan-text recovery, OCR for cell-less pages,
    /// TableFormer, enrichment, and page assembly. The serial path: waits
    /// for the shared TableFormer when a table needs it.
    fn finish_page(
        &mut self,
        n: usize,
        page: &mut PdfPage,
        regions: Vec<layout::Region>,
    ) -> Result<PageOut, PdfError> {
        let prepared = self.prepare_page(n, page, regions)?;
        let table_rows = self.table_rows_blocking(page, &prepared.regions);
        self.complete_page(n, page, prepared, table_rows)
    }

    /// The pool path: like [`finish_page`](Self::finish_page), except that a
    /// page whose TableFormer turn would have to wait comes back as
    /// [`Staged::NeedsTables`] for the worker loop to park (see `Staged`).
    fn stage_page(
        &mut self,
        n: usize,
        page: &mut PdfPage,
        regions: Vec<layout::Region>,
    ) -> Result<Staged, PdfError> {
        let prepared = self.prepare_page(n, page, regions)?;
        match self.table_rows_try(page, &prepared.regions) {
            Some(rows) => Ok(Staged::Done(self.complete_page(n, page, prepared, rows)?)),
            None => Ok(Staged::NeedsTables(prepared)),
        }
    }

    /// Does this page need the shared TableFormer at all? Table-free pages
    /// never touch (or load) it.
    fn needs_tables(&self, regions: &[layout::Region]) -> bool {
        self.tables.is_some() && regions.iter().any(|r| assemble::is_table_like(r.label))
    }

    /// TableFormer structure for every table region of the page, on an
    /// already-locked slot (loading the model on first use). Tables serialise
    /// on this mutex, so the one instance gets the shared thread budget
    /// (quota-aware, #262) — DOCLING_RS_TF_INTRA narrows it further where the
    /// memory-per-thread tradeoff matters more than table latency.
    fn predict_tables(
        guard: &mut TfSlot,
        page: &PdfPage,
        regions: &[layout::Region],
    ) -> Vec<Option<tf_core::TableGrid>> {
        let mut table_rows: Vec<Option<tf_core::TableGrid>> = vec![None; regions.len()];
        guard.load();
        if let TfSlot::Ready(tf) = guard {
            // One 1024-px frame per page, shared by all of its tables, and one
            // call for all of them: with the dynamic-batch decoder their
            // decode steps are shared (each step costs about the same for B
            // tables as for one).
            let page1024 = tableformer::TableFormer::page_1024(&page.image);
            let (idx, boxes): (Vec<usize>, Vec<[f32; 4]>) = regions
                .iter()
                .enumerate()
                .filter(|(_, r)| assemble::is_table_like(r.label))
                .map(|(i, r)| (i, [r.l, r.t, r.r, r.b]))
                .unzip();
            let rows =
                tf.predict_tables_on(page.image.height(), &page1024, &boxes, &page.word_cells);
            for (i, grid) in idx.into_iter().zip(rows) {
                table_rows[i] = grid;
            }
        }
        table_rows
    }

    /// Table structure for the page, waiting for the shared slot if another
    /// worker holds it (else geometric fallback downstream when there is no
    /// TableFormer at all). The `tableformer` timing stage here includes any
    /// wait.
    fn table_rows_blocking(
        &self,
        page: &PdfPage,
        regions: &[layout::Region],
    ) -> Vec<Option<tf_core::TableGrid>> {
        match self.tables.as_ref().filter(|_| self.needs_tables(regions)) {
            Some(slot) => timing::timed("tableformer", || {
                Self::predict_tables(&mut slot.lock().unwrap(), page, regions)
            }),
            None => vec![None; regions.len()],
        }
    }

    /// Non-blocking variant: `None` when the slot is held by another worker
    /// right now — the caller parks the page and tries again later.
    fn table_rows_try(
        &self,
        page: &PdfPage,
        regions: &[layout::Region],
    ) -> Option<Vec<Option<tf_core::TableGrid>>> {
        let Some(slot) = self.tables.as_ref().filter(|_| self.needs_tables(regions)) else {
            return Some(vec![None; regions.len()]);
        };
        match slot.try_lock() {
            Ok(mut guard) => Some(timing::timed("tableformer", || {
                Self::predict_tables(&mut guard, page, regions)
            })),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(e)) => panic!("TableFormer slot poisoned: {e}"),
        }
    }

    /// The stages before TableFormer: fp32 escalation, per-label confidence
    /// thresholds, overlap resolution, orphan-text recovery, OCR for cell-less
    /// pages, in-picture text and table-word recognition.
    fn prepare_page(
        &mut self,
        n: usize,
        page: &mut PdfPage,
        regions: Vec<layout::Region>,
    ) -> Result<Prepared, PdfError> {
        // Force-OCR is exactly "pretend the text layer is not there": clear
        // every cell kind the extractors produced before anything reads them,
        // and the ordinary no-text-layer machinery below — full-page OCR,
        // OCR-fed TableFormer matching — takes over unchanged. (`no_ocr` wins
        // when both are set, mirroring docling, where `force_full_page_ocr`
        // is a sub-option of `do_ocr`; the no-ocr path never reaches here.)
        // Done here rather than in `process` so the batched layout path
        // (`process_batch` → `finish_page`) honors the flag too.
        // Parse quality is scored on the extracted text layer before force-OCR
        // discards it (docling's page-preprocessing stage runs before OCR too,
        // so its parse_score also reflects the original text layer).
        let parse = quality::parse_score(&page.cells);
        // Recognition confidences of every OCR'd cell on this page → ocr_score.
        let mut ocr_confs: Vec<f32> = Vec::new();
        // The bitmap the OCR reads (#254): with `ocr_scale` set, a resample of
        // the page render at the requested px/pt, built lazily on the first
        // OCR use so non-OCR pages never pay for it. Copied out of `self` up
        // front — the OCR sites hold `self.ocr_model()`'s mutable borrow.
        let ocr_scale = page_ocr_scale(self.ocr_scale, page.width, page.height, page.scale);
        let mut ocr_view: Option<image::RgbImage> = None;
        if self.force_full_page_ocr {
            page.cells.clear();
            page.code_cells.clear();
            page.word_cells.clear();
        }
        // Quant-robustness guard: the default int8 layout graph keeps its
        // confidences near the 0.5 label thresholds, and a different CPU's
        // quantized kernels can flip a whole page's detections under them —
        // tables and paragraphs then dissolve into orphan one-liners while the
        // same build converts the page perfectly elsewhere. When a dense
        // digital page ends up with detections covering almost none of its
        // text cells, re-run that one page on the fp32 graph (lazy-loaded,
        // auto-int8 selection only) and keep whichever detections cover more.
        let mut regions = regions;
        if !page.cells.is_empty() {
            let thresholded = |rs: &[layout::Region]| -> Vec<layout::Region> {
                rs.iter()
                    .filter(|r| r.score >= layout::label_threshold(r.label))
                    .cloned()
                    .collect()
            };
            let text_cells = page
                .cells
                .iter()
                .filter(|c| !c.text.trim().is_empty())
                .count();
            let cov = assemble::layout_cell_coverage(&thresholded(&regions), &page.cells);
            if text_cells >= 15 && cov < 0.5 {
                let retry = self
                    .layout
                    .as_mut()
                    .expect("layout model loaded unless no_ocr")
                    .predict_fp32_fallback(layout_src(page), page.width, page.height)
                    .map_err(|e| PdfError::Layout(format!("page {}: {e}", n + 1)))?;
                if let Some(retry) = retry {
                    let cov2 = assemble::layout_cell_coverage(&thresholded(&retry), &page.cells);
                    if cov2 > cov {
                        debug_log!(
                            "docling-pdf: page {}: int8 layout covered {:.0}% of the text \
                             cells; the fp32 retry covers {:.0}% — using it",
                            n + 1,
                            cov * 100.0,
                            cov2 * 100.0
                        );
                        regions = retry;
                    }
                }
            }
        }
        // docling's LayoutPostprocessor drops each detection below its label's
        // confidence threshold (stricter than the 0.3 base the predictor keeps),
        // before any overlap resolution. This removes the low-confidence tables /
        // pictures / list-items that otherwise double-emit or mis-classify.
        if env::flag("DOCLING_RS_DEBUG_REGIONS") {
            for r in &regions {
                eprintln!(
                    "DBG raw {} {:.2} [{:.0},{:.0},{:.0},{:.0}]",
                    r.label, r.score, r.l, r.t, r.r, r.b
                );
            }
        }
        // A picture the page captions "Table N" and the model also proposed
        // as a table is read as a table (deliberate deviation).
        assemble::promote_captioned_table_pictures(&mut regions, &page.cells);
        regions.retain(|r| r.score >= layout::label_threshold(r.label));
        // docling's full-page picture filter and same-label picture dedup run
        // on the thresholded detections, before overlap resolution: a picture
        // that is the whole page goes (its text reads out as text), and a
        // figure proposed both whole and as sub-panels collapses to one box
        // (see `dedup_pictures`).
        assemble::drop_full_page_pictures(&mut regions, page.width, page.height);
        assemble::dedup_pictures(&mut regions);
        // Resolve overlapping detections once, before OCR.
        let mut regions = assemble::resolve(regions);
        // Emit text the detector missed as orphan text regions (docling parity).
        assemble::add_orphan_regions(&mut regions, &page.cells);
        // Drop phantom empty low-confidence picture boxes (docling parity).
        assemble::drop_false_pictures(&mut regions, &page.cells, page.width, page.height);
        // A regular region fully inside a surviving table/index/picture is that
        // special's child (a cell / in-figure label), not a separate block —
        // remove it so it isn't emitted twice (docling parity).
        assemble::drop_contained_regulars(&mut regions);
        // A one-line paragraph in the bottom margin under a body-less heading
        // is that heading's text, not furniture (deliberate deviation).
        assemble::reclaim_heading_body_footers(&mut regions, page.width);
        // No text layer → recognise text from the page image via OCR.
        let ocred = page.cells.is_empty();
        // Lines the text detector found outside every layout region (#429).
        let mut det_cells: Vec<pdfium_backend::TextCell> = Vec::new();
        if ocred {
            // Region-scoped OCR recognizes each region's crop, so two regular
            // regions over the same ink would read it twice — a low-score
            // paragraph box over the high-score line boxes `greedy` keeps.
            // Collapse such groups to one region first; a digital
            // page resolves the same overlap through cell ownership in
            // `fit_regions_to_cells` and needs nothing here.
            assemble::merge_overlapping_regulars(&mut regions);
            // The text detector's lines (#429), fetched before the region
            // pass: with `det` line mode (#570, the default) they are the
            // recognizer's crops inside the regions too — RapidOCR's own line
            // source — not only the supplement outside them. Usually computed
            // already, alongside layout (see `detect_alongside`); a page that
            // reached OCR another way detects here. Degradation over failure:
            // a detector that loaded but cannot run (a damaged file, an
            // allocation failure) costs the page its detected lines, not its
            // conversion — the projection segmentation is complete on its own.
            let detected: Vec<ocr_det::DetBox> = if self.ocr_model()?.is_some() {
                let (img, _) = ocr_input(&mut ocr_view, &page.image, page.scale, ocr_scale);
                match self.pending_det.remove(&n) {
                    Some(result) => result,
                    None => match self.det_model() {
                        Some(det) => timing::timed("ocr.det", || det.detect(img)),
                        None => Ok(Vec::new()),
                    },
                }
                .unwrap_or_else(|e| {
                    docling_core::debug_log!(
                        "docling-pdf: page {}: text detection failed ({e}); keeping the \
                         region-scoped OCR only",
                        n + 1
                    );
                    Vec::new()
                })
            } else {
                Vec::new()
            };
            docling_core::debug_log!(
                "docling-pdf: page {}: text detector found {} line(s): {:?}",
                n + 1,
                detected.len(),
                detected
                    .iter()
                    .map(|d| (
                        d.l.round(),
                        d.t.round(),
                        d.r.round(),
                        d.b.round(),
                        (d.score * 100.0).round() / 100.0
                    ))
                    .collect::<Vec<_>>()
            );
            let det_lines =
                (ocr::det_lines() && !detected.is_empty()).then_some(detected.as_slice());
            // `None` = `skip_ocr` or a missing model (#244): the page keeps
            // its layout regions (and TableFormer structure below) with no
            // recognized text, instead of failing the conversion.
            if let Some(ocr) = self.ocr_model()? {
                let (img, scl) = ocr_input(&mut ocr_view, &page.image, page.scale, ocr_scale);
                let cells = timing::timed("ocr.page", || {
                    ocr.ocr_page_with(img, &regions, scl, det_lines)
                })
                .map_err(|e| PdfError::Ocr(format!("page {}: {e}", n + 1)))?;
                ocr_confs.extend(cells.iter().map(|(_, conf)| conf));
                page.cells = cells.into_iter().map(|(cell, _)| cell).collect();
                // Table interiors carry no words yet: region-scoped OCR skips
                // table labels, and a scanned page has no pdfium text layer — so
                // TableFormer's cell matcher got an empty word list and the table
                // dissolved (#173). Recognize the table regions' word crops
                // (mirroring the browser scanned path): `word_cells` feeds the
                // matcher, and the same cells join `cells` so the geometric
                // fallback and the table's region text see them too.
                if regions.iter().any(|r| assemble::is_table_like(r.label)) {
                    let (img, scl) = ocr_input(&mut ocr_view, &page.image, page.scale, ocr_scale);
                    let words = timing::timed("ocr.table_words", || {
                        ocr.ocr_table_words(img, &regions, scl, det_lines)
                    })
                    .map_err(|e| PdfError::Ocr(format!("page {}: {e}", n + 1)))?;
                    ocr_confs.extend(words.iter().map(|(_, conf)| conf));
                    let words: Vec<_> = words.into_iter().map(|(cell, _)| cell).collect();
                    page.cells.extend(words.iter().cloned());
                    page.word_cells = words;
                }
            }
            // docling's OCR engines detect text lines over the whole bitmap
            // and every line becomes a cell, so text the layout model gave no
            // region — a diagram's labels, a stamp, a margin note — still
            // reads out as orphan text. Region-scoped recognition above stays
            // the source inside layout regions; the detector (#429) adds only
            // the lines no recognized cell already covers, and the orphan pass
            // below places them (those inside a kept picture or table become
            // that special's silent children, as upstream).
            if self.ocr_model()?.is_some() {
                let (img, scl) = ocr_input(&mut ocr_view, &page.image, page.scale, ocr_scale);
                let uncovered = ocr_det::uncovered_lines(&detected, scl, &regions, &page.cells);
                docling_core::debug_log!(
                    "docling-pdf: page {}: {} of {} detected line(s) not covered by the region pass",
                    n + 1,
                    uncovered.len(),
                    detected.len()
                );
                if let (false, Some(ocr)) = (uncovered.is_empty(), self.ocr_model()?) {
                    let scored =
                        timing::timed("ocr.det_lines", || ocr.ocr_page(img, &uncovered, scl))
                            .map_err(|e| PdfError::Ocr(format!("page {}: {e}", n + 1)))?;
                    ocr_confs.extend(scored.iter().map(|(_, conf)| conf));
                    det_cells = scored.into_iter().map(|(cell, _)| cell).collect();
                    page.cells.extend(det_cells.iter().cloned());
                }
            }
        }
        // Region-scoped OCR skips `picture` interiors, and a digital page's
        // text layer cannot see into an embedded raster either — so a figure
        // that is really a text box (terms-and-conditions exported as an
        // image) lost its words on every page kind. Python docling OCRs the
        // bitmap-covered areas of *every* page — even digital ones — once they
        // exceed `bitmap_area_threshold` (5 % of the page); the browser paths
        // already do. Recognize the big text-less crops here too; the panel
        // demotion / orphan recovery below place the lines.
        let mut pic_cells: Vec<pdfium_backend::TextCell> = Vec::new();
        {
            let page_area = (page.width * page.height).max(1.0);
            let has_text = |r: &layout::Region| {
                page.cells.iter().any(|c| {
                    let ca = ((c.r - c.l) * (c.b - c.t)).max(1.0);
                    let ix = (r.r.min(c.r) - r.l.max(c.l)).max(0.0);
                    let iy = (r.b.min(c.b) - r.t.max(c.t)).max(0.0);
                    !c.text.trim().is_empty() && ix * iy / ca > 0.5
                })
            };
            // A captioned picture can never demote to a text panel (see
            // recover_text_panels), and on digital pages its speculative OCR
            // would be discarded anyway — don't pay for it.
            let captioned = |r: &layout::Region| {
                regions.iter().any(|c| {
                    c.label == "caption"
                        && c.r.min(r.r) - c.l.max(r.l) > 0.0
                        && ((c.t >= r.b && c.t - r.b <= 25.0) || (r.t >= c.b && r.t - c.b <= 25.0))
                })
            };
            let bare: Vec<layout::Region> = regions
                .iter()
                .filter(|r| {
                    r.label == "picture"
                        && (r.r - r.l) * (r.b - r.t) / page_area >= 0.05
                        && !has_text(r)
                        && (ocred || !captioned(r))
                })
                .map(|r| layout::Region {
                    label: "text",
                    ..r.clone()
                })
                .collect();
            // Speculative OCR (#244): with `skip_ocr` or no model, big bare
            // pictures simply stay pictures.
            if let (false, Some(ocr)) = (bare.is_empty(), self.ocr_model()?) {
                let (img, scl) = ocr_input(&mut ocr_view, &page.image, page.scale, ocr_scale);
                let scored = timing::timed("ocr.pictures", || ocr.ocr_page(img, &bare, scl))
                    .map_err(|e| PdfError::Ocr(format!("page {}: {e}", n + 1)))?;
                // Speculative in-picture OCR counts toward ocr_score only on
                // OCR'd pages, where the recognized lines actually join the
                // output; on a digital page they may be discarded below.
                if ocred {
                    ocr_confs.extend(scored.iter().map(|(_, conf)| conf));
                }
                pic_cells = scored.into_iter().map(|(cell, _)| cell).collect();
                page.cells.extend(pic_cells.iter().cloned());
            }
        }
        let cells_before_pic_ocr = page.cells.len() - pic_cells.len();
        // A "picture" that is really a colored text panel — dense, wide,
        // multi-line — reads out as paragraphs instead of shipping as pixels;
        // sparse in-picture text (a chart's labels) keeps the crop and stays
        // inside it as the picture's silent children (docling parity, #200).
        // `no_text_panels` (#173) opts out entirely for image-extraction
        // workflows.
        if !self.no_text_panels {
            assemble::recover_text_panels(&mut regions, &page.cells);
        }
        // On an OCR'd page, in-picture text that did NOT demote its picture
        // mostly stays silent, exactly as in docling: its postprocess step
        // "Remove regular clusters that are included in wrappers" walks
        // SPECIAL_TYPES — which includes PICTURE — so an orphan text cluster
        // >80 % contained in a kept picture becomes that picture's child and
        // never reaches the serializer. Only border-straddlers (≤80 %
        // containment) survive as text. Emitting *everything* here used to
        // splice a chart's OCR'd axis ticks into the body text right next to
        // the image chunk (#200) — so the orphan pass places the recognized
        // lines, the containment drop re-runs for the in-table ones, and
        // `assemble_page` nests the in-picture ones under their picture
        // (JSON-only children, `assemble::picture_parents`).
        if ocred && (!pic_cells.is_empty() || !det_cells.is_empty()) {
            // Pictures (and wrappers) no longer count as claimers (#165), so
            // the plain orphan pass places the recognized lines directly —
            // the detector's lines (#429) the same way.
            assemble::add_orphan_regions(&mut regions, &pic_cells);
            assemble::add_orphan_regions(&mut regions, &det_cells);
            assemble::drop_contained_regulars(&mut regions);
        } else if !ocred && !pic_cells.is_empty() {
            // Digital page, picture kept: its speculative OCR cells must not
            // linger in the text-cell set (they were appended at the tail).
            let kept: Vec<layout::Region> = regions
                .iter()
                .filter(|r| r.label == "picture")
                .cloned()
                .collect();
            let tail = page.cells.split_off(cells_before_pic_ocr);
            page.cells.extend(tail.into_iter().filter(|c| {
                !kept.iter().any(|r| {
                    let ca = ((c.r - c.l) * (c.b - c.t)).max(1.0);
                    let ix = (r.r.min(c.r) - r.l.max(c.l)).max(0.0);
                    let iy = (r.b.min(c.b) - r.t.max(c.t)).max(0.0);
                    ix * iy / ca > 0.5
                })
            }));
        }
        // A text-less *table* detected inside a picture on a digital page — a
        // screenshot of a table (2203's Figure 10) — has no text layer and no
        // scanned-path OCR to feed it, so its grid used to serialize empty and
        // the whole element vanished. docling OCRs bitmap-covered areas on
        // every page kind and its table cluster collects those cells; mirror
        // the scanned path for exactly these tables: recognize word crops and
        // feed them to the TableFormer matcher and the cell set.
        if !ocred {
            let has_text = |t: &layout::Region| {
                page.cells.iter().any(|c| {
                    let ca = ((c.r - c.l) * (c.b - c.t)).max(1.0);
                    let ix = (t.r.min(c.r) - t.l.max(c.l)).max(0.0);
                    let iy = (t.b.min(c.b) - t.t.max(c.t)).max(0.0);
                    !c.text.trim().is_empty() && ix * iy / ca > 0.5
                })
            };
            let in_picture = |t: &layout::Region| {
                regions.iter().any(|r| {
                    r.label == "picture" && {
                        let ta = ((t.r - t.l) * (t.b - t.t)).max(1.0);
                        let ix = (r.r.min(t.r) - r.l.max(t.l)).max(0.0);
                        let iy = (r.b.min(t.b) - r.t.max(t.t)).max(0.0);
                        ix * iy / ta > 0.5
                    }
                })
            };
            let pic_tables: Vec<layout::Region> = regions
                .iter()
                .filter(|t| assemble::is_table_like(t.label) && !has_text(t) && in_picture(t))
                .cloned()
                .collect();
            // Same degradation as above: without OCR the in-picture table
            // keeps its structure (TableFormer is geometry-driven) minus text.
            if let (false, Some(ocr)) = (pic_tables.is_empty(), self.ocr_model()?) {
                let (img, scl) = ocr_input(&mut ocr_view, &page.image, page.scale, ocr_scale);
                let words = timing::timed("ocr.table_words", || {
                    ocr.ocr_table_words(img, &pic_tables, scl, None)
                })
                .map_err(|e| PdfError::Ocr(format!("page {}: {e}", n + 1)))?;
                ocr_confs.extend(words.iter().map(|(_, conf)| conf));
                let words: Vec<_> = words.into_iter().map(|(cell, _)| cell).collect();
                page.cells.extend(words.iter().cloned());
                page.word_cells.extend(words);
            }
        }
        // The cells are final: fit every regular region to the cells it
        // claims and fold the orphans it now surrounds (#419), before
        // TableFormer and the reading order see the boxes.
        assemble::fit_regions_to_cells(&mut regions, &page.cells);
        Ok(Prepared {
            regions,
            ocr_confs,
            parse,
        })
    }

    /// The stages after TableFormer: enrichment, the page confidence report and
    /// assembly into typed nodes.
    fn complete_page(
        &mut self,
        n: usize,
        page: &mut PdfPage,
        prepared: Prepared,
        table_rows: Vec<Option<tf_core::TableGrid>>,
    ) -> Result<PageOut, PdfError> {
        let Prepared {
            regions,
            ocr_confs,
            parse,
        } = prepared;
        if env::flag("DOCLING_RS_DEBUG_REGIONS") {
            for (i, r) in regions.iter().enumerate() {
                eprintln!(
                    "DBG final {} {:.2} [{:.0},{:.0},{:.0},{:.0}] rows={:?}",
                    r.label,
                    r.score,
                    r.l,
                    r.t,
                    r.r,
                    r.b,
                    table_rows[i]
                        .as_ref()
                        .map(|t| (t.rows.len(), t.rows.first().map(|r| r.len())))
                );
            }
            eprintln!(
                "DBG cells={} words={}",
                page.cells.len(),
                page.word_cells.len()
            );
        }
        // Enrichment passes (opt-in): DocumentPictureClassifier over picture
        // regions, CodeFormulaV2 over code/formula regions. Same shared-slot
        // shape as TableFormer — one lazily-loaded instance per pipeline, only
        // ever locked when a page actually has a matching region.
        let mut enrich_out: Vec<Option<assemble::Enrichment>> = vec![None; regions.len()];
        if let Some(slot) = self.classifier.as_ref() {
            if regions.iter().any(|r| r.label == "picture") {
                timing::timed("picture_classifier", || {
                    let mut guard = slot.lock().unwrap();
                    if matches!(*guard, EnrichSlot::Unloaded) {
                        *guard = match enrich::PictureClassifier::load_with(intra_threads()) {
                            Some(m) => EnrichSlot::Ready(m),
                            None => EnrichSlot::Missing,
                        };
                    }
                    if let EnrichSlot::Ready(model) = &mut *guard {
                        for (i, r) in regions.iter().enumerate() {
                            if r.label != "picture" {
                                continue;
                            }
                            let Some(crop) = assemble::crop_region_scaled(
                                page,
                                [r.l, r.t, r.r, r.b],
                                enrich::CLASSIFIER_SCALE,
                            ) else {
                                continue;
                            };
                            match model.classify(&crop) {
                                Ok(classes) => {
                                    enrich_out[i] =
                                        Some(assemble::Enrichment::PictureClasses(classes));
                                }
                                Err(e) => eprintln!("docling-pdf: page {}: {e}", n + 1),
                            }
                        }
                    }
                });
            }
        }
        if let Some(slot) = self.code_formula.as_ref() {
            let wants = |label: &str| {
                (label == "code" && self.enrich.code) || (label == "formula" && self.enrich.formula)
            };
            if regions.iter().any(|r| wants(r.label)) {
                timing::timed("code_formula", || {
                    let mut guard = slot.lock().unwrap();
                    if matches!(*guard, EnrichSlot::Unloaded) {
                        *guard = match enrich::CodeFormula::load_with(intra_threads()) {
                            Some(m) => EnrichSlot::Ready(m),
                            None => EnrichSlot::Missing,
                        };
                    }
                    if let EnrichSlot::Ready(model) = &mut *guard {
                        for (i, r) in regions.iter().enumerate() {
                            if !wants(r.label) {
                                continue;
                            }
                            // docling crops the postprocessed cluster box — the
                            // union of the region's text cells, not the raw
                            // detector box — expanded by 18% per side, at
                            // ~120 dpi.
                            let [bl, bt, br, bb] = assemble::region_cell_bbox(r, &page.cells)
                                .unwrap_or([r.l, r.t, r.r, r.b]);
                            let (w, h) = (br - bl, bb - bt);
                            let ex = enrich::CODE_FORMULA_EXPANSION;
                            let bbox = [bl - w * ex, bt - h * ex, br + w * ex, bb + h * ex];
                            let Some(crop) = assemble::crop_region_scaled(
                                page,
                                bbox,
                                enrich::CODE_FORMULA_SCALE,
                            ) else {
                                continue;
                            };
                            let kind = if r.label == "code" {
                                enrich::CodeFormulaKind::Code
                            } else {
                                enrich::CodeFormulaKind::Formula
                            };
                            match model.predict(&crop, kind) {
                                Ok(text) => {
                                    enrich_out[i] = Some(match kind {
                                        enrich::CodeFormulaKind::Code => {
                                            let (code, language) =
                                                enrich::extract_code_language(&text);
                                            assemble::Enrichment::Code {
                                                language,
                                                text: code,
                                            }
                                        }
                                        enrich::CodeFormulaKind::Formula => {
                                            assemble::Enrichment::Formula { latex: text }
                                        }
                                    });
                                }
                                Err(e) => eprintln!("docling-pdf: page {}: {e}", n + 1),
                            }
                        }
                    }
                });
            }
        }
        // Score the final region set (docling assigns layout_score over the
        // postprocessed clusters — the page elements assemble_page emits,
        // which leave out the regulars nested in a picture as its children).
        let parents = assemble::picture_parents(&regions);
        let elements: Vec<layout::Region> = regions
            .iter()
            .zip(&parents)
            .filter(|(_, p)| p.is_none())
            .map(|(r, _)| r.clone())
            .collect();
        let conf = quality::page_confidence(parse, &elements, &ocr_confs);
        let (nodes, links) = timing::timed("assemble_page", || {
            assemble::assemble_page(page, regions, &table_rows, &enrich_out, self.images.scale)
        });
        Ok((nodes, links, conf, self.page_image(n, page)))
    }
}

#[cfg(feature = "ml")]
/// Per-worker ONNX intra-op threads. The layout model is memory-bandwidth bound,
/// so on a typical machine two threads per worker (sharing one in-cache copy of
/// the weights) extracts more throughput than one fat model or many single-thread
/// workers. `DOCLING_RS_PDF_INTRA` overrides for per-machine tuning.
fn pdf_intra() -> usize {
    if let Some(n) = env::parse::<usize>("DOCLING_RS_PDF_INTRA").filter(|&n| n > 0) {
        return n;
    }
    if intra_threads() >= 2 {
        2
    } else {
        1
    }
}

#[cfg(feature = "ml")]
/// How many page-workers to spin up for a multi-page PDF. `DOCLING_RS_PDF_WORKERS`
/// overrides; otherwise size the pool so `workers × intra ≈ cores`.
///
/// The pool scales with the machine (#324 follow-up testing): the old hard cap
/// of 4 left most of a many-core box idle — on a 16-core M4 Max, 10 workers
/// measured ~1.2× over the capped pool (10.0 → 8.5 s on a 130-page document,
/// byte-identical output). The ceiling of 16 is a memory bound, not a
/// performance one: each worker holds its own layout/OCR sessions (~0.4 GB),
/// so a worst-case pool stays under ~6.5 GB even on a ≥32-core host — and
/// docling-serve's per-request pools sit behind its `DOCLING_RS_MAX_MEMORY_MB`
/// admission control besides. Machines with 4 or fewer effective threads keep
/// the exact old sizing (`threads / intra`, min 1).
fn pdf_worker_count() -> usize {
    if let Some(n) = env::parse::<usize>("DOCLING_RS_PDF_WORKERS").filter(|&n| n > 0) {
        return n;
    }
    (intra_threads() / pdf_intra()).clamp(1, 16)
}

#[cfg(feature = "ml")]
/// Max pages a worker layout-detects with one batched inference call (issue
/// #73). Workers drain the work channel opportunistically up to this size —
/// whatever is already rendered gets batched, so batching never *waits* for
/// pages and adds no latency when rendering is the bottleneck.
///
/// Default: per-page (1) on the CPU provider, 4 when a CUDA-class GPU
/// provider is selected (#338) — CoreML stays per-page (#602,
/// [`docling_onnx::prefers_batching`]): only the pinned batch=1 graph is
/// static, and static partitions are all CoreML is allowed to take. The old
/// "4 on 8+ cores" CPU default was a hypothesis — that single-session
/// amortization pays off with a wider thread budget —
/// and every actual CPU measurement lands the other way: a 4-core x86 box
/// runs the 9-page 2206.01062 fixture in 8.5 s/conv at batch=1 vs 9.3 s at
/// batch=4 (re-measured for #338; the original 8.1 vs 9.3 agrees), and the
/// issue-#338 report measured batch=1 ~2× faster on a 16-core M4 Max at
/// every worker count — batching only adds cache pressure once workers
/// saturate the cores. On a GPU the per-call dispatch overhead is real and
/// batching amortizes it, so the GPU default stays. Output is bit-identical
/// at every batch size, so this is purely a throughput knob.
/// `DOCLING_RS_PDF_LAYOUT_BATCH` overrides either way; `1` = per-page.
pub(crate) fn pdf_layout_batch() -> usize {
    env::parse::<usize>("DOCLING_RS_PDF_LAYOUT_BATCH")
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            if docling_onnx::prefers_batching() {
                4
            } else {
                1
            }
        })
}

#[cfg(feature = "ml")]
/// Pages the serial path renders ahead of the one being processed
/// ([`Pipeline::render_ahead`], #682): one in flight on the worker, two
/// rendered and waiting — a bound on resident page bitmaps, like the
/// pool's channel.
const RENDER_AHEAD: usize = 2;

#[cfg(feature = "ml")]
/// Minimum page count before a PDF goes to the parallel worker pool; below
/// it the serial primary runs its model on every core. Four since #682 (was
/// six, from the days of the pool's uncached model load): the layout model's
/// intra-op scaling is weak (on a 4-core x86 box 4 threads run a page 2.35×
/// faster than one, 2 threads 1.6×), so two workers of two threads convert
/// pages faster than one worker of four converts them in turn — 4–5-page
/// digital documents measured 18% faster than the serial path with its
/// render-ahead, the pool's load included (the pool is sized to the page
/// count, [`Pipeline::ensure_pool_for`]). Not lower: the text detector a
/// scanned page needs scales well with threads, so a 2–3-page scan runs
/// 15–20% *slower* split across two-thread workers than on the four-thread
/// primary. `DOCLING_RS_PDF_PARALLEL_MIN` overrides.
fn pdf_parallel_min() -> usize {
    env::parse::<usize>("DOCLING_RS_PDF_PARALLEL_MIN")
        .filter(|&n| n > 0)
        .unwrap_or(4)
}

#[cfg(feature = "ml")]
/// A reusable PDF pipeline. The **primary** worker runs its models on every core,
/// so a single-page / small / image / METS input is converted at full intra-op
/// speed with no pool to load. A document with enough pages instead fans out
/// across a **pool** of narrower workers processed concurrently. Both load lazily
/// and are cached for reuse, so a one-shot conversion only pays for what it uses.
pub struct Pipeline {
    /// Full-intra worker for the serial path; loaded on first serial use.
    primary: Option<Worker>,
    /// Narrower workers (≈cores/`target_workers` threads each) for the parallel
    /// path; loaded on first multi-page use and cached.
    pool: Vec<Worker>,
    /// The single TableFormer instance every worker shares (see [`TfSlot`]).
    tables: SharedTables,
    /// The shared enrichment-model slots (same pattern as [`TfSlot`]).
    classifier: SharedClassifier,
    code_formula: SharedCodeFormula,
    /// Desired pool size for multi-page documents.
    target_workers: usize,
    /// Page count at/above which the parallel pool is worth its load cost.
    parallel_min: usize,
    /// Skip loading/running TableFormer; table regions fall back to geometric
    /// reconstruction. See [`Pipeline::no_table_former`].
    no_table_former: bool,
    /// Skip layout, OCR, and TableFormer entirely. See [`Pipeline::no_ocr`].
    no_ocr: bool,
    /// Keep layout + TableFormer, never OCR (#244). See [`Pipeline::skip_ocr`].
    skip_ocr: bool,
    /// OCR every page even when it carries a text layer. See
    /// [`Pipeline::force_full_page_ocr`].
    force_full_page_ocr: bool,
    /// Never demote text-panel pictures. See [`Pipeline::no_text_panels`].
    no_text_panels: bool,
    /// Opt-in enrichment passes. See [`Pipeline::enrichments`].
    enrich: EnrichmentOptions,
    /// 1-based inclusive page window to convert. See [`Pipeline::pages`].
    page_range: Option<(usize, usize)>,
    /// OCR recognition language. See [`Pipeline::ocr_lang`].
    ocr_lang: ocr::OcrLang,
    /// Which OCR engine runs (#460). See [`Pipeline::ocr_engine`].
    ocr_engine: ocr::OcrEngine,
    /// Tesseract's languages (#460). See [`Pipeline::tesseract_lang`].
    tesseract_lang: Option<String>,
    /// Which regions feed the OCR (#254). See [`Pipeline::ocr_mode`].
    ocr_mode: ocr::OcrMode,
    /// OCR render scale override in px/pt (#254). See [`Pipeline::ocr_scale`].
    ocr_scale: Option<f32>,
    /// Picture OCR scale (#666). See [`Pipeline::picture_ocr_scale`].
    picture_ocr_scale: Option<f32>,
    /// RapidOCR's line orientation handling (#666). See
    /// [`Pipeline::ocr_textline_orientation`].
    textline_orientation: bool,
    /// Picture-crop scale and page images (#519/#520). See
    /// [`Pipeline::images_scale`] / [`Pipeline::generate_page_images`].
    images: ImageOutput,
    /// Heading-level inference (#302). See [`Pipeline::heading_hierarchy`].
    heading_hierarchy: HeadingHierarchyOptions,
    /// Optional per-page progress hook `(done, selected_total)`, invoked after
    /// each page finishes on both the serial and parallel buffered paths. Set
    /// by the CLI batch mode for dot-progress; `None` costs nothing.
    progress: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>,
    /// Per-document wall-clock budget (docling's `document_timeout`, #497).
    /// See [`Pipeline::document_timeout`].
    document_timeout: Option<std::time::Duration>,
}

#[cfg(feature = "ml")]
impl Pipeline {
    /// Construct the pipeline. Models load lazily on first use (full-intra primary
    /// for serial inputs, the helper pool for multi-page PDFs), so nothing is
    /// loaded that a given document doesn't need.
    pub fn new() -> Result<Self, PdfError> {
        Ok(Self {
            primary: None,
            pool: Vec::new(),
            tables: Arc::new(Mutex::new(TfSlot::Unloaded)),
            classifier: Arc::new(Mutex::new(EnrichSlot::Unloaded)),
            code_formula: Arc::new(Mutex::new(EnrichSlot::Unloaded)),
            target_workers: pdf_worker_count(),
            parallel_min: pdf_parallel_min(),
            no_table_former: false,
            no_ocr: false,
            skip_ocr: false,
            force_full_page_ocr: false,
            no_text_panels: false,
            enrich: EnrichmentOptions::default(),
            page_range: None,
            ocr_lang: ocr::OcrLang::from_env(),
            ocr_engine: ocr::OcrEngine::from_env(),
            tesseract_lang: None,
            ocr_mode: ocr::OcrMode::from_env(),
            ocr_scale: ocr::scale_from_env(),
            picture_ocr_scale: ocr::picture_scale_from_env(),
            textline_orientation: ocr::textline_orientation_from_env(),
            images: ImageOutput::default(),
            heading_hierarchy: HeadingHierarchyOptions::default(),
            progress: None,
            document_timeout: None,
        })
    }

    /// A wall-clock budget for each document (docling's
    /// `PipelineOptions.document_timeout`, #497; `None` = unlimited, the
    /// default). The budget starts when a conversion starts and is checked
    /// cooperatively between pages: once it is spent, no further page is
    /// rendered or processed, the pages already finished are assembled into
    /// the document, and the outcome says so (`Completion::TimedOut`, which
    /// the converter reports as `PartialSuccess` with a timeout error, the
    /// way docling does). A page in flight finishes — the check costs
    /// nothing inside a page, and a single-page document (an image) is never
    /// cut. For a long-lived pipeline use
    /// [`set_document_timeout`](Self::set_document_timeout) before each
    /// conversion.
    pub fn document_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.document_timeout = timeout;
        self
    }

    /// In-place variant of [`document_timeout`](Self::document_timeout) for a
    /// warm pipeline — set it before every conversion so no request inherits
    /// a previous one's budget.
    pub fn set_document_timeout(&mut self, timeout: Option<std::time::Duration>) {
        self.document_timeout = timeout;
    }

    /// The deadline of a conversion starting now, from the configured budget.
    fn deadline(&self) -> Option<std::time::Instant> {
        self.document_timeout.map(|t| std::time::Instant::now() + t)
    }

    /// The [`Completion`] of a walk that processed `done` of `selected` pages.
    fn completion(&self, timed_out: bool, done: usize, selected: usize) -> Completion {
        match (timed_out, self.document_timeout) {
            (true, Some(budget)) => Completion::TimedOut {
                pages_done: done,
                pages_selected: selected,
                budget,
            },
            _ => Completion::Complete,
        }
    }

    /// Infer section-header levels after assembly (#302, docling's
    /// `HeadingHierarchyModel`): PDF bookmarks > legal/outline numbering >
    /// font style, off by default — see [`HeadingHierarchyOptions`]. Pure
    /// post-processing configuration; for a warm pipeline use
    /// [`set_heading_hierarchy`](Self::set_heading_hierarchy).
    pub fn heading_hierarchy(mut self, opts: HeadingHierarchyOptions) -> Self {
        self.heading_hierarchy = opts;
        self
    }

    /// In-place variant of [`heading_hierarchy`](Self::heading_hierarchy) for
    /// a long-lived pipeline (docling-serve's warm instance) — like
    /// [`set_pages`](Self::set_pages), set it before every conversion so no
    /// request inherits a previous one's choice.
    pub fn set_heading_hierarchy(&mut self, opts: HeadingHierarchyOptions) {
        self.heading_hierarchy = opts;
    }

    /// Run the enabled heading-hierarchy stage (#302) on an assembled
    /// document: gather the outline (bookmarks) and the per-page glyph styles
    /// on demand, then assign levels in place. `bytes` is `None` on paths
    /// with no PDF behind them (standalone images, METS) — those degrade to
    /// the numbering signal, exactly like docling without parsed pages.
    fn apply_heading_hierarchy(&self, nodes: &mut [Node], bytes: Option<&[u8]>) {
        let opts = &self.heading_hierarchy;
        if !opts.enabled {
            return;
        }
        let outline = match bytes {
            Some(bytes) if opts.use_bookmarks => outline::extract_outline(bytes),
            _ => Vec::new(),
        };
        let styles = match bytes {
            Some(bytes) if opts.use_style => {
                let pages = heading_hierarchy::heading_pages(nodes);
                textparse::glyph_styles(bytes, &pages)
            }
            _ => Default::default(),
        };
        heading_hierarchy::apply(nodes, &outline, &styles, opts);
    }

    /// Install (or clear) the per-page progress hook: called with
    /// `(pages_done, pages_selected)` after each page completes during
    /// [`convert`](Self::convert). Shared with the parallel workers, so the
    /// callback must be cheap and thread-safe.
    pub fn set_progress(&mut self, cb: Option<Arc<dyn Fn(usize, usize) + Send + Sync>>) {
        self.progress = cb;
    }

    /// Convert only pages `first..=last` (**1-based**, like the page numbers a
    /// PDF viewer shows — issue #80's `--pages A-B`). Out-of-range pages are
    /// skipped before rasterization, so the cost is proportional to the window,
    /// not the document. `last` past the end of the document clamps; a window
    /// that selects no pages at all (`first` beyond the last page) is an error
    /// at convert time. `None` (the default) converts everything.
    pub fn pages(mut self, range: Option<(usize, usize)>) -> Self {
        self.page_range = range;
        self
    }

    /// In-place variant of [`pages`](Self::pages) for a long-lived pipeline
    /// (e.g. docling-serve's warm instance) that applies a per-request window
    /// without rebuilding — unlike the model switches, the window is pure
    /// configuration. Set it before every conversion; it stays until changed.
    pub fn set_pages(&mut self, range: Option<(usize, usize)>) {
        self.page_range = range;
    }

    /// OCR recognition language (see [`OcrLang`]): English by default, `ch`
    /// for the multilingual docling-conformance model. `None` keeps the
    /// process default (`DOCLING_RS_OCR_LANG`, else English). Set before the
    /// first conversion; for a warm pipeline use
    /// [`set_ocr_lang`](Self::set_ocr_lang).
    pub fn ocr_lang(mut self, lang: Option<ocr::OcrLang>) -> Self {
        self.set_ocr_lang(lang);
        self
    }

    /// In-place variant of [`ocr_lang`](Self::ocr_lang) for a long-lived
    /// pipeline (docling-serve's warm instance). Unlike the page window this
    /// is a *model* switch: any worker whose cached recognition model was
    /// loaded for a different language drops it, to be lazily reloaded on the
    /// next OCR-needing page (cheap — the rec models are ~10 MB).
    pub fn set_ocr_lang(&mut self, lang: Option<ocr::OcrLang>) {
        let lang = lang.unwrap_or_else(ocr::OcrLang::from_env);
        self.ocr_lang = lang;
        for worker in self.primary.iter_mut().chain(self.pool.iter_mut()) {
            if worker.ocr_lang != lang {
                worker.ocr_lang = lang;
                worker.ocr = OcrSlot::Unloaded;
            }
        }
    }

    /// Which OCR engine recognizes text (#460, see [`OcrEngine`]): the
    /// built-in PP-OCRv3 recognizer by default, or the system `tesseract`
    /// binary (docling's `TesseractCliOcrOptions`) — same layout-region
    /// crops, same cells; Tesseract does its own line/word segmentation and
    /// reads orientation from its OSD. `None` keeps the process default
    /// (`DOCLING_RS_OCR_ENGINE`, else PP-OCR). Set before the first
    /// conversion; for a warm pipeline use
    /// [`set_ocr_engine`](Self::set_ocr_engine).
    pub fn ocr_engine(mut self, engine: Option<ocr::OcrEngine>) -> Self {
        self.set_ocr_engine(engine);
        self
    }

    /// In-place variant of [`ocr_engine`](Self::ocr_engine): a model switch
    /// like [`set_ocr_lang`](Self::set_ocr_lang) — workers holding the other
    /// engine drop it, to be lazily reloaded.
    pub fn set_ocr_engine(&mut self, engine: Option<ocr::OcrEngine>) {
        let engine = engine.unwrap_or_else(ocr::OcrEngine::from_env);
        self.ocr_engine = engine;
        for worker in self.primary.iter_mut().chain(self.pool.iter_mut()) {
            if worker.ocr_engine != engine {
                worker.ocr_engine = engine;
                worker.ocr = OcrSlot::Unloaded;
            }
        }
    }

    /// Tesseract's languages (#460): the `-l` argument, tessdata stems joined
    /// with `+` — build it from an `ocr_lang` value with
    /// [`tesseract_lang_arg`], which also maps BCP-47 tags and the PP-OCR
    /// codes. `None` runs Tesseract's default (`eng`). Ignored under the
    /// PP-OCR engine, whose language is [`ocr_lang`](Self::ocr_lang). For a
    /// warm pipeline use [`set_tesseract_lang`](Self::set_tesseract_lang).
    pub fn tesseract_lang(mut self, lang: Option<String>) -> Self {
        self.set_tesseract_lang(lang);
        self
    }

    /// In-place variant of [`tesseract_lang`](Self::tesseract_lang): a model
    /// switch like [`set_ocr_lang`](Self::set_ocr_lang) — a worker whose
    /// Tesseract was probed for other languages drops it.
    pub fn set_tesseract_lang(&mut self, lang: Option<String>) {
        self.tesseract_lang = lang.clone();
        for worker in self.primary.iter_mut().chain(self.pool.iter_mut()) {
            if worker.tesseract_lang != lang {
                worker.tesseract_lang = lang.clone();
                if worker.ocr_engine == ocr::OcrEngine::Tesseract {
                    worker.ocr = OcrSlot::Unloaded;
                }
            }
        }
    }

    /// Resolve the configured 1-based window against a page count into the
    /// 0-based inclusive form the backend walks, validating it selects at
    /// least one existing page.
    fn resolve_range(&self, total: usize) -> Result<Option<(usize, usize)>, PdfError> {
        let Some((first, last)) = self.page_range else {
            return Ok(None);
        };
        if first == 0 || last < first {
            return Err(PdfError::Pdfium(format!(
                "invalid page range {first}-{last} (pages are 1-based, first <= last)"
            )));
        }
        if first > total {
            return Err(PdfError::Pdfium(format!(
                "page range {first}-{last} is outside the document ({total} page(s))"
            )));
        }
        Ok(Some((first - 1, last.min(total) - 1)))
    }

    /// Enable the opt-in enrichment passes (docling's
    /// `do_picture_classification` / `do_code_enrichment` /
    /// `do_formula_enrichment`). Each enabled pass lazily loads its model on
    /// the first matching region; a missing model warns once and is skipped.
    /// Set before the first conversion (no effect on already-loaded workers).
    pub fn enrichments(mut self, opts: EnrichmentOptions) -> Self {
        self.enrich = opts;
        self
    }

    /// Skip loading and running the TableFormer table-structure model. Table
    /// regions still get emitted, but reconstructed geometrically from cell
    /// positions instead of via the ONNX model's predicted structure — faster
    /// (no model load, no per-table inference) at the cost of table fidelity.
    /// No effect if a worker is already loaded; set this before the first
    /// conversion.
    pub fn no_table_former(mut self, disable: bool) -> Self {
        self.no_table_former = disable;
        self
    }

    /// Keep every detected `picture` region as a picture. By default an
    /// *uncaptioned* picture that reads like a dense, uniform text panel (a
    /// terms-and-conditions box exported as an image) is demoted into
    /// paragraphs (#157); a chart the layout mislabels can still trip that
    /// heuristic on scanned pages, and image-extraction workflows may simply
    /// want every crop — this flag disables the demotion entirely (#173).
    /// No effect on already-loaded workers; set before the first conversion.
    pub fn no_text_panels(mut self, disable: bool) -> Self {
        self.no_text_panels = disable;
        self
    }

    /// Skip layout detection, OCR, and TableFormer entirely — no model load, no
    /// inference of any kind. The PDF's embedded text cells are grouped by line
    /// and emitted as plain paragraphs in reading order: no headings, lists,
    /// tables, code blocks, or pictures, since that structure comes from the
    /// layout model. The fastest possible PDF path, but pages with no embedded
    /// text layer (scanned/image-only PDFs) yield no text at all — convert those
    /// without this flag. Implies `no_table_former`. No effect if a worker is
    /// already loaded; set this before the first conversion.
    pub fn no_ocr(mut self, disable: bool) -> Self {
        self.no_ocr = disable;
        self
    }

    /// Never run OCR, but keep layout detection and TableFormer — docling's
    /// independent `do_ocr=False` (#244), the counterpart of
    /// [`no_table_former`](Self::no_table_former). Unlike
    /// [`no_ocr`](Self::no_ocr) (which skips the whole ML stack), structured
    /// output — headings, tables, pictures, reading order — is preserved;
    /// only text that exists solely as pixels is lost: scanned pages come
    /// back with their regions empty, and the speculative OCR of large
    /// embedded images never runs. The OCR model is never loaded. Ignored
    /// when `no_ocr` is set (there is no OCR to skip);
    /// takes precedence over [`force_full_page_ocr`](Self::force_full_page_ocr),
    /// mirroring docling where forcing is a sub-option of `do_ocr`.
    pub fn skip_ocr(mut self, disable: bool) -> Self {
        self.skip_ocr = disable;
        self
    }

    /// OCR every page from its rendered image even when the page carries an
    /// embedded text layer — docling's `force_full_page_ocr`. The escape hatch
    /// for text layers that exist but lie: broken encodings, subset fonts with
    /// garbage mappings, a scanned form with a few typed-in fields. Ignored
    /// when [`no_ocr`](Self::no_ocr) is set, mirroring docling (there
    /// `force_full_page_ocr` is a sub-option of `do_ocr`).
    pub fn force_full_page_ocr(mut self, force: bool) -> Self {
        self.force_full_page_ocr = force;
        self
    }

    /// Which document regions feed the OCR — docling's `OcrMode` (#254). The
    /// default (`default` = `pdf_aware_layout_regions`) is the standard
    /// text-layer-aware behavior; `full_page`/`layout_regions` discard the
    /// text layer like [`force_full_page_ocr`](Self::force_full_page_ocr)
    /// (see [`ocr::OcrMode`] for why both map onto it). Whichever of the flag
    /// and the mode demands forcing wins, mirroring docling's
    /// `force_full_page_ocr` → `mode=full_page` bridge. `None` keeps the
    /// process default (`DOCLING_RS_OCR_MODE`, else `default`).
    pub fn ocr_mode(mut self, mode: Option<ocr::OcrMode>) -> Self {
        self.ocr_mode = mode.unwrap_or_else(ocr::OcrMode::from_env);
        self
    }

    /// In-place variants of [`force_full_page_ocr`](Self::force_full_page_ocr),
    /// [`ocr_mode`](Self::ocr_mode) and [`ocr_scale`](Self::ocr_scale) for a
    /// long-lived pipeline (docling-serve's warm instance): all three are pure
    /// per-worker configuration — no model reloads — so they apply per request
    /// like [`set_pages`](Self::set_pages). Set them before every conversion so
    /// no request inherits a previous one's choice.
    pub fn set_force_full_page_ocr(&mut self, force: bool) {
        self.force_full_page_ocr = force;
        self.sync_ocr_config();
    }

    /// See [`set_force_full_page_ocr`](Self::set_force_full_page_ocr).
    pub fn set_ocr_mode(&mut self, mode: Option<ocr::OcrMode>) {
        self.ocr_mode = mode.unwrap_or_else(ocr::OcrMode::from_env);
        self.sync_ocr_config();
    }

    /// See [`set_force_full_page_ocr`](Self::set_force_full_page_ocr).
    pub fn set_ocr_scale(&mut self, scale: Option<f32>) {
        self.ocr_scale = scale
            .filter(|s| s.is_finite() && *s > 0.0)
            .or_else(ocr::scale_from_env);
        self.sync_ocr_config();
    }

    /// Resolution of the picture-OCR enrichment (#645) in pixels per
    /// picture pixel, before RapidOCR's 2000 px longer-side cap (#666) —
    /// RapidOCR reads a picture at its native pixels (`1.0`) and only
    /// shrinks what exceeds 2000 px. `None` (default:
    /// `DOCLING_RS_PICTURE_OCR_SCALE`, else unset) keeps the image-input
    /// rule pictures shared with scanned images: [`ocr_scale`](Self::ocr_scale)
    /// when set, else 3 px/pt shrunk to the 2000 px cap — which upscales a
    /// 720 × 180 screenshot to 2000 × 500. Pages and image inputs are
    /// unaffected. Non-positive values are ignored.
    pub fn picture_ocr_scale(mut self, scale: Option<f32>) -> Self {
        self.set_picture_ocr_scale(scale);
        self
    }

    /// In-place [`picture_ocr_scale`](Self::picture_ocr_scale) for a warm
    /// pipeline.
    pub fn set_picture_ocr_scale(&mut self, scale: Option<f32>) {
        self.picture_ocr_scale = scale
            .filter(|s| s.is_finite() && *s > 0.0)
            .or_else(ocr::picture_scale_from_env);
    }

    /// RapidOCR's handling of text-line orientation (#666), off by default
    /// (`DOCLING_RS_OCR_TEXTLINE_ORIENTATION` sets the process default): each
    /// detected line is cropped along the detector's rotated rectangle (a
    /// perspective warp, `get_rotate_crop_image`) instead of its axis-aligned
    /// box, a crop at least 1.5× taller than wide is turned 90°
    /// counter-clockwise, and the PP-LCNet text-line orientation classifier
    /// (`.models/ocr_textline_ori.onnx`, `DOCLING_OCR_TEXTLINE_ORI_ONNX`)
    /// turns lines it reads as upside down (`180` above 0.9). Vertical axis
    /// titles, rotated headers, skewed and upside-down lines then read as
    /// text. Applies to every PP-OCR line read off the detector — pages and
    /// pictures alike — but not to table words or the projection fallback;
    /// the reported boxes stay axis-aligned. A missing classifier warns once
    /// and keeps the crop and the 90° turn. Tesseract orients on its own.
    pub fn ocr_textline_orientation(mut self, enable: bool) -> Self {
        self.set_ocr_textline_orientation(Some(enable));
        self
    }

    /// In-place [`ocr_textline_orientation`](Self::ocr_textline_orientation)
    /// for a warm pipeline (serve, the bindings' `Pipeline`), applied to
    /// loaded workers too; `None` restores the process default
    /// (`DOCLING_RS_OCR_TEXTLINE_ORIENTATION`), like
    /// [`set_ocr_scale`](Self::set_ocr_scale).
    pub fn set_ocr_textline_orientation(&mut self, enable: Option<bool>) {
        self.textline_orientation = enable.unwrap_or_else(ocr::textline_orientation_from_env);
        self.sync_ocr_config();
    }

    /// Whether page extraction should decode the text layer at all. Forced
    /// full-page OCR (the flag or `ocr_mode=full_page|layout_regions`) clears
    /// every extracted cell unread, so the decode is skipped outright —
    /// docling#4061's `skip_cell_extraction` (2.122). `no_ocr` wins over the
    /// forcing, as everywhere else: its fast path *is* the text layer.
    fn extract_text_layer(&self) -> bool {
        self.no_ocr || !(self.force_full_page_ocr || self.ocr_mode.forces_full_page())
    }

    /// Push the current OCR forcing/scale choice onto already-loaded workers
    /// (new workers read it at [`Worker::load`]).
    fn sync_ocr_config(&mut self) {
        let force = self.force_full_page_ocr || self.ocr_mode.forces_full_page();
        let scale = self.ocr_scale;
        let orient = self.textline_orientation;
        for worker in self.primary.iter_mut().chain(self.pool.iter_mut()) {
            worker.force_full_page_ocr = force;
            worker.ocr_scale = scale;
            worker.set_textline_orientation(orient);
        }
    }

    /// Pixels per PDF point for picture crops and page images — docling's
    /// `images_scale` (#519/#520). `None` (the default) delivers crops at the
    /// pipeline's own page render, 2.0 px/pt (144 dpi), untouched. Another
    /// value resamples that render (CatmullRom, docling's PIL BICUBIC), so
    /// values above 2.0 upsample rather than re-render. Each picture's
    /// `dpi` (docling-core's `ImageRef.dpi`) is 72·scale, so consumers mapping
    /// pixels back to points stay exact. Layout, OCR and TableFormer inputs
    /// are unaffected. Non-positive values are ignored.
    pub fn images_scale(mut self, scale: Option<f32>) -> Self {
        self.set_images(ImageOutput {
            scale,
            ..self.images
        });
        self
    }

    /// Keep each page's render as the document's page image — docling's
    /// `generate_page_images` (#520): the JSON export writes it as the page's
    /// `image` (docling-core's `PageItem.image`, at [`Self::images_scale`],
    /// `dpi` = 72·scale), which is what docling-core's
    /// `TableItem.get_image` / `FormulaItem.get_image` crop from. Off by
    /// default: a page image is a full-page PNG per page held in memory.
    /// Pages converted with `no_ocr` (text layer only) are never rendered and
    /// get none; streaming conversions carry no page map and drop them.
    pub fn generate_page_images(mut self, enabled: bool) -> Self {
        self.set_images(ImageOutput {
            page_images: enabled,
            ..self.images
        });
        self
    }

    /// Both image outputs at once, also on already-loaded workers — the
    /// per-request form a warm pipeline (serve, the Node `Pipeline`) uses,
    /// like [`set_ocr_scale`](Self::set_ocr_scale). A non-finite or
    /// non-positive scale reads as unset.
    pub fn set_images(&mut self, images: ImageOutput) {
        self.images = ImageOutput {
            scale: images.scale.filter(|s| s.is_finite() && *s > 0.0),
            ..images
        };
        let images = self.images;
        for worker in self.primary.iter_mut().chain(self.pool.iter_mut()) {
            worker.images = images;
        }
    }

    /// OCR render scale in pixels per PDF point — docling's `OcrOptions.scale`
    /// (#254, upstream docling#3877; their default 3 = 216 dpi). `None`
    /// (default: `DOCLING_RS_OCR_SCALE`, else unset) feeds the recognizer the
    /// pipeline's own page render (2.0 px/pt = 144 dpi) for a rendered page,
    /// and for a standalone image input docling's own resolution (#570): 3
    /// px/pt shrunk so the longer side stays within RapidOCR's 2000 px — what
    /// its models see after RapidOCR's `max_side_len` pass (a 754 × 1000 scan
    /// reads at 2.0). A set value resamples the render / image for the OCR
    /// input only — layout and TableFormer keep their pinned-resolution
    /// pixels, so the conformance baseline never moves. Lower it when the
    /// source raster is already high-resolution and upscaling degrades
    /// recognition; raise it toward docling's 216 dpi for parity experiments.
    /// Non-positive values are ignored.
    pub fn ocr_scale(mut self, scale: Option<f32>) -> Self {
        self.ocr_scale = scale
            .filter(|s| s.is_finite() && *s > 0.0)
            .or_else(ocr::scale_from_env);
        self
    }

    /// The shared TableFormer slot handed to each worker, or `None` when the
    /// pipeline options skip TableFormer entirely.
    fn tables_slot(&self) -> Option<SharedTables> {
        if self.no_table_former || self.no_ocr {
            None
        } else {
            Some(Arc::clone(&self.tables))
        }
    }

    /// The shared enrichment slots for a worker (`None` per model unless its
    /// flag is on; `no_ocr` skips layout, so there are no regions to enrich).
    fn enrich_slots(&self) -> (Option<SharedClassifier>, Option<SharedCodeFormula>) {
        if self.no_ocr || !self.enrich.any() {
            return (None, None);
        }
        (
            self.enrich
                .picture_classification
                .then(|| Arc::clone(&self.classifier)),
            (self.enrich.code || self.enrich.formula).then(|| Arc::clone(&self.code_formula)),
        )
    }

    /// Eagerly load the models (the full-intra serial worker: layout + OCR, and
    /// the shared TableFormer unless disabled) so the first conversion doesn't pay
    /// the load cost. Idempotent; respects `no_ocr` / `no_table_former` (with
    /// `no_ocr` there is nothing to load). The docling.rs analogue of docling's
    /// `DocumentConverter.initialize_pipeline`.
    pub fn warm_up(&mut self) -> Result<(), PdfError> {
        self.primary()?;
        // The shared TableFormer loads on the first table otherwise — a
        // document with one would still pay for it (#548).
        if let Some(tables) = self.tables_slot() {
            tables.lock().unwrap_or_else(|p| p.into_inner()).load();
        }
        Ok(())
    }

    /// [`warm_up`](Self::warm_up) plus the parallel page-worker pool, so the
    /// first *multi-page* conversion is no slower than later ones either: a
    /// document of `DOCLING_RS_PDF_PARALLEL_MIN` pages (6) or more fans out across the pool,
    /// whose workers each load their own layout/OCR sessions on first use
    /// (#548 — docling-serve's `--warmup` left a 9-page PDF paying that load).
    /// Costs the pool's memory up front (~0.4 GB a worker); with a pool of one
    /// worker it is exactly `warm_up`.
    pub fn warm_up_all(&mut self) -> Result<(), PdfError> {
        self.warm_up()?;
        if self.target_workers >= 2 {
            self.ensure_pool()?;
        }
        Ok(())
    }

    /// The full-intra serial worker, loaded on first use.
    fn primary(&mut self) -> Result<&mut Worker, PdfError> {
        if self.primary.is_none() {
            self.primary = Some(Worker::load(
                intra_threads(),
                self.tables_slot(),
                self.enrich_slots(),
                self.enrich,
                self.no_ocr,
                self.skip_ocr,
                // The mode-shaped spelling (#254) and the flag are one engine
                // truth: whichever demands forcing wins, mirroring docling's
                // `force_full_page_ocr` → `mode=full_page` bridge.
                self.force_full_page_ocr || self.ocr_mode.forces_full_page(),
                self.no_text_panels,
                self.ocr_lang,
                self.ocr_engine,
                self.tesseract_lang.clone(),
                self.ocr_scale,
                self.textline_orientation,
                self.images,
            )?);
        }
        Ok(self.primary.as_mut().unwrap())
    }

    /// Convert a PDF (bytes) to a [`DoclingDocument`]. A document with fewer than
    /// `parallel_min` pages (or a pool size of 1) streams through the full-intra
    /// primary; a larger one renders on this thread (pdfium is not thread-safe) and
    /// fans the pages out across the worker pool, reassembled in page order so the
    /// output is byte-identical to the serial path.
    pub fn convert(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
    ) -> Result<DoclingDocument, PdfError> {
        self.convert_outcome(bytes, password, name)
            .map(|c| c.document)
    }

    /// [`convert`](Self::convert) that also says how the conversion ended —
    /// whether the [`document_timeout`](Self::document_timeout) cut it short
    /// and how many pages made it (`Completion`).
    pub fn convert_outcome(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
    ) -> Result<Converted, PdfError> {
        let deadline = self.deadline();
        let pages = pdfium_backend::page_count(bytes, password)?;
        let range = self.resolve_range(pages)?;
        // Serial vs parallel is decided by the pages actually converted: a
        // 3-page window over a 500-page PDF should not pay the pool load.
        let selected = range.map_or(pages, |(a, b)| b - a + 1);
        let (document, done, timed_out) =
            if self.target_workers >= 2 && selected >= self.parallel_min {
                self.convert_parallel(bytes, password, name, range, selected, deadline)?
            } else {
                self.convert_serial(bytes, password, name, range, selected, deadline)?
            };
        timing::report();
        Ok(Converted {
            document,
            completion: self.completion(timed_out, done, selected),
        })
    }

    /// Stream pages one at a time through the primary worker — render → process →
    /// drop — so the document holds ~one page bitmap (~5 MB) at a time.
    fn convert_serial(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
        range: Option<(usize, usize)>,
        selected: usize,
        deadline: Option<std::time::Instant>,
    ) -> Result<(DoclingDocument, usize, bool), PdfError> {
        let mut doc = DoclingDocument::new(name);
        let mut confs = std::collections::BTreeMap::new();
        let render_image = !self.no_ocr;
        let extract_text = self.extract_text_layer();
        let progress = self.progress.clone();
        let mut done = 0usize;
        let worker = self.primary()?;
        let timed_out = Self::render_ahead(
            bytes,
            password,
            render_image,
            extract_text,
            range,
            deadline,
            |n, mut page| {
                let (mut nodes, links, conf, page_image) = worker.process(n, &mut page)?;
                assemble::stamp_page_no(&mut nodes, n + 1);
                if let Some(img) = page_image {
                    doc.page_images.insert(n + 1, img);
                }
                doc.nodes.extend(nodes);
                doc.links.extend(links);
                confs.insert(n + 1, conf);
                done += 1;
                if let Some(cb) = &progress {
                    cb(done, selected);
                }
                Ok(())
            },
        )?;
        assemble::merge_continuations(&mut doc.nodes);
        self.apply_heading_hierarchy(&mut doc.nodes, Some(bytes));
        doc.confidence = Some(docling_core::ConfidenceReport::from_pages(confs));
        Ok((doc, done, timed_out))
    }

    /// The serial path's schedule (#682): pages render on a scoped thread
    /// and queue through a [`RENDER_AHEAD`]-page channel while the primary
    /// worker processes the previous one on the calling thread, so a page's
    /// two rasters (about a third of a digital page's cost) hide behind
    /// layout and TableFormer instead of preceding them — the render-ahead
    /// the worker pool always had, for documents too small for the pool.
    /// One consumer, pages in order: the output is byte-identical to the
    /// sequential walk. `consume` runs on the calling thread (a streaming
    /// `emit` need not be `Send`). A spent budget (#497) ends the walk at
    /// the renderer and the consumer alike; a consumer error closes the
    /// channel, which stops the renderer, and is the one reported.
    fn render_ahead<F>(
        bytes: &[u8],
        password: Option<&str>,
        render_image: bool,
        extract_text: bool,
        range: Option<(usize, usize)>,
        deadline: Option<std::time::Instant>,
        mut consume: F,
    ) -> Result<bool, PdfError>
    where
        F: FnMut(usize, PdfPage) -> Result<(), PdfError>,
    {
        let (tx, rx) = sync_channel::<(usize, PdfPage)>(RENDER_AHEAD);
        std::thread::scope(|s| {
            let render = s.spawn(move || {
                pdfium_backend::for_each_page(
                    bytes,
                    password,
                    render_image,
                    extract_text,
                    range,
                    |n, _total, page| {
                        if expired(deadline) {
                            return Err(timeout_sentinel());
                        }
                        tx.send((n, page))
                            .map_err(|_| PdfError::Pdfium("page channel closed".into()))
                    },
                )
            });
            let mut timed_out = false;
            let mut failed = None;
            for (n, page) in rx.iter() {
                if expired(deadline) {
                    timed_out = true;
                    break;
                }
                if let Err(e) = consume(n, page) {
                    failed = Some(e);
                    break;
                }
            }
            // Closing the channel ends a walk still rendering.
            drop(rx);
            let walk = render
                .join()
                .map_err(|_| PdfError::Pdfium("page render thread panicked".into()))?;
            if let Some(e) = failed {
                return Err(e);
            }
            match walk {
                Ok(()) => Ok(timed_out),
                Err(PdfError::Timeout(_)) => Ok(true),
                Err(_) if timed_out => Ok(true),
                Err(e) => Err(e),
            }
        })
    }

    /// Render pages serially on this thread (pdfium) and process them in parallel
    /// across the worker pool. A bounded channel applies backpressure so only a
    /// handful of page bitmaps are resident at once; results carry their page
    /// index and are reassembled in order, so the output is byte-identical to the
    /// serial path.
    fn convert_parallel(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
        range: Option<(usize, usize)>,
        selected: usize,
        deadline: Option<std::time::Instant>,
    ) -> Result<(DoclingDocument, usize, bool), PdfError> {
        self.ensure_pool_for(selected)?;
        let progress = self.progress.clone();
        let pages_done = std::sync::atomic::AtomicUsize::new(0);
        let timed_out = std::sync::atomic::AtomicBool::new(false);
        let n_workers = self.pool.len();
        let render_image = !self.no_ocr;
        let extract_text = self.extract_text_layer();
        let layout_batch = pdf_layout_batch();
        // Bound sized so every worker can accumulate a full layout batch while
        // rendering stays ahead (and never below the pre-#73 render-ahead of
        // two pages per worker); still a hard cap on resident page bitmaps.
        let (work_tx, work_rx) = sync_channel::<(usize, PdfPage)>(n_workers * layout_batch.max(2));
        let work_rx: Arc<Mutex<Receiver<(usize, PdfPage)>>> = Arc::new(Mutex::new(work_rx));
        let results: Arc<Mutex<Vec<(usize, PageOut)>>> = Arc::new(Mutex::new(Vec::new()));
        let first_err: Arc<Mutex<Option<PdfError>>> = Arc::new(Mutex::new(None));

        // Move the pool into the scope so each worker gets an exclusive `&mut`.
        let mut workers = std::mem::take(&mut self.pool);
        std::thread::scope(|s| {
            for worker in workers.iter_mut() {
                let work_rx = Arc::clone(&work_rx);
                let results = Arc::clone(&results);
                let first_err = Arc::clone(&first_err);
                let progress = progress.clone();
                let pages_done = &pages_done;
                let timed_out = &timed_out;
                s.spawn(move || {
                    worker.run_pool(&work_rx, layout_batch, deadline, |idx, out| {
                        match out {
                            Ok(out) => {
                                results.lock().unwrap().push((idx, out));
                                let d = pages_done
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    + 1;
                                if let Some(cb) = &progress {
                                    cb(d, selected);
                                }
                            }
                            // The budget ran out before this page's turn: the
                            // page is left out, the document is partial.
                            Err(PdfError::Timeout(_)) => {
                                timed_out.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            Err(e) => {
                                let mut slot = first_err.lock().unwrap();
                                if slot.is_none() {
                                    *slot = Some(e);
                                }
                            }
                        }
                        true
                    });
                });
            }
            // Render on this thread and feed the workers; backpressure blocks here
            // when the channel is full. Dropping `work_tx` afterwards signals the
            // workers (recv → Err) to finish. A spent budget ends the walk before
            // the next page is rendered.
            let render = pdfium_backend::for_each_page(
                bytes,
                password,
                render_image,
                extract_text,
                range,
                |i, _total, page| {
                    if expired(deadline) {
                        return Err(timeout_sentinel());
                    }
                    work_tx
                        .send((i, page))
                        .map_err(|_| PdfError::Pdfium("page-worker channel closed".into()))
                },
            );
            drop(work_tx);
            match render {
                Ok(()) => {}
                Err(PdfError::Timeout(_)) => {
                    timed_out.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) => {
                    let mut slot = first_err.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                }
            }
        });
        // Threads have joined; restore the pool for the next conversion.
        self.pool = workers;

        if let Some(e) = first_err.lock().unwrap().take() {
            return Err(e);
        }
        let mut results = Arc::try_unwrap(results)
            .unwrap_or_else(|arc| Mutex::new(arc.lock().unwrap().clone()))
            .into_inner()
            .unwrap();
        results.sort_by_key(|(idx, _)| *idx);
        let done = results.len();
        let mut doc = DoclingDocument::new(name);
        let mut confs = std::collections::BTreeMap::new();
        for (idx, (mut nodes, links, conf, page_image)) in results {
            assemble::stamp_page_no(&mut nodes, idx + 1);
            if let Some(img) = page_image {
                doc.page_images.insert(idx + 1, img);
            }
            doc.nodes.extend(nodes);
            doc.links.extend(links);
            confs.insert(idx + 1, conf);
        }
        assemble::merge_continuations(&mut doc.nodes);
        self.apply_heading_hierarchy(&mut doc.nodes, Some(bytes));
        doc.confidence = Some(docling_core::ConfidenceReport::from_pages(confs));
        let timed_out = timed_out.load(std::sync::atomic::Ordering::Relaxed);
        Ok((doc, done, timed_out))
    }

    /// Convert a PDF in **streaming** mode: `emit` is called with each finalized,
    /// in-document-order batch of nodes (and that span's recovered links) as pages
    /// complete, so a caller can serialize Markdown page by page instead of waiting
    /// for the whole document. The batches are exactly the buffered [`convert`]'s
    /// nodes, split at safe block boundaries by [`assemble::StreamAssembler`] — the
    /// parallel path reorders pages back into document order before emitting, so
    /// the output is identical regardless of worker scheduling.
    ///
    /// `emit` runs on the calling thread (never a worker), so it needn't be `Send`
    /// and its backpressure throttles the whole pipeline. Returning `Err` from
    /// `emit` aborts the conversion with that error.
    pub fn convert_streaming<F>(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
        emit: F,
    ) -> Result<(), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        self.convert_streaming_outcome(bytes, password, name, emit)
            .map(|_| ())
    }

    /// [`convert_streaming`](Self::convert_streaming) that also says how the
    /// conversion ended ([`Completion`]): a spent
    /// [`document_timeout`](Self::document_timeout) stops the walk, the pages
    /// finished so far are emitted (the tail included) and the outcome is
    /// `TimedOut` — never an error.
    pub fn convert_streaming_outcome<F>(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        name: &str,
        emit: F,
    ) -> Result<Completion, PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        let _ = name; // page nodes carry no name; the caller owns the document name.
        let deadline = self.deadline();
        let pages = pdfium_backend::page_count(bytes, password)?;
        let range = self.resolve_range(pages)?;
        let selected = range.map_or(pages, |(a, b)| b - a + 1);
        let r = if self.target_workers >= 2 && selected >= self.parallel_min {
            self.convert_streaming_parallel(bytes, password, range, deadline, selected, emit)
        } else {
            self.convert_streaming_serial(bytes, password, range, deadline, emit)
        };
        timing::report();
        let (done, timed_out) = r?;
        Ok(self.completion(timed_out, done, selected))
    }

    /// Serial streaming: render → process → emit, one page at a time, holding back
    /// only the tail that might still merge into the next page.
    fn convert_streaming_serial<F>(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        range: Option<(usize, usize)>,
        deadline: Option<std::time::Instant>,
        mut emit: F,
    ) -> Result<(usize, bool), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        let mut asm = assemble::StreamAssembler::new();
        let render_image = !self.no_ocr;
        let extract_text = self.extract_text_layer();
        let worker = self.primary()?;
        let mut done = 0usize;
        let timed_out = Self::render_ahead(
            bytes,
            password,
            render_image,
            extract_text,
            range,
            deadline,
            |n, mut page| {
                // Confidence is dropped on the streaming path: the report is
                // only complete once every page has run, which defeats
                // page-by-page emission — buffered `convert` carries it.
                let (nodes, links, _conf, _page_image) = worker.process(n, &mut page)?;
                done += 1;
                emit(asm.push(nodes), links)
            },
        )?;
        emit(asm.finish(), Vec::new())?;
        Ok((done, timed_out))
    }

    /// Parallel streaming: pages render serially on a dedicated thread (pdfium is
    /// not thread-safe) and process across the worker pool; results carry their
    /// page index and are reordered on the calling thread into a
    /// [`assemble::StreamAssembler`], which emits each page in document order as
    /// soon as its predecessors have arrived. Bounded channels keep only a handful
    /// of pages resident and let `emit`'s backpressure reach the renderer.
    fn convert_streaming_parallel<F>(
        &mut self,
        bytes: &[u8],
        password: Option<&str>,
        range: Option<(usize, usize)>,
        deadline: Option<std::time::Instant>,
        selected: usize,
        mut emit: F,
    ) -> Result<(usize, bool), PdfError>
    where
        F: FnMut(Vec<Node>, Vec<(String, String)>) -> Result<(), PdfError>,
    {
        self.ensure_pool_for(selected)?;
        let n_workers = self.pool.len();
        let render_image = !self.no_ocr;
        let extract_text = self.extract_text_layer();
        let layout_batch = pdf_layout_batch();
        // Bound sized so every worker can accumulate a full layout batch while
        // rendering stays ahead (and never below the pre-#73 render-ahead of
        // two pages per worker); still a hard cap on resident page bitmaps.
        let (work_tx, work_rx) = sync_channel::<(usize, PdfPage)>(n_workers * layout_batch.max(2));
        let work_rx: Arc<Mutex<Receiver<(usize, PdfPage)>>> = Arc::new(Mutex::new(work_rx));
        // Workers and the renderer report here; the calling thread drains it in
        // page order. Bounded so workers block (bounding resident bitmaps) when the
        // consumer falls behind.
        let (res_tx, res_rx) = sync_channel::<Result<(usize, PageOut), PdfError>>(n_workers * 2);

        let mut workers = std::mem::take(&mut self.pool);
        let mut asm = assemble::StreamAssembler::new();
        let mut first_err: Option<PdfError> = None;
        let mut timed_out = false;
        let mut done = 0usize;

        std::thread::scope(|s| {
            // Workers: pull a batch of pages (whatever is already rendered, up
            // to the layout batch size), process it, report (index-tagged)
            // results.
            for worker in workers.iter_mut() {
                let work_rx = Arc::clone(&work_rx);
                let res_tx = res_tx.clone();
                s.spawn(move || {
                    worker.run_pool(&work_rx, layout_batch, deadline, |idx, out| {
                        // `false` once the consumer is gone.
                        res_tx.send(out.map(|o| (idx, o))).is_ok()
                    });
                });
            }
            // Renderer: feed pages to the pool on its own thread (pdfium stays on a
            // single thread); report a render error through the same channel. A
            // spent budget ends the walk before the next page is rendered.
            {
                let res_tx = res_tx.clone();
                s.spawn(move || {
                    let render = pdfium_backend::for_each_page(
                        bytes,
                        password,
                        render_image,
                        extract_text,
                        range,
                        |i, _total, page| {
                            if expired(deadline) {
                                return Err(timeout_sentinel());
                            }
                            work_tx
                                .send((i, page))
                                .map_err(|_| PdfError::Pdfium("page-worker channel closed".into()))
                        },
                    );
                    drop(work_tx); // signal workers to finish
                    if let Err(e) = render {
                        let _ = res_tx.send(Err(e));
                    }
                });
            }
            // Drop our own sender so the channel closes once the threads finish.
            drop(res_tx);

            // Collector (this thread): reorder into document order and emit.
            // With a page window, indices start at the window's first page.
            // A timed-out page leaves a hole in the sequence: everything
            // before it emits in order, the pages after it stay buffered —
            // they are the pages the budget did not cover, and the document
            // ends at the cut like the buffered path's.
            let mut buffer: BTreeMap<usize, PageOut> = BTreeMap::new();
            let mut next = range.map_or(0, |(first, _)| first);
            for msg in res_rx.iter() {
                match msg {
                    Err(PdfError::Timeout(_)) => timed_out = true,
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                    Ok((idx, out)) => {
                        buffer.insert(idx, out);
                        if first_err.is_some() {
                            continue; // keep draining so the threads can exit
                        }
                        // A stream carries Markdown, not the page map: page
                        // images (#520) have nowhere to go and are dropped.
                        while let Some((nodes, links, _conf, _page_image)) = buffer.remove(&next) {
                            if let Err(e) = emit(asm.push(nodes), links) {
                                first_err = Some(e);
                                break;
                            }
                            next += 1;
                            done += 1;
                        }
                    }
                }
            }
            // Pages that finished after a hole (a page the budget skipped
            // before them) cannot follow it in document order; count them
            // out of the document like the skipped one.
            if timed_out && !buffer.is_empty() {
                buffer.clear();
            }
        });
        // Threads have joined; restore the pool for the next conversion.
        self.pool = workers;

        if let Some(e) = first_err {
            return Err(e);
        }
        emit(asm.finish(), Vec::new())?;
        Ok((done, timed_out))
    }

    /// Lazily grow the pool to `target_workers`, loading the new workers
    /// concurrently (model load is mostly I/O + mmap, so N loads overlap to roughly
    /// one load's wall-time). Cached for reuse across documents.
    fn ensure_pool(&mut self) -> Result<(), PdfError> {
        self.ensure_pool_for(self.target_workers)
    }

    /// [`ensure_pool`](Self::ensure_pool) for a document of `pages` pages:
    /// no more workers than pages (#682 — a two-page document on a 16-core
    /// host loads two workers, not eight), never fewer than two, and never
    /// more than `target_workers`; a later, longer document grows the pool.
    fn ensure_pool_for(&mut self, pages: usize) -> Result<(), PdfError> {
        let want = pages.max(2).min(self.target_workers);
        let need = want.saturating_sub(self.pool.len());
        if need == 0 {
            return Ok(());
        }
        let intra = pdf_intra();
        let no_ocr = self.no_ocr;
        let skip_ocr = self.skip_ocr;
        let force = self.force_full_page_ocr || self.ocr_mode.forces_full_page();
        let ntp = self.no_text_panels;
        let ocr_lang = self.ocr_lang;
        let ocr_engine = self.ocr_engine;
        let tesseract_lang = self.tesseract_lang.clone();
        let ocr_scale = self.ocr_scale;
        let textline_orientation = self.textline_orientation;
        let images = self.images;
        let enrich = self.enrich;
        let tables = self.tables_slot();
        let enrich_slots = self.enrich_slots();
        let loaded: Vec<Result<Worker, PdfError>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..need)
                .map(|_| {
                    let tables = tables.clone();
                    let enrich_slots = enrich_slots.clone();
                    let tesseract_lang = tesseract_lang.clone();
                    s.spawn(move || {
                        Worker::load(
                            intra,
                            tables,
                            enrich_slots,
                            enrich,
                            no_ocr,
                            skip_ocr,
                            force,
                            ntp,
                            ocr_lang,
                            ocr_engine,
                            tesseract_lang,
                            ocr_scale,
                            textline_orientation,
                            images,
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for w in loaded {
            self.pool.push(w?);
        }
        Ok(())
    }

    /// Convert a standalone image (PNG/JPEG/TIFF/WebP/…) as a single page —
    /// docling routes images through the same layout+OCR pipeline as a PDF page.
    pub fn convert_image(&mut self, bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
        let image = decode_image_limited(bytes)?;
        let (w, h) = image.dimensions();
        // The image is its own page rendered at 1 px per "point" (scale 1.0); a
        // standalone image has no text layer, so OCR supplies the cells.
        let page = PdfPage {
            width: w as f32,
            height: h as f32,
            scale: 1.0,
            cells: Vec::new(),
            code_cells: Vec::new(),
            checkboxes: Vec::new(),
            word_cells: Vec::new(),
            // A standalone image *is* its own scale-1.0 page image, so the
            // layout model sees it through the docling-exact PIL kernel.
            image_layout: Some(image.clone()),
            image,
            links: Vec::new(),
            rotation: 0,
            pending_image: None,
        };
        self.process_pages(vec![page], name)
    }

    /// Run layout (+ OCR for cell-less pages) and assemble each already-rendered
    /// page (image / METS inputs, which are small and already materialised).
    /// Public so [`mets::convert_mets_gbs_with_pipeline`] can drive a
    /// caller-configured pipeline (#244).
    pub fn process_pages(
        &mut self,
        mut pages: Vec<PdfPage>,
        name: &str,
    ) -> Result<DoclingDocument, PdfError> {
        let mut doc = DoclingDocument::new(name);
        let mut confs = std::collections::BTreeMap::new();
        let worker = self.primary()?;
        for (n, page) in pages.iter_mut().enumerate() {
            let (mut nodes, links, conf, page_image) = worker.process(n, page)?;
            assemble::stamp_page_no(&mut nodes, n + 1);
            if let Some(img) = page_image {
                doc.page_images.insert(n + 1, img);
            }
            doc.nodes.extend(nodes);
            doc.links.extend(links);
            confs.insert(n + 1, conf);
        }
        assemble::merge_continuations(&mut doc.nodes);
        // No PDF behind these pages (images, METS): the heading-hierarchy
        // stage degrades to the numbering signal — exactly docling without
        // an outline or parsed pages.
        self.apply_heading_hierarchy(&mut doc.nodes, None);
        doc.confidence = Some(docling_core::ConfidenceReport::from_pages(confs));
        Ok(doc)
    }
}

/// Number of pages in a PDF, without converting anything — what the CLI batch
/// mode prints in its per-document start line.
#[cfg(feature = "ml")]
pub fn page_count(bytes: &[u8], password: Option<&str>) -> Result<usize, PdfError> {
    pdfium_backend::page_count(bytes, password)
}

#[cfg(feature = "ml")]
/// Convenience one-shot conversion (loads the pipeline per call). Errors are
/// detailed and surfaced (never silently skipped).
pub fn convert(
    bytes: &[u8],
    password: Option<&str>,
    name: &str,
) -> Result<DoclingDocument, PdfError> {
    convert_with_options(
        bytes,
        password,
        name,
        false,
        false,
        false,
        false,
        EnrichmentOptions::default(),
        None,
        None,
    )
}

#[cfg(feature = "ml")]
/// Like [`convert`], but optionally skips loading/running TableFormer (see
/// [`Pipeline::no_table_former`]) and/or layout+OCR+TableFormer entirely (see
/// [`Pipeline::no_ocr`]), and/or enables the enrichment passes (see
/// [`Pipeline::enrichments`]).
// One positional per pipeline switch mirrors the Pipeline builder; growing
// past clippy's arity cap is the price of keeping this one-shot signature
// stable-ish instead of churning callers into an options struct mid-series.
#[allow(clippy::too_many_arguments)]
pub fn convert_with_options(
    bytes: &[u8],
    password: Option<&str>,
    name: &str,
    no_table_former: bool,
    no_ocr: bool,
    force_full_page_ocr: bool,
    no_text_panels: bool,
    enrich: EnrichmentOptions,
    pages: Option<(usize, usize)>,
    ocr_lang: Option<OcrLang>,
) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?
        .no_table_former(no_table_former)
        .no_ocr(no_ocr)
        .force_full_page_ocr(force_full_page_ocr)
        .no_text_panels(no_text_panels)
        .enrichments(enrich)
        .pages(pages)
        .ocr_lang(ocr_lang)
        .convert(bytes, password, name)
}

#[cfg(feature = "ml")]
/// Convenience one-shot image conversion (loads the pipeline per call).
pub fn convert_image(bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
    convert_image_with_options(
        bytes,
        name,
        false,
        false,
        false,
        EnrichmentOptions::default(),
        None,
    )
}

#[cfg(feature = "ml")]
/// Like [`convert_image`], but optionally skips loading/running TableFormer (see
/// [`Pipeline::no_table_former`]) and/or layout+OCR+TableFormer entirely (see
/// [`Pipeline::no_ocr`]), and/or enables the enrichment passes.
pub fn convert_image_with_options(
    bytes: &[u8],
    name: &str,
    no_table_former: bool,
    no_ocr: bool,
    no_text_panels: bool,
    enrich: EnrichmentOptions,
    ocr_lang: Option<OcrLang>,
) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?
        .no_table_former(no_table_former)
        .no_ocr(no_ocr)
        .no_text_panels(no_text_panels)
        .enrichments(enrich)
        .ocr_lang(ocr_lang)
        .convert_image(bytes, name)
}

#[cfg(feature = "ml")]
/// Convert pre-segmented pages (image + already-known text cells, e.g. METS/hOCR
/// scans) through the shared layout + assembly pipeline.
pub fn convert_pages(pages: Vec<PdfPage>, name: &str) -> Result<DoclingDocument, PdfError> {
    convert_pages_with_options(
        pages,
        name,
        false,
        false,
        false,
        EnrichmentOptions::default(),
    )
}

#[cfg(feature = "ml")]
/// Like [`convert_pages`], but optionally skips loading/running TableFormer (see
/// [`Pipeline::no_table_former`]) and/or layout+OCR+TableFormer entirely (see
/// [`Pipeline::no_ocr`]), and/or enables the enrichment passes.
pub fn convert_pages_with_options(
    pages: Vec<PdfPage>,
    name: &str,
    no_table_former: bool,
    no_ocr: bool,
    no_text_panels: bool,
    enrich: EnrichmentOptions,
) -> Result<DoclingDocument, PdfError> {
    Pipeline::new()?
        .no_table_former(no_table_former)
        .no_text_panels(no_text_panels)
        .no_ocr(no_ocr)
        .enrichments(enrich)
        .process_pages(pages, name)
}

#[cfg(feature = "ml")]
#[cfg(all(test, feature = "ml"))]
mod image_limit_tests {
    use super::decode_image_with_max_side;

    /// A small valid PNG encoded via the `image` crate (robust vs. a hand-rolled
    /// byte literal).
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        use std::io::Cursor;
        let img = image::RgbImage::new(w, h);
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn normal_image_decodes_under_the_cap() {
        let img = decode_image_with_max_side(&png_bytes(8, 8), 30_000).expect("8x8 decodes");
        assert_eq!(img.dimensions(), (8, 8));
    }

    /// docling#4247 (2.128): the EXIF orientation is applied when a frame is
    /// loaded — a 4×2 JPEG tagged "rotate 90° CW" (orientation 6) decodes as
    /// 2×4, with the pixels turned.
    #[test]
    fn exif_orientation_is_applied() {
        use std::io::Cursor;
        let mut img = image::RgbImage::new(4, 2);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        let mut jpeg = Vec::new();
        img.write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        // Splice an APP1 Exif segment (little-endian TIFF, one IFD entry:
        // Orientation = 6) right after the SOI marker.
        let mut tiff = b"II*\0".to_vec();
        tiff.extend(8u32.to_le_bytes());
        tiff.extend(1u16.to_le_bytes());
        tiff.extend(0x0112u16.to_le_bytes());
        tiff.extend(3u16.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        tiff.extend(6u16.to_le_bytes());
        tiff.extend([0, 0]);
        tiff.extend(0u32.to_le_bytes());
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend([0xff, 0xe1]);
        out.extend((app1.len() as u16 + 2).to_be_bytes());
        out.extend(app1);
        out.extend(&jpeg[2..]);
        let plain = decode_image_with_max_side(&jpeg, 30_000).unwrap();
        assert_eq!(plain.dimensions(), (4, 2));
        let turned = decode_image_with_max_side(&out, 30_000).unwrap();
        assert_eq!(turned.dimensions(), (2, 4), "quarter turn swaps the sides");
        // Rotating 90° CW moves the top-left pixel to the top-right corner.
        assert!(turned.get_pixel(1, 0).0[0] > 128);
        assert!(turned.get_pixel(0, 0).0[0] <= 128);
    }

    #[test]
    fn dimensions_over_the_cap_are_rejected_not_aborted() {
        // A per-side cap below the image's declared size must yield a
        // recoverable Err, never an allocation-abort — the mechanism that stops
        // a crafted image declaring 60000×60000 from OOM-killing the process.
        let r = decode_image_with_max_side(&png_bytes(8, 8), 4);
        assert!(
            r.is_err(),
            "decode must fail under the pixel cap, not abort"
        );
    }
}

#[cfg(all(test, feature = "ml"))]
mod ocr_scale_tests {
    use super::*;

    /// #570: an explicit scale wins everywhere; a rendered page keeps its
    /// render; a scale-1.0 image page reads at docling's 3 px/pt, shrunk to
    /// RapidOCR's 2000 px longer side (a FUNSD scan at 2.0, a 3000 px photo
    /// downsampled to 0.67), and an image that lands at 1.0 is not resampled.
    #[test]
    fn image_inputs_follow_rapidocrs_resolution() {
        assert_eq!(page_ocr_scale(Some(1.5), 754.0, 1000.0, 1.0), Some(1.5));
        assert_eq!(page_ocr_scale(None, 612.0, 792.0, 2.0), None);
        assert_eq!(page_ocr_scale(None, 754.0, 1000.0, 1.0), Some(2.0));
        assert_eq!(page_ocr_scale(None, 400.0, 600.0, 1.0), Some(3.0));
        let s = page_ocr_scale(None, 3000.0, 2000.0, 1.0).unwrap();
        assert!((s - 2.0 / 3.0).abs() < 1e-6, "{s}");
        assert_eq!(page_ocr_scale(None, 2000.0, 1500.0, 1.0), None);
    }
}

#[cfg(test)]
mod median_tests {
    #[test]
    fn median_of_empty_is_zero_not_a_panic() {
        // A crafted table can leave a row/column with zero matched cells; the
        // even-count branch would index values[0 - 1] and panic (→ remote crash
        // via docling-serve) without the empty guard.
        assert_eq!(super::tf_match::median_for_test(&mut []), 0.0);
        assert_eq!(super::tf_match::median_for_test(&mut [4.0, 2.0]), 3.0);
        assert_eq!(super::tf_match::median_for_test(&mut [5.0, 1.0, 3.0]), 3.0);
    }
}

#[cfg(test)]
mod unreadable_tests {
    /// The text-layer path (the wasm build's only one) tells a file nothing
    /// can open apart from a PDF that merely has no text layer: garbage is
    /// an error about the file, a real PDF converts.
    #[test]
    fn text_layer_path_reports_an_unreadable_file() {
        for bytes in [b"garbage".as_slice(), b"", b"%PDF-1.7\n%%EOF\n"] {
            let err = super::convert_text_layer(bytes, "x.pdf").unwrap_err();
            assert!(err.to_string().contains("not a readable PDF"), "{err}");
        }
        let real = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/data/pdf/sources/multi_page.pdf"),
        )
        .unwrap();
        assert!(!super::convert_text_layer(&real, "multi_page.pdf")
            .unwrap()
            .nodes
            .is_empty());
    }
}

#[cfg(test)]
mod send_check {
    /// The Node bindings (`docling-node`) run a shared [`super::Pipeline`] on
    /// libuv worker threads (`Arc<Mutex<Pipeline>>`), which is only sound while
    /// `Pipeline: Send` holds — this fails to compile if a non-`Send` field
    /// (e.g. an `Rc` or a raw pdfium handle) ever lands in the pipeline.
    fn assert_send<T: Send>() {}

    #[test]
    fn pipeline_is_send() {
        assert_send::<super::Pipeline>();
    }
}

#[cfg(all(test, feature = "ml"))]
mod ocr_input_tests {
    /// #254: without an `ocr_scale` (or with one equal to the render scale)
    /// the OCR reads the page render untouched and the cache stays cold; a
    /// different scale builds one resampled view, reuses it across calls, and
    /// reports the requested px/pt so cell geometry divides back to points.
    #[test]
    fn ocr_input_resamples_only_on_a_real_scale_change() {
        let img = image::RgbImage::new(200, 100);
        let mut cache = None;
        let (v, s) = super::ocr_input(&mut cache, &img, 2.0, None);
        assert!(std::ptr::eq(v, &img) && s == 2.0 && cache.is_none());
        let (v, s) = super::ocr_input(&mut cache, &img, 2.0, Some(2.0));
        assert!(std::ptr::eq(v, &img) && s == 2.0 && cache.is_none());

        let (v, s) = super::ocr_input(&mut cache, &img, 2.0, Some(3.0));
        assert_eq!((v.width(), v.height(), s), (300, 150, 3.0));
        let first = cache.as_ref().map(|c| c as *const image::RgbImage);
        let (v, _) = super::ocr_input(&mut cache, &img, 2.0, Some(3.0));
        assert_eq!(
            Some(v as *const image::RgbImage),
            first,
            "cached, not rebuilt"
        );

        let mut down = None;
        let (v, s) = super::ocr_input(&mut down, &img, 2.0, Some(1.0));
        assert_eq!((v.width(), v.height(), s), (100, 50, 1.0));
    }
}
