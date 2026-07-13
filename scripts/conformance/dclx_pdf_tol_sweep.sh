#!/usr/bin/env bash
# Sweep the PDF .dclx geometry tolerance and print the average-similarity ladder.
# Converts each PDF fixture ONCE, then re-diffs the cached document.xml at every
# tolerance (the tolerance only affects the diff, not the conversion). Exact
# (0) is the raw byte-for-byte score; the largest step ignores geometry entirely
# (structure + text only) and is the ceiling the tolerance approaches.
#
#   scripts/conformance/dclx_pdf_tol_sweep.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
BIN="$(pwd)/target/release/docling-rs"
DIFF="$(dirname "$0")/dclx_diff.py"
export PDFIUM_DYNAMIC_LIB_PATH="$(pwd)/.pdfium/lib"

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
ln -sfn "$(pwd)/models" "$tmp/models"; ln -sfn "$(pwd)/.pdfium" "$tmp/.pdfium"

n=0
for ref in tests/data/pdf/groundtruth_dclx/*.dclx; do
  [ -f "$ref" ] || continue
  base=$(basename "$ref" .dclx)
  src=tests/data/pdf/sources/$base
  [ -f "$src" ] || continue
  ( cd "$tmp" && "$BIN" --to dclx "$OLDPWD/$src" >/dev/null 2>&1 )
  ours=$tmp/"${base%.*}".dclx
  [ -f "$ours" ] || { echo "convert FAIL: $base" >&2; continue; }
  unzip -p "$ref"  document.xml > "$tmp/$n.ref.xml"  2>/dev/null
  unzip -p "$ours" document.xml > "$tmp/$n.ours.xml" 2>/dev/null
  rm -f "$ours"; n=$((n+1))
done
echo "converted $n PDF fixtures" >&2

printf "%-24s %s\n" "TOLERANCE" "PDF avg similarity"
for tol in 0 2 5 10 100000; do
  sum=0
  for k in $(seq 0 $((n-1))); do
    d=$(python3 "$DIFF" "$tmp/$k.ref.xml" "$tmp/$k.ours.xml" --tol "$tol")
    lr=$(wc -l < "$tmp/$k.ref.xml"); lo=$(wc -l < "$tmp/$k.ours.xml")
    max=$(( lr > lo ? lr : lo )); [ "$max" -eq 0 ] && max=1
    sim=$(( 100 * (max*2 - d) / (max*2) )); [ "$sim" -lt 0 ] && sim=0
    sum=$((sum+sim))
  done
  [ "$n" -gt 0 ] && avg=$((sum/n)) || avg=0
  case "$tol" in
    0)      label="exact (0)";;
    100000) label="geometry ignored";;
    *)      label="+/-${tol} grid units";;
  esac
  printf "%-24s %s%%\n" "$label" "$avg"
done
