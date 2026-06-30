#!/usr/bin/env bash
#
# Compare *processing cost* of Python `docling` vs Rust `fleischwolf` for one
# input file: wall-clock time, CPU utilization, and peak memory (RSS).
#
# Usage:
#   scripts/performance.sh <input-file> [runs]
#   scripts/performance.sh tests/data/html/sources/example_07.html 10
#
# Notes:
#   * The Rust side is built in --release and the binary is invoked directly.
#   * The Python side uses the latest published docling, installed from PyPI (see
#     _common.sh), and for declarative formats calls the backend directly.
#     End-to-end timing therefore includes the Python interpreter + import
#     startup, which is real CLI cost but dominates on small inputs — so we also
#     report a "warm" in-process Python number that isolates the conversion work.

set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <input-file> [runs]" >&2
  exit 2
fi

INPUT="$(realpath "$1")"
RUNS="${2:-5}"
TIME_BIN="/usr/bin/time"

if [[ ! -x "$TIME_BIN" ]]; then
  echo "error: GNU time not found at $TIME_BIN (install the 'time' package)" >&2
  exit 1
fi

echo ">> input: $INPUT"
echo ">> runs:  $RUNS"
ensure_docling
echo ">> building Rust release binary ..."
RUST_BIN="$(build_rust_release)"

# Point the Rust PDF pipeline at the fetched libs/models (scripts/pdf_setup.sh)
# using absolute paths, so it runs the full pipeline no matter the caller's CWD.
# Harmless for non-PDF inputs (the binary only reads these for PDFs/images).
[[ -e "$WORKSPACE_DIR/.pdfium/lib/libpdfium.so" ]] && export PDFIUM_DYNAMIC_LIB_PATH="${PDFIUM_DYNAMIC_LIB_PATH:-$WORKSPACE_DIR/.pdfium/lib}"
[[ -e "$WORKSPACE_DIR/models/layout_heron.onnx" ]] && export DOCLING_LAYOUT_ONNX="${DOCLING_LAYOUT_ONNX:-$WORKSPACE_DIR/models/layout_heron.onnx}"
[[ -e "$WORKSPACE_DIR/models/ocr_rec.onnx" ]] && export DOCLING_OCR_REC_ONNX="${DOCLING_OCR_REC_ONNX:-$WORKSPACE_DIR/models/ocr_rec.onnx}"
[[ -e "$WORKSPACE_DIR/models/ppocr_keys_v1.txt" ]] && export DOCLING_OCR_DICT="${DOCLING_OCR_DICT:-$WORKSPACE_DIR/models/ppocr_keys_v1.txt}"

# Run a command RUNS times under GNU time; echo "min avg peak_rss_kb cpu%".
# GNU time format: %e elapsed seconds, %P CPU percent, %M max RSS in KB.
# %e has only centisecond resolution, so for sub-20ms commands we add a
# precision pass that times a batch of invocations and divides (memory/CPU are
# still taken from the real single runs).
bench_process() {
  local tmp e c m
  local -a times=()
  local maxrss=0 cpu=""
  "$@" >/dev/null 2>&1 || true   # warmup (disk cache, .pyc compile)
  for ((i = 0; i < RUNS; i++)); do
    tmp="$(mktemp)"
    "$TIME_BIN" -f "%e %P %M" -o "$tmp" "$@" >/dev/null 2>&1 || true
    read -r e c m <"$tmp" || true
    rm -f "$tmp"
    times+=("$e")
    cpu="$c"
    if [[ "$m" =~ ^[0-9]+$ ]] && ((m > maxrss)); then maxrss="$m"; fi
  done

  local min avg
  read -r min avg < <(printf '%s\n' "${times[@]}" |
    awk '{ s += $1; if (min == "" || $1 < min) min = $1 } END { printf "%.5f %.5f\n", min, s / NR }')

  # Precision pass for fast commands (avg below %e resolution).
  if awk -v a="$avg" 'BEGIN { exit !(a < 0.02) }'; then
    local batch=400 total per
    tmp="$(mktemp)"
    "$TIME_BIN" -f '%e' -o "$tmp" \
      bash -c 'n=$1; shift; for ((i = 0; i < n; i++)); do "$@" >/dev/null 2>&1; done' \
      _ "$batch" "$@" || true
    total="$(cat "$tmp")"
    rm -f "$tmp"
    per="$(awk -v t="$total" -v b="$batch" 'BEGIN { printf "%.5f", t / b }')"
    min="$per"
    avg="$per"
  fi

  printf "%s %s %d %s\n" "$min" "$avg" "$maxrss" "$cpu"
}

# Warm, in-process Python: import once, then time RUNS conversions; report
# avg seconds/conversion and peak RSS (KB).
bench_python_warm() {
  "$PYBIN" - "$INPUT" "$PY_RUNNER" "$RUNS" <<'PY'
import importlib.util, resource, sys, time
from pathlib import Path

inp, runner, runs = sys.argv[1], sys.argv[2], int(sys.argv[3])
spec = importlib.util.spec_from_file_location("docling_convert", runner)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

path = Path(inp)
mod.convert_to_markdown(path)  # warmup + trigger lazy imports
start = time.perf_counter()
for _ in range(runs):
    mod.convert_to_markdown(path)
avg = (time.perf_counter() - start) / runs
rss_kb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
print(f"{avg:.6f} {rss_kb}")
PY
}

# The single `.venv-compare` env (installed by `ensure_docling` above) handles
# every format, so the pipeline label is driven by the input format, not by which
# env answered: a PDF/image/audio input goes through docling's full ML pipeline
# (layout + tables + OCR), every other format through the torch-free declarative
# backend. A probe gates whether a head-to-head is shown at all — some formats the
# locally-installed docling can't convert, in which case we report Rust-only.
case "${INPUT,,}" in
  *.pdf | *.png | *.jpg | *.jpeg | *.tif | *.tiff | *.bmp | *.webp | *.gif | *.wav | *.mp3 | *.flac | *.m4a)
    PY_KIND="full ML pipeline (layout + tables + OCR)" ;;
  *)
    PY_KIND="declarative backend (no torch)" ;;
esac
PY_OK=1
probe="$(mktemp)"
if ! "$PYBIN" "$PY_RUNNER" "$INPUT" "$probe" >/dev/null 2>&1 || [[ ! -s "$probe" ]]; then
  PY_OK=0
fi
rm -f "$probe"

echo ">> measuring Rust fleischwolf (end-to-end process) ..."
read -r rs_min rs_avg rs_rss rs_cpu < <(bench_process "$RUST_BIN" "$INPUT")

if [[ "$PY_OK" -eq 1 ]]; then
  echo ">> measuring Python docling [$PY_KIND] (end-to-end process) ..."
  read -r py_min py_avg py_rss py_cpu < <(bench_process "$PYBIN" "$PY_RUNNER" "$INPUT")
  echo ">> measuring Python docling (warm, in-process) ..."
  read -r pyw_avg pyw_rss < <(bench_python_warm 2>/dev/null) || { pyw_avg=""; pyw_rss=0; }
  # Rust warm conversion (pipeline loaded once, startup excluded) — the fair
  # counterpart to Python's warm number. Only the PDF/image pipeline supports it.
  rsw_avg=""
  if [[ "$PY_KIND" == full* ]]; then
    echo ">> measuring Rust fleischwolf (warm, in-process) ..."
    rsw_avg=$("$RUST_BIN" --bench-warm "$RUNS" "$INPUT" 2>/dev/null || echo "")
  fi
fi

mb() { awk -v k="$1" 'BEGIN { printf "%.1f", k / 1024 }'; }
ratio() { awk -v a="$1" -v b="$2" 'BEGIN { if (b > 0 && a != "") printf "%.1f", a / b; else printf "n/a" }'; }
fmtt() { awk -v v="$1" 'BEGIN { printf "%.4g", v }'; }

if [[ "$PY_OK" -eq 1 ]]; then
  echo
  echo "================ end-to-end (whole process) ================"
  printf "%-22s %8s %10s %10s %8s %12s\n" "ENGINE" "RUNS" "TIME-min" "TIME-avg" "CPU" "PEAK-MEM"
  printf "%-22s %8s %9ss %9ss %8s %9s MB\n" "docling (python)"   "$RUNS" "$(fmtt "$py_min")" "$(fmtt "$py_avg")" "$py_cpu" "$(mb "$py_rss")"
  printf "%-22s %8s %9ss %9ss %8s %9s MB\n" "fleischwolf (rust)" "$RUNS" "$(fmtt "$rs_min")" "$(fmtt "$rs_avg")" "$rs_cpu" "$(mb "$rs_rss")"
  echo
  echo "  wall-time speedup (avg):  $(ratio "$py_avg" "$rs_avg")x faster (rust)"
  echo "  peak-memory ratio:        $(ratio "$py_rss" "$rs_rss")x less (rust)"
  echo
  echo "================ conversion only (startup excluded) ========"
  printf "  python (warm, in-process): %ss/doc, peak %s MB\n" "$(fmtt "$pyw_avg")" "$(mb "$pyw_rss")"
  if [[ -n "$rsw_avg" ]]; then
    printf "  rust   (warm, in-process): %ss/doc\n" "$(fmtt "$rsw_avg")"
    echo "  warm-conversion speedup:   $(ratio "$pyw_avg" "$rsw_avg")x faster (rust)"
  else
    printf "  rust   (whole process incl. startup): %ss/doc\n" "$(fmtt "$rs_avg")"
    echo "  warm-conversion speedup:   $(ratio "$pyw_avg" "$rs_avg")x faster (rust) [rust incl. startup]"
  fi
  echo
  if [[ "$PY_KIND" == full* ]]; then
    echo "Note: this PDF/image head-to-head runs docling's full pipeline (layout +"
    echo "tables + OCR). The end-to-end figures re-pay process startup (torch import +"
    echo "cold ONNX model load) on every run; the warm figures load the pipeline once"
    echo "and time conversion only, so warm-vs-warm is the fair conversion-speed"
    echo "comparison. (Rust warm = 'fleischwolf --bench-warm'.)"
  else
    echo "Note: Python end-to-end time includes interpreter + import startup"
    echo "(~0.3-0.6s), which dominates on small inputs. The warm number isolates the"
    echo "actual parse/convert work; use larger inputs to see steady-state behavior."
  fi
else
  echo
  echo "================ fleischwolf (rust) — whole process ================"
  printf "%-22s %8s %10s %10s %8s %12s\n" "ENGINE" "RUNS" "TIME-min" "TIME-avg" "CPU" "PEAK-MEM"
  printf "%-22s %8s %9ss %9ss %8s %9s MB\n" "fleischwolf (rust)" "$RUNS" "$(fmtt "$rs_min")" "$(fmtt "$rs_avg")" "$rs_cpu" "$(mb "$rs_rss")"
  echo
  echo "Note: the local Python docling (.venv-compare) can't convert this format —"
  echo "PDF/images/audio need the full torch/ML pipeline it omits — so no head-to-head"
  echo "is shown. The Rust figure runs the complete pipeline (pdfium + layout + OCR)."
fi
