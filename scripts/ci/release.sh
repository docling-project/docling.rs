#!/usr/bin/env bash
#
# Master-only release step (run by .github/workflows/ci.yml after the lint/test
# gates pass). Computes the next version from the commits since the last tag
# (scripts/ci/bump_version.sh: fix:/perf:/revert:-only range -> patch, anything
# else -> the 0.x "major"; no automatic 1.0.0); if there is one, it bumps the
# workspace version, commits + tags it and pushes to master. A clean no-op
# when nothing landed since the last tag. Nothing is built, uploaded or
# released here: the GitHub Release for the tag is cut by release.yml when
# the maintainer publishes it (scripts/ci/release_notes.sh writes its notes
# from the tags then), and the crates go to crates.io from
# crates-publish.yml, one of the channels release.yml dispatches
# (scripts/ci/ci_publish.sh).
#
# The release commit is pushed with RELEASE_PAT (an admin token, so it satisfies
# the master branch ruleset) and carries `[skip ci]`, so it does not re-trigger CI.
#
# Set FORCE_VERSION=X.Y.Z to (re)release that exact version instead of computing it
# from the commit history — used to release a version a failed/blocked run skipped.
#
# Requires: push access to master.
# Usage: scripts/ci/release.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

# A forced version is a bare X.Y.Z; a leading "v" (the tag spelling,
# `-f force_version=v2.48.0`) is dropped rather than written into Cargo.toml
# as an invalid "v2.48.0" and tagged "vv2.48.0". Anything else is refused
# before a commit is made.
FORCE_VERSION="${FORCE_VERSION#v}"
if [[ -n "${FORCE_VERSION:-}" && ! "$FORCE_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "FORCE_VERSION must be X.Y.Z (got '$FORCE_VERSION')" >&2
  exit 1
fi
new="${FORCE_VERSION:-$(scripts/ci/bump_version.sh)}"
if [[ -z "$new" ]]; then
  echo "No commits since the last tag — nothing to release."
  exit 0
fi
[[ -n "${FORCE_VERSION:-}" ]] && echo ">> forced release of v$new"

# Commits alone are not enough: only cut a new version when something that a
# release channel publishes actually changed since the last tag. This is what
# stops a docs / CI / demo-only merge (now that every non-fix commit is
# release-worthy) from minting a version with no code change. The channels
# are all dispatched by hand from release.yml, so a tag publishes nothing by
# itself — the maintainer picks crates.io, PyPI, npm, Docker, the binaries —
# which is why the bindings count too: a Python- or Node-only merge (a new
# facade in docling_rs, a napi option) is a PyPI / npm release even though no
# crate on crates.io changed. FORCE_VERSION bypasses the gate (a deliberate
# manual (re)release).
if [[ -z "${FORCE_VERSION:-}" ]]; then
  gate_prev_tag="$(git tag --list 'v*' --sort=-version:refname | head -n1)"
  gate_range="${gate_prev_tag:+$gate_prev_tag..}HEAD"
  # Publishable crates — MUST match scripts/ci/ci_publish.sh. A release fires if
  # one of these crates' Rust source or manifest changed (root Cargo.toml too,
  # for workspace-wide dependency/config changes) …
  gate_crates=(docling-core docling-onnx docling-pdf docling-asr docling docling-serve docling-rag docling-cli)
  gate_paths=(Cargo.toml)
  for c in "${gate_crates[@]}"; do
    gate_paths+=("crates/$c/src" "crates/$c/Cargo.toml" "crates/$c/build.rs")
  done
  # … or a binding package's: the PyPI wheel (pypi-publish.yml), the npm
  # packages (npm-publish.yml) and the C header of the release binaries
  # (cli-binaries.yml). Their tests, READMEs and examples do not count.
  gate_paths+=(
    crates/docling-py/src crates/docling-py/python crates/docling-py/Cargo.toml crates/docling-py/pyproject.toml
    crates/docling-py/mcp/src crates/docling-py/mcp/pyproject.toml
    crates/docling-node/src crates/docling-node/Cargo.toml crates/docling-node/package.json crates/docling-node/index.js crates/docling-node/index.d.ts
    crates/docling-wasm/src crates/docling-wasm/Cargo.toml crates/docling-wasm/npm
    crates/docling-ffi/src crates/docling-ffi/Cargo.toml crates/docling-ffi/include
  )
  if [[ -z "$(git diff --name-only "$gate_range" -- "${gate_paths[@]}")" ]]; then
    echo "Commits since the last tag, but no publishable crate source changed" \
      "since ${gate_prev_tag:-the start of history} — skipping release."
    exit 0
  fi
fi

current="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
echo ">> releasing v$new (was v$current)"

# Bump the single version key in the root manifest's [workspace.package].
sed -i -E "0,/^version = \"[^\"]+\"/ s//version = \"$new\"/" Cargo.toml

# Keep internal path-dependency requirements in lockstep with the workspace
# version, pinned exactly (`=X.Y.Z`): see the script for why.
scripts/ci/sync_internal_versions.sh "$new"

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
# Re-sync the committed Cargo.lock with the bumped workspace versions —
# without this every `--locked` build off master (CI, the Dockerfiles) fails
# from this commit until the next manual `cargo update`. `--workspace` only
# refreshes the workspace members' own entries; third-party pins stay put.
cargo update --workspace --quiet
git add Cargo.toml Cargo.lock crates/*/Cargo.toml
# Commit only if the bump changed something — a forced re-publish of a version
# whose manifests are already at $new has nothing to commit.
if git diff --cached --quiet; then
  echo ">> manifests already at v$new — re-tagging only"
else
  git commit -m "chore(release): v$new [skip ci]"
fi
# Tag only if it doesn't already exist (idempotent for a forced re-release).
if ! git rev-parse -q --verify "refs/tags/v$new" >/dev/null; then
  git tag -a "v$new" -m "v$new"
fi
git push origin HEAD:master
git push origin "v$new"

# Hand the released version to the workflow (the issue comments). Guarded so
# the script still runs locally (where $GITHUB_OUTPUT is unset).
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "released=true"
    echo "version=$new"
  } >>"$GITHUB_OUTPUT"
fi

echo ">> released v$new"
