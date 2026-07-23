# docling.rs-cuda

CUDA (NVIDIA GPU) build of [docling.rs](https://www.npmjs.com/package/docling.rs)
for Node.js / Bun — **Linux x64 only**. Same JavaScript API, GPU-accelerated
PDF/image ML pipeline (RT-DETR layout, TableFormer, OCR): measured 1.5–2.1× on
multi-page digital PDFs and up to 8.7× on very large documents vs CPU.

The npm tarball is a few KB of JavaScript; the CUDA native addon and the ONNX
Runtime CUDA provider libraries (hundreds of MB — past npm's practical size
limits) are downloaded by a `postinstall` script from this project's GitHub
release that matches the package version, and verified against the release's
sha256 manifest.

```bash
npm install docling.rs-cuda
# or keep `require('docling.rs')` working unchanged via an npm alias:
npm install docling.rs@npm:docling.rs-cuda
```

Requirements:

- Linux x64, glibc ≥ 2.38 (Ubuntu 24.04+ / Debian 13+ era — inherited from the
  CUDA ONNX Runtime binaries, same floor as the `docling-rs-cuda` Python wheel)
- CUDA 12 + cuDNN 9 installed on the system (the download ships the ONNX
  Runtime provider, not the CUDA toolkit)
- network access to `github.com` at install time — or set
  `DOCLING_RS_NPM_CUDA_URL` to a mirror base URL / local directory holding the
  same release assets (air-gapped installs)

A GPU build defaults to `DOCLING_RS_EP=auto`: GPU when one is usable, CPU
fallback when not. `DOCLING_RS_EP=cuda` forces the GPU (fail loudly),
`DOCLING_RS_EP=cpu` forces CPU. Everything else — API, model downloads, docs —
is identical to [docling.rs](https://www.npmjs.com/package/docling.rs); see its
README.
