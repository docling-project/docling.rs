#!/usr/bin/env python3
"""Export docling's TableFormer (TableModel04_rs) to ONNX for the Rust pipeline.

TableFormer is autoregressive: an image encoder + a tag-transformer encoder run
once to produce a memory tensor, then a decoder step is looped to emit OTSL
structure tokens, and a bbox decoder turns the per-cell hidden states into boxes.
We export three graphs and drive the loop from Rust:

  encoder.onnx : image[1,3,448,448]            -> memory[784,1,512]
  decoder.onnx : tags[seq,1] + memory           -> logits[1,V], hidden[1,512]
  bbox.onnx    : memory + cell_hidden[ncells,512] -> classes, coords  (optional)

IMPORTANT: export from the *same* checkpoint docling runs. Current docling pulls
`docling-project/docling-models` (NOT the older `ds4sd/docling-models`); their
TableFormer weights differ and produce different OTSL. Point the arg at:
  ~/.cache/huggingface/hub/models--docling-project--docling-models/snapshots/*/model_artifacts/tableformer/accurate

Verified: with these weights the exported graphs reproduce docling's OTSL token
sequence byte-exact on docling's own preprocessed table tensor.

Run inside the docling venv:
  .venv-compare/bin/python scripts/export_tableformer.py <accurate-artifacts-dir> [out_dir]
"""
import json
import os
import sys
import warnings

import torch
import torch.nn as nn

warnings.filterwarnings("ignore")

ART = sys.argv[1]
OUT = sys.argv[2] if len(sys.argv) > 2 else "models/tableformer"
os.makedirs(OUT, exist_ok=True)

cfg = json.load(open(f"{ART}/tm_config.json"))
cfg["model"]["save_dir"] = ART
cfg["predict"]["profiling"] = False

from docling_ibm_models.tableformer.data_management.tf_predictor import TFPredictor  # noqa: E402

pred = TFPredictor(cfg, device="cpu")
m = pred._model
m.eval()
torch.set_grad_enabled(False)
for p in m.parameters():
    p.requires_grad_(False)
tt = m._tag_transformer
nh = tt._n_heads
word_map = pred._init_data["word_map"]["word_map_tag"]
start = word_map["<start>"]


class Encode(nn.Module):
    def forward(self, img):
        eo_raw = m._encoder(img)  # [1,28,28,512] — also feeds the bbox decoder
        eo = tt._input_filter(eo_raw.permute(0, 3, 1, 2)).permute(0, 2, 3, 1)
        ei = eo.reshape(1, -1, eo.size(-1)).permute(1, 0, 2)
        pos = ei.shape[0]
        mem = tt._encoder(ei, mask=torch.zeros((nh, pos, pos), dtype=torch.bool))
        return mem, eo_raw


class BBoxDecode(nn.Module):
    # docling's BBoxDecoder.inference, batched over cells: each cell's tag hidden
    # state attends over the (bbox-decoder-filtered) encoder output to a box.
    def forward(self, enc_out, tag_h):  # enc_out [1,28,28,512], tag_h [N,512]
        bd = m._bbox_decoder
        e = bd._input_filter(enc_out.permute(0, 3, 1, 2)).permute(0, 2, 3, 1)
        e = e.reshape(1, -1, e.size(3))  # [1, 784, 512]
        n = tag_h.shape[0]
        h = bd._init_h(e.mean(dim=1)).expand(n, -1)  # [N, dim] (same for all cells)
        a = bd._attention
        att = a._full_att(
            a._relu(
                a._encoder_att(e)
                + a._tag_decoder_att(tag_h).unsqueeze(1)
                + a._language_att(h).unsqueeze(1)
            )
        ).squeeze(2)
        alpha = a._softmax(att)  # [N, 784]
        awe = (e * alpha.unsqueeze(2)).sum(dim=1)  # [N, 512]
        h = (bd._sigmoid(bd._f_beta(h)) * awe) * h
        return bd._bbox_embed(h).sigmoid(), bd._class_embed(h)


class Decode(nn.Module):
    # The model's custom decoder layer keeps only the last token per layer and
    # relies on a (non-standard) cache for the previous tokens' per-layer states.
    # Re-running it cache-less loses deep context. Equivalently and statelessly we
    # apply each layer to the *whole* prefix under a causal mask — verified to
    # match the cache-based output exactly — so the ONNX graph is a plain step.
    def forward(self, tags, memory):
        o = tt._positional_encoding(tt._embedding(tags))
        s = o.shape[0]
        cm = torch.triu(torch.full((s, s), float("-inf")), diagonal=1)
        for mod in tt._decoder.layers:
            o = mod.norm1(o + mod.self_attn(o, o, o, attn_mask=cm, need_weights=False)[0])
            o = mod.norm2(o + mod.multihead_attn(o, memory, memory, need_weights=False)[0])
            o = mod.norm3(o + mod.linear2(mod.activation(mod.linear1(o))))
        last = o[-1]
        return tt._fc(last), last


def check(name, a, b):
    import numpy as np

    d = float(np.abs(a - b).max())
    print(f"  {name}: shape {tuple(a.shape)} | max|onnx-torch| = {d:.2e}")
    return d


from torch.export import Dim  # noqa: E402

img = torch.randn(1, 3, 448, 448)
with torch.no_grad():
    mem, enc_out = Encode()(img)
torch.onnx.export(
    Encode(), (img,), f"{OUT}/encoder.onnx",
    input_names=["image"], output_names=["memory", "enc_out"],
    opset_version=17, dynamo=False,
)
tags = torch.full((4, 1), start, dtype=torch.long)
with torch.no_grad():
    logits, hidden = Decode()(tags, mem)
# The dynamo exporter is needed here: the legacy tracer bakes the sequence length
# into nn.MultiheadAttention's reshape, so a 1-token first step fails. dynamo keeps
# the `seq` axis symbolic.
seq = Dim("seq", min=1, max=1024)
torch.onnx.export(
    Decode(), (tags, mem), f"{OUT}/decoder.onnx",
    input_names=["tags", "memory"], output_names=["logits", "hidden"],
    dynamo=True, dynamic_shapes=({0: seq}, {}),
)
# bbox decoder: N cell hiddens → N boxes (+ classes). N is dynamic.
tag_h = torch.randn(5, 512)
with torch.no_grad():
    boxes, classes = BBoxDecode()(enc_out, tag_h)
ncells = Dim("ncells", min=1, max=1024)
torch.onnx.export(
    BBoxDecode(), (enc_out, tag_h), f"{OUT}/bbox.onnx",
    input_names=["enc_out", "tag_h"], output_names=["boxes", "classes"],
    dynamo=True, dynamic_shapes=({}, {0: ncells}),
)

import onnxruntime as ort  # noqa: E402

print("encoder.onnx:")
eres = ort.InferenceSession(f"{OUT}/encoder.onnx").run(None, {"image": img.numpy()})
check("memory", eres[0], mem.numpy())
check("enc_out", eres[1], enc_out.numpy())
print("decoder.onnx:")
do = ort.InferenceSession(f"{OUT}/decoder.onnx").run(
    None, {"tags": tags.numpy(), "memory": mem.numpy()}
)
check("logits", do[0], logits.numpy())
check("hidden", do[1], hidden.numpy())
print("bbox.onnx:")
bo = ort.InferenceSession(f"{OUT}/bbox.onnx").run(
    None, {"enc_out": enc_out.numpy(), "tag_h": tag_h.numpy()}
)
check("boxes", bo[0], boxes.numpy())
check("classes", bo[1], classes.numpy())

# word map → tokens file for the Rust decode loop
json.dump(
    {"word_map_tag": word_map, "start": start, "end": word_map["<end>"]},
    open(f"{OUT}/wordmap.json", "w"),
)
print("wrote wordmap.json; OTSL vocab size:", len(word_map))
