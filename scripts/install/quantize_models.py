#!/usr/bin/env python3
"""Quantize the PDF-pipeline ONNX models to INT8 for faster CPU inference.

Two quantizations, both validated against the PDF corpus (see
docs/PDF_CONFORMANCE.md for the measured speed/quality numbers):

* **layout** — static QDQ INT8 of the RT-DETR layout model, **Conv ops only**
  (the HGNetv2 backbone). Calibrated on real corpus pages rendered exactly the
  way `layout.rs::predict` preprocesses them. The transformer decoder and
  detection heads stay fp32: quantizing their MatMuls shifts class scores near
  the 0.3 threshold and visibly degrades output (headers demoted to text,
  page-footers leaking in), while conv-only keeps groundtruth conformance at
  fp32 level. ~2.4x faster layout inference on AVX-512-VNNI CPUs, 172 -> 68 MB.

* **tableformer-decoder** — dynamic INT8 (weights-only MatMul) of the
  legacy autoregressive tag decoder (~10% faster than its fp32 file,
  78 -> 50 MB). Byte-exactness is quantizer-environment-sensitive:
  redp5110's TOC decode has near-tie tokens that a re-quantization can
  flip (the currently shipped asset is corpus-exact; a fresh one drifted
  that single fixture) — always re-gate pdf_conformance.sh after
  re-quantizing and keep the previously validated asset if it drifts.
  Since #97 the Rust loop prefers the byte-exact fp32 decoder_kv over
  this file anyway, so it only serves setups without the KV export.

* **code-formula-decoder** — dynamic INT8 of the CodeFormulaV2 KV-cache
  decoder step (the enrichment VLM; needs the --enrich models). Not in the
  default target list because the fp32 export is opt-in too. ~655 -> ~165 MB
  (4x less decoder RAM). NEAR-exact, not byte-exact: greedy decoding has
  occasional near-tie tokens the weight rounding can flip - on the
  conformance fixture the only drift is one extra blank line inside the
  code block (per-channel and fp32-lm_head variants flip it identically,
  so per-tensor is kept for the smaller file). DOCLING_RS_FP32=1 restores
  the byte-exact fp32 decoder.

Usage (from the repo root, models fetched by scripts/install/download_dependencies.sh):

    uv venv .venv-quant && uv pip install --python .venv-quant/bin/python \
        onnx onnxruntime sympy pypdfium2 pillow numpy
    .venv-quant/bin/python scripts/install/quantize_models.py layout tableformer-decoder

Then point the pipeline at the quantized files:

    export DOCLING_LAYOUT_ONNX=$PWD/models/layout_heron_int8.onnx
    export DOCLING_TABLEFORMER_DECODER=$PWD/models/tableformer/decoder_int8.onnx

Re-run scripts/conformance/pdf_conformance.sh (or diff Markdown against
tests/data/pdf/groundtruth) after re-quantizing to re-verify quality.
"""

import glob
import os
import sys

import numpy as np

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
# Where the fp32 models live and the int8 outputs go (a checkout's models/ by
# default); DOCLING_RS_MODELS_DIR relocates it (e.g. /opt/models in a Docker
# models stage).
MODELS = os.environ.get("DOCLING_RS_MODELS_DIR", f"{REPO}/models")
# DOCLING_RS_CALIBRATION_DIR: a directory scanned recursively for calibration
# *.pdf files; defaults to the repo's PDF + scanned corpus (the set the
# published quality numbers were measured with).
CALIB = os.environ.get("DOCLING_RS_CALIBRATION_DIR")
SIDE = 640  # layout model input side (layout.rs)


def calibration_pages():
    """Render up to 3 pages of every calibration PDF the way layout.rs
    preprocesses: pdfium at scale 2.0, resize to 640x640 bilinear, /255, CHW
    float32."""
    import pypdfium2 as pdfium
    from PIL import Image

    if CALIB:
        pdfs = sorted(glob.glob(f"{CALIB}/**/*.pdf", recursive=True))
    else:
        pdfs = sorted(glob.glob(f"{REPO}/tests/data/pdf/sources/*.pdf")) + sorted(
            glob.glob(f"{REPO}/tests/data/scanned/sources/*.pdf")
        )
    if not pdfs:
        sys.exit("no calibration PDFs found (set DOCLING_RS_CALIBRATION_DIR)")
    for path in pdfs:
        try:
            doc = pdfium.PdfDocument(path)
        except Exception:
            continue
        for i in range(min(3, len(doc))):
            bmp = doc[i].render(scale=2.0)
            img = bmp.to_pil().convert("RGB").resize((SIDE, SIDE), Image.BILINEAR)
            arr = np.asarray(img, dtype=np.float32) / 255.0
            yield np.transpose(arr, (2, 0, 1))[None, ...]
        doc.close()


def quantize_layout():
    from onnxruntime.quantization import (
        CalibrationDataReader,
        QuantFormat,
        QuantType,
        quantize_static,
    )
    from onnxruntime.quantization.shape_inference import quant_pre_process

    src = f"{MODELS}/layout_heron.onnx"
    pre = f"{MODELS}/layout_heron_pre.onnx"
    dst = f"{MODELS}/layout_heron_int8.onnx"

    class Reader(CalibrationDataReader):
        def __init__(self):
            self.data = [{"pixel_values": x} for x in calibration_pages()]
            print(f"layout: {len(self.data)} calibration samples", flush=True)
            self.it = iter(self.data)

        def get_next(self):
            return next(self.it, None)

    print("layout: pre-processing (shape inference)...", flush=True)
    # skip_symbolic_shape: the #73 dynamic-batch graph makes ORT's symbolic
    # shape inference bail ("Incomplete symbolic shape inference"); the
    # ONNX-level inference quant_pre_process falls back to is enough for the
    # Conv-only QDQ pass.
    quant_pre_process(src, pre, skip_symbolic_shape=True)
    print("layout: static QDQ INT8 quantization (Conv only)...", flush=True)
    quantize_static(
        pre,
        dst,
        Reader(),
        quant_format=QuantFormat.QDQ,
        activation_type=QuantType.QUInt8,
        weight_type=QuantType.QInt8,
        per_channel=True,
        # Conv-only: the RT-DETR decoder/head MatMuls are threshold-sensitive.
        op_types_to_quantize=["Conv"],
    )
    os.remove(pre)
    print(f"layout: done -> {dst} ({os.path.getsize(dst) / 1e6:.1f} MB)")
    validate_layout(src, dst)


# Mirror of layout.rs::LABELS — for the gate's reporting and the table check.
LAYOUT_LABELS = [
    "caption", "footnote", "formula", "list_item", "page_footer",
    "page_header", "picture", "section_header", "table", "text", "title",
    "document_index", "code", "checkbox_selected", "checkbox_unselected",
    "form", "key_value_region",
]
TABLE = LAYOUT_LABELS.index("table")


def _layout_detections(logits, boxes, score_min):
    """Decode one page the way layout.rs does: sigmoid over every
    (query, class), keep the top num_queries scores, threshold, and convert
    cxcywh -> xyxy (normalized). Returns [(class_id, score, (l, t, r, b))]."""
    q, c = logits.shape
    scores = 1.0 / (1.0 + np.exp(-logits.reshape(-1)))
    top = np.argsort(-scores)[:q]
    dets = []
    for idx in top:
        s = float(scores[idx])
        if s <= score_min:
            continue
        qi, ci = divmod(int(idx), c)
        cx, cy, w, h = boxes[qi]
        dets.append((ci, s, (cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2)))
    return dets


def _iou(a, b):
    il = max(a[0], b[0])
    it = max(a[1], b[1])
    ir = min(a[2], b[2])
    ib = min(a[3], b[3])
    inter = max(0.0, ir - il) * max(0.0, ib - it)
    if inter == 0.0:
        return 0.0
    area = lambda r: max(0.0, r[2] - r[0]) * max(0.0, r[3] - r[1])  # noqa: E731
    return inter / (area(a) + area(b) - inter)


def validate_layout(src, dst):
    """Agreement gate: every confident fp32 detection (score >= 0.6) must
    survive quantization — an int8 detection of the same class with IoU >= 0.5
    at the pipeline's base threshold (0.3). Zero tolerance for lost *tables*
    (a dropped table silently degrades to `<!-- image -->` downstream — the
    exact regression this gate exists to stop) and <= 2% for the rest.
    On failure the int8 file is DELETED so a publish run stages fp32 only
    (download_dependencies.sh falls back gracefully — int8 is fetch_optional)."""
    import onnxruntime as ort

    for path, kind in ((src, "fp32"), (dst, "int8")):
        if not os.path.exists(path):
            sys.exit(
                f"layout gate: {path} not found — nothing to validate. "
                + (
                    "Fetch the models first (scripts/install/download_dependencies.sh)."
                    if kind == "fp32"
                    else "No int8 model on disk: the release ships fp32-only when a "
                    "previous gate failed; build one with "
                    "`python scripts/install/quantize_models.py layout` (it re-runs "
                    "this gate on the result)."
                )
            )

    print("layout: validating int8 against fp32 (agreement gate)...", flush=True)
    opts = ort.SessionOptions()
    ref = ort.InferenceSession(src, opts, providers=["CPUExecutionProvider"])
    qnt = ort.InferenceSession(dst, opts, providers=["CPUExecutionProvider"])

    total = missed = tables_missed = 0
    for page_no, x in enumerate(calibration_pages()):
        rl, rb = ref.run(["logits", "pred_boxes"], {"pixel_values": x})
        ql, qb = qnt.run(["logits", "pred_boxes"], {"pixel_values": x})
        ref_dets = _layout_detections(rl[0], rb[0], 0.6)
        qnt_dets = _layout_detections(ql[0], qb[0], 0.3)
        for ci, s, bb in ref_dets:
            total += 1
            if any(cj == ci and _iou(bb, b2) >= 0.5 for cj, _, b2 in qnt_dets):
                continue
            missed += 1
            tables_missed += ci == TABLE
            print(
                f"layout gate: page {page_no}: lost {LAYOUT_LABELS[ci]}"
                f" (fp32 score {s:.2f}, box {tuple(round(v, 3) for v in bb)})"
            )

    rate = missed / total if total else 1.0
    print(
        f"layout gate: {total} confident fp32 detections,"
        f" {missed} lost by int8 ({rate:.2%}), {tables_missed} tables"
    )
    if total == 0 or tables_missed > 0 or rate > 0.02:
        os.remove(dst)
        sys.exit(
            f"layout gate FAILED — {dst} deleted"
            " (int8 disagrees with fp32; the fp32 model remains usable)"
        )
    print("layout gate: PASSED")


def quantize_tableformer_decoder():
    import onnx
    from onnxruntime.quantization import QuantType, quantize_dynamic

    # Quantize the legacy layer-output-cache decoder only. The #97 hoisted-KV
    # decoder_kv.onnx is deliberately NOT quantized: weights-only INT8 of that
    # graph drifts the heavy-table fixtures off the fp32 snapshots (redp5110's
    # TOC decode flips even with per-channel scales; 2206 flips per-tensor),
    # and the measured gain over its fp32 file was only ~2.5% — the Rust loop
    # prefers the byte-exact fp32 decoder_kv instead.
    for stem in ("decoder",):
        src = f"{MODELS}/tableformer/{stem}.onnx"
        if not os.path.exists(src):
            continue
        tmp = f"{MODELS}/tableformer/{stem}_clean.onnx"
        dst = f"{MODELS}/tableformer/{stem}_int8.onnx"

        # The export carries stale value_info shapes that break ORT's shape
        # inference; strip them (external weights get folded into the output).
        m = onnx.load(src)
        del m.graph.value_info[:]
        onnx.save(m, tmp, save_as_external_data=True, location=f"{stem}_clean.onnx.data")
        print(f"tableformer-decoder: dynamic INT8 quantization ({stem})...", flush=True)
        quantize_dynamic(
            tmp,
            dst,
            weight_type=QuantType.QInt8,
            extra_options={"MatMulConstBOnly": True},
        )
        os.remove(tmp)
        os.remove(f"{tmp}.data")
        print(f"tableformer-decoder: done -> {dst} ({os.path.getsize(dst) / 1e6:.1f} MB)")


def quantize_code_formula_decoder():
    """Dynamic INT8 (weights-only MatMul) of the CodeFormulaV2 KV-cache decoder
    step — the autoregressive stage that dominates enrichment latency. Same
    recipe as the TableFormer decoder. Near-exact (see the module docstring);
    re-run scripts/conformance/enrich_conformance.sh after re-quantizing.
    ~655 -> ~165 MB."""
    import onnx
    from onnxruntime.quantization import QuantType, quantize_dynamic

    src = f"{MODELS}/code_formula/decoder_kv.onnx"
    if not os.path.exists(src):
        print(f"code-formula-decoder: {src} not found — skipping")
        return
    tmp = f"{MODELS}/code_formula/decoder_kv_fold.onnx"
    dst = f"{MODELS}/code_formula/decoder_kv_int8.onnx"

    # torch's exporter emits the Linear weights as `Constant` *nodes*, but
    # `MatMulConstBOnly` only quantizes MatMuls whose B is an *initializer* —
    # so fold every tensor-valued Constant into an initializer first (the
    # unfolded graph quantizes to a byte-identical no-op).
    m = onnx.load(src)
    keep = []
    for node in m.graph.node:
        t = next((a.t for a in node.attribute if a.name == "value"), None)
        if node.op_type == "Constant" and t is not None:
            t.name = node.output[0]
            m.graph.initializer.append(t)
        else:
            keep.append(node)
    del m.graph.node[:]
    m.graph.node.extend(keep)
    del m.graph.value_info[:]
    onnx.save(m, tmp, save_as_external_data=True, location="decoder_kv_fold.onnx.data")

    print("code-formula-decoder: dynamic INT8 quantization...", flush=True)
    quantize_dynamic(
        tmp, dst, weight_type=QuantType.QInt8, extra_options={"MatMulConstBOnly": True}
    )
    os.remove(tmp)
    os.remove(f"{tmp}.data")
    print(f"code-formula-decoder: done -> {dst} ({os.path.getsize(dst) / 1e6:.1f} MB)")


def main():
    targets = sys.argv[1:] or ["layout", "tableformer-decoder"]
    for t in targets:
        if t == "layout":
            quantize_layout()
        elif t == "validate-layout":
            # Gate an existing fp32/int8 pair without re-quantizing (e.g. to
            # vet already-downloaded release assets).
            validate_layout(f"{MODELS}/layout_heron.onnx", f"{MODELS}/layout_heron_int8.onnx")
        elif t == "tableformer-decoder":
            quantize_tableformer_decoder()
        elif t == "code-formula-decoder":
            quantize_code_formula_decoder()
        else:
            sys.exit(
                f"unknown target {t!r} "
                "(expected: layout, validate-layout, tableformer-decoder, code-formula-decoder)"
            )


if __name__ == "__main__":
    main()
