#!/usr/bin/env bash
# Build the models the PDF pipeline needs from source (all gitignored) — the
# from-scratch alternative to download_dependencies.sh's prebuilt fetch. The
# page renderer is pure Rust; no native PDF library is involved.
#
#   scripts/install/pdf_setup.sh
#
# Downloads:
#   - PP-OCRv3 recognition model     -> .models/ocr_rec.onnx
#   - PP-OCR character dictionary     -> .models/ppocr_keys_v1.txt
# And exports two ONNX model sets (need a Python with torch+onnx; set $PYTHON,
# default `python3`):
#   - the RT-DETR layout model -> .models/layout_heron.onnx (torch+transformers)
#   - TableFormer encoder/decoder/bbox -> .models/tableformer/*.onnx (needs
#     docling_ibm_models + onnxscript onnxruntime; auto-downloads the docling
#     weights). Skipped with a note if those deps are missing — the pipeline then
#     falls back to geometric table reconstruction.
set -euo pipefail
cd "$(dirname "$0")/../.."   # docling.rs/
mkdir -p .models

if [ ! -f .models/ocr_rec.onnx ]; then
  echo "→ PP-OCRv3 recognition model"
  curl -sSL -o .models/ocr_rec.onnx \
    "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx"
fi
if [ ! -f .models/ppocr_keys_v1.txt ]; then
  echo "→ PP-OCR dictionary"
  curl -sSL -o .models/ppocr_keys_v1.txt \
    "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt"
fi

if [ ! -f .models/layout_heron.onnx ]; then
  echo "→ exporting RT-DETR layout model (needs torch+transformers+onnx)"
  "${PYTHON:-python3}" scripts/install/export_layout.py .models/layout_heron.onnx
fi

if [ ! -f .models/tableformer/decoder.onnx ]; then
  echo "→ exporting TableFormer (needs docling_ibm_models + onnxscript onnxruntime)"
  if ! "${PYTHON:-python3}" scripts/install/export_tableformer.py .models/tableformer; then
    echo "  ! TableFormer export failed (missing deps or weights). Tables will use"
    echo "    the geometric fallback. Re-run after: pip install docling onnx onnxscript onnxruntime"
  fi
fi

# INT8-quantize the layout model + TableFormer decoder for faster CPU
# inference (validated conformance-neutral — see docs/PERFORMANCE.md). The
# pipeline prefers the int8 files automatically once they exist; skip building
# them with DOCLING_RS_FP32=1. Needs onnx onnxruntime sympy pypdfium2 pillow
# numpy — a missing-deps failure is non-fatal (fp32 keeps working).
if [ "${DOCLING_RS_FP32:-0}" != "1" ] && [ ! -f .models/layout_heron_int8.onnx ]; then
  echo "→ INT8-quantizing layout + TableFormer decoder"
  if ! "${PYTHON:-python3}" scripts/install/quantize_models.py; then
    echo "  ! quantization failed (missing deps?). The fp32 models still work;"
    echo "    re-run after: pip install onnx onnxruntime sympy pypdfium2 pillow numpy"
  fi
fi

echo "done. export these before running the pipeline:"
if [ -f .models/layout_heron_int8.onnx ]; then
  echo "  export DOCLING_LAYOUT_ONNX=$(pwd)/.models/layout_heron_int8.onnx   # int8 default (fp32: layout_heron.onnx, or DOCLING_RS_FP32=1)"
else
  echo "  export DOCLING_LAYOUT_ONNX=$(pwd)/.models/layout_heron.onnx"
fi
echo "  export DOCLING_OCR_REC_ONNX=$(pwd)/.models/ocr_rec.onnx"
echo "  export DOCLING_OCR_DICT=$(pwd)/.models/ppocr_keys_v1.txt"
if [ -f .models/tableformer/decoder.onnx ]; then
  # These default to a *relative* path if unset, which only resolves when the
  # process's CWD is this repo root — export them explicitly so TableFormer
  # still loads (instead of silently falling back to geometric reconstruction)
  # no matter where the binary/binding is actually invoked from.
  echo "  export DOCLING_TABLEFORMER_ENCODER=$(pwd)/.models/tableformer/encoder.onnx"
  if [ -f .models/tableformer/decoder_int8.onnx ]; then
    echo "  export DOCLING_TABLEFORMER_DECODER=$(pwd)/.models/tableformer/decoder_int8.onnx   # int8 default (fp32: decoder.onnx, or DOCLING_RS_FP32=1)"
  else
    echo "  export DOCLING_TABLEFORMER_DECODER=$(pwd)/.models/tableformer/decoder.onnx"
  fi
  echo "  export DOCLING_TABLEFORMER_BBOX=$(pwd)/.models/tableformer/bbox.onnx"
fi
