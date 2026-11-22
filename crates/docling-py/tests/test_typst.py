"""Tests for ``docling_rs.typst`` — the Rust Typst serializer (#1226)
exposed to Python. Declarative path only, no ML models."""

import json

import pytest

docling_rs = pytest.importorskip("docling_rs")

from docling_rs.typst import export_to_typst, save_as_typst  # noqa: E402

MD = b"# Guide\n\nInstall the **tools**.\n\n- clone\n- build\n\n| a | b |\n|---|---|\n| 1 | 2 |\n"


def _document():
    return docling_rs.DocumentConverter().convert_bytes("guide.md", MD).document


def test_export_is_typst_markup():
    out = export_to_typst(_document())
    assert out.startswith("= Guide\n\nInstall the #strong[tools].\n\n- clone\n- build\n\n#table(")
    assert "columns: 2" in out


def test_dict_and_string_inputs_match_the_document():
    doc = _document()
    expected = export_to_typst(doc)
    assert export_to_typst(doc.export_to_dict()) == expected
    assert export_to_typst(json.dumps(doc.export_to_dict())) == expected


def test_referenced_needs_save(tmp_path):
    with pytest.raises(ValueError):
        export_to_typst(_document(), image_mode="referenced")
    target = tmp_path / "guide.typ"
    save_as_typst(_document(), target)
    assert target.read_text(encoding="utf-8") == export_to_typst(_document())
