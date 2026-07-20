"""docling-parity alias of ``docling.document_converter``.

Lets a docling codebase migrate by replacing ``docling`` with ``docling_rs``
in the import path — same module, same names::

    # was: from docling.document_converter import DocumentConverter, PdfFormatOption
    from docling_rs.document_converter import DocumentConverter, PdfFormatOption

Everything here is the exact object also exported flat from ``docling_rs``.
"""

from . import ConversionResult, DocumentConverter
from .options import PdfFormatOption

__all__ = ["DocumentConverter", "PdfFormatOption", "ConversionResult"]
