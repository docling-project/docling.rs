# docling-serve — container image for the HTTP conversion API.
#
# Build (from the repository root):
#   docker build -f crates/docling-serve/Dockerfile -t docling-serve .
#
# Run. The ML assets (ONNX models + pdfium) are fetched into the image by
# `download_dependencies.sh` at build time; to keep the image slim instead,
# build with `--build-arg FETCH_ASSETS=0` and mount them:
#   docker run -p 5001:5001 docling-serve
#   docker run -p 5001:5001 -v ./models:/app/models -v ./.pdfium:/app/.pdfium docling-serve
#
# The server binds 0.0.0.0 inside the container (the container boundary is the
# network policy); URL fetching is enabled — front with a policy proxy or add
# --no-url-fetch to CMD if the service should accept uploads only.

FROM rust:1.96-bookworm AS build
WORKDIR /src
COPY . .
ARG FETCH_ASSETS=1
RUN cargo build --release -p docling-serve
RUN if [ "$FETCH_ASSETS" = "1" ]; then sh scripts/install/download_dependencies.sh; \
    else mkdir -p models .pdfium; fi

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /src/target/release/docling-serve /usr/local/bin/docling-serve
# Assets resolve relative to the working directory (`models/`, `.pdfium/lib`).
COPY --from=build /src/models ./models
COPY --from=build /src/.pdfium ./.pdfium
ENV PDFIUM_DYNAMIC_LIB_PATH=/app/.pdfium/lib
EXPOSE 5001
# Liveness: GET /health; readiness: GET /ready (503 until --warmup finishes).
CMD ["docling-serve", "--addr", "0.0.0.0:5001", "--warmup"]
