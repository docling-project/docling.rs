"""Model/asset download for the docling.rs Python bindings.

Mirrors how Python docling manages its artifacts: models are fetched once
into a per-user cache directory (default ``~/.cache/docling.rs``, override
with ``$DOCLING_RS_CACHE_DIR``) and the pipeline is pointed at them via the
same ``DOCLING_*`` / ``PDFIUM_*`` environment variables the Rust CLI uses.
Assets come from this repo's GitHub model release
(https://github.com/docling-project/docling.rs/releases/tag/models-v1 — override the
base URL with ``$DOCLING_RS_MODELS_URL``).

Usage::

    import docling_rs
    docling_rs.download_models()          # once; idempotent, skips present files

``DocumentConverter`` calls :func:`ensure_env` automatically, so after the
one-time download no configuration is needed at all.
"""

from __future__ import annotations

import os
import sys
import urllib.request
from pathlib import Path

BASE_URL = os.environ.get(
    "DOCLING_RS_MODELS_URL",
    "https://github.com/docling-project/docling.rs/releases/download/models-v1",
)

# release asset name -> path under the cache dir (the CLI's layout).
_REQUIRED = {
    "layout_heron.onnx": "models/layout_heron.onnx",
    "ocr_rec.onnx": "models/ocr_rec.onnx",
    "ppocr_keys_v1.txt": "models/ppocr_keys_v1.txt",
    "encoder.onnx": "models/tableformer/encoder.onnx",
    "decoder.onnx": "models/tableformer/decoder.onnx",
    "bbox.onnx": "models/tableformer/bbox.onnx",
}
# Fetched when the release hosts them; a 404 is fine (older tag, optional
# sidecars, INT8 variants — the pipeline falls back to fp32 gracefully).
_OPTIONAL = {
    "layout_heron_int8.onnx": "models/layout_heron_int8.onnx",
    "decoder_int8.onnx": "models/tableformer/decoder_int8.onnx",
    "encoder.onnx.data": "models/tableformer/encoder.onnx.data",
    "decoder.onnx.data": "models/tableformer/decoder.onnx.data",
    "bbox.onnx.data": "models/tableformer/bbox.onnx.data",
}
# pdfium rasterizer (Linux x64, matching what the release hosts).
_PDFIUM = {"libpdfium.so": ".pdfium/lib/libpdfium.so"}


def cache_dir() -> Path:
    """The asset cache root (``$DOCLING_RS_CACHE_DIR`` or ``~/.cache/docling.rs``)."""
    if env := os.environ.get("DOCLING_RS_CACHE_DIR"):
        return Path(env)
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "docling.rs"


def _fetch(url: str, dest: Path, optional: bool, progress: bool) -> bool:
    if dest.exists():
        return True
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".download")
    try:
        if progress:
            print(f"  > {dest}", file=sys.stderr, flush=True)
        with urllib.request.urlopen(url) as r, open(tmp, "wb") as f:
            while chunk := r.read(1 << 20):
                f.write(chunk)
        tmp.rename(dest)
        return True
    except Exception:
        tmp.unlink(missing_ok=True)
        if optional:
            return False
        raise


def download_models(dest: "str | Path | None" = None, progress: bool = True) -> Path:
    """Fetch the PDF/image pipeline's models + pdfium into the cache (idempotent).

    Returns the cache root. Pass ``dest`` to use a custom directory (also set
    it as ``$DOCLING_RS_CACHE_DIR`` at runtime, or pass the same value as
    ``DocumentConverter(artifacts_path=...)``).
    """
    root = Path(dest) if dest else cache_dir()
    if progress:
        print(f"docling.rs: fetching models to {root}", file=sys.stderr, flush=True)
    for name, rel in {**_REQUIRED, **_PDFIUM}.items():
        _fetch(f"{BASE_URL}/{name}", root / rel, optional=False, progress=progress)
    for name, rel in _OPTIONAL.items():
        _fetch(f"{BASE_URL}/{name}", root / rel, optional=True, progress=progress)
    return root


def _setdefault_if_exists(var: str, path: Path) -> None:
    if var not in os.environ and path.exists():
        os.environ[var] = str(path)


def ensure_env(dest: "str | Path | None" = None) -> Path:
    """Point the native pipeline at the cached assets via the ``DOCLING_*`` /
    ``PDFIUM_*`` env vars (only filling ones that are not already set, so
    explicit configuration always wins). Prefers the INT8 models when present,
    matching the Rust pipeline's default; ``DOCLING_RS_FP32=1`` opts out.
    Safe to call when nothing is downloaded yet — missing files simply leave
    the env untouched (and the converter will fail with its usual clear
    "model not found" message)."""
    root = Path(dest) if dest else cache_dir()
    fp32 = os.environ.get("DOCLING_RS_FP32", "0") not in ("", "0")
    m = root / "models"

    layout = m / "layout_heron_int8.onnx"
    if fp32 or not layout.exists():
        layout = m / "layout_heron.onnx"
    _setdefault_if_exists("DOCLING_LAYOUT_ONNX", layout)

    decoder = m / "tableformer/decoder_int8.onnx"
    if fp32 or not decoder.exists():
        decoder = m / "tableformer/decoder.onnx"
    _setdefault_if_exists("DOCLING_TABLEFORMER_DECODER", decoder)

    _setdefault_if_exists("DOCLING_OCR_REC_ONNX", m / "ocr_rec.onnx")
    _setdefault_if_exists("DOCLING_OCR_DICT", m / "ppocr_keys_v1.txt")
    _setdefault_if_exists("DOCLING_TABLEFORMER_ENCODER", m / "tableformer/encoder.onnx")
    _setdefault_if_exists("DOCLING_TABLEFORMER_BBOX", m / "tableformer/bbox.onnx")
    _setdefault_if_exists("PDFIUM_DYNAMIC_LIB_PATH", root / ".pdfium/lib")
    return root
