"""docling-parity alias of ``docling.exceptions``::

    # was: from docling.exceptions import ConversionError
    from docling_rs.exceptions import ConversionError
"""

from ._native import ConversionError

__all__ = ["ConversionError"]
