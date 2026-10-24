#!/usr/bin/env sh
# Fetch the PDF/image ML pipeline's dependencies — the ONNX models (layout,
# OCR, TableFormer) — from this repo's GitHub Releases, straight into the
# current directory. No npm, no Python, no env vars needed afterwards: both
# the Rust CLI and the Node.js/Bun bindings look for `.models/` relative to
# the process's current directory by default. The PDF pages themselves are
# rendered in pure Rust — no native PDF library is fetched.
#
# Run from your app's directory (or a checkout of this repo):
#   scripts/install/download_dependencies.sh
# or, without a checkout:
#   curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh
# or, for a `cargo install docling-cli` binary (--cli: into the cargo install
# root, where that binary finds them from any directory):
#   curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/download_dependencies.sh | sh -s -- --cli
#
# Then either:
#   cargo run -p docling-cli -- <file>
# or:
#   npm i docling.rs
#   node -e "import { convertFileAsync } from 'docling.rs'; const r = await convertFileAsync('example.pdf', { to: 'markdown' }); console.log(r.content) "
#
# Downloads (from https://github.com/docling-project/docling.rs/releases, tag
# models-v1 by default — override the base with $DOCLING_RS_MODELS_URL):
#   .models/layout_heron.onnx
#   .models/ocr_rec_v6.onnx + .models/ocr_rec_v6_dict.txt (PP-OCRv6 recognition,
#     multilingual — RapidOCR's / docling's recognizer and the runtime default
#     when present, #570; from the release when the tag mirrors it, else from
#     RapidOCR's model hub)
#   .models/ocr_rec_en.onnx + .models/en_dict.txt   (English PP-OCRv3
#     recognition — the fallback without the v6 pair; from the release when the
#     tag mirrors it, else straight from upstream PP-OCRv3 hosting)
#   .models/ocr_rec.onnx + .models/ppocr_keys_v1.txt (multilingual ch_ pair —
#     what docling conformance is measured with; DOCLING_RS_OCR_LANG=ch)
#   .models/ocr_det.onnx (PP-OCRv6 text detector, #429 — reads lines the
#     layout model gives no region; from the release when the tag mirrors it,
#     else from RapidOCR's model hub)
#   .models/ocr_textline_ori.onnx (PP-LCNet text-line orientation classifier,
#     #666 — ocr_textline_orientation's 0/180 check; from the release when the
#     tag mirrors it, else from RapidOCR's model hub)
#   .models/tableformer/encoder.onnx (+ .data, if the export needs it)
#   .models/tableformer/decoder.onnx (+ .data, if the export needs it)
#   .models/tableformer/decoder_kv.onnx (+ .data; preferred when hosted)
#   .models/tableformer/decoder_kv_layers.onnx (its per-layer-cache graph over
#     the same .data, #871; preferred over it when hosted)
#   .models/tableformer/bbox.onnx (+ .data, if the export needs it)
#   .models/asr/{encoder_model,decoder_model}.onnx + vocab.json   (Whisper tiny,
#     from the release when the tag mirrors it, else Hugging Face; skip with
#     --no-asr)
#   .models/asr/<preset>/…  (--asr-model=<preset>, repeatable: the Whisper
#     presets, or parakeet_tdt_0.6b_v3 — NVIDIA Parakeet TDT 0.6B v3, #508:
#     int8 encoder + decoder-joint ~670 MB, fp32 ~2.5 GB with --no-int8 — plus
#     the Silero VAD into .models/asr/vad/silero_vad.onnx)
#   .models/chunk/tokenizer.json                   (all-MiniLM-L6-v2's tokenizer,
#     the HybridChunker's default token counter; falls back to Hugging Face when
#     the release doesn't host it; skip with --no-chunk)
#   .models/picture_classifier.onnx                (DocumentFigureClassifier-v2.5,
#     the --enrich-picture-classes model, ~17 MB; falls back to Hugging Face when
#     the release doesn't host it)
#   .models/code_formula/{vision,embed,decoder_kv}.onnx + tokenizer.json
#     (CodeFormulaV2, the --enrich-code/--enrich-formula VLM, ~1.3 GB fp32 —
#     opt-in with --enrich; release-hosted only. With int8 enabled the ~165 MB
#     decoder_kv_int8.onnx replaces the ~655 MB fp32 decoder)
#   .models/ner/{model.onnx,tokenizer.json,config.json}  (dslim/bert-base-NER's
#     ONNX export, MIT, ~430 MB — the names / organizations / locations
#     detector of the --redact-pii pass, #621; opt-in with --with-ner. From
#     Hugging Face, the release mirror first when it hosts it; override the
#     host with $DOCLING_RS_NER_MODELS_URL)
#   .models/embed/bge-m3.onnx + model.onnx.data + tokenizer.json   (bge-m3 for
#     docling-rag's local ONNX embedder, ~2.3 GB — opt-in with --embed; from
#     Hugging Face, matching the RAG_EMBED_ONNX_PATH/RAG_EMBED_TOKENIZER
#     defaults)
#
# Also fetches the INT8-quantized CPU models when the release hosts them (see
# docs/PDF_CONFORMANCE.md — ~2.4x faster layout inference at unchanged conformance):
#   .models/layout_heron_int8.onnx
#   .models/tableformer/decoder_int8.onnx
#   .models/tableformer/encoder_fp16.onnx   (fp16-weight repack, fp32 compute; #374)
# The pipeline picks these up automatically when they sit next to the fp32
# files (no env vars needed); set DOCLING_RS_FP32=1 at runtime to force full
# precision, or skip fetching them entirely with --no-int8. If the release
# doesn't host the int8 assets (older tag), a note explains how to produce
# them locally with scripts/install/quantize_models.py.
#
# --with-docling-parse fetches docling-parse's renderer plugin
# (.docling-parse/lib + pdf_resources) — the development oracle the PDF
# conformance scripts run with DOCLING_RS_RENDERER=docling-parse; the pipeline
# itself never loads it. Building the models from source: see
# scripts/install/pdf_setup.sh.
#
# --with-onnxruntime fetches the ONNX Runtime shared library for IBM Z into
# .models/onnxruntime/ (#504; automatic on an s390x host, where no prebuilt
# runtime is linked in and the `ort-load-dynamic` build dlopens this one).
# --with-fonts drops the Liberation and DejaVu families into .models/fonts —
# the faces the pure-Rust page renderer substitutes for fonts a PDF does not
# embed (the base-14 Helvetica/Times/Courier of most office exports). Only
# needed where the host has no fonts of its own: a slim container, a bare
# CI runner. Linux desktops, macOS and Windows already carry fonts the
# renderer scans (`fonts-liberation`/`fonts-dejavu-core` packages, Arial /
# Times / Courier); DOCLING_RS_FONT_DIRS adds more directories at runtime.
#
# --cli installs into the cargo install root instead of the current directory
# — `$CARGO_INSTALL_ROOT`, else `$CARGO_HOME`, else `~/.cargo` — whose bin/
# is where `cargo install docling-cli` puts the binary: the CLI looks for
# .models/ next to its executable and one level above it
# (crates/docling-core/src/assets.rs), so `<root>/.models` is found whatever
# the working directory.
#
# Idempotent: skips files already on disk. Pass --force to re-fetch everything.
#
# Integrity: every asset fetched from the release is checked against its
# SHA256SUMS (scripts/ci/models_manifest.sh writes it after each upload)
# before it is moved into place — see "integrity" below and
# DOCLING_RS_MODELS_VERIFY (auto | require | off).
set -eu

BASE_URL="${DOCLING_RS_MODELS_URL:-https://github.com/docling-project/docling.rs/releases/download/models-v1}"
# Whisper tiny (docling's ASR default) for the audio pipeline: the
# onnx-community export (~150 MB), mirrored into the models release as
# asr_*, with Hugging Face as the fallback host. Override the base with
# $DOCLING_RS_ASR_MODELS_URL (e.g. an internal re-host) — an explicit override
# is used alone, without the release mirror. Skip entirely with --no-asr.
ASR_BASE_URL="${DOCLING_RS_ASR_MODELS_URL:-https://huggingface.co/onnx-community/whisper-tiny/resolve/main}"
ASR_MIRROR_URL="$BASE_URL"
if [ -n "${DOCLING_RS_ASR_MODELS_URL:-}" ]; then
  ASR_MIRROR_URL=
fi
# bge-m3 ONNX export for docling-rag's local embedder (--embed): community
# export with a pooled `dense_vecs` output, fetched straight from HF.
EMBED_BASE_URL="${DOCLING_RS_EMBED_MODELS_URL:-https://huggingface.co/aapot/bge-m3-onnx/resolve/main}"

FORCE=false
WITH_ASR=true
ASR_PRESETS=
WITH_INT8=true
WITH_CHUNK=true
WITH_ENRICH=false
WITH_EMBED=false
WITH_DPARSE="${DOCLING_RS_WITH_DOCLING_PARSE:-false}"
WITH_FONTS="${DOCLING_RS_WITH_FONTS:-false}"
WITH_NER="${DOCLING_RS_WITH_NER:-false}"
case "$(uname -m)" in
  s390x) WITH_ORT_DEFAULT=true ;;
  *) WITH_ORT_DEFAULT=false ;;
esac
WITH_ORT="${DOCLING_RS_WITH_ONNXRUNTIME:-$WITH_ORT_DEFAULT}"
INTO_CLI=false

for arg in "$@"; do
  case "$arg" in
    --force) FORCE=true ;;
    --no-asr) WITH_ASR=false ;;
    --asr-model=*) ASR_PRESETS="$ASR_PRESETS ${arg#--asr-model=}" ;;
    --int8) WITH_INT8=true ;; # accepted for compatibility; int8 is the default
    --no-int8) WITH_INT8=false ;;
    --no-chunk) WITH_CHUNK=false ;;
    --enrich) WITH_ENRICH=true ;;
    --embed) WITH_EMBED=true ;;
    --with-docling-parse) WITH_DPARSE=true ;;
    --with-fonts) WITH_FONTS=true ;;
    --with-ner) WITH_NER=true ;;
    --with-onnxruntime) WITH_ORT=true ;;
    --no-onnxruntime) WITH_ORT=false ;;
    --cli) INTO_CLI=true ;;

    *)
      echo "usage: download_dependencies.sh [--cli] [--force] [--no-asr] [--asr-model=<preset>] [--no-int8] [--no-chunk] [--enrich] [--embed] [--with-docling-parse] [--with-fonts] [--with-ner] [--with-onnxruntime|--no-onnxruntime]" >&2
      echo "  ASR presets: whisper_tiny_en whisper_base_en whisper_small_en whisper_distil_small_en parakeet_tdt_0.6b_v3" >&2
      exit 2
      ;;
  esac
done

if ! command -v curl >/dev/null 2>&1; then
  echo "error: curl is required" >&2
  exit 1
fi

if [ "$INTO_CLI" = true ]; then
  CLI_ROOT="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"
  mkdir -p "$CLI_ROOT"
  cd "$CLI_ROOT"
  echo "--cli: installing into $CLI_ROOT/.models (beside $CLI_ROOT/bin, where cargo install puts docling-rs)"
fi

mkdir -p .models/tableformer
if [ "$WITH_ASR" = true ]; then
  mkdir -p .models/asr
fi

# Never hang forever on a dead mirror: cap the connect phase, abort a transfer
# that stalls below 1 KiB/s for a minute, and retry transient failures with
# curl's built-in backoff (docling proper added the same guard, issue #3784).
# No --retry-delay: a fixed delay pins every attempt to the same short wait —
# the container build's four tries at 2s landed inside six seconds, which is
# nothing against the per-IP 429 Hugging Face answers CI runners with. Curl's
# default doubling (1s, 2s, 4s, …) spreads the attempts out instead; a
# server-sent Retry-After still overrides either way. Eight retries reach
# ~4 minutes (1+2+…+128 s): the v1.48.4 image publish died on `bbox.onnx`
# when GitHub's release-asset CDN answered 504 for every one of five tries
# inside 15 seconds — an outage that outlasts the old half-minute window
# but not a few minutes. Only transient answers (408/429/5xx, a timeout) are
# retried: a 404 still fails at once, which fetch_optional and the mirror
# fallbacks rely on for the sidecars a release does not host.
CURL_TIMEOUTS="--connect-timeout 30 --speed-limit 1024 --speed-time 60 --retry 8"

# --- integrity ----------------------------------------------------------------
# Every asset fetched from $BASE_URL is checked against the release's
# SHA256SUMS (the `sha256sum` format; scripts/ci/models_manifest.sh regenerates
# it after every upload into the release) before it is moved into place: a
# truncated transfer, a tampered mirror behind DOCLING_RS_MODELS_URL or a
# swapped asset fails here, not at run time — libpdfium.so and
# libonnxruntime.so are code the process loads. The upstream fallback hosts
# (Hugging Face, RapidOCR's hub, the font packages) are outside the
# manifest and stay unchecked. A file already on disk is not re-hashed
# (--force re-fetches and checks it).
# DOCLING_RS_MODELS_VERIFY: `auto` (default) checks what the manifest lists
# and warns about a release without one or an asset missing from it (a tag
# that predates the manifest, an asset uploaded by hand since); `require`
# fails on either; `off` skips the check.
VERIFY="${DOCLING_RS_MODELS_VERIFY:-auto}"
MANIFEST=
case "$VERIFY" in
  off) ;;
  auto | require)
    MANIFEST="$(mktemp)"
    # shellcheck disable=SC2086
    if ! curl -fsSL $CURL_TIMEOUTS -o "$MANIFEST" "$BASE_URL/SHA256SUMS" 2>/dev/null; then
      rm -f "$MANIFEST"
      MANIFEST=
      if [ "$VERIFY" = require ]; then
        echo "error: $BASE_URL/SHA256SUMS unavailable and DOCLING_RS_MODELS_VERIFY=require" >&2
        exit 1
      fi
      echo "  ! $BASE_URL hosts no SHA256SUMS — downloads from it are not integrity-checked" >&2
    fi
    ;;
  *)
    echo "error: DOCLING_RS_MODELS_VERIFY must be auto, require or off (got '$VERIFY')" >&2
    exit 2
    ;;
esac
cleanup() { if [ -n "$MANIFEST" ]; then rm -f "$MANIFEST"; fi; }
trap cleanup EXIT

sha256_of() { # <file> → the hex digest, or nothing when no tool can compute it
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 -r "$1" | cut -d' ' -f1
  fi
}

verify() { # <url> <downloaded-file> — a release asset against SHA256SUMS; a mismatch ends the run
  case "$1" in
    "$BASE_URL"/*) ;;
    *) return 0 ;; # an upstream fallback host: outside the manifest
  esac
  if [ -z "$MANIFEST" ]; then
    return 0
  fi
  name="${1#"$BASE_URL"/}"
  want="$(awk -v n="$name" '$2 == n { print $1; exit }' "$MANIFEST")"
  if [ -z "$want" ]; then
    if [ "$VERIFY" = require ]; then
      rm -f "$2"
      echo "error: $name is not listed in $BASE_URL/SHA256SUMS (DOCLING_RS_MODELS_VERIFY=require)" >&2
      exit 1
    fi
    echo "  ! $name is not listed in SHA256SUMS — not integrity-checked" >&2
    return 0
  fi
  got="$(sha256_of "$2")"
  if [ -z "$got" ]; then
    if [ "$VERIFY" = require ]; then
      rm -f "$2"
      echo "error: no sha256sum, shasum or openssl on PATH to check $name (DOCLING_RS_MODELS_VERIFY=require)" >&2
      exit 1
    fi
    echo "  ! no sha256sum, shasum or openssl on PATH — $name not integrity-checked" >&2
    return 0
  fi
  if [ "$got" != "$want" ]; then
    rm -f "$2"
    echo "error: $name: sha256 mismatch (expected $want, got $got) — a corrupted download or a tampered mirror; re-run, and only if it repeats set DOCLING_RS_MODELS_VERIFY=off to bypass the check at your own risk" >&2
    exit 1
  fi
}

fetch() { # <url> <dest>
  if [ "$FORCE" = false ] && [ -f "$2" ]; then
    echo "  = $2 (already present)"
    return 0
  fi
  echo "  > $2"
  # shellcheck disable=SC2086 # CURL_TIMEOUTS is a flag list, splitting intended
  curl -fsSL $CURL_TIMEOUTS -o "$2.download" "$1"
  verify "$1" "$2.download"
  mv "$2.download" "$2"
}

fetch_optional() { # <url> <dest> — ignore a missing/failed asset (sidecar files)
  # An empty URL is a mirror that doesn't apply to this run (see fetch_mirrored).
  if [ -z "$1" ] || { [ "$FORCE" = false ] && [ -f "$2" ]; }; then
    return 0
  fi
  # shellcheck disable=SC2086
  if curl -fsSL $CURL_TIMEOUTS -o "$2.download" "$1" 2>/dev/null; then
    verify "$1" "$2.download"
    mv "$2.download" "$2"
    echo "  > $2"
  else
    rm -f "$2.download"
  fi
}

fetch_mirrored() { # <dest> <url>... — first URL that lands wins
  dest=$1
  shift
  if [ "$FORCE" = false ] && [ -f "$dest" ]; then
    echo "  = $dest (already present)"
    return 0
  fi
  for url in "$@"; do
    # An empty entry is a mirror that doesn't apply to this run (e.g. the
    # release mirror when $DOCLING_RS_ASR_MODELS_URL overrides the host).
    [ -n "$url" ] || continue
    # shellcheck disable=SC2086
    if curl -fsSL $CURL_TIMEOUTS -o "$dest.download" "$url"; then
      verify "$url" "$dest.download"
      mv "$dest.download" "$dest"
      echo "  > $dest"
      return 0
    fi
    rm -f "$dest.download"
    echo "  ! $url unavailable — trying the next mirror" >&2
  done
  echo "error: could not fetch $dest from any mirror" >&2
  return 1
}

echo "fetching docling.rs ML dependencies from $BASE_URL"
# docling-parse's page renderer as a runtime plugin (#478) — opt-in
# (--with-docling-parse / DOCLING_RS_WITH_DOCLING_PARSE=1): the raster docling
# 2.123+ feeds its models, the reference the PDF baselines are pinned to and
# what scripts/conformance/*.sh run with DOCLING_RS_RENDERER=docling-parse.
# The pipeline renders in pure Rust and never loads it by default. Built by
# .github/workflows/docling-parse-render.yml into the models release as
# docling-parse-render-<os>-<arch>.tar.gz (lib/ + pdf_resources/); a tag that
# does not host it, or a platform without a build, can build it locally with
# scripts/install/build_docling_parse_render.sh.
if [ "$WITH_DPARSE" = true ]; then
  case "$(uname -s)" in
    Darwin) HOST_OS=mac ;;
    *) HOST_OS=linux ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) HOST_ARCH=x64 ;;
    aarch64 | arm64) HOST_ARCH=arm64 ;;
    *) HOST_ARCH= ;;
  esac
  if [ -z "$HOST_ARCH" ]; then
    echo "  (skipping the docling-parse plugin: unsupported arch $(uname -m) — scripts/install/build_docling_parse_render.sh builds it)"
  else
    DPR_ASSET="docling-parse-render-$HOST_OS-$HOST_ARCH.tar.gz"
    if [ "$FORCE" = false ] && ls .docling-parse/lib/libdparse_render.* >/dev/null 2>&1; then
      echo "  = .docling-parse/lib/libdparse_render (already present)"
    else
      mkdir -p .docling-parse
      # shellcheck disable=SC2086
      if curl -fsSL $CURL_TIMEOUTS -o .docling-parse/plugin.tgz "$BASE_URL/$DPR_ASSET" 2>/dev/null; then
        verify "$BASE_URL/$DPR_ASSET" .docling-parse/plugin.tgz
        tar xzf .docling-parse/plugin.tgz -C .docling-parse
        rm -f .docling-parse/plugin.tgz
        echo "  > .docling-parse/lib + pdf_resources ($DPR_ASSET)"
      else
        rm -f .docling-parse/plugin.tgz
        echo "  ($DPR_ASSET not hosted for this tag/platform — scripts/install/build_docling_parse_render.sh builds the plugin locally)"
      fi
    fi
  fi
fi
# ONNX Runtime for IBM Z (#504; --with-onnxruntime / DOCLING_RS_WITH_ONNXRUNTIME=1,
# on by default on an s390x host): pyke's `ort` links a prebuilt runtime on
# x86_64/aarch64 only, so the s390x build of docling.rs (`ort-load-dynamic`)
# dlopens libonnxruntime.so — `ORT_DYLIB_PATH`, then this directory through
# the models-dir resolution, then the library search path. Built from source
# by .github/workflows/onnxruntime-s390x.yml (scripts/install/build_onnxruntime_s390x.sh)
# into the models release as onnxruntime-linux-s390x.tar.gz, which unpacks
# into .models/onnxruntime/ as is. Only the s390x build exists: on another
# arch the flag is a no-op with a note (the runtime is linked in there).
if [ "$WITH_ORT" = true ]; then
  case "$(uname -m)" in
    s390x)
      ORT_ASSET="onnxruntime-linux-s390x.tar.gz"
      if [ "$FORCE" = false ] && [ -f .models/onnxruntime/libonnxruntime.so ]; then
        echo "  = .models/onnxruntime/libonnxruntime.so (already present)"
      else
        mkdir -p .models/onnxruntime
        # shellcheck disable=SC2086
        if curl -fsSL $CURL_TIMEOUTS -o .models/onnxruntime/ort.tgz "$BASE_URL/$ORT_ASSET" 2>/dev/null; then
          verify "$BASE_URL/$ORT_ASSET" .models/onnxruntime/ort.tgz
          tar xzf .models/onnxruntime/ort.tgz -C .models/onnxruntime
          rm -f .models/onnxruntime/ort.tgz
          echo "  > .models/onnxruntime/libonnxruntime.so ($ORT_ASSET, ONNX Runtime $(cat .models/onnxruntime/VERSION 2>/dev/null))"
        else
          rm -f .models/onnxruntime/ort.tgz
          echo "  ($ORT_ASSET not hosted for this tag — scripts/install/build_onnxruntime_s390x.sh builds it; without it the ML stages are unavailable on this host)"
        fi
      fi
      ;;
    *)
      echo "  (--with-onnxruntime: only the s390x build exists; on $(uname -m) the runtime is linked into the binary)"
      ;;
  esac
fi
# Fallback fonts for the Rust renderer (--with-fonts / DOCLING_RS_WITH_FONTS=1):
# Liberation (metric-compatible with Arial / Times New Roman / Courier New,
# SIL OFL 1.1) from Debian's binary package — upstream publishes 2.x only as
# FontForge sources — and DejaVu (Bitstream Vera licence) from its GitHub
# release. Fetched straight from those hosts, not re-hosted: they are stable
# and the files are not ours to redistribute under the models release's
# notice. The licence texts land next to the faces.
if [ "$WITH_FONTS" = true ]; then
  FONTS_DIR=.models/fonts
  LIBERATION_DEB="https://deb.debian.org/debian/pool/main/f/fonts-liberation/fonts-liberation_2.1.5-3_all.deb"
  DEJAVU_TBZ="https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/dejavu-fonts-ttf-2.37.tar.bz2"
  if [ "$FORCE" = false ] && [ -f "$FONTS_DIR/liberation/LiberationSans-Regular.ttf" ]; then
    echo "  = $FONTS_DIR/liberation (already present)"
  elif ! command -v ar >/dev/null 2>&1; then
    echo "  (skipping Liberation: unpacking the Debian package needs \`ar\` (binutils); install fonts-liberation from your distribution instead)"
  else
    FONTS_TMP="$(mktemp -d)"
    # shellcheck disable=SC2086
    if curl -fsSL $CURL_TIMEOUTS -o "$FONTS_TMP/liberation.deb" "$LIBERATION_DEB"; then
      (cd "$FONTS_TMP" && ar x liberation.deb data.tar.xz && tar xJf data.tar.xz)
      rm -rf "$FONTS_DIR/liberation"
      mkdir -p "$FONTS_DIR/liberation"
      cp "$FONTS_TMP"/usr/share/fonts/truetype/liberation/*.ttf "$FONTS_DIR/liberation/"
      cp "$FONTS_TMP"/usr/share/doc/fonts-liberation/copyright "$FONTS_DIR/liberation/LICENSE"
      echo "  > $FONTS_DIR/liberation (12 faces)"
    else
      echo "  ! Liberation unavailable from $LIBERATION_DEB" >&2
    fi
    rm -rf "$FONTS_TMP"
  fi
  if [ "$FORCE" = false ] && [ -f "$FONTS_DIR/dejavu/DejaVuSans.ttf" ]; then
    echo "  = $FONTS_DIR/dejavu (already present)"
  else
    FONTS_TMP="$(mktemp -d)"
    # shellcheck disable=SC2086
    if curl -fsSL $CURL_TIMEOUTS -o "$FONTS_TMP/dejavu.tar.bz2" "$DEJAVU_TBZ"; then
      tar xjf "$FONTS_TMP/dejavu.tar.bz2" -C "$FONTS_TMP"
      rm -rf "$FONTS_DIR/dejavu"
      mkdir -p "$FONTS_DIR/dejavu"
      cp "$FONTS_TMP"/dejavu-fonts-ttf-*/ttf/*.ttf "$FONTS_DIR/dejavu/"
      cp "$FONTS_TMP"/dejavu-fonts-ttf-*/LICENSE "$FONTS_DIR/dejavu/LICENSE"
      echo "  > $FONTS_DIR/dejavu ($(ls "$FONTS_DIR/dejavu"/*.ttf | wc -l | tr -d ' ') faces)"
    else
      echo "  ! DejaVu unavailable from $DEJAVU_TBZ" >&2
    fi
    rm -rf "$FONTS_TMP"
  fi
fi
fetch "$BASE_URL/layout_heron.onnx" .models/layout_heron.onnx
fetch "$BASE_URL/ocr_rec.onnx" .models/ocr_rec.onnx
fetch "$BASE_URL/ppocr_keys_v1.txt" .models/ppocr_keys_v1.txt
# English PP-OCRv3 recognition pair — the runtime default (the ch_ pair above
# stays the conformance model, selected with DOCLING_RS_OCR_LANG=ch). The
# release mirrors both (publish-models.yml re-hosts them unmodified); the
# upstream hosts stay as the fallback for release tags that predate the
# mirror.
fetch_mirrored .models/ocr_rec_en.onnx \
  "$BASE_URL/ocr_rec_en.onnx" \
  "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/en_PP-OCRv3_rec_infer.onnx"
fetch_mirrored .models/en_dict.txt \
  "$BASE_URL/en_dict.txt" \
  "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/en_dict.txt"
# PP-OCRv6 text detector (#429) — the model docling's RapidOCR default runs
# in front of its recognizer, re-hosted unmodified; RapidOCR's own hub is the
# fallback for release tags that predate the mirror. Optional: without it OCR
# reads the layout regions' projection strips only (text outside layout
# regions on bitmap pages is lost, and forms read far worse — #570).
fetch_mirrored .models/ocr_det.onnx \
  "$BASE_URL/ocr_det.onnx" \
  "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/det/PP-OCRv6_det_small.onnx"
# PP-OCRv6 recognizer + dictionary (#570) — RapidOCR's `PP-OCRv6_rec_small`,
# the multilingual model docling recognizes with for every language; preferred
# over the PP-OCRv3 pairs when present (SHA256 pinned in RapidOCR's hub:
# 6f327246b50388f3c176ae304bd95767ea6dc0c9ae92153ef8cbe210b3c14884). Optional:
# without it the v3 pairs below run.
fetch_mirrored .models/ocr_rec_v6.onnx \
  "$BASE_URL/ocr_rec_v6.onnx" \
  "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/rec/PP-OCRv6_rec_small.onnx"
fetch_mirrored .models/ocr_rec_v6_dict.txt \
  "$BASE_URL/ocr_rec_v6_dict.txt" \
  "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt"
# PP-LCNet text-line orientation classifier (#666, ~1 MB) — RapidOCR's
# `ch_PP-LCNet_x0_25_textline_ori_cls_mobile` (SHA256 pinned in RapidOCR's
# hub: 54379ae5174d026780215fc748a7f31910dee36818e63d49e17dc598ecc82df7).
# Optional: only `ocr_textline_orientation` reads it; without it that mode
# still crops and turns lines but never flips an upside-down one.
fetch_mirrored .models/ocr_textline_ori.onnx \
  "$BASE_URL/ocr_textline_ori.onnx" \
  "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv5/cls/ch_PP-LCNet_x0_25_textline_ori_cls_mobile.onnx"
fetch "$BASE_URL/encoder.onnx" .models/tableformer/encoder.onnx
fetch_optional "$BASE_URL/encoder.onnx.data" .models/tableformer/encoder.onnx.data
fetch "$BASE_URL/decoder.onnx" .models/tableformer/decoder.onnx
fetch_optional "$BASE_URL/decoder.onnx.data" .models/tableformer/decoder.onnx.data
# True-KV-cache decoder variant — preferred by the Rust loop when present
# (~13-17% faster table-structure decode, byte-identical output). Optional:
# older release tags don't host it, and the legacy decoder above still works.
fetch_optional "$BASE_URL/decoder_kv.onnx" .models/tableformer/decoder_kv.onnx
fetch_optional "$BASE_URL/decoder_kv.onnx.data" .models/tableformer/decoder_kv.onnx.data
# The same decoder with per-layer caches (#871, scripts/install/kv_layers.py):
# a ~0.25 MB graph over decoder_kv.onnx.data, bit-identical output, whose
# caches the Rust loop keeps in its own buffers — preferred when present.
fetch_optional "$BASE_URL/decoder_kv_layers.onnx" .models/tableformer/decoder_kv_layers.onnx
fetch "$BASE_URL/bbox.onnx" .models/tableformer/bbox.onnx
fetch_optional "$BASE_URL/bbox.onnx.data" .models/tableformer/bbox.onnx.data

if [ "$WITH_ASR" = true ]; then
  # Whisper tiny for audio/ASR: encoder + (cache-less) decoder + vocabulary;
  # added_tokens.json feeds non-English language selection and the special-
  # token layout, so a missing asset there is not fatal for the default model.
  fetch_mirrored .models/asr/encoder_model.onnx \
    "${ASR_MIRROR_URL:+$ASR_MIRROR_URL/asr_encoder_model.onnx}" \
    "$ASR_BASE_URL/onnx/encoder_model.onnx"
  fetch_mirrored .models/asr/decoder_model.onnx \
    "${ASR_MIRROR_URL:+$ASR_MIRROR_URL/asr_decoder_model.onnx}" \
    "$ASR_BASE_URL/onnx/decoder_model.onnx"
  fetch_mirrored .models/asr/vocab.json \
    "${ASR_MIRROR_URL:+$ASR_MIRROR_URL/asr_vocab.json}" \
    "$ASR_BASE_URL/vocab.json"
  fetch_optional "${ASR_MIRROR_URL:+$ASR_MIRROR_URL/asr_added_tokens.json}" .models/asr/added_tokens.json
  fetch_optional "$ASR_BASE_URL/added_tokens.json" .models/asr/added_tokens.json
fi

# Named ASR model presets (docling's English-only / Distil-Whisper specs,
# limited to variants with public ONNX exports, and NVIDIA's Parakeet TDT
# 0.6B v3, #508): each lands in its own .models/asr/<preset>/ directory,
# selected at run time with DocumentConverter::asr_model / the serve
# `asr_model` option.
for preset in $ASR_PRESETS; do
  case "$preset" in
    whisper_tiny_en) repo="whisper-tiny.en" ;;
    whisper_base_en) repo="whisper-base.en" ;;
    whisper_small_en) repo="whisper-small.en" ;;
    whisper_distil_small_en) repo="distil-small.en" ;;
    parakeet_tdt_0.6b_v3)
      # The onnx-asr export of NVIDIA's NeMo checkpoint (CC-BY-4.0): the int8
      # encoder (~650 MB) + decoder-joint by default, the fp32 graphs
      # (~2.5 GB, encoder weights in a .data sidecar) with --no-int8 — the
      # pipeline loads whichever is present, int8 first unless DOCLING_RS_FP32.
      # The Silero VAD (v5, MIT, ~2 MB; the file onnx-asr loads) segments long
      # recordings — optional at run time, fetched alongside.
      base="${DOCLING_RS_PARAKEET_MODELS_URL:-https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main}"
      mkdir -p ".models/asr/$preset" .models/asr/vad
      if [ "$WITH_INT8" = true ]; then
        fetch "$base/encoder-model.int8.onnx" ".models/asr/$preset/encoder-model.int8.onnx"
        fetch "$base/decoder_joint-model.int8.onnx" ".models/asr/$preset/decoder_joint-model.int8.onnx"
      else
        fetch "$base/encoder-model.onnx" ".models/asr/$preset/encoder-model.onnx"
        fetch "$base/encoder-model.onnx.data" ".models/asr/$preset/encoder-model.onnx.data"
        fetch "$base/decoder_joint-model.onnx" ".models/asr/$preset/decoder_joint-model.onnx"
      fi
      fetch "$base/vocab.txt" ".models/asr/$preset/vocab.txt"
      fetch_optional "$base/config.json" ".models/asr/$preset/config.json"
      fetch "${DOCLING_RS_VAD_MODEL_URL:-https://huggingface.co/istupakov/silero-vad-onnx/resolve/main/silero_vad.onnx}" \
        .models/asr/vad/silero_vad.onnx
      continue
      ;;
    *) echo "unknown --asr-model '$preset' (available: whisper_tiny_en whisper_base_en whisper_small_en whisper_distil_small_en parakeet_tdt_0.6b_v3)" >&2; exit 2 ;;
  esac
  base="https://huggingface.co/onnx-community/$repo/resolve/main"
  mkdir -p ".models/asr/$preset"
  fetch "$base/onnx/encoder_model.onnx" ".models/asr/$preset/encoder_model.onnx"
  fetch "$base/onnx/decoder_model.onnx" ".models/asr/$preset/decoder_model.onnx"
  fetch "$base/vocab.json" ".models/asr/$preset/vocab.json"
  # English-only exports keep their special tokens here; required for the
  # shifted token layout to resolve.
  fetch "$base/added_tokens.json" ".models/asr/$preset/added_tokens.json"
done

if [ "$WITH_CHUNK" = true ]; then
  # The hybrid chunker's default tokenizer (all-MiniLM-L6-v2's tokenizer.json,
  # ~0.5 MB). The CLI (`--to chunks`), the Node/Python bindings and docling-rag
  # all pick it up at .models/chunk/tokenizer.json when no explicit path is
  # given. Fetched from the release when hosted (newer tags), else straight
  # from Hugging Face.
  mkdir -p .models/chunk
  fetch_mirrored .models/chunk/tokenizer.json \
    "$BASE_URL/chunk_tokenizer.json" \
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json"
fi

# DocumentFigureClassifier (picture classification enrichment, ~17 MB): the
# `--enrich-picture-classes` / `do_picture_classification` model. Small, so
# fetched by default — from the release when hosted, else the upstream ONNX
# straight from Hugging Face (docling-project/DocumentFigureClassifier-v2.5
# ships the graph itself).
fetch_mirrored .models/picture_classifier.onnx \
  "$BASE_URL/picture_classifier.onnx" \
  "https://huggingface.co/docling-project/DocumentFigureClassifier-v2.5/resolve/main/model.onnx"

if [ "$WITH_NER" = true ]; then
  # dslim/bert-base-NER (MIT) — the token classifier the PII redaction pass
  # (#621, `--redact-pii`) reads names, organizations and locations with;
  # the pass runs pattern-only without it. Hugging Face ships the ONNX
  # export itself (~430 MB fp32), so no re-export is needed; the release
  # mirror is tried first for when it hosts a copy. Opt-in because of the
  # size, like --enrich.
  NER_BASE_URL="${DOCLING_RS_NER_MODELS_URL:-https://huggingface.co/dslim/bert-base-NER/resolve/main/onnx}"
  mkdir -p .models/ner
  fetch_mirrored .models/ner/model.onnx "$BASE_URL/ner_model.onnx" "$NER_BASE_URL/model.onnx"
  fetch_mirrored .models/ner/tokenizer.json "$BASE_URL/ner_tokenizer.json" "$NER_BASE_URL/tokenizer.json"
  fetch_mirrored .models/ner/config.json "$BASE_URL/ner_config.json" "$NER_BASE_URL/config.json"
fi

if [ "$WITH_ENRICH" = true ]; then
  # CodeFormulaV2 (code/formula enrichment, ~1.3 GB fp32): the
  # `--enrich-code`/`--enrich-formula` VLM, exported to ONNX by
  # scripts/install/export_code_formula.py and hosted with the release
  # (there is no upstream ONNX export to fall back to). Opt-in (--enrich)
  # because of its size.
  mkdir -p .models/code_formula
  fetch "$BASE_URL/cf_vision.onnx" .models/code_formula/vision.onnx
  fetch "$BASE_URL/cf_embed.onnx" .models/code_formula/embed.onnx
  fetch "$BASE_URL/cf_tokenizer.json" .models/code_formula/tokenizer.json
  if [ "$WITH_INT8" = true ]; then
    # INT8 decoder (~165 MB vs ~655 MB fp32) — preferred automatically when
    # present. Near-exact, not byte-exact: greedy near-tie tokens can flip
    # (whitespace-only drift on the conformance fixture); fetch with --no-int8
    # or set DOCLING_RS_FP32=1 at runtime for the byte-exact fp32 graph.
    fetch_optional "$BASE_URL/cf_decoder_kv_int8.onnx" .models/code_formula/decoder_kv_int8.onnx
  fi
  if [ "$WITH_INT8" = true ] && [ -f .models/code_formula/decoder_kv_int8.onnx ]; then
    echo "code_formula: int8 decoder present — fp32 decoder_kv.onnx not needed (skipped)"
  else
    fetch "$BASE_URL/cf_decoder_kv.onnx" .models/code_formula/decoder_kv.onnx
  fi
fi

if [ "$WITH_EMBED" = true ]; then
  # bge-m3 for docling-rag's local ONNX embedder (RAG_EMBED_PROVIDER=onnx,
  # build with --features onnx-embed): the graph + its external-weights file
  # (~2.3 GB) + the XLM-R tokenizer. The graph internally references
  # `model.onnx.data` by that exact name — do not rename it. Paths match the
  # RAG_EMBED_ONNX_PATH / RAG_EMBED_TOKENIZER defaults.
  mkdir -p .models/embed
  fetch "$EMBED_BASE_URL/model.onnx" .models/embed/bge-m3.onnx
  fetch "$EMBED_BASE_URL/model.onnx.data" .models/embed/model.onnx.data
  fetch "$EMBED_BASE_URL/tokenizer.json" .models/embed/tokenizer.json
fi

if [ "$WITH_INT8" = true ]; then
  # INT8-quantized CPU models (optional release assets). The pipeline prefers
  # them automatically when they sit at the default paths; DOCLING_RS_FP32=1
  # forces the fp32 models at runtime.
  fetch_optional "$BASE_URL/layout_heron_int8.onnx" .models/layout_heron_int8.onnx
  fetch_optional "$BASE_URL/decoder_int8.onnx" .models/tableformer/decoder_int8.onnx
  fetch_optional "$BASE_URL/decoder_kv_int8.onnx" .models/tableformer/decoder_kv_int8.onnx
  fetch_optional "$BASE_URL/decoder_kv_int8.onnx.data" .models/tableformer/decoder_kv_int8.onnx.data
  # fp16-weight repack of the TableFormer encoder (#374): fp32 compute, half
  # the download; preferred when present, DOCLING_RS_FP32=1 opts out.
  fetch_optional "$BASE_URL/encoder_fp16.onnx" .models/tableformer/encoder_fp16.onnx
  if [ -f .models/layout_heron_int8.onnx ]; then
    echo "int8 models present — used by default (DOCLING_RS_FP32=1 forces full precision)"
  else
    echo "layout int8 not hosted at $BASE_URL — the fp32 layout model will be used"
    echo "(correct output, ~2.4x slower layout stage on CPU; irrelevant for GPU builds,"
    echo "which prefer fp32 anyway). Publish runs left it unhosted while the quantizer's"
    echo "gate kept rejecting the result — that was int16 saturation of full-range"
    echo "weights on runners without VNNI, now fixed with 7-bit weights. Until the next"
    echo "models publish, build one from the fp32 model this script just fetched:"
    echo "  python scripts/install/quantize_models.py layout"
    echo "(it self-validates and keeps nothing that fails the gates)."
    echo "See docs/PDF_CONFORMANCE.md."
  fi
fi

echo "done — .models/ populated in $(pwd)"
if [ "$INTO_CLI" = true ]; then
  echo "a cargo-installed docling-rs ($(pwd)/bin/docling-rs) now finds them from any directory"
fi
