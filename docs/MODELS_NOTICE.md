# Third-party model notice

`docling.rs`'s pipelines use third-party models of two kinds: ONNX graphs
that are **format conversions of docling-project's own PyTorch models** (not
weights docling.rs trains or modifies), and a few **re-hosted, unmodified
public releases** of models docling itself builds on (OCR, ASR, the chunk
tokenizer). All are licensed separately from docling.rs's own MIT code (see
[`LICENSE`](../LICENSE)) under their upstream terms. This notice is hosted in
the models release as `MODELS_NOTICE.md`, beside the digest-bound manifest
`models.json` and the licence texts `LICENSE-<SPDX>.txt`, and every downloader
keeps the three next to the files it fetches (see "Records" below).

| Model | Release assets (an ONNX graph + its `.onnx.data` external-weights sidecar are one artifact) | Source and upstream revision | License |
|---|---|---|---|
| RT-DETR layout model | `layout_heron.onnx` (→ `.models/layout_heron.onnx`); `layout_heron_int8.onnx`, a post-training static INT8 quantization of it | [`docling-project/docling-layout-heron`](https://huggingface.co/docling-project/docling-layout-heron) @ `8f39ad3c0b4c58e9c2d2c84a38465abf757272d8`, exported by `scripts/install/export_layout.py` | Apache-2.0 |
| TableFormer (`accurate`) | `encoder.onnx` (+ `encoder.onnx.data` when the export needs it); `decoder.onnx` + `decoder.onnx.data`; `decoder_kv.onnx` + `decoder_kv_layers.onnx` + `decoder_kv.onnx.data` (both graphs read that one sidecar); `bbox.onnx` + `bbox.onnx.data` (→ `.models/tableformer/`). `encoder_fp16.onnx` is the encoder with fp16-stored weights, `decoder_int8.onnx` / `decoder_kv_int8.onnx` (+ `.data`) post-training dynamic INT8 quantizations of the decoders | [`docling-project/docling-models`](https://huggingface.co/docling-project/docling-models) (`model_artifacts/tableformer/accurate`) @ `2199320848bb9a8a519d22e4b528185a4f9a6f64`, exported by `scripts/install/export_tableformer.py` | CDLA-Permissive-2.0 and Apache-2.0 (upstream lists both) |
| DocumentFigureClassifier-v2.5 | `picture_classifier.onnx` — upstream's own ONNX graph, re-hosted unmodified | [`docling-project/DocumentFigureClassifier-v2.5`](https://huggingface.co/docling-project/DocumentFigureClassifier-v2.5) @ `f859dfbff5c9916cd996942d4b0db7fa25808220` (`model.onnx`) | MIT (declared in the model card; upstream ships no LICENSE file, so the SPDX text is bundled) |
| CodeFormulaV2 | `cf_vision.onnx` + `cf_embed.onnx` + `cf_decoder_kv.onnx` + `cf_tokenizer.json` (→ `.models/code_formula/`); `cf_decoder_kv_int8.onnx` is a post-training dynamic INT8 quantization of the decoder | [`docling-project/CodeFormulaV2`](https://huggingface.co/docling-project/CodeFormulaV2) @ `ecedbe111d15c2dc60bfd4a823cbe80127b58af4`, exported by `scripts/install/export_code_formula.py` (the tokenizer re-hosted unmodified) | CDLA-Permissive-2.0 (declared in the model card) |
| PP-OCRv3 recognition model + dictionary, multilingual | `ocr_rec.onnx` + `ppocr_keys_v1.txt` | [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)'s `ch_PP-OCRv3_rec_infer`, taken as the ready ONNX conversion re-hosted by [`SWHL/RapidOCR`](https://huggingface.co/SWHL/RapidOCR) @ `1cfba2e90fc938db55889873735088de210cc173` (unmodified); the dictionary is PaddleOCR's own `ppocr/utils/ppocr_keys_v1.txt` | Apache-2.0 |
| PP-OCRv3 recognition model + dictionary, English | `ocr_rec_en.onnx` + `en_dict.txt` | The same pair for PaddleOCR's `en_PP-OCRv3_rec_infer`, from the same `SWHL/RapidOCR` revision, with the dictionary PaddleOCR's `ppocr/utils/en_dict.txt`; both re-hosted unmodified | Apache-2.0 |
| PP-OCRv6 recognition model + dictionary, multilingual | `ocr_rec_v6.onnx` + `ocr_rec_v6_dict.txt` | [RapidOCR](https://github.com/RapidAI/RapidOCR)'s `PP-OCRv6_rec_small.onnx` + `ppocrv6_dict.txt` (the recognizer its default pipeline — and so docling — runs, derived from [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)), re-hosted unmodified from RapidOCR's model hub @ `v3.9.2` (hub sha256 `6f327246…`) | Apache-2.0 |
| PP-OCRv6 text detection model | `ocr_det.onnx` | [RapidOCR](https://github.com/RapidAI/RapidOCR)'s `PP-OCRv6_det_small.onnx` (the DB detector its default pipeline runs, derived from [PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)), re-hosted unmodified from RapidOCR's model hub @ `v3.9.2` (hub sha256 `090f04ab…`) | Apache-2.0 |
| Text-line orientation classifier (#666) | `ocr_textline_ori.onnx` | [RapidOCR](https://github.com/RapidAI/RapidOCR)'s `ch_PP-LCNet_x0_25_textline_ori_cls_mobile.onnx` ([PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR)'s `PP-LCNet_x0_25_textline_ori`), re-hosted unmodified from RapidOCR's model hub @ `v3.9.2` (hub sha256 `54379ae5…`) | Apache-2.0 |
| Whisper tiny | `asr_encoder_model.onnx` + `asr_decoder_model.onnx` + `asr_vocab.json` + `asr_added_tokens.json` (→ `.models/asr/`) | [`onnx-community/whisper-tiny`](https://huggingface.co/onnx-community/whisper-tiny) @ `ff4177021cc41f7db950912b73ea4fdf7d01d8e7`, the community ONNX export of [`openai/whisper-tiny`](https://huggingface.co/openai/whisper-tiny) (re-hosted unmodified; Hugging Face stays the fallback host) | Apache-2.0 |
| Hybrid-chunker tokenizer | `chunk_tokenizer.json` (→ `.models/chunk/tokenizer.json`) | [`sentence-transformers/all-MiniLM-L6-v2`](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2) @ `1110a243fdf4706b3f48f1d95db1a4f5529b4d41` (`tokenizer.json` — a tokenizer definition, no weights) | Apache-2.0 |

Not models, hosted in the same release:

| Asset | Source | License |
|---|---|---|
| `libpdfium.so` — used only by a build with docling-pdf's opt-in `pdfium` feature and its oracle tests; the default pipeline renders in pure Rust | [bblanchon/pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) @ `chromium/8100` (`pdfium-linux-x64.tgz`), a build of Google's PDFium, re-hosted unmodified; the build's own `LICENSE` is the release's `LICENSE-pdfium-binaries.txt`, its dependencies' notices ship inside the upstream tarball | BSD-3-Clause (PDFium) / Apache-2.0 |
| `docling-parse-render-<os>-<arch>.tar.gz` — docling-parse's Blend2D renderer as a `dlopen`-able shim (#478), the conformance scripts' development oracle | [docling-project/docling-parse](https://github.com/docling-project/docling-parse) @ `v7.22.1`, built from source by `.github/workflows/docling-parse-render.yml` | MIT |
| `onnxruntime-linux-s390x.tar.gz` — the ONNX Runtime an `ort-load-dynamic` build `dlopen`s on IBM Z (#504); carries upstream's `LICENSE` | [microsoft/onnxruntime](https://github.com/microsoft/onnxruntime) @ `v1.31.0`, built from source by `.github/workflows/onnxruntime-s390x.yml` | MIT |

`scripts/install/export_layout.py`, `scripts/install/export_tableformer.py` and
`scripts/install/export_code_formula.py` do the conversion (PyTorch → ONNX via
`torch.onnx.export`) at the upstream revision above — pinned in
`scripts/install/models_catalog.json`, which they read; no weights are
retrained, fine-tuned, or otherwise altered. The `*_int8` / `*_fp16` assets
are `scripts/install/quantize_models.py`'s numeric re-encodings of those same
exports. `.github/workflows/publish-models.yml` runs the conversion (and
re-hosts the third-party models a default install needs, fetched at the
catalog's revisions, plus pdfium for the opt-in feature — both OCR pairs,
Whisper tiny, the chunk tokenizer, the picture classifier — alongside it) and
publishes everything as GitHub Release assets on this repo (tag `models-v1`),
fetched by `scripts/install/download_dependencies.sh` — see that script and
`crates/docling-node/deps.js` — purely to spare downstream users the
PyTorch/`transformers`/`docling_ibm_models` toolchain needed to export them
locally. Which toolchain, on which run, from which input bytes — the
execution a source revision and a script name alone do not pin — is
`export_record.json` (below, #955); what upstream published about training
the weights, how the producer of a re-hosted file made it, and what no
public record exists for, is each artifact's `lineage` in `models.json`.

## Records

The release's attribution travels with its bytes (#942):

- `models.json` — the digest-bound manifest `scripts/ci/models_manifest.sh`
  generates from `scripts/install/models_catalog.json` after every upload
  into the release: each artifact's assets with their sha256 and size (a
  graph and its `.onnx.data` sidecar grouped, the sidecar naming the graph it
  belongs to), the upstream repository, revision and files, how the bytes
  were produced (`export`, `quantization`, `repack`, `rehost`, `build`) and
  by which script, and the SPDX licence; `notice` and `licenses` name the
  files below. A release asset the catalog does not cover fails the
  generation, so nothing is hosted unattributed. Each artifact's `lineage`
  (#955) links the upstream model card and the published records of how
  the weights were trained — technical reports, papers, dataset cards —
  and, for a re-hosted file, how its producer made it
  (`upstream_conversion`); `unavailable` names what no public record
  exists for (a training run's logs and checkpoints, a hub's exact
  Paddle→ONNX invocation), stated rather than left to inference. The
  manifest's `execution` names the record below, its digest, the run and
  the commit; it is `null`, with a note, for a release whose assets
  predate the record.
- `export_record.json` — the execution record of the `publish-models.yml`
  run that produced the `export`, `quantization` and `repack` assets and
  fetched the re-hosted ones (`scripts/ci/export_record.py`, #955): the
  run's URL, id, attempt, commit and actor, the runner image and CPU, the
  Python and the version of every package the conversions ran with, the
  sha256 of every upstream input file the exports read (the Hugging Face
  hub's snapshot at the catalog's revision) and of every asset staged. A
  run's logs and artifacts expire; the asset does not. The export steps
  pin their toolchain, so the record also says what the next run must
  install to compare.
- `MODELS_NOTICE.md` — this file.
- `LICENSE-Apache-2.0.txt`, `LICENSE-CDLA-Permissive-2.0.txt`,
  `LICENSE-MIT.txt`, `LICENSE-BSD-3-Clause.txt` — the licence texts
  (`docs/licenses/`), and `LICENSE-pdfium-binaries.txt`, the pdfium build's
  own.

`scripts/install/download_dependencies.sh` / `.bat` and the Python
`download_models()` fetch them beside the models (`.models/models.json`,
`.models/MODELS_NOTICE.md`, `.models/LICENSE-*.txt`), and `SHA256SUMS` lists
them with every other asset.

All the upstream licenses permit redistribution with attribution; this file
and `models.json` are that attribution. See the linked model cards for any
additional upstream terms.
