//! Browser TableFormer — stage 3 of #157. The autoregressive structure loop
//! and every ONNX-free step (448 preprocessing, tag corrections, bbox
//! bookkeeping, span merge, OTSL→grid, cell matching) run in Rust via
//! `docling_pdf::tf_core` — the *same* code the native pipeline uses — and only
//! the three ONNX graphs (encoder / decoder / bbox) are delegated to ONNX
//! Runtime Web through the [`TfSession`] interop object.
//!
//! The heavy tensors never cross the wasm boundary: [`TfSession`] holds the
//! encoder's constant cross-attention K/V and `enc_out`, and the growing
//! decoder KV-cache, entirely on the JS side. Each decode step sends only the
//! last tag (one int) and gets back `logits` + `hidden` (525 floats). See
//! `www/scan.html` for the JS wiring (the `decoder_kv` graph: N_LAYERS=6,
//! KV_HEADS=8, head_dim=64, cross length 784).

use docling_pdf::pdfium_backend::TextCell;
use docling_pdf::tf_core::{
    argmax, build_table_cells, merge_spans, preprocess_input, table_rows, BboxBook, TableCell,
    MAX_STEPS,
};
use image::RgbImage;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// The JS-side TableFormer session: a stateful wrapper around the three
    /// `ort.InferenceSession`s. `encode` runs the image encoder and stashes the
    /// constant cross tensors + `enc_out` and resets the KV-cache; `step` runs
    /// one decoder step (feeding the stored cross + growing cache) and returns
    /// `{ logits: Float32Array, hidden: Float32Array }`; `bbox` runs the bbox
    /// decoder over the collected per-cell hidden states and returns
    /// `{ boxes: Float32Array, classes: Float32Array }`.
    pub type TfSession;

    #[wasm_bindgen(method, catch)]
    async fn encode(this: &TfSession, image: js_sys::Float32Array) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    async fn step(this: &TfSession, tag: i32) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    async fn bbox(
        this: &TfSession,
        tag_h: js_sys::Float32Array,
        n: u32,
    ) -> Result<JsValue, JsValue>;
}

/// Pull a named `Float32Array` out of a JS result object.
fn get_f32(obj: &JsValue, key: &str) -> Result<Vec<f32>, JsError> {
    let v = js_sys::Reflect::get(obj, &JsValue::from_str(key))
        .map_err(|_| JsError::new(&format!("tf result has no `{key}`")))?;
    let arr: js_sys::Float32Array = v
        .dyn_into()
        .map_err(|_| JsError::new(&format!("tf `{key}` is not a Float32Array")))?;
    Ok(arr.to_vec())
}

/// Run the full structure model on a `SIDE×SIDE`-croppable region image: encode
/// once, step the decoder to the OTSL sequence (`tf_core::BboxBook` drives the
/// exact bbox bookkeeping), run the bbox decoder, then merge spans and lay the
/// cells onto the grid — all shared with native.
async fn predict_structure(
    session: &TfSession,
    crop: &RgbImage,
) -> Result<Vec<TableCell>, JsError> {
    let input = preprocess_input(crop);
    session
        .encode(js_sys::Float32Array::from(input.as_slice()))
        .await
        .map_err(|e| JsError::new(&format!("tf encode: {e:?}")))?;

    let mut book = BboxBook::new();
    while book.otsl.len() < MAX_STEPS {
        let last = *book.tags.last().expect("decode starts from <start>");
        let out = session
            .step(last as i32)
            .await
            .map_err(|e| JsError::new(&format!("tf decode step: {e:?}")))?;
        let logits = get_f32(&out, "logits")?;
        let hidden = get_f32(&out, "hidden")?;
        let raw = argmax(&logits) as i64;
        if !book.step(raw, &hidden) {
            break;
        }
    }
    if book.n == 0 {
        return Ok(Vec::new());
    }

    let out = session
        .bbox(
            js_sys::Float32Array::from(book.hiddens.as_slice()),
            book.n as u32,
        )
        .await
        .map_err(|e| JsError::new(&format!("tf bbox: {e:?}")))?;
    let boxes: Vec<[f32; 4]> = get_f32(&out, "boxes")?
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect();
    let classes: Vec<i64> = get_f32(&out, "classes")?
        .chunks_exact(3)
        .map(|c| argmax(c) as i64)
        .collect();
    let (merged, merged_classes) = merge_spans(&boxes, &classes, &book.merge);
    Ok(build_table_cells(&book.otsl, &merged, &merged_classes))
}

/// Predict a table region's grid, the browser counterpart of the native
/// `TableFormer::predict_table_rows`: docling's page→1024px box-average + crop
/// (`docling_pdf::resample`), the ONNX structure model, then the shared word
/// matcher (`tf_core::table_rows`). `page_image` is the rendered page (2 px per
/// point, like the native pipeline); `region` is `(l, t, r, b)` in page points.
/// `None` when no structure is predicted.
pub(crate) async fn predict_table_rows(
    session: &TfSession,
    page_image: &RgbImage,
    region: [f32; 4],
    words: &[TextCell],
) -> Option<Vec<Vec<String>>> {
    // page → 1024px height (cv2.INTER_AREA), then crop the table bbox — docling's
    // coordinate chain with the same rounding the native path reproduces.
    let sf = 1024.0 / page_image.height() as f32;
    let pw = (page_image.width() as f32 * sf) as u32;
    let page1024 = docling_pdf::resample::inter_area(page_image, pw, 1024);
    let k = 2.0 * 1024.0 / page_image.height() as f64;
    let px = |v: f32| (v as f64).round_ties_even() * k;
    let x = (px(region[0]).round_ties_even()).max(0.0) as u32;
    let y = (px(region[1]).round_ties_even()).max(0.0) as u32;
    let x2 = (px(region[2]).round_ties_even() as u32).min(page1024.width());
    let y2 = (px(region[3]).round_ties_even() as u32).min(page1024.height());
    if x2 <= x || y2 <= y {
        return None;
    }
    let crop = image::imageops::crop_imm(&page1024, x, y, x2 - x, y2 - y).to_image();
    let cells = predict_structure(session, &crop).await.ok()?;
    if cells.is_empty() {
        return None;
    }
    table_rows(&cells, region, words)
}
