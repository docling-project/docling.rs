# syntax=docker/dockerfile:1
#
# Example deployment image for docling.rs (the Rust docling port) with the full
# PDF ML pipeline — pdfium rasterization + RT-DETR layout + TableFormer table
# structure (hoisted-KV cached decoder, #97) + PP-OCRv3 — plus the picture
# classifier and the hybrid-chunker tokenizer. Builds a slim runtime with the
# `docling-rs` CLI, the native libs, and all models baked in, so a container can
# convert PDFs/images offline with no Python at runtime.
#
# Build from the repository root:
#   docker build -f examples/Dockerfile -t docling-rs .
# Convert a document (Markdown to stdout):
#   docker run --rm -v "$PWD:/data" docling-rs /data/input.pdf
#
# Optimizations baked in by default (see PDF_CONFORMANCE.md for the measured
# numbers; every one is byte-exact on the snapshot corpus):
#   * layout INT8 — static Conv-only quantization calibrated on the repo's PDF
#     corpus, ~2.4x faster layout inference (`--build-arg INT8=0` for fp32;
#     both precisions ship in the image, switchable per container via
#     `-e DOCLING_LAYOUT_ONNX=/opt/models/layout_heron.onnx`)
#   * TableFormer hoisted-KV decoder (fp32) — one-token-per-step decode with
#     per-layer cross-attention tensors computed once per table; preferred
#     over the quantized legacy decoder on every machine measured, so the
#     legacy TF INT8 file is deliberately not built (see quantize_models.py)
#   * codegen-units=1 on top of the workspace's lto="thin"
#   * optional `--build-arg TARGET_CPU=x86-64-v3` (or `native` for same-host
#     deploys): lets the compiler use AVX2+ in the image-processing hot paths;
#     the default stays portable to any x86-64
#
# Stages:
#   models  — export the layout + TableFormer ONNX (torch, one-time), fetch
#             the OCR model/dict, picture classifier, chunker tokenizer and
#             pdfium, INT8-quantize the layout model (unless INT8=0)
#   builder — compile the CLI (the `ort` crate downloads ONNX Runtime at build)
#   runtime — slim image with just the binary, native libs and models

# ----------------------------------------------------------------------------
# Stage 1: ML models + native libs (Python only here, never at runtime)
# ----------------------------------------------------------------------------
FROM python:3.12-slim AS models

RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates tar \
    && rm -rf /var/lib/apt/lists/*

# torch (CPU) + the docling model packages used by the export scripts, plus
# the quantization/calibration deps (pypdfium2, pillow, sympy).
RUN pip install --no-cache-dir torch --index-url https://download.pytorch.org/whl/cpu \
    && pip install --no-cache-dir --extra-index-url https://download.pytorch.org/whl/cpu \
        "transformers>=4.45,<5" "onnx>=1.16,<2" \
        docling-ibm-models onnxscript onnxruntime huggingface_hub \
        sympy pypdfium2 pillow numpy

WORKDIR /build
COPY scripts/install ./scripts/install
# Real corpus pages for INT8 calibration (build-stage only, not in the runtime).
COPY tests/data/pdf/sources ./tests/data/pdf/sources
COPY tests/data/scanned/sources ./tests/data/scanned/sources

RUN mkdir -p /opt/models/chunk /opt/pdfium \
    # RT-DETR layout detector -> models/layout_heron.onnx
    && python scripts/install/export_layout.py /opt/models/layout_heron.onnx \
    # TableFormer encoder/decoder/bbox. Exports both decoder generations: the
    # #97 hoisted-KV step graph (decoder_kv.onnx — per-layer cross-attention
    # K/V hoisted into the encoder, byte-exact vs docling and the fastest
    # variant on every machine measured) and the legacy layer-output-cache
    # decoder.onnx as a fallback. The artifacts dir auto-resolves (downloads
    # docling-project/docling-models if absent).
    && python scripts/install/export_tableformer.py /opt/models/tableformer \
    # PP-OCRv3 recognition model + dictionary (for scanned pages)
    && curl -fsSL -o /opt/models/ocr_rec.onnx \
        "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx" \
    && curl -fsSL -o /opt/models/ppocr_keys_v1.txt \
        "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt" \
    # DocumentFigureClassifier-v2.5 (--enrich-picture-classes)
    && curl -fsSL -o /opt/models/picture_classifier.onnx \
        "https://huggingface.co/docling-project/DocumentFigureClassifier-v2.5/resolve/main/model.onnx" \
    # all-MiniLM-L6-v2 tokenizer — the hybrid chunker works out of the box
    && curl -fsSL -o /opt/models/chunk/tokenizer.json \
        "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json" \
    # pdfium prebuilt (page rasterization only)
    && curl -fsSL -o /tmp/pdfium.tgz \
        "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-x64.tgz" \
    && tar xzf /tmp/pdfium.tgz -C /opt/pdfium \
    && rm /tmp/pdfium.tgz

# INT8=1 (default): quantize the layout model (static, Conv-only, calibrated on
# the corpus copied above) and point the `layout_heron_active.onnx` name — which
# the runtime ENV references — at the int8 file. INT8=0 keeps it on fp32. Both
# precisions ship in the image either way.
#
# Only `layout` is quantized: the TableFormer decoder the runtime uses is the
# hoisted-KV fp32 graph, which outperforms the quantized legacy decoder AND is
# byte-exact — a freshly quantized TF decoder can also flip near-tie tokens on
# heavy tables (see quantize_models.py), so it has no place in this image.
ARG INT8=1
RUN if [ "$INT8" = "1" ]; then \
        DOCLING_RS_MODELS_DIR=/opt/models python scripts/install/quantize_models.py layout \
        && ln -s layout_heron_int8.onnx /opt/models/layout_heron_active.onnx; \
    else \
        ln -s layout_heron.onnx /opt/models/layout_heron_active.onnx; \
    fi

# ----------------------------------------------------------------------------
# Stage 2: Rust build
# ----------------------------------------------------------------------------
FROM rust:1-slim-trixie AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential cmake perl pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# TARGET_CPU: empty (default) = portable x86-64 image; "x86-64-v3" = require
# AVX2 (2013+ CPUs, most cloud fleets); "native" = tune for the build host
# (only for images that run where they're built). Affects the Rust-side image
# processing hot paths (resize, normalization); ONNX Runtime ships its own
# runtime CPU dispatch either way.
ARG TARGET_CPU=""
ENV CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
COPY Cargo.toml Cargo.loc[k] ./
COPY crates ./crates
# --features serve compiles in the `docling-rs serve` subcommand (the HTTP
# conversion API), so one image covers both CLI and server use.
# Cargo.lock is gitignored in this repo, so a fresh clone has none — the
# `Cargo.loc[k]` glob makes the COPY tolerate its absence, and --locked is
# applied only when a lock file actually came along.
RUN if [ -n "$TARGET_CPU" ]; then export RUSTFLAGS="-C target-cpu=$TARGET_CPU"; fi \
    && cargo build --release -p docling-cli --features serve \
        $([ -f Cargo.lock ] && echo --locked)

# Collect the binary + the ONNX Runtime shared lib `ort` downloaded during build.
RUN mkdir -p /artifacts \
    && cp target/release/docling-rs /artifacts/ \
    && find target -name 'libonnxruntime*.so*' -exec cp -av {} /artifacts/ \; || true

# ----------------------------------------------------------------------------
# Stage 3: runtime (no Python, no toolchain)
# ----------------------------------------------------------------------------
FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 libgomp1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /artifacts/ /tmp/artifacts/
RUN cp /tmp/artifacts/docling-rs /usr/local/bin/ \
    && (cp /tmp/artifacts/libonnxruntime*.so* /usr/local/lib/ 2>/dev/null || true) \
    && rm -rf /tmp/artifacts \
    && ldconfig

COPY --from=models /opt/models /opt/models
COPY --from=models /opt/pdfium /opt/pdfium
# The picture classifier has no env override — it resolves models/… relative
# to the CWD, the binary's dir, or the binary's parent. The binary lives in
# /usr/local/bin, so this symlink makes /opt/models visible from any CWD.
RUN ln -s /opt/models /usr/local/models

# The pipeline finds its native lib + models via these env vars (set explicitly
# so the binary works from any working directory).
#   * layout_heron_active.onnx is a symlink resolved by the models stage's INT8
#     arg (int8 by default, fp32 with --build-arg INT8=0); both concrete files
#     are present, so a container can pin either precision explicitly, e.g.
#     `-e DOCLING_LAYOUT_ONNX=/opt/models/layout_heron.onnx` for fp32.
#   * The TableFormer decoder is pinned to the hoisted-KV fp32 graph (#97);
#     `-e DOCLING_TABLEFORMER_DECODER=/opt/models/tableformer/decoder.onnx`
#     switches to the legacy graph (both are auto-detected by input names).
ENV PDFIUM_DYNAMIC_LIB_PATH=/opt/pdfium/lib \
    DOCLING_LAYOUT_ONNX=/opt/models/layout_heron_active.onnx \
    DOCLING_OCR_REC_ONNX=/opt/models/ocr_rec.onnx \
    DOCLING_OCR_DICT=/opt/models/ppocr_keys_v1.txt \
    DOCLING_TABLEFORMER_ENCODER=/opt/models/tableformer/encoder.onnx \
    DOCLING_TABLEFORMER_DECODER=/opt/models/tableformer/decoder_kv.onnx \
    DOCLING_TABLEFORMER_BBOX=/opt/models/tableformer/bbox.onnx \
    DOCLING_CHUNK_TOKENIZER=/opt/models/chunk/tokenizer.json \
    LD_LIBRARY_PATH=/usr/local/lib:/opt/pdfium/lib

# Default: read a file arg and print Markdown to stdout, e.g.
#   docker run --rm -v "$PWD:/data" docling-rs /data/input.pdf --to json
# The HTTP conversion API is compiled in as well:
#   docker run --rm -p 5001:5001 docling-rs serve --addr 0.0.0.0:5001
EXPOSE 5001
ENTRYPOINT ["docling-rs"]
