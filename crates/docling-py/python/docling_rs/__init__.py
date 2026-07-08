"""docling.rs — Rust docling port, Python bindings.

A strangler-fig drop-in for Python docling's common path::

    from docling_rs import DocumentConverter          # was: from docling.document_converter import ...

    result = DocumentConverter().convert("document.pdf")
    print(result.document.export_to_markdown())
    data = result.document.export_to_dict()            # docling JSON wire format

One-time model setup (mirrors docling's artifact download; ~700 MB into
``~/.cache/docling.rs``)::

    import docling_rs; docling_rs.download_models()

Declarative formats (DOCX/HTML/XLSX/…) need no models at all.
"""

from . import models
from .models import cache_dir, download_models, ensure_env
from ._native import (
    ConversionResult,
    DoclingDocument,
    __version__,
)
from ._native import DocumentConverter as _NativeDocumentConverter

__all__ = [
    "DocumentConverter",
    "ConversionResult",
    "DoclingDocument",
    "download_models",
    "ensure_env",
    "cache_dir",
    "models",
    "__version__",
]


class DocumentConverter:
    """docling-shaped converter. ``artifacts_path`` overrides the model cache
    directory (docling's ``artifacts_path``); by default the pipeline is
    pointed at ``~/.cache/docling.rs`` (see :func:`download_models`)."""

    def __init__(self, strict=False, fetch_images=False, artifacts_path=None):
        ensure_env(artifacts_path)
        self._inner = _NativeDocumentConverter(strict=strict, fetch_images=fetch_images)

    def convert(self, source) -> ConversionResult:
        """Convert a filesystem path (str / pathlib.Path)."""
        return self._inner.convert(source)

    def convert_bytes(self, name: str, data: bytes) -> ConversionResult:
        """Convert in-memory bytes; ``name``'s extension drives format detection
        (docling's ``DocumentStream`` counterpart)."""
        return self._inner.convert_bytes(name, data)
