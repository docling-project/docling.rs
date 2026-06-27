#!/usr/bin/env bash
# Shared helpers for the comparison/performance scripts. Source this file.
#
# Provides paths and two functions:
#   ensure_docling      — make local Python docling importable (sets up a venv)
#   build_rust_release  — build the release CLI and echo its path

_COMMON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRAB_DIR="$(cd "$_COMMON_DIR/.." && pwd)"
REPO_ROOT="$(cd "$CRAB_DIR/.." && pwd)"
MANIFEST="$CRAB_DIR/Cargo.toml"
VENV="$CRAB_DIR/.venv-compare"
PYBIN="$VENV/bin/python"
PY_RUNNER="$_COMMON_DIR/docling_convert.py"

# Heavier sibling env, set up on demand for inputs the lightweight one can't do
# (PDF, images): the full ML pipeline — torch, layout/table models, OCR.
VENV_PDF="$CRAB_DIR/.venv-compare-pdf"
PYBIN_PDF="$VENV_PDF/bin/python"

# Extras needed to convert the declarative formats docling-crab supports today
# (HTML, Markdown, CSV, AsciiDoc, plus Office: DOCX/PPTX/XLSX) straight from local
# sources. No torch / ML / model weights — the Python side calls the backend
# directly (see docling_convert.py).
DOCLING_EXTRAS="convert-core,format-web,format-office,format-email"

# Extras for the PDF/image head-to-head: the canonical full-conversion install
# (`standard`) plus EasyOCR, which is docling's default OCR backend. This pulls
# in torch and the layout/table models — heavy, but the only honest way to time
# what docling actually does to a PDF (vs the Rust pipeline's pdfium+layout+OCR).
DOCLING_PDF_EXTRAS="standard,feat-ocr-easyocr"

# Make `import docling` work from the local checkout, installing into a
# dedicated venv on first use. Idempotent and fast after the first run.
ensure_docling() {
  if [[ -x "$PYBIN" ]] && "$PYBIN" -c "import docling.backend.html_backend" >/dev/null 2>&1; then
    return 0
  fi
  echo ">> setting up local docling (editable from $REPO_ROOT, extras: $DOCLING_EXTRAS) ..." >&2
  if ! command -v uv >/dev/null 2>&1; then
    echo "error: 'uv' not found. Either install uv (https://docs.astral.sh/uv/)" >&2
    echo "       or create $VENV and 'pip install -e \"$REPO_ROOT[$DOCLING_EXTRAS]\"' yourself." >&2
    return 1
  fi
  uv venv "$VENV" >&2
  uv pip install --quiet --python "$PYBIN" -e "$REPO_ROOT[$DOCLING_EXTRAS]" >&2
  # The msword backend imports these unconditionally (image rendering + OMML→LaTeX),
  # even though they aren't pulled in by format-office.
  uv pip install --quiet --python "$PYBIN" pypdfium2 pylatexenc >&2
  if ! "$PYBIN" -c "import docling.backend.html_backend" >/dev/null 2>&1; then
    echo "error: docling still not importable after setup" >&2
    return 1
  fi
  echo ">> docling ready ($PYBIN)" >&2
}

# Make the full PDF/image pipeline (DocumentConverter + torch) importable from a
# dedicated heavier venv. Idempotent; only invoked when an input needs it.
ensure_docling_pdf() {
  if [[ -x "$PYBIN_PDF" ]] && "$PYBIN_PDF" -c "import torch, docling.document_converter" >/dev/null 2>&1; then
    return 0
  fi
  echo ">> setting up PDF-capable docling (full ML pipeline, extras: $DOCLING_PDF_EXTRAS) ..." >&2
  echo "   first run installs torch + docling models — can take a few minutes." >&2
  if ! command -v uv >/dev/null 2>&1; then
    echo "error: 'uv' not found. Either install uv (https://docs.astral.sh/uv/)" >&2
    echo "       or create $VENV_PDF and 'pip install -e \"$REPO_ROOT[$DOCLING_PDF_EXTRAS]\"' yourself." >&2
    return 1
  fi
  uv venv "$VENV_PDF" >&2
  uv pip install --quiet --python "$PYBIN_PDF" -e "$REPO_ROOT[$DOCLING_PDF_EXTRAS]" >&2
  if ! "$PYBIN_PDF" -c "import torch, docling.document_converter" >/dev/null 2>&1; then
    echo "error: PDF docling still not importable after setup" >&2
    return 1
  fi
  echo ">> PDF docling ready ($PYBIN_PDF)" >&2
}

# Build the optimized Rust CLI once and echo the binary path.
build_rust_release() {
  cargo build --release --quiet --manifest-path "$MANIFEST" -p docling-crab-cli >&2
  echo "$CRAB_DIR/target/release/docling-crab"
}
