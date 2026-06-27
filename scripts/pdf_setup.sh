#!/usr/bin/env bash
# Fetch the native libs + models the PDF pipeline needs (all gitignored).
#
#   scripts/pdf_setup.sh
#
# Downloads:
#   - libpdfium (bblanchon prebuilt) -> .pdfium/lib/libpdfium.so
#   - PP-OCRv3 recognition model     -> models/ocr_rec.onnx
#   - PP-OCR character dictionary     -> models/ppocr_keys_v1.txt
# And exports the RT-DETR layout model -> models/layout_heron.onnx
#   (needs a Python with torch+transformers+onnx; set $PYTHON, default `python3`).
set -euo pipefail
cd "$(dirname "$0")/.."   # docling-crab/
mkdir -p .pdfium models

PLATFORM="${PDFIUM_PLATFORM:-linux-x64}"
if [ ! -f .pdfium/lib/libpdfium.so ]; then
  echo "→ libpdfium ($PLATFORM)"
  curl -sSL -o /tmp/pdfium.tgz \
    "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-${PLATFORM}.tgz"
  tar xzf /tmp/pdfium.tgz -C .pdfium
fi

if [ ! -f models/ocr_rec.onnx ]; then
  echo "→ PP-OCRv3 recognition model"
  curl -sSL -o models/ocr_rec.onnx \
    "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx"
fi
if [ ! -f models/ppocr_keys_v1.txt ]; then
  echo "→ PP-OCR dictionary"
  curl -sSL -o models/ppocr_keys_v1.txt \
    "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt"
fi

if [ ! -f models/layout_heron.onnx ]; then
  echo "→ exporting RT-DETR layout model (needs torch+transformers+onnx)"
  "${PYTHON:-python3}" scripts/export_layout.py models/layout_heron.onnx
fi

echo "done. export these before running the pipeline:"
echo "  export PDFIUM_DYNAMIC_LIB_PATH=$(pwd)/.pdfium/lib"
echo "  export DOCLING_LAYOUT_ONNX=$(pwd)/models/layout_heron.onnx"
echo "  export DOCLING_OCR_REC_ONNX=$(pwd)/models/ocr_rec.onnx"
echo "  export DOCLING_OCR_DICT=$(pwd)/models/ppocr_keys_v1.txt"
