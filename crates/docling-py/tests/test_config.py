"""Tests for the docling-shaped configuration surface and re-exports
(declarative path only — no ML models required)."""

import io
import json
import math
import os
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[3]
HTML = REPO / "tests/data/html/sources/hyperlink_03.html"

docling_rs = pytest.importorskip("docling_rs")


def test_ipynb_outputs_forward_and_notebooks_convert():
    """#673: a notebook converts through the wrapper; ``ipynb_outputs``
    reaches the engine and is validated there."""
    from docling_rs import DocumentConverter, InputFormat

    nb = REPO / "crates/docling/tests/data/ipynb/sources/test_notebook.ipynb"
    assert InputFormat.IPYNB == "ipynb"
    quiet = DocumentConverter().convert(nb).document.export_to_markdown()
    assert quiet.startswith("# Test Notebook\n") and "## Code Cell Below" in quiet
    assert "\n42\n" not in quiet
    loud = DocumentConverter(ipynb_outputs="text").convert(nb).document.export_to_markdown()
    assert "```\n42\n```" in loud
    with pytest.raises(Exception, match="ipynb_outputs"):
        DocumentConverter(ipynb_outputs="all")


def test_metadata_reaches_the_result():
    """#679: ``metadata="json"`` puts the file's own metadata on the result
    (an EPUB's Dublin Core, a DOCX's core properties); the default reads
    none; the spelling is validated by the engine."""
    from docling_rs import DocumentConverter

    epub = REPO / "crates/docling-py/tests/data/markitdown/test.epub"
    assert DocumentConverter().convert(epub).metadata is None
    r = DocumentConverter(metadata="json").convert(epub)
    assert r.metadata["title"] == "Test EPUB Document"
    assert r.metadata["authors"] == ["Test Author"]
    assert r.metadata["language"] == "en"
    docx = REPO / "crates/docling-py/tests/data/markitdown/test.docx"
    m = DocumentConverter(metadata="frontmatter").convert(docx).metadata
    assert m["authors"] == ["Adam Fourney"] and m["creator_tool"] == "Microsoft Office Word"
    assert m["properties"]["pages"] == "2"
    with pytest.raises(Exception, match="metadata"):
        DocumentConverter(metadata="yaml")


def test_pdf_confidence_reaches_the_result():
    """PDF confidence is a typed docling-shaped report on the result."""
    from docling_rs import (
        ConfidenceReport,
        DocumentConverter,
        PageConfidenceScores,
        QualityGrade,
    )

    pdf = REPO / "tests/data/pdf/sources/normal_4pages.pdf"
    result = DocumentConverter(text_layer_only=True).convert(pdf, page_range=(1, 2))

    assert isinstance(result.confidence, ConfidenceReport)
    assert set(result.confidence.pages) == {1, 2}
    page = result.confidence.pages[1]
    assert isinstance(page, PageConfidenceScores)
    assert 0.0 <= page.parse_score <= 1.0
    assert 0.0 <= page.layout_score <= 1.0
    assert math.isnan(page.ocr_score)
    assert math.isnan(page.table_score)
    dumped = json.loads(result.confidence.model_dump_json())
    assert dumped["pages"]["1"]["table_score"] is None
    assert dumped["pages"]["1"]["mean_grade"] in {
        "poor", "fair", "good", "excellent", "unspecified"
    }

    empty = DocumentConverter().convert(HTML).confidence
    assert isinstance(empty, ConfidenceReport) and not empty.pages
    assert math.isnan(empty.mean_score)
    assert empty.mean_grade == QualityGrade.UNSPECIFIED


def test_warm_pdf_pipeline_confidence_reaches_the_result():
    """The warm pipeline's separate native result path keeps confidence."""
    from docling_rs import ConfidenceReport, DocumentConverter, InputFormat

    pdf = REPO / "tests/data/pdf/sources/normal_4pages.pdf"
    converter = DocumentConverter(text_layer_only=True)
    converter.initialize_pipeline(InputFormat.PDF)
    result = converter.convert(pdf, page_range=(2, 2))

    assert isinstance(result.confidence, ConfidenceReport)
    assert set(result.confidence.pages) == {2}


def test_detect_format_reads_the_bytes():
    """#672: ``detect_format`` names a format by its content — a path, bytes
    or a DocumentStream — and a nameless stream converts the same way."""
    import io

    from docling_rs import DocumentConverter, DocumentStream, InputFormat, detect_format

    pptx = REPO / "tests/data/pptx/sources/powerpoint_bad_text.pptx"
    assert detect_format(pptx) == InputFormat.PPTX
    assert detect_format(pptx.read_bytes()) == InputFormat.PPTX
    assert detect_format(b"%PDF-1.7\n") == InputFormat.PDF
    assert detect_format(b"<svg xmlns='http://www.w3.org/2000/svg'></svg>") == "svg"
    assert detect_format(b"just words") is None
    nameless = DocumentStream(name="deck", stream=io.BytesIO(pptx.read_bytes()))
    assert detect_format(nameless) == InputFormat.PPTX
    got = DocumentConverter().convert(nameless).document.export_to_markdown()
    want = DocumentConverter().convert(pptx).document.export_to_markdown()
    assert got == want and got.strip()


def test_legacy_charsets_are_guessed():
    """#675: a cp932 CSV with no hint converts with its own letters; the
    ``encoding`` kwarg still wins."""
    from docling_rs import DocumentConverter

    csv = REPO / "crates/docling/tests/data/csv/sources/test_mskanji.csv"
    md = DocumentConverter().convert(csv).document.export_to_markdown()
    assert "髙橋淳" in md and "名古屋" in md and "名前" in md
    forced = DocumentConverter(encoding="shift_jis").convert(csv).document.export_to_markdown()
    assert forced == md


def test_picture_alt_text_reaches_the_document():
    """#676: a picture's alt text is on the document as its description
    annotation (``provenance="alt_text"``) whatever the flag; the kwarg
    reaches the engine (the facade renders it in Markdown)."""
    from docling_rs import DocumentConverter

    pptx = REPO / "crates/docling-py/tests/data/markitdown/test.pptx"
    alt = "This phrase of the caption is Human-written."
    for flag in (False, True):
        doc = DocumentConverter(picture_alt_text=flag).convert(pptx).document.export_to_dict()
        annotations = [a for p in doc["pictures"] for a in p.get("annotations", [])]
        assert any(
            a.get("kind") == "description" and a.get("text") == alt and a.get("provenance") == "alt_text"
            for a in annotations
        ), annotations


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
        "ConfidenceReport",
        "PageConfidenceScores",
        "QualityGrade",
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
    from docling_rs import ConfidenceReport, DocumentConverter, QualityGrade

    missing = REPO / "tests/data/html/sources/__does_not_exist__.html"
    out = list(DocumentConverter().convert_all([HTML, missing], raises_on_error=False))
    assert len(out) == 2
    assert out[0].status == "success"
    assert out[1].status == "failure"
    assert isinstance(out[1].confidence, ConfidenceReport) and not out[1].confidence.pages
    assert math.isnan(out[1].confidence.mean_score)
    assert out[1].confidence.mean_grade == QualityGrade.UNSPECIFIED


def test_conversion_error_type():
    from docling_rs import DocumentConverter, ConversionError

    missing = REPO / "tests/data/html/sources/__nope__.html"
    with pytest.raises(ConversionError):
        DocumentConverter().convert(missing)


def test_accelerator_device_maps_to_ep_env(monkeypatch):
    # device=cuda/cpu maps to DOCLING_RS_EP (setdefault — an explicit env
    # override wins); AUTO leaves the engine default alone (auto on the GPU
    # wheel, CPU otherwise); MPS maps to CoreML where the build has it
    # (#602) and warns elsewhere.
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

    import docling_rs

    monkeypatch.delenv("DOCLING_RS_EP", raising=False)
    monkeypatch.setattr(docling_rs, "_compiled_providers", lambda: ["cpu"])
    with pytest.warns(UserWarning, match="mps"):
        convert_with(AcceleratorDevice.MPS)
    assert "DOCLING_RS_EP" not in os.environ

    # A CoreML build: MPS is how docling-shaped code opts into CoreML.
    monkeypatch.setattr(docling_rs, "_compiled_providers", lambda: ["cpu", "coreml"])
    convert_with(AcceleratorDevice.MPS)
    assert os.environ["DOCLING_RS_EP"] == "coreml"

    monkeypatch.setenv("DOCLING_RS_EP", "cpu")
    convert_with(AcceleratorDevice.MPS)  # explicit env wins over the option
    assert os.environ["DOCLING_RS_EP"] == "cpu"


def test_native_reports_compiled_providers():
    from docling_rs import _native

    providers = _native.compiled_providers()
    assert providers[0] == "cpu"
    assert set(providers) <= {"cpu", "cuda", "tensorrt", "directml", "coreml", "xnnpack"}


def test_initialize_pipeline_noop_for_non_ml_format():
    from docling_rs import DocumentConverter, InputFormat

    conv = DocumentConverter()
    # No models needed for a declarative format → clean no-op, and conversion
    # still works afterwards.
    conv.initialize_pipeline(InputFormat.MD)
    assert conv.convert(HTML).status == "success"


def test_no_text_panels_reaches_the_native_converter():
    """#197: the facade must accept ``no_text_panels`` — both as a direct
    kwarg and via docling-shaped ``PdfPipelineOptions`` — and still convert
    (declarative path; the flag only alters the ML pipeline, so constructing
    without a TypeError and forwarding to the native converter is the bug
    surface)."""
    from docling_rs import (
        DocumentConverter,
        InputFormat,
        PdfFormatOption,
        PdfPipelineOptions,
    )

    for converter in (
        DocumentConverter(no_text_panels=True),
        DocumentConverter(
            format_options={
                InputFormat.PDF: PdfFormatOption(
                    pipeline_options=PdfPipelineOptions(no_text_panels=True)
                )
            }
        ),
    ):
        result = converter.convert(HTML)
        assert "homepage" in result.document.export_to_markdown().lower()


def test_page_range_is_accepted_like_docling():
    """#518: docling takes ``page_range`` per call — ``convert(source,
    page_range=(a, b))`` — and the README documents it for this wrapper; the
    public converter used to reject it with a TypeError (only the native
    class had it). Constructor default, per-call override, ``convert_all``
    and ``convert_bytes`` all accept it (the window only narrows PDFs, so
    the declarative fixture converts unchanged), and a malformed window is
    a ``ValueError``, not a crash."""
    import sys

    from docling_rs import DocumentConverter, DocumentStream

    converter = DocumentConverter(page_range=(2, 3))
    assert "homepage" in converter.convert(HTML).document.export_to_markdown().lower()
    plain = DocumentConverter()
    # docling's own default window spelled out.
    for window in ((1, 2), [1, 1], (1, sys.maxsize)):
        md = plain.convert(HTML, page_range=window).document.export_to_markdown()
        assert "homepage" in md.lower()
    assert next(iter(plain.convert_all([HTML], page_range=(1, 1)))).status == "success"
    data = HTML.read_bytes()
    assert plain.convert_bytes(HTML.name, data, page_range=(1, 1)).status == "success"
    stream = DocumentStream(name=HTML.name, stream=io.BytesIO(data))
    assert plain.convert(stream, page_range=(1, 1)).status == "success"
    for bad in ((0, 2), (3, 1), (1,), "1-2"):
        with pytest.raises(ValueError):
            plain.convert(HTML, page_range=bad)
    with pytest.raises(ValueError):
        DocumentConverter(page_range=(0, 1))


def test_image_options_forward_and_validate():
    """#520: ``images_scale`` / ``generate_page_images`` reach the engine —
    directly and docling-shaped through ``PdfPipelineOptions`` — and an
    out-of-range scale raises at construction."""
    from docling_rs import (
        DocumentConverter,
        InputFormat,
        PdfFormatOption,
        PdfPipelineOptions,
    )

    for converter in (
        DocumentConverter(images_scale=1.5, generate_page_images=True),
        DocumentConverter(
            format_options={
                InputFormat.PDF: PdfFormatOption(
                    pipeline_options=PdfPipelineOptions(
                        images_scale=2.0,
                        generate_page_images=True,
                        generate_picture_images=True,
                    )
                )
            }
        ),
    ):
        assert converter.convert(HTML).status == "success"
    with pytest.raises(ValueError):
        DocumentConverter(images_scale=9.0)
    # docling's dataclass default (1.0) with no image generation requested
    # leaves the engine's crops alone rather than tripping validation.
    DocumentConverter(
        format_options={
            InputFormat.PDF: PdfFormatOption(pipeline_options=PdfPipelineOptions())
        }
    )


def test_sparse_sheet_kwargs_forward_to_engine():
    """#274: the wrapper forwards skip_empty_cells / compact_tables to the
    native converter (they were native-only in v1.14.0). skip_empty_cells is
    structural, so it must show up in the docling-core document the wrapper
    hands back: fewer table cells on a gappy sheet."""
    from docling_rs import DocumentConverter

    xlsx = REPO / "tests/data/xlsx/sources/xlsx_07_gap_tolerance_.xlsx"

    def cell_count(converter):
        doc = converter.convert(xlsx).document
        return sum(len(t.data.table_cells) for t in doc.tables)

    dense = cell_count(DocumentConverter())
    sparse = cell_count(
        DocumentConverter(skip_empty_cells=True, compact_tables=True)
    )
    assert sparse < dense


def test_picture_ocr_scale_and_textline_orientation_forward():
    """#666: picture_ocr_scale / ocr_textline_orientation reach the native
    converter (a bad scale is its validation error), directly and on
    PdfPipelineOptions; docling's RapidOcrOptions.use_cls maps onto the
    orientation switch."""
    from types import SimpleNamespace

    from docling_rs import DocumentConverter, InputFormat, PdfFormatOption
    from docling_rs.options import PdfPipelineOptions

    DocumentConverter(picture_ocr_scale=1.0, ocr_textline_orientation=True)
    with pytest.raises(Exception, match="picture_ocr_scale"):
        DocumentConverter(picture_ocr_scale=0.0)
    opts = PdfPipelineOptions(picture_ocr_scale=-2.0, ocr_textline_orientation=True)
    with pytest.raises(Exception, match="picture_ocr_scale"):
        DocumentConverter(
            format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)}
        )
    shaped = SimpleNamespace(
        do_ocr=True,
        do_table_structure=True,
        ocr_options=SimpleNamespace(use_cls=True, lang=[]),
    )
    DocumentConverter(
        format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=shaped)}
    )


def test_ocr_kwargs_forward_and_validate():
    """#274 follow-through for #254: ocr_mode / ocr_scale reach the native
    converter (which validates them), directly and docling-shaped via
    ocr_options.mode / .scale."""
    from types import SimpleNamespace

    from docling_rs import DocumentConverter, InputFormat, PdfFormatOption

    # Valid values construct; the native layer rejects bad ones — the error
    # surfacing at all is proof of forwarding.
    DocumentConverter(ocr_mode="full_page", ocr_scale=3.0)
    with pytest.raises(Exception):
        DocumentConverter(ocr_mode="not_a_mode")
    with pytest.raises(Exception):
        DocumentConverter(ocr_scale=-1.0)

    # docling-shaped: OcrOptions.mode may be an enum (collapses to .value).
    shaped = SimpleNamespace(
        do_ocr=True,
        do_table_structure=True,
        ocr_options=SimpleNamespace(
            mode=SimpleNamespace(value="layout_regions"), scale=2.5, lang=[]
        ),
    )
    DocumentConverter(
        format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=shaped)}
    )
    shaped.ocr_options.mode = "bogus"
    with pytest.raises(Exception):
        DocumentConverter(
            format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=shaped)}
        )


def test_ensure_env_leaves_ocr_lang_selectable(tmp_path, monkeypatch):
    """#285: ensure_env must NOT pin DOCLING_OCR_REC_ONNX/DOCLING_OCR_DICT —
    those per-file overrides beat the engine's en/ch pair selection and made
    the ocr_lang kwarg silently inert. The cache is handed over as
    DOCLING_RS_MODELS_DIR instead, and download_models() knows the English
    pair."""
    from docling_rs import models as m

    # A fake cache with the layout the downloader produces.
    cache = tmp_path / "cache"
    for rel in (
        "models/ocr_rec.onnx",
        "models/ppocr_keys_v1.txt",
        "models/ocr_rec_en.onnx",
        "models/en_dict.txt",
    ):
        p = cache / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(b"stub")

    # Run from a directory without a local .models/ (a checkout's own assets
    # outrank the cache by design and would short-circuit the test).
    monkeypatch.chdir(tmp_path)
    for var in ("DOCLING_RS_MODELS_DIR", "DOCLING_OCR_REC_ONNX", "DOCLING_OCR_DICT"):
        monkeypatch.delenv(var, raising=False)

    m.ensure_env(cache)

    import os

    assert os.environ.get("DOCLING_RS_MODELS_DIR") == str(cache / "models")
    assert "DOCLING_OCR_REC_ONNX" not in os.environ
    assert "DOCLING_OCR_DICT" not in os.environ

    # The download manifest carries the English pair (with upstream
    # fallbacks for release tags that don't host it).
    assert m._OPTIONAL["ocr_rec_en.onnx"] == "models/ocr_rec_en.onnx"
    assert m._OPTIONAL["ocr_det.onnx"] == "models/ocr_det.onnx"
    assert "models/ocr_det.onnx" in m._FALLBACK_URLS
    assert m._OPTIONAL["ocr_textline_ori.onnx"] == "models/ocr_textline_ori.onnx"
    assert "models/ocr_textline_ori.onnx" in m._FALLBACK_URLS
    assert m._OPTIONAL["ocr_rec_v6.onnx"] == "models/ocr_rec_v6.onnx"
    assert m._OPTIONAL["ocr_rec_v6_dict.txt"] == "models/ocr_rec_v6_dict.txt"
    assert "models/ocr_rec_v6.onnx" in m._FALLBACK_URLS
    assert "models/ocr_rec_v6_dict.txt" in m._FALLBACK_URLS
    assert m._OPTIONAL["en_dict.txt"] == "models/en_dict.txt"
    assert "models/ocr_rec_en.onnx" in m._FALLBACK_URLS
    assert "models/en_dict.txt" in m._FALLBACK_URLS


def test_download_assets_are_in_the_provenance_catalog():
    """#942: every release asset the Python downloader fetches has an entry in
    ``scripts/install/models_catalog.json`` — the provenance record
    ``scripts/ci/models_manifest.sh`` turns into the release's ``models.json``
    (upstream repository + revision, licence, graph/sidecar grouping) — and
    the attribution records it keeps beside the models are the catalog's."""
    from docling_rs import models as m

    catalog = json.loads((REPO / "scripts/install/models_catalog.json").read_text("utf-8"))
    by_asset = {f["asset"]: a for a in catalog["artifacts"] for f in a["files"]}
    fetched = {**m._REQUIRED, **m._OPTIONAL, **m._ENRICH, dict([m._ENRICH_FP32_DECODER]).popitem()[0]: ""}
    missing = sorted(set(fetched) - set(by_asset))
    assert not missing, f"assets without a catalog entry: {missing}"
    for asset, rel in {**m._REQUIRED, **m._OPTIONAL, **m._ENRICH}.items():
        # The catalog's install path is the CLI's `.models/…` spelling of the
        # cache's `models/…` one.
        entry = next(f for f in by_asset[asset]["files"] if f["asset"] == asset)
        assert entry["path"] == "." + rel, (asset, entry["path"], rel)
    for artifact in catalog["artifacts"]:
        assert artifact["license"]
        assert artifact["provenance"] in {"export", "quantization", "repack", "rehost", "build"}
        if not artifact.get("pattern"):
            assert artifact["upstream"]["revision"], artifact["id"]
        # A `.onnx.data` sidecar names its graph, and the graph it.
        for f in artifact["files"]:
            if f["role"] == "external-data":
                graphs = [g for g in artifact["files"] if g.get("data") == f["asset"]]
                assert graphs and any(g["asset"] == f["of"] for g in graphs), f
    records = set(m._RECORDS)
    assert catalog["notice"] in records
    assert set(catalog["licenses"].values()) <= records
    assert all(rel == f"models/{name}" for name, rel in m._RECORDS.items())


def test_ensure_env_still_respects_explicit_ocr_pins(tmp_path, monkeypatch):
    """The per-file vars remain the pin-any-model hatch (#285 workaround 2):
    ensure_env never overwrites caller-set values."""
    from docling_rs import models as m

    cache = tmp_path / "cache"
    (cache / "models").mkdir(parents=True)
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("DOCLING_OCR_REC_ONNX", "/custom/rec.onnx")
    monkeypatch.setenv("DOCLING_OCR_DICT", "/custom/dict.txt")
    monkeypatch.delenv("DOCLING_RS_MODELS_DIR", raising=False)

    m.ensure_env(cache)

    import os

    assert os.environ["DOCLING_OCR_REC_ONNX"] == "/custom/rec.onnx"
    assert os.environ["DOCLING_OCR_DICT"] == "/custom/dict.txt"


def test_ensure_env_pins_no_model_file(tmp_path, monkeypatch):
    """#602: ensure_env hands the cache over as DOCLING_RS_MODELS_DIR only.
    A per-file pin (DOCLING_LAYOUT_ONNX → layout_heron_int8.onnx) outranks
    the engine's provider-aware choice, so CoreML/CUDA ran the CPU-calibrated
    int8 graph. The directory is handed over even when the working directory
    has a .models/ of its own: the resolver tries that first, per file, and
    the cache fills in what it lacks."""
    from docling_rs import models as m

    cache = tmp_path / "cache"
    for rel in (
        "models/layout_heron.onnx",
        "models/layout_heron_int8.onnx",
        "models/tableformer/decoder_int8.onnx",
        "models/picture_classifier.onnx",
    ):
        p = cache / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(b"stub")
    work = tmp_path / "work"
    (work / ".models").mkdir(parents=True)
    monkeypatch.chdir(work)
    pins = (
        "DOCLING_LAYOUT_ONNX",
        "DOCLING_TABLEFORMER_ENCODER",
        "DOCLING_TABLEFORMER_DECODER",
        "DOCLING_TABLEFORMER_BBOX",
        "DOCLING_PICTURE_CLASSIFIER_ONNX",
        "DOCLING_CODE_FORMULA_DIR",
    )
    for var in ("DOCLING_RS_MODELS_DIR", *pins):
        monkeypatch.delenv(var, raising=False)

    m.ensure_env(cache)

    import os

    assert os.environ.get("DOCLING_RS_MODELS_DIR") == str(cache / "models")
    for var in pins:
        assert var not in os.environ, var

    # A caller's own directory is kept.
    monkeypatch.setenv("DOCLING_RS_MODELS_DIR", "/custom/models")
    m.ensure_env(cache)
    assert os.environ["DOCLING_RS_MODELS_DIR"] == "/custom/models"


# --- #508: ASR model downloads ----------------------------------------------


def _record_fetches(monkeypatch, m, hosted=lambda url: True):
    """Replace the network fetch with a recorder: ``hosted(url)`` decides
    whether a URL "exists"; fetched files are written as stubs."""
    calls = []

    def fake(url, dest, optional, progress, force=False):
        calls.append((url, dest))
        if not hosted(url):
            if optional:
                return False
            raise OSError(f"404 {url}")
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"stub")
        return True

    monkeypatch.setattr(m, "_fetch", fake)
    return calls


def test_download_models_fetches_asr_presets_into_the_engine_layout(tmp_path, monkeypatch):
    """``asr_model`` fetches the files the engine resolves under
    ``DOCLING_RS_MODELS_DIR`` (= ``<cache>/models``), mirroring
    ``download_dependencies.sh --asr-model=…``; ``pdf_models=False`` skips the
    PDF stack."""
    from docling_rs import models as m

    monkeypatch.delenv("DOCLING_RS_FP32", raising=False)
    calls = _record_fetches(monkeypatch, m)
    m.download_models(
        tmp_path,
        progress=False,
        asr_model=["parakeet_tdt_0.6b_v3", "whisper_tiny_en"],
        pdf_models=False,
    )
    got = sorted(str(dest.relative_to(tmp_path)) for _, dest in calls)
    assert got == sorted(
        [
            "models/asr/parakeet_tdt_0.6b_v3/encoder-model.int8.onnx",
            "models/asr/parakeet_tdt_0.6b_v3/decoder_joint-model.int8.onnx",
            "models/asr/parakeet_tdt_0.6b_v3/vocab.txt",
            "models/asr/parakeet_tdt_0.6b_v3/config.json",
            "models/asr/vad/silero_vad.onnx",
            "models/asr/whisper_tiny_en/encoder_model.onnx",
            "models/asr/whisper_tiny_en/decoder_model.onnx",
            "models/asr/whisper_tiny_en/vocab.json",
            "models/asr/whisper_tiny_en/added_tokens.json",
        ]
    )
    assert any("istupakov/parakeet-tdt-0.6b-v3-onnx" in url for url, _ in calls)

    # DOCLING_RS_FP32 fetches the full-precision graphs (encoder weights in a
    # .data sidecar) instead of the int8 ones.
    monkeypatch.setenv("DOCLING_RS_FP32", "1")
    calls = _record_fetches(monkeypatch, m)
    m.download_models(tmp_path / "fp32", progress=False, asr_model="parakeet_tdt_0.6b_v3", pdf_models=False)
    names = {dest.name for _, dest in calls}
    assert {"encoder-model.onnx", "encoder-model.onnx.data", "decoder_joint-model.onnx"} <= names
    assert not any("int8" in n for n in names)


def test_download_models_whisper_tiny_falls_back_to_hugging_face(tmp_path, monkeypatch):
    """The default Whisper tiny comes from the release mirror (``asr_*``) and
    falls back to Hugging Face when the release doesn't host it; it lands in
    ``models/asr/`` itself, where the engine's default looks."""
    from docling_rs import models as m

    monkeypatch.delenv("DOCLING_RS_ASR_MODELS_URL", raising=False)
    calls = _record_fetches(monkeypatch, m, hosted=lambda url: "huggingface.co" in url)
    m.download_models(tmp_path, progress=False, asr_model="whisper_tiny", pdf_models=False)
    for name in ("encoder_model.onnx", "decoder_model.onnx", "vocab.json"):
        assert (tmp_path / "models/asr" / name).exists()
        urls = [url for url, dest in calls if dest.name == name]
        assert urls[0].endswith(f"/asr_{name}") and "huggingface.co" in urls[-1]


def test_download_models_rejects_unknown_asr_presets_before_fetching(tmp_path, monkeypatch):
    from docling_rs import models as m

    calls = _record_fetches(monkeypatch, m)
    with pytest.raises(ValueError, match="parakeet_tdt_0.6b_v3"):
        m.download_models(tmp_path, progress=False, asr_model="parakeet_v2")
    assert calls == []
    assert "parakeet_tdt_0.6b_v3" in m.ASR_MODELS and "whisper_tiny" in m.ASR_MODELS


def test_asr_model_kwarg_reaches_the_engine(tmp_path):
    """An unknown ``asr_model`` is passed through and named by the engine's
    error on an audio input (the kwarg is not silently dropped)."""
    import wave

    from docling_rs import ConversionError, DocumentConverter

    wav = tmp_path / "silence.wav"
    with wave.open(str(wav), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(16000)
        w.writeframes(b"\0\0" * 1600)
    with pytest.raises((ConversionError, RuntimeError, ValueError), match="no_such_preset"):
        DocumentConverter(asr_model="no_such_preset").convert(wav)


# --- #304: remote VLM pipeline ----------------------------------------------

# 1x1 red PNG: the VLM image leg needs no models.
_PNG = bytes(
    [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D,
        0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00,
        0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
        0x00, 0x00, 0x03, 0x00, 0x01, 0x6E, 0x2C, 0xDC, 0x33, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
)


def test_vlm_requires_an_endpoint_at_construction(monkeypatch):
    from docling_rs import DocumentConverter

    monkeypatch.delenv("DOCLING_RS_VLM_ENDPOINT", raising=False)
    with pytest.raises(ValueError, match="vlm_endpoint"):
        DocumentConverter(pipeline="vlm")


def test_unknown_pipeline_is_a_value_error():
    from docling_rs import DocumentConverter

    with pytest.raises(ValueError, match="unknown pipeline"):
        DocumentConverter(pipeline="magic")


def test_vlm_max_tokens_zero_is_a_value_error():
    from docling_rs import DocumentConverter

    with pytest.raises(ValueError, match="vlm_max_tokens"):
        DocumentConverter(
            pipeline="vlm",
            vlm_endpoint="http://localhost:11434/v1",
            vlm_model="granite-docling",
            vlm_max_tokens=0,
        )


def test_standard_pipeline_ignores_stray_vlm_kwargs(tmp_path):
    """The Node bindings' contract: without pipeline="vlm" the vlm_* kwargs
    are ignored, not rejected — and conversions stay fully local."""
    from docling_rs import DocumentConverter

    conv = DocumentConverter(
        vlm_endpoint="http://localhost:1/v1", vlm_model="nope"
    )
    doc = tmp_path / "d.md"
    doc.write_text("# hi\n")
    result = conv.convert(doc)
    assert "hi" in result.document.export_to_markdown()


def test_vlm_converts_an_image_through_a_stub_endpoint(tmp_path):
    """End-to-end over a local OpenAI-compatible stub (the counterpart of
    crates/docling/tests/vlm.rs::mock_openai): a PNG page goes out, the
    stub's answer comes back as the document text."""
    import json
    import threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    from docling_rs import DocumentConverter

    seen = {}

    class Stub(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            seen["path"] = self.path
            seen["body"] = json.loads(body)
            payload = json.dumps(
                {"choices": [{"message": {"role": "assistant",
                                          "content": "Hello from the VLM"}}]}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *args):  # keep pytest output clean
            pass

    server = HTTPServer(("127.0.0.1", 0), Stub)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        conv = DocumentConverter(
            pipeline="vlm",
            vlm_endpoint=f"http://127.0.0.1:{server.server_port}/v1",
            vlm_model="mock-docling",
            vlm_api_key="sk-test",
            vlm_max_tokens=512,
        )
        png = tmp_path / "page.png"
        png.write_bytes(_PNG)
        result = conv.convert(png)
    finally:
        server.shutdown()
        thread.join()

    assert "Hello from the VLM" in result.document.export_to_markdown()
    assert seen["path"] == "/v1/chat/completions"
    assert seen["body"]["model"] == "mock-docling"
    assert seen["body"]["max_tokens"] == 512


def test_ocr_engine_forwards_and_maps_tesseract_options(monkeypatch):
    """#460: ocr_engine reaches the native converter (which validates it and
    reads ocr_lang against it), directly and docling-shaped via a
    TesseractCliOcrOptions-like ocr_options (kind, lang, tesseract_cmd, path,
    psm)."""
    from types import SimpleNamespace

    from docling_rs import DocumentConverter, InputFormat, PdfFormatOption

    DocumentConverter(ocr_engine="tesseract", ocr_lang="deu+fra")
    DocumentConverter(ocr_engine="ppocr", ocr_lang="en")
    with pytest.raises(Exception):
        DocumentConverter(ocr_engine="easyocr")
    # A tessdata stem is not a PP-OCR model, and an unknown tag is nothing.
    with pytest.raises(Exception):
        DocumentConverter(ocr_engine="ppocr", ocr_lang="deu")
    with pytest.raises(Exception):
        DocumentConverter(ocr_engine="tesseract", ocr_lang="xx")

    for var in ("DOCLING_TESSERACT", "DOCLING_RS_TESSDATA_DIR", "DOCLING_RS_TESSERACT_PSM"):
        monkeypatch.delenv(var, raising=False)
    shaped = SimpleNamespace(
        do_ocr=True,
        do_table_structure=True,
        ocr_options=SimpleNamespace(
            kind="tesseract",
            lang=["deu", "iso:fr"],
            tesseract_cmd="/opt/tesseract/bin/tesseract",
            path="/opt/tessdata",
            psm=6,
        ),
    )
    DocumentConverter(
        format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=shaped)}
    )
    assert os.environ["DOCLING_TESSERACT"] == "/opt/tesseract/bin/tesseract"
    assert os.environ["DOCLING_RS_TESSDATA_DIR"] == "/opt/tessdata"
    assert os.environ["DOCLING_RS_TESSERACT_PSM"] == "6"


def test_document_timeout_forwards_and_validates():
    """#497: document_timeout reaches the native converter (which validates
    it), directly and docling-shaped via pipeline_options.document_timeout;
    a declarative conversion is never cut by it."""
    from types import SimpleNamespace

    from docling_rs import ConversionStatus, DocumentConverter, InputFormat, PdfFormatOption

    DocumentConverter(document_timeout=90.0)
    with pytest.raises(Exception):
        DocumentConverter(document_timeout=0)
    with pytest.raises(Exception):
        DocumentConverter(document_timeout=-1.5)

    shaped = SimpleNamespace(do_ocr=True, do_table_structure=True, document_timeout=120)
    converter = DocumentConverter(
        format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=shaped)}
    )
    result = converter.convert(HTML)
    assert result.status == ConversionStatus.SUCCESS
    assert result.errors == []


def _caption_stub(answer="A duck on a pond."):
    """An OpenAI-compatible stub answering every chat completion with
    `answer`; returns (server, thread, seen-requests list)."""
    import json
    import threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    seen = []

    class Stub(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers["Content-Length"]))
            seen.append({"path": self.path, "auth": self.headers.get("Authorization"), "body": json.loads(body)})
            payload = json.dumps({"choices": [{"message": {"role": "assistant", "content": answer}}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Stub)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, seen


_PICTURE_DOCX = (
    __import__("pathlib").Path(__file__).resolve().parents[2] / "docling/tests/data/docx/sources/picture_ocr.docx"
)


def test_picture_description_kwargs_reach_the_engine():
    """#670: the engine's picture-description pass over a local stub — the
    caption lands in meta.description with the model as created_by, the
    request carries the mode's prompt and the bearer token."""
    from docling_rs import DocumentConverter

    server, thread, seen = _caption_stub()
    try:
        conv = DocumentConverter(
            do_picture_description=True,
            picture_description_endpoint=f"http://127.0.0.1:{server.server_port}/v1",
            picture_description_model="mock-vision",
            picture_description_api_key="sk-test",
            picture_description_mode="ocr",
            picture_ocr_min_side=0,
        )
        doc = conv.convert(_PICTURE_DOCX).document
    finally:
        server.shutdown()
        thread.join()
    described = [p for p in doc.pictures if p.meta and p.meta.description]
    assert len(described) == 2, [p.meta for p in doc.pictures]
    assert described[0].meta.description.text == "A duck on a pond."
    assert described[0].meta.description.created_by == "mock-vision"
    assert "A duck on a pond." in doc.export_to_markdown()
    assert len(seen) == 2
    assert seen[0]["path"] == "/v1/chat/completions"
    assert seen[0]["auth"] == "Bearer sk-test"
    assert seen[0]["body"]["model"] == "mock-vision"
    assert seen[0]["body"]["messages"][0]["content"][0]["text"].startswith("Extract all text from this image.")


def test_picture_description_api_options_map_onto_the_engine():
    """docling's PictureDescriptionApiOptions on the pipeline options is the
    engine pass, not the Python post-pass: url → endpoint, params["model"] →
    model, the Authorization header → key, prompt and concurrency theirs."""
    from docling_rs import DocumentConverter, InputFormat, PdfFormatOption, PdfPipelineOptions
    from docling_rs.datamodel.pipeline_options import PictureDescriptionApiOptions

    server, thread, seen = _caption_stub("Via the API options.")
    try:
        api = PictureDescriptionApiOptions(
            url=f"http://127.0.0.1:{server.server_port}/v1/chat/completions",
            headers={"Authorization": "Bearer sk-api"},
            params={"model": "gpt-4o-mini"},
            prompt="Describe the picture.",
            concurrency=2,
        )
        opts = PdfPipelineOptions(do_picture_description=True, picture_description_options=api)
        conv = DocumentConverter(format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)})
        assert conv._picture_description is None  # no Python post-pass
        doc = conv.convert(_PICTURE_DOCX).document
    finally:
        server.shutdown()
        thread.join()
    assert [p.meta.description.text for p in doc.pictures if p.meta and p.meta.description] == ["Via the API options."]
    assert seen[0]["auth"] == "Bearer sk-api"
    assert seen[0]["body"]["model"] == "gpt-4o-mini"
    assert seen[0]["body"]["messages"][0]["content"][0]["text"] == "Describe the picture."


def test_picture_description_rejects_a_bad_mode():
    from docling_rs import DocumentConverter

    with pytest.raises(ValueError, match="picture_description_mode"):
        DocumentConverter(do_picture_description=True, picture_description_mode="summary")


def test_redact_allow_patterns_and_terms_reach_the_engine():
    """#694: allow patterns/terms, deny terms and the NER floor are kwargs;
    an allowed shape survives the pass, the rest is redacted."""
    from docling_rs import DocumentConverter

    html = __import__("pathlib").Path(__file__).resolve().parents[2] / "docling/tests/data/html/sources/pii_sample.html"
    conv = DocumentConverter(
        redact_pii=True,
        redact_allow_patterns=[r"\+1 \(555\) \d{3}-\d{4}"],
        redact_allow_terms=["john.doe@example.com"],
        redact_deny_terms="Angela Merkel",
        redact_ner_min_score=0.5,
    )
    md = conv.convert(html).document.export_to_markdown()
    assert "+1 (555) 123-4567" in md
    assert "john.doe@example.com" in md
    assert "[REDACTED]" in md and "Angela Merkel" not in md
    assert "4111 1111 1111 1111" not in md
    with pytest.raises(ValueError, match="redact_allow_patterns"):
        DocumentConverter(redact_pii=True, redact_allow_patterns=["("])
