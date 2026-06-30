#!/usr/bin/env python3
"""Export docling's TableFormer (TableModel04_rs) to ONNX for the Rust pipeline.

TableFormer is autoregressive: an image encoder + a tag-transformer encoder run
once to produce a memory tensor, then a decoder step is looped to emit OTSL
structure tokens, and a bbox decoder turns the per-cell hidden states into boxes.
We export three graphs and drive the loop from Rust:

  encoder.onnx : image[1,3,448,448]
                   -> cross_k/cross_v[L,1,H,784,head_dim], enc_out[1,28,28,512]
                 (the per-layer cross-attention K/V, projected from the image
                  memory once so the decoder never re-projects it)
  decoder.onnx : tags[seq,1] + cross_k + cross_v + cache[L,past,1,512]
                   -> logits[1,V], hidden[1,512], out_cache[L,past+1,1,512]
                 (doubly-cached step: self-attn state cache + precomputed cross K/V;
                  pass cache=[L,0,1,512] on the first call)
  bbox.onnx    : enc_out + cell_hidden[ncells,512] -> classes, coords  (optional)

IMPORTANT: export from the *same* checkpoint docling runs. Current docling pulls
`docling-project/docling-models` (NOT the older `ds4sd/docling-models`); their
TableFormer weights differ and produce different OTSL. Point the arg at:
  ~/.cache/huggingface/hub/models--docling-project--docling-models/snapshots/*/model_artifacts/tableformer/accurate

Verified: with these weights the exported graphs reproduce docling's OTSL token
sequence byte-exact on docling's own preprocessed table tensor.

Run inside a Python env with docling (`docling_ibm_models` + torch) plus
`onnx onnxscript onnxruntime`. The accurate-artifacts dir auto-resolves from the
HuggingFace cache (downloading `docling-project/docling-models` if absent), or
pass it explicitly:
  .venv-compare/bin/python scripts/export_tableformer.py [accurate-artifacts-dir] [out_dir]
"""
import glob
import json
import os
import sys
import warnings

import torch
import torch.nn as nn

warnings.filterwarnings("ignore")


def resolve_artifacts():
    """The `accurate` TableFormer artifacts dir from the published docling models.
    Uses the HF cache if present, else downloads `docling-project/docling-models`
    (the checkpoint current docling runs — its OTSL differs from the older ds4sd
    weights, so the exported graphs must come from this one)."""
    pat = "models--docling-project--docling-models/snapshots/*/model_artifacts/tableformer/accurate"
    hits = glob.glob(os.path.expanduser(f"~/.cache/huggingface/hub/{pat}"))
    if hits:
        return sorted(hits)[-1]
    from huggingface_hub import snapshot_download

    root = snapshot_download("docling-project/docling-models")
    return os.path.join(root, "model_artifacts", "tableformer", "accurate")


# Optional positional args: [artifacts-dir] [out-dir]. The artifacts dir is
# recognised by its `tm_config.json`; anything else is the out-dir, so both
# `… <artifacts> <out>` and the bare `… <out>` (auto-resolve) calls work.
_args = sys.argv[1:]
ART = _args[0] if _args and os.path.isfile(os.path.join(_args[0], "tm_config.json")) else None
if ART:
    _args = _args[1:]
OUT = _args[0] if _args else "models/tableformer"
if ART is None:
    ART = resolve_artifacts()
print(f"tableformer artifacts: {ART}")
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


import torch.nn.functional as F  # noqa: E402

N_HEADS = nh
EMBED_DIM_ = tt._fc.in_features
HEAD_DIM = EMBED_DIM_ // N_HEADS


def _cross_kv(layer, memory):
    # Project the constant cross-attention memory to this decoder layer's K/V once
    # (the per-step cost the stateless/​self-cache graphs paid every token). Splits
    # nn.MultiheadAttention's packed in_proj; reshaped [1, H, S, head_dim] for SDPA.
    mha = layer.multihead_attn
    W, b = mha.in_proj_weight, mha.in_proj_bias
    e = EMBED_DIM_
    k = F.linear(memory, W[e : 2 * e], b[e : 2 * e])
    v = F.linear(memory, W[2 * e :], b[2 * e :])
    s = memory.shape[0]
    k = k.reshape(s, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
    v = v.reshape(s, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
    return k, v


def _cross_attn(layer, query, k, v):
    # nn.MultiheadAttention cross-attention with K/V precomputed (`_cross_kv`).
    # Verified bit-identical to `layer.multihead_attn(query, memory, memory)`.
    mha = layer.multihead_attn
    e = EMBED_DIM_
    lq = query.shape[0]
    q = F.linear(query, mha.in_proj_weight[:e], mha.in_proj_bias[:e])
    q = q.reshape(lq, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
    o = F.scaled_dot_product_attention(q, k, v).squeeze(0).permute(1, 0, 2).reshape(lq, 1, e)
    return F.linear(o, mha.out_proj.weight, mha.out_proj.bias)


class Encode(nn.Module):
    def forward(self, img):
        eo_raw = m._encoder(img)  # [1,28,28,512] — also feeds the bbox decoder
        eo = tt._input_filter(eo_raw.permute(0, 3, 1, 2)).permute(0, 2, 3, 1)
        ei = eo.reshape(1, -1, eo.size(-1)).permute(1, 0, 2)
        pos = ei.shape[0]
        mem = tt._encoder(ei, mask=torch.zeros((nh, pos, pos), dtype=torch.bool))
        # Precompute each decoder layer's cross-attention K/V from the memory once,
        # so the autoregressive decoder never re-projects it. [L,1,H,S,head_dim].
        ks, vs = [], []
        for layer in tt._decoder.layers:
            k, v = _cross_kv(layer, mem)
            ks.append(k)
            vs.append(v)
        return torch.stack(ks, 0), torch.stack(vs, 0), eo_raw


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


N_LAYERS = len(tt._decoder.layers)
EMBED_DIM = tt._fc.in_features


class Decode(nn.Module):
    # A doubly-cached step of docling's TMTransformerDecoder:
    #  * self-attention keeps docling's per-layer state `cache` ([L,past,1,dim],
    #    empty [L,0,1,dim] on the first step), so each layer attends the new token
    #    against the cached prefix instead of re-decoding it (O(n^2) total, not the
    #    stateless graph's O(n^3));
    #  * cross-attention reads the per-layer K/V precomputed once by the encoder
    #    (`cross_k`/`cross_v`, [L,1,H,S,head_dim]) instead of re-projecting the image
    #    memory every step — the dominant per-step cost.
    # Faithfully reproduces TMTransformerDecoderLayer (dropout is identity in eval),
    # verified argmax-identical to the stateless graph.
    def forward(self, tags, cross_k, cross_v, cache):
        out = tt._positional_encoding(tt._embedding(tags))
        tag_cache = []
        for i, layer in enumerate(tt._decoder.layers):
            tgt = out
            tgt_last = tgt[-1:]
            t = layer.self_attn(tgt_last, tgt, tgt, need_weights=False)[0]
            tgt_last = layer.norm1(tgt_last + t)
            t = _cross_attn(layer, tgt_last, cross_k[i], cross_v[i])
            tgt_last = layer.norm2(tgt_last + t)
            t = layer.linear2(layer.activation(layer.linear1(tgt_last)))
            tgt_last = layer.norm3(tgt_last + t)
            out = tgt_last
            tag_cache.append(out)
            out = torch.cat([cache[i], out], dim=0)
        out_cache = torch.cat([cache, torch.stack(tag_cache, 0)], dim=1)
        last = out[-1]
        return tt._fc(last), last, out_cache


def check(name, a, b):
    import numpy as np

    d = float(np.abs(a - b).max())
    print(f"  {name}: shape {tuple(a.shape)} | max|onnx-torch| = {d:.2e}")
    return d


from torch.export import Dim  # noqa: E402

img = torch.randn(1, 3, 448, 448)
with torch.no_grad():
    cross_k, cross_v, enc_out = Encode()(img)
torch.onnx.export(
    Encode(), (img,), f"{OUT}/encoder.onnx",
    input_names=["image"], output_names=["cross_k", "cross_v", "enc_out"],
    opset_version=17, dynamo=False,
)
tags = torch.full((3, 1), start, dtype=torch.long)
cache0 = torch.zeros((N_LAYERS, 2, 1, EMBED_DIM))  # trace with past>0 → symbolic
with torch.no_grad():
    logits, hidden, out_cache = Decode()(tags, cross_k, cross_v, cache0)
# The dynamo exporter is needed here: the legacy tracer bakes the sequence length
# into the attention reshapes, so a 1-token first step fails. dynamo keeps the
# `seq` and cache `past` axes symbolic.
seq = Dim("seq", min=1, max=1024)
past = Dim("past", min=0, max=1024)
torch.onnx.export(
    Decode(), (tags, cross_k, cross_v, cache0), f"{OUT}/decoder.onnx",
    input_names=["tags", "cross_k", "cross_v", "cache"],
    output_names=["logits", "hidden", "out_cache"],
    dynamo=True, dynamic_shapes=({0: seq}, {}, {}, {1: past}),
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
check("cross_k", eres[0], cross_k.numpy())
check("cross_v", eres[1], cross_v.numpy())
check("enc_out", eres[2], enc_out.numpy())
print("decoder.onnx (doubly-cached step):")
do = ort.InferenceSession(f"{OUT}/decoder.onnx").run(
    None,
    {
        "tags": tags.numpy(),
        "cross_k": cross_k.numpy(),
        "cross_v": cross_v.numpy(),
        "cache": cache0.numpy(),
    },
)
check("logits", do[0], logits.numpy())
check("hidden", do[1], hidden.numpy())
check("out_cache", do[2], out_cache.numpy())
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
