// wasm-side OCR pipeline (stage 2 of #157), used inside worker.js (default) or
// on the main thread (fallback). It owns ONLY the wasm work — model loading,
// per-page `add_page`, image decode. Rasterization stays on the MAIN thread
// (pdf.js + a real HTMLCanvas, which produces correct pixels and uses pdf.js's
// own worker); pages arrive here as ready RGBA buffers. Keeping pdf.js off the
// Web Worker avoids its "fake worker" fallback, which garbled the line crops.
//
// DOM-free: the only host concern is onStatus(msg, spinning) for progress.

import init, { DigitalConverter, ScannedConverter, convert_scanned_image } from "./pkg/docling_wasm.js";

// ONNX Runtime Web, pinned (#629): an unpinned import follows every release,
// and the build this page used to load (`ort.min.mjs`) is the JSEP one,
// deprecated since 1.29. Two builds of the same version:
//   * ort.wasm.min.mjs   — CPU (wasm) only, the default (3.5 MB gzipped wasm);
//   * ort.webgpu.min.mjs — adds the native WebGPU EP (6.3 MB gzipped).
// Not JSEP: its WebGPU AveragePool lacks `ceil_mode`, so the heron layout
// model fails on it, while the native EP runs it with detections identical to
// wasm (measured on the #629 probe pages).
const ORT_BASE = "https://cdn.jsdelivr.net/npm/onnxruntime-web@1.30.0/dist/";

// The models WebGPU may take, and the ones it takes by default: the
// conv-heavy, fixed-shape graphs. Recognition (batches of varying width) and
// the TableFormer decoder/bbox (a loop of small per-token ops) stay on wasm,
// where GPU dispatch and per-shape shader compiles are expected to cost more
// than they save — to be confirmed on real devices, which is what the
// `?webgpu=layout,rec,…` override and the per-model timings are for.
export const GPU_MODELS = ["layout", "det", "rec", "tf_enc", "tf_dec", "tf_bbox"];
const GPU_DEFAULT = ["layout", "det", "tf_enc"];

/// Parse a WebGPU request: "" / "0" / "off" / "false" → off; "1" / "on" /
/// "true" → the defaults; "all" → every model; else a comma list of
/// GPU_MODELS keys (unknown keys ignored).
export function gpuPlan(req) {
  const v = String(req ?? "").trim().toLowerCase();
  if (!v || ["0", "off", "false", "no"].includes(v)) return new Set();
  if (["1", "on", "true", "yes"].includes(v)) return new Set(GPU_DEFAULT);
  if (v === "all") return new Set(GPU_MODELS);
  return new Set(v.split(",").map((s) => s.trim()).filter((s) => GPU_MODELS.includes(s)));
}

// Load the runtime once per realm: the WebGPU build when the plan wants the
// GPU and an adapter exists, else the wasm build. A realm keeps the build it
// loaded first (two ORT builds must not share one) — the host restarts the
// worker to switch.
let ort = null;
let runtime = null;
async function loadRuntime(plan) {
  if (runtime) return runtime;
  let adapter = null;
  let why = "";
  if (plan.size) {
    try {
      adapter = navigator.gpu ? await navigator.gpu.requestAdapter({ powerPreference: "high-performance" }) : null;
      if (!adapter) why = navigator.gpu ? "no WebGPU adapter" : "no WebGPU in this browser";
    } catch (e) {
      why = `WebGPU adapter request failed: ${(e && e.message) || e}`;
    }
  }
  // The CDN is the one thing OCR fetches that the page does not ship, so a
  // failure here is the likeliest reason OCR will not start — say so. A
  // WebGPU build that will not load still leaves the wasm one to try.
  const load = async (file) => {
    try {
      return await import(ORT_BASE + file);
    } catch (e) {
      throw new Error(
        `could not load ONNX Runtime Web (${ORT_BASE + file}) — OCR needs it. Check the network ` +
          `(a blocker or an offline device will do this); everything else on this page works without it.`,
      );
    }
  };
  if (adapter) {
    try {
      ort = await load("ort.webgpu.min.mjs");
    } catch (e) {
      adapter = null;
      why = "ONNX Runtime's WebGPU build did not load";
    }
  }
  if (!ort) ort = await load("ort.wasm.min.mjs");
  // Multi-threaded wasm when cross-origin isolated (coi.js); else one thread.
  // It matters under WebGPU too: ORT keeps shape arithmetic and any op the
  // EP lacks on the CPU.
  ort.env.wasm.numThreads = self.crossOriginIsolated
    ? Math.min(navigator.hardwareConcurrency || 4, 8)
    : 1;
  const info = adapter && adapter.info ? [adapter.info.vendor, adapter.info.architecture].filter(Boolean).join(" ") : "";
  runtime = {
    threads: ort.env.wasm.numThreads,
    // The WebGPU build is an asyncify build: a call that enters its wasm while
    // another is suspended on a GPU round trip corrupts the suspended stack
    // ("memory access out of bounds", or a hang — measured with an inference
    // overlapping a session creation, which is what the background detector
    // load does). Its calls are serialized; the wasm build has no suspension
    // points and keeps overlapping.
    serial: !!adapter,
    gpu: adapter ? plan : new Set(),
    gpuInfo: info,
    gpuUnavailable: plan.size && !adapter ? why : "",
  };
  return runtime;
}

// Model bases, tried in order after any user-provided file: local ./.models/
// (download_dependencies.sh for local dev), then a CORS-enabled Hugging
// Face mirror so the page works cross-origin (the hosted phone demo on
// raw.githack) — GitHub Release assets carry no CORS header.
const MODEL_BASE = "https://huggingface.co/pivozavrus/docling-rs-models/resolve/main/";

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

// TableFormer graphs: local ./.models/tableformer/ first (download_dependencies.sh),
// then the CORS Hugging Face mirror for the hosted demo. The encoder is
// self-contained; the decoder/bbox use ONNX external data, so their .onnx.data
// sidecar rides along via ort-web's externalData option (path = the location
// stored in the .onnx). Fetched lazily — ~380 MB total — only when a table
// profile is chosen.
const TF_DIRS = ["./.models/tableformer/", MODEL_BASE];

/// The stateful session the wasm TfSession interop expects (see
/// src/tableformer.rs): `encode` runs the image encoder once and stashes the
/// constant cross-attention K/V + enc_out and resets the KV-cache; `step` runs
/// one decoder step feeding the stored cross + growing cache; `bbox` runs the
/// bbox decoder. The heavy tensors stay here — only tags and logits/hidden
/// cross the wasm boundary. Geometry: 6 layers, 8 KV heads, head_dim 64.
/// `run(key, session, feeds)` is the pipeline's timed session.run; the three
/// sessions may sit on different EPs (outputs come back as CPU tensors, so an
/// encoder on WebGPU feeds a decoder on wasm as is).
class JsTfSession {
  constructor(enc, dec, bbox, run) {
    this.run = run;
    this.enc = enc;
    this.dec = dec;
    this.bboxSess = bbox;
    this.cross = null;
    this.encOut = null;
    this.cacheK = null;
    this.cacheV = null;
  }
  async encode(image) {
    const out = await this.run("tf_enc", this.enc, { image: new ort.Tensor("float32", image, [1, 3, 448, 448]) });
    this.cross = {};
    for (let i = 0; i < 6; i++) {
      this.cross["cross_kt_" + i] = out["cross_kt_" + i];
      this.cross["cross_v_" + i] = out["cross_v_" + i];
    }
    this.encOut = out["enc_out"];
    // Empty first-step KV-cache: [N_LAYERS, 1, KV_HEADS, past=0, head_dim].
    this.cacheK = new ort.Tensor("float32", new Float32Array(0), [6, 1, 8, 0, 64]);
    this.cacheV = new ort.Tensor("float32", new Float32Array(0), [6, 1, 8, 0, 64]);
  }
  async step(tag) {
    const out = await this.run("tf_dec", this.dec, {
      tag: new ort.Tensor("int64", BigInt64Array.from([BigInt(tag)]), [1, 1]),
      cache_k: this.cacheK,
      cache_v: this.cacheV,
      ...this.cross,
    });
    this.cacheK = out["out_cache_k"];
    this.cacheV = out["out_cache_v"];
    return { logits: out["logits"].data, hidden: out["hidden"].data };
  }
  async bbox(tagH, n) {
    const out = await this.run("tf_bbox", this.bboxSess, {
      enc_out: this.encOut,
      tag_h: new ort.Tensor("float32", tagH, [n, 512]),
    });
    return { boxes: out["boxes"].data, classes: out["classes"].data };
  }
}

export function createOcr({ onStatus }) {
  const status = (msg, spinning = true) => onStatus && onStatus(msg, spinning);

  // Which execution provider each model actually runs on, and the inference
  // time spent in it — reported back so the GPU and CPU paths can be compared
  // on real devices (#629).
  let plan = new Set();
  const backend = {};
  let stats = {};
  // One ORT call at a time on the WebGPU build (see loadRuntime). Each
  // create/run call queues on its own — never a whole createSession, whose
  // wasm fallback queues again.
  let queue = Promise.resolve();
  function serial(fn) {
    if (!runtime || !runtime.serial) return fn();
    const p = queue.then(fn, fn);
    queue = p.catch(() => {});
    return p;
  }
  // Session creation per model key: WebGPU when the plan has it, else wasm. A
  // WebGPU session that will not start (an op the EP lacks, a lost device, GPU
  // memory) falls back to wasm instead of failing the conversion. `key` null =
  // never on the GPU (the int8 layout graph: CPU-calibrated QDQ, diverges on
  // WebGPU — the native pipeline's fp32-on-GPU rule, #74).
  async function createSession(key, model, opts = {}) {
    const base = { logSeverityLevel: 3, ...opts };
    if (key && plan.has(key)) {
      try {
        const s = await serial(() => ort.InferenceSession.create(model, { ...base, executionProviders: ["webgpu"] }));
        backend[key] = "webgpu";
        return s;
      } catch (e) {
        console.warn(`docling.rs: ${key} could not start on WebGPU (${(e && e.message) || e}); using wasm`);
        backend[key] = "wasm (WebGPU failed)";
      }
    } else if (key) {
      backend[key] = "wasm";
    }
    return serial(() => ort.InferenceSession.create(model, { ...base, executionProviders: ["wasm"] }));
  }
  // session.run with the time charged to `key` (time in the queue excluded).
  async function run(key, session, feeds) {
    let t = 0;
    try {
      return await serial(() => {
        t = performance.now();
        return session.run(feeds);
      });
    } finally {
      const s = (stats[key] ||= { runs: 0, ms: 0 });
      s.runs++;
      s.ms += performance.now() - t;
    }
  }
  // The per-model timings since the last call, with each model's backend.
  function takeStats() {
    const out = {};
    for (const [k, s] of Object.entries(stats)) out[k] = { ...s, ms: Math.round(s.ms), on: backend[k] || "wasm" };
    stats = {};
    return out;
  }

  // Model files the user picked from the device (basename → ArrayBuffer),
  // used instead of any network fetch — see index.html's model picker. Reading a
  // File to an ArrayBuffer is a single allocation, so it also sidesteps the
  // double-buffering peak fetchProgress hits on the 225 MB encoder.
  let provided = {};
  function setProvidedModels(map) {
    provided = map || {};
  }
  // Resolve a model by name: a user-provided file wins, else the first base
  // that serves it. Returns { buf, base } (base is null for a provided file).
  async function fetchModel(name, bases, label) {
    if (provided[name]) return { buf: provided[name], base: null };
    let lastErr = null;
    for (const b of bases) {
      try {
        return { buf: await fetchProgress(b + name, label), base: b };
      } catch (e) {
        lastErr = e;
      }
    }
    throw new Error(`${name} failed: ${(lastErr && lastErr.message) || lastErr}`);
  }

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
      const session = await createSession("rec", model);
      recCache[lang] = {
        dict,
        rec: {
          run: async (n, h, w, data) => {
            const results = await run("rec", session, {
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

  // Text detector (#429): RapidOCR's PP-OCRv6 DB model, the same file the
  // native pipeline installs as .models/ocr_det.onnx. Optional — without it
  // the browser pipeline is recognition-only (text outside layout regions on
  // a bitmap page is lost, as before). Local ./.models/ first, then the model
  // mirror — and nothing else: a third-party host without CORS or with a slow
  // route stalls the fetch for minutes, and the first cut of this loader did
  // exactly that, hanging the demo at "starting en recognition session …"
  // while it waited on RapidOCR's hub. The load also runs in the background
  // from boot and a document waits for it at most briefly (`DET_WAIT_MS`):
  // a detector that is not ready yet is simply not used for that document,
  // and is picked up by the next one.
  const DET_WAIT_MS = 3000;
  let detPromise = null;
  function loadDetector() {
    if (!detPromise) {
      detPromise = (async () => {
        let buf;
        try {
          ({ buf } = await fetchModel("ocr_det.onnx", ["./.models/", MODEL_BASE], "text detector (first load only)"));
        } catch (e) {
          return null; // not provided and not fetchable → recognition-only
        }
        try {
          const session = await createSession("det", buf);
          return {
            run: async (h, w, data) => {
              const results = await run("det", session, {
                [session.inputNames[0]]: new ort.Tensor("float32", data, [1, 3, h, w]),
              });
              const t = results[session.outputNames[0]];
              return { data: t.data, dims: Array.from(t.dims) };
            },
          };
        } catch (e) {
          return null; // a session that will not start → recognition-only
        }
      })();
    }
    return detPromise;
  }
  // The detector if it is ready within `ms`, else null (the load goes on).
  function detectorSoon(ms) {
    return Promise.race([
      loadDetector(),
      new Promise((resolve) => setTimeout(() => resolve(null), ms)),
    ]);
  }

  // Interop wrapper docling_wasm expects (see src/scanned.rs).
  let layout = null;
  let layoutKind = null;
  async function loadLayout() {
    // int8 first on wasm (smaller download, conformance-validated on CPU);
    // fp32 first under WebGPU, where the int8 graph runs but diverges (#629:
    // max logit diff 5–7, most detections relabelled — CPU-calibrated QDQ).
    // A device with only the int8 file still converts: int8 on wasm.
    const gpuLayout = plan.has("layout");
    const candidates = gpuLayout
      ? [["layout_heron.onnx", "fp32"], ["layout_heron_int8.onnx", "int8"]]
      : [["layout_heron_int8.onnx", "int8"], ["layout_heron.onnx", "fp32"]];
    for (const [name, kind] of candidates) {
      let buf;
      try {
        const label = gpuLayout && kind === "fp32" ? "fp32 layout model for WebGPU (first load only)" : "layout model (first load only)";
        ({ buf } = await fetchModel(name, ["./.models/", MODEL_BASE], label));
      } catch (e) {
        continue; // not provided and not fetchable → try the next candidate
      }
      status("starting layout session …", true);
      const session = await createSession(kind === "fp32" ? "layout" : null, buf);
      if (kind === "int8") backend.layout = gpuLayout ? "wasm (int8 never runs on WebGPU)" : "wasm";
      layoutKind = kind;
      layout = {
        run: async (data) => {
          const results = await run("layout", session, {
            pixel_values: new ort.Tensor("float32", data, [1, 3, 640, 640]),
          });
          const t = (n) => ({ data: results[n].data, dims: Array.from(results[n].dims) });
          return { logits: t("logits"), boxes: t("pred_boxes") };
        },
      };
      return layoutKind;
    }
    return null;
  }

  // Bring ONNX Runtime, wasm and the layout model up. `gpu` is the WebGPU
  // request (see gpuPlan). Returns { kind: "int8" | "fp32" | null, threads,
  // gpu: [models WebGPU may take], gpuInfo, gpuUnavailable: reason or "" }.
  async function boot(gpu) {
    status("loading ONNX Runtime …", true);
    const rt = await loadRuntime(gpuPlan(gpu));
    plan = rt.gpu;
    status("loading wasm module …", true);
    await init();
    const kind = await loadLayout();
    // Start fetching the (optional) text detector now, off the critical path.
    loadDetector();
    return { kind, threads: rt.threads, gpu: [...plan], gpuInfo: rt.gpuInfo, gpuUnavailable: rt.gpuUnavailable };
  }

  // One blank inference through layout + rec so ORT's lazy kernel/thread init
  // (and, on WebGPU, the shader compiles) happens now, not inside the first
  // real page. Not counted in the per-model timings.
  async function warmup(lang) {
    try {
      await layout.run(new Float32Array(3 * 640 * 640));
      const { rec } = await recFor(lang);
      await rec.run(1, 48, 320, new Float32Array(3 * 48 * 320));
    } catch (e) {
      // best-effort — a failure just means the first page pays it.
    }
    stats = {};
  }

  // TableFormer sessions, loaded lazily on first use (the encoder alone is
  // ~225 MB, so this only downloads when the table profile is chosen).
  let tf = null;
  async function ensureTf() {
    if (tf) return tf;
    const load = async (name, key, external) => {
      // A provided file wins; else the first base (local, then HF). External
      // data comes from a provided file or the same base the .onnx came from.
      const { buf: model, base } = await fetchModel(name + ".onnx", TF_DIRS, `tableformer ${name}`);
      const opts = {};
      if (external) {
        const { buf: data } = await fetchModel(
          name + ".onnx.data",
          base ? [base] : TF_DIRS,
          `tableformer ${name} data`,
        );
        opts.externalData = [{ path: name + ".onnx.data", data: new Uint8Array(data) }];
      }
      return createSession(key, model, opts);
    };
    const enc = await load("encoder", "tf_enc", false);
    const dec = await load("decoder_kv", "tf_dec", true);
    const bbox = await load("bbox", "tf_bbox", true);
    status("starting tableformer sessions …", true);
    tf = new JsTfSession(enc, dec, bbox, run);
    return tf;
  }

  // Multi-page document lifecycle: startDoc → addPage* → finishDoc. Pages come
  // in as RGBA (already rasterized on the main thread), one document at a time.
  let cur = null;
  async function startDoc(lang, useTf) {
    const { dict, rec } = await recFor(lang);
    const tfSess = useTf ? await ensureTf() : null;
    const conv = new ScannedConverter(dict);
    const detector = await detectorSoon(DET_WAIT_MS);
    if (detector) conv.setDetector(detector);
    cur = { conv, rec, tf: tfSess };
  }

  // A digital PDF (one with a text layer): the text comes out of the file, so
  // this is both much faster and exact. The recognition model still loads —
  // embedded raster pictures (a terms box exported as an image) carry text the
  // text layer cannot see, and docling OCRs those areas on every page; a page
  // with no text layer at all is OCR'd whole, as a scanned one. Throws
  // when there is no text layer — the host then falls back to startDoc.
  // Returns the page count, since the parser already knows it.
  async function startDigital(bytes, useTf, lang) {
    // Probe the text layer first (the constructor throws on a scan, which the
    // host retries through startDoc) — only then fetch the recognition model.
    const conv = new DigitalConverter(new Uint8Array(bytes));
    const { dict, rec } = await recFor(lang || "en");
    conv.setDict(dict);
    // A page without a text layer (a scan bound after typed pages) is OCR'd
    // like a scanned one, with the detector when it has arrived.
    const detector = await detectorSoon(DET_WAIT_MS);
    if (detector) conv.setDetector(detector);
    const tfSess = useTf ? await ensureTf() : null;
    cur = { conv, rec, tf: tfSess, digital: true };
    return conv.page_count();
  }
  async function addPage(rgba, w, h, scale, index) {
    // Guard the single in-flight document: without it a stray addPage/finish
    // after (or before) the lifecycle reads `cur` as null and the host sees an
    // opaque "Cannot read properties of null".
    if (!cur) throw new Error("addPage called with no document open (startDoc first)");
    if (cur.digital) {
      const args = [index, rgba, w, h, scale, layout];
      return cur.tf
        ? cur.conv.addPageTf(...args, cur.tf, cur.rec)
        : cur.conv.add_page(...args, cur.rec);
    }
    if (cur.tf) {
      await cur.conv.addPageTf(rgba, w, h, scale, layout, cur.rec, cur.tf);
    } else {
      await cur.conv.add_page(rgba, w, h, scale, layout, cur.rec);
    }
  }
  function finishDoc(name, to, images) {
    if (!cur) throw new Error("finishDoc called with no document open (startDoc first)");
    const md = cur.conv.finish(name, to || "md", images || "placeholder");
    cur = null;
    return md;
  }

  // Standalone image: the wasm side decodes it (no canvas needed).
  async function convertImage(bytes, name, lang, to, images) {
    const { dict, rec } = await recFor(lang);
    return convert_scanned_image(
      new Uint8Array(bytes), name, dict, layout, rec, to || "md", images || "placeholder",
    );
  }

  return {
    boot, warmup, recFor, startDoc, startDigital, addPage, finishDoc, convertImage, setProvidedModels, takeStats,
    get layoutKind() { return layoutKind; },
  };
}
