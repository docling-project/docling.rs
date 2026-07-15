#!/usr/bin/env python3
"""Export docling's RT-DETR layout model (heron) to ONNX for the Rust pipeline.

The graph gets a **dynamic batch dimension** so the Rust worker can layout-detect
several pages with one inference call (issue #73). A dynamic-axes export leaves
the AIFI encoder's sincos position embedding as runtime ops, which drift ~1e-6
from the constant the static export folds through torch — enough to flip
borderline detections corpus-wide. To keep numerics identical to the historical
static export, this script exports twice (static + dynamic), folds the static
graph's position-embedding subgraph offline (onnx ReferenceEvaluator, exact
IEEE semantics), and splices the folded constant into the dynamic graph. The
constant is batch-independent (it broadcasts over the batch in one Add), so
per-sample outputs are bit-identical at every batch size.

Needs a Python env with torch + transformers + onnx:
    pip install torch transformers onnx

Usage:
    python scripts/install/export_layout.py models/layout_heron.onnx
"""

import os
import sys
import tempfile

import onnx
import torch
from onnx import helper, numpy_helper
from onnx.reference import ReferenceEvaluator
from transformers import RTDetrV2ForObjectDetection

REPO = "docling-project/docling-layout-heron"
POS_EMBED = "/position_embedding/"


class Wrap(torch.nn.Module):
    """Return just (logits, pred_boxes) so the ONNX graph is simple."""

    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, pixel_values):
        o = self.m(pixel_values=pixel_values)
        return o.logits, o.pred_boxes


def export(wrap, out, dynamic):
    dummy = torch.zeros(1, 3, 640, 640, dtype=torch.float32)
    kwargs = {}
    if dynamic:
        kwargs["dynamic_axes"] = {
            "pixel_values": {0: "batch"},
            "logits": {0: "batch"},
            "pred_boxes": {0: "batch"},
        }
    torch.onnx.export(
        wrap,
        dummy,
        out,
        input_names=["pixel_values"],
        output_names=["logits", "pred_boxes"],
        opset_version=17,
        do_constant_folding=True,
        dynamo=False,
        **kwargs,
    )


def fold_static_pos_embed(static_path):
    """Fold the static export's (self-contained) position-embedding subgraph to
    the constant torch would have produced, evaluated with exact ONNX reference
    semantics."""
    m = onnx.load(static_path)
    sel = [n for n in m.graph.node if POS_EMBED in n.name]
    produced = {o for n in sel for o in n.output}
    boundary = sorted(
        {i for n in m.graph.node if POS_EMBED not in n.name for i in n.input if i in produced}
    )
    assert len(boundary) == 1, f"expected one pos-embed output, got {boundary}"
    sub = helper.make_model(
        helper.make_graph(
            sel,
            "pos",
            [],
            [helper.make_tensor_value_info(boundary[0], onnx.TensorProto.UNDEFINED, None)],
        ),
        opset_imports=list(m.opset_import),
    )
    (value,) = ReferenceEvaluator(sub).run(None, {})
    return boundary[0], value


def splice_pos_embed(dynamic_path, pe_name, pe_value, out):
    """Replace the dynamic graph's runtime position-embedding computation with
    the static export's folded constant, keeping the two exact int64 spatial
    scalars the surrounding reshapes still consume."""
    m = onnx.load(dynamic_path)
    g = m.graph
    sel = [n for n in g.node if POS_EMBED in n.name]
    produced = {o for n in sel for o in n.output}
    boundary = sorted(
        {i for n in g.node if POS_EMBED not in n.name for i in n.input if i in produced}
    )
    float_out = [b for b in boundary if not b.endswith(("Cast_output_0", "Cast_1_output_0"))]
    assert float_out == [pe_name], f"pos-embed boundary mismatch: {float_out} vs {pe_name}"
    # Backward slice: keep only the nodes the int64 scalar casts need.
    keep_targets = [b for b in boundary if b != pe_name]
    by_out = {o: n for n in sel for o in n.output}
    need, stack = set(), list(keep_targets)
    while stack:
        n = by_out.get(stack.pop())
        if n is None or id(n) in need:
            continue
        need.add(id(n))
        stack.extend(n.input)
    for n in [n for n in sel if id(n) not in need]:
        g.node.remove(n)
    g.initializer.append(numpy_helper.from_array(pe_value, name=pe_name))
    onnx.save(m, out)


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "models/layout_heron.onnx"
    out_dir = os.path.dirname(out)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    print(f"loading {REPO} ...", flush=True)
    model = RTDetrV2ForObjectDetection.from_pretrained(REPO, torch_dtype=torch.float32).eval()
    wrap = Wrap(model)
    with torch.no_grad():
        logits, boxes = wrap(torch.zeros(1, 3, 640, 640, dtype=torch.float32))
    print(f"logits {tuple(logits.shape)} boxes {tuple(boxes.shape)}", flush=True)
    with tempfile.TemporaryDirectory() as tmp:
        static = os.path.join(tmp, "static.onnx")
        dynamic = os.path.join(tmp, "dynamic.onnx")
        print("exporting static graph (numeric reference) ...", flush=True)
        export(wrap, static, dynamic=False)
        print("exporting dynamic-batch graph ...", flush=True)
        export(wrap, dynamic, dynamic=True)
        print("splicing static-folded position embedding ...", flush=True)
        pe_name, pe_value = fold_static_pos_embed(static)
        splice_pos_embed(dynamic, pe_name, pe_value, out)
    print(f"wrote {out}", flush=True)


if __name__ == "__main__":
    main()
