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
one-time download no configuration is needed at all. Local assets outrank the
cache: when a matching ``models/`` / ``.pdfium/`` asset exists in the working
directory (e.g. a repo checkout with its own exports), the env var is left
unset and the native pipeline resolves the local path itself, exactly like
the Rust CLI. Re-published release assets are picked up with
``download_models(force=True)`` — the cache has no version stamp.
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
    # The #97 hoisted-KV TableFormer decoder — byte-exact vs the legacy graph
    # and the fastest variant on every machine measured; ensure_env prefers it.
    "decoder_kv.onnx": "models/tableformer/decoder_kv.onnx",
    "decoder_kv.onnx.data": "models/tableformer/decoder_kv.onnx.data",
    "decoder_kv_int8.onnx": "models/tableformer/decoder_kv_int8.onnx",
    # DocumentFigureClassifier-v2.5 (~17 MB) for do_picture_classification;
    # missing file just skips the enrichment with a one-time warning.
    "picture_classifier.onnx": "models/picture_classifier.onnx",
    "encoder.onnx.data": "models/tableformer/encoder.onnx.data",
    "decoder.onnx.data": "models/tableformer/decoder.onnx.data",
    "bbox.onnx.data": "models/tableformer/bbox.onnx.data",
    # The hybrid chunker's default tokenizer (all-MiniLM-L6-v2's, ~0.5 MB);
    # falls back to Hugging Face below when the release doesn't host it.
    "chunk_tokenizer.json": "models/chunk/tokenizer.json",
}

# CodeFormula (do_code_enrichment / do_formula_enrichment) — the int8 decoder
# (~165 MB) makes the ~655 MB fp32 decoder unnecessary (same rule as
# download_dependencies.sh), so the fp32 graph is fetched only when the int8
# variant isn't hosted.
_ENRICH = {
    "cf_vision.onnx": "models/code_formula/vision.onnx",
    "cf_embed.onnx": "models/code_formula/embed.onnx",
    "cf_decoder_kv_int8.onnx": "models/code_formula/decoder_kv_int8.onnx",
    "cf_tokenizer.json": "models/code_formula/tokenizer.json",
}
_ENRICH_FP32_DECODER = ("cf_decoder_kv.onnx", "models/code_formula/decoder_kv.onnx")

# Straight-from-upstream fallback for assets older release tags don't host:
# cache path -> upstream URL.
_FALLBACK_URLS = {
    "models/chunk/tokenizer.json": (
        "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json"
    ),
    "models/picture_classifier.onnx": (
        "https://huggingface.co/docling-project/DocumentFigureClassifier-v2.5/resolve/main/model.onnx"
    ),
}
# pdfium rasterizer (Linux x64, matching what the release hosts).
_PDFIUM = {"libpdfium.so": ".pdfium/lib/libpdfium.so"}


def cache_dir() -> Path:
    """The asset cache root (``$DOCLING_RS_CACHE_DIR`` or ``~/.cache/docling.rs``)."""
    if env := os.environ.get("DOCLING_RS_CACHE_DIR"):
        return Path(env)
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "docling.rs"


def _fetch(url: str, dest: Path, optional: bool, progress: bool, force: bool = False) -> bool:
    if dest.exists() and not force:
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


def download_models(
    dest: "str | Path | None" = None, progress: bool = True, force: bool = False
) -> Path:
    """Fetch the PDF/image pipeline's models + pdfium into the cache (idempotent).

    Returns the cache root. Pass ``dest`` to use a custom directory (also set
    it as ``$DOCLING_RS_CACHE_DIR`` at runtime, or pass the same value as
    ``DocumentConverter(artifacts_path=...)``). Pass ``force=True`` to
    re-download files that are already cached — the cache has no version
    stamp, so this is how a stale cache picks up re-published model assets
    (e.g. the dynamic-batch layout graph or the hoisted-KV TableFormer
    decoder).
    """
    root = Path(dest) if dest else cache_dir()
    if progress:
        print(f"docling.rs: fetching models to {root}", file=sys.stderr, flush=True)
    for name, rel in {**_REQUIRED, **_PDFIUM}.items():
        _fetch(f"{BASE_URL}/{name}", root / rel, optional=False, progress=progress, force=force)
    for name, rel in {**_OPTIONAL, **_ENRICH}.items():
        if not _fetch(
            f"{BASE_URL}/{name}", root / rel, optional=True, progress=progress, force=force
        ):
            if fallback := _FALLBACK_URLS.get(rel):
                _fetch(fallback, root / rel, optional=True, progress=progress, force=force)
    # The huge fp32 CodeFormula decoder only matters when its int8 variant
    # isn't hosted (or DOCLING_RS_FP32 users fetch it here as the fallback).
    name, rel = _ENRICH_FP32_DECODER
    if not (root / _ENRICH["cf_decoder_kv_int8.onnx"]).exists():
        _fetch(f"{BASE_URL}/{name}", root / rel, optional=True, progress=progress, force=force)
    return root


def _point_at(var: str, local: "list[str]", cached: Path) -> None:
    """Set ``var`` to ``cached`` unless configuration already exists.

    Two things outrank the cache: an env var the caller already set, and a
    matching asset in the working directory (any of the ``local`` relative
    paths) — the native pipeline resolves those CWD paths itself when the env
    var stays unset, exactly like the Rust CLI run from a checkout. The env
    is also left untouched when ``cached`` doesn't exist."""
    if var in os.environ:
        return
    if any(Path(rel).exists() for rel in local):
        return
    if cached.exists():
        os.environ[var] = str(cached)


def ensure_env(dest: "str | Path | None" = None) -> Path:
    """Point the native pipeline at the cached assets via the ``DOCLING_*`` /
    ``PDFIUM_*`` env vars. Local assets win: a variable is only filled when it
    is not already set AND no matching ``models/`` / ``.pdfium/`` asset exists
    in the working directory (the native code resolves those itself, so a repo
    checkout keeps using its own exports). Prefers the INT8 models when
    present, matching the Rust pipeline's default; ``DOCLING_RS_FP32=1`` opts
    out. Safe to call when nothing is downloaded yet — missing files simply
    leave the env untouched (and the converter will fail with its usual clear
    "model not found" message)."""
    root = Path(dest) if dest else cache_dir()
    fp32 = os.environ.get("DOCLING_RS_FP32", "0") not in ("", "0")
    m = root / "models"

    layout_chain = ["models/layout_heron.onnx"]
    if not fp32:
        layout_chain.insert(0, "models/layout_heron_int8.onnx")
    layout = m / "layout_heron_int8.onnx"
    if fp32 or not layout.exists():
        layout = m / "layout_heron.onnx"
    _point_at("DOCLING_LAYOUT_ONNX", layout_chain, layout)

    # TableFormer decoder preference, mirroring the Rust pipeline's default
    # chain (tableformer.rs): the #97 hoisted-KV graph ranks ahead of the
    # legacy layer-output-cache graph within each precision, and decoder_kv
    # (fp32) ranks above the quantized *legacy* decoder — it is faster on
    # every machine measured and byte-exact.
    if fp32:
        chain = ["tableformer/decoder_kv.onnx", "tableformer/decoder.onnx"]
    else:
        chain = [
            "tableformer/decoder_kv_int8.onnx",
            "tableformer/decoder_kv.onnx",
            "tableformer/decoder_int8.onnx",
            "tableformer/decoder.onnx",
        ]
    decoder = next((p for rel in chain if (p := m / rel).exists()), m / "tableformer/decoder.onnx")
    _point_at("DOCLING_TABLEFORMER_DECODER", [f"models/{rel}" for rel in chain], decoder)

    classifier_chain = ["models/picture_classifier.onnx"]
    if not fp32:
        classifier_chain.insert(0, "models/picture_classifier_int8.onnx")
    classifier = m / "picture_classifier_int8.onnx"
    if fp32 or not classifier.exists():
        classifier = m / "picture_classifier.onnx"
    _point_at("DOCLING_PICTURE_CLASSIFIER_ONNX", classifier_chain, classifier)

    _point_at("DOCLING_OCR_REC_ONNX", ["models/ocr_rec.onnx"], m / "ocr_rec.onnx")
    _point_at("DOCLING_OCR_DICT", ["models/ppocr_keys_v1.txt"], m / "ppocr_keys_v1.txt")
    _point_at(
        "DOCLING_TABLEFORMER_ENCODER",
        ["models/tableformer/encoder.onnx"],
        m / "tableformer/encoder.onnx",
    )
    _point_at(
        "DOCLING_TABLEFORMER_BBOX",
        ["models/tableformer/bbox.onnx"],
        m / "tableformer/bbox.onnx",
    )
    _point_at(
        "DOCLING_CODE_FORMULA_DIR",
        ["models/code_formula"],
        m / "code_formula",
    )
    _point_at("PDFIUM_DYNAMIC_LIB_PATH", [".pdfium/lib"], root / ".pdfium/lib")
    return root
