#!/usr/bin/env bash
#
# Install the latest PUBLISHED docling from PyPI into fleischwolf/.venv-compare
# so the comparison scripts can import it. (Published docling 2.x bundles every
# format backend plus the full PDF pipeline — torch + models — so the first run
# downloads several hundred MB.)
#
# Usage: scripts/setup-docling.sh
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"
ensure_docling
echo "Done. Python docling is available at: $PYBIN"
