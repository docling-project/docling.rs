// Cross-origin isolation shim, so ONNX Runtime Web can spin up multi-threaded
// wasm (SharedArrayBuffer needs `crossOriginIsolated`, which needs the page to
// be served with COOP+COEP). A plain static file server (python -m http.server,
// GitHub Pages) can't set those headers, so a service worker re-serves every
// response with them added — the standard "coi-serviceworker" trick.
//
// COEP is `credentialless` (not `require-corp`): cross-origin subresources —
// the jsDelivr ORT/pdf.js bundles and the Hugging Face recognition model —
// then load without needing their own CORP header, while the page still counts
// as cross-origin isolated. Browsers that don't support credentialless simply
// stay non-isolated and ORT falls back to a single thread (the page still
// works, just slower).
//
// One-liner to enable: <script src="coi.js"></script> before the module script.

if (typeof window === "undefined") {
  // --- service-worker context -----------------------------------------------
  self.addEventListener("install", () => self.skipWaiting());
  self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

  self.addEventListener("fetch", (event) => {
    const req = event.request;
    // `only-if-cached` is only valid with same-origin `no-cors`; leave it alone.
    if (req.cache === "only-if-cached" && req.mode !== "same-origin") return;

    event.respondWith(
      fetch(req)
        .then((res) => {
          if (res.status === 0) return res; // opaque response — pass through
          const headers = new Headers(res.headers);
          headers.set("Cross-Origin-Embedder-Policy", "credentialless");
          headers.set("Cross-Origin-Opener-Policy", "same-origin");
          return new Response(res.body, {
            status: res.status,
            statusText: res.statusText,
            headers,
          });
        })
        .catch((e) => console.error(e)),
    );
  });
} else {
  // --- page context: register self, reload once to gain isolation -----------
  (async () => {
    if (window.crossOriginIsolated) return; // already isolated — nothing to do
    if (!window.isSecureContext || !navigator.serviceWorker) return; // can't
    const reg = await navigator.serviceWorker
      .register(document.currentScript.src)
      .catch(() => null);
    if (!reg) return;
    // A fresh registration doesn't control this load yet; reload so the SW can
    // stamp COOP/COEP on the document and crossOriginIsolated flips true.
    if (!navigator.serviceWorker.controller) window.location.reload();
  })();
}
