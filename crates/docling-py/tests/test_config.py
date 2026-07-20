"""Tests for the docling-shaped configuration surface and re-exports
(declarative path only — no ML models required)."""

import io
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[3]
HTML = REPO / "tests/data/html/sources/hyperlink_03.html"

docling_rs = pytest.importorskip("docling_rs")


def test_input_format_matches_docling_values():
    from docling_rs import InputFormat

    # docling's own members carry these exact string values.
    assert InputFormat.PDF == "pdf"
    assert InputFormat.DOCX == "docx"
    assert InputFormat.XML_JATS == "xml_jats"
    assert InputFormat.JSON_DOCLING == "json_docling"


def test_reexports_are_importable():
    import docling_rs as d

    for name in (
        "DocumentConverter",
        "ConversionResult",
        "ConversionStatus",
        "DoclingDocument",
        "ImageRefMode",
        "InputFormat",
        "DocumentStream",
        "PdfPipelineOptions",
        "PdfFormatOption",
        "AcceleratorOptions",
        "AcceleratorDevice",
        "TableFormerMode",
    ):
        assert hasattr(d, name), name


def test_pipeline_options_via_format_options_convert():
    from docling_rs import (
        DocumentConverter,
        InputFormat,
        PdfFormatOption,
        PdfPipelineOptions,
        AcceleratorOptions,
    )

    opts = PdfPipelineOptions(
        do_ocr=False,
        do_table_structure=False,
        accelerator_options=AcceleratorOptions(num_threads=2),
    )
    conv = DocumentConverter(
        format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)}
    )
    # Options are PDF-pipeline knobs; a declarative HTML convert still works.
    res = conv.convert(HTML)
    assert res.status == "success"
    assert res.document.export_to_markdown()


def test_shorthand_flags_convert():
    from docling_rs import DocumentConverter

    res = DocumentConverter(do_ocr=False, do_table_structure=True).convert(HTML)
    assert res.status == "success"


def test_document_stream_source():
    from docling_rs import DocumentConverter, DocumentStream

    stream = DocumentStream(name="hyperlink_03.html", stream=io.BytesIO(HTML.read_bytes()))
    res = DocumentConverter().convert(stream)
    assert res.status == "success"
    assert res.document.texts


def test_image_ref_mode_reexport_drives_export():
    from docling_rs import DocumentConverter, ImageRefMode

    doc = DocumentConverter().convert(HTML).document
    # docling-core's own export honours the re-exported enum.
    md = doc.export_to_markdown(image_mode=ImageRefMode.EMBEDDED)
    assert isinstance(md, str)


def test_allowed_formats_restricts_conversion():
    from docling_rs import DocumentConverter, InputFormat

    # HTML allowed → converts.
    ok = DocumentConverter(allowed_formats=[InputFormat.HTML]).convert(HTML)
    assert ok.status == "success"

    # HTML not in the allowed set → the engine refuses it.
    conv = DocumentConverter(allowed_formats=[InputFormat.PDF, InputFormat.DOCX])
    with pytest.raises(Exception):
        conv.convert(HTML)


def test_unknown_allowed_format_raises():
    from docling_rs import DocumentConverter

    with pytest.raises(Exception):
        DocumentConverter(allowed_formats=["not_a_format"])


def test_convert_all_yields_results():
    from docling_rs import DocumentConverter

    results = list(DocumentConverter().convert_all([HTML, HTML]))
    assert len(results) == 2
    assert all(r.status == "success" for r in results)


def test_convert_all_raises_on_error_false():
    from docling_rs import DocumentConverter

    missing = REPO / "tests/data/html/sources/__does_not_exist__.html"
    out = list(DocumentConverter().convert_all([HTML, missing], raises_on_error=False))
    assert len(out) == 2
    assert out[0].status == "success"
    assert out[1].status == "failure"


def test_conversion_error_type():
    from docling_rs import DocumentConverter, ConversionError

    missing = REPO / "tests/data/html/sources/__nope__.html"
    with pytest.raises(ConversionError):
        DocumentConverter().convert(missing)


def test_accelerator_device_maps_to_ep_env(monkeypatch):
    # device=cuda/cpu maps to DOCLING_RS_EP (setdefault — an explicit env
    # override wins); AUTO leaves the engine default alone (auto on the GPU
    # wheel, CPU otherwise); MPS has no provider here and warns.
    import os

    from docling_rs import (
        DocumentConverter,
        InputFormat,
        PdfFormatOption,
        PdfPipelineOptions,
        AcceleratorOptions,
        AcceleratorDevice,
    )

    def convert_with(device):
        opts = PdfPipelineOptions(accelerator_options=AcceleratorOptions(device=device))
        DocumentConverter(
            format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)}
        )

    monkeypatch.delenv("DOCLING_RS_EP", raising=False)
    convert_with(AcceleratorDevice.CUDA)
    assert os.environ["DOCLING_RS_EP"] == "cuda"

    monkeypatch.setenv("DOCLING_RS_EP", "cpu")
    convert_with(AcceleratorDevice.CUDA)  # explicit env wins over the option
    assert os.environ["DOCLING_RS_EP"] == "cpu"

    monkeypatch.delenv("DOCLING_RS_EP", raising=False)
    convert_with(AcceleratorDevice.CPU)
    assert os.environ["DOCLING_RS_EP"] == "cpu"

    monkeypatch.delenv("DOCLING_RS_EP", raising=False)
    convert_with(AcceleratorDevice.AUTO)
    assert "DOCLING_RS_EP" not in os.environ

    with pytest.warns(UserWarning, match="mps"):
        convert_with(AcceleratorDevice.MPS)


def test_initialize_pipeline_noop_for_non_ml_format():
    from docling_rs import DocumentConverter, InputFormat

    conv = DocumentConverter()
    # No models needed for a declarative format → clean no-op, and conversion
    # still works afterwards.
    conv.initialize_pipeline(InputFormat.MD)
    assert conv.convert(HTML).status == "success"
