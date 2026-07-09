#!/usr/bin/env bash
#
# Compare Markdown output of Python `docling` vs Rust `docling.rs` for one
# input file, and show a unified diff.
#
# Usage:
#   scripts/conformance/compare.sh <input-file>
#   scripts/conformance/compare.sh tests/data/html/sources/example_03.html
#
# Python docling is the latest PUBLISHED release, installed from PyPI into
# docling.rs/.venv-compare on first run (see _common.sh). For declarative
# formats the Python side calls the format backend directly via
# docling_convert.py, mirroring the work docling.rs does.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../_common.sh"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <input-file>" >&2
  exit 2
fi

INPUT="$(realpath "$1")"
OUT_DIR="$(mktemp -d)"
trap 'rm -rf "$OUT_DIR"' EXIT

echo ">> input:  $INPUT"
ensure_docling

# --- Python docling --------------------------------------------------------
echo ">> running Python docling ..."
"$PYBIN" "$PY_RUNNER" "$INPUT" "$OUT_DIR/python.md"

# --- Rust docling.rs -----------------------------------------------------
echo ">> running Rust docling.rs ..."
cargo run --quiet --manifest-path "$MANIFEST" -p docling-cli -- "$INPUT" \
  > "$OUT_DIR/rust.md"

# Normalize trailing whitespace/newlines so a single missing final newline
# doesn't show up as a difference on every file.
norm() { printf '%s\n' "$(cat "$1")" > "$1.norm" && mv "$1.norm" "$1"; }
norm "$OUT_DIR/python.md"
norm "$OUT_DIR/rust.md"

# --- Compare ---------------------------------------------------------------
echo ">> python -> $OUT_DIR/python.md"
echo ">> rust   -> $OUT_DIR/rust.md"
echo

if diff -u --label "python/docling" "$OUT_DIR/python.md" \
            --label "rust/docling.rs" "$OUT_DIR/rust.md"; then
  echo
  echo "✅ IDENTICAL"
else
  echo
  echo "ℹ️  outputs differ (see diff above)"
  # If the only differences are whitespace (collapsing runs of spaces/tabs and
  # trimming line ends), say so — e.g. docling's spurious double space in a
  # fraction, where our single-spaced output is the more faithful rendering.
  norm() { sed -E 's/[[:space:]]+/ /g; s/^ +//; s/ +$//' "$1"; }
  if diff <(norm "$OUT_DIR/python.md") <(norm "$OUT_DIR/rust.md") >/dev/null; then
    echo "✅ IDENTICAL after whitespace normalization (spacing-only differences)"
  fi
  # Quick similarity stat: shared lines / python lines.
  shared=$(comm -12 <(sort "$OUT_DIR/python.md") <(sort "$OUT_DIR/rust.md") | grep -c . || true)
  total=$(grep -c . "$OUT_DIR/python.md" || true)
  echo "   shared non-empty lines: ${shared}/${total}"
fi
