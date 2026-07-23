#!/usr/bin/env bash
# VLM corpus comparison (#153): run the PDF corpus through docling.rs's
# `--pipeline vlm` AND Python docling's VlmPipeline — both against the SAME
# OpenAI-compatible endpoint (scripts/dev/granite_vlm_server.py keeps
# granite-docling's DocTags tokens intact; llama.cpp-family servers strip
# them) — and report per-fixture similarity between the two Markdown outputs.
#
# This is a measurement, not a gate: model output legitimately varies across
# checkpoints/servers, so the script always exits 0 and prints numbers for
# docs/PDF_CONFORMANCE.md. Outputs are kept under target/vlm-conformance/ for
# triage (rust/ vs python/ per fixture).
#
#   DOCLING_RS_VLM_ENDPOINT=http://localhost:8000/v1  (default)
#   DOCLING_RS_VLM_MODEL=granite-docling              (default)
#   FIXTURES="a.pdf b.pdf"   restrict to specific corpus files (basenames)
#
# Prereqs: the shim serving (see its header for the venv recipe), pdfium for
# the Rust side, and Python docling with VLM extras for the reference side
# (scripts/conformance/setup-docling.sh).
set -euo pipefail
cd "$(dirname "$0")/../.."   # docling.rs/

ENDPOINT="${DOCLING_RS_VLM_ENDPOINT:-http://localhost:8000/v1}"
MODEL="${DOCLING_RS_VLM_MODEL:-granite-docling}"
OUT="target/vlm-conformance"
export PDFIUM_DYNAMIC_LIB_PATH="${PDFIUM_DYNAMIC_LIB_PATH:-$(pwd)/.pdfium/lib}"

# Reachable? Any HTTP status will do (the shim only answers POST); only a
# connection failure means "no server".
if ! curl -s -o /dev/null --max-time 5 -X POST "$ENDPOINT/chat/completions" -d '{}'; then
  echo "no VLM endpoint at $ENDPOINT — start scripts/dev/granite_vlm_server.py first" >&2
  exit 1
fi
python3 -c "import docling" 2>/dev/null || {
  echo "python docling not importable — run scripts/conformance/setup-docling.sh" >&2
  exit 1
}

[ -x target/release/docling-rs ] || cargo build --release -q -p docling-cli
mkdir -p "$OUT/rust" "$OUT/python"

# Whitespace-normalized character-level similarity of two Markdown files,
# 0–100 (line-level scoring would zero a whole paragraph over one differing
# word). Model output is not byte-stable across renderers, so exactness is
# reported separately.
similarity() {
  python3 - "$1" "$2" <<'EOF'
import difflib, re, sys
def norm(p):
    text = open(p, encoding="utf-8").read()
    return "\n".join(
        re.sub(r"\s+", " ", l).strip() for l in text.splitlines() if l.strip()
    )
a, b = norm(sys.argv[1]), norm(sys.argv[2])
print(f"{difflib.SequenceMatcher(a=a, b=b, autojunk=False).ratio() * 100:.1f}")
EOF
}

total=0; exact=0; sum="0"
printf "%-38s %8s %s\n" "fixture" "sim%" "exact"
for src in tests/data/pdf/sources/*.pdf; do
  name="$(basename "$src")"
  if [ -n "${FIXTURES:-}" ] && ! grep -qw "$name" <<<"$FIXTURES"; then
    continue
  fi
  rust_md="$OUT/rust/$name.md"
  py_md="$OUT/python/$name.md"
  # Cached across reruns (each side costs minutes of GPU time PER PAGE —
  # multi-page fixtures take a while; the shim's terminal shows per-page
  # progress). Writes go through a temp file so an interrupted run never
  # caches a half-written output; delete target/vlm-conformance/ to force
  # regeneration.
  if [ ! -s "$rust_md" ]; then
    echo "[$name] rust side converting (watch the shim terminal for per-page progress) ..." >&2
    if ./target/release/docling-rs --pipeline vlm --no-stream \
      --vlm-endpoint "$ENDPOINT" --vlm-model "$MODEL" "$src" > "$rust_md.tmp"; then
      mv "$rust_md.tmp" "$rust_md"
    else
      echo "  rust side failed on $name" >&2
      rm -f "$rust_md.tmp"
      continue
    fi
  fi
  if [ ! -s "$py_md" ]; then
    echo "[$name] python docling side converting ..." >&2
    if python3 scripts/conformance/vlm_convert.py \
      --endpoint "$ENDPOINT" --model "$MODEL" "$src" "$py_md.tmp"; then
      mv "$py_md.tmp" "$py_md"
    else
      echo "  python side failed on $name" >&2
      rm -f "$py_md.tmp"
      continue
    fi
  fi
  sim="$(similarity "$py_md" "$rust_md")"
  is_exact="no"
  diff -q "$py_md" "$rust_md" >/dev/null 2>&1 && { is_exact="yes"; exact=$((exact + 1)); }
  printf "%-38s %8s %s\n" "$name" "$sim" "$is_exact"
  total=$((total + 1))
  sum="$(python3 -c "print($sum + $sim)")"
done

if [ "$total" -gt 0 ]; then
  mean="$(python3 -c "print(f'{$sum / $total:.1f}')")"
  echo "VLM corpus similarity (rust vs python docling, $MODEL): mean $mean% over $total fixtures, $exact byte-exact"
  echo "outputs kept in $OUT/ for triage"
fi
