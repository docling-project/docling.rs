//! OCR for scanned pages, via the PP-OCRv3 recognition model (CRNN/SVTR) run
//! with `ort`. The layout model locates the text regions on the page image
//! (it works without a text layer); inside them the recognizer reads the
//! lines the PP-OCR text detector found (`ocr_det`, #429/#570 — RapidOCR's
//! own crops, each one text run with the detector's margin), falling back to
//! a horizontal-projection split of the region crop where the detector saw
//! nothing or is not installed; each line is recognised and decoded with CTC
//! — producing [`TextCell`]s the normal layout assembly then consumes. A line
//! under RapidOCR's `text_score` confidence is dropped (`ocr_prep::text_score`).

use image::RgbImage;
use ort::session::Session;
use ort::value::Tensor;

use crate::layout::Region;
// The ONNX-free half (line prep, batching, CTC decode) lives in `ocr_prep`
// so the wasm build shares it verbatim (#79 phase 2).
use crate::ocr_prep::{
    batch_input, decode_row_scored, dict_chars, prep_region_lines, prep_region_lines_det,
    prep_table_words, prep_table_words_det, width_batches, PrepLine, REC_HEIGHT,
};
use crate::pdfium_backend::TextCell;

pub struct OcrModel {
    /// Single-threaded recognition sessions, one per parallel lane (see
    /// [`Self::load_with`]); lines are dealt across them by batch index.
    recs: Vec<Session>,
    /// CTC classes: index 0 = blank, then the dictionary, then space.
    chars: Vec<String>,
    /// RapidOCR's line orientation handling (#666). See
    /// [`crate::Pipeline::ocr_textline_orientation`].
    orient: bool,
    /// The text-line orientation classifier, loaded on the first oriented
    /// page.
    cls: ClsSlot,
}

/// [`OcrModel::cls`]: `Missing` records a failed load (warned once per
/// process), so it is not retried per page.
enum ClsSlot {
    Unloaded,
    Ready(crate::ocr_cls::TextlineCls),
    Missing,
}

/// OCR recognition language: which PP-OCRv3 model + dictionary pair runs
/// when the PP-OCRv6 recognizer is not installed.
///
/// With `.models/ocr_rec_v6.onnx` + `.models/ocr_rec_v6_dict.txt` on disk
/// (#570; `download_dependencies.sh` fetches them) both languages run that
/// one multilingual model — RapidOCR's `PP-OCRv6_rec_small`, the recognizer
/// docling runs for English and Chinese alike, and the single largest factor
/// in the FUNSD word-recall gap once the detector's lines are the crops
/// (0.70 → 0.77 on 30 forms). Without it, the PP-OCRv3 pairs: the default is
/// **English** (`.models/ocr_rec_en.onnx` + `.models/en_dict.txt`) — the
/// multilingual `ch_` v3 model reads Latin scripts with badly degraded word
/// spacing (glued words on ordinary English scans) — and `Ch` selects the
/// `ch_` pair (`.models/ocr_rec.onnx` + `.models/ppocr_keys_v1.txt`), what
/// the PDF conformance baselines were pinned against;
/// `scripts/conformance/pdf_*.sh` pin it explicitly by path, which wins over
/// this selector and over the v6 preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcrLang {
    /// en_PP-OCRv3 — English-only, proper Latin word spacing.
    #[default]
    En,
    /// ch_PP-OCRv3 — multilingual; the docling-conformance model.
    Ch,
}

impl OcrLang {
    /// Parse a user-supplied language id (#388): the engine's own codes
    /// (`en`, `ch`) and BCP-47 tags naming a language one of the two
    /// recognizers reads, trimmed and case-insensitive. `None` for anything
    /// else — callers surface their own error/warning.
    ///
    /// docling canonicalizes OCR languages across its engines (docling#4075):
    /// a bare value is the engine's native code, an `iso:`-prefixed value a
    /// BCP-47 tag reduced to a language-script pair with the region dropped
    /// (`zh-CN` and `zh-Hans` are the same recognizer, `en-GB` is `en`), and
    /// the RapidOCR adapter maps `en` → its `en` model and `zh-Hans` → `ch`.
    /// With only those two PP-OCRv3 pairs on board there is no ambiguity, so
    /// the prefix is optional here: `en-US`, `eng`, `zh`, `zh-Hans`, `iso:zh-CN`
    /// all resolve without a warning. Accepted primary subtags: English as
    /// `en` / ISO 639-2/3 `eng` / docling's legacy `english`; Chinese as the
    /// engine code `ch` (and RapidOCR's `chinese_cht`), `zh` / `zho` / `chi` /
    /// `cmn` / legacy `chinese` / EasyOCR's `ch_sim` / `ch_tra`. Script,
    /// region and variant subtags (`-Hans`, `-Hant`, `-CN`, `-TW`, `_US`) are
    /// ignored: a traditional-script request (`zh-Hant`, `zh-TW`) gets the
    /// multilingual `ch` recognizer too, the closest model shipped — upstream
    /// would pick RapidOCR's separate `chinese_cht`, which this engine does
    /// not carry. Genuinely unsupported languages (`de`, `fr`, `ja`, …) parse
    /// to `None` and keep warning.
    pub fn parse(s: &str) -> Option<Self> {
        let token = s.trim().to_ascii_lowercase();
        let tag = token.strip_prefix("iso:").unwrap_or(&token).trim();
        let primary = tag.split(['-', '_']).next().unwrap_or_default();
        match primary {
            "en" | "eng" | "english" => Some(Self::En),
            "ch" | "chinese_cht" | "zh" | "zho" | "chi" | "cmn" | "chinese" | "ch_sim"
            | "ch_tra" => Some(Self::Ch),
            _ => None,
        }
    }

    /// The process-level choice from `DOCLING_RS_OCR_LANG` (empty/unset → the
    /// English default; unknown values warn and use English). Under a
    /// `DOCLING_RS_OCR_ENGINE=tesseract` default the variable is Tesseract's
    /// language list (`rus+eng`, [`crate::tesseract::lang_arg`]), not a
    /// PP-OCR model choice: a value naming no en/ch model then selects the
    /// English model silently — it is only used if a request switches the
    /// engine back to PP-OCR — instead of warning on every request (#889).
    pub fn from_env() -> Self {
        let raw = docling_core::env::nonempty("DOCLING_RS_OCR_LANG");
        Self::from_env_value(raw.as_deref(), OcrEngine::from_env)
    }

    /// [`Self::from_env`] on the variable's value and the default engine
    /// (asked only when the value names no model).
    fn from_env_value(raw: Option<&str>, engine: impl Fn() -> OcrEngine) -> Self {
        let Some(raw) = raw else {
            return Self::default();
        };
        Self::parse(raw).unwrap_or_else(|| {
            if engine() == OcrEngine::Tesseract {
                return Self::default();
            }
            eprintln!(
                "docling-pdf: DOCLING_RS_OCR_LANG={raw:?} names no language the en/ch \
                 recognizers read ({}); using en",
                Self::ACCEPTED
            );
            Self::default()
        })
    }

    /// The accepted spellings, for error messages and docs.
    pub const ACCEPTED: &'static str =
        "en | ch, or a BCP-47 tag for English or Chinese such as en-US, eng, zh, zh-Hans, zh-TW";
}

/// Which document regions feed the OCR — docling 2.116's `OcrMode` (#254,
/// upstream docling#3710). Upstream restructured its pipeline so OCR runs
/// *after* layout, on layout regions filtered by the PDF text layer — the
/// architecture this port has always had — and named the strategies:
///
/// - `PdfAwareLayoutRegions` (upstream's **default**): OCR only layout regions
///   the embedded text layer can't cover. Exactly the standard path here —
///   scanned pages OCR their regions, digital pages OCR their bare pictures
///   and every text/furniture/table region no programmatic text touches
///   (vector-outlined text beside a native text layer, #931).
/// - `FullPage` / `LayoutRegions`: ignore the PDF text layer and OCR
///   everything. Both map onto the [`force_full_page_ocr`] machinery (discard
///   the text layer, OCR every layout region): the upstream distinction —
///   whole-page vs per-region *detector* input — has no analogue in this
///   engine, whose PP-OCR recognizer always consumes per-region line crops.
///
/// [`force_full_page_ocr`]: crate::Pipeline::force_full_page_ocr
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcrMode {
    /// Upstream's `default`: currently wired to `PdfAwareLayoutRegions`.
    #[default]
    Default,
    /// OCR the full page, text layer ignored (docling's `full_page`; the
    /// mode-shaped spelling of `force_full_page_ocr`).
    FullPage,
    /// OCR every layout region, text layer ignored (docling's
    /// `layout_regions`).
    LayoutRegions,
    /// OCR layout regions the text layer can't cover (docling's
    /// `pdf_aware_layout_regions` — the default behavior).
    PdfAwareLayoutRegions,
}

impl OcrMode {
    /// Parse docling's mode ids. `None` for anything else — callers surface
    /// their own error/warning.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "full_page" => Some(Self::FullPage),
            "layout_regions" => Some(Self::LayoutRegions),
            "pdf_aware_layout_regions" => Some(Self::PdfAwareLayoutRegions),
            _ => None,
        }
    }

    /// The process-level choice from `DOCLING_RS_OCR_MODE` (empty/unset → the
    /// default; unknown values warn and use the default).
    pub fn from_env() -> Self {
        let Some(raw) = docling_core::env::nonempty("DOCLING_RS_OCR_MODE") else {
            return Self::default();
        };
        Self::parse(&raw).unwrap_or_else(|| {
            eprintln!(
                "docling-pdf: DOCLING_RS_OCR_MODE={raw:?} is not \
                 default|full_page|layout_regions|pdf_aware_layout_regions; using default"
            );
            Self::default()
        })
    }

    /// Whether this mode discards the embedded text layer — the engine truth
    /// both non-default modes reduce to.
    pub fn forces_full_page(self) -> bool {
        matches!(self, Self::FullPage | Self::LayoutRegions)
    }
}

/// Which OCR engine recognizes text (#460): the built-in PP-OCRv3 recognizer
/// (+ the RapidOCR text detector) — the default and the engine every
/// conformance baseline is pinned against — or the system `tesseract` binary
/// (see [`crate::tesseract`]), docling's `TesseractCliOcrOptions`
/// counterpart. Both consume the same layout-region crops and produce the
/// same cells; everything downstream is engine-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OcrEngine {
    /// PP-OCRv3 recognition via ONNX Runtime (docling's `rapidocr` kind).
    #[default]
    PpOcr,
    /// The `tesseract` CLI (docling's `tesseract` kind).
    Tesseract,
}

impl OcrEngine {
    /// Parse an engine id: `ppocr` (also `pp-ocr`, `rapidocr`, docling's
    /// kind name) or `tesseract` (also `tesseract_cli`, `tesserocr`),
    /// trimmed and case-insensitive. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ppocr" | "pp-ocr" | "pp_ocr" | "rapidocr" | "onnx" | "default" => Some(Self::PpOcr),
            "tesseract" | "tesseract_cli" | "tesseract-cli" | "tesserocr" => Some(Self::Tesseract),
            _ => None,
        }
    }

    /// The process-level choice from `DOCLING_RS_OCR_ENGINE` (empty/unset →
    /// PP-OCR; unknown values warn and use PP-OCR).
    pub fn from_env() -> Self {
        let Some(raw) = docling_core::env::nonempty("DOCLING_RS_OCR_ENGINE") else {
            return Self::default();
        };
        Self::parse(&raw).unwrap_or_else(|| {
            eprintln!(
                "docling-pdf: DOCLING_RS_OCR_ENGINE={raw:?} is not ppocr|tesseract; using ppocr"
            );
            Self::default()
        })
    }

    /// The accepted spellings, for error messages and docs.
    pub const ACCEPTED: &'static str = "ppocr | tesseract";

    /// Whether `raw` is an `ocr_lang` this engine can act on — what the
    /// option surfaces validate up front: the en/ch model switch (or a
    /// BCP-47 tag for either, [`OcrLang::parse`]) under PP-OCR; tessdata
    /// stems and BCP-47 tags ([`crate::tesseract::lang_arg`]) under
    /// Tesseract. The `Err` says what is accepted.
    pub fn validate_lang(self, raw: &str) -> Result<(), String> {
        match self {
            Self::PpOcr => OcrLang::parse(raw).map(|_| ()).ok_or_else(|| {
                format!(
                    "ocr_lang {raw:?} names no language the OCR models read ({})",
                    OcrLang::ACCEPTED
                )
            }),
            Self::Tesseract => crate::tesseract::lang_arg(raw).map(|_| ()),
        }
    }
}

/// The process-level OCR render scale from `DOCLING_RS_OCR_SCALE` (#254,
/// upstream docling#3877's `OcrOptions.scale`): pixels per PDF point fed to
/// the recognizer. Unset/empty → `None` (OCR reads the pipeline's own page
/// render, 2.0 px/pt); non-positive or unparsable values warn and are ignored.
pub fn scale_from_env() -> Option<f32> {
    positive_scale_env("DOCLING_RS_OCR_SCALE")
}

/// The process-level picture OCR scale from `DOCLING_RS_PICTURE_OCR_SCALE`
/// (#666): pixels per picture pixel for the picture-OCR enrichment, before
/// RapidOCR's 2000 px cap — see [`crate::Pipeline::picture_ocr_scale`].
/// Unset/empty → `None`; non-positive or unparsable values warn and are
/// ignored.
pub fn picture_scale_from_env() -> Option<f32> {
    positive_scale_env("DOCLING_RS_PICTURE_OCR_SCALE")
}

fn positive_scale_env(name: &str) -> Option<f32> {
    let raw = docling_core::env::nonempty(name)?;
    match raw.parse::<f32>() {
        Ok(s) if s > 0.0 && s.is_finite() => Some(s),
        _ => {
            eprintln!("docling-pdf: {name}={raw:?} is not a positive number; ignored");
            None
        }
    }
}

/// The process-level default of [`crate::Pipeline::ocr_textline_orientation`]
/// from `DOCLING_RS_OCR_TEXTLINE_ORIENTATION` (#666; a `docling_core::env`
/// flag). Off when unset.
pub fn textline_orientation_from_env() -> bool {
    docling_core::env::flag("DOCLING_RS_OCR_TEXTLINE_ORIENTATION")
}

/// Whether the text detector's boxes are the recognizer's line source inside
/// layout regions (#570; `DOCLING_RS_OCR_LINES`: `det` default, `projection`
/// = the ink-projection strips alone, the pre-#570 behavior). Cached.
pub fn det_lines() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        let raw = docling_core::env::nonempty("DOCLING_RS_OCR_LINES").unwrap_or_default();
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "det" | "detector" | "auto" => true,
            "projection" | "proj" | "off" => false,
            _ => {
                eprintln!(
                    "docling-pdf: DOCLING_RS_OCR_LINES={raw:?} is not det|projection; using det"
                );
                true
            }
        }
    })
}

/// Resolve the recognition model + dictionary pair for `lang`. An English
/// default that isn't on disk (older model checkouts) degrades to the `ch_`
/// pair with a warning rather than failing — the usual missing-optional-asset
/// convention. Explicit `DOCLING_OCR_REC_ONNX` / `DOCLING_OCR_DICT` paths win
/// over all of this; they are a pair, so set both together.
pub(crate) fn resolve_rec_pair(lang: OcrLang) -> (String, String) {
    const CH: (&str, &str) = (".models/ocr_rec.onnx", ".models/ppocr_keys_v1.txt");
    const EN: (&str, &str) = (".models/ocr_rec_en.onnx", ".models/en_dict.txt");
    const V6: (&str, &str) = (".models/ocr_rec_v6.onnx", ".models/ocr_rec_v6_dict.txt");
    let exists = |p: &str| std::path::Path::new(p).exists();
    // The multilingual PP-OCRv6 recognizer, when installed, serves both
    // languages (see `OcrLang`); explicit paths below still win.
    let (v6_rec, v6_dict) = (crate::resolve_asset(V6.0), crate::resolve_asset(V6.1));
    let (mut rec, mut dict) = if exists(&v6_rec) && exists(&v6_dict) {
        (v6_rec, v6_dict)
    } else {
        let pick = if lang == OcrLang::Ch { CH } else { EN };
        (crate::resolve_asset(pick.0), crate::resolve_asset(pick.1))
    };
    let want_ch = lang == OcrLang::Ch;
    if !want_ch && (!exists(&rec) || !exists(&dict)) {
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
        docling_core::env::nonempty("DOCLING_OCR_REC_ONNX").unwrap_or(rec),
        docling_core::env::nonempty("DOCLING_OCR_DICT").unwrap_or(dict),
    )
}

/// One recognised line: its text and mean emitted-character confidence.
type Recognized = (String, f32);

/// One crop read by [`OcrModel::recognize_crops`] /
/// [`OcrModel::recognize_det_boxes`] (#939).
#[derive(Debug, Clone, PartialEq)]
pub struct RecognizedLine {
    /// The CTC-decoded text, trimmed of surrounding whitespace (as the page
    /// OCR trims its cells); empty when nothing decoded.
    pub text: String,
    /// The *recognition* confidence: the mean probability of the emitted
    /// characters (PaddleOCR's per-line score — what the page OCR reports on
    /// its cells and `ConfidenceReport::ocr_cells`), `0.0` when nothing
    /// decoded. Not the detector's [`DetBox::score`](crate::ocr_det::DetBox).
    pub confidence: f32,
}

impl RecognizedLine {
    /// Whether the page OCR would keep this line: non-empty text at or above
    /// RapidOCR's `text_score` ([`crate::ocr_prep::text_score`], 0.5,
    /// `DOCLING_RS_OCR_TEXT_SCORE`). The crop API itself drops nothing.
    pub fn passes_text_score(&self) -> bool {
        !self.text.is_empty() && self.confidence >= crate::ocr_prep::text_score()
    }
}

impl OcrModel {
    /// Load the recognition model and its character dictionary for `lang` —
    /// see `resolve_rec_pair` for the selection rules (explicit
    /// `DOCLING_OCR_REC_ONNX`/`DOCLING_OCR_DICT` paths win) — with `lanes`
    /// recognition sessions.
    ///
    /// Each session is pinned to one intra-op thread: ORT's multi-threaded
    /// float-reduction order varies across runs, which flips the CTC argmax on
    /// low-confidence characters (e.g. noisy faxes) and makes the snapshot
    /// output non-deterministic. Recognition is linear in line width (~0.17 ms
    /// per pixel column on one core) and on a scanned page it, plus the
    /// orientation probe that reads the six widest lines, is ~35% of the wall
    /// time while the other cores idle. Lines are independent, so `lanes`
    /// sessions recognise disjoint same-width batches concurrently — each
    /// line still sees exactly the single-thread kernel path, results are
    /// placed by index, and the output is byte-identical to one lane.
    /// `DOCLING_RS_OCR_SESSIONS` overrides the caller's lane count; under a
    /// CUDA-class provider the default is one lane (#842) — the lanes exist
    /// to use idle CPU cores, and on one GPU each is another session's arena.
    pub fn load_with(lang: OcrLang, lanes: usize) -> Result<Self, String> {
        let (rec_path, dict_path) = resolve_rec_pair(lang);
        let lanes = docling_core::env::parse::<usize>("DOCLING_RS_OCR_SESSIONS")
            .filter(|&n| n > 0)
            .unwrap_or(if docling_onnx::prefers_single_worker() {
                1
            } else {
                lanes
            })
            .clamp(1, 8);
        let open = || -> Result<Session, String> {
            let builder = docling_onnx::session_builder()
                .map_err(|e| format!("ocr: builder: {e}"))?
                .with_intra_threads(1)
                .map_err(|e| format!("ocr: intra_threads: {e}"))?;
            let builder = docling_onnx::apply(builder).map_err(|e| format!("ocr: {e}"))?;
            docling_onnx::commit(builder, &rec_path, "rec")
                .map_err(|e| format!("ocr: load {rec_path}: {e}"))
        };
        // The lanes are independent sessions over the same file — open them
        // concurrently so extra lanes cost no extra start-up latency.
        let recs: Vec<Session> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..lanes).map(|_| s.spawn(open)).collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .map_err(|_| "ocr: session thread panicked".to_string())?
                })
                .collect::<Result<Vec<_>, String>>()
        })?;
        let dict = std::fs::read_to_string(&dict_path)
            .map_err(|e| format!("ocr: read dict {dict_path}: {e}"))?;
        Ok(Self {
            recs,
            chars: dict_chars(&dict),
            orient: false,
            cls: ClsSlot::Unloaded,
        })
    }

    /// Turn RapidOCR's line orientation handling on or off (#666): oriented
    /// crops, the 90° turn, and the 0/180 classifier on detector lines.
    pub fn set_textline_orientation(&mut self, enable: bool) {
        self.orient = enable;
    }

    /// The orientation classifier, loading it on first use; `None` (warned
    /// once) when the model is not installed or fails to load.
    fn cls(&mut self) -> Option<&mut crate::ocr_cls::TextlineCls> {
        if matches!(self.cls, ClsSlot::Unloaded) {
            self.cls = match crate::ocr_cls::TextlineCls::load() {
                Ok(m) => ClsSlot::Ready(m),
                Err(e) => {
                    static WARNED: std::sync::Once = std::sync::Once::new();
                    WARNED.call_once(|| {
                        eprintln!(
                            "warning: {e}; text lines are cropped and turned upright but not \
                             checked for 180° (run scripts/install/download_dependencies.sh)"
                        );
                    });
                    ClsSlot::Missing
                }
            };
        }
        match &mut self.cls {
            ClsSlot::Ready(m) => Some(m),
            _ => None,
        }
    }

    /// Turn the crops the orientation classifier reads as upside down by
    /// 180°, in place; a no-op without the classifier (warned once).
    fn upright(&mut self, mut crops: Vec<&mut RgbImage>) -> Result<(), String> {
        if crops.is_empty() {
            return Ok(());
        }
        let Some(cls) = self.cls() else {
            return Ok(());
        };
        let refs: Vec<&RgbImage> = crops.iter().map(|c| &**c).collect();
        let flip = crate::timing::timed("ocr.cls", || cls.upside_down(&refs))?;
        for (c, flip) in crops.iter_mut().zip(flip) {
            if flip {
                image::imageops::rotate180_in_place(*c);
            }
        }
        Ok(())
    }

    /// The oriented line prep (#666): RapidOCR's crops, the upside-down ones
    /// turned 180° by the classifier when it is installed, then prepared
    /// for recognition like any other line.
    fn oriented_lines(
        &mut self,
        img: &RgbImage,
        regions: &[Region],
        scale: f32,
        detected: &[crate::ocr_det::DetBox],
    ) -> Result<(Vec<crate::ocr_prep::LineBox>, Vec<PrepLine>), String> {
        use crate::ocr_prep::LineSrc;
        let (bboxes, mut srcs) =
            crate::ocr_prep::region_line_crops_det(img, regions, scale, detected);
        let crops: Vec<&mut RgbImage> = srcs
            .iter_mut()
            .filter_map(|s| match s {
                LineSrc::Crop(c) => Some(c),
                LineSrc::Prepared(_) => None,
            })
            .collect();
        self.upright(crops)?;
        let mut boxes = Vec::with_capacity(bboxes.len());
        let mut lines = Vec::with_capacity(bboxes.len());
        for (bb, src) in bboxes.into_iter().zip(srcs) {
            let line = match src {
                LineSrc::Crop(c) => crate::ocr_prep::prep_line(&c),
                LineSrc::Prepared(p) => Some(p),
            };
            if let Some(line) = line {
                boxes.push(bb);
                lines.push(line);
            }
        }
        Ok((boxes, lines))
    }

    /// Recognize standalone line crops (#939) — no layout model, no page
    /// assembly, no detector: each image is taken to be one upright text
    /// line (dark ink on light ground, as the recognizer was trained),
    /// resized to the recognizer's 48 px height at its aspect ratio (width
    /// clamped to 8 px–[`crate::ocr_prep::REC_MAX_WIDTH`]) and read with the same deterministic
    /// width-batching and recognition lanes as the page OCR — so a crop
    /// yields what the page OCR would read from the same pixels.
    ///
    /// Returns exactly one [`RecognizedLine`] per crop, in input order; a
    /// zero-sized crop reads as empty text with confidence `0.0`. Nothing is
    /// filtered — the page OCR's `text_score` drop is
    /// [`RecognizedLine::passes_text_score`], left to the caller — and no
    /// orientation handling runs here (see [`Self::recognize_det_boxes`]).
    pub fn recognize_crops(&mut self, crops: &[RgbImage]) -> Result<Vec<RecognizedLine>, String> {
        let prepped: Vec<Option<PrepLine>> = crops.iter().map(crate::ocr_prep::prep_line).collect();
        self.recognize_prepped(prepped)
    }

    /// Recognize the lines a text detector found on `img` (#939) — the
    /// boxes [`DetModel::detect`](crate::ocr_det::DetModel) returns, in
    /// `img` pixels — without running page layout. Each box is cropped as
    /// the page OCR crops it ([`crate::ocr_prep::det_box_crop`]): RapidOCR's
    /// warp of the box's quad, upright out of the detector's rotated
    /// rectangle (tall lines turned 90°; #981) and, with
    /// [`Self::set_textline_orientation`] on, a 180° turn where the
    /// orientation classifier reads it upside down.
    ///
    /// One [`RecognizedLine`] per box, in input order (so `boxes[i]` is the
    /// geometry of result `i`, in `img` pixels); a box with nothing inside
    /// the image reads as empty with confidence `0.0`. Nothing is filtered,
    /// as in [`Self::recognize_crops`].
    pub fn recognize_det_boxes(
        &mut self,
        img: &RgbImage,
        boxes: &[crate::ocr_det::DetBox],
    ) -> Result<Vec<RecognizedLine>, String> {
        let oriented = self.orient;
        let mut crops: Vec<Option<RgbImage>> = boxes
            .iter()
            .map(|d| crate::ocr_prep::det_box_crop(img, d))
            .collect();
        if oriented {
            self.upright(crops.iter_mut().flatten().collect())?;
        }
        let prepped = crops
            .iter()
            .map(|c| c.as_ref().and_then(crate::ocr_prep::prep_line))
            .collect();
        self.recognize_prepped(prepped)
    }

    /// Recognize the prepared lines, keeping a slot (empty, `0.0`) for each
    /// `None`, so the results stay index-aligned with the caller's input.
    fn recognize_prepped(
        &mut self,
        prepped: Vec<Option<PrepLine>>,
    ) -> Result<Vec<RecognizedLine>, String> {
        let mut slots = Vec::with_capacity(prepped.len());
        let mut lines = Vec::with_capacity(prepped.len());
        for p in prepped {
            slots.push(p.as_ref().map(|_| lines.len()));
            lines.extend(p);
        }
        let mut read = vec![(String::new(), 0.0f32); lines.len()];
        crate::timing::timed("ocr.rec", || -> Result<(), String> {
            for (i, r) in self.recognize_all(&lines)? {
                read[i] = r;
            }
            Ok(())
        })?;
        Ok(slots
            .into_iter()
            .map(|slot| match slot {
                Some(i) => {
                    let (text, confidence) = std::mem::take(&mut read[i]);
                    RecognizedLine {
                        text: text.trim().to_string(),
                        confidence,
                    }
                }
                None => RecognizedLine {
                    text: String::new(),
                    confidence: 0.0,
                },
            })
            .collect())
    }

    /// Recognise every width batch of `lines`, dealt round-robin across the
    /// lanes, and return `(line index, (text, confidence))` in batch order —
    /// the same order the sequential loop produced, whatever the scheduling.
    fn recognize_all(&mut self, lines: &[PrepLine]) -> Result<Vec<(usize, Recognized)>, String> {
        let batches = width_batches(lines);
        let lanes = self.recs.len().min(batches.len()).max(1);
        let chars = &self.chars;
        // One result slot per batch keeps the merge order independent of
        // which lane finished first.
        let mut per_batch: Vec<Option<Result<Vec<Recognized>, String>>> =
            (0..batches.len()).map(|_| None).collect();
        if lanes <= 1 {
            for (slot, (w, chunk)) in per_batch.iter_mut().zip(&batches) {
                *slot = Some(recognize_batch(&mut self.recs[0], chars, *w, chunk, lines));
            }
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = self
                    .recs
                    .iter_mut()
                    .take(lanes)
                    .enumerate()
                    .map(|(lane, rec)| {
                        let batches = &batches;
                        s.spawn(move || {
                            batches
                                .iter()
                                .enumerate()
                                .filter(|(k, _)| k % lanes == lane)
                                .map(|(k, (w, chunk))| {
                                    (k, recognize_batch(rec, chars, *w, chunk, lines))
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                for h in handles {
                    for (k, r) in h.join().expect("ocr lane panicked") {
                        per_batch[k] = Some(r);
                    }
                }
            });
        }
        let mut out = Vec::with_capacity(lines.len());
        for ((_, chunk), slot) in batches.iter().zip(per_batch) {
            let texts = slot.expect("every batch is assigned a lane")?;
            out.extend(chunk.iter().copied().zip(texts));
        }
        Ok(out)
    }

    /// Recognise a batch of prepared *same-width* lines in one session run.
    ///
    /// Only equal widths ever share a run: same-width batching is
    /// bit-identical to one-at-a-time recognition (each sample keeps its own
    /// data and per-sample kernel reduction order — verified empirically on
    /// the scanned corpus), whereas width-padding leaks into the real
    /// timesteps through the model's global-attention blocks and measurably
    /// changes low-confidence characters.
    /// Recognize `lines` and reduce to orientation-probe evidence: the
    /// confidence-weighted character count `Σ(conf × chars)` plus the raw
    /// character total (#225). Same deterministic width-batching as page OCR.
    pub(crate) fn score_lines(&mut self, lines: &[PrepLine]) -> Result<(f32, usize), String> {
        // Same accumulation order as the sequential loop (batch order), so
        // the f32 sum is bit-identical regardless of lane scheduling.
        let mut weighted = 0.0f32;
        let mut chars = 0usize;
        for (_, (text, conf)) in self.recognize_all(lines)? {
            let n = text.trim().chars().count();
            weighted += conf * n as f32;
            chars += n;
        }
        Ok((weighted, chars))
    }

    /// OCR a page: produce text cells (page points) for every line found inside
    /// the text regions, each paired with its recognition confidence (mean
    /// emitted-character probability — feeds the page `ocr_score`, #183).
    /// `scale` is image-px per page-point. `detected` — the text detector's
    /// boxes, in image pixels of `img` — makes them the line source inside
    /// the regions (#570, see [`prep_region_lines_det`]); `None` keeps the
    /// projection segmentation.
    pub fn ocr_page_with(
        &mut self,
        img: &RgbImage,
        regions: &[Region],
        scale: f32,
        detected: Option<&[crate::ocr_det::DetBox]>,
    ) -> Result<Vec<(TextCell, f32)>, String> {
        // Gather every line crop on the page first (shared with the browser
        // path), so equal-width lines can share a recognition run regardless
        // of which region they came from.
        let (bboxes, lines) = match detected {
            Some(det) if self.orient => self.oriented_lines(img, regions, scale, det)?,
            _ => crate::timing::timed("ocr.prep", || match detected {
                Some(det) => prep_region_lines_det(img, regions, scale, det),
                None => prep_region_lines(img, regions, scale),
            }),
        };

        // Deterministic width-batching (shared with the wasm path), dealt
        // across the recognition lanes.
        let mut texts = vec![(String::new(), 0.0f32); lines.len()];
        crate::timing::timed("ocr.rec", || -> Result<(), String> {
            for (i, text) in self.recognize_all(&lines)? {
                texts[i] = text;
            }
            Ok(())
        })?;

        // Emit cells in page order, exactly as the sequential walk did.
        Ok(collect_cells(bboxes, texts))
    }

    /// Recognize the *word* crops inside the page's table regions (mirroring
    /// the browser scanned path): [`ocr_page_with`](Self::ocr_page_with) deliberately
    /// skips table labels, so a scanned table would otherwise reach the cell
    /// matcher with no words at all and dissolve (#173). Returns word-level
    /// [`TextCell`]s in page points.
    pub fn ocr_table_words(
        &mut self,
        img: &RgbImage,
        regions: &[Region],
        scale: f32,
        detected: Option<&[crate::ocr_det::DetBox]>,
    ) -> Result<Vec<(TextCell, f32)>, String> {
        let (bboxes, lines) = match detected {
            Some(det) => prep_table_words_det(img, regions, scale, det),
            None => prep_table_words(img, regions, scale),
        };
        let mut texts = vec![(String::new(), 0.0f32); lines.len()];
        for (i, text) in self.recognize_all(&lines)? {
            texts[i] = text;
        }
        Ok(collect_cells(bboxes, texts))
    }
}

/// Pair recognized texts with their line boxes into page-point cells, in page
/// order, dropping empty lines and those under [`text_score`].
fn collect_cells(
    bboxes: Vec<crate::ocr_prep::LineBox>,
    texts: Vec<Recognized>,
) -> Vec<(TextCell, f32)> {
    let min_conf = crate::ocr_prep::text_score();
    let mut cells = Vec::new();
    for ((l, t, r, b), (text, conf)) in bboxes.into_iter().zip(texts) {
        let text = text.trim().to_string();
        if text.is_empty() || conf < min_conf {
            continue;
        }
        cells.push((TextCell { text, l, t, r, b }, conf));
    }
    cells
}

/// Recognise a batch of prepared *same-width* lines in one run of `rec`.
///
/// Only equal widths ever share a run: same-width batching is bit-identical
/// to one-at-a-time recognition (each sample keeps its own data and per-sample
/// kernel reduction order — verified empirically on the scanned corpus),
/// whereas width-padding leaks into the real timesteps through the model's
/// global-attention blocks and measurably changes low-confidence characters.
fn recognize_batch(
    rec: &mut Session,
    chars: &[String],
    w: usize,
    chunk: &[usize],
    lines: &[PrepLine],
) -> Result<Vec<(String, f32)>, String> {
    let n = chunk.len();
    let data = batch_input(w, chunk, lines);
    let input = Tensor::from_array(([n, 3, REC_HEIGHT as usize, w], data))
        .map_err(|e| format!("ocr: input tensor: {e}"))?;
    let outputs = docling_onnx::run(rec, ort::inputs!["x" => input])
        .map_err(|e| format!("ocr: rec inference: {e}"))?;
    let (shape, probs) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| format!("ocr: extract rec: {e}"))?;
    let t_len = shape[1] as usize;
    let nc = shape[2] as usize;
    Ok((0..n)
        .map(|i| decode_row_scored(chars, &probs[i * t_len * nc..(i + 1) * t_len * nc], nc))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #889: under a Tesseract default, `DOCLING_RS_OCR_LANG` is Tesseract's
    /// language list — a value naming no en/ch model picks English without
    /// the PP-OCR warning; under PP-OCR an en/ch value still selects it.
    #[test]
    fn env_lang_under_a_tesseract_default_is_not_a_model_choice() {
        let tess = || OcrEngine::Tesseract;
        let pp = || OcrEngine::PpOcr;
        assert_eq!(OcrLang::from_env_value(Some("rus+eng"), tess), OcrLang::En);
        assert_eq!(OcrLang::from_env_value(Some("rus+eng"), pp), OcrLang::En);
        assert_eq!(OcrLang::from_env_value(Some("zh-Hans"), tess), OcrLang::Ch);
        assert_eq!(OcrLang::from_env_value(None, pp), OcrLang::En);
    }

    /// #388: BCP-47 tags and the ISO 639-2/3 codes for English and Chinese
    /// resolve to the two recognizers, with or without docling's `iso:`
    /// prefix and whatever the script/region subtags; other languages and
    /// nonsense stay `None`.
    #[test]
    fn ocr_lang_accepts_bcp47_tags_for_the_two_recognizers() {
        for id in [
            "en",
            "EN",
            " en ",
            "en-US",
            "en_GB",
            "eng",
            "english",
            "iso:en",
            "ISO:en-GB",
            "en-Latn-US",
        ] {
            assert_eq!(OcrLang::parse(id), Some(OcrLang::En), "{id:?}");
        }
        for id in [
            "ch",
            "zh",
            "zho",
            "chi",
            "cmn",
            "chinese",
            "ch_sim",
            "ch_tra",
            "chinese_cht",
            "zh-Hans",
            "zh-Hant",
            "zh-CN",
            "zh-TW",
            "zh-Hant-HK",
            "zh_SG",
            "iso:zh-Hans",
        ] {
            assert_eq!(OcrLang::parse(id), Some(OcrLang::Ch), "{id:?}");
        }
        for id in [
            "", "de", "fr-FR", "ja", "deu", "cn", "latin", "iso:", "iso:und", "e",
        ] {
            assert_eq!(OcrLang::parse(id), None, "{id:?}");
        }
    }

    /// #254: docling's four `OcrMode` ids parse; `full_page`/`layout_regions`
    /// reduce to the force-full-page machinery, the default/pdf-aware pair to
    /// the standard text-layer-aware path. Unknown ids parse to nothing.
    #[test]
    fn ocr_mode_ids_parse_and_map_to_forcing() {
        for (id, mode, forces) in [
            ("default", OcrMode::Default, false),
            ("full_page", OcrMode::FullPage, true),
            ("layout_regions", OcrMode::LayoutRegions, true),
            (
                "pdf_aware_layout_regions",
                OcrMode::PdfAwareLayoutRegions,
                false,
            ),
        ] {
            assert_eq!(OcrMode::parse(id), Some(mode));
            assert_eq!(mode.forces_full_page(), forces, "{id}");
        }
        assert_eq!(OcrMode::parse(" Full_Page "), Some(OcrMode::FullPage));
        assert_eq!(OcrMode::parse("easyocr"), None);
        assert_eq!(OcrMode::parse(""), None);
    }

    /// #460: the engine ids, and engine-aware `ocr_lang` validation — `deu`
    /// is a Tesseract stem, not a PP-OCR model; `en` works under both.
    #[test]
    fn ocr_engine_ids_and_lang_validation() {
        for id in ["ppocr", "PP-OCR", " rapidocr ", "default"] {
            assert_eq!(OcrEngine::parse(id), Some(OcrEngine::PpOcr), "{id:?}");
        }
        for id in ["tesseract", "Tesseract_CLI", "tesserocr"] {
            assert_eq!(OcrEngine::parse(id), Some(OcrEngine::Tesseract), "{id:?}");
        }
        assert_eq!(OcrEngine::parse("easyocr"), None);
        assert!(OcrEngine::PpOcr.validate_lang("en").is_ok());
        assert!(OcrEngine::PpOcr.validate_lang("deu").is_err());
        assert!(OcrEngine::Tesseract.validate_lang("en").is_ok());
        assert!(OcrEngine::Tesseract.validate_lang("deu+fra").is_ok());
        assert!(OcrEngine::Tesseract.validate_lang("xx").is_err());
    }
}
