#!/usr/bin/env bash
# Shared helpers for the comparison/performance scripts. Source this file.
#
# Provides paths and helpers:
#   ensure_docling      — install the latest PUBLISHED docling into a venv
#   ensure_docling_pdf  — alias of ensure_docling (one env now does everything)
#   build_rust_release  — build the release CLI and echo its path

_COMMON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$_COMMON_DIR/.." && pwd)"
MANIFEST="$WORKSPACE_DIR/Cargo.toml"
# The conformance corpus ships in-repo (sources + docling groundtruth).
CORPUS_DIR="$WORKSPACE_DIR/tests/data"

VENV="$WORKSPACE_DIR/.venv-compare"
PYBIN="$VENV/bin/python"
PY_RUNNER="$_COMMON_DIR/docling_convert.py"

# Published docling (2.x) bundles every format backend AND the full PDF pipeline
# (layout/table via docling-ibm-models → torch). There is no longer a no-torch
# "lightweight" install, so a single env handles both declarative and PDF/image
# conversion; the older two-venv split survives only as aliases below.
VENV_PDF="$VENV"
PYBIN_PDF="$PYBIN"

# We install the LATEST PUBLISHED docling from PyPI (not from local sources),
# with the `easyocr` extra so the PDF/image head-to-head exercises docling's
# default OCR backend.
DOCLING_PKG="docling[easyocr]"

# Install the latest published docling into a dedicated venv on first use.
# Idempotent and fast after the first run. (The first run is heavy: docling
# pulls torch + model packages — several hundred MB.)
ensure_docling() {
  if [[ -x "$PYBIN" ]] && "$PYBIN" -c "import docling.backend.html_backend" >/dev/null 2>&1; then
    return 0
  fi
  echo ">> installing latest published '$DOCLING_PKG' from PyPI ..." >&2
  if ! command -v uv >/dev/null 2>&1; then
    echo "error: 'uv' not found. Either install uv (https://docs.astral.sh/uv/)" >&2
    echo "       or create $VENV and 'pip install \"$DOCLING_PKG\"' yourself." >&2
    return 1
  fi
  uv venv "$VENV" >&2
  uv pip install --quiet --python "$PYBIN" "$DOCLING_PKG" >&2
  # The msword backend imports these unconditionally (image rendering + OMML→LaTeX).
  uv pip install --quiet --python "$PYBIN" pypdfium2 pylatexenc >&2
  if ! "$PYBIN" -c "import docling.backend.html_backend" >/dev/null 2>&1; then
    echo "error: docling still not importable after install" >&2
    return 1
  fi
  echo ">> docling ready ($PYBIN)" >&2
}

# The single published-docling env already carries the full PDF pipeline (torch +
# layout/table models), so this is just an alias kept for callers that asked for
# the heavier path explicitly.
ensure_docling_pdf() { ensure_docling; }

# Build the optimized Rust CLI once and echo the binary path.
build_rust_release() {
  cargo build --release --quiet --manifest-path "$MANIFEST" -p fleischwolf-cli >&2
  echo "$WORKSPACE_DIR/target/release/fleischwolf"
}
