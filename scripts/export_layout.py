#!/usr/bin/env python3
"""Export docling's RT-DETR layout model (heron) to ONNX for the Rust pipeline.

Needs a Python env with torch + transformers + onnx:
    pip install torch transformers onnx

Usage:
    python scripts/export_layout.py models/layout_heron.onnx
"""

import sys

import torch
from transformers import RTDetrV2ForObjectDetection

REPO = "docling-project/docling-layout-heron"


class Wrap(torch.nn.Module):
    """Return just (logits, pred_boxes) so the ONNX graph is simple."""

    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, pixel_values):
        o = self.m(pixel_values=pixel_values)
        return o.logits, o.pred_boxes


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "models/layout_heron.onnx"
    print(f"loading {REPO} ...", flush=True)
    model = RTDetrV2ForObjectDetection.from_pretrained(
        REPO, torch_dtype=torch.float32
    ).eval()
    wrap = Wrap(model)
    dummy = torch.zeros(1, 3, 640, 640, dtype=torch.float32)
    with torch.no_grad():
        logits, boxes = wrap(dummy)
    print(f"logits {tuple(logits.shape)} boxes {tuple(boxes.shape)}", flush=True)
    torch.onnx.export(
        wrap,
        dummy,
        out,
        input_names=["pixel_values"],
        output_names=["logits", "pred_boxes"],
        opset_version=17,
        do_constant_folding=True,
        dynamo=False,
    )
    print(f"wrote {out}", flush=True)


if __name__ == "__main__":
    main()
