#!/usr/bin/env bash
#
# Assemble the `docling.rs-wasm` npm package: compile docling-wasm for
# wasm32-unknown-unknown, run wasm-bindgen for the `bundler` (import from
# "docling.rs-wasm") and `web` (import from "docling.rs-wasm/web" + init())
# targets, and lay the results out next to the package skeleton from
# crates/docling-wasm/npm/, versioned from the workspace Cargo.toml.
#
# The wasm-bindgen CLI must match the wasm-bindgen crate version in Cargo.lock
# (the generated glue is version-locked); npm-publish.yml installs the pinned
# version the same way pages.yml does.
#
# Usage: scripts/ci/build_wasm_pkg.sh [outdir]   (default target/npm-wasm)
set -euo pipefail
cd "$(dirname "$0")/../.."

out="${1:-target/npm-wasm}"

cargo build -p docling-wasm --target wasm32-unknown-unknown --release --locked
wasm="target/wasm32-unknown-unknown/release/docling_wasm.wasm"

rm -rf "$out"
mkdir -p "$out"
for tgt in bundler web; do
  wasm-bindgen --target "$tgt" --out-dir "$out/$tgt" "$wasm"
done

cp crates/docling-wasm/npm/package.json crates/docling-wasm/npm/README.md "$out/"
cp LICENSE "$out/"
# The wasm build leaves the Adobe Glyph List and the CID tables out, but the
# Symbol / ZapfDingbats and standard-14 tables derived from Adobe's AFM files
# are part of the text-layer parser: their notice ships with the package.
cp crates/docling-pdf/data/adobe-glyph-list/LICENSE.txt "$out/LICENSE-adobe-glyph-list.txt"

version="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
(cd "$out" && npm version "$version" --no-git-tag-version --allow-same-version >/dev/null)

echo ">> $out ready: docling.rs-wasm@$version"
du -sh "$out"/bundler/docling_wasm_bg.wasm
