#!/usr/bin/env bash
# Vendor Adobe's CID -> Unicode tables (pdf2unicode/, with LICENSE.txt and
# README.md) from the 20230118 release of mapping-resources-pdf.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p crates/docling-pdf/data && cd crates/docling-pdf/data
repo=https://github.com/adobe-type-tools/mapping-resources-pdf.git
commit=2dd5e53fb74a01718b9dfd448a0d1cce6fff2aa5
src="$(mktemp -d)" && trap 'rm -rf "$src"' EXIT
git -C "$src" init -q && git -C "$src" fetch -q --depth 1 "$repo" "$commit"
rm -rf mapping-resources-pdf && mkdir mapping-resources-pdf
git -C "$src" archive "$commit" LICENSE.txt README.md pdf2unicode | tar -x -C mapping-resources-pdf
echo "$repo @ $commit" > mapping-resources-pdf.txt
# maturin ships the LICENSE-* files next to pyproject.toml in every wheel.
cp mapping-resources-pdf/LICENSE.txt ../../docling-py/LICENSE-mapping-resources-pdf.txt
