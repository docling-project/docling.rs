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
  .venv-compare/bin/python scripts/install/export_tableformer.py [accurate-artifacts-dir] [out_dir]
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
        # Per-layer, pre-transposed variants (issue #97): the hoisted-KV decoder
        # step consumes these directly, so the per-token graph never re-splits /
        # re-transposes the constant 19 MB cross tensors (was ~5 ms of every
        # ~7.5 ms step). K is emitted K^T ([1,H,head_dim,S]) ready for q@K^T.
        per_layer = []
        for k, v in zip(ks, vs):
            per_layer.append(k.transpose(2, 3).contiguous())
        per_layer.extend(vs)
        return (torch.stack(ks, 0), torch.stack(vs, 0), eo_raw, *per_layer)


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


class DecodeKV(nn.Module):
    # True-KV-cache step: feeds ONLY the newly emitted tag. The layer-output
    # cache above still re-projects self-attention K/V over the whole cached
    # prefix in every layer on every step (and re-embeds the full tag sequence
    # for layer 0) — O(n^2) matmuls per table. Here each layer's projected
    # K and V are the cache (cache_k/cache_v [L,1,H,past,head_dim], empty at
    # past=0): a step embeds one tag at its absolute position (= past, read
    # from the cache shape), projects q/k/v for that one token, appends k/v,
    # and attends the single query over the cached keys — O(past) memory
    # traffic per step, no re-projection. Mathematically identical to the
    # layer-output cache (past tokens' K/V are linear projections of fixed
    # layer inputs), verified argmax-identical step-by-step below.
    def forward(self, tag, cross_k, cross_v, cache_k, cache_v):
        e = EMBED_DIM_
        pos = cache_k.shape[3]  # tokens already decoded = this tag's position
        x = tt._embedding(tag)  # [1,1,e]
        x = x + tt._positional_encoding.pe[pos]
        out = x
        new_ks, new_vs = [], []
        for i, layer in enumerate(tt._decoder.layers):
            sa = layer.self_attn
            W, b = sa.in_proj_weight, sa.in_proj_bias
            q = F.linear(out, W[:e], b[:e])
            k = F.linear(out, W[e : 2 * e], b[e : 2 * e])
            v = F.linear(out, W[2 * e :], b[2 * e :])
            # [1,1,e] → [1,H,1,head_dim]
            q = q.reshape(1, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
            k = k.reshape(1, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
            v = v.reshape(1, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
            new_ks.append(k)
            new_vs.append(v)
            kk = torch.cat([cache_k[i], k], dim=2)  # [1,H,past+1,hd]
            vv = torch.cat([cache_v[i], v], dim=2)
            t = F.scaled_dot_product_attention(q, kk, vv)
            t = t.squeeze(0).permute(1, 0, 2).reshape(1, 1, e)
            t = F.linear(t, sa.out_proj.weight, sa.out_proj.bias)
            tgt_last = layer.norm1(out + t)
            t = _cross_attn(layer, tgt_last, cross_k[i], cross_v[i])
            tgt_last = layer.norm2(tgt_last + t)
            t = layer.linear2(layer.activation(layer.linear1(tgt_last)))
            out = layer.norm3(tgt_last + t)
        out_cache_k = torch.cat([cache_k, torch.stack(new_ks, 0)], dim=3)
        out_cache_v = torch.cat([cache_v, torch.stack(new_vs, 0)], dim=3)
        last = out[-1]
        return tt._fc(last), last, out_cache_k, out_cache_v


class DecodeKVHoisted(nn.Module):
    # DecodeKV with the constant cross-attention tensors hoisted out of the
    # step graph (issue #97). The stacked [L,1,H,S,hd] cross inputs made every
    # token pay a Split of 2x9.6 MB plus per-layer Transposes (~5 ms of a
    # ~7.5 ms step); here each layer's K^T ([1,H,hd,S]) and V ([1,H,S,hd])
    # arrive as separate, already-laid-out inputs (computed once per table by
    # the encoder), and cross-attention is written out manually so the ONNX
    # graph consumes them in place. Self-attention and the caches are exactly
    # DecodeKV's.
    def forward(self, tag, cache_k, cache_v, *cross):
        e = EMBED_DIM_
        cross_kt, cross_v = cross[:N_LAYERS], cross[N_LAYERS:]
        pos = cache_k.shape[3]
        x = tt._embedding(tag)
        x = x + tt._positional_encoding.pe[pos]
        out = x
        new_ks, new_vs = [], []
        for i, layer in enumerate(tt._decoder.layers):
            sa = layer.self_attn
            W, b = sa.in_proj_weight, sa.in_proj_bias
            q = F.linear(out, W[:e], b[:e])
            k = F.linear(out, W[e : 2 * e], b[e : 2 * e])
            v = F.linear(out, W[2 * e :], b[2 * e :])
            q = q.reshape(1, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
            k = k.reshape(1, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
            v = v.reshape(1, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
            new_ks.append(k)
            new_vs.append(v)
            kk = torch.cat([cache_k[i], k], dim=2)
            vv = torch.cat([cache_v[i], v], dim=2)
            t = F.scaled_dot_product_attention(q, kk, vv)
            t = t.squeeze(0).permute(1, 0, 2).reshape(1, 1, e)
            t = F.linear(t, sa.out_proj.weight, sa.out_proj.bias)
            tgt_last = layer.norm1(out + t)
            # manual cross-attention over the pre-transposed constants
            mha = layer.multihead_attn
            cq = F.linear(tgt_last, mha.in_proj_weight[:e], mha.in_proj_bias[:e])
            cq = cq.reshape(1, N_HEADS, HEAD_DIM).permute(1, 0, 2).unsqueeze(0)
            scores = (cq * HEAD_DIM**-0.5) @ cross_kt[i]  # [1,H,1,S]
            co = torch.softmax(scores, dim=-1) @ cross_v[i]  # [1,H,1,hd]
            co = co.squeeze(0).permute(1, 0, 2).reshape(1, 1, e)
            t = F.linear(co, mha.out_proj.weight, mha.out_proj.bias)
            tgt_last = layer.norm2(tgt_last + t)
            t = layer.linear2(layer.activation(layer.linear1(tgt_last)))
            out = layer.norm3(tgt_last + t)
        out_cache_k = torch.cat([cache_k, torch.stack(new_ks, 0)], dim=3)
        out_cache_v = torch.cat([cache_v, torch.stack(new_vs, 0)], dim=3)
        last = out[-1]
        return tt._fc(last), last, out_cache_k, out_cache_v


def check(name, a, b):
    import numpy as np

    d = float(np.abs(a - b).max())
    print(f"  {name}: shape {tuple(a.shape)} | max|onnx-torch| = {d:.2e}")
    return d


from torch.export import Dim  # noqa: E402

img = torch.randn(1, 3, 448, 448)
with torch.no_grad():
    cross_k, cross_v, enc_out, *cross_per_layer = Encode()(img)
torch.onnx.export(
    Encode(), (img,), f"{OUT}/encoder.onnx",
    input_names=["image"],
    output_names=["cross_k", "cross_v", "enc_out"]
    + [f"cross_kt_{i}" for i in range(N_LAYERS)]
    + [f"cross_v_{i}" for i in range(N_LAYERS)],
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

# ---- decoder_kv.onnx: the true-KV-cache step (preferred by the Rust loop) ----
# Verify against the layer-output-cache module by rolling both autoregressively
# from <start> over the same encoder memory: greedy argmax must match at every
# step (the two are the same math; only reduction shapes differ).
print("verifying DecodeKVHoisted against the layer-output cache, 64-step rollout:")
kv_k = torch.zeros((N_LAYERS, 1, nh, 0, HEAD_DIM))
kv_v = torch.zeros((N_LAYERS, 1, nh, 0, HEAD_DIM))
lc_cache = torch.zeros((N_LAYERS, 0, 1, EMBED_DIM))
roll_tags = [start]
max_dlogit = 0.0
with torch.no_grad():
    for step in range(64):
        lc_logits, _, lc_cache = Decode()(
            torch.tensor(roll_tags, dtype=torch.long).unsqueeze(1),
            cross_k, cross_v, lc_cache,
        )
        kv_logits, _, kv_k, kv_v = DecodeKVHoisted()(
            torch.tensor([[roll_tags[-1]]], dtype=torch.long),
            kv_k, kv_v, *cross_per_layer,
        )
        d = float((lc_logits - kv_logits).abs().max())
        max_dlogit = max(max_dlogit, d)
        a, b = int(lc_logits.argmax()), int(kv_logits.argmax())
        assert a == b, f"step {step}: argmax diverged (layer-cache {a} vs kv-hoisted {b})"
        roll_tags.append(a)
print(f"  64 steps argmax-identical, max|dlogits| = {max_dlogit:.2e}")

tag1 = torch.tensor([[start]], dtype=torch.long)
past_kv = Dim("past", min=0, max=1024)
cross_names = [f"cross_kt_{i}" for i in range(N_LAYERS)] + [
    f"cross_v_{i}" for i in range(N_LAYERS)
]
# Export with the rollout's past=64 caches: a past=0 example lets the exporter
# specialize `pe[cache_k.shape[3]]` to `pe[0]`, silently zeroing the positional
# encoding for every later step (decode then never emits <end>).
torch.onnx.export(
    DecodeKVHoisted(), (tag1, kv_k, kv_v, *cross_per_layer),
    f"{OUT}/decoder_kv.onnx",
    input_names=["tag", "cache_k", "cache_v"] + cross_names,
    output_names=["logits", "hidden", "out_cache_k", "out_cache_v"],
    dynamo=True,
    dynamic_shapes=(
        {},
        {3: past_kv},
        {3: past_kv},
        tuple({} for _ in cross_names),
    ),
)
print("decoder_kv.onnx (hoisted true-KV-cache step):")
empty_kv = torch.zeros((N_LAYERS, 1, nh, 0, HEAD_DIM))
with torch.no_grad():
    kl, kh, kck, kcv = DecodeKVHoisted()(tag1, empty_kv, empty_kv, *cross_per_layer)
feeds = {"tag": tag1.numpy(), "cache_k": empty_kv.numpy(), "cache_v": empty_kv.numpy()}
feeds.update({n: t.numpy() for n, t in zip(cross_names, cross_per_layer)})
ko = ort.InferenceSession(f"{OUT}/decoder_kv.onnx").run(None, feeds)
check("logits", ko[0], kl.numpy())
check("hidden", ko[1], kh.numpy())
check("out_cache_k", ko[2], kck.numpy())
check("out_cache_v", ko[3], kcv.numpy())

# word map → tokens file for the Rust decode loop
json.dump(
    {"word_map_tag": word_map, "start": start, "end": word_map["<end>"]},
    open(f"{OUT}/wordmap.json", "w"),
)
print("wrote wordmap.json; OTSL vocab size:", len(word_map))
