#!/usr/bin/env sh
# Fetch the PDF/image ML pipeline's native dependencies — pdfium + the ONNX
# models (layout, OCR, TableFormer) — from this repo's GitHub Releases,
# straight into the current directory. No npm, no Python, no env vars needed
# afterwards: both the Rust CLI and the Node.js/Bun bindings look for
# `models/` and `.pdfium/lib` relative to the process's current directory by
# default.
#
# Run from your app's directory (or a checkout of this repo):
#   scripts/download_dependencies.sh
# or, without a checkout:
#   curl -fsSL https://raw.githubusercontent.com/artiz/fleischwolf/master/scripts/download_dependencies.sh | sh
#
# Then either:
#   cargo run -p fleischwolf-cli -- <file>
# or:
#   npm i fleischwolf
#   node -e "import { convertFileAsync } from 'fleischwolf'; const r = await convertFileAsync('example.pdf', { to: 'markdown' }); console.log(r.content) "
#
# Downloads (from https://github.com/artiz/fleischwolf/releases, tag
# models-v1 by default — override the base with $FLEISCHWOLF_MODELS_URL):
#   .pdfium/lib/libpdfium.so                      (Linux x64)
#   models/layout_heron.onnx
#   models/ocr_rec.onnx
#   models/ppocr_keys_v1.txt
#   models/tableformer/encoder.onnx
#   models/tableformer/decoder.onnx (+ decoder.onnx.data, if the export needs it)
#   models/tableformer/bbox.onnx
#
# pdfium is Linux x64 only for now, matching what's hosted in the release; for
# other platforms (or to build the models from source) see scripts/pdf_setup.sh.
#
# Idempotent: skips files already on disk. Pass --force to re-fetch everything.
set -eu

BASE_URL="${FLEISCHWOLF_MODELS_URL:-https://github.com/artiz/fleischwolf/releases/download/models-v1}"

FORCE=false
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=true ;;
    *)
      echo "usage: download_dependencies.sh [--force]" >&2
      exit 2
      ;;
  esac
done

if ! command -v curl >/dev/null 2>&1; then
  echo "error: curl is required" >&2
  exit 1
fi

mkdir -p .pdfium/lib models/tableformer

fetch() { # <url> <dest>
  if [ "$FORCE" = false ] && [ -f "$2" ]; then
    echo "  = $2 (already present)"
    return 0
  fi
  echo "  > $2"
  curl -fsSL -o "$2.download" "$1"
  mv "$2.download" "$2"
}

fetch_optional() { # <url> <dest> — ignore a missing/failed asset (sidecar files)
  if [ "$FORCE" = false ] && [ -f "$2" ]; then
    return 0
  fi
  if curl -fsSL -o "$2.download" "$1" 2>/dev/null; then
    mv "$2.download" "$2"
    echo "  > $2"
  else
    rm -f "$2.download"
  fi
}

echo "fetching fleischwolf ML dependencies from $BASE_URL"
fetch "$BASE_URL/libpdfium.so" .pdfium/lib/libpdfium.so
fetch "$BASE_URL/layout_heron.onnx" models/layout_heron.onnx
fetch "$BASE_URL/ocr_rec.onnx" models/ocr_rec.onnx
fetch "$BASE_URL/ppocr_keys_v1.txt" models/ppocr_keys_v1.txt
fetch "$BASE_URL/encoder.onnx" models/tableformer/encoder.onnx
fetch "$BASE_URL/decoder.onnx" models/tableformer/decoder.onnx
fetch_optional "$BASE_URL/decoder.onnx.data" models/tableformer/decoder.onnx.data
fetch "$BASE_URL/bbox.onnx" models/tableformer/bbox.onnx

echo "done — models/ and .pdfium/lib populated in $(pwd)"
