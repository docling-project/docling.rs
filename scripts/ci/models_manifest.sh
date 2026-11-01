#!/usr/bin/env bash
# Regenerate the records of a models release from the assets it hosts right
# now, and upload them (clobbering the previous ones):
#
#   SHA256SUMS         every asset's digest (the `sha256sum` format) — what
#                      the downloaders verify each fetched asset against;
#   models.json        the digest-bound artifact manifest (#942): the
#                      provenance catalog scripts/install/models_catalog.json
#                      joined with the release's digests and sizes — each
#                      artifact's files (an ONNX graph with its `.onnx.data`
#                      external-weights sidecar is one artifact), upstream
#                      repository + revision, how the bytes were produced
#                      (export / quantization / repack / rehost / build), the
#                      SPDX licence, its `lineage` (#955: the upstream model
#                      card, training records, the producer's conversion,
#                      and what is unavailable) and the notice/licence files
#                      that apply — plus `execution`: the publish run's
#                      export_record.json (scripts/ci/export_record.py),
#                      linked and digest-bound when the release has one;
#   MODELS_NOTICE.md   docs/MODELS_NOTICE.md, the attribution notice;
#   LICENSE-<SPDX>.txt docs/licenses/<SPDX>.txt, the licence texts the
#                      catalog names.
#
#   scripts/ci/models_manifest.sh models-v1          # GITHUB_REPOSITORY or docling-project/docling.rs
#   scripts/ci/models_manifest.sh --check SHA256SUMS [export_record.json]
#                                                    # offline: print the models.json a
#                                                    # release with these sums (and record) would get
#
# Every workflow that uploads into a `models-v*` release ends with this
# (publish-models.yml, docling-parse-render.yml, onnxruntime-s390x.yml), so
# the records always cover the whole release, whichever workflow touched it
# last; models-manifest.yml runs it alone for a release that predates them or
# after a manual `gh release upload`. The downloaders —
# scripts/install/download_dependencies.sh / .bat and the Python
# `download_models()` — verify each asset they fetch from the release against
# SHA256SUMS (a corrupted or truncated transfer, a tampered mirror behind
# DOCLING_RS_MODELS_URL, a swapped asset — `libpdfium.so` and
# `libonnxruntime.so` are executable code loaded into the process) and keep
# the other records beside the files (`.models/MODELS_NOTICE.md`,
# `.models/models.json`, `.models/LICENSE-*.txt`), so a downstream
# redistribution carries the attribution with the bytes.
#
# A release asset the catalog does not cover fails the run before anything
# is uploaded: a new export staged by publish-models.yml needs its catalog
# entry (upstream, revision, licence) in the same change, or the manifest
# would silently present an unattributed file.
#
# Hashes come from GitHub's own `digest` of each asset (computed on upload,
# `sha256:<hex>` in the REST API); an asset without one (uploaded before
# GitHub recorded digests) is downloaded and hashed here. SHA256SUMS is
# sorted by name, so `sha256sum -c` reads it as is, and lists the other
# records too (they are uploaded first), itself excepted.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CATALOG="$ROOT/scripts/install/models_catalog.json"
NOTICE_SRC="$ROOT/docs/MODELS_NOTICE.md"
LICENSES_SRC="$ROOT/docs/licenses"

# build_models_json <tag> <repo> <sums-file> <sizes-tsv> <record-file> > models.json
#
# The catalog's artifacts with each file's `sha256` (+ `size` when known)
# filled in from the release; a catalog file the release does not host (an
# optional sidecar the export did not need, an int8 variant the quantizer's
# gate rejected) is dropped, an artifact left without files too. The
# `pattern` artifacts (the per-platform tarballs other workflows upload)
# collect every asset their regex matches. Fails, naming them, on release
# assets no artifact covers — the records themselves excepted.
#
# <record-file> is the release's export_record.json (#955) or /dev/null:
# with one, `execution` names it, its digest, the run and the commit, and
# every export / quantization / repack artifact points at it; without one
# (a release whose assets predate the record) `execution` is null with a
# note saying so, rather than silent.
build_models_json() {
  local tag=$1 repo=$2 sums=$3 sizes=$4 record=$5
  jq -n \
    --arg tag "$tag" --arg repo "$repo" \
    --arg generated "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --slurpfile catalog "$CATALOG" \
    --slurpfile record "$record" \
    --rawfile sums "$sums" --rawfile sizes "$sizes" '
    ($sums | split("\n") | map(select(length > 0) | capture("^(?<hash>[0-9a-f]{64}) [ *](?<name>.+)$"))
      | map({(.name): .hash}) | add // {}) as $hash
    | ($sizes | split("\n") | map(select(length > 0) | split("\t") | {(.[0]): (.[1] | tonumber)})
      | add // {}) as $size
    | $catalog[0] as $cat
    | (($record | length) > 0) as $has_record
    | ($cat.licenses | to_entries | map(.value)) as $license_files
    | ($hash | keys) as $assets
    | ($cat.artifacts | map(
        if .pattern then
          (.pattern) as $re | (.install_dir // "./") as $dir
          | .files = ($assets | map(select(test($re))) | map({asset: ., path: ($dir + .), role: "archive"}))
        else . end
        | .files = (.files | map(select($hash[.asset] != null)
            | . + {sha256: $hash[.asset]} + (if $size[.asset] then {size: $size[.asset]} else {} end)))
        | select(.files | length > 0)
        | del(.pattern, .install_dir)
        | if (.provenance | IN("export", "quantization", "repack")) then
            . + {execution: (if $has_record then "export_record.json" else null end)}
          else . end
      )) as $artifacts
    | ($artifacts | map(.files[].asset)) as $covered
    | ($assets - $covered - ["SHA256SUMS", "models.json", "export_record.json", $cat.notice] - $license_files) as $uncovered
    | if ($uncovered | length) > 0 then
        error("release assets missing from scripts/install/models_catalog.json: " + ($uncovered | join(", ")))
      else . end
    | (if $has_record then {
          record: "export_record.json",
          sha256: $hash["export_record.json"],
          run: $record[0].workflow.url,
          commit: $record[0].workflow.commit,
          generated: $record[0].generated,
          covers: "the artifacts whose provenance is export, quantization or repack — produced by this run, which also fetched the rehost assets at the catalog revisions; what upstream published about each model is its `lineage`"
        } else {
          record: null,
          note: "no export_record.json in the release: its assets predate the execution record (#955), so the run that produced them is not recoverable from the release — the next publish-models.yml run writes one"
        } end) as $execution
    | {
        "$comment": "Digest-bound manifest of the docling.rs models release (scripts/ci/models_manifest.sh, from scripts/install/models_catalog.json). Each artifact lists the release assets that form it, with their sha256 and byte size, the upstream repository and revision the bytes come from, how they were produced, the SPDX licence and its lineage (the upstream model card, the published training records, the producer conversion of a re-hosted file, and what no public record exists for); `execution` is the publish run that produced the export/quantization/repack assets (export_record.json: run, runner, toolchain versions, input and output digests), null with a note when the release predates it; `notice` and `licenses` name the attribution notice and licence texts hosted beside the assets.",
        schema: $cat.schema,
        release: $tag,
        repository: ("https://github.com/" + $repo),
        generated: $generated,
        notice: $cat.notice,
        licenses: $cat.licenses,
        execution: $execution,
        artifacts: $artifacts
      }'
}

# The record files, materialized into <dir>: the notice and the licence
# texts the catalog names, under their asset names.
stage_records() {
  local dir=$1
  cp "$NOTICE_SRC" "$dir/$(jq -r .notice "$CATALOG")"
  jq -r '.licenses | to_entries[] | "\(.key)\t\(.value)"' "$CATALOG" |
    while IFS=$'\t' read -r spdx asset; do
      cp "$LICENSES_SRC/$spdx.txt" "$dir/$asset"
    done
}

if [ "${1:-}" = --check ]; then
  SUMS="${2:?usage: models_manifest.sh --check <SHA256SUMS> [export_record.json]}"
  build_models_json "$(jq -r .release "$CATALOG")" "docling-project/docling.rs" "$SUMS" /dev/null "${3:-/dev/null}"
  exit 0
fi

TAG="${1:?usage: models_manifest.sh <release-tag> | --check <SHA256SUMS>}"
REPO="${GITHUB_REPOSITORY:-docling-project/docling.rs}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

gh api "repos/$REPO/releases/tags/$TAG" \
  --jq '.assets[] | [.name, (.digest // ""), (.size // 0)] | @tsv' > "$tmp/assets.tsv"

: > "$tmp/unsorted"
: > "$tmp/sizes.tsv"
while IFS=$'\t' read -r name digest size; do
  [ -n "$name" ] || continue
  # The records are regenerated below; their previous copies do not count.
  case "$name" in
    SHA256SUMS | models.json | MODELS_NOTICE.md | LICENSE-*) continue ;;
  esac
  case "$digest" in
    sha256:*) hash="${digest#sha256:}" ;;
    *)
      echo "no digest recorded for $name — downloading to hash it" >&2
      mkdir -p "$tmp/dl"
      gh release download "$TAG" --repo "$REPO" --pattern "$name" --dir "$tmp/dl" --clobber
      hash="$(sha256sum "$tmp/dl/$name" | cut -d' ' -f1)"
      rm -f "$tmp/dl/$name"
      ;;
  esac
  printf '%s  %s\n' "$hash" "$name" >> "$tmp/unsorted"
  printf '%s\t%s\n' "$name" "$size" >> "$tmp/sizes.tsv"
done < "$tmp/assets.tsv"

if [ ! -s "$tmp/unsorted" ]; then
  echo "release $TAG of $REPO has no assets — nothing to write" >&2
  exit 1
fi

# The publish run's execution record (#955), when the release hosts one:
# an ordinary asset in SHA256SUMS above, read here so models.json can name
# the run and bind the record by its digest.
record=/dev/null
if grep -q $'^export_record.json\t' "$tmp/assets.tsv"; then
  gh release download "$TAG" --repo "$REPO" --pattern export_record.json --dir "$tmp" --clobber
  record="$tmp/export_record.json"
fi

# The records: models.json from the model assets' digests (fails on an
# uncovered asset, before any upload), the notice and the licence texts.
mkdir -p "$tmp/records"
build_models_json "$TAG" "$REPO" "$tmp/unsorted" "$tmp/sizes.tsv" "$record" > "$tmp/records/models.json"
stage_records "$tmp/records"
for f in "$tmp"/records/*; do
  printf '%s  %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$(basename "$f")" >> "$tmp/unsorted"
done

LC_ALL=C sort -k2 "$tmp/unsorted" > "$tmp/SHA256SUMS"
echo "records for $TAG ($(wc -l < "$tmp/SHA256SUMS" | tr -d ' ') assets):"
cat "$tmp/SHA256SUMS"
jq -r '.artifacts[] | "  \(.id): \(.files | map(.asset) | join(" + "))"' "$tmp/records/models.json"
jq -r '"  execution: \(.execution.record // "none") \(.execution.run // .execution.note)"' "$tmp/records/models.json"
# The records first, so the SHA256SUMS that lands covers the copies that are
# actually hosted.
gh release upload "$TAG" "$tmp"/records/* --repo "$REPO" --clobber
gh release upload "$TAG" "$tmp/SHA256SUMS" --repo "$REPO" --clobber
