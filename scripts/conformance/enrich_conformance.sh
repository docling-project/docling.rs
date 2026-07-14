#!/usr/bin/env bash
#
# Conformance of the opt-in enrichment models (issue #76) against Python
# docling's output on the enrichment test PDFs, checked into
# tests/data/pdf/groundtruth-enriched/ (generated with docling 2.112,
# do_picture_classification/do_code_enrichment/do_formula_enrichment on,
# do_ocr off — the sources have embedded text, so OCR contributes nothing).
#
# * code_and_formula.pdf — Markdown must match byte-for-byte: the CodeFormula
#   VLM's code rewrite and formula LaTeX are compared against docling's.
# * picture_classification.pdf — the JSON picture items must carry docling's
#   classification annotation + meta with the same class ranking. Confidences
#   are compared to 2 decimal places: the crops are resized from the page
#   render rather than re-rendered per region, so the model sees pixels that
#   differ sub-pixel from docling's and third-decimal drift is expected.
#
# Needs the enrichment models on disk (scripts/install/download_dependencies.sh
# --enrich, or the local exports). CodeFormula runs an autoregressive VLM per
# code/formula region — expect ~half a minute on CPU.
#
# Usage: scripts/conformance/enrich_conformance.sh

set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../.."

export PDFIUM_DYNAMIC_LIB_PATH="${PDFIUM_DYNAMIC_LIB_PATH:-$(pwd)/.pdfium/lib}"
export DOCLING_RS_SLOW_RESIZE="${DOCLING_RS_SLOW_RESIZE:-1}"

cargo build --release --quiet -p docling-cli
BIN=./target/release/docling-rs
GT=tests/data/pdf/groundtruth-enriched
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail=0

echo "== code_and_formula.pdf (--enrich-code --enrich-formula, markdown)"
"$BIN" --no-stream --enrich-code --enrich-formula \
  tests/data/pdf/sources/code_and_formula.pdf > "$TMP/cf.md"
if diff -u "$GT/code_and_formula.md" "$TMP/cf.md" > "$TMP/cf.diff"; then
  echo "   EXACT"
else
  echo "   DIFFERS:"
  sed 's/^/   /' "$TMP/cf.diff"
  fail=1
fi

echo "== picture_classification.pdf (--enrich-picture-classes, JSON)"
"$BIN" --no-stream --to json --enrich-picture-classes \
  tests/data/pdf/sources/picture_classification.pdf > "$TMP/pc.json"
if python3 - "$GT/picture_classification.json" "$TMP/pc.json" <<'PY'
import json
import sys

gt = json.load(open(sys.argv[1]))
rs = json.load(open(sys.argv[2]))
ok = True
if len(rs.get("pictures", [])) != len(gt["pictures"]):
    print(f"   picture count: rs {len(rs.get('pictures', []))} vs gt {len(gt['pictures'])}")
    ok = False
for i, (a, b) in enumerate(zip(rs.get("pictures", []), gt["pictures"])):
    for src, name in ((a, "rs"), (b, "gt")):
        if not src.get("annotations") or "classification" not in (src.get("meta") or {}):
            print(f"   picture {i}: {name} missing classification annotation/meta")
            ok = False
    if not ok:
        continue
    pa = a["annotations"][0]["predicted_classes"]
    pb = b["annotations"][0]["predicted_classes"]
    # Same ranking on the confident head of the distribution; the long tail of
    # ~1e-6 classes may reorder from sub-pixel crop differences.
    ra = [(c["class_name"], round(c["confidence"], 2)) for c in pa[:3]]
    rb = [(c["class_name"], round(c["confidence"], 2)) for c in pb[:3]]
    if ra != rb:
        print(f"   picture {i}: top-3 differs\n    rs {ra}\n    gt {rb}")
        ok = False
    ma = [p["class_name"] for p in a["meta"]["classification"]["predictions"][:3]]
    if ma != [c for c, _ in ra]:
        print(f"   picture {i}: meta/annotation ranking mismatch: {ma} vs {ra}")
        ok = False
    else:
        print(f"   picture {i}: {ra[0][0]} ({ra[0][1]}) — matches docling")
sys.exit(0 if ok else 1)
PY
then
  echo "   OK"
else
  fail=1
fi

exit $fail
