// Shared browser scanned-document pipeline (stage 2 of #157), DOM-free so it
// runs identically on the main thread (fallback) or inside a Web Worker
// (worker.js — the default, keeps the UI responsive). It owns every import
// (ORT Web, pdf.js, the wasm glue) and delegates only two host concerns:
//   onStatus(msg, spinning) — progress reporting (DOM span vs postMessage)
//   makeCanvas()            — an HTMLCanvasElement or an OffscreenCanvas
// Same code as the native pipeline behind the wasm boundary; drift can only
// come from the ORT kernels.

import * as ort from "https://cdn.jsdelivr.net/npm/onnxruntime-web/dist/ort.min.mjs";
import * as pdfjs from "https://cdn.jsdelivr.net/npm/pdfjs-dist/build/pdf.min.mjs";
import init, { ScannedConverter, convert_scanned_image } from "./pkg/docling_wasm.js";

pdfjs.GlobalWorkerOptions.workerSrc =
  "https://cdn.jsdelivr.net/npm/pdfjs-dist/build/pdf.worker.min.mjs";

// Multi-threaded wasm when cross-origin isolated (coi.js); else one thread.
// SIMD is auto-selected by the ORT bundle when the browser supports it.
ort.env.wasm.numThreads = self.crossOriginIsolated
  ? Math.min(navigator.hardwareConcurrency || 4, 8)
  : 1;
export const THREADS = ort.env.wasm.numThreads;

// Same-origin candidates (release assets carry no CORS header — see scan.html's
// setup note); int8 preferred, fp32 fallback.
const LAYOUT_PATHS = ["./models/layout_heron_int8.onnx", "./models/layout_heron.onnx"];
const SCALE = 2.0; // px per PDF point — the native pipeline's RENDER_SCALE

const REC_MODELS = {
  en: {
    model: "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/en_PP-OCRv3_rec_infer.onnx",
    dict: "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/en_dict.txt",
  },
  cyrillic: {
    // PP-OCRv5: markedly better Cyrillic accuracy than the v3 export (spaces
    // and case survive); its dictionary only exists inside the repo's
    // inference.yml, so a flattened copy ships next to this page.
    model: "https://huggingface.co/PaddlePaddle/cyrillic_PP-OCRv5_mobile_rec_onnx/resolve/main/inference.onnx",
    dict: "cyrillic_v5_dict.txt",
  },
  ch: {
    model: "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx",
    dict: "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt",
  },
};

export function createPipeline({ onStatus, makeCanvas }) {
  const status = (msg, spinning = true) => onStatus && onStatus(msg, spinning);

  // fetch() with a live "x / y MB" progress line.
  async function fetchProgress(url, label) {
    const resp = await fetch(url, { cache: "force-cache" });
    if (!resp.ok) throw new Error(`${label}: HTTP ${resp.status}`);
    const total = Number(resp.headers.get("Content-Length")) || 0;
    if (!resp.body) return resp.arrayBuffer();
    const reader = resp.body.getReader();
    const chunks = [];
    let got = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      got += value.length;
      const mb = (got / 1048576).toFixed(1);
      status(total ? `${label} — ${mb} / ${(total / 1048576).toFixed(1)} MB` : `${label} — ${mb} MB`, true);
    }
    const buf = new Uint8Array(got);
    let off = 0;
    for (const c of chunks) { buf.set(c, off); off += c.length; }
    return buf.buffer;
  }

  const recCache = {};
  async function recFor(lang) {
    if (!recCache[lang]) {
      const [model, dict] = await Promise.all([
        fetchProgress(REC_MODELS[lang].model, `${lang} recognition model`),
        fetch(REC_MODELS[lang].dict, { cache: "force-cache" }).then((r) => r.text()),
      ]);
      status(`starting ${lang} recognition session …`, true);
      const session = await ort.InferenceSession.create(model, {
        executionProviders: ["wasm"],
        logSeverityLevel: 3,
      });
      recCache[lang] = {
        dict,
        rec: {
          run: async (n, h, w, data) => {
            const results = await session.run({
              [session.inputNames[0]]: new ort.Tensor("float32", data, [n, 3, h, w]),
            });
            const t = results[session.outputNames[0]];
            return { data: t.data, dims: Array.from(t.dims) };
          },
        },
      };
    }
    return recCache[lang];
  }

  // Interop wrapper docling_wasm expects (see src/scanned.rs).
  let layout = null;
  let layoutKind = null;
  async function loadLayout() {
    for (const path of LAYOUT_PATHS) {
      try {
        const buf = await fetchProgress(path, "layout model (first load only)");
        status("starting layout session …", true);
        const session = await ort.InferenceSession.create(buf, {
          executionProviders: ["wasm"],
          logSeverityLevel: 3,
        });
        layoutKind = path.includes("int8") ? "int8" : "fp32";
        layout = {
          run: async (data) => {
            const results = await session.run({
              pixel_values: new ort.Tensor("float32", data, [1, 3, 640, 640]),
            });
            const t = (n) => ({ data: results[n].data, dims: Array.from(results[n].dims) });
            return { logits: t("logits"), boxes: t("pred_boxes") };
          },
        };
        return layoutKind;
      } catch (e) {
        // 404 / decode failure → try the next candidate.
      }
    }
    return null;
  }

  // Bring wasm + the layout model up. Returns "int8" | "fp32" | null.
  async function boot() {
    status("loading wasm module …", true);
    await init();
    return loadLayout();
  }

  // Run one blank inference through layout + rec so ORT's lazy kernel/thread
  // init happens now, not inside the first real document.
  async function warmup(lang) {
    try {
      await layout.run(new Float32Array(3 * 640 * 640));
      const { rec } = await recFor(lang);
      await rec.run(1, 48, 320, new Float32Array(3 * 48 * 320));
    } catch (e) {
      // warm-up is best-effort; a failure just means the first page pays it.
    }
  }

  async function handlePdf(bytes, name, dict, rec) {
    const pdf = await pdfjs.getDocument({ data: bytes }).promise;
    const conv = new ScannedConverter(dict);
    // Rasterize page p+1 while converting page p; getImageData hands back an
    // independent buffer, so one canvas ping-pongs safely across pages.
    const canvas = makeCanvas();
    const ctx = canvas.getContext("2d", { willReadFrequently: true });
    async function render(p) {
      const page = await pdf.getPage(p);
      const viewport = page.getViewport({ scale: SCALE });
      canvas.width = viewport.width;
      canvas.height = viewport.height;
      await page.render({ canvasContext: ctx, viewport }).promise;
      const img = ctx.getImageData(0, 0, canvas.width, canvas.height);
      return { rgba: new Uint8Array(img.data.buffer), w: canvas.width, h: canvas.height };
    }

    const t0 = performance.now();
    let elapsed = 0;
    let next = render(1);
    for (let p = 1; p <= pdf.numPages; p++) {
      const done = p - 1;
      const avg = done ? ` — ${(elapsed / done / 1000).toFixed(1)}s/page avg` : "";
      status(`${name}: page ${p}/${pdf.numPages}${avg} …`, true);
      const cur = await next;
      if (p < pdf.numPages) next = render(p + 1);
      const tp = performance.now();
      await conv.add_page(cur.rgba, cur.w, cur.h, SCALE, layout, rec);
      const dt = performance.now() - tp;
      elapsed += dt;
      console.log(`${name}: page ${p}/${pdf.numPages} — ${(dt / 1000).toFixed(1)}s convert`);
    }
    const wall = (performance.now() - t0) / 1000;
    const avg = pdf.numPages ? wall / pdf.numPages : 0;
    console.log(`${name}: ${pdf.numPages} pages in ${wall.toFixed(1)}s wall (${avg.toFixed(1)}s/page)`);
    return conv.finish(name, "md");
  }

  async function convert(bytes, name, lang) {
    const { dict, rec } = await recFor(lang);
    const u8 = new Uint8Array(bytes);
    if (name.toLowerCase().endsWith(".pdf")) return handlePdf(u8, name, dict, rec);
    return convert_scanned_image(u8, name, dict, layout, rec, "md");
  }

  return { boot, warmup, recFor, convert, get layoutKind() { return layoutKind; } };
}
