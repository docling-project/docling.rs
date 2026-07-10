"""End-to-end tests for the docling.rs Python drop-in (declarative path only —
no ML models required). Build first with ``maturin develop`` in a venv that also
has ``docling-core`` installed, then run ``pytest``.

These assert the strangler-fig contract: the Rust engine is the processor, but
``result.document`` is a genuine ``docling_core`` ``DoclingDocument`` and every
downstream export is docling's own Python code.
"""

from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[3]
HTML = REPO / "tests/data/html/sources/hyperlink_03.html"


docling_rs = pytest.importorskip("docling_rs")


def test_document_is_real_docling_core_object():
    from docling_core.types.doc import DoclingDocument

    res = docling_rs.DocumentConverter().convert(HTML)
    assert isinstance(res.document, DoclingDocument)
    assert res.document.__class__.__module__.startswith("docling_core")


def test_status_is_str_enum():
    res = docling_rs.DocumentConverter().convert(HTML)
    assert res.status == "success"
    assert res.status == docling_rs.ConversionStatus.SUCCESS
    assert res.input.file.name == "hyperlink_03.html" or res.input.file.name == "hyperlink_03"


def test_exports_come_from_docling_core():
    res = docling_rs.DocumentConverter().convert(HTML)
    md = res.document.export_to_markdown()
    assert isinstance(md, str) and md.strip()

    d = res.document.export_to_dict()
    assert d["schema_name"] == "DoclingDocument"
    assert d["version"]  # docling-core wire-format version

    # docling-core exclusives that the Rust engine never implemented:
    assert res.document.export_to_doctags()


def test_convert_bytes_uses_name_for_format():
    data = HTML.read_bytes()
    res = docling_rs.DocumentConverter().convert_bytes("hyperlink_03.html", data)
    assert res.status == "success"
    assert res.document.texts


def test_path_and_str_sources_agree():
    conv = docling_rs.DocumentConverter()
    a = conv.convert(str(HTML)).document.export_to_markdown()
    b = conv.convert(HTML).document.export_to_markdown()
    assert a == b
