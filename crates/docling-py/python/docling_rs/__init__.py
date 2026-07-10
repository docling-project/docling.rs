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

One-time model setup (mirrors docling's artifact download; ~700 MB into
``~/.cache/docling.rs``)::

    import docling_rs; docling_rs.download_models()

Declarative formats (DOCX/HTML/XLSX/…) need no models at all.
"""

from __future__ import annotations

import enum
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Union

from docling_core.types.doc import DoclingDocument

from . import models
from .models import cache_dir, download_models, ensure_env
from ._native import __version__
from ._native import DocumentConverter as _NativeDocumentConverter

__all__ = [
    "DocumentConverter",
    "ConversionResult",
    "ConversionStatus",
    "DoclingDocument",
    "InputDocument",
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

    ``artifacts_path`` overrides the model cache directory (docling's
    ``artifacts_path``); by default the pipeline is pointed at
    ``~/.cache/docling.rs`` (see :func:`download_models`). ``fetch_images``
    resolves remote/local ``<img src>`` for HTML/EPUB.
    """

    def __init__(self, fetch_images: bool = False, artifacts_path=None):
        ensure_env(artifacts_path)
        self._inner = _NativeDocumentConverter(fetch_images=fetch_images)

    def convert(self, source: Union[str, os.PathLike]) -> ConversionResult:
        """Convert a filesystem path (str / pathlib.Path)."""
        native = self._inner.convert(source)
        return _wrap(native)

    def convert_bytes(self, name: str, data: bytes) -> ConversionResult:
        """Convert in-memory bytes; ``name``'s extension drives format detection
        (docling's ``DocumentStream`` counterpart)."""
        native = self._inner.convert_bytes(name, data)
        return _wrap(native)


def _wrap(native) -> ConversionResult:
    """Validate the Rust engine's JSON into a real ``DoclingDocument``."""
    document = DoclingDocument.model_validate_json(native.document_json)
    return ConversionResult(native.status, native.input_name, document)
