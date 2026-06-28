#!/usr/bin/env python3
"""Export docling's TableFormer (TableModel04_rs) to ONNX for the Rust pipeline.

TableFormer is autoregressive: an image encoder + a tag-transformer encoder run
once to produce a memory tensor, then a decoder step is looped to emit OTSL
structure tokens, and a bbox decoder turns the per-cell hidden states into boxes.
We export three graphs and drive the loop from Rust:

  encoder.onnx : image[1,3,448,448]            -> memory[784,1,512]
  decoder.onnx : tags[seq,1] + memory           -> logits[1,V], hidden[1,512]
  bbox.onnx    : memory + cell_hidden[ncells,512] -> classes, coords  (optional)

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
        eo = m._encoder(img)
        eo = tt._input_filter(eo.permute(0, 3, 1, 2)).permute(0, 2, 3, 1)
        ei = eo.reshape(1, -1, eo.size(-1)).permute(1, 0, 2)
        pos = ei.shape[0]
        return tt._encoder(ei, mask=torch.zeros((nh, pos, pos), dtype=torch.bool))


class Decode(nn.Module):
    def forward(self, tags, memory):
        emb = tt._positional_encoding(tt._embedding(tags))
        decoded, _ = tt._decoder(emb, memory, None, memory_key_padding_mask=None)
        last = decoded[-1]
        return tt._fc(last), last


def check(name, a, b):
    import numpy as np

    d = float(np.abs(a - b).max())
    print(f"  {name}: shape {tuple(a.shape)} | max|onnx-torch| = {d:.2e}")
    return d


img = torch.randn(1, 3, 448, 448)
with torch.no_grad():
    mem = Encode()(img)
torch.onnx.export(
    Encode(), (img,), f"{OUT}/encoder.onnx",
    input_names=["image"], output_names=["memory"], opset_version=17, dynamo=False,
)
tags = torch.full((3, 1), start, dtype=torch.long)
with torch.no_grad():
    logits, hidden = Decode()(tags, mem)
torch.onnx.export(
    Decode(), (tags, mem), f"{OUT}/decoder.onnx",
    input_names=["tags", "memory"], output_names=["logits", "hidden"],
    dynamic_axes={"tags": {0: "seq"}}, opset_version=17, dynamo=False,
)

import onnxruntime as ort  # noqa: E402

print("encoder.onnx:")
eo = ort.InferenceSession(f"{OUT}/encoder.onnx").run(None, {"image": img.numpy()})[0]
check("memory", eo, mem.numpy())
print("decoder.onnx:")
do = ort.InferenceSession(f"{OUT}/decoder.onnx").run(
    None, {"tags": tags.numpy(), "memory": mem.numpy()}
)
check("logits", do[0], logits.numpy())
check("hidden", do[1], hidden.numpy())

# word map → tokens file for the Rust decode loop
json.dump(
    {"word_map_tag": word_map, "start": start, "end": word_map["<end>"]},
    open(f"{OUT}/wordmap.json", "w"),
)
print("wrote wordmap.json; OTSL vocab size:", len(word_map))
