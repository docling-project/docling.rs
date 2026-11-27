"""#1246: the OCR'd text-line cells of the PDF/image pipeline — text, bbox
(page points, top-left origin), recognition confidence, ``from_ocr`` — on
``ConversionResult.ocr_cells``, the ``from_ocr`` cells Python docling exposes
through ``generate_parsed_pages``. The first tests feed ``_wrap`` a native
result directly (no models); the last converts a real scan when the OCR
models are installed."""

import json
import math
import os
from pathlib import Path
from types import SimpleNamespace

import pytest

docling_rs = pytest.importorskip("docling_rs")

from docling_core.types.doc import CoordOrigin  # noqa: E402

REPO = Path(__file__).resolve().parents[3]
EMPTY_DOC = json.dumps(
    {
        "schema_name": "DoclingDocument",
        "version": "1.10.0",
        "name": "scan",
        "body": {"self_ref": "#/body", "children": [], "content_layer": "body", "label": "unspecified"},
    }
)


def _native(ocr_cells_json):
    return SimpleNamespace(
        status="success",
        input_name="scan.pdf",
        document_json=EMPTY_DOC,
        errors=[],
        redaction=None,
        metadata_json=None,
        confidence_json=None,
        pdf_repairs_json=None,
        ocr_status="available",
        ocr_unavailable_reason=None,
        ocr_cells_json=ocr_cells_json,
    )


def test_cells_are_keyed_by_page_with_top_left_boxes():
    cells = {
        "2": [
            {"text": "Invoice", "l": 10.0, "t": 20.0, "r": 60.5, "b": 32.0, "confidence": 0.75},
            {"text": "smudge", "l": 1.0, "t": 2.0, "r": 3.0, "b": 4.0, "confidence": None},
        ]
    }
    result = docling_rs._wrap(_native(json.dumps(cells)))
    assert list(result.ocr_cells) == [2]
    first, second = result.textline_cells(2)
    assert isinstance(first, docling_rs.TextLineCell)
    assert first.text == "Invoice" and first.from_ocr
    assert first.confidence == 0.75
    assert (first.bbox.l, first.bbox.t, first.bbox.r, first.bbox.b) == (10.0, 20.0, 60.5, 32.0)
    assert first.bbox.coord_origin == CoordOrigin.TOPLEFT
    # serde writes a NaN confidence as null.
    assert math.isnan(second.confidence)
    assert result.textline_cells(1) == []


def test_no_pipeline_means_no_cells():
    for value in (None, "{}"):
        result = docling_rs._wrap(_native(value))
        assert result.ocr_cells == {}
        assert result.textline_cells(1) == []


def _ocr_models_ready():
    models = Path(os.environ.get("DOCLING_RS_MODELS_DIR", REPO / ".models"))
    return any(models.glob("layout_heron*.onnx")) and any(models.glob("ocr_rec*.onnx"))


@pytest.mark.skipif(not _ocr_models_ready(), reason="OCR models not installed")
def test_scanned_page_reports_its_ocr_lines():
    scan = REPO / "tests/data/scanned/sources/old_newspaper.png"
    result = docling_rs.DocumentConverter(do_ocr=True).convert(scan)
    cells = result.textline_cells(1)
    assert cells, "an image page is read by OCR"
    assert all(c.from_ocr and 0.0 <= c.confidence <= 1.0 for c in cells)
    assert all(c.bbox.l <= c.bbox.r and c.bbox.t <= c.bbox.b for c in cells)
    # The page score is the mean of these confidences.
    page = result.confidence.pages[1]
    mean = sum(c.confidence for c in cells) / len(cells)
    assert page.ocr_score == pytest.approx(mean, abs=1e-3)
