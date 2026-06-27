#!/usr/bin/env bash
#
# Set up a local, editable install of Python docling from this repo's sources so
# the comparison scripts can import it without `pip install docling`.
#
# Creates docling-crab/.venv-compare with the minimal extras needed for the
# declarative formats (HTML, Markdown, CSV) — no torch / ML weights.
#
# Usage: scripts/setup-docling.sh
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"
ensure_docling
echo "Done. Python docling is available at: $PYBIN"
