# Third-party model notice

`docling.rs`'s PDF/image pipeline uses two ONNX graphs that are **format
conversions of docling-project's own PyTorch models**, not weights docling.rs
trains or modifies. They're licensed separately from docling.rs's own MIT
code (see [`LICENSE`](./LICENSE)) under their upstream terms:

| Model | Source | License |
|---|---|---|
| RT-DETR layout model (`layout_heron.onnx`) | [`docling-project/docling-layout-heron`](https://huggingface.co/docling-project/docling-layout-heron) | Apache-2.0 |
| TableFormer (`tableformer/{encoder,decoder,bbox}.onnx`) | [`docling-project/docling-models`](https://huggingface.co/docling-project/docling-models) (`model_artifacts/tableformer/accurate`) | CDLA-Permissive-2.0 / Apache-2.0 |

`scripts/export_layout.py` and `scripts/export_tableformer.py` do the
conversion (PyTorch → ONNX via `torch.onnx.export`); no weights are retrained,
fine-tuned, or otherwise altered. `.github/workflows/publish-models.yml` runs
that conversion (and re-hosts pdfium + the OCR model alongside it) and
publishes everything as GitHub Release assets on this repo (tag `models-v1`),
fetched by `scripts/download_dependencies.sh` — see that script and
`crates/docling-node/deps.js` — purely to spare downstream users the
PyTorch/`transformers`/`docling_ibm_models` toolchain needed to export them
locally.

Both upstream licenses permit redistribution with attribution; this file is
that attribution. See the linked model cards for the full license text and any
additional upstream terms.
