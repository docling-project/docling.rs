"""docling-parity alias of the ``docling.utils`` package (only the pieces
docling.rs has an equivalent for — currently ``model_downloader``)."""

from . import model_downloader

__all__ = ["model_downloader"]
