#!/usr/bin/env bash
# Regenerate PDF output for the test corpus and diff it against the committed
# snapshot baseline (tests/pdf_snapshots/). The pipeline is deterministic, so a
# clean checkout should report every fixture EXACT; a non-zero diff means the
# output drifted. Run scripts/pdf_setup.sh first to fetch the libs/models.
set -euo pipefail
cd "$(dirname "$0")/.."   # docling-crab/

export PDFIUM_DYNAMIC_LIB_PATH="${PDFIUM_DYNAMIC_LIB_PATH:-$(pwd)/.pdfium/lib}"
export DOCLING_LAYOUT_ONNX="${DOCLING_LAYOUT_ONNX:-$(pwd)/models/layout_heron.onnx}"
export DOCLING_OCR_REC_ONNX="${DOCLING_OCR_REC_ONNX:-$(pwd)/models/ocr_rec.onnx}"
export DOCLING_OCR_DICT="${DOCLING_OCR_DICT:-$(pwd)/models/ppocr_keys_v1.txt}"

for f in "$PDFIUM_DYNAMIC_LIB_PATH/libpdfium.so" "$DOCLING_LAYOUT_ONNX" \
         "$DOCLING_OCR_REC_ONNX" "$DOCLING_OCR_DICT"; do
  [ -e "$f" ] || { echo "MISSING: $f  (run scripts/pdf_setup.sh)"; exit 1; }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "regenerating PDF output ..."
cargo run --release -q -p docling-crab-pdf --example snapshot -- ../tests/data "$tmp" 1>&2

exact=0; drift=0; tot=0
while IFS= read -r snap; do
  rel="${snap#tests/pdf_snapshots/}"
  gen="$tmp/$rel"
  tot=$((tot + 1))
  if [ -f "$gen" ] && diff -q "$snap" "$gen" >/dev/null 2>&1; then
    exact=$((exact + 1))
  else
    drift=$((drift + 1))
    d=$(diff "$snap" "$gen" 2>/dev/null | grep -cE '^[<>]' || true)
    printf "  %-55s %s\n" "$rel" "${d:-MISSING}"
  fi
done < <(find tests/pdf_snapshots -name '*.md' | sort)

echo "PDF snapshot conformance: $exact/$tot exact ($drift drifted)"
[ "$drift" -eq 0 ]
