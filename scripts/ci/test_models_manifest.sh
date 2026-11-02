#!/usr/bin/env bash
# Regression test for #973: models.json must not declare an external-data
# sidecar (`data`) the release does not host, and every `data` / `of`
# reference must name a file of the same artifact. Offline: builds the
# manifest from a synthetic SHA256SUMS via `models_manifest.sh --check`.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CATALOG="$ROOT/scripts/install/models_catalog.json"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT

# Every catalog asset, except the *.onnx.data sidecars of encoder/bbox
# (hosted: decoder.onnx.data), the shape of snapshot models-v1-df257987d1fc.
jq -r '.artifacts[] | select(.files) | .files[].asset' "$CATALOG" \
  | grep -vxE 'encoder\.onnx\.data|bbox\.onnx\.data' | sort -u \
  | while read -r a; do printf '%064d  %s\n' 1 "$a"; done > "$tmp/SHA256SUMS"
grep -q 'decoder.onnx.data' "$tmp/SHA256SUMS"

"$ROOT/scripts/ci/models_manifest.sh" --check "$tmp/SHA256SUMS" > "$tmp/models.json"

# 1. unhosted sidecar is not declared
jq -e '[.artifacts[].files[] | select(.data == "encoder.onnx.data" or .data == "bbox.onnx.data")] | length == 0' "$tmp/models.json" >/dev/null \
  || { echo "FAIL: models.json declares an unpublished sidecar" >&2; exit 1; }
# 2. a hosted sidecar stays bound to its graph
jq -e '[.artifacts[].files[] | select(.data == "decoder.onnx.data")] | length == 1' "$tmp/models.json" >/dev/null \
  || { echo "FAIL: hosted sidecar reference lost" >&2; exit 1; }
# 3. no dangling data/of reference anywhere
jq -e '[.artifacts[] | (.files | map(.asset)) as $k | .files[] | (.data // empty, .of // empty) | select(IN($k[]) | not)] | length == 0' "$tmp/models.json" >/dev/null \
  || { echo "FAIL: dangling data/of reference" >&2; exit 1; }
echo "ok"
