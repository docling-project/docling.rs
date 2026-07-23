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
import os
import warnings
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, Iterable, Iterator, Optional, Union

from docling_core.types.doc import DoclingDocument, ImageRefMode

from . import models
from .models import cache_dir, download_models, ensure_env
from .options import (
    AcceleratorDevice,
    AcceleratorOptions,
    DocumentStream,
    InputFormat,
    PdfFormatOption,
    PdfPipelineOptions,
    TableFormerMode,
    TableStructureOptions,
)
from . import chunking

# GPU wheels (PyPI ``docling-rs-cuda``) bundle ONNX Runtime's CUDA provider
# libraries next to `_native` in this package directory; the native module is
# linked with an `$ORIGIN` rpath, so ONNX Runtime's dlopen-by-name finds them
# there with no Python-side setup — the same mechanism the native CLI uses.
# (Deliberately NO ctypes preload here: loading the provider libraries with
# RTLD_GLOBAL before the static ORT inside `_native` initializes duplicates
# ORT symbols process-wide and segfaulted at session creation in testing.)
from ._native import ConversionError, __version__
from ._native import DocumentConverter as _NativeDocumentConverter

__all__ = [
    "DocumentConverter",
    "ConversionResult",
    "ConversionStatus",
    "ConversionError",
    "InputDocument",
    "DoclingDocument",
    "ImageRefMode",
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
class InputDocument:
    """docling's ``ConversionResult.input`` shim: the source's file name/path."""

    file: Path


class ConversionResult:
    """docling's ``ConversionResult``: ``.document`` (a genuine
    :class:`~docling_core.types.doc.DoclingDocument`), ``.status`` and
    ``.input``."""

    def __init__(self, status: str, input_name: str, document: DoclingDocument):
        self.status = ConversionStatus(status)
        self.document = document
        self.input = InputDocument(file=Path(input_name))


class DocumentConverter:
    """docling-shaped converter whose processor is Rust.

    Parameters mirror docling's converter and ``PdfPipelineOptions``:

    * ``format_options`` — ``{InputFormat.PDF: PdfFormatOption(pipeline_options=...)}``,
      as in docling. The PDF/image pipeline options ``do_ocr``,
      ``do_table_structure`` and ``accelerator_options.num_threads`` take effect.
    * ``do_ocr`` / ``do_table_structure`` — a shorthand for the same, used when no
      ``format_options`` is given.
    * ``fetch_images`` — resolve remote/local ``<img src>`` for HTML/EPUB.
    * ``use_web_browser`` — render HTML via headless Chrome before parsing.
    * ``allowed_formats`` — restrict conversion to these :class:`InputFormat`\\ s
      (docling's converter arg); a source of any other format raises.
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
        do_picture_classification: bool = False,
        do_code_enrichment: bool = False,
        do_formula_enrichment: bool = False,
        fetch_images: bool = False,
        use_web_browser: bool = False,
        ocr_lang: Optional[str] = None,
        artifacts_path=None,
    ):
        ensure_env(artifacts_path)

        # A PDF/IMAGE PdfFormatOption overrides the shorthand kwargs.
        pipeline = _pdf_pipeline_options(format_options)
        if pipeline is not None:
            do_ocr = pipeline.do_ocr
            do_table_structure = pipeline.do_table_structure
            do_picture_classification = getattr(
                pipeline, "do_picture_classification", do_picture_classification
            )
            do_code_enrichment = getattr(
                pipeline, "do_code_enrichment", do_code_enrichment
            )
            do_formula_enrichment = getattr(
                pipeline, "do_formula_enrichment", do_formula_enrichment
            )
            # Map docling's ocr_options.lang (a list of language ids) onto the
            # engine's en/ch recognition-model switch. First entry wins;
            # anything that isn't recognisably English/Chinese is ignored with
            # a warning (the engine default — English — applies).
            ocr_opts = getattr(pipeline, "ocr_options", None)
            langs = list(getattr(ocr_opts, "lang", None) or [])
            if langs:
                head = str(langs[0]).lower()
                if head in ("en", "english"):
                    ocr_lang = "en"
                elif head in ("ch", "chinese", "ch_sim", "zh"):
                    ocr_lang = "ch"
                else:
                    warnings.warn(
                        f"docling.rs OCR supports en|ch recognition models; "
                        f"ocr_options.lang={langs!r} is ignored",
                        stacklevel=2,
                    )
            acc = getattr(pipeline, "accelerator_options", None)
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
                    warnings.warn(
                        "docling.rs has no MPS execution provider; device "
                        "'mps' is ignored (CoreML exists behind the `coreml` "
                        "cargo feature for native macOS builds).",
                        stacklevel=2,
                    )
                if acc.num_threads:
                    # Process-wide ONNX Runtime intra-op threads; don't clobber an
                    # explicit environment override.
                    os.environ.setdefault("DOCLING_RS_PDF_THREADS", str(acc.num_threads))

        self._inner = _NativeDocumentConverter(
            fetch_images=fetch_images,
            do_ocr=do_ocr,
            do_table_structure=do_table_structure,
            use_web_browser=use_web_browser,
            do_picture_classification=do_picture_classification,
            do_code_enrichment=do_code_enrichment,
            do_formula_enrichment=do_formula_enrichment,
            ocr_lang=ocr_lang,
            allowed_formats=(
                [InputFormat(f).value for f in allowed_formats]
                if allowed_formats is not None
                else None
            ),
        )

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

    def convert(self, source: Union[str, os.PathLike, DocumentStream]) -> ConversionResult:
        """Convert a filesystem path (str / pathlib.Path) or an in-memory
        :class:`DocumentStream`."""
        native = self._convert_native(source)
        return _wrap(native)

    def convert_all(
        self,
        sources: Iterable[Union[str, os.PathLike, DocumentStream]],
        raises_on_error: bool = True,
    ) -> Iterator[ConversionResult]:
        """Convert many sources, yielding a :class:`ConversionResult` each
        (docling's ``convert_all``). With ``raises_on_error=False`` a failing
        source yields a ``failure`` result (empty document) instead of raising."""
        for source in sources:
            try:
                yield _wrap(self._convert_native(source))
            except Exception:
                if raises_on_error:
                    raise
                name = source.name if isinstance(source, DocumentStream) else str(source)
                yield ConversionResult("failure", name, DoclingDocument(name=Path(name).name))

    def convert_bytes(self, name: str, data: bytes) -> ConversionResult:
        """Convert in-memory bytes; ``name``'s extension drives format detection
        (docling's ``DocumentStream`` counterpart)."""
        native = self._inner.convert_bytes(name, data)
        return _wrap(native)

    def _convert_native(self, source):
        if isinstance(source, DocumentStream):
            return self._inner.convert_bytes(source.name, source.stream.read())
        return self._inner.convert(source)


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


def _wrap(native) -> ConversionResult:
    """Validate the Rust engine's JSON into a real ``DoclingDocument``."""
    document = DoclingDocument.model_validate_json(native.document_json)
    return ConversionResult(native.status, native.input_name, document)
