"""docling-parity alias of ``docling.datamodel.pipeline_options``::

    # was: from docling.datamodel.pipeline_options import PdfPipelineOptions, TableFormerMode
    from docling_rs.datamodel.pipeline_options import PdfPipelineOptions, TableFormerMode

``AcceleratorDevice`` / ``AcceleratorOptions`` are importable both from here
(docling's historical location) and from ``datamodel.accelerator_options``
(their current one) — mirroring docling's own back-compat re-export.
"""

from ..options import (
    AcceleratorDevice,
    AcceleratorOptions,
    PdfPipelineOptions,
    TableFormerMode,
    TableStructureOptions,
)

__all__ = [
    "PdfPipelineOptions",
    "TableStructureOptions",
    "TableFormerMode",
    "AcceleratorDevice",
    "AcceleratorOptions",
]
