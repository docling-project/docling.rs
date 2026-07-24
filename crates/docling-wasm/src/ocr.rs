//! Browser OCR — stage 1 of #157 (`#79 phase 2: ML pipeline in the browser`).
//!
//! Scanned **images** convert fully client-side: all pre/post-processing
//! (line segmentation, crop preparation, width-batching, CTC decoding) runs
//! in Rust via `docling_pdf::ocr_prep` — the *same* code the native pipeline
//! uses — and only the PP-OCRv3 recognition inference is delegated to
//! [ONNX Runtime Web](https://onnxruntime.ai/docs/tutorials/web/) on the JS
//! side through the [`RecSession`] interop interface. One shared
//! implementation is what keeps the wasm output comparable to the native CPU
//! path; drift can then only come from the runtime's kernels.
//!
//! Without a layout model (that's stage 2), the whole image is treated as a
//! single text region and split into lines by the ink-projection profile —
//! fine for typical single-column scans, degraded on complex layouts.
//!
//! ```js
//! import * as ort from "onnxruntime-web";
//! import init, { ocr_image } from "./pkg/docling_wasm.js";
//! await init();
//! const session = await ort.InferenceSession.create("ocr_rec_en.onnx");
//! const dict = await (await fetch("en_dict.txt")).text();
//! const md = await ocr_image(imageBytes, dict, {
//!   run: async (n, h, w, data) => {
//!     const out = (await session.run({ x: new ort.Tensor("float32", data, [n, 3, h, w]) })).softmax_2.data
//!       ?? Object.values(await session.run({ x: new ort.Tensor("float32", data, [n, 3, h, w]) }))[0];
//!     // return the output tensor: { data: Float32Array, dims: [n, t, c] }
//!   },
//! });
//! ```
//! (see `www/ocr.html` for the complete wiring, including output-name
//! discovery and model/dict caching.)

use docling_core::{DoclingDocument, Node};
use docling_pdf::ocr_prep::{
    batch_input, decode_row, dict_chars, prep_page_lines, width_batches, REC_HEIGHT,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// The JS-side recognition session: a wrapper object around an
    /// `ort.InferenceSession` exposing one method,
    /// `run(count, height, width, data)`, which feeds the `(N,3,H,W)` CHW
    /// float buffer to the model and resolves to
    /// `{ data: Float32Array, dims: [n, t, c] }` — the recognition
    /// probabilities tensor.
    pub type RecSession;

    #[wasm_bindgen(method, catch)]
    async fn run(
        this: &RecSession,
        count: u32,
        height: u32,
        width: u32,
        data: js_sys::Float32Array,
    ) -> Result<JsValue, JsValue>;
}

/// OCR a scanned image entirely in the browser: `bytes` is the image file
/// (PNG/JPEG/…), `dict` the recognition dictionary text (`en_dict.txt` for
/// the default English model), `session` the JS inference wrapper. Returns
/// Markdown (default) or docling JSON per `to`, one paragraph per recognized
/// line.
#[wasm_bindgen]
pub async fn ocr_image(
    bytes: &[u8],
    dict: &str,
    session: &RecSession,
    to: Option<String>,
) -> Result<String, JsError> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| JsError::new(&format!("decode image: {e}")))?
        .to_rgb8();
    let lines = prep_page_lines(&img);
    let chars = dict_chars(dict);
    let mut texts = vec![String::new(); lines.len()];
    for (w, chunk) in width_batches(&lines) {
        let input = batch_input(w, &chunk, &lines);
        let out = session
            .run(
                chunk.len() as u32,
                REC_HEIGHT,
                w as u32,
                js_sys::Float32Array::from(input.as_slice()),
            )
            .await
            .map_err(|e| JsError::new(&format!("session.run: {e:?}")))?;
        let (probs, t_len, nc) = tensor_parts(&out)?;
        if probs.len() < chunk.len() * t_len * nc {
            return Err(JsError::new("session.run returned a short tensor"));
        }
        for (i, &ix) in chunk.iter().enumerate() {
            texts[ix] = decode_row(&chars, &probs[i * t_len * nc..(i + 1) * t_len * nc], nc);
        }
    }
    let mut doc = DoclingDocument::new("image");
    doc.nodes.extend(
        texts
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .map(|text| Node::Paragraph { text }),
    );
    match to.as_deref().unwrap_or("md") {
        "md" | "markdown" => Ok(doc.export_to_markdown()),
        "json" => Ok(doc.export_to_json()),
        other => Err(JsError::new(&format!(
            "unknown output format {other:?} (expected \"md\" or \"json\")"
        ))),
    }
}

/// Pull `{ data: Float32Array, dims: [n, t, c] }` out of the JS result.
fn tensor_parts(out: &JsValue) -> Result<(Vec<f32>, usize, usize), JsError> {
    let get = |k: &str| {
        js_sys::Reflect::get(out, &JsValue::from_str(k))
            .map_err(|_| JsError::new(&format!("session.run result has no `{k}`")))
    };
    let data: js_sys::Float32Array = get("data")?
        .dyn_into()
        .map_err(|_| JsError::new("`data` is not a Float32Array"))?;
    let dims: js_sys::Array = get("dims")?
        .dyn_into()
        .map_err(|_| JsError::new("`dims` is not an array"))?;
    if dims.length() != 3 {
        return Err(JsError::new("`dims` must be [n, t, c]"));
    }
    let t_len = dims.get(1).as_f64().unwrap_or(0.0) as usize;
    let nc = dims.get(2).as_f64().unwrap_or(0.0) as usize;
    if t_len == 0 || nc == 0 {
        return Err(JsError::new("`dims` must be [n, t, c] with t, c > 0"));
    }
    Ok((data.to_vec(), t_len, nc))
}
