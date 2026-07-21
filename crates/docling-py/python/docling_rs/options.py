"""docling-shaped configuration objects.

These mirror the names and fields of docling's ``InputFormat`` /
``PdfPipelineOptions`` / ``PdfFormatOption`` / ``DocumentStream`` so existing
docling code reads unchanged, but they are plain, dependency-free dataclasses:
the Rust engine acts on the subset it supports (``do_ocr``,
``do_table_structure``, and ``accelerator_options.num_threads``), and the rest
are accepted for API compatibility. See the crate README for the support matrix.
"""

from __future__ import annotations

import enum
from dataclasses import dataclass, field
from typing import BinaryIO, Optional


class InputFormat(str, enum.Enum):
    """docling's ``InputFormat``. The first block matches docling's members
    one-for-one (same string values); the second lists formats docling.rs also
    handles. A ``str`` enum, so ``InputFormat.PDF == "pdf"``."""

    DOCX = "docx"
    PPTX = "pptx"
    HTML = "html"
    IMAGE = "image"
    PDF = "pdf"
    ASCIIDOC = "asciidoc"
    MD = "md"
    CSV = "csv"
    XLSX = "xlsx"
    # Legacy binary Office (docling converts via LibreOffice; docling.rs
    # parses natively):
    DOC = "doc"
    XLS = "xls"
    PPT = "ppt"
    XML_USPTO = "xml_uspto"
    XML_JATS = "xml_jats"
    JSON_DOCLING = "json_docling"
    AUDIO = "audio"
    # docling.rs also supports:
    ODT = "odt"
    ODS = "ods"
    ODP = "odp"
    EPUB = "epub"
    VTT = "vtt"
    EMAIL = "email"
    LATEX = "latex"
    MHTML = "mhtml"
    XML_XBRL = "xml_xbrl"
    XML_DOCLANG = "xml_doclang"
    METS_GBS = "mets_gbs"


class AcceleratorDevice(str, enum.Enum):
    """docling's ``AcceleratorDevice``, mapped to the engine's
    ``DOCLING_RS_EP``. ``AUTO`` (the default) leaves the engine's own default
    in place: GPU-when-usable with CPU fallback on the ``docling-rs-cuda``
    wheel, plain CPU on the CPU wheel. ``CUDA`` requires the GPU wheel and
    fails loudly when the GPU can't initialize; ``CPU`` forces CPU. ``MPS``
    has no ONNX Runtime provider here and warns."""

    AUTO = "auto"
    CPU = "cpu"
    CUDA = "cuda"
    MPS = "mps"


class TableFormerMode(str, enum.Enum):
    """docling's ``TableFormerMode`` (accepted; the Rust TableFormer runs a
    single accurate mode)."""

    FAST = "fast"
    ACCURATE = "accurate"


@dataclass
class AcceleratorOptions:
    """docling's ``AcceleratorOptions``. ``num_threads`` maps to the engine's
    ONNX Runtime intra-op thread count (via ``DOCLING_RS_PDF_THREADS``)."""

    num_threads: int = 4
    device: AcceleratorDevice = AcceleratorDevice.AUTO


@dataclass
class TableStructureOptions:
    """docling's ``TableStructureOptions`` (accepted for API compatibility)."""

    do_cell_matching: bool = True
    mode: TableFormerMode = TableFormerMode.ACCURATE


@dataclass
class PdfPipelineOptions:
    """docling's ``PdfPipelineOptions``.

    Acted on by the Rust engine: ``do_ocr``, ``do_table_structure``,
    ``do_picture_classification`` / ``do_code_enrichment`` /
    ``do_formula_enrichment`` (the opt-in enrichment models) and
    ``accelerator_options.num_threads``. The remaining fields are accepted so
    docling code constructs unchanged, but do not alter the pipeline (images are
    always extracted; the export image mode is chosen by docling-core at
    ``export_to_markdown(...)`` time)."""

    do_ocr: bool = True
    do_table_structure: bool = True
    do_picture_classification: bool = False
    do_code_enrichment: bool = False
    do_formula_enrichment: bool = False
    table_structure_options: TableStructureOptions = field(
        default_factory=TableStructureOptions
    )
    accelerator_options: AcceleratorOptions = field(default_factory=AcceleratorOptions)
    images_scale: float = 1.0
    generate_page_images: bool = False
    generate_picture_images: bool = False


@dataclass
class PdfFormatOption:
    """docling's ``PdfFormatOption``: carries ``pipeline_options`` for a format,
    as passed in ``DocumentConverter(format_options={InputFormat.PDF: ...})``."""

    pipeline_options: Optional[PdfPipelineOptions] = None


@dataclass
class DocumentStream:
    """docling's ``DocumentStream``: an in-memory source whose ``name`` (with
    extension) drives format detection. ``stream`` is any binary file-like
    object (e.g. ``io.BytesIO``)."""

    name: str
    stream: BinaryIO
