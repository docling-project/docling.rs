#!/usr/bin/env bash
# Fetch the native libs + models the PDF pipeline needs (all gitignored).
#
#   scripts/pdf_setup.sh
#
# Downloads:
#   - libpdfium (bblanchon prebuilt) -> .pdfium/lib/libpdfium.so
#   - PP-OCRv3 recognition model     -> models/ocr_rec.onnx
#   - PP-OCR character dictionary     -> models/ppocr_keys_v1.txt
# And exports two ONNX model sets (need a Python with torch+onnx; set $PYTHON,
# default `python3`):
#   - the RT-DETR layout model -> models/layout_heron.onnx (torch+transformers)
#   - TableFormer encoder/decoder/bbox -> models/tableformer/*.onnx (needs
#     docling_ibm_models + onnxscript onnxruntime; auto-downloads the docling
#     weights). Skipped with a note if those deps are missing — the pipeline then
#     falls back to geometric table reconstruction.
set -euo pipefail
cd "$(dirname "$0")/.."   # fleischwolf/
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

if [ ! -f models/tableformer/decoder.onnx ]; then
  echo "→ exporting TableFormer (needs docling_ibm_models + onnxscript onnxruntime)"
  if ! "${PYTHON:-python3}" scripts/export_tableformer.py models/tableformer; then
    echo "  ! TableFormer export failed (missing deps or weights). Tables will use"
    echo "    the geometric fallback. Re-run after: pip install docling onnx onnxscript onnxruntime"
  fi
fi

echo "done. export these before running the pipeline:"
echo "  export PDFIUM_DYNAMIC_LIB_PATH=$(pwd)/.pdfium/lib"
echo "  export DOCLING_LAYOUT_ONNX=$(pwd)/models/layout_heron.onnx"
echo "  export DOCLING_OCR_REC_ONNX=$(pwd)/models/ocr_rec.onnx"
echo "  export DOCLING_OCR_DICT=$(pwd)/models/ppocr_keys_v1.txt"
