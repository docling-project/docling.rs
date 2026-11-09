# docling.rs container images: the `docling-serve` HTTP conversion API and the
# `docling-rs` CLI, built from one Dockerfile as multiple targets over shared
# layers (same dependency compile, same runtime base, same ML assets).
# The MCP server alone, from PyPI and without the models, is the root
# Dockerfile.
#
# Standard CPU targets (built on Debian Trixie):
#   docker build -f crates/docling-serve/Dockerfile --target serve -t docling-rs-serve .
#   docker build -f crates/docling-serve/Dockerfile --target cli   -t docling-rs .
# (`serve` is the default CPU target if --target is omitted.)
#
# NVIDIA CUDA 13 GPU targets (built with --features cuda on Ubuntu 24.04 + cuDNN 9;
# host driver >= 580):
#   docker build -f crates/docling-serve/Dockerfile --target serve-cuda -t docling-rs-serve-cuda .
#   docker build -f crates/docling-serve/Dockerfile --target cli-cuda   -t docling-rs-cuda .
#
# CLI image: the entrypoint is `docling-rs`, the working directory /data — mount
# the documents there and pass the usual flags:
#   docker run --rm -v "$PWD:/data" docling-rs report.pdf --to md
#   docker run --rm -v "$PWD:/data" docling-rs --input in --output out --to latex
#
# Run. The ML assets (the ONNX models) are fetched into the image by
# `download_dependencies.sh` at build time; to keep the image slim instead,
# build with `--build-arg FETCH_ASSETS=0` and mount them:
#   docker run -p 5001:5001 docling-rs-serve
#   docker run -p 5001:5001 -v ./models:/app/.models docling-rs-serve
# Speech recognition beyond the default Whisper tiny: bake more presets in
# with `--build-arg ASR_MODELS="parakeet_tdt_0.6b_v3 whisper_small_en"`.
#
# OCR with Tesseract (`ocr_engine=tesseract`, #460): the images build
# Tesseract 5.5.3 from source (the distro packages are older — trixie 5.5.0,
# Ubuntu 24.04 5.3.4) and bake in every language model of tessdata_fast
# (~ 340 MB; `ocr_lang=rus+eng`, `deu`, `chi_sim`, …). A slimmer image:
# `--build-arg TESSDATA_LANGS="eng osd rus"` (tessdata stems); the script
# models too: `TESSDATA_LANGS="all scripts"` (~ 670 MB, `script/Cyrillic`);
# another release: `--build-arg TESSERACT_VERSION=5.5.2`.
#
# GPU run: pass --gpus all to expose the host NVIDIA GPU:
#   docker run --gpus all -p 5001:5001 docling-rs-serve-cuda
#   docker run --gpus all --rm -v "$PWD:/data" docling-rs-cuda report.pdf --to md
#
# The server binds 0.0.0.0 inside the container (the container boundary is the
# network policy); URL fetching is enabled — front with a policy proxy or add
# --no-url-fetch to CMD if the service should accept uploads only.
#
# trixie, not bookworm (#246): ort's prebuilt static onnxruntime is built
# with GCC 13 and references GLIBCXX_3.4.31 — bookworm's libstdc++ (GCC 12)
# tops out at 3.4.30, so the release link fails with undefined references.
# The runtime stage below must stay on the same Debian release: the binary
# links that libstdc++ (and trixie's glibc) dynamically.
#
# Layering for CI cache hits (docker-publish.yml exports every stage to the
# GitHub Actions cache): a plain `COPY . . && cargo build` invalidates the
# whole dependency compile on any commit, so the ~10 min build ran from
# scratch on every PR and push (`Cached 0%`). cargo-chef splits it — the
# `planner` stage reduces the workspace to its manifests (recipe.json), and
# `cook` compiles only the dependency graph from that recipe, so that layer
# is reused until Cargo.toml/Cargo.lock change. A source change then rebuilds
# just the workspace crates. The ML assets are fetched in their own stage
# that depends on nothing but the download script, so ~900 MB of models are
# not re-downloaded per build either.

# Tesseract release and tessdata_fast languages built into the runtimes (see
# the tesseract and tessdata stages).
ARG TESSERACT_VERSION=5.5.3
ARG TESSDATA_LANGS=all

# ------------------------------------------------------------------------------
# Stage: chef & planner (shared across CPU and CUDA builds)
# ------------------------------------------------------------------------------
FROM rust:1.96-trixie AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ------------------------------------------------------------------------------
# Stage: CPU build
# ------------------------------------------------------------------------------
FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
# Same profile/packages/flags as the real build below, or cargo recompiles.
RUN cargo chef cook --release -p docling-serve -p docling-cli --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release -p docling-serve -p docling-cli --locked

# ------------------------------------------------------------------------------
# Stage: CUDA build (Linux x86_64, ort/cuda + copy-dylibs on Ubuntu 24.04 devel)
#
# Ubuntu 24.04 (glibc 2.39 + GCC 14) matches the runtime-cuda base below (#333),
# avoiding glibc version mismatches between build and runtime environments.
# ------------------------------------------------------------------------------
# CUDA 13 (#664): pyke's ONNX Runtime 1.28 build for ort 2.0.0-rc.13 — the
# only CUDA build it ships for this target — links libcublas/libcudart .so.13,
# so a CUDA 12 base cannot load its CUDA provider. ORT_CUDA_VERSION pins the
# download instead of leaving it to ort's detection. 13.0, the oldest 13.x:
# the NVIDIA runtime admits it on any driver with CUDA 13 support (>= 580).
FROM nvidia/cuda:13.0.3-cudnn-devel-ubuntu24.04 AS chef-cuda
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl build-essential pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    ORT_CUDA_VERSION=13
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.96.0 --profile minimal \
    && cargo install cargo-chef --locked
WORKDIR /src

FROM chef-cuda AS build-cuda
COPY --from=planner /src/recipe.json recipe.json
# Build dependency tree with --features cuda so ONNX Runtime with CUDA is fetched and cooked.
RUN cargo chef cook --release -p docling-serve -p docling-cli --features cuda --locked --recipe-path recipe.json
COPY . .
# ort's copy-dylibs leaves the provider libraries in target/release as
# symlinks into its download cache (~/.cache/ort.pyke.io), which no later
# stage has: copy the files they point to (`cp -L`), and fail the build if a
# symlink still reached the copy — `cp -a` shipped dangling links (#664).
RUN cargo build --release -p docling-serve -p docling-cli --features cuda --locked \
    && mkdir -p /src/cuda-dylibs \
    && test $(find -L target/release -maxdepth 1 -name "libonnxruntime_providers_*.so" -type f | wc -l) -ge 2 \
    && find target/release -maxdepth 1 -name "libonnxruntime_providers_*.so" -exec cp -vL {} /src/cuda-dylibs/ \; \
    && ! find /src/cuda-dylibs -type l | grep .
# ------------------------------------------------------------------------------
# Stage: IBM Z (s390x) cross build (#504)
#
# Runs on the *build* platform (--platform=$BUILDPLATFORM) with the distro
# cross toolchain: an s390x `rust` stage would compile the whole workspace
# under QEMU for hours. pyke's `ort` has no prebuilt ONNX Runtime for the
# target, so the binaries are built with `ort-load-dynamic` and dlopen the
# libonnxruntime.so the assets stage fetches into .models/onnxruntime/
# (download_dependencies.sh does so on an s390x host). No cargo-chef here:
# the planner stage is a target-platform stage, i.e. QEMU again.
# ------------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM rust:1.96-trixie AS build-s390x
RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc-s390x-linux-gnu g++-s390x-linux-gnu \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add s390x-unknown-linux-gnu
ENV CARGO_TARGET_S390X_UNKNOWN_LINUX_GNU_LINKER=s390x-linux-gnu-gcc \
    CC_s390x_unknown_linux_gnu=s390x-linux-gnu-gcc \
    CXX_s390x_unknown_linux_gnu=s390x-linux-gnu-g++ \
    AR_s390x_unknown_linux_gnu=s390x-linux-gnu-ar
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p docling-serve -p docling-cli \
    --features docling-serve/ort-load-dynamic,docling-cli/ort-load-dynamic \
    --target s390x-unknown-linux-gnu

# ------------------------------------------------------------------------------
# Stage: assets (the ONNX models, shared across CPU and CUDA)
# ------------------------------------------------------------------------------
FROM debian:trixie-slim AS assets
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY scripts/install/download_dependencies.sh scripts/install/download_dependencies.sh
ARG FETCH_ASSETS=1
# Extra speech-recognition presets to bake in (#508), space- or comma-separated
# `--asr-model` names of download_dependencies.sh — e.g.
# `--build-arg ASR_MODELS=parakeet_tdt_0.6b_v3` (~650 MB int8 + the Silero
# VAD) or `whisper_small_en`. Whisper tiny, the default model, is always
# fetched; requests pick a preset with the `asr_model` option.
ARG ASR_MODELS=""
RUN if [ "$FETCH_ASSETS" = "1" ]; then \
      set -- ; for m in $(echo "$ASR_MODELS" | tr ',' ' '); do set -- "$@" "--asr-model=$m"; done; \
      sh scripts/install/download_dependencies.sh "$@"; \
    else mkdir -p .models; fi

# ------------------------------------------------------------------------------
# Stage: Tesseract OCR from source (`ocr_engine=tesseract`, #460) — the CLI
# only, libtesseract linked in, against the distro's Leptonica (see
# scripts/install/build_tesseract.sh). Built on the build platform and
# cross-compiled for a foreign TARGETARCH (s390x) like the Rust binaries:
# compiling Tesseract under QEMU takes the better part of an hour. Depends on
# nothing but the script and TESSERACT_VERSION, so the cache keeps it.
# ------------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM debian:trixie-slim AS tesseract
ARG TARGETARCH
ARG TESSERACT_VERSION
COPY scripts/install/build_tesseract.sh /usr/local/bin/build_tesseract.sh
RUN set -eu; host=$(dpkg --print-architecture); arch=${TARGETARCH:-$host}; \
    case "$arch" in ppc64le) arch=ppc64el ;; esac; \
    if [ "$arch" = "$host" ]; then pkgs="g++ libleptonica-dev"; \
    else dpkg --add-architecture "$arch"; pkgs="crossbuild-essential-$arch dpkg-dev libleptonica-dev:$arch"; fi; \
    apt-get update && apt-get install -y --no-install-recommends ca-certificates git cmake make pkgconf $pkgs \
    && rm -rf /var/lib/apt/lists/* \
    && TARGET_ARCH="$arch" DESTDIR=/out WORK_DIR=/tmp/tesseract sh /usr/local/bin/build_tesseract.sh \
    && rm -rf /tmp/tesseract

# The same build for the CUDA runtime's Ubuntu 24.04 (glibc 2.39, Leptonica
# 1.82): the trixie binary needs glibc 2.41 and libleptonica6.
FROM ubuntu:24.04 AS tesseract-cuda
ARG TESSERACT_VERSION
COPY scripts/install/build_tesseract.sh /usr/local/bin/build_tesseract.sh
RUN apt-get update && apt-get install -y --no-install-recommends \
       ca-certificates git cmake make pkgconf g++ libleptonica-dev \
    && rm -rf /var/lib/apt/lists/* \
    && DESTDIR=/out WORK_DIR=/tmp/tesseract sh /usr/local/bin/build_tesseract.sh \
    && rm -rf /tmp/tesseract

# ------------------------------------------------------------------------------
# Stage: tessdata — Tesseract's language models from tessdata_fast (the
# LSTM models Debian's tesseract-ocr-* packages ship too), pinned to its
# 4.1.0 tag. TESSDATA_LANGS, space- or comma-separated: `all` (the default,
# #889) = every language model (~ 130, `osd` for the orientation detection
# among them), `scripts` = the script models (`script/Cyrillic`, …), anything
# else a tessdata stem (`eng osd rus`). A sparse, blob-less checkout fetches
# only the files asked for; a stem that does not exist fails the build.
# ------------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM debian:trixie-slim AS tessdata
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
ARG TESSDATA_LANGS
ARG TESSDATA_REF=4.1.0
RUN set -eu; set --; \
    for lang in $(echo "$TESSDATA_LANGS" | tr ',' ' '); do \
      case "$lang" in \
        all) set -- "$@" '/*.traineddata' ;; \
        scripts) set -- "$@" '/script/*.traineddata' ;; \
        *) set -- "$@" "/$lang.traineddata" ;; \
      esac; \
    done; \
    git clone --quiet --depth 1 --filter=blob:none --no-checkout --branch "$TESSDATA_REF" \
      https://github.com/tesseract-ocr/tessdata_fast /tmp/tessdata_fast; \
    git -C /tmp/tessdata_fast sparse-checkout set --no-cone "$@"; \
    git -C /tmp/tessdata_fast checkout --quiet; \
    for lang in $(echo "$TESSDATA_LANGS" | tr ',' ' '); do \
      case "$lang" in all|scripts) ;; *) test -f "/tmp/tessdata_fast/$lang.traineddata" \
        || { echo "TESSDATA_LANGS: no $lang.traineddata in tessdata_fast $TESSDATA_REF" >&2; exit 1; } ;; esac; \
    done; \
    mkdir /tessdata; \
    cd /tmp/tessdata_fast && find . -name '*.traineddata' -exec cp --parents {} /tessdata/ \; ; \
    rm -rf /tmp/tessdata_fast; \
    ls /tessdata | wc -l

# ------------------------------------------------------------------------------
# Stage: runtime (CPU base, Debian Trixie slim)
# ------------------------------------------------------------------------------
FROM debian:trixie-slim AS runtime
# fonts-liberation + fonts-dejavu-core: the faces the pure-Rust page renderer
# substitutes for fonts a PDF does not embed (the base-14 Helvetica/Times/
# Courier references of most office exports) — without any font on the
# system those glyphs draw as placeholder boxes and the layout model sees a
# different page.
# libleptonica6: the one library the Tesseract build links (tesseract stage).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg libleptonica6 curl \
       fonts-liberation fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
# Tesseract and its configs under /usr/local (its compiled-in tessdata
# directory is /usr/local/share/tessdata), the language models beside them.
# The check fails the build on a missing library or a binary that does not
# read that directory (build_tesseract.sh: without a compiled-in path it
# reads `./`, and `--list-langs` walks the filesystem).
COPY --from=tesseract /out/usr/local/ /usr/local/
COPY --from=tessdata /tessdata/ /usr/local/share/tessdata/
RUN tesseract --version \
    && timeout 60 tesseract --list-langs 2>&1 | tee /dev/stderr \
       | grep -q '^List of available languages in "/usr/local/share/tessdata/"'
# #633: the page render — the layout model's input — depends on the fallback
# faces, so it is pinned to the two packages above: a derived image's extra
# fonts or a bind-mounted $HOME cannot change it. Rendering other scripts
# (a CJK deck) means installing the font and adding its directory to
# DOCLING_RS_FONT_DIRS, or unsetting DOCLING_RS_SYSTEM_FONTS to search the
# host again.
ENV DOCLING_RS_SYSTEM_FONTS=0 \
    DOCLING_RS_FONT_DIRS=/usr/share/fonts/truetype/liberation:/usr/share/fonts/truetype/dejavu
COPY crates/docling-pdf/data/mapping-resources-pdf/LICENSE.txt /usr/share/doc/docling.rs/LICENSE-mapping-resources-pdf.txt
WORKDIR /app
COPY --from=assets /src/.models ./.models

# ------------------------------------------------------------------------------
# Stage: runtime-cuda (CUDA 13 + cuDNN 9 base on Ubuntu 24.04)
# ------------------------------------------------------------------------------
FROM nvidia/cuda:13.0.3-cudnn-runtime-ubuntu24.04 AS runtime-cuda
# liblept5: Ubuntu 24.04's Leptonica runtime (tesseract-cuda stage).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg liblept5 curl \
       fonts-liberation fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
COPY --from=tesseract-cuda /out/usr/local/ /usr/local/
COPY --from=tessdata /tessdata/ /usr/local/share/tessdata/
RUN tesseract --version \
    && timeout 60 tesseract --list-langs 2>&1 | tee /dev/stderr \
       | grep -q '^List of available languages in "/usr/local/share/tessdata/"'
COPY crates/docling-pdf/data/mapping-resources-pdf/LICENSE.txt /usr/share/doc/docling.rs/LICENSE-mapping-resources-pdf.txt
WORKDIR /app
COPY --from=assets /src/.models ./.models
# ONNX Runtime opens its providers by absolute path from the directory of the
# binary it is linked into — never through LD_LIBRARY_PATH (#664) — so they
# must be reachable next to /usr/local/bin/docling-{serve,rs}: the files live
# in /usr/local/lib, linked from /usr/local/bin. A binary started by bare
# name re-executes under its absolute path to keep that lookup off the
# working directory.
COPY --from=build-cuda /src/cuda-dylibs/ /usr/local/lib/
RUN ln -s /usr/local/lib/libonnxruntime_providers_*.so /usr/local/bin/
# Fonts pinned as in the CPU runtime stage (#633).
ENV LD_LIBRARY_PATH=/usr/local/lib:${LD_LIBRARY_PATH:-} \
    DOCLING_RS_EP=auto \
    DOCLING_RS_SYSTEM_FONTS=0 \
    DOCLING_RS_FONT_DIRS=/usr/share/fonts/truetype/liberation:/usr/share/fonts/truetype/dejavu

# ==============================================================================
# Final targets: CPU
# ==============================================================================

# --- ghcr.io/docling-project/docling-rs: the CLI (CPU) ------------------------
FROM runtime AS cli
# The official MCP registry (#669) checks this label to accept the image as
# the `io.github.docling-project/docling-rs` server's OCI package
# (server.json): `docker run -i --rm -v DIR:/data <image> mcp --root /data`.
LABEL io.modelcontextprotocol.server.name="io.github.docling-project/docling-rs"
COPY --from=build /src/target/release/docling-rs /usr/local/bin/docling-rs
# The CLI runs from /data (the user's mount), so `.models/…` cannot resolve
# CWD-relative; point the whole-directory override (#285) at the baked assets.
ENV DOCLING_RS_MODELS_DIR=/app/.models
WORKDIR /data
ENTRYPOINT ["docling-rs"]

# --- ghcr.io/docling-project/docling-rs-serve: the HTTP API (CPU) -------------
FROM runtime AS serve
COPY --from=build /src/target/release/docling-serve /usr/local/bin/docling-serve
# Assets resolve relative to the working directory (`.models/`).
EXPOSE 5001
HEALTHCHECK --interval=15s --timeout=5s --start-period=45s --retries=3 \
    CMD curl -f http://localhost:5001/ready || exit 1
# Liveness: GET /health; readiness: GET /ready (503 until --warmup has loaded the
# models, and for good if they fail to load — the healthcheck then reports it).
CMD ["docling-serve", "--addr", "0.0.0.0:5001", "--warmup"]

# ==============================================================================
# Final targets: IBM Z (s390x, #504) — the CPU runtime stage for linux/s390x
# (Debian ships ffmpeg, Leptonica and the fonts for it), the cross-built
# binaries and Tesseract, and the ONNX Runtime the assets stage put in
# .models/onnxruntime/.
# ==============================================================================

# --- ghcr.io/docling-project/docling-rs (linux/s390x): the CLI ----------------
FROM runtime AS cli-s390x
# The official MCP registry (#669) checks this label to accept the image as
# the `io.github.docling-project/docling-rs` server's OCI package
# (server.json): `docker run -i --rm -v DIR:/data <image> mcp --root /data`.
LABEL io.modelcontextprotocol.server.name="io.github.docling-project/docling-rs"
COPY --from=build-s390x /src/target/s390x-unknown-linux-gnu/release/docling-rs /usr/local/bin/docling-rs
ENV DOCLING_RS_MODELS_DIR=/app/.models
WORKDIR /data
ENTRYPOINT ["docling-rs"]

# --- ghcr.io/docling-project/docling-rs-serve (linux/s390x): the HTTP API -----
FROM runtime AS serve-s390x
COPY --from=build-s390x /src/target/s390x-unknown-linux-gnu/release/docling-serve /usr/local/bin/docling-serve
EXPOSE 5001
HEALTHCHECK --interval=15s --timeout=5s --start-period=45s --retries=3 \
    CMD curl -f http://localhost:5001/ready || exit 1
CMD ["docling-serve", "--addr", "0.0.0.0:5001", "--warmup"]

# ==============================================================================
# Final targets: CUDA GPU
# ==============================================================================

# --- ghcr.io/docling-project/docling-rs-cuda: the CLI (CUDA) ------------------
FROM runtime-cuda AS cli-cuda
# The official MCP registry (#669) checks this label to accept the image as
# the `io.github.docling-project/docling-rs` server's OCI package
# (server.json): `docker run -i --rm -v DIR:/data <image> mcp --root /data`.
LABEL io.modelcontextprotocol.server.name="io.github.docling-project/docling-rs"
COPY --from=build-cuda /src/target/release/docling-rs /usr/local/bin/docling-rs
ENV DOCLING_RS_MODELS_DIR=/app/.models
WORKDIR /data
# Absolute, so ONNX Runtime finds its providers next to the binary (#664).
ENTRYPOINT ["/usr/local/bin/docling-rs"]

# --- ghcr.io/docling-project/docling-rs-serve-cuda: the HTTP API (CUDA) --------
FROM runtime-cuda AS serve-cuda
COPY --from=build-cuda /src/target/release/docling-serve /usr/local/bin/docling-serve
EXPOSE 5001
HEALTHCHECK --interval=15s --timeout=5s --start-period=45s --retries=3 \
    CMD curl -f http://localhost:5001/ready || exit 1
# Absolute, so ONNX Runtime finds its providers next to the binary (#664).
CMD ["/usr/local/bin/docling-serve", "--addr", "0.0.0.0:5001", "--warmup"]
