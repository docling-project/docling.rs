"""Model/asset download for the docling.rs Python bindings.

Mirrors how Python docling manages its artifacts: models are fetched once
into a per-user cache directory (default ``~/.cache/docling.rs``, override
with ``$DOCLING_RS_CACHE_DIR``) and the pipeline is pointed at them via the
same ``DOCLING_*`` environment variables the Rust CLI uses.
Assets come from this repo's GitHub model release
(https://github.com/docling-project/docling.rs/releases/tag/models-v1 — override the
base URL with ``$DOCLING_RS_MODELS_URL``).

Usage::

    import docling_rs
    docling_rs.download_models()          # once; idempotent, skips present files
    # audio/video: the speech-recognition models are opt-in, per preset
    docling_rs.download_models(asr_model="parakeet_tdt_0.6b_v3", pdf_models=False)

``DocumentConverter`` calls :func:`ensure_env` automatically, so after the
one-time download no configuration is needed at all. Local assets outrank the
cache: when a matching ``.models/`` asset exists in the
working directory (e.g. a repo checkout with its own exports), the env var is
left unset and the native pipeline resolves the local path itself, exactly
like the Rust CLI. Re-published release assets are picked up with
``download_models(force=True)`` — the cache has no version stamp. (The
cache's *internal* layout keeps the plain ``models/`` folder name — it lives
under the docling-owned cache root, so nothing collides.)

Integrity: every asset fetched from the release is checked against the
release's ``SHA256SUMS`` (``scripts/ci/models_manifest.sh`` regenerates it
after each upload) before it lands in the cache — a truncated transfer, a
tampered mirror behind ``$DOCLING_RS_MODELS_URL`` or a swapped asset raises
:class:`IntegrityError` instead of loading (``libonnxruntime.so`` on IBM Z
is code the process loads). The upstream fallback hosts (Hugging Face,
RapidOCR's hub) are outside the manifest. ``$DOCLING_RS_MODELS_VERIFY``:
``auto`` (default) checks what the manifest lists and warns, once, about a
release without one or an asset missing from it; ``require`` raises on
either; ``off`` skips the check — the same knob
``download_dependencies.sh`` reads.
"""

from __future__ import annotations

import hashlib
import os
import platform
import sys
import tarfile
import urllib.request
from pathlib import Path
from typing import Iterable

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
    # The English PP-OCRv3 recognition pair — the engine's ocr_lang="en"
    # *default* (#285; the ch_ pair above stays the docling-conformance
    # model, selected with ocr_lang="ch"). The release may not host them;
    # the fallbacks below fetch straight from upstream, mirroring
    # scripts/install/download_dependencies.sh.
    "ocr_rec_en.onnx": "models/ocr_rec_en.onnx",
    "en_dict.txt": "models/en_dict.txt",
    # The PP-OCRv6 text detector (#429): reads the lines the layout model
    # gives no region on bitmap pages (diagram labels, stamps). Optional —
    # without it OCR stays region-scoped; RapidOCR's hub is the fallback.
    "ocr_det.onnx": "models/ocr_det.onnx",
    # The PP-OCRv6 recognizer + dictionary (#570): RapidOCR's multilingual
    # model, docling's recognizer for every language; preferred over the v3
    # pairs when present. RapidOCR's hub is the fallback.
    "ocr_rec_v6.onnx": "models/ocr_rec_v6.onnx",
    "ocr_rec_v6_dict.txt": "models/ocr_rec_v6_dict.txt",
    # The text-line orientation classifier (#666): the 0/180 check of
    # ocr_textline_orientation. Optional; RapidOCR's hub is the fallback.
    "ocr_textline_ori.onnx": "models/ocr_textline_ori.onnx",
    "layout_heron_int8.onnx": "models/layout_heron_int8.onnx",
    "decoder_int8.onnx": "models/tableformer/decoder_int8.onnx",
    # The #97 hoisted-KV TableFormer decoder — byte-exact vs the legacy graph
    # and the fastest variant on every machine measured; ensure_env prefers it.
    "decoder_kv.onnx": "models/tableformer/decoder_kv.onnx",
    "decoder_kv.onnx.data": "models/tableformer/decoder_kv.onnx.data",
    # Its per-layer-cache graph over the same .data (#871): bit-identical,
    # caches in the loop's own buffers; the engine prefers it when present.
    "decoder_kv_layers.onnx": "models/tableformer/decoder_kv_layers.onnx",
    "decoder_kv_int8.onnx": "models/tableformer/decoder_kv_int8.onnx",
    # fp16-weight repack of the TableFormer encoder (#374): fp32 compute,
    # half the download; the Rust pipeline prefers it unless fp32 is forced.
    "encoder_fp16.onnx": "models/tableformer/encoder_fp16.onnx",
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

# dslim/bert-base-NER (MIT, ~430 MB): the names / organizations / locations
# detector of ``DocumentConverter(redact_pii=True)`` (#621) — the pass runs
# pattern-only without it. Opt-in (``download_models(ner=True)``), release
# asset name -> cache path, with Hugging Face's own ONNX export as the
# fallback (it ships the graph itself).
_NER = {
    "ner_model.onnx": "models/ner/model.onnx",
    "ner_tokenizer.json": "models/ner/tokenizer.json",
    "ner_config.json": "models/ner/config.json",
}
_NER_BASE_URL = os.environ.get(
    "DOCLING_RS_NER_MODELS_URL", "https://huggingface.co/dslim/bert-base-NER/resolve/main/onnx"
)

# IBM Z (#504): the s390x wheel has no ONNX Runtime linked in — pyke ships
# none for the target — and dlopens libonnxruntime.so from ``ORT_DYLIB_PATH``,
# else ``<models dir>/onnxruntime/`` (``DOCLING_RS_MODELS_DIR``, which
# ensure_env points at the cache), else the library search path. The models
# release hosts the library (ONNX Runtime 1.29.0, cross-compiled by the
# onnxruntime-s390x.yml workflow) as a tarball that unpacks into that
# directory; fetched on s390x hosts only.
_ORT_S390X = ("onnxruntime-linux-s390x.tar.gz", "models/onnxruntime")

# Straight-from-upstream fallback for assets older release tags don't host:
# cache path -> upstream URL.
_FALLBACK_URLS = {
    "models/ocr_rec_en.onnx": (
        "https://huggingface.co/SWHL/RapidOCR/resolve/main/PP-OCRv3/en_PP-OCRv3_rec_infer.onnx"
    ),
    "models/en_dict.txt": (
        "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/main/ppocr/utils/en_dict.txt"
    ),
    "models/ocr_det.onnx": (
        "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/det/PP-OCRv6_det_small.onnx"
    ),
    "models/ocr_rec_v6.onnx": (
        "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv6/rec/PP-OCRv6_rec_small.onnx"
    ),
    "models/ocr_rec_v6_dict.txt": (
        "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt"
    ),
    "models/ocr_textline_ori.onnx": (
        "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.2/onnx/PP-OCRv5/cls/ch_PP-LCNet_x0_25_textline_ori_cls_mobile.onnx"
    ),
    "models/chunk/tokenizer.json": (
        "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/tokenizer.json"
    ),
    "models/picture_classifier.onnx": (
        "https://huggingface.co/docling-project/DocumentFigureClassifier-v2.5/resolve/main/model.onnx"
    ),
}
# Speech recognition (audio/video), opt-in per preset via
# ``download_models(asr_model=…)`` — the same files and layout as
# ``download_dependencies.sh --asr-model=<preset>``, so the converter's
# ``asr_model`` kwarg finds them through ``DOCLING_RS_MODELS_DIR``.
#
# "whisper_tiny" is docling's default (multilingual Whisper tiny, the model a
# converter without ``asr_model`` uses) and lives in ``models/asr/`` itself;
# the release mirrors it as ``asr_*``, Hugging Face is the fallback host
# (``$DOCLING_RS_ASR_MODELS_URL`` replaces both, like the script).
_ASR_RELEASE_PREFIX = "asr_"
_WHISPER_TINY_URL = os.environ.get(
    "DOCLING_RS_ASR_MODELS_URL", "https://huggingface.co/onnx-community/whisper-tiny/resolve/main"
)
# The named Whisper presets (docling's English-only / Distil-Whisper specs with
# public ONNX exports): onnx-community repo per preset, each in
# ``models/asr/<preset>/``.
_WHISPER_PRESETS = {
    "whisper_tiny_en": "whisper-tiny.en",
    "whisper_base_en": "whisper-base.en",
    "whisper_small_en": "whisper-small.en",
    "whisper_distil_small_en": "distil-small.en",
}
# NVIDIA Parakeet TDT 0.6B v3 (#508; CC-BY-4.0), the onnx-asr export: the int8
# encoder (~650 MB) + decoder-joint by default, the fp32 graphs (~2.5 GB, the
# encoder weights in a .data sidecar) when DOCLING_RS_FP32 is set — the engine
# loads whichever is present, int8 first unless fp32 is forced. The Silero VAD
# (v5, MIT, ~2 MB) segments long recordings; optional at run time (without it
# an energy-based splitter takes over), fetched alongside.
_PARAKEET_PRESETS = ("parakeet_tdt_0.6b_v3",)
_PARAKEET_URL = os.environ.get(
    "DOCLING_RS_PARAKEET_MODELS_URL",
    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main",
)
_VAD_URL = os.environ.get(
    "DOCLING_RS_VAD_MODEL_URL",
    "https://huggingface.co/istupakov/silero-vad-onnx/resolve/main/silero_vad.onnx",
)
_VAD = "models/asr/vad/silero_vad.onnx"

#: Every value ``download_models(asr_model=…)`` and
#: ``DocumentConverter(asr_model=…)`` accept.
ASR_MODELS = ("whisper_tiny", *_WHISPER_PRESETS, *_PARAKEET_PRESETS)


def cache_dir() -> Path:
    """The asset cache root (``$DOCLING_RS_CACHE_DIR`` or ``~/.cache/docling.rs``)."""
    if env := os.environ.get("DOCLING_RS_CACHE_DIR"):
        return Path(env)
    return Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "docling.rs"


class IntegrityError(RuntimeError):
    """A release asset's sha256 does not match the release's ``SHA256SUMS``
    (or, under ``DOCLING_RS_MODELS_VERIFY=require``, cannot be checked). Never
    swallowed as an "optional asset missing": a tampered or corrupted
    download must not fall through to the upstream fallback host."""


_MANIFEST_NAME = "SHA256SUMS"
# base URL -> {asset name: sha256 hex}, or None for a release without a
# manifest (looked up once per process; the warning is printed once too).
_manifests: "dict[str, dict[str, str] | None]" = {}


def _verify_mode() -> str:
    mode = os.environ.get("DOCLING_RS_MODELS_VERIFY", "auto").strip().lower() or "auto"
    if mode not in ("auto", "require", "off"):
        raise ValueError(f"DOCLING_RS_MODELS_VERIFY must be auto, require or off (got {mode!r})")
    return mode


def _manifest(base_url: str) -> "dict[str, str] | None":
    """The release's ``SHA256SUMS`` as ``{name: hex}`` — ``sha256sum``'s
    ``<hex>  <name>`` lines — or ``None`` when the release hosts none."""
    if base_url in _manifests:
        return _manifests[base_url]
    entries: "dict[str, str] | None" = None
    try:
        with urllib.request.urlopen(f"{base_url}/{_MANIFEST_NAME}") as r:
            text = r.read().decode("utf-8", "replace")
        entries = {}
        for line in text.splitlines():
            parts = line.split()
            if len(parts) >= 2 and len(parts[0]) == 64:
                entries[parts[-1].lstrip("*")] = parts[0].lower()
    except Exception:
        entries = None
    if entries is None:
        if _verify_mode() == "require":
            raise IntegrityError(
                f"{base_url}/{_MANIFEST_NAME} unavailable and DOCLING_RS_MODELS_VERIFY=require"
            )
        print(
            f"docling.rs: {base_url} hosts no {_MANIFEST_NAME} — downloads from it are not "
            "integrity-checked",
            file=sys.stderr,
            flush=True,
        )
    _manifests[base_url] = entries
    return entries


def _expected_sha256(url: str) -> "str | None":
    """The manifest digest a download from the release must match, or
    ``None`` when the URL is outside the release (an upstream fallback host),
    the check is off, or the release/manifest does not cover it."""
    if _verify_mode() == "off" or not url.startswith(BASE_URL + "/"):
        return None
    name = url[len(BASE_URL) + 1 :]
    entries = _manifest(BASE_URL)
    if entries is None:
        return None
    want = entries.get(name)
    if want is None:
        if _verify_mode() == "require":
            raise IntegrityError(
                f"{name} is not listed in {BASE_URL}/{_MANIFEST_NAME} (DOCLING_RS_MODELS_VERIFY=require)"
            )
        print(
            f"docling.rs: {name} is not listed in {_MANIFEST_NAME} — not integrity-checked",
            file=sys.stderr,
            flush=True,
        )
    return want


def _fetch(url: str, dest: Path, optional: bool, progress: bool, force: bool = False) -> bool:
    if dest.exists() and not force:
        return True
    want = _expected_sha256(url)
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".download")
    try:
        if progress:
            print(f"  > {dest}", file=sys.stderr, flush=True)
        digest = hashlib.sha256()
        with urllib.request.urlopen(url) as r, open(tmp, "wb") as f:
            while chunk := r.read(1 << 20):
                digest.update(chunk)
                f.write(chunk)
        if want is not None and digest.hexdigest() != want:
            raise IntegrityError(
                f"{url}: sha256 mismatch (expected {want}, got {digest.hexdigest()}) — a corrupted "
                "download or a tampered mirror; retry, and only if it repeats set "
                "DOCLING_RS_MODELS_VERIFY=off to bypass the check at your own risk"
            )
        tmp.rename(dest)
        return True
    except IntegrityError:
        tmp.unlink(missing_ok=True)
        raise
    except Exception:
        tmp.unlink(missing_ok=True)
        if optional:
            return False
        raise


def download_models(
    dest: "str | Path | None" = None,
    progress: bool = True,
    force: bool = False,
    asr_model: "str | Iterable[str] | None" = None,
    pdf_models: bool = True,
    ner: bool = False,
    enrichment: bool = True,
) -> Path:
    """Fetch the PDF/image pipeline's models into the cache (idempotent).

    Returns the cache root. Pass ``dest`` to use a custom directory (also set
    it as ``$DOCLING_RS_CACHE_DIR`` at runtime, or pass the same value as
    ``DocumentConverter(artifacts_path=...)``). Pass ``force=True`` to
    re-download files that are already cached — the cache has no version
    stamp, so this is how a stale cache picks up re-published model assets
    (e.g. the dynamic-batch layout graph or the hoisted-KV TableFormer
    decoder).

    ``asr_model`` adds speech-recognition models for audio/video — one preset
    name or several (see :data:`ASR_MODELS`): ``"whisper_tiny"`` (the
    converter's default model), the Whisper presets, or
    ``"parakeet_tdt_0.6b_v3"`` (NVIDIA Parakeet TDT 0.6B v3, 25 European
    languages, plus the Silero VAD). None are fetched by default. Select the
    model at conversion time with ``DocumentConverter(asr_model=…)``.
    ``pdf_models=False`` skips the PDF/image models (~700 MB), e.g. for an
    audio-only install. ``ner=True`` adds the named-entity model of the PII
    redaction pass (``DocumentConverter(redact_pii=True)``, #621): dslim's
    ``bert-base-NER`` ONNX export (MIT, ~430 MB) into ``models/ner/`` — the
    release asset when hosted, else straight from Hugging Face.
    ``enrichment=False`` skips the CodeFormula models (~740 MB) that only
    ``do_code_enrichment`` / ``do_formula_enrichment`` use — what the
    ``docling-rs-mcp --download-models`` launcher fetches.
    """
    presets = _asr_presets(asr_model)
    root = Path(dest) if dest else cache_dir()
    if progress:
        print(f"docling.rs: fetching models to {root}", file=sys.stderr, flush=True)
    for preset in presets:
        _fetch_asr(root, preset, progress=progress, force=force)
    if ner:
        for name, rel in _NER.items():
            if not _fetch(
                f"{BASE_URL}/{name}", root / rel, optional=True, progress=progress, force=force
            ):
                _fetch(
                    f"{_NER_BASE_URL}/{name.removeprefix('ner_')}",
                    root / rel,
                    optional=False,
                    progress=progress,
                    force=force,
                )
    if not pdf_models:
        return root
    for name, rel in _REQUIRED.items():
        _fetch(f"{BASE_URL}/{name}", root / rel, optional=False, progress=progress, force=force)
    for name, rel in {**_OPTIONAL, **(_ENRICH if enrichment else {})}.items():
        if not _fetch(
            f"{BASE_URL}/{name}", root / rel, optional=True, progress=progress, force=force
        ):
            if fallback := _FALLBACK_URLS.get(rel):
                _fetch(fallback, root / rel, optional=True, progress=progress, force=force)
    # The huge fp32 CodeFormula decoder only matters when its int8 variant
    # isn't hosted (or DOCLING_RS_FP32 users fetch it here as the fallback).
    name, rel = _ENRICH_FP32_DECODER
    if enrichment and not (root / _ENRICH["cf_decoder_kv_int8.onnx"]).exists():
        _fetch(f"{BASE_URL}/{name}", root / rel, optional=True, progress=progress, force=force)
    if platform.machine() == "s390x":
        _fetch_onnxruntime(root, progress=progress, force=force)
    return root


_CHUNK_TOKENIZER = "models/chunk/tokenizer.json"


def chunk_tokenizer(fetch: bool = False, progress: bool = False) -> "Path | None":
    """The hybrid chunker's default ``tokenizer.json`` (all-MiniLM-L6-v2's),
    where the native resolver would find it: ``./.models/chunk/``, then
    ``$DOCLING_RS_MODELS_DIR/chunk/``, then this package's cache. With
    ``fetch=True`` a missing tokenizer is downloaded into the cache on its own
    (~0.5 MB, release asset then Hugging Face) — docling's ``HybridChunker``
    likewise pulls its tokenizer from the Hub on first use — instead of
    requiring the full :func:`download_models`. ``None`` when absent (and not
    fetched)."""
    candidates = [Path("." + _CHUNK_TOKENIZER)]
    if env := os.environ.get("DOCLING_RS_MODELS_DIR"):
        candidates.append(Path(env) / "chunk/tokenizer.json")
    cached = cache_dir() / _CHUNK_TOKENIZER
    candidates.append(cached)
    for p in candidates:
        if p.exists():
            return p
    if not fetch:
        return None
    if not _fetch(f"{BASE_URL}/chunk_tokenizer.json", cached, optional=True, progress=progress):
        _fetch(_FALLBACK_URLS[_CHUNK_TOKENIZER], cached, optional=False, progress=progress)
    return cached


def _asr_presets(asr_model: "str | Iterable[str] | None") -> "list[str]":
    """Normalize ``asr_model`` to a list of known preset names; an unknown one
    raises before anything is downloaded."""
    if asr_model is None:
        return []
    names = [asr_model] if isinstance(asr_model, str) else list(asr_model)
    unknown = [n for n in names if n not in ASR_MODELS]
    if unknown:
        raise ValueError(
            f"unknown asr_model {', '.join(map(repr, unknown))} (available: {', '.join(ASR_MODELS)})"
        )
    return list(dict.fromkeys(names))


def _fp32() -> bool:
    # Same truthiness vocabulary as Rust's docling_core::env::flag.
    return os.environ.get("DOCLING_RS_FP32", "").strip().lower() not in ("", "0", "false", "no", "off")


def _fetch_asr(root: Path, preset: str, progress: bool, force: bool) -> None:
    """Fetch one ASR preset into ``root/models/asr/…`` — the layout
    ``download_dependencies.sh --asr-model=<preset>`` writes under ``.models/``."""
    get = lambda url, rel, optional=False: _fetch(  # noqa: E731
        url, root / rel, optional=optional, progress=progress, force=force
    )
    if preset == "whisper_tiny":
        mirrored = "DOCLING_RS_ASR_MODELS_URL" not in os.environ
        for name, upstream in (
            ("encoder_model.onnx", "onnx/encoder_model.onnx"),
            ("decoder_model.onnx", "onnx/decoder_model.onnx"),
            ("vocab.json", "vocab.json"),
            ("added_tokens.json", "added_tokens.json"),
        ):
            rel = f"models/asr/{name}"
            optional = name == "added_tokens.json"
            if mirrored and get(f"{BASE_URL}/{_ASR_RELEASE_PREFIX}{name}", rel, optional=True):
                continue
            get(f"{_WHISPER_TINY_URL}/{upstream}", rel, optional=optional)
    elif preset in _WHISPER_PRESETS:
        base = f"https://huggingface.co/onnx-community/{_WHISPER_PRESETS[preset]}/resolve/main"
        d = f"models/asr/{preset}"
        get(f"{base}/onnx/encoder_model.onnx", f"{d}/encoder_model.onnx")
        get(f"{base}/onnx/decoder_model.onnx", f"{d}/decoder_model.onnx")
        get(f"{base}/vocab.json", f"{d}/vocab.json")
        # English-only exports keep their special tokens here; required for
        # the shifted token layout to resolve.
        get(f"{base}/added_tokens.json", f"{d}/added_tokens.json")
    else:  # Parakeet
        d = f"models/asr/{preset}"
        if _fp32():
            files = ["encoder-model.onnx", "encoder-model.onnx.data", "decoder_joint-model.onnx"]
        else:
            files = ["encoder-model.int8.onnx", "decoder_joint-model.int8.onnx"]
        for name in files + ["vocab.txt"]:
            get(f"{_PARAKEET_URL}/{name}", f"{d}/{name}")
        get(f"{_PARAKEET_URL}/config.json", f"{d}/config.json", optional=True)
        get(_VAD_URL, _VAD)


def _fetch_onnxruntime(root: Path, progress: bool, force: bool) -> bool:
    """Fetch and unpack the s390x ONNX Runtime library into the cache
    (``models/onnxruntime/libonnxruntime.so`` + its versioned name, LICENSE,
    VERSION). Optional: an older release tag without it leaves the ML stages
    unavailable on this host with the native pipeline's own message."""
    name, rel = _ORT_S390X
    dest = root / rel
    if (dest / "libonnxruntime.so").exists() and not force:
        return True
    tgz = dest.with_suffix(".tar.gz")
    if not _fetch(f"{BASE_URL}/{name}", tgz, optional=True, progress=progress, force=True):
        return False
    try:
        dest.mkdir(parents=True, exist_ok=True)
        with tarfile.open(tgz) as tar:
            # The tarball is flat (the names ONNX Runtime's build produces plus
            # LICENSE/VERSION); the filter keeps a crafted archive inside dest.
            tar.extractall(dest, filter="data") if hasattr(tarfile, "data_filter") else tar.extractall(dest)
    finally:
        tgz.unlink(missing_ok=True)
    return (dest / "libonnxruntime.so").exists()


def ensure_env(dest: "str | Path | None" = None) -> Path:
    """Point the native pipeline at the cached assets. The cache's models
    directory is handed over as ``DOCLING_RS_MODELS_DIR`` — the asset
    resolver's whole-directory fallback behind the working directory — and
    nothing is pinned per file, so the engine's own selection logic runs
    against the cache exactly as it does against a checkout's ``.models/``:
    the ``ocr_lang`` kwarg picks the en/ch recognition pair (#285), and the
    int8/fp32 choice follows the execution provider — fp32 under a GPU
    provider, int8 on CPU, ``DOCLING_RS_FP32=1`` forcing fp32 (#602: a
    ``DOCLING_LAYOUT_ONNX`` pin set here used to outrank that and loaded the
    CPU-calibrated int8 layout graph under CoreML/CUDA; it also disabled the
    engine's int8→fp32 escalation for a page the int8 graph fumbles).

    Local assets still win: the resolver tries the working directory's
    ``.models/…`` before the cache, per file. A variable the caller already
    set is never touched — the per-file ``DOCLING_*_ONNX`` vars remain the
    pin-any-model hatch. Safe to call when nothing is downloaded yet — a
    missing cache leaves the env untouched (and the converter fails with its
    usual clear "model not found" message)."""
    # Absolute paths in the env: a later os.chdir() must not orphan them.
    root = (Path(dest) if dest else cache_dir()).expanduser().resolve()
    m = root / "models"
    # No working-directory short-circuit: the resolver
    # already prefers a local file, and a checkout's partial .models/ must
    # still fall back to the cache for whatever it lacks.
    if "DOCLING_RS_MODELS_DIR" not in os.environ and m.exists():
        os.environ["DOCLING_RS_MODELS_DIR"] = str(m)
    return root
