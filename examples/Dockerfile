# syntax=docker/dockerfile:1
#
# Example deployment image for docling.rs (the Rust docling port) with the full
# PDF ML pipeline — pdfium rasterization + RT-DETR layout + TableFormer table
# structure (KV-cached decoder) + PP-OCRv3. Builds a slim runtime with the
# `docling.rs` CLI, the native libs, and all models baked in, so a container can
# convert PDFs/images offline with no Python at runtime.
#
# Build from the repository root:
#   docker build -f examples/Dockerfile -t docling.rs .
# Convert a document (Markdown to stdout):
#   docker run --rm -v "$PWD:/data" docling.rs /data/input.pdf
#
# By default the models stage also INT8-quantizes the layout model (Conv-only,
# calibrated on the repo's PDF corpus) and the TableFormer decoder, and the
# runtime points at the int8 files — ~2.4x faster layout inference on CPU at
# validated-unchanged conformance (see PDF_PERFORMANCE.md). Build with
# `--build-arg INT8=0` for pure fp32, or override per container:
#   docker run -e DOCLING_LAYOUT_ONNX=/opt/models/layout_heron.onnx ...
# (both precisions are baked into the image).
#
# Stages:
#   models  — export the layout + TableFormer ONNX (torch, one-time), fetch
#             the OCR model/dict + pdfium, INT8-quantize (unless INT8=0)
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
COPY scripts/export_layout.py scripts/export_tableformer.py scripts/quantize_models.py ./scripts/
# Real corpus pages for INT8 calibration (build-stage only, not in the runtime).
COPY tests/data/pdf/sources ./tests/data/pdf/sources
COPY tests/data/scanned/sources ./tests/data/scanned/sources

RUN mkdir -p /opt/models /opt/pdfium \
    # RT-DETR layout detector -> models/layout_heron.onnx
    && python scripts/export_layout.py /opt/models/layout_heron.onnx \
    # TableFormer encoder/decoder/bbox. The decoder is the KV-cached step
    # (self-attention state cache + precomputed cross-attention K/V) — byte-exact
    # vs docling and ~2-4x faster on table-dense pages. The artifacts dir
    # auto-resolves (downloads docling-project/docling-models if absent).
    && python scripts/export_tableformer.py /opt/models/tableformer \
    # PP-OCRv3 recognition model + dictionary (for scanned pages)
    && curl -sSL -o /opt/models/ocr_rec.onnx \
        "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/ch_PP-OCRv3_rec_infer.onnx" \
    && curl -sSL -o /opt/models/ppocr_keys_v1.txt \
        "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/ppocr_keys_v1.txt" \
    # pdfium prebuilt (page rasterization only)
    && curl -sSL -o /tmp/pdfium.tgz \
        "https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-linux-x64.tgz" \
    && tar xzf /tmp/pdfium.tgz -C /opt/pdfium \
    && rm /tmp/pdfium.tgz

# INT8=1 (default): quantize layout (static, conv-only, calibrated on the
# corpus copied above) + TableFormer decoder (dynamic), and point the
# `*_active` names — which the runtime ENV references — at the int8 files.
# INT8=0 keeps the active names on fp32. Both precisions ship in the image
# either way, so a container can switch back via the DOCLING_* env vars.
ARG INT8=1
RUN if [ "$INT8" = "1" ]; then \
        DOCLING_RS_MODELS_DIR=/opt/models python scripts/quantize_models.py \
        && ln -s layout_heron_int8.onnx /opt/models/layout_heron_active.onnx \
        && ln -s decoder_int8.onnx /opt/models/tableformer/decoder_active.onnx; \
    else \
        ln -s layout_heron.onnx /opt/models/layout_heron_active.onnx \
        && ln -s decoder.onnx /opt/models/tableformer/decoder_active.onnx; \
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

# Warm the dependency cache on the manifests first (fast rebuilds on src edits).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p docling-cli --locked

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
RUN cp /tmp/artifacts/docling.rs /usr/local/bin/ \
    && (cp /tmp/artifacts/libonnxruntime*.so* /usr/local/lib/ 2>/dev/null || true) \
    && rm -rf /tmp/artifacts \
    && ldconfig

COPY --from=models /opt/models /opt/models
COPY --from=models /opt/pdfium /opt/pdfium

# The pipeline finds its native lib + models via these env vars (the TableFormer
# graphs default to models/tableformer/*; set them explicitly so the binary works
# from any working directory).
# The `*_active` names are symlinks resolved by the models stage's INT8 arg
# (int8 by default, fp32 with --build-arg INT8=0); the concrete files are all
# present, so a container can pin either precision explicitly, e.g.
# `-e DOCLING_LAYOUT_ONNX=/opt/models/layout_heron.onnx` for fp32.
ENV PDFIUM_DYNAMIC_LIB_PATH=/opt/pdfium/lib \
    DOCLING_LAYOUT_ONNX=/opt/models/layout_heron_active.onnx \
    DOCLING_OCR_REC_ONNX=/opt/models/ocr_rec.onnx \
    DOCLING_OCR_DICT=/opt/models/ppocr_keys_v1.txt \
    DOCLING_TABLEFORMER_ENCODER=/opt/models/tableformer/encoder.onnx \
    DOCLING_TABLEFORMER_DECODER=/opt/models/tableformer/decoder_active.onnx \
    DOCLING_TABLEFORMER_BBOX=/opt/models/tableformer/bbox.onnx \
    LD_LIBRARY_PATH=/usr/local/lib:/opt/pdfium/lib

# Default: read a file arg and print Markdown to stdout, e.g.
#   docker run --rm -v "$PWD:/data" docling.rs /data/input.pdf --to json
ENTRYPOINT ["docling.rs"]
