#!/usr/bin/env bash
#
# Score docling.rs's Markdown output against the latest PUBLISHED Python docling
# across a corpus. docling is installed from PyPI on first use (see _common.sh);
# it is always the reference now. The committed groundtruth under
# tests/data/<fmt>/groundtruth/*.md is used only as a fallback for sources the
# installed docling can't convert.
#
# Usage:
#   scripts/conformance/conformance.sh [format]      # default: html
#   scripts/conformance/conformance.sh docx
#
# Output: a per-file table (diff-line count; one changed line counts as 2) and a
# summary.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../_common.sh"

FORMAT="${1:-html}"

DATA_DIR="$CORPUS_DIR/$FORMAT"
SRC_DIR="$DATA_DIR/sources"
GT_DIR="$DATA_DIR/groundtruth"

if [[ ! -d "$SRC_DIR" ]]; then
  echo "error: corpus not found under $DATA_DIR" >&2
  exit 1
fi

ensure_docling
echo ">> reference: latest published Python docling (committed groundtruth as fallback)"

# Build once up front so per-file timing isn't dominated by compilation.
cargo build --quiet --manifest-path "$MANIFEST" -p docling-cli

# Write the reference Markdown for a source into $1; returns non-zero to skip.
# The installed docling is the reference; when it can't produce output for a
# source we fall back to the committed groundtruth ("<source>.md" for most
# formats, "<stem>.md" for PDF, e.g. 2305.03393v1-pg9.md).
reference_into() {
  local src="$1" out="$2" base stem cand
  base="$(basename "$src")"
  stem="${base%.*}"
  if "$PYBIN" "$PY_RUNNER" "$src" "$out" 2>/dev/null && [[ -s "$out" ]]; then
    return 0
  fi
  for cand in "$GT_DIR/$base.md" "$GT_DIR/$stem.md"; do
    [[ -f "$cand" ]] && { cp "$cand" "$out"; return 0; }
  done
  return 1
}

# Collapse every run of whitespace to a single space and trim line ends, so a
# difference that is *only* spacing (e.g. docling's spurious double space before
# the `amt` fraction `up to  1 / 4`, where our single-spaced `up to 1 / 4` is in
# fact the more faithful rendering) counts as a match. Reported alongside — never
# instead of — the strict byte comparison.
norm() { sed -E 's/[[:space:]]+/ /g; s/^ +//; s/ +$//'; }

printf "%-44s %14s\n" "FIXTURE" "DIFF-LINES"
printf "%-44s %14s\n" "-------" "----------"

total=0
exact=0
nmatch=0
ref="$(mktemp)"
trap 'rm -f "$ref"' EXIT
for src in "$SRC_DIR"/*; do
  reference_into "$src" "$ref" || continue
  total=$((total + 1))

  out="$(cargo run --quiet --manifest-path "$MANIFEST" -p docling-cli -- "$src" 2>/dev/null || echo '<ERROR>')"
  # Strict, trailing-newline-insensitive byte comparison.
  d="$(diff <(printf '%s' "$out") <(printf '%s' "$(cat "$ref")") | grep -cE '^[<>]' || true)"
  # Whitespace-normalized comparison (spacing-only diffs ignored).
  dn="$(diff <(printf '%s' "$out" | norm) <(printf '%s' "$(cat "$ref")" | norm) | grep -cE '^[<>]' || true)"

  if [[ "$d" -eq 0 ]]; then
    exact=$((exact + 1))
    nmatch=$((nmatch + 1))
    mark="EXACT"
  elif [[ "$dn" -eq 0 ]]; then
    nmatch=$((nmatch + 1))
    mark="$d (ws-ok)"
  else
    mark="$d"
  fi
  printf "%-44s %14s\n" "$(basename "$src")" "$mark"
done

echo
echo "Exact (strict):                $exact / $total"
echo "Whitespace-normalized matches: $nmatch / $total"
