"""``download_models()`` checks every release asset against the release's
SHA256SUMS (the manifest scripts/ci/models_manifest.sh writes), the same
contract scripts/install/download_dependencies.sh implements.

The module under test is stdlib-only, so it is loaded straight from its file:
these tests run without the native extension built (``maturin develop``).
"""

import hashlib
import importlib.util
import sys
from pathlib import Path

import pytest

_MODELS_PY = Path(__file__).resolve().parents[1] / "python" / "docling_rs" / "models.py"


@pytest.fixture
def models(monkeypatch, tmp_path):
    spec = importlib.util.spec_from_file_location("_docling_rs_models_under_test", _MODELS_PY)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    release = tmp_path / "release"
    release.mkdir()
    monkeypatch.setattr(mod, "BASE_URL", release.as_uri())
    monkeypatch.delenv("DOCLING_RS_MODELS_VERIFY", raising=False)
    mod._release = release  # the fake release directory, served over file://
    return mod


def _publish(release: Path, name: str, data: bytes, manifest=True, listed=True) -> str:
    (release / name).write_bytes(data)
    digest = hashlib.sha256(data).hexdigest()
    if manifest and listed:
        with open(release / "SHA256SUMS", "a") as f:
            f.write(f"{digest}  {name}\n")
    return digest


def test_matching_asset_lands(models, tmp_path):
    _publish(models._release, "layout_heron.onnx", b"weights")
    dest = tmp_path / "cache" / "models" / "layout_heron.onnx"
    assert models._fetch(f"{models.BASE_URL}/layout_heron.onnx", dest, optional=False, progress=False)
    assert dest.read_bytes() == b"weights"
    assert not dest.with_suffix(".onnx.download").exists()


def test_mismatch_raises_and_leaves_nothing(models, tmp_path):
    _publish(models._release, "libonnxruntime.tar.gz", b"genuine")
    (models._release / "libonnxruntime.tar.gz").write_bytes(b"tampered")
    dest = tmp_path / "cache" / "models" / "onnxruntime.tar.gz"
    with pytest.raises(models.IntegrityError, match="sha256 mismatch"):
        models._fetch(f"{models.BASE_URL}/libonnxruntime.tar.gz", dest, optional=False, progress=False)
    assert not dest.exists()
    assert not dest.with_suffix(".tar.gz.download").exists()


def test_mismatch_is_fatal_for_optional_assets_too(models, tmp_path):
    """An optional asset falls back to its upstream host when the release
    lacks it — never when the release's copy fails the check."""
    _publish(models._release, "ocr_det.onnx", b"genuine")
    (models._release / "ocr_det.onnx").write_bytes(b"tampered")
    dest = tmp_path / "cache" / "models" / "ocr_det.onnx"
    with pytest.raises(models.IntegrityError):
        models._fetch(f"{models.BASE_URL}/ocr_det.onnx", dest, optional=True, progress=False)
    assert not dest.exists()


def test_missing_manifest_warns_and_proceeds(models, tmp_path, capsys):
    _publish(models._release, "bbox.onnx", b"weights", manifest=False)
    dest = tmp_path / "cache" / "models" / "bbox.onnx"
    assert models._fetch(f"{models.BASE_URL}/bbox.onnx", dest, optional=False, progress=False)
    assert dest.read_bytes() == b"weights"
    assert "hosts no SHA256SUMS" in capsys.readouterr().err
    # Looked up once per process: a second fetch does not warn again.
    assert models._fetch(f"{models.BASE_URL}/bbox.onnx", tmp_path / "b2", optional=False, progress=False)
    assert "SHA256SUMS" not in capsys.readouterr().err


def test_missing_manifest_fails_under_require(models, tmp_path, monkeypatch):
    monkeypatch.setenv("DOCLING_RS_MODELS_VERIFY", "require")
    _publish(models._release, "bbox.onnx", b"weights", manifest=False)
    with pytest.raises(models.IntegrityError, match="require"):
        models._fetch(f"{models.BASE_URL}/bbox.onnx", tmp_path / "bbox.onnx", optional=False, progress=False)
    assert not (tmp_path / "bbox.onnx").exists()


def test_unlisted_asset_warns_or_fails(models, tmp_path, monkeypatch, capsys):
    _publish(models._release, "encoder.onnx", b"listed")
    _publish(models._release, "encoder_fp16.onnx", b"newer", listed=False)
    url = f"{models.BASE_URL}/encoder_fp16.onnx"
    assert models._fetch(url, tmp_path / "a", optional=True, progress=False)
    assert "encoder_fp16.onnx is not listed" in capsys.readouterr().err
    monkeypatch.setenv("DOCLING_RS_MODELS_VERIFY", "require")
    with pytest.raises(models.IntegrityError, match="not listed"):
        models._fetch(url, tmp_path / "b", optional=True, progress=False)


def test_upstream_hosts_are_outside_the_manifest(models, tmp_path):
    """A fallback URL (Hugging Face, RapidOCR's hub) is not a release asset:
    nothing to check it against, no warning either."""
    _publish(models._release, "encoder.onnx", b"listed")
    upstream = tmp_path / "upstream"
    upstream.mkdir()
    (upstream / "model.onnx").write_bytes(b"from the hub")
    dest = tmp_path / "cache" / "model.onnx"
    assert models._fetch(f"{upstream.as_uri()}/model.onnx", dest, optional=True, progress=False)
    assert dest.read_bytes() == b"from the hub"


def test_off_skips_the_check(models, tmp_path, monkeypatch):
    monkeypatch.setenv("DOCLING_RS_MODELS_VERIFY", "off")
    _publish(models._release, "decoder.onnx", b"genuine")
    (models._release / "decoder.onnx").write_bytes(b"tampered")
    dest = tmp_path / "decoder.onnx"
    assert models._fetch(f"{models.BASE_URL}/decoder.onnx", dest, optional=False, progress=False)
    assert dest.read_bytes() == b"tampered"


def test_bad_mode_is_rejected(models, tmp_path, monkeypatch):
    monkeypatch.setenv("DOCLING_RS_MODELS_VERIFY", "maybe")
    _publish(models._release, "decoder.onnx", b"x")
    with pytest.raises(ValueError, match="DOCLING_RS_MODELS_VERIFY"):
        models._fetch(f"{models.BASE_URL}/decoder.onnx", tmp_path / "d", optional=False, progress=False)


def test_manifest_parser_accepts_sha256sum_binary_marker(models):
    """`sha256sum -b` writes `<hex> *<name>`; the asterisk is not part of the name."""
    digest = "a" * 64
    (models._release / "SHA256SUMS").write_text(f"{digest} *bbox.onnx\n{'b' * 64}  encoder.onnx\n")
    assert models._manifest(models.BASE_URL) == {"bbox.onnx": digest, "encoder.onnx": "b" * 64}


def test_pinned_sums_catch_a_replaced_asset(models, monkeypatch, tmp_path):
    """#951: models-v1 is rolling — a replaced asset carries a matching
    SHA256SUMS of its own, so only a pinned copy of the old file notices."""
    old = hashlib.sha256(b"frozen").hexdigest()
    pin = tmp_path / "pinned.sums"
    pin.write_text(f"{old}  bbox.onnx\n")
    _publish(models._release, "bbox.onnx", b"replaced")  # release SHA256SUMS agrees with the new bytes
    monkeypatch.setenv("DOCLING_RS_MODELS_SUMS", str(pin))
    dest = tmp_path / "cache" / "bbox.onnx"
    with pytest.raises(models.IntegrityError, match="replaced since the pin"):
        models._fetch(f"{models.BASE_URL}/bbox.onnx", dest, optional=False, progress=False)
    assert not dest.exists()


def test_pinned_sums_accept_the_frozen_bytes(models, monkeypatch, tmp_path):
    pin = tmp_path / "pinned.sums"
    pin.write_text(f"{hashlib.sha256(b'frozen').hexdigest()}  bbox.onnx\n")
    (models._release / "bbox.onnx").write_bytes(b"frozen")  # e.g. served by a snapshot release
    monkeypatch.setenv("DOCLING_RS_MODELS_SUMS", str(pin))
    dest = tmp_path / "cache" / "bbox.onnx"
    assert models._fetch(f"{models.BASE_URL}/bbox.onnx", dest, optional=False, progress=False)
    assert dest.read_bytes() == b"frozen"


def test_unreadable_pin_is_fatal(models, monkeypatch, tmp_path):
    _publish(models._release, "bbox.onnx", b"x")
    monkeypatch.setenv("DOCLING_RS_MODELS_SUMS", str(tmp_path / "missing.sums"))
    with pytest.raises(models.IntegrityError, match="unreadable"):
        models._fetch(f"{models.BASE_URL}/bbox.onnx", tmp_path / "bbox.onnx", optional=False, progress=False)
