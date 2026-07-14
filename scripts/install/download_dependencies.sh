#!/usr/bin/env sh
# Fetch the PDF/image ML pipeline's native dependencies — pdfium + the ONNX
# models (layout, OCR, TableFormer) — from this repo's GitHub Releases,
# straight into the current directory. No npm, no Python, no env vars needed
# afterwards: both the Rust CLI and the Node.js/Bun bindings look for
# `models/` and `.pdfium/lib` relative to the process's current directory by
# default.
#
# Run from your app's directory (or a checkout of this repo):
#   scripts/install/download_dependencies.sh
# or, without a checkout:
#   curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh
#
# Then either:
#   cargo run -p docling-cli -- <file>
# or:
#   npm i docling.rs
#   node -e "import { convertFileAsync } from 'docling.rs'; const r = await convertFileAsync('example.pdf', { to: 'markdown' }); console.log(r.content) "
#
# Downloads (from https://github.com/docling-project/docling.rs/releases, tag
# models-v1 by default — override the base with $DOCLING_RS_MODELS_URL):
#   .pdfium/lib/libpdfium.so                      (Linux x64)
#   models/layout_heron.onnx
#   models/ocr_rec.onnx
#   models/ppocr_keys_v1.txt
#   models/tableformer/encoder.onnx (+ .data, if the export needs it)
#   models/tableformer/decoder.onnx (+ .data, if the export needs it)
#   models/tableformer/bbox.onnx (+ .data, if the export needs it)
#   models/asr/{encoder_model,decoder_model}.onnx + vocab.json   (Whisper tiny,
#     from Hugging Face; skip with --no-asr)
#   models/chunk/tokenizer.json                   (all-MiniLM-L6-v2's tokenizer,
#     the HybridChunker's default token counter; falls back to Hugging Face when
#     the release doesn't host it; skip with --no-chunk)
#   models/picture_classifier.onnx                (DocumentFigureClassifier-v2.5,
#     the --enrich-picture-classes model, ~17 MB; falls back to Hugging Face when
#     the release doesn't host it)
#   models/code_formula/{vision,embed,decoder_kv}.onnx + tokenizer.json
#     (CodeFormulaV2, the --enrich-code/--enrich-formula VLM, ~1.3 GB fp32 —
#     opt-in with --enrich; release-hosted only. With int8 enabled the ~165 MB
#     decoder_kv_int8.onnx replaces the ~655 MB fp32 decoder)
#
# Also fetches the INT8-quantized CPU models when the release hosts them (see
# PDF_CONFORMANCE.md — ~2.4x faster layout inference at unchanged conformance):
#   models/layout_heron_int8.onnx
#   models/tableformer/decoder_int8.onnx
# The pipeline picks these up automatically when they sit next to the fp32
# files (no env vars needed); set DOCLING_RS_FP32=1 at runtime to force full
# precision, or skip fetching them entirely with --no-int8. If the release
# doesn't host the int8 assets (older tag), a note explains how to produce
# them locally with scripts/install/quantize_models.py.
#
# pdfium is Linux x64 only for now, matching what's hosted in the release; for
# other platforms (or to build the models from source) see scripts/install/pdf_setup.sh.
#
# Idempotent: skips files already on disk. Pass --force to re-fetch everything.
set -eu

BASE_URL="${DOCLING_RS_MODELS_URL:-https://github.com/docling-project/docling.rs/releases/download/models-v1}"
# Whisper tiny (docling's ASR default) for the audio pipeline, fetched straight
# from the onnx-community export on Hugging Face (~150 MB). Override the base
# with $DOCLING_RS_ASR_MODELS_URL (e.g. to re-host alongside the other models);
# skip entirely with --no-asr.
ASR_BASE_URL="${DOCLING_RS_ASR_MODELS_URL:-https://huggingface.co/onnx-community/whisper-tiny/resolve/main}"

FORCE=false
WITH_ASR=true
WITH_INT8=true
WITH_CHUNK=true
WITH_ENRICH=false
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=true ;;
    --no-asr) WITH_ASR=false ;;
    --int8) WITH_INT8=true ;; # accepted for compatibility; int8 is the default
    --no-int8) WITH_INT8=false ;;
    --no-chunk) WITH_CHUNK=false ;;
    --enrich) WITH_ENRICH=true ;;
    *)
      echo "usage: download_dependencies.sh [--force] [--no-asr] [--no-int8] [--no-chunk] [--enrich]" >&2
      exit 2
      ;;
  esac
done

if ! command -v curl >/dev/null 2>&1; then
  echo "error: curl is required" >&2
  exit 1
fi

mkdir -p .pdfium/lib models/tableformer
if [ "$WITH_ASR" = true ]; then
  mkdir -p models/asr
fi

fetch() { # <url> <dest>
  if [ "$FORCE" = false ] && [ -f "$2" ]; then
    echo "  = $2 (already present)"
    return 0
  fi
  echo "  > $2"
  curl -fsSL -o "$2.download" "$1"
  mv "$2.download" "$2"
}

fetch_optional() { # <url> <dest> — ignore a missing/failed asset (sidecar files)
  if [ "$FORCE" = false ] && [ -f "$2" ]; then
    return 0
  fi
  if curl -fsSL -o "$2.download" "$1" 2>/dev/null; then
    mv "$2.download" "$2"
    echo "  > $2"
  else
    rm -f "$2.download"
  fi
}

echo "fetching docling.rs ML dependencies from $BASE_URL"
fetch "$BASE_URL/libpdfium.so" .pdfium/lib/libpdfium.so
fetch "$BASE_URL/layout_heron.onnx" models/layout_heron.onnx
fetch "$BASE_URL/ocr_rec.onnx" models/ocr_rec.onnx
fetch "$BASE_URL/ppocr_keys_v1.txt" models/ppocr_keys_v1.txt
fetch "$BASE_URL/encoder.onnx" models/tableformer/encoder.onnx
fetch_optional "$BASE_URL/encoder.onnx.data" models/tableformer/encoder.onnx.data
fetch "$BASE_URL/decoder.onnx" models/tableformer/decoder.onnx
fetch_optional "$BASE_URL/decoder.onnx.data" models/tableformer/decoder.onnx.data
fetch "$BASE_URL/bbox.onnx" models/tableformer/bbox.onnx
fetch_optional "$BASE_URL/bbox.onnx.data" models/tableformer/bbox.onnx.data

if [ "$WITH_ASR" = true ]; then
  # Whisper tiny for audio/ASR: encoder + (cache-less) decoder + vocabulary;
  # added_tokens.json only feeds non-English language selection, so a missing
  # asset there is not fatal.
  fetch "$ASR_BASE_URL/onnx/encoder_model.onnx" models/asr/encoder_model.onnx
  fetch "$ASR_BASE_URL/onnx/decoder_model.onnx" models/asr/decoder_model.onnx
  fetch "$ASR_BASE_URL/vocab.json" models/asr/vocab.json
  fetch_optional "$ASR_BASE_URL/added_tokens.json" models/asr/added_tokens.json
fi

if [ "$WITH_CHUNK" = true ]; then
  # The hybrid chunker's default tokenizer (all-MiniLM-L6-v2's tokenizer.json,
  # ~0.5 MB). The CLI (`--to chunks`), the Node/Python bindings and docling-rag
  # all pick it up at models/chunk/tokenizer.json when no explicit path is
  # given. Fetched from the release when hosted (newer tags), else straight
  # from Hugging Face.
  mkdir -p models/chunk
  fetch_optional "$BASE_URL/chunk_tokenizer.json" models/chunk/tokenizer.json
  if [ ! -f models/chunk/tokenizer.json ]; then
    fetch "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json" \
      models/chunk/tokenizer.json
  fi
fi

# DocumentFigureClassifier (picture classification enrichment, ~17 MB): the
# `--enrich-picture-classes` / `do_picture_classification` model. Small, so
# fetched by default — from the release when hosted, else the upstream ONNX
# straight from Hugging Face (docling-project/DocumentFigureClassifier-v2.5
# ships the graph itself).
fetch_optional "$BASE_URL/picture_classifier.onnx" models/picture_classifier.onnx
if [ ! -f models/picture_classifier.onnx ]; then
  fetch "https://huggingface.co/docling-project/DocumentFigureClassifier-v2.5/resolve/main/model.onnx" \
    models/picture_classifier.onnx
fi

if [ "$WITH_ENRICH" = true ]; then
  # CodeFormulaV2 (code/formula enrichment, ~1.3 GB fp32): the
  # `--enrich-code`/`--enrich-formula` VLM, exported to ONNX by
  # scripts/install/export_code_formula.py and hosted with the release
  # (there is no upstream ONNX export to fall back to). Opt-in (--enrich)
  # because of its size.
  mkdir -p models/code_formula
  fetch "$BASE_URL/cf_vision.onnx" models/code_formula/vision.onnx
  fetch "$BASE_URL/cf_embed.onnx" models/code_formula/embed.onnx
  fetch "$BASE_URL/cf_tokenizer.json" models/code_formula/tokenizer.json
  if [ "$WITH_INT8" = true ]; then
    # INT8 decoder (~165 MB vs ~655 MB fp32) — preferred automatically when
    # present. Near-exact, not byte-exact: greedy near-tie tokens can flip
    # (whitespace-only drift on the conformance fixture); fetch with --no-int8
    # or set DOCLING_RS_FP32=1 at runtime for the byte-exact fp32 graph.
    fetch_optional "$BASE_URL/cf_decoder_kv_int8.onnx" models/code_formula/decoder_kv_int8.onnx
  fi
  if [ "$WITH_INT8" = true ] && [ -f models/code_formula/decoder_kv_int8.onnx ]; then
    echo "code_formula: int8 decoder present — fp32 decoder_kv.onnx not needed (skipped)"
  else
    fetch "$BASE_URL/cf_decoder_kv.onnx" models/code_formula/decoder_kv.onnx
  fi
fi

if [ "$WITH_INT8" = true ]; then
  # INT8-quantized CPU models (optional release assets). The pipeline prefers
  # them automatically when they sit at the default paths; DOCLING_RS_FP32=1
  # forces the fp32 models at runtime.
  fetch_optional "$BASE_URL/layout_heron_int8.onnx" models/layout_heron_int8.onnx
  fetch_optional "$BASE_URL/decoder_int8.onnx" models/tableformer/decoder_int8.onnx
  if [ -f models/layout_heron_int8.onnx ]; then
    echo "int8 models present — used by default (DOCLING_RS_FP32=1 forces full precision)"
  else
    echo "int8 assets not hosted at $BASE_URL — the fp32 models will be used."
    echo "To build the int8 models locally (see PDF_CONFORMANCE.md):"
    echo "  pip install onnx onnxruntime sympy pypdfium2 pillow numpy"
    echo "  python scripts/install/quantize_models.py"
  fi
fi

echo "done — models/ and .pdfium/lib populated in $(pwd)"
