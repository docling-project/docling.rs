"""docling-parity alias of the ``docling.datamodel`` package: the same
submodule layout (``base_models``, ``pipeline_options``,
``accelerator_options``, ``document``) re-exporting docling_rs's objects, so
docling imports migrate by swapping the package name only.
"""

from . import accelerator_options, base_models, document, pipeline_options

__all__ = ["accelerator_options", "base_models", "document", "pipeline_options"]
