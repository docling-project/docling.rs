# Third-party model notice

`docling.rs`'s pipelines use third-party models of two kinds: ONNX graphs
that are **format conversions of docling-project's own PyTorch models** (not
weights docling.rs trains or modifies), and a few **re-hosted, unmodified
public releases** of models docling itself builds on (OCR, ASR, the chunk
tokenizer). All are licensed separately from docling.rs's own MIT code (see
[`LICENSE`](../LICENSE)) under their upstream terms:

| Model | Source | License |
|---|---|---|
| RT-DETR layout model (`layout_heron.onnx`) | [`docling-project/docling-layout-heron`](https://huggingface.co/docling-project/docling-layout-heron) | Apache-2.0 |
| TableFormer (`tableformer/{encoder,decoder,bbox}.onnx`) | [`docling-project/docling-models`](https://huggingface.co/docling-project/docling-models) (`model_artifacts/tableformer/accurate`) | CDLA-Permissive-2.0 / Apache-2.0 |
| DocumentFigureClassifier (`picture_classifier.onnx`) | [`docling-project/DocumentFigureClassifier-v2.5`](https://huggingface.co/docling-project/DocumentFigureClassifier-v2.5) (upstream's own ONNX, re-hosted unmodified) | Apache-2.0 |
| CodeFormulaV2 (`cf_{vision,embed,decoder_kv}.onnx` + `cf_tokenizer.json`; `cf_decoder_kv_int8.onnx` is a post-training quantization of the same export) | [`docling-project/CodeFormulaV2`](https://huggingface.co/docling-project/CodeFormulaV2) | Apache-2.0 |
| PP-OCRv3 recognition model + dictionary (`ocr_rec.onnx`, `ppocr_keys_v1.txt`) | [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)'s `ch_PP-OCRv3_rec_infer`, taken as the ready ONNX conversion re-hosted by [`SWHL/RapidOCR`](https://huggingface.co/SWHL/RapidOCR) (unmodified) | Apache-2.0 |
| Whisper tiny (`asr/{encoder_model,decoder_model}.onnx`, `vocab.json`) | [`onnx-community/whisper-tiny`](https://huggingface.co/onnx-community/whisper-tiny), the community ONNX export of [`openai/whisper-tiny`](https://huggingface.co/openai/whisper-tiny) (fetched directly, unmodified) | Apache-2.0 |
| Hybrid-chunker tokenizer (`chunk/tokenizer.json`) | [`sentence-transformers/all-MiniLM-L6-v2`](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2)'s `tokenizer.json` (a tokenizer definition, no weights) | Apache-2.0 |

(The `pdfium` shared library re-hosted alongside the models is not a model but
carries its own terms: [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)
builds of Google's PDFium, BSD-3-Clause / Apache-2.0.)

`scripts/install/export_layout.py`, `scripts/install/export_tableformer.py` and
`scripts/install/export_code_formula.py` do the conversion (PyTorch → ONNX via
`torch.onnx.export`); no weights are retrained, fine-tuned, or otherwise
altered. `.github/workflows/publish-models.yml` runs
that conversion (and re-hosts pdfium + the OCR model alongside it) and
publishes everything as GitHub Release assets on this repo (tag `models-v1`),
fetched by `scripts/install/download_dependencies.sh` — see that script and
`crates/docling-node/deps.js` — purely to spare downstream users the
PyTorch/`transformers`/`docling_ibm_models` toolchain needed to export them
locally.

All the upstream licenses permit redistribution with attribution; this file is
that attribution. See the linked model cards for the full license text and any
additional upstream terms.
