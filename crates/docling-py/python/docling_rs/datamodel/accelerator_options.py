"""docling-parity alias of ``docling.datamodel.accelerator_options`` (where
docling ≥ 2.29 defines the accelerator config)::

    # was: from docling.datamodel.accelerator_options import AcceleratorDevice, AcceleratorOptions
    from docling_rs.datamodel.accelerator_options import AcceleratorDevice, AcceleratorOptions
"""

from ..options import AcceleratorDevice, AcceleratorOptions

__all__ = ["AcceleratorDevice", "AcceleratorOptions"]
