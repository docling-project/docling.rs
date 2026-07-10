//! Layout detection via the RT-DETR (`docling-layout-heron`) model exported to
//! ONNX, run with `ort`. A port of docling-ibm-models' `LayoutPredictor`:
//! resize the page image to 640×640 and rescale to `[0,1]` (the heron processor
//! has `do_normalize=false`), run the model, then RT-DETR
//! `post_process_object_detection` (sigmoid → top-k over query×class →
//! center-to-corners boxes scaled to the page).

use image::imageops::FilterType;
use image::RgbImage;
use ort::session::Session;
use ort::value::Tensor;

/// The 17 canonical layout classes, indexed by the model's class id
/// (`config.json` `id2label`).
pub const LABELS: [&str; 17] = [
    "caption",
    "footnote",
    "formula",
    "list_item",
    "page_footer",
    "page_header",
    "picture",
    "section_header",
    "table",
    "text",
    "title",
    "document_index",
    "code",
    "checkbox_selected",
    "checkbox_unselected",
    "form",
    "key_value_region",
];

/// One detected region, in page points (top-left origin).
#[derive(Debug, Clone)]
pub struct Region {
    pub label: &'static str,
    pub score: f32,
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

/// Base confidence threshold (docling-ibm-models `base_threshold`): the raw
/// RT-DETR floor before docling's `LayoutPostprocessor` applies its stricter
/// per-label thresholds ([`label_threshold`]).
const THRESHOLD: f32 = 0.3;
const SIDE: u32 = 640;

/// Per-label confidence threshold, ported from docling's
/// `LayoutPostprocessor.CONFIDENCE_THRESHOLDS`. The raw predictor keeps every
/// detection above the 0.3 base; the postprocessor then drops a cluster whose
/// score is below its label's threshold. Applying it here (equivalent, since
/// every per-label threshold is ≥ the 0.3 base) keeps low-confidence pictures /
/// tables / list-items out of the assembly, matching docling.
pub fn label_threshold(label: &str) -> f32 {
    match label {
        "section_header" | "title" | "code" | "checkbox_selected"
        | "checkbox_unselected" | "form" | "key_value_region" | "document_index" => 0.45,
        // caption, footnote, formula, list_item, page_footer, page_header,
        // picture, table, text — all 0.5 in docling.
        _ => 0.5,
    }
}

pub struct LayoutModel {
    session: Session,
}

impl LayoutModel {
    /// Load the ONNX model from `DOCLING_LAYOUT_ONNX`. Without the override,
    /// prefers `models/layout_heron_int8.onnx` when present (the quantized
    /// default; `DOCLING_RS_FP32=1` opts out), else `models/layout_heron.onnx`.
    pub fn load() -> Result<Self, String> {
        Self::load_with(crate::intra_threads())
    }

    /// Like [`load`](Self::load) but with an explicit intra-op thread count. A
    /// parallel page-worker pool loads its helper models on a single thread each
    /// and gets its speed-up from running pages concurrently instead.
    pub fn load_with(intra: usize) -> Result<Self, String> {
        let path = crate::model_path(
            "DOCLING_LAYOUT_ONNX",
            "models/layout_heron.onnx",
            "models/layout_heron_int8.onnx",
        );
        let session = Session::builder()
            .map_err(|e| format!("layout: builder: {e}"))?
            // Let inference use the available cores (ort otherwise defaults low);
            // a large PDF runs this model once per page.
            .with_intra_threads(intra)
            .map_err(|e| format!("layout: intra_threads: {e}"))?
            .commit_from_file(&path)
            .map_err(|e| format!("layout: load {path}: {e}"))?;
        Ok(Self { session })
    }

    /// Detect layout regions on a page image. `page_w`/`page_h` are the page size
    /// in points; returned boxes are in those coordinates.
    pub fn predict(
        &mut self,
        img: &RgbImage,
        page_w: f32,
        page_h: f32,
    ) -> Result<Vec<Region>, String> {
        // Resize to 640×640 (RT-DETR ignores aspect ratio), rescale to [0,1],
        // lay out as CHW.
        let resized = image::imageops::resize(img, SIDE, SIDE, FilterType::Triangle);
        let n = (SIDE * SIDE) as usize;
        let mut data = vec![0f32; 3 * n];
        for (i, px) in resized.pixels().enumerate() {
            data[i] = px[0] as f32 / 255.0;
            data[n + i] = px[1] as f32 / 255.0;
            data[2 * n + i] = px[2] as f32 / 255.0;
        }
        let input = Tensor::from_array(([1usize, 3, SIDE as usize, SIDE as usize], data))
            .map_err(|e| format!("layout: input tensor: {e}"))?;
        let outputs = self
            .session
            .run(ort::inputs!["pixel_values" => input])
            .map_err(|e| format!("layout: inference: {e}"))?;
        let (lshape, logits) = outputs["logits"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("layout: extract logits: {e}"))?;
        let (_, boxes) = outputs["pred_boxes"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("layout: extract boxes: {e}"))?;

        let num_queries = lshape[1] as usize;
        let num_classes = lshape[2] as usize;

        // sigmoid over every (query, class); take the top `num_queries` scores.
        let mut scored: Vec<(f32, usize)> = (0..num_queries * num_classes)
            .map(|idx| (sigmoid(logits[idx]), idx))
            .collect();
        scored.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        scored.truncate(num_queries);

        let mut regions = Vec::new();
        for (score, idx) in scored {
            if score <= THRESHOLD {
                continue;
            }
            let label_id = idx % num_classes;
            let q = idx / num_classes;
            let cx = boxes[q * 4];
            let cy = boxes[q * 4 + 1];
            let w = boxes[q * 4 + 2];
            let h = boxes[q * 4 + 3];
            // center_to_corners, then scale normalized coords to page points.
            let l = (cx - w / 2.0) * page_w;
            let t = (cy - h / 2.0) * page_h;
            let r = (cx + w / 2.0) * page_w;
            let b = (cy + h / 2.0) * page_h;
            regions.push(Region {
                label: LABELS.get(label_id).copied().unwrap_or("text"),
                score,
                l,
                t,
                r,
                b,
            });
        }
        Ok(regions)
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}
