#!/usr/bin/env python3
"""Quantize the PDF-pipeline ONNX models to INT8 for faster CPU inference.

Two quantizations, both validated against the PDF corpus (see
PDF_PERFORMANCE.md for the measured speed/quality numbers):

* **layout** — static QDQ INT8 of the RT-DETR layout model, **Conv ops only**
  (the HGNetv2 backbone). Calibrated on real corpus pages rendered exactly the
  way `layout.rs::predict` preprocesses them. The transformer decoder and
  detection heads stay fp32: quantizing their MatMuls shifts class scores near
  the 0.3 threshold and visibly degrades output (headers demoted to text,
  page-footers leaking in), while conv-only keeps groundtruth conformance at
  fp32 level. ~2.4x faster layout inference on AVX-512-VNNI CPUs, 172 -> 68 MB.

* **tableformer-decoder** — dynamic INT8 (weights-only MatMul) of the
  autoregressive tag decoder. Output is byte-identical on the corpus;
  ~10% faster table-structure decode, 78 -> 50 MB.

Usage (from the repo root, models fetched by scripts/download_dependencies.sh):

    uv venv .venv-quant && uv pip install --python .venv-quant/bin/python \
        onnx onnxruntime sympy pypdfium2 pillow numpy
    .venv-quant/bin/python scripts/quantize_models.py layout tableformer-decoder

Then point the pipeline at the quantized files:

    export DOCLING_LAYOUT_ONNX=$PWD/models/layout_heron_int8.onnx
    export DOCLING_TABLEFORMER_DECODER=$PWD/models/tableformer/decoder_int8.onnx

Re-run scripts/pdf_conformance.sh (or diff Markdown against
tests/data/pdf/groundtruth) after re-quantizing to re-verify quality.
"""

import glob
import os
import sys

import numpy as np

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# Where the fp32 models live and the int8 outputs go (a checkout's models/ by
# default); FLEISCHWOLF_MODELS_DIR relocates it (e.g. /opt/models in a Docker
# models stage).
MODELS = os.environ.get("FLEISCHWOLF_MODELS_DIR", f"{REPO}/models")
# FLEISCHWOLF_CALIBRATION_DIR: a directory scanned recursively for calibration
# *.pdf files; defaults to the repo's PDF + scanned corpus (the set the
# published quality numbers were measured with).
CALIB = os.environ.get("FLEISCHWOLF_CALIBRATION_DIR")
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
        sys.exit("no calibration PDFs found (set FLEISCHWOLF_CALIBRATION_DIR)")
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
    quant_pre_process(src, pre, skip_symbolic_shape=False)
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


def quantize_tableformer_decoder():
    import onnx
    from onnxruntime.quantization import QuantType, quantize_dynamic

    # Quantize the legacy layer-output-cache decoder and, when the export
    # produced it, the true-KV-cache variant (decoder_kv.onnx — preferred by
    # the Rust loop for very-large-table workloads).
    for stem in ("decoder", "decoder_kv"):
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
            tmp, dst, weight_type=QuantType.QInt8, extra_options={"MatMulConstBOnly": True}
        )
        os.remove(tmp)
        os.remove(f"{tmp}.data")
        print(f"tableformer-decoder: done -> {dst} ({os.path.getsize(dst) / 1e6:.1f} MB)")


def main():
    targets = sys.argv[1:] or ["layout", "tableformer-decoder"]
    for t in targets:
        if t == "layout":
            quantize_layout()
        elif t == "tableformer-decoder":
            quantize_tableformer_decoder()
        else:
            sys.exit(f"unknown target {t!r} (expected: layout, tableformer-decoder)")


if __name__ == "__main__":
    main()
