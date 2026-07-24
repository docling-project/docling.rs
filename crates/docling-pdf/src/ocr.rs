//! OCR for scanned pages, via the PP-OCRv3 recognition model (CRNN/SVTR) run
//! with `ort`. The layout model already locates text regions on the page image
//! (it works without a text layer), so OCR only needs *recognition*: each text
//! region is cropped, split into lines by horizontal projection, and each line
//! is recognised and decoded with CTC — producing [`TextCell`]s the normal
//! layout assembly then consumes. This avoids a separate text-detection model.

use image::{imageops, RgbImage};
use ort::session::Session;
use ort::value::Tensor;

use crate::layout::Region;
// The ONNX-free half (line prep, batching, CTC decode) lives in `ocr_prep`
// so the wasm build shares it verbatim (#79 phase 2).
use crate::ocr_prep::{
    batch_input, decode_row, dict_chars, prep_line, segment_lines, width_batches, PrepLine,
    REC_HEIGHT,
};
use crate::pdfium_backend::TextCell;

pub struct OcrModel {
    rec: Session,
    /// CTC classes: index 0 = blank, 1..=6623 = dictionary, 6624 = space.
    chars: Vec<String>,
}

/// Layout labels whose content is recognised as running text.
fn is_text_label(label: &str) -> bool {
    matches!(
        label,
        "text"
            | "title"
            | "section_header"
            | "list_item"
            | "caption"
            | "footnote"
            | "code"
            | "formula"
    )
}

/// OCR recognition language: which PP-OCRv3 model + dictionary pair runs.
///
/// The default is **English** (`models/ocr_rec_en.onnx` + `models/en_dict.txt`):
/// the multilingual `ch_` model reads Latin scripts with badly degraded word
/// spacing (glued words on ordinary English scans), which is the common
/// real-world case. `Ch` selects the `ch_` pair (`models/ocr_rec.onnx` +
/// `models/ppocr_keys_v1.txt`) — that is what upstream docling conformance is
/// measured with, and `scripts/conformance/pdf_*.sh` pin it explicitly (by
/// path, which wins over this selector).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcrLang {
    /// en_PP-OCRv3 — English-only, proper Latin word spacing.
    #[default]
    En,
    /// ch_PP-OCRv3 — multilingual; the docling-conformance model.
    Ch,
}

impl OcrLang {
    /// Parse a user-supplied language id. `None` for anything but `en`/`ch`
    /// (trimmed, case-insensitive) — callers surface their own error/warning.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "en" => Some(Self::En),
            "ch" => Some(Self::Ch),
            _ => None,
        }
    }

    /// The process-level choice from `DOCLING_RS_OCR_LANG` (empty/unset → the
    /// English default; unknown values warn and use English).
    pub fn from_env() -> Self {
        let raw = std::env::var("DOCLING_RS_OCR_LANG").unwrap_or_default();
        if raw.trim().is_empty() {
            return Self::default();
        }
        Self::parse(&raw).unwrap_or_else(|| {
            eprintln!("docling-pdf: DOCLING_RS_OCR_LANG={raw:?} is not en|ch; using en");
            Self::default()
        })
    }
}

/// Resolve the recognition model + dictionary pair for `lang`. An English
/// default that isn't on disk (older model checkouts) degrades to the `ch_`
/// pair with a warning rather than failing — the usual missing-optional-asset
/// convention. Explicit `DOCLING_OCR_REC_ONNX` / `DOCLING_OCR_DICT` paths win
/// over all of this; they are a pair, so set both together.
fn resolve_rec_pair(lang: OcrLang) -> (String, String) {
    const CH: (&str, &str) = ("models/ocr_rec.onnx", "models/ppocr_keys_v1.txt");
    const EN: (&str, &str) = ("models/ocr_rec_en.onnx", "models/en_dict.txt");
    let want_ch = lang == OcrLang::Ch;
    let pick = if want_ch { CH } else { EN };
    let (mut rec, mut dict) = (crate::resolve_asset(pick.0), crate::resolve_asset(pick.1));
    if !want_ch && (!std::path::Path::new(&rec).exists() || !std::path::Path::new(&dict).exists()) {
        let (ch_rec, ch_dict) = (crate::resolve_asset(CH.0), crate::resolve_asset(CH.1));
        if std::path::Path::new(&ch_rec).exists() && std::path::Path::new(&ch_dict).exists() {
            eprintln!(
                "docling-pdf: English OCR model not found ({rec}); falling back to the \
                 multilingual ch_ model — expect weak Latin word spacing. Fetch it with \
                 scripts/install/download_dependencies.sh"
            );
            (rec, dict) = (ch_rec, ch_dict);
        }
    }
    (
        std::env::var("DOCLING_OCR_REC_ONNX").unwrap_or(rec),
        std::env::var("DOCLING_OCR_DICT").unwrap_or(dict),
    )
}

impl OcrModel {
    /// Load the recognition model and its character dictionary for `lang` —
    /// see [`resolve_rec_pair`] for the selection rules (explicit
    /// `DOCLING_OCR_REC_ONNX`/`DOCLING_OCR_DICT` paths win).
    pub fn load(lang: OcrLang) -> Result<Self, String> {
        let (rec_path, dict_path) = resolve_rec_pair(lang);
        // Single-threaded: ORT's multi-threaded float-reduction order varies
        // across runs, which flips the CTC argmax on low-confidence characters
        // (e.g. noisy faxes) and makes the snapshot output non-deterministic. The
        // recognition inputs are tiny per-line crops, so the throughput cost is
        // negligible.
        let builder = Session::builder()
            .map_err(|e| format!("ocr: builder: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| format!("ocr: intra_threads: {e}"))?;
        let rec = crate::ep::apply(builder)
            .map_err(|e| format!("ocr: {e}"))?
            .commit_from_file(&rec_path)
            .map_err(|e| format!("ocr: load {rec_path}: {e}"))?;
        let dict = std::fs::read_to_string(&dict_path)
            .map_err(|e| format!("ocr: read dict {dict_path}: {e}"))?;
        Ok(Self {
            rec,
            chars: dict_chars(&dict),
        })
    }

    /// Recognise a batch of prepared *same-width* lines in one session run.
    ///
    /// Only equal widths ever share a run: same-width batching is
    /// bit-identical to one-at-a-time recognition (each sample keeps its own
    /// data and per-sample kernel reduction order — verified empirically on
    /// the scanned corpus), whereas width-padding leaks into the real
    /// timesteps through the model's global-attention blocks and measurably
    /// changes low-confidence characters.
    fn recognize_batch(
        &mut self,
        w: usize,
        chunk: &[usize],
        lines: &[PrepLine],
    ) -> Result<Vec<String>, String> {
        let n = chunk.len();
        let data = batch_input(w, chunk, lines);
        let input = Tensor::from_array(([n, 3, REC_HEIGHT as usize, w], data))
            .map_err(|e| format!("ocr: input tensor: {e}"))?;
        let outputs = self
            .rec
            .run(ort::inputs!["x" => input])
            .map_err(|e| format!("ocr: rec inference: {e}"))?;
        let (shape, probs) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("ocr: extract rec: {e}"))?;
        let t_len = shape[1] as usize;
        let nc = shape[2] as usize;
        Ok((0..n)
            .map(|i| {
                decode_row(
                    &self.chars,
                    &probs[i * t_len * nc..(i + 1) * t_len * nc],
                    nc,
                )
            })
            .collect())
    }

    /// OCR a page: produce text cells (page points) for every line found inside
    /// the text regions. `scale` is image-px per page-point.
    pub fn ocr_page(
        &mut self,
        img: &RgbImage,
        regions: &[Region],
        scale: f32,
    ) -> Result<Vec<TextCell>, String> {
        let (iw, ih) = img.dimensions();
        // Gather every line crop on the page first, so equal-width lines can
        // share a recognition run regardless of which region they came from.
        let mut bboxes: Vec<(f32, f32, f32, f32)> = Vec::new();
        let mut lines: Vec<PrepLine> = Vec::new();
        for region in regions {
            if !is_text_label(region.label) {
                continue;
            }
            let l = (region.l * scale).max(0.0) as u32;
            let t = (region.t * scale).max(0.0) as u32;
            let r = ((region.r * scale).max(0.0) as u32).min(iw);
            let b = ((region.b * scale).max(0.0) as u32).min(ih);
            if r <= l || b <= t {
                continue;
            }
            let crop = imageops::crop_imm(img, l, t, r - l, b - t).to_image();
            for (lx, ly, rx, ry) in segment_lines(&crop) {
                let line = imageops::crop_imm(&crop, lx, ly, rx - lx, ry - ly).to_image();
                let Some(pl) = prep_line(&line) else {
                    continue;
                };
                bboxes.push((
                    (l + lx) as f32 / scale,
                    (t + ly) as f32 / scale,
                    (l + rx) as f32 / scale,
                    (t + ry) as f32 / scale,
                ));
                lines.push(pl);
            }
        }

        // Deterministic width-batching (shared with the wasm path).
        let mut texts = vec![String::new(); lines.len()];
        for (w, chunk) in width_batches(&lines) {
            for (&i, text) in chunk.iter().zip(self.recognize_batch(w, &chunk, &lines)?) {
                texts[i] = text;
            }
        }

        // Emit cells in page order, exactly as the sequential walk did.
        let mut cells = Vec::new();
        for ((l, t, r, b), text) in bboxes.into_iter().zip(texts) {
            let text = text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            cells.push(TextCell { text, l, t, r, b });
        }
        Ok(cells)
    }
}
