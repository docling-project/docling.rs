// Web Worker host for the scanned-document pipeline (stage 2 of #157). Moving
// the whole run off the main thread keeps the page responsive: pdf.js
// rasterization (OffscreenCanvas here) and the wasm pre/post-processing that
// runs synchronously between ORT calls no longer block clicks or scrolling.
//
// Protocol — main → worker: {type:"boot", lang} | {type:"convert", bytes, name,
// lang} | {type:"rec", lang}. Worker → main: {type:"up"} | {type:"status", msg,
// spinning} | {type:"ready", kind, threads} | {type:"no-layout"} |
// {type:"result", md, name} | {type:"rec-ready", lang} | {type:"error", msg}.

import { createPipeline, THREADS } from "./pipeline.js";

const post = (type, extra) => self.postMessage({ type, ...extra });

const pipeline = createPipeline({
  onStatus: (msg, spinning) => post("status", { msg, spinning }),
  makeCanvas: () => new OffscreenCanvas(1, 1),
});

self.onmessage = async (e) => {
  const m = e.data;
  try {
    if (m.type === "boot") {
      const kind = await pipeline.boot();
      if (!kind) return post("no-layout");
      await pipeline.recFor(m.lang);
      await pipeline.warmup(m.lang);
      post("ready", { kind, threads: THREADS });
    } else if (m.type === "rec") {
      await pipeline.recFor(m.lang);
      await pipeline.warmup(m.lang);
      post("rec-ready", { lang: m.lang });
    } else if (m.type === "convert") {
      const md = await pipeline.convert(m.bytes, m.name, m.lang);
      post("result", { md, name: m.name });
    }
  } catch (err) {
    post("error", { msg: String((err && err.message) || err) });
  }
};

post("up");
