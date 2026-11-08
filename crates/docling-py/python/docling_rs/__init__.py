"""docling.rs — Rust docling port, Python bindings.

A strangler-fig drop-in for Python docling's common path::

    from docling_rs import DocumentConverter          # was: from docling.document_converter import ...

    result = DocumentConverter().convert("document.pdf")
    print(result.document.export_to_markdown())
    data = result.document.export_to_dict()            # docling JSON wire format

Only the *document processor* is Rust. The Rust engine parses the input and
returns docling-core's JSON wire format; this module loads that into the genuine
``docling_core.types.doc.DoclingDocument``, so every downstream capability —
``export_to_markdown()`` / ``export_to_dict()`` / ``export_to_doctags()``, the
serializers, and the chunkers — is docling's own Python code, unchanged.

Configuration follows docling's shape — ``PdfPipelineOptions`` / ``PdfFormatOption``
and per-call kwargs::

    from docling_rs import DocumentConverter, InputFormat, PdfFormatOption, PdfPipelineOptions

    opts = PdfPipelineOptions(do_ocr=False, do_table_structure=True)
    conv = DocumentConverter(format_options={InputFormat.PDF: PdfFormatOption(pipeline_options=opts)})

One-time model setup (mirrors docling's artifact download; ~700 MB into
``~/.cache/docling.rs``)::

    import docling_rs; docling_rs.download_models()

Declarative formats (DOCX/HTML/XLSX/…) need no models at all.
"""

from __future__ import annotations

import enum
import io
import json
import os
import warnings
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, Iterator, List, NamedTuple, Optional, Tuple, Union

from docling_core.types.doc import DoclingDocument, ImageRefMode

from . import models
from .models import IntegrityError, cache_dir, download_models, ensure_env
from .options import (
    AcceleratorDevice,
    AcceleratorOptions,
    DocumentStream,
    HeadingHierarchyOptions,
    InputFormat,
    PdfFormatOption,
    PdfPipelineOptions,
    TableFormerMode,
    TableStructureOptions,
)
from . import chunking
from . import progress

# GPU wheels (PyPI ``docling-rs-cuda``) bundle ONNX Runtime's CUDA provider
# libraries next to `_native` in this package directory; the native module is
# linked with an `$ORIGIN` rpath, so ONNX Runtime's dlopen-by-name finds them
# there with no Python-side setup — the same mechanism the native CLI uses.
# (Deliberately NO ctypes preload here: loading the provider libraries with
# RTLD_GLOBAL before the static ORT inside `_native` initializes duplicates
# ORT symbols process-wide and segfaulted at session creation in testing.)
from ._native import (
    ConversionError,
    EncryptionError,
    PasswordRequiredError,
    WrongPasswordError,
)
from ._native import __version__ as _native_version
from ._native import DocumentConverter as _NativeDocumentConverter
from ._native import compiled_providers as _compiled_providers
from ._native import email_attachments as _email_attachments
from ._native import detect_format as _detect_format


def _distribution_version() -> str:
    """The version of the installed distribution — what PyPI shows and
    ``pip`` installed. The release workflow writes it into
    ``pyproject.toml`` at publish time, while the native module's
    ``CARGO_PKG_VERSION`` is the crate's placeholder, so the metadata is the
    truth; the native constant is the fallback for a checkout without
    installed metadata (``docling-rs-cuda`` is the same package under
    another distribution name)."""
    try:
        from importlib.metadata import PackageNotFoundError, version
    except ImportError:  # pragma: no cover - Python < 3.8
        return _native_version
    for dist in ("docling-rs", "docling-rs-cuda"):
        try:
            return version(dist)
        except PackageNotFoundError:
            continue
    return _native_version


#: The installed package's version (``pip show docling-rs``).
__version__ = _distribution_version()

#: ``video_frames`` meaning "every distinct cut" (#647) — ``video_frames="all"``
#: spells the same.
VIDEO_FRAMES_ALL = 2**64 - 1

__all__ = [
    "DocumentConverter",
    "progress",
    "VIDEO_FRAMES_ALL",
    "ConversionResult",
    "ArchiveItem",
    "ConversionStatus",
    "ConfidenceReport",
    "PageConfidenceScores",
    "QualityGrade",
    "ErrorItem",
    "ConversionError",
    "EncryptionError",
    "PasswordRequiredError",
    "WrongPasswordError",
    "InputDocument",
    "DoclingDocument",
    "ImageRefMode",
    # email attachment payloads (.eml / .msg)
    "EmailAttachment",
    "email_attachments",
    "detect_format",
    # pluggable backends (#680)
    "register_backend",
    "registered_backends",
    "BackendSpec",
    # docling-shaped configuration
    "InputFormat",
    "DocumentStream",
    "PdfPipelineOptions",
    "PdfFormatOption",
    "TableStructureOptions",
    "TableFormerMode",
    "AcceleratorOptions",
    "AcceleratorDevice",
    # Rust-native chunkers (docling_rs.chunking.HierarchicalChunker / HybridChunker)
    "chunking",
    # model / env helpers
    "download_models",
    "IntegrityError",
    "ensure_env",
    "cache_dir",
    "models",
    "__version__",
]


class ConversionStatus(str, enum.Enum):
    """docling's ``ConversionStatus`` (a subset). A ``str`` enum, so both
    ``result.status == "success"`` and ``result.status == ConversionStatus.SUCCESS``
    hold — matching how docling callers branch on the result."""

    SUCCESS = "success"
    PARTIAL_SUCCESS = "partial_success"
    FAILURE = "failure"


@dataclass(frozen=True)
class ArchiveItem:
    """One entry of a ZIP archive converted with
    :meth:`DocumentConverter.convert_archive` (#557). ``outcome`` is
    ``"converted"`` (``result`` holds the :class:`ConversionResult`),
    ``"skipped"`` (``error`` says why: unsupported type, nested archive,
    unsafe path, over a ``DOCLING_RS_ZIP_MAX_*`` limit) or ``"failed"``
    (``error`` is the conversion error — the other entries are unaffected)."""

    path: str
    outcome: str
    result: Optional["ConversionResult"] = None
    error: Optional[str] = None


@dataclass(frozen=True)
class InputDocument:
    """docling's ``ConversionResult.input`` shim: the source's file name/path."""

    file: Path


@dataclass(frozen=True)
class ErrorItem:
    """docling's ``ErrorItem``: one recorded problem of a conversion that still
    produced a document (``component_type``, ``module_name``,
    ``error_message``, ``page_no``): a spent ``document_timeout`` (#497), or
    a VLM page that failed or came back cut off (#762, #763). ``page_no`` is
    the 1-based page the problem belongs to, ``None`` when it is
    document-scoped."""

    component_type: str
    module_name: str
    error_message: str
    page_no: Optional[int] = None


class ConversionResult:
    """docling's ``ConversionResult``: ``.document`` (a genuine
    :class:`~docling_core.types.doc.DoclingDocument`), ``.status``,
    ``.input`` and ``.errors`` (non-empty exactly when the status is
    ``PARTIAL_SUCCESS``). ``.redaction`` (a docling.rs extension, #621) is
    the PII pass's counts per label — ``{"EMAIL": 2, "PHONE": 1}`` — when
    the converter was built with ``redact_pii=True``, else ``None``.
    ``.metadata`` (#679) is the document's metadata as the JSON's
    ``metadata`` object — ``{"title": …, "authors": [...], "exif": {...}}``
    — when the converter was built with ``metadata="json"`` or
    ``"frontmatter"`` and the file carries any, else ``None``.
    ``.confidence`` is the PDF/image pipeline's :class:`ConfidenceReport`,
    keyed by the real 1-based page number; conversions without scores carry
    an empty report. ``.pdf_repairs`` (#934) is what the PDF reader repaired
    to read the input — ``[{"kind": "stream_length", "object": [4, 0],
    "offset": 202, "declared": 5, "recovered": 3}]``, ``[]`` for a
    well-formed file — when the converter was built with
    ``pdf_report_repairs=True`` and the input is a PDF, else ``None``."""

    def __init__(
        self,
        status: str,
        input_name: str,
        document: DoclingDocument,
        errors: Iterable[ErrorItem] = (),
        redaction: Optional[Dict[str, int]] = None,
        metadata: Optional[Dict[str, Any]] = None,
        confidence: Optional[ConfidenceReport] = None,
        pdf_repairs: Optional[List[Dict[str, Any]]] = None,
    ):
        self.status = ConversionStatus(status)
        self.document = document
        self.input = InputDocument(file=Path(input_name))
        self.errors = list(errors)
        self.redaction = dict(redaction) if redaction is not None else None
        self.metadata = dict(metadata) if metadata is not None else None
        self.confidence = confidence if confidence is not None else ConfidenceReport()
        self.pdf_repairs = list(pdf_repairs) if pdf_repairs is not None else None


# Imported here because datamodel re-exports ConversionResult from this module.
from .datamodel.base_models import (  # noqa: E402
    ConfidenceReport,
    PageConfidenceScores,
    QualityGrade,
)


def _is_api_options(pd: Any) -> bool:
    """docling's ``PictureDescriptionApiOptions`` (ours, or docling's own):
    anything with a ``url`` and no Python-side describer."""
    return pd is not None and hasattr(pd, "url") and not hasattr(pd, "_annotate_images")


def _progress_bridge(callback):
    """The native layer hands each progress event up as a flat ``dict``
    (#783); the user's callback gets the typed :mod:`docling_rs.progress`
    event."""
    if callback is None:
        return None

    def bridge(event):
        callback(progress.event_from_dict(event))

    return bridge


class DocumentConverter:
    """docling-shaped converter whose processor is Rust.

    Parameters mirror docling's converter and ``PdfPipelineOptions``:

    * ``format_options`` — ``{InputFormat.PDF: PdfFormatOption(pipeline_options=...)}``,
      as in docling. The PDF/image pipeline options ``do_ocr``,
      ``do_table_structure`` and ``accelerator_options.num_threads`` take effect.
    * ``do_ocr`` / ``do_table_structure`` — a shorthand for the same, used when no
      ``format_options`` is given.
    * ``no_text_panels`` — PDF/image: keep every detected picture as a picture
      (disable the demotion of uncaptioned dense-text "picture" regions into
      paragraphs — the image-extraction escape hatch, #174).
    * ``heading_hierarchy`` — PDF/image: infer section-header levels after
      assembly (docling's ``HeadingHierarchyModel``, #302 — bookmarks >
      numbering > font style). Also accepted docling-shaped, via
      ``pipeline_options.heading_hierarchy_options.enabled``.
    * ``fetch_images`` — resolve remote/local ``<img src>`` for HTML/EPUB
      (the alias of ``image_sources="remote"``; #646).
    * ``image_sources`` — which image references resolve (#646): ``"none"``
      (default — every picture a placeholder), ``"embedded"`` (``data:`` URIs
      and parts of the same container: EPUB/MHTML entries, an email's
      ``cid:`` attachments — no filesystem, no network), ``"local"`` (plus
      files under the source file's directory, never an absolute path or one
      that escapes it), ``"remote"`` (plus ``http(s)`` fetches). HTML, EPUB,
      MHTML, JATS, AsciiDoc, ODF, Markdown (``![…](data:…)`` and inline
      ``<img>``) and email bodies (``cid:`` images, in body order).
    * ``image_hosts`` — hosts a ``remote`` image fetch may reach (a list or a
      comma-separated string; exact names or ``*.suffix`` wildcards); redirects
      are held to the same list. Default: any host.
    * ``max_image_bytes`` / ``max_images`` / ``max_image_total_mb`` /
      ``min_image_bytes`` — the per-document image limits (#646): the largest
      image that resolves (32 MiB), how many resolve (unlimited), their total
      in MiB (unlimited), the smallest that resolves (0 — raise it to skip
      spacer / tracking pixels). Over a limit a picture stays a placeholder;
      the conversion never fails. Defaults come from ``DOCLING_RS_MAX_IMAGE_BYTES``,
      ``DOCLING_RS_MAX_IMAGES``, ``DOCLING_RS_MAX_IMAGE_TOTAL_MB``,
      ``DOCLING_RS_MIN_IMAGE_BYTES``.
    * ``use_web_browser`` — render HTML via headless Chrome before parsing.
    * ``ocr_mode`` / ``ocr_scale`` — docling 2.116's ``OcrMode`` (which regions
      feed the OCR; ``"full_page"``/``"layout_regions"`` discard the embedded
      text layer) and ``OcrOptions.scale`` (OCR input resolution in px per PDF
      point), #254. Also accepted docling-shaped, via
      ``pipeline_options.ocr_options.mode`` / ``.scale``.
    * ``pdf_word_gap`` — where a PDF's text layer splits words the file draws
      with a cursor move and no space glyph (#921): ``"adaptive"`` (default,
      docling-parse >= 7's line-local threshold; also
      ``DOCLING_RS_PDF_WORD_GAP``) | ``"legacy"`` (docling-parse <= 6) | a
      factor such as ``0.2`` of the two flanking glyphs' mean advance, in
      (0, 4].
    * ``pdf_keep_typography`` — keep a PDF's typographic punctuation as drawn
      (#921): ``„ “ ‚ ‘``, en/em dashes, ``…``. ``None`` folds it to ASCII
      like docling-parse's sanitizer unless ``DOCLING_RS_PDF_KEEP_TYPOGRAPHY``
      is set.
    * ``pdf_report_repairs`` — report what the PDF reader repaired to read the
      input (#934) on ``result.pdf_repairs``: streams whose ``/Length`` misses
      their ``endstream``, a padded xref. ``None`` = off unless
      ``DOCLING_RS_PDF_REPORT_REPAIRS`` is set. The conversion is unchanged.
    * ``ocr_textline_orientation`` — RapidOCR's text-line orientation
      handling (#666): detected lines cropped along their rotated rectangle,
      tall crops turned 90°, upside-down lines turned by the 0/180 classifier
      (``.models/ocr_textline_ori.onnx``). ``None`` = off unless
      ``DOCLING_RS_OCR_TEXTLINE_ORIENTATION`` is set; docling's
      ``RapidOcrOptions.use_cls`` (when set) maps onto it.
    * ``picture_ocr_scale`` — with ``do_picture_ocr``, the pictures' OCR
      resolution in px per picture pixel under RapidOCR's 2000 px cap (#666):
      ``1.0`` reads them at their native pixels; ``None`` keeps the
      image-input rule.
    * ``do_picture_description`` + ``picture_description_endpoint`` /
      ``_model`` / ``_api_key`` / ``_prompt`` / ``_mode`` (``"caption"`` |
      ``"ocr"``) / ``_concurrency`` (#670) — describe every picture of any
      format through an OpenAI-compatible vision endpoint; the answer is the
      picture's ``meta.description``. Endpoint, model and key fall back to
      the ``vlm_*`` kwargs, then ``DOCLING_RS_VLM_*``. docling's
      ``PictureDescriptionApiOptions`` on the pipeline options maps onto
      these.
    * ``ocr_engine`` — which OCR engine reads scanned pages (#460):
      ``"ppocr"`` (default, the built-in PP-OCRv3 recognizer) or
      ``"tesseract"`` (the system ``tesseract`` binary, docling's
      ``TesseractCliOcrOptions``). Under Tesseract ``ocr_lang`` is its
      language list — tessdata stems (``"deu+fra"``) or BCP-47 tags. Also
      accepted docling-shaped: a ``TesseractCliOcrOptions`` /
      ``TesseractOcrOptions`` as ``pipeline_options.ocr_options`` selects
      the engine, its ``lang`` list joins with ``+``, and its
      ``tesseract_cmd`` / ``path`` / ``psm`` seed ``DOCLING_TESSERACT`` /
      ``DOCLING_RS_TESSDATA_DIR`` / ``DOCLING_RS_TESSERACT_PSM`` (process-wide,
      an explicit environment override wins).
    * ``skip_empty_cells`` / ``compact_tables`` — sparse-spreadsheet output
      controls (docling.rs extensions, #271): omit empty cells from XLSX/XLS
      table rows; render Markdown tables unpadded (all formats). Note
      ``compact_tables`` shapes the *engine's* Markdown serializer only —
      this wrapper's ``document.export_to_markdown()`` runs upstream Python
      docling-core, whose padded table style is untouched;
      ``skip_empty_cells`` is structural and carries through everywhere.
    * ``picture_alt_text`` — render pictures' author-written alt text (the
      ``descr``/``title`` of a DOCX, PPTX or XLSX picture, #676) in the
      engine's Markdown and HTML; the JSON (and so ``result.document``)
      carries it always as the picture's ``meta.description`` with
      ``created_by="alt_text"``. Off by default; strict Markdown implies it.
      Likewise page breaks: ``document.export_to_markdown(page_break_placeholder=…)``
      is upstream docling-core's own option here and works off the items'
      ``prov.page_no`` in the engine's JSON (PDF and DjVu pages, slides, sheets); the
      engine-side ``--page-break-placeholder`` of the CLI / serve / Node is
      not a kwarg of this wrapper.
    * ``text_layer_only`` — skip the whole PDF ML stack and read the embedded
      text layer only (the docling.rs fast path behind the CLI's ``--text-layer-only``).
      ``"auto"`` decides per document: the text layer alone when it carries
      the document (no model touched), the full pipeline — OCR included —
      when it does not (a scanned / image-only file, or a vestigial layer
      of a few typed-in form fields over scans).
    * ``list_attachments`` — email (.eml/.msg): append an Attachments section
      with names and content types (#251; the payload is never embedded).
    * ``ebcdic_layout`` — EBCDIC (#252): the copybook layout, inline
      ``EbcdicLayout`` JSON or a file path (default: the ``<stem>.layout.json``
      sidecar next to the source).
    * ``video_frames`` — max frames sampled from a video (0 = transcript only;
      needs the ``ffmpeg`` binary); default 8. ``"all"`` (or
      :data:`VIDEO_FRAMES_ALL`) keeps every distinct cut (#647).
    * ``video_scene_threshold`` — ffmpeg's scene score (0–1) a frame must
      exceed to count as a cut (#647); default 0.27, 0.6 keeps hard cuts only.
    * ``video_frame_max_side`` — downscale each sampled frame inside ffmpeg
      so its longer side is at most this many px (#647); 0 / ``None`` = the
      source resolution.
    * ``video_frame_dedupe`` — drop a sampled frame whose 64-bit difference
      hash is within this Hamming distance (0–64) of a frame already kept
      (#647): the same slide after a fade becomes one picture. ``None`` =
      keep every frame; 4–6 is a good distance.
    * ``xbrl_taxonomy`` — XBRL: the directory the instance's taxonomy is read
      from (docling's ``XBRLBackendOptions.taxonomy``); default: the instance's
      own directory.
      These five reached the native class before but not this wrapper; every
      surface now shares one option set (#577, ``docs/OPTIONS.md``).
    * ``allowed_formats`` — restrict conversion to these :class:`InputFormat`\\ s
      (docling's converter arg); a source of any other format raises.
    * ``asr_model`` — the speech-recognition model for audio/video, by preset
      name (docling's ``asr_model_specs``): ``None`` / ``"whisper_tiny"``
      (default, multilingual Whisper tiny), ``"whisper_tiny_en"``,
      ``"whisper_base_en"``, ``"whisper_small_en"``,
      ``"whisper_distil_small_en"``, or ``"parakeet_tdt_0.6b_v3"`` (NVIDIA
      Parakeet TDT 0.6B v3: 25 European languages detected per utterance,
      ``asr_lang`` does not apply). Fetch the files once with
      ``download_models(asr_model=…)``; an unknown name, or a preset whose
      files are missing, fails the conversion with a message naming them.
    * ``asr_lang`` — transcription language for audio/video: a Whisper code
      (``"en"``, ``"de"``, …) or ``"auto"`` (default) to detect it from the
      first 30 seconds (docling 2.116 parity).
    * ``encoding`` — character encoding of text inputs (Markdown, CSV,
      AsciiDoc, WebVTT, XML, …): docling's ``TextBackendOptions.encoding``
      (``MarkdownBackendOptions(encoding="shift_jis")``), a WHATWG label or
      Python codec name. ``None`` (default) detects — BOM, UTF-8, a
      statistical guess for a non-Latin encoding (Shift_JIS, GBK, KOI8-R,
      …; #675), else windows-1252; bytes the requested encoding cannot
      decode raise.
    * ``ipynb_outputs`` — Jupyter notebooks (#673): ``"none"`` (default,
      the notebook as written — markdown cells, code cells as code items in
      the kernel's language, raw cells as code blocks) or ``"text"`` (stream
      and ``text/plain`` results as code blocks after their cell, image
      outputs as pictures).
    * ``track_changes`` — tracked changes in DOCX / DOC / ODT (#851, Pandoc's
      ``--track-changes``): ``"accept"`` (insertions shown, deletions left
      out), ``"reject"`` (the reverse) or ``"all"`` (both kept, typed with
      author and date in the Pandoc AST). ``None`` (default): each reader's
      own view. DOCX reads all three, DOC accept and reject, ODT accept; a
      mode a reader lacks raises.
    * ``extract_diagrams`` — DOCX DrawingML drawings (#858): ``"mermaid"``
      (the default) adds a ``mermaid`` code item after a drawing's picture
      with the flowchart its labelled shapes and connectors describe — only
      when every connector names its two end shapes; a drawing laid by eye
      gets no graph. ``"none"`` turns the extraction off.
    * ``use_outline_level_for_headings`` — Word (DOCX / DOC): a style's
      ``w:outlineLvl`` alone makes it a heading (docling's
      ``MsWordBackendOptions`` field, docling#4480; default ``True``).
      ``False`` for templates whose body styles carry an outline level only
      for the TOC; a style named a heading keeps its outline level.
    * ``metadata`` — document metadata (#679): the file's own (an EPUB's
      Dublin Core, Office properties, ODF ``meta.xml``, a photo's EXIF,
      audio tags, a PDF's Info dictionary). ``"none"`` (default) reads
      nothing; ``"json"`` adds a ``metadata`` object to the docling JSON and
      sets ``result.metadata`` (a dict); ``"frontmatter"`` adds YAML front
      matter at the top of the Markdown as well. ``redact_pii`` masks the
      author fields and drops the GPS position.
    * ``pipeline`` — ``"standard"`` (default) or ``"vlm"`` (#304): convert
      PDF / image inputs by sending each page to a remote OpenAI-compatible
      vision model instead of the local ML stack (no models needed). The
      ``vlm_*`` kwargs mirror the Node bindings' options and fall back to the
      ``DOCLING_RS_VLM_*`` environment: ``vlm_endpoint`` and ``vlm_model``
      are required (a missing one raises ``ValueError`` here, at
      construction); ``vlm_api_key`` (Bearer token), ``vlm_prompt`` and
      ``vlm_max_tokens`` (default 8192) are optional. With
      ``pipeline="standard"`` the ``vlm_*`` kwargs are ignored, not rejected.
    * ``document_timeout`` — docling's ``PipelineOptions.document_timeout``
      (#497): a per-document budget in seconds for the PDF pipeline, checked
      between pages. Once spent, the pages done so far are the document and
      the result is a ``PARTIAL_SUCCESS`` whose ``.errors`` says why. ``None``
      (default) is unlimited. Also accepted docling-shaped, via
      ``pipeline_options.document_timeout``.
    * ``progress_callback`` — docling's (#783, docling 2.139): a callable
      receiving the progress events of every conversion — document start
      and end, one per PDF / image page with the completed count, the
      picture passes' item counts — as the :mod:`docling_rs.progress`
      classes, from the converting threads. ``show_progress=True`` prints
      progress bars to stderr instead (or as well); ``convert`` /
      ``convert_all`` also take a per-call ``progress_callback``.
    * ``password`` — the password of an encrypted PDF (#611; docling's
      ``--pdf-password``) or Office document — ``.docx``/``.xlsx``/
      ``.pptx``/``.doc``/``.xls``/``.ppt`` (#625, beyond docling, which
      reads none). ``pdf_password``, its earlier name, still works. Also
      accepted docling-shaped, as the PDF ``PdfFormatOption``'s
      ``backend_options.password`` (a plain string or a pydantic
      ``SecretStr``).
    * ``page_range`` — ``(first, last)``, a 1-based inclusive PDF page window
      for every conversion (#518). docling takes it per call —
      ``convert(source, page_range=(a, b))`` — which works here too and wins
      over this default.
    * ``images_scale`` / ``generate_page_images`` — docling's
      ``PdfPipelineOptions`` fields of the same names (#520): picture crops
      (and page images) at ``images_scale`` px per PDF point, and each page's
      render kept as ``document.pages[n].image`` so docling-core's
      ``TableItem.get_image`` / ``FormulaItem.get_image`` work. Also accepted
      docling-shaped via ``pipeline_options``, where ``images_scale`` applies
      once ``generate_picture_images`` or ``generate_page_images`` is set
      (docling only renders images then; without either, crops keep the
      engine's 2.0 px/pt render). Every picture's ``image.dpi`` is
      72·scale (#519).
    * ``use_embedded_picture_images`` — docling's option of that name
      (#1007, docling#5143; also read off ``PdfPipelineOptions``): a PDF
      picture whose region holds exactly one embedded image with nothing
      drawn over it gets that bitmap at native resolution (``dpi`` 72)
      instead of the render crop.
    * ``artifacts_path`` — override the model cache dir (docling's
      ``artifacts_path``); defaults to ``~/.cache/docling.rs``.
    """

    def __init__(
        self,
        format_options: Optional[Dict[InputFormat, PdfFormatOption]] = None,
        *,
        allowed_formats: Optional[Iterable[InputFormat]] = None,
        do_ocr: bool = True,
        do_table_structure: bool = True,
        force_full_page_ocr: bool = False,
        replace_broken_text_layer: bool = False,
        no_text_panels: bool = False,
        heading_hierarchy: bool = False,
        do_picture_classification: bool = False,
        do_code_enrichment: bool = False,
        do_formula_enrichment: bool = False,
        do_picture_ocr: bool = False,
        picture_ocr_classes: Optional[Union[str, Iterable[str]]] = None,
        picture_ocr_min_side: Optional[int] = None,
        picture_ocr_scale: Optional[float] = None,
        keep_picture_images: bool = True,
        do_picture_description: bool = False,
        picture_description_endpoint: Optional[str] = None,
        picture_description_model: Optional[str] = None,
        picture_description_api_key: Optional[str] = None,
        picture_description_prompt: Optional[str] = None,
        picture_description_mode: Optional[str] = None,
        picture_description_concurrency: Optional[int] = None,
        redact_pii: bool = False,
        redact_mode: Optional[str] = None,
        redact_kinds: Optional[Union[str, Iterable[str]]] = None,
        redact_pattern: Optional[Union[str, Iterable[str]]] = None,
        redact_images: Optional[str] = None,
        redact_allow_terms: Optional[Iterable[str]] = None,
        redact_deny_terms: Optional[Iterable[str]] = None,
        redact_allow_patterns: Optional[Iterable[str]] = None,
        redact_ner_min_score: Optional[float] = None,
        fetch_images: bool = False,
        image_sources: Optional[str] = None,
        image_hosts: Optional[Union[str, Iterable[str]]] = None,
        max_image_bytes: Optional[int] = None,
        max_images: Optional[int] = None,
        max_image_total_mb: Optional[int] = None,
        min_image_bytes: Optional[int] = None,
        use_web_browser: bool = False,
        ocr_lang: Optional[str] = None,
        ocr_mode: Optional[str] = None,
        ocr_scale: Optional[float] = None,
        pdf_word_gap: Optional[Union[str, float]] = None,
        pdf_keep_typography: Optional[bool] = None,
        pdf_report_repairs: Optional[bool] = None,
        ocr_textline_orientation: Optional[bool] = None,
        ocr_engine: Optional[str] = None,
        skip_empty_cells: bool = False,
        compact_tables: bool = False,
        picture_alt_text: bool = False,
        asr_model: Optional[str] = None,
        asr_lang: Optional[str] = None,
        encoding: Optional[str] = None,
        ipynb_outputs: Optional[str] = None,
        metadata: Optional[str] = None,
        track_changes: Optional[str] = None,
        extract_diagrams: Optional[str] = None,
        use_outline_level_for_headings: bool = True,
        pipeline: Optional[str] = None,
        vlm_endpoint: Optional[str] = None,
        vlm_model: Optional[str] = None,
        vlm_api_key: Optional[str] = None,
        vlm_prompt: Optional[str] = None,
        vlm_max_tokens: Optional[int] = None,
        document_timeout: Optional[float] = None,
        password: Optional[str] = None,
        pdf_password: Optional[str] = None,
        progress_callback: Optional["progress.ProgressCallback"] = None,
        show_progress: bool = False,
        page_range: Optional[Tuple[int, int]] = None,
        images_scale: Optional[float] = None,
        generate_page_images: bool = False,
        use_embedded_picture_images: bool = False,
        text_layer_only: Union[bool, str] = False,
        list_attachments: bool = False,
        ebcdic_layout: Optional[str] = None,
        video_frames: Optional[Union[int, str]] = None,
        video_scene_threshold: Optional[float] = None,
        video_frame_max_side: Optional[int] = None,
        video_frame_dedupe: Optional[int] = None,
        xbrl_taxonomy: Optional[Union[str, "os.PathLike[str]"]] = None,
        artifacts_path=None,
    ):
        ensure_env(artifacts_path)

        if isinstance(video_frames, str):
            if video_frames.strip().lower() != "all":
                raise ValueError(
                    "video_frames: expected a non-negative int or 'all', "
                    f"got {video_frames!r}"
                )
            video_frames = VIDEO_FRAMES_ALL

        if password is None:
            password = pdf_password
        if password is None:
            password = _pdf_backend_password(format_options)
        # A PDF/IMAGE PdfFormatOption overrides the shorthand kwargs.
        pdf_opts = _pdf_pipeline_options(format_options)
        if pdf_opts is not None:
            do_ocr = pdf_opts.do_ocr
            do_table_structure = pdf_opts.do_table_structure
            # docling proper carries the flag on ocr_options; accept both the
            # direct field and docling-shaped ocr_options.force_full_page_ocr.
            force_full_page_ocr = getattr(
                pdf_opts, "force_full_page_ocr", force_full_page_ocr
            ) or getattr(
                getattr(pdf_opts, "ocr_options", None), "force_full_page_ocr", False
            )
            # docling's OcrOptions.replace_broken_text_layer (#1006); the
            # direct field is accepted too.
            replace_broken_text_layer = getattr(
                pdf_opts, "replace_broken_text_layer", replace_broken_text_layer
            ) or getattr(
                getattr(pdf_opts, "ocr_options", None),
                "replace_broken_text_layer",
                False,
            )
            no_text_panels = getattr(pdf_opts, "no_text_panels", no_text_panels)
            hh = getattr(pdf_opts, "heading_hierarchy_options", None)
            if hh is not None:
                heading_hierarchy = bool(getattr(hh, "enabled", heading_hierarchy))
            do_picture_classification = getattr(
                pdf_opts, "do_picture_classification", do_picture_classification
            )
            do_code_enrichment = getattr(
                pdf_opts, "do_code_enrichment", do_code_enrichment
            )
            do_formula_enrichment = getattr(
                pdf_opts, "do_formula_enrichment", do_formula_enrichment
            )
            # The picture-OCR enrichment (#645) and its filters, when set on
            # the pipeline options (a docling.rs extension of
            # PdfPipelineOptions).
            do_picture_ocr = getattr(pdf_opts, "do_picture_ocr", do_picture_ocr)
            if getattr(pdf_opts, "picture_ocr_classes", None) is not None:
                picture_ocr_classes = pdf_opts.picture_ocr_classes
            if getattr(pdf_opts, "picture_ocr_min_side", None) is not None:
                picture_ocr_min_side = pdf_opts.picture_ocr_min_side
            if getattr(pdf_opts, "picture_ocr_scale", None) is not None:
                picture_ocr_scale = float(pdf_opts.picture_ocr_scale)
            # RapidOCR's line orientation (#666), a docling.rs extension of
            # PdfPipelineOptions.
            if getattr(pdf_opts, "ocr_textline_orientation", None) is not None:
                ocr_textline_orientation = bool(pdf_opts.ocr_textline_orientation)
            keep_picture_images = getattr(
                pdf_opts, "keep_picture_images", keep_picture_images
            )
            # docling's picture description through an API
            # (PictureDescriptionApiOptions, #670) runs in the engine: url →
            # endpoint, params["model"] → model, an Authorization header →
            # key, prompt and concurrency theirs. Other describers (a
            # LangChain chat model) stay the Python post-pass below.
            pd = getattr(pdf_opts, "picture_description_options", None)
            if getattr(pdf_opts, "do_picture_description", False) and _is_api_options(pd):
                do_picture_description = True
                picture_description_endpoint = str(pd.url) or picture_description_endpoint
                params = getattr(pd, "params", None) or {}
                if params.get("model"):
                    picture_description_model = str(params["model"])
                headers = getattr(pd, "headers", None) or {}
                auth = next((v for k, v in headers.items() if k.lower() == "authorization"), None)
                if isinstance(auth, str) and auth.lower().startswith("bearer "):
                    picture_description_api_key = auth[7:].strip()
                if getattr(pd, "prompt", None):
                    picture_description_prompt = str(pd.prompt)
                if getattr(pd, "concurrency", None):
                    picture_description_concurrency = int(pd.concurrency)
            # docling's PipelineOptions.document_timeout (#497), when set on
            # the pipeline options, wins over the shorthand kwarg.
            dt = getattr(pdf_opts, "document_timeout", None)
            if dt is not None:
                document_timeout = float(dt)
            # docling's image outputs (#520): page images on request, and
            # images_scale whenever docling would render images at all.
            generate_page_images = bool(
                getattr(pdf_opts, "generate_page_images", generate_page_images)
            )
            if generate_page_images or getattr(pdf_opts, "generate_picture_images", False):
                scale = getattr(pdf_opts, "images_scale", None)
                if scale is not None:
                    images_scale = float(scale)
            # docling's use_embedded_picture_images (#1007, docling#5143).
            use_embedded_picture_images = bool(
                getattr(
                    pdf_opts, "use_embedded_picture_images", use_embedded_picture_images
                )
            )
            # Map docling's ocr_options.lang (a list of language ids) onto the
            # engine's en/ch recognition-model switch. First entry wins;
            # anything that isn't recognisably English/Chinese is ignored with
            # a warning (the engine default — English — applies).
            ocr_opts = getattr(pdf_opts, "ocr_options", None)
            # docling 2.116's OcrMode / OcrOptions.scale (#254). An enum mode
            # collapses to its string value; the direct kwargs stay the
            # fallback, matching the pipeline-overrides-shorthand rule above.
            mode = getattr(ocr_opts, "mode", None)
            if mode is not None:
                ocr_mode = getattr(mode, "value", mode)
            scale = getattr(ocr_opts, "scale", None)
            if scale is not None:
                ocr_scale = float(scale)
            # docling's RapidOcrOptions.use_cls (RapidOCR's text-line
            # orientation classifier, #666) when set explicitly.
            use_cls = getattr(ocr_opts, "use_cls", None)
            if use_cls is not None and ocr_textline_orientation is None:
                ocr_textline_orientation = bool(use_cls)
            # docling's Tesseract kinds (#460) select the engine; the CLI
            # options' binary, tessdata path and page segmentation mode are
            # process-wide knobs here, seeded like the accelerator device.
            kind = getattr(ocr_opts, "kind", None)
            if kind in ("tesseract", "tesserocr"):
                ocr_engine = "tesseract"
                cmd = getattr(ocr_opts, "tesseract_cmd", None)
                if cmd:
                    os.environ.setdefault("DOCLING_TESSERACT", str(cmd))
                tessdata = getattr(ocr_opts, "path", None)
                if tessdata:
                    os.environ.setdefault("DOCLING_RS_TESSDATA_DIR", str(tessdata))
                psm = getattr(ocr_opts, "psm", None)
                if psm is not None:
                    os.environ.setdefault("DOCLING_RS_TESSERACT_PSM", str(int(psm)))
            langs = list(getattr(ocr_opts, "lang", None) or [])
            if langs and ocr_engine == "tesseract":
                # Tesseract's own vocabulary: stems and `iso:` tags pass
                # through, the engine maps and validates them (#460).
                ocr_lang = "+".join(str(lang).strip() for lang in langs)
            elif langs:
                # The same spellings the engine's own `ocr_lang` accepts (#388):
                # engine codes, docling's legacy names, EasyOCR's, and BCP-47
                # tags for English / Chinese with or without docling's `iso:`
                # prefix; script and region subtags are ignored. Passed through
                # as written — the engine canonicalizes and would reject
                # anything else, so the unrecognized case warns here instead.
                head = str(langs[0]).strip().lower()
                if head.startswith("iso:"):
                    head = head[4:].strip()
                # `ch_sim`, `ch_tra`, `chinese_cht`, `en_GB` all reduce to
                # their first subtag.
                primary = head.replace("_", "-").split("-", 1)[0]
                if primary in ("en", "eng", "english"):
                    ocr_lang = "en"
                elif primary in ("ch", "zh", "zho", "chi", "cmn", "chinese"):
                    ocr_lang = "ch"
                else:
                    warnings.warn(
                        f"docling.rs OCR ships English and Chinese recognition "
                        f"models only (en | ch, or a BCP-47 tag for either); "
                        f"ocr_options.lang={langs!r} is ignored",
                        stacklevel=2,
                    )
            acc = getattr(pdf_opts, "accelerator_options", None)
            if acc is not None:
                # Map docling's device to the engine's DOCLING_RS_EP (resolved
                # once per process, so this must run before the first
                # conversion; an explicit environment override always wins).
                # AUTO maps to nothing: the engine's own default already is
                # "auto" in a GPU build (docling-rs-cuda) and CPU otherwise.
                if acc.device == AcceleratorDevice.CUDA:
                    os.environ.setdefault("DOCLING_RS_EP", "cuda")
                elif acc.device == AcceleratorDevice.CPU:
                    os.environ.setdefault("DOCLING_RS_EP", "cpu")
                elif acc.device == AcceleratorDevice.MPS:
                    # docling's Apple-GPU device → the CoreML provider, which
                    # the engine keeps opt-in (#602): this is how
                    # docling-shaped code asks for it. Only where the build
                    # has it — the PyPI macOS wheels are CPU-only.
                    if "coreml" in _compiled_providers():
                        os.environ.setdefault("DOCLING_RS_EP", "coreml")
                    else:
                        warnings.warn(
                            "docling.rs has no MPS execution provider and this "
                            "build has no CoreML; device 'mps' is ignored (build "
                            "the wheel on macOS with `maturin build --features "
                            "coreml` for CoreML).",
                            stacklevel=2,
                        )
                if acc.num_threads:
                    # Process-wide ONNX Runtime intra-op threads; don't clobber an
                    # explicit environment override.
                    os.environ.setdefault("DOCLING_RS_PDF_THREADS", str(acc.num_threads))

        if isinstance(image_hosts, str):
            image_hosts = [h.strip() for h in image_hosts.split(",") if h.strip()]
        elif image_hosts is not None:
            image_hosts = [str(h) for h in image_hosts]

        self._inner = _NativeDocumentConverter(
            fetch_images=fetch_images,
            image_sources=image_sources,
            image_hosts=image_hosts,
            max_image_bytes=max_image_bytes,
            max_images=max_images,
            max_image_total_mb=max_image_total_mb,
            min_image_bytes=min_image_bytes,
            do_ocr=do_ocr,
            force_full_page_ocr=force_full_page_ocr,
            replace_broken_text_layer=replace_broken_text_layer,
            no_text_panels=no_text_panels,
            heading_hierarchy=heading_hierarchy,
            do_table_structure=do_table_structure,
            use_web_browser=use_web_browser,
            do_picture_classification=do_picture_classification,
            do_code_enrichment=do_code_enrichment,
            do_formula_enrichment=do_formula_enrichment,
            do_picture_ocr=do_picture_ocr,
            picture_ocr_classes=_label_list(picture_ocr_classes),
            picture_ocr_min_side=picture_ocr_min_side,
            picture_ocr_scale=picture_ocr_scale,
            keep_picture_images=keep_picture_images,
            do_picture_description=do_picture_description,
            picture_description_endpoint=picture_description_endpoint,
            picture_description_model=picture_description_model,
            picture_description_api_key=picture_description_api_key,
            picture_description_prompt=picture_description_prompt,
            picture_description_mode=picture_description_mode,
            picture_description_concurrency=picture_description_concurrency,
            redact_pii=redact_pii,
            redact_mode=redact_mode,
            redact_kinds=_joined(redact_kinds, ","),
            redact_pattern=_joined(redact_pattern, "\n"),
            redact_images=redact_images,
            redact_allow_terms=_joined(redact_allow_terms, ","),
            redact_deny_terms=_joined(redact_deny_terms, ","),
            redact_allow_patterns=_joined(redact_allow_patterns, "\n"),
            redact_ner_min_score=redact_ner_min_score,
            ocr_lang=ocr_lang,
            ocr_mode=ocr_mode,
            ocr_scale=ocr_scale,
            pdf_word_gap=None if pdf_word_gap is None else str(pdf_word_gap),
            pdf_keep_typography=pdf_keep_typography,
            pdf_report_repairs=pdf_report_repairs,
            ocr_textline_orientation=ocr_textline_orientation,
            ocr_engine=ocr_engine,
            skip_empty_cells=skip_empty_cells,
            compact_tables=compact_tables,
            picture_alt_text=picture_alt_text,
            asr_model=asr_model,
            asr_lang=asr_lang,
            encoding=encoding,
            ipynb_outputs=ipynb_outputs,
            metadata=metadata,
            track_changes=track_changes,
            extract_diagrams=extract_diagrams,
            use_outline_level_for_headings=use_outline_level_for_headings,
            pipeline=pipeline,
            vlm_endpoint=vlm_endpoint,
            vlm_model=vlm_model,
            vlm_api_key=vlm_api_key,
            vlm_prompt=vlm_prompt,
            vlm_max_tokens=vlm_max_tokens,
            document_timeout=document_timeout,
            password=password,
            progress_callback=_progress_bridge(progress_callback),
            show_progress=show_progress,
            page_range=_page_range(page_range),
            images_scale=images_scale,
            generate_page_images=generate_page_images,
            use_embedded_picture_images=use_embedded_picture_images,
            text_layer_only=text_layer_only,
            list_attachments=list_attachments,
            ebcdic_layout=ebcdic_layout,
            video_frames=video_frames,
            video_scene_threshold=video_scene_threshold,
            video_frame_max_side=video_frame_max_side,
            video_frame_dedupe=video_frame_dedupe,
            xbrl_taxonomy=xbrl_taxonomy,
            allowed_formats=(
                [InputFormat(f).value for f in allowed_formats]
                if allowed_formats is not None
                else None
            ),
        )
        # docling's do_picture_description enrichment, run on the converted
        # document (picture_description.py): the engine embeds each picture's
        # crop, so any Python-side describer — e.g. a LangChain chat model via
        # docling_rs.langchain — works without an engine change.
        self._picture_description = None
        if pdf_opts is not None and getattr(pdf_opts, "do_picture_description", False):
            pd = getattr(pdf_opts, "picture_description_options", None)
            if _is_api_options(pd):
                pass  # the engine's pass (#670), wired above
            elif pd is None or not hasattr(pd, "_annotate_images"):
                # docling's default (a local SmolVLM) has no counterpart here;
                # degrade like a missing enrichment model.
                warnings.warn(
                    "do_picture_description needs picture_description_options with a "
                    "describer (e.g. docling_rs.langchain.PictureDescriptionLangChainOptions); "
                    "pictures are left undescribed",
                    stacklevel=2,
                )
            else:
                self._picture_description = pd

        # Backends registered module-wide (#680) before this converter.
        for spec in _BACKENDS:
            self._register(spec)

    def register_backend(
        self,
        fn: Optional[Callable[[bytes, str], Any]] = None,
        *,
        extensions: Optional[Iterable[str]] = None,
        media_types: Optional[Iterable[str]] = None,
        sniff: Optional[Callable[[bytes], bool]] = None,
        priority: int = 0,
        name: Optional[str] = None,
    ) -> Any:
        """Register ``fn`` as a backend of this converter (#680) — see the
        module-level :func:`register_backend`, which registers it on every
        converter created afterwards. Usable as a decorator."""
        if fn is None:
            return lambda f: self.register_backend(
                f, extensions=extensions, media_types=media_types, sniff=sniff,
                priority=priority, name=name,
            )
        self._register(_backend_spec(fn, extensions, media_types, sniff, priority, name))
        return fn

    def _register(self, spec: "BackendSpec") -> None:
        self._inner.register_backend(
            spec.name, spec.fn, list(spec.extensions), list(spec.media_types), spec.sniff,
            spec.priority,
        )

    def registered_backends(self) -> List["BackendSpec"]:
        """This converter's registered backends (#680), in the order they are
        tried — highest priority first, the earliest registration first among
        equals — all before the built-in formats. ``fn`` is ``None`` here:
        the callables live in the engine."""
        return [
            BackendSpec(name, None, tuple(exts), tuple(mts), has_sniff, priority)
            for name, exts, mts, has_sniff, priority in self._inner.registered_backends()
        ]

    def supported_extensions(self) -> List[str]:
        """Every input file extension this converter takes: the engine's and
        the registered backends' (#680), sorted, lowercase, no dot."""
        return list(self._inner.supported_extensions())

    def initialize_pipeline(self, format: Optional[InputFormat] = None) -> None:
        """Eagerly load the ML models for ``format`` (docling's
        ``initialize_pipeline``), so the first PDF conversion doesn't pay the
        model-load cost and later ones reuse the warm pipeline. Only ``PDF`` /
        ``IMAGE`` have models; other formats are a no-op. Uses the converter's
        configured ``do_ocr`` / ``do_table_structure`` (and needs the models
        available — see :func:`download_models`)."""
        self._inner.initialize_pipeline(
            InputFormat(format).value if format is not None else None
        )

    def convert(
        self,
        source: Union[str, os.PathLike, DocumentStream],
        page_range: Optional[Tuple[int, int]] = None,
        progress_callback: Optional["progress.ProgressCallback"] = None,
    ) -> ConversionResult:
        """Convert a filesystem path (str / pathlib.Path), an ``http(s)://``
        URL (downloaded first, as docling does) or an in-memory
        :class:`DocumentStream`. ``page_range=(first, last)`` converts only
        that 1-based inclusive PDF page window, as docling's
        ``convert(source, page_range=…)`` does (#518); ``None`` keeps the
        constructor's window (all pages by default). A YouTube watch URL
        converts to the video's title, metadata and caption transcript as
        timed items (#681), the language from ``asr_lang``.
        ``progress_callback`` receives this call's progress events (#783)
        on top of the constructor's callback — see :mod:`docling_rs.progress`."""
        return self._finish(_wrap(self._convert_native(source, page_range, progress_callback)))

    def convert_all(
        self,
        sources: Iterable[Union[str, os.PathLike, DocumentStream]],
        raises_on_error: bool = True,
        page_range: Optional[Tuple[int, int]] = None,
        progress_callback: Optional["progress.ProgressCallback"] = None,
    ) -> Iterator[ConversionResult]:
        """Convert many sources, yielding a :class:`ConversionResult` each
        (docling's ``convert_all``). With ``raises_on_error=False`` a failing
        source yields a ``failure`` result (empty document) instead of raising.
        ``page_range`` and ``progress_callback`` as in :meth:`convert`; the
        events' ``document_index`` is the source's 1-based position here."""
        for index, source in enumerate(sources, start=1):
            try:
                yield self._finish(
                    _wrap(self._convert_native(source, page_range, progress_callback, index))
                )
            except Exception:
                if raises_on_error:
                    raise
                name = source.name if isinstance(source, DocumentStream) else str(source)
                yield ConversionResult("failure", name, DoclingDocument(name=Path(name).name))

    def convert_bytes(
        self,
        name: str,
        data: bytes,
        page_range: Optional[Tuple[int, int]] = None,
        base_url: Optional[str] = None,
        media_type: Optional[str] = None,
        progress_callback: Optional["progress.ProgressCallback"] = None,
    ) -> ConversionResult:
        """Convert in-memory bytes; ``name``'s extension drives format detection
        (docling's ``DocumentStream`` counterpart). ``page_range`` and
        ``progress_callback`` as in :meth:`convert`. ``base_url`` is the URL
        the bytes came from (#681): relative image references resolve
        against it, and a YouTube watch URL makes the page its transcript.
        ``media_type`` (``Content-Type``) is what a registered backend can
        claim the bytes by (#680), and the format when the name has no known
        extension."""
        native = self._inner.convert_bytes(
            name,
            data,
            page_range=_page_range(page_range),
            base_url=base_url,
            media_type=media_type,
            progress_callback=_progress_bridge(progress_callback),
        )
        return self._finish(_wrap(native))

    def convert_archive(
        self, source: Union[str, os.PathLike, bytes, DocumentStream]
    ) -> Iterator[ArchiveItem]:
        """Convert every document inside a ZIP archive (#557) — a path,
        the archive's ``bytes`` or a :class:`DocumentStream` — yielding an
        :class:`ArchiveItem` per entry in archive order. A broken document
        fails only its own item; raises only when ``source`` is not a
        readable ZIP archive. Nothing is extracted to disk, and the
        ``DOCLING_RS_ZIP_MAX_*`` limits bound what is inflated."""
        if isinstance(source, DocumentStream):
            items = self._inner.convert_archive_bytes(_stream_bytes(source.stream))
        elif isinstance(source, (bytes, bytearray)):
            items = self._inner.convert_archive_bytes(bytes(source))
        elif isinstance(source, str) and _is_url(source):
            _, data = _fetch_url(source)
            items = self._inner.convert_archive_bytes(data)
        else:
            items = self._inner.convert_archive(source)
        for item in items:
            result = None
            if item.result is not None:
                result = self._finish(_wrap(item.result))
            yield ArchiveItem(item.path, item.outcome, result, item.error)

    def _convert_native(self, source, page_range=None, progress_callback=None, document_index=1):
        page_range = _page_range(page_range)
        callback = _progress_bridge(progress_callback)
        if isinstance(source, DocumentStream):
            fmt = getattr(source, "format", None)
            return self._inner.convert_bytes(
                source.name,
                _stream_bytes(source.stream),
                page_range=page_range,
                format=InputFormat(fmt).value if fmt is not None else None,
                progress_callback=callback,
                document_index=document_index,
            )
        if isinstance(source, str) and _is_url(source):
            name, data = _fetch_url(source)
            # The URL rides along (#681): a YouTube watch page is routed by it.
            return self._inner.convert_bytes(
                name,
                data,
                page_range=page_range,
                base_url=source,
                progress_callback=callback,
                document_index=document_index,
            )
        return self._inner.convert(
            source,
            page_range=page_range,
            progress_callback=callback,
            document_index=document_index,
        )

    def _finish(self, result: ConversionResult) -> ConversionResult:
        if self._picture_description is not None and result.status != "failure":
            from .picture_description import describe_pictures

            describe_pictures(result.document, self._picture_description)
        return result


@dataclass
class EmailAttachment:
    """One attachment of an ``.eml`` / Outlook ``.msg``, from
    :func:`email_attachments`. ``data`` is the payload when it was kept
    (``None`` for an attachment over a limit, or one without a payload — a
    reference, an OLE object); ``format`` is the :class:`InputFormat` it
    converts as, ``None`` with ``skipped`` saying why it will not. A
    forwarded message is an ``.eml`` entry whose ``data`` is the nested
    message."""

    index: int
    name: str
    content_type: Optional[str]
    format: Optional[InputFormat]
    size: int
    inline: bool
    skipped: Optional[str]
    data: Optional[bytes]

    def as_stream(self) -> DocumentStream:
        """The payload as a :class:`DocumentStream` named after the attachment
        and carrying its detected ``format`` (#564: a ``scan.bin`` sent as
        ``application/pdf``, or a nameless sniffed part, converts as what it
        is, not what its extension says), ready for
        :meth:`DocumentConverter.convert`. Raises ``ValueError`` for an
        attachment without a payload."""
        if self.data is None:
            raise ValueError(f"attachment {self.name!r} has no payload ({self.skipped})")
        return DocumentStream(name=self.name, stream=io.BytesIO(self.data), format=self.format)


class BackendSpec(NamedTuple):
    """A registered backend (#680): what :func:`register_backend` recorded.
    ``sniff`` is the check (or, from :meth:`DocumentConverter.registered_backends`,
    whether there is one)."""

    name: str
    fn: Optional[Callable[[bytes, str], Any]]
    extensions: Tuple[str, ...]
    media_types: Tuple[str, ...]
    sniff: Any
    priority: int


#: The backends registered module-wide, applied to every DocumentConverter
#: created after them (#680).
_BACKENDS: List[BackendSpec] = []


def _backend_spec(fn, extensions, media_types, sniff, priority, name) -> BackendSpec:
    if not callable(fn):
        raise TypeError("register_backend needs a callable fn(data: bytes, name: str)")
    if sniff is not None and not callable(sniff):
        raise TypeError("sniff must be a callable sniff(data: bytes) -> bool")
    if isinstance(extensions, str) or isinstance(media_types, str):
        raise TypeError("extensions and media_types are lists of strings")
    extensions = tuple(str(e).strip().lstrip(".").lower() for e in (extensions or ()) if str(e).strip())
    media_types = tuple(str(m).strip().lower() for m in (media_types or ()) if str(m).strip())
    if not (extensions or media_types or sniff):
        raise ValueError("register_backend needs extensions, media_types or sniff to claim anything")
    name = name or getattr(fn, "__name__", None) or "backend"
    return BackendSpec(str(name), fn, extensions, media_types, sniff, int(priority))


def register_backend(
    fn: Optional[Callable[[bytes, str], Any]] = None,
    *,
    extensions: Optional[Iterable[str]] = None,
    media_types: Optional[Iterable[str]] = None,
    sniff: Optional[Callable[[bytes], bool]] = None,
    priority: int = 0,
    name: Optional[str] = None,
) -> Any:
    """Add an input format without forking (#680): register ``fn`` as a
    backend for every :class:`DocumentConverter` created afterwards.

    ``fn(data: bytes, name: str)`` gets the source's bytes and file name and
    returns its content as Markdown (a ``str``, read by the engine's Markdown
    backend) or as a docling-core ``DoclingDocument`` (or its dict). It
    claims the sources whose extension is in ``extensions`` (``"rtf2"`` or
    ``".rtf2"``), whose media type is in ``media_types``, or whose bytes
    ``sniff(data) -> bool`` accepts. Registered backends are tried before the
    built-in formats — the highest ``priority`` first, the earliest
    registration first among equals — so a backend can add a format or take
    one over; the document then goes through the converter's usual
    post-processing (enrichment, redaction, picture descriptions). Works as a
    decorator::

        @docling_rs.register_backend(extensions=["rtf2"])
        def read_rtf2(data: bytes, name: str) -> str:
            return "# " + name + "\n\n" + data.decode()

    Archive entries are classified by the engine's own extension table
    before anything is inflated, so a registered format inside a ``.zip`` is
    skipped as an unsupported type.
    """
    if fn is None:
        return lambda f: register_backend(
            f, extensions=extensions, media_types=media_types, sniff=sniff,
            priority=priority, name=name,
        )
    _BACKENDS.append(_backend_spec(fn, extensions, media_types, sniff, priority, name))
    return fn


def registered_backends() -> List[BackendSpec]:
    """The backends registered module-wide with :func:`register_backend`,
    in registration order."""
    return list(_BACKENDS)


def detect_format(
    source: Union[bytes, bytearray, memoryview, str, os.PathLike, DocumentStream],
) -> Optional[Union[InputFormat, str]]:
    """The format ``source`` evidently is, by its content alone (#672; the
    engine's ``InputFormat::detect``, what MarkItDown asks magika for):
    the bytes, a path, or a :class:`DocumentStream`. Signatures only — PDF,
    the Office / ODF / EPUB / iWork packages, the OLE files, RTF, HTML, the
    raster images, DjVu, audio and video containers, WordPerfect, WebVTT,
    DocTags, SVG, AbiWord, XML, JSON and Jupyter notebooks, MIME messages,
    LaTeX. Returns the :class:`InputFormat` member, the bare format id for
    a format the enum does not spell (``"svg"``, ``"pages"``, …), or
    ``None`` for bytes without a signature (prose, Markdown, CSV, random
    bytes) — those convert by their name, or as plain text.

    A :class:`DocumentConverter` already does this for a file with no or an
    unknown extension, a nameless stream and a ``data:`` URL; call it to
    route or label an input yourself."""
    if isinstance(source, DocumentStream):
        data = _stream_bytes(source.stream)
    elif isinstance(source, (bytes, bytearray, memoryview)):
        data = bytes(source)
    else:
        data = Path(source).read_bytes()
    fmt = _detect_format(data)
    if fmt is None:
        return None
    try:
        return InputFormat(fmt)
    except ValueError:
        return fmt


def email_attachments(
    source: Union[str, os.PathLike, bytes, DocumentStream],
    *,
    max_entries: Optional[int] = None,
    max_entry_size: Optional[int] = None,
    max_total_size: Optional[int] = None,
) -> List[EmailAttachment]:
    """List the attachments of an ``.eml`` / ``.msg`` with their payloads — a
    path, the message bytes, or a :class:`DocumentStream`. The limits are byte
    counts bounding what is kept (defaults: 10 000 attachments, 256 MiB each,
    1 GiB in all); file names are reduced to a safe base name, so ``name``
    can be written to a directory as it is. Convert one with
    ``converter.convert(attachment.as_stream())``."""
    if isinstance(source, DocumentStream):
        data = _stream_bytes(source.stream)
    elif isinstance(source, (bytes, bytearray, memoryview)):
        data = bytes(source)
    else:
        data = Path(source).read_bytes()
    rows = _email_attachments(data, max_entries, max_entry_size, max_total_size)
    out = []
    for index, name, content_type, fmt, size, inline, skipped, payload in rows:
        try:
            fmt = InputFormat(fmt) if fmt is not None else None
        except ValueError:
            pass
        out.append(EmailAttachment(index, name, content_type, fmt, size, inline, skipped, payload))
    return out


def _pdf_pipeline_options(
    format_options: Optional[Dict[InputFormat, PdfFormatOption]],
) -> Optional[PdfPipelineOptions]:
    """The PDF (or image) pipeline options from a docling-style ``format_options``
    mapping, if any."""
    if not format_options:
        return None
    for fmt in (InputFormat.PDF, InputFormat.IMAGE):
        fo = format_options.get(fmt)
        if fo is not None and getattr(fo, "pipeline_options", None) is not None:
            return fo.pipeline_options
    return None


def _pdf_backend_password(
    format_options: Optional[Dict[InputFormat, PdfFormatOption]],
) -> Optional[str]:
    """The PDF password from a docling-style ``format_options`` mapping —
    ``PdfFormatOption(backend_options=PdfBackendOptions(password=...))``, a
    plain string or a pydantic ``SecretStr`` — if any (#611)."""
    if not format_options:
        return None
    fo = format_options.get(InputFormat.PDF)
    password = getattr(getattr(fo, "backend_options", None), "password", None)
    if password is None:
        return None
    if hasattr(password, "get_secret_value"):
        password = password.get_secret_value()
    return str(password)


def _stream_bytes(stream) -> bytes:
    """A :class:`DocumentStream`'s whole content: rewound first when it can
    be, as docling's backends do (``path_or_stream.seek(0)``), so a stream
    already read once — or handed over after a peek — still converts (#564)."""
    if hasattr(stream, "seekable") and stream.seekable():
        stream.seek(0)
    return stream.read()


def _label_list(labels) -> Optional[str]:
    """``picture_ocr_classes`` as the engine's comma-separated string: a
    string passes through, an iterable of labels is joined."""
    if labels is None or isinstance(labels, str):
        return labels
    return ",".join(str(label) for label in labels)
def _joined(value, sep: str) -> Optional[str]:
    """A list-or-string option as the engine's joined string (``redact_kinds``
    with commas, ``redact_pattern`` with newlines); a string passes through."""
    if value is None or isinstance(value, str):
        return value
    return sep.join(str(v) for v in value)


def _page_range(page_range) -> Optional[Tuple[int, int]]:
    """docling's ``page_range`` argument as the engine's ``(first, last)``:
    any two-item sequence of integers; ``None`` stays ``None``. docling's
    default window ``(1, sys.maxsize)`` simply means "all pages"."""
    if page_range is None:
        return None
    try:
        first, last = page_range
        return (int(first), int(last))
    except (TypeError, ValueError):
        raise ValueError(
            f"page_range must be a (first, last) pair of page numbers, got {page_range!r}"
        ) from None


def _is_url(source: str) -> bool:
    return source.startswith(("http://", "https://"))


# Media types whose extension the URL path may not carry (e.g. arXiv's
# https://arxiv.org/pdf/2408.09869) — format detection is by extension.
_URL_EXTENSIONS = {
    "application/pdf": ".pdf",
    "text/html": ".html",
    "application/xhtml+xml": ".html",
    "text/markdown": ".md",
    "text/plain": ".txt",
    "text/csv": ".csv",
    "application/rss+xml": ".rss",
    "application/atom+xml": ".atom",
    "application/json": ".json",
    "application/xml": ".xml",
    "text/xml": ".xml",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document": ".docx",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation": ".pptx",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet": ".xlsx",
    "application/epub+zip": ".epub",
    "image/png": ".png",
    "image/jpeg": ".jpg",
    "image/tiff": ".tiff",
    "image/webp": ".webp",
    "audio/mpeg": ".mp3",
    "audio/wav": ".wav",
    "audio/x-wav": ".wav",
    "video/mp4": ".mp4",
}


def _fetch_url(url: str) -> "tuple[str, bytes]":
    """Download an http(s) source (docling's ``convert(url)``): the file name
    comes from ``Content-Disposition``, else the URL path; when that has no
    extension, the response's media type supplies one. Read whole into memory
    — the engine converts bytes, like ``DocumentStream``."""
    import email.message
    import urllib.parse
    import urllib.request

    req = urllib.request.Request(url, headers={"User-Agent": f"docling-rs/{__version__}"})
    with urllib.request.urlopen(req) as resp:
        data = resp.read()
        disposition = resp.headers.get("Content-Disposition", "")
        media_type = resp.headers.get_content_type()
    name = ""
    if disposition:
        msg = email.message.Message()
        msg["Content-Disposition"] = disposition
        name = msg.get_filename() or ""
    if not name:
        name = Path(urllib.parse.unquote(urllib.parse.urlparse(url).path)).name or "document"
    if not Path(name).suffix and media_type in _URL_EXTENSIONS:
        name += _URL_EXTENSIONS[media_type]
    return name, data


def _wrap(native) -> ConversionResult:
    """Validate the Rust engine's JSON into a real ``DoclingDocument``."""
    document = DoclingDocument.model_validate_json(native.document_json)
    errors = [ErrorItem(*item) for item in getattr(native, "errors", ())]
    metadata_json = getattr(native, "metadata_json", None)
    confidence_json = getattr(native, "confidence_json", None)
    pdf_repairs_json = getattr(native, "pdf_repairs_json", None)
    return ConversionResult(
        native.status,
        native.input_name,
        document,
        errors,
        getattr(native, "redaction", None),
        json.loads(metadata_json) if metadata_json else None,
        (
            ConfidenceReport.model_validate_json(confidence_json)
            if confidence_json
            else ConfidenceReport()
        ),
        json.loads(pdf_repairs_json) if pdf_repairs_json is not None else None,
    )
