#!/usr/bin/env bash
#
# CPU-vs-GPU benchmark over the PDF corpus (issue #108): per-file wall time
# for each execution provider plus an output-equivalence check (the GPU run
# must produce the same markdown as the CPU run — both use the fp32 models,
# so any drift is EP-kernel-level and we want to see it, not average it away).
#
#   scripts/test/gpu_benchmark.sh [runs-per-file]     # default: 3
#
# Requires a `--features cuda` (or other EP) release build and the fetched
# models. Writes per-file outputs, a CSV, and a ready-to-paste markdown table
# to bench-gpu/. Environment overrides:
#   DOCLING_RS_BENCH_EPS   providers to time (default: "cpu cuda")
#   DOCLING_RS_BENCH_GLOB  extra corpus glob(s) appended to the PDF sources
#   DOCLING_RS_BENCH_ONLY  regex filter on file basenames (quick spot checks)
set -uo pipefail
cd "$(dirname "$0")/../.."

RUNS="${1:-3}"
EPS="${DOCLING_RS_BENCH_EPS:-cpu cuda}"
BIN=target/release/docling-rs
OUT=bench-gpu
[ -x "$BIN" ] || { echo "build first: cargo build --release -p docling-cli --features cuda" >&2; exit 1; }
mkdir -p "$OUT"

pdfs=(tests/data/pdf/sources/*.pdf)
[ -d tests/data/scanned/sources ] && pdfs+=(tests/data/scanned/sources/*.pdf)
if [ -n "${DOCLING_RS_BENCH_GLOB:-}" ]; then
  # shellcheck disable=SC2206
  pdfs+=($DOCLING_RS_BENCH_GLOB)
fi
if [ -n "${DOCLING_RS_BENCH_ONLY:-}" ]; then
  filtered=()
  for pdf in "${pdfs[@]}"; do
    [[ "$(basename "$pdf")" =~ ${DOCLING_RS_BENCH_ONLY} ]] && filtered+=("$pdf")
  done
  pdfs=("${filtered[@]}")
  [ ${#pdfs[@]} -gt 0 ] || { echo "no corpus files match DOCLING_RS_BENCH_ONLY" >&2; exit 1; }
fi

# --- environment header (goes verbatim into the report) ---------------------
{
  echo "## Environment"
  echo
  echo '```'
  date -u +"%Y-%m-%d %H:%M UTC"
  if command -v nvidia-smi >/dev/null 2>&1; then
    nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
  else
    echo "nvidia-smi: not found"
  fi
  grep -m1 "model name" /proc/cpuinfo | sed 's/model name[[:space:]]*: //'
  echo "$(nproc) logical cores"
  echo "providers: $EPS · $RUNS runs/file · best-of taken; run 1 reported as cold"
  echo '```'
  echo
} | tee "$OUT/report.md"

# --- timing + equivalence ---------------------------------------------------
# Monotonic clock: wall-clock (`date`) can step backwards under NTP sync
# (observed on a laptop: a -29s "measurement"), /proc/uptime cannot.
now_ms() { awk '{printf "%d", $1 * 1000}' /proc/uptime; }

csv="$OUT/results.csv"
echo "file,ep,cold_s,best_s" > "$csv"

declare -A BEST COLD
for pdf in "${pdfs[@]}"; do
  base="$(basename "$pdf" .pdf)"
  for ep in $EPS; do
    best=""; cold=""
    for r in $(seq "$RUNS"); do
      t0=$(now_ms)
      if ! DOCLING_RS_EP="$ep" "$BIN" "$pdf" > "$OUT/$base.$ep.md" 2> "$OUT/$base.$ep.err"; then
        echo "FAILED: $pdf ($ep) — see $OUT/$base.$ep.err" >&2
        best="fail"; cold="fail"; break
      fi
      t=$(( $(now_ms) - t0 ))
      [ "$r" = 1 ] && cold=$t
      [ -z "$best" ] || [ "$t" -lt "$best" ] && best=$t
    done
    BEST["$base.$ep"]=$best; COLD["$base.$ep"]=$cold
    if [ "$best" != "fail" ]; then
      printf "%s,%s,%d.%03d,%d.%03d\n" "$base" "$ep" \
        $((cold / 1000)) $((cold % 1000)) $((best / 1000)) $((best % 1000)) >> "$csv"
    else
      echo "$base,$ep,fail,fail" >> "$csv"
    fi
    echo "  $base [$ep] cold ${cold}ms best ${best}ms"
  done
done

# --- report -----------------------------------------------------------------
first_ep=${EPS%% *}
{
  echo "## Results (best of $RUNS, seconds; cold = run 1 incl. model/EP init)"
  echo
  hdr="| file |"; sep="|---|"
  for ep in $EPS; do hdr+=" $ep cold | $ep best |"; sep+="---|---|"; done
  [ "$EPS" != "$first_ep" ] && { hdr+=" speedup (best) | output |"; sep+="---|---|"; }
  echo "$hdr"; echo "$sep"

  total_first=0; total_last=0; last_ep=${EPS##* }
  for pdf in "${pdfs[@]}"; do
    base="$(basename "$pdf" .pdf)"
    row="| $base |"
    for ep in $EPS; do
      b=${BEST["$base.$ep"]}; c=${COLD["$base.$ep"]}
      if [ "$b" = "fail" ]; then row+=" fail | fail |"; continue; fi
      row+=" $(awk "BEGIN{printf \"%.2f\", $c/1000}") | $(awk "BEGIN{printf \"%.2f\", $b/1000}") |"
    done
    if [ "$EPS" != "$first_ep" ]; then
      bf=${BEST["$base.$first_ep"]}; bl=${BEST["$base.$last_ep"]}
      if [ "$bf" != "fail" ] && [ "$bl" != "fail" ]; then
        row+=" $(awk "BEGIN{printf \"%.2fx\", $bf/$bl}") |"
        total_first=$((total_first + bf)); total_last=$((total_last + bl))
        if cmp -s "$OUT/$base.$first_ep.md" "$OUT/$base.$last_ep.md"; then
          row+=" identical |"
        else
          row+=" **$(diff "$OUT/$base.$first_ep.md" "$OUT/$base.$last_ep.md" | grep -c '^[<>]') diff lines** |"
        fi
      else
        row+=" — | — |"
      fi
    fi
    echo "$row"
  done

  if [ "$EPS" != "$first_ep" ] && [ "$total_last" -gt 0 ]; then
    echo
    echo "**Corpus total (best): $first_ep $(awk "BEGIN{printf \"%.1f\", $total_first/1000}")s · $last_ep $(awk "BEGIN{printf \"%.1f\", $total_last/1000}")s · speedup $(awk "BEGIN{printf \"%.2fx\", $total_first/$total_last}")**"
  fi
} | tee -a "$OUT/report.md"

echo
echo "report: $OUT/report.md · raw: $csv · outputs kept in $OUT/ for diffing"
