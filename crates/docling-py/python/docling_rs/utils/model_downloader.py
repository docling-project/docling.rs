"""docling-parity alias of ``docling.utils.model_downloader``::

    # was: from docling.utils.model_downloader import download_models
    from docling_rs.utils.model_downloader import download_models

Note the signature is docling.rs's own (``force=…`` etc., returning the cache
dir) — the *import path* matches docling, the download itself fetches this
repo's ONNX models, not docling's torch artifacts.
"""

from ..models import download_models

__all__ = ["download_models"]
