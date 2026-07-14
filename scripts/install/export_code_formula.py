#!/usr/bin/env python3
"""Export docling's CodeFormulaV2 VLM (code language + formula LaTeX enrichment)
to ONNX for the Rust pipeline.

CodeFormulaV2 (docling-project/CodeFormulaV2) is an Idefics3 / SmolVLM-256M-class
model: a SigLIP-base vision tower (512x512 tiles, patch 16), a pixel-shuffle
connector projecting to the text width, and a 30-layer Llama-style decoder
(hidden 576, 9 heads / 3 KV heads, head_dim 64, RoPE theta 100000, vocab
100480, untied lm_head). Python docling drives it with
`transformers.AutoModelForImageTextToText.generate` (greedy); the Rust port
needs the same computation as three ONNX graphs plus the HF tokenizer.json:

  vision.onnx     : pixel_values[T,3,512,512]
                      -> image_features[T,64,576]
                    (SigLIP tower + pixel-shuffle connector, T = image tiles;
                     the 512x512 grid yields 1024 patches -> 64 visual tokens)
  embed.onnx      : input_ids[1,S] -> inputs_embeds[1,S,576]
                    (token embeddings only; the Rust side scatters the visual
                     tokens into the <image> positions, exactly like
                     Idefics3Model.inputs_merger)
  decoder_kv.onnx : inputs_embeds[1,S,576] + position_ids[1,S]
                    + past_k/past_v[30,1,3,P,64]
                      -> logits[1,V] (last position only)
                         + new_k/new_v[30,1,3,P+S,64]
                    (a manually written KV-cache Llama step, prefill and decode
                     in one graph: pass P=0 tensors on the first call, then
                     S=1 steps; the causal mask `j <= P + i` is built inside)

Like the TableFormer export, the decoder graph re-implements the layer math
(eager attention) from the checkpoint's own weight modules instead of tracing
transformers' masking utilities — the trace is shape-generic and verified
argmax-identical to `model.generate` end-to-end below.

Needs a Python env with torch + transformers (>=4.46 for Idefics3) + onnx:
    pip install torch transformers onnx pillow

Usage:
    python scripts/install/export_code_formula.py [out_dir]   # default models/code_formula
"""

import math
import os
import sys
import warnings

import torch
import torch.nn as nn
import torch.nn.functional as F

warnings.filterwarnings("ignore")

REPO = "docling-project/CodeFormulaV2"
OUT = sys.argv[1] if len(sys.argv) > 1 else "models/code_formula"
os.makedirs(OUT, exist_ok=True)

print(f"loading {REPO} ...", flush=True)
from transformers import AutoModelForImageTextToText, AutoProcessor  # noqa: E402

model = AutoModelForImageTextToText.from_pretrained(REPO, dtype=torch.float32)
model.eval()
# The wrapper modules below close over `model`'s submodules without registering
# them, so the tracer treats the weights as constants — which torch.onnx
# rejects while they still require grad.
model.requires_grad_(False)
processor = AutoProcessor.from_pretrained(REPO)

text_model = model.model.text_model
vision_model = model.model.vision_model
connector = model.model.connector
lm_head = model.lm_head
cfg = model.config.text_config
N_LAYERS = cfg.num_hidden_layers
N_HEADS = cfg.num_attention_heads
N_KV = cfg.num_key_value_heads
HEAD_DIM = cfg.head_dim
EPS = cfg.rms_norm_eps
# transformers 5.x moved rope_theta into `rope_parameters`; older versions keep
# the flat attribute. (The composite Idefics3 config carries a stale top-level
# rope_theta=100000 from training — the text_config value is the one the
# checkpoint's Llama layers actually run with, and the generate() cross-check
# below would fail loudly on a mismatch.)
_rp = getattr(cfg, "rope_parameters", None) or {}
THETA = _rp.get("rope_theta") if isinstance(_rp, dict) else None
if THETA is None:
    THETA = getattr(cfg, "rope_theta", 10000.0)


# ---------------------------------------------------------------------------
# vision.onnx — SigLIP tower + connector. T (tile count) is dynamic.
# ---------------------------------------------------------------------------
class Vision(nn.Module):
    # Idefics3VisionEmbeddings assigns bucketed position ids through a boolean
    # patch-mask scatter, which traces to a mixed-type ScatterND that
    # onnxruntime rejects. Every tile the processor produces is exactly
    # 512x512 (fully valid patches), so the position ids are just
    # arange(num_patches) — compute the embeddings manually and run the
    # encoder + connector as-is. Verified against the full vision_model below.
    def forward(self, pixel_values):
        emb = vision_model.embeddings.patch_embedding(pixel_values)  # [T,768,32,32]
        emb = emb.flatten(2).transpose(1, 2)  # [T,1024,768]
        emb = emb + vision_model.embeddings.position_embedding.weight[None]
        hidden = vision_model.encoder(inputs_embeds=emb).last_hidden_state
        hidden = vision_model.post_layernorm(hidden)
        return connector(hidden)


# ---------------------------------------------------------------------------
# embed.onnx — token embeddings (the <image> positions are overwritten with
# the vision features by the Rust caller, mirroring Idefics3's inputs_merger).
# ---------------------------------------------------------------------------
class Embed(nn.Module):
    def forward(self, input_ids):
        return text_model.embed_tokens(input_ids)


# ---------------------------------------------------------------------------
# decoder_kv.onnx — one KV-cached decoder pass over S new positions.
# ---------------------------------------------------------------------------
def rms_norm(x, weight):
    var = x.pow(2).mean(-1, keepdim=True)
    return weight * (x * torch.rsqrt(var + EPS))


class DecoderKV(nn.Module):
    def forward(self, inputs_embeds, position_ids, past_k, past_v):
        # RoPE tables for the new positions (Llama convention: the half-split
        # rotate_half layout with cos/sin repeated across both halves).
        inv_freq = 1.0 / (
            THETA
            ** (torch.arange(0, HEAD_DIM, 2, dtype=torch.float32) / HEAD_DIM)
        )  # [head_dim/2]
        freqs = position_ids.to(torch.float32)[..., None] * inv_freq  # [1,S,hd/2]
        emb = torch.cat([freqs, freqs], dim=-1)  # [1,S,hd]
        cos = emb.cos()[:, None, :, :]  # [1,1,S,hd]
        sin = emb.sin()[:, None, :, :]

        def rope(t):
            half = t.shape[-1] // 2
            rot = torch.cat([-t[..., half:], t[..., :half]], dim=-1)
            return t * cos + rot * sin

        past_len = past_k.shape[3]
        seq_len = inputs_embeds.shape[1]
        # Causal mask over the concatenated sequence: query i (absolute
        # position past+i) may attend keys j <= past+i.
        q_pos = past_len + torch.arange(seq_len)[:, None]
        k_pos = torch.arange(past_len + seq_len)[None, :]
        mask = torch.where(
            k_pos <= q_pos, torch.tensor(0.0), torch.tensor(torch.finfo(torch.float32).min)
        )  # [S, P+S]

        x = inputs_embeds
        new_ks, new_vs = [], []
        for i, layer in enumerate(text_model.layers):
            attn = layer.self_attn
            h = rms_norm(x, layer.input_layernorm.weight)
            q = attn.q_proj(h).view(1, seq_len, N_HEADS, HEAD_DIM).transpose(1, 2)
            k = attn.k_proj(h).view(1, seq_len, N_KV, HEAD_DIM).transpose(1, 2)
            v = attn.v_proj(h).view(1, seq_len, N_KV, HEAD_DIM).transpose(1, 2)
            q, k = rope(q), rope(k)
            k = torch.cat([past_k[i], k], dim=2)  # [1,kv,P+S,hd]
            v = torch.cat([past_v[i], v], dim=2)
            new_ks.append(k)
            new_vs.append(v)
            # GQA: each KV head serves n_heads/n_kv query heads.
            kx = k.repeat_interleave(N_HEADS // N_KV, dim=1)
            vx = v.repeat_interleave(N_HEADS // N_KV, dim=1)
            scores = q @ kx.transpose(-1, -2) / math.sqrt(HEAD_DIM) + mask
            out = F.softmax(scores, dim=-1) @ vx  # [1,heads,S,hd]
            out = out.transpose(1, 2).reshape(1, seq_len, N_HEADS * HEAD_DIM)
            x = x + attn.o_proj(out)
            h = rms_norm(x, layer.post_attention_layernorm.weight)
            mlp = layer.mlp
            x = x + mlp.down_proj(F.silu(mlp.gate_proj(h)) * mlp.up_proj(h))

        x = rms_norm(x[:, -1:, :], text_model.norm.weight)
        logits = lm_head(x)[:, 0, :]  # [1,V]
        return logits, torch.stack(new_ks, 0), torch.stack(new_vs, 0)


# ---------------------------------------------------------------------------
# Sanity check the hand-written decoder against transformers' generate on a
# real prompt BEFORE exporting: prefill + a few greedy steps must match.
# ---------------------------------------------------------------------------
def greedy_reference(pixel_values, input_ids, n_new):
    with torch.no_grad():
        out = model.generate(
            input_ids=input_ids,
            pixel_values=pixel_values,
            attention_mask=torch.ones_like(input_ids),
            max_new_tokens=n_new,
            do_sample=False,
            use_cache=True,
        )
    return out[0, input_ids.shape[1]:].tolist()


def greedy_manual(vision_mod, embed_mod, dec, pixel_values, input_ids, n_new):
    image_token = model.config.image_token_id
    with torch.no_grad():
        feats = vision_mod(pixel_values)  # [T,64,576]
        embeds = embed_mod(input_ids)  # [1,S,576]
        flat = feats.reshape(-1, feats.shape[-1])
        positions = (input_ids[0] == image_token).nonzero(as_tuple=True)[0]
        assert positions.numel() == flat.shape[0], (positions.numel(), flat.shape)
        embeds[0, positions] = flat
        past_k = torch.zeros(N_LAYERS, 1, N_KV, 0, HEAD_DIM)
        past_v = torch.zeros(N_LAYERS, 1, N_KV, 0, HEAD_DIM)
        pos = torch.arange(input_ids.shape[1])[None, :]
        toks = []
        x = embeds
        for _ in range(n_new):
            logits, past_k, past_v = dec(x, pos, past_k, past_v)
            t = int(logits.argmax(-1))
            toks.append(t)
            if t == model.config.eos_token_id:
                break
            x = embed_mod(torch.tensor([[t]]))
            pos = torch.tensor([[past_k.shape[3]]])
        return toks


print("verifying the manual decoder against transformers.generate ...", flush=True)
from PIL import Image, ImageDraw  # noqa: E402

img = Image.new("RGB", (400, 120), (255, 255, 255))
ImageDraw.Draw(img).text((10, 40), "x = 1 + 2", fill=(0, 0, 0))
messages = [
    {"role": "user", "content": [{"type": "image"}, {"type": "text", "text": "<code>"}]}
]
prompt = processor.apply_chat_template(messages, add_generation_prompt=True)
inputs = processor(text=[prompt], images=[img], return_tensors="pt")
pv = inputs["pixel_values"][0]  # [T,3,512,512]
ids = inputs["input_ids"]

vision = Vision().eval()
embed = Embed().eval()
decoder = DecoderKV().eval()

N_CHECK = 12
# generate() wants the batched [B, num_images, C, H, W] layout; the manual
# path (and the ONNX graphs) run the flattened tile batch [T, C, H, W].
ref = greedy_reference(inputs["pixel_values"], ids, N_CHECK)
got = greedy_manual(vision, embed, decoder, pv, ids.clone(), N_CHECK)
assert got == ref[: len(got)], f"manual decode diverges:\n  ref {ref}\n  got {got}"
print(f"  greedy tokens match transformers.generate ({got[:8]}...)", flush=True)

# ---------------------------------------------------------------------------
# Export the three graphs.
# ---------------------------------------------------------------------------
with torch.no_grad():
    print("exporting vision.onnx ...", flush=True)
    torch.onnx.export(
        vision,
        (pv,),
        f"{OUT}/vision.onnx",
        input_names=["pixel_values"],
        output_names=["image_features"],
        dynamic_axes={"pixel_values": {0: "tiles"}, "image_features": {0: "tiles"}},
        opset_version=17,
        do_constant_folding=True,
        dynamo=False,
    )

    print("exporting embed.onnx ...", flush=True)
    torch.onnx.export(
        Embed().eval(),
        (ids,),
        f"{OUT}/embed.onnx",
        input_names=["input_ids"],
        output_names=["inputs_embeds"],
        dynamic_axes={"input_ids": {1: "seq"}, "inputs_embeds": {1: "seq"}},
        opset_version=17,
        do_constant_folding=True,
        dynamo=False,
    )

    print("exporting decoder_kv.onnx ...", flush=True)
    embeds0 = embed(ids)
    pos0 = torch.arange(ids.shape[1])[None, :]
    pk = torch.zeros(N_LAYERS, 1, N_KV, 3, HEAD_DIM)
    pv0 = torch.zeros(N_LAYERS, 1, N_KV, 3, HEAD_DIM)
    torch.onnx.export(
        decoder,
        (embeds0, pos0, pk, pv0),
        f"{OUT}/decoder_kv.onnx",
        input_names=["inputs_embeds", "position_ids", "past_k", "past_v"],
        output_names=["logits", "new_k", "new_v"],
        dynamic_axes={
            "inputs_embeds": {1: "seq"},
            "position_ids": {1: "seq"},
            "past_k": {3: "past"},
            "past_v": {3: "past"},
            "new_k": {3: "total"},
            "new_v": {3: "total"},
        },
        opset_version=17,
        do_constant_folding=True,
        dynamo=False,
    )

# The Rust side decodes with the HF tokenizer.json (tokenizers crate).
import shutil  # noqa: E402

from huggingface_hub import hf_hub_download  # noqa: E402

for f in ["tokenizer.json"]:
    shutil.copyfile(hf_hub_download(REPO, f), f"{OUT}/{f}")

# ---------------------------------------------------------------------------
# Verify the exported graphs against torch end-to-end (greedy, zero-len prefill
# cache exercises the dynamic `past` axis at P=0).
# ---------------------------------------------------------------------------
print("verifying ONNX graphs with onnxruntime ...", flush=True)
import numpy as np  # noqa: E402
import onnxruntime as ort  # noqa: E402

sv = ort.InferenceSession(f"{OUT}/vision.onnx", providers=["CPUExecutionProvider"])
se = ort.InferenceSession(f"{OUT}/embed.onnx", providers=["CPUExecutionProvider"])
sd = ort.InferenceSession(f"{OUT}/decoder_kv.onnx", providers=["CPUExecutionProvider"])

feats = sv.run(None, {"pixel_values": pv.numpy()})[0]
with torch.no_grad():
    tfeats = vision(pv).numpy()
dv = float(np.abs(feats - tfeats).max())
print(f"  vision max|diff| = {dv:.2e}", flush=True)
assert dv < 5e-4, "vision graph diverges from torch"

embeds = se.run(None, {"input_ids": ids.numpy()})[0]
image_token = model.config.image_token_id
positions = (ids[0] == image_token).nonzero(as_tuple=True)[0].numpy()
embeds[0, positions] = feats.reshape(-1, feats.shape[-1])

toks = []
pk = np.zeros((N_LAYERS, 1, N_KV, 0, HEAD_DIM), dtype=np.float32)
pvv = np.zeros((N_LAYERS, 1, N_KV, 0, HEAD_DIM), dtype=np.float32)
pos = np.arange(ids.shape[1], dtype=np.int64)[None, :]
x = embeds
for _ in range(N_CHECK):
    logits, pk, pvv = sd.run(
        None,
        {"inputs_embeds": x, "position_ids": pos, "past_k": pk, "past_v": pvv},
    )
    t = int(logits.argmax(-1)[0])
    toks.append(t)
    if t == model.config.eos_token_id:
        break
    x = se.run(None, {"input_ids": np.array([[t]], dtype=np.int64)})[0]
    pos = np.array([[pk.shape[3]]], dtype=np.int64)

assert toks == ref[: len(toks)], f"ONNX decode diverges:\n  ref {ref}\n  got {toks}"
print(f"  ONNX greedy tokens match transformers.generate: {toks}", flush=True)
print(f"wrote {OUT}/vision.onnx, embed.onnx, decoder_kv.onnx, tokenizer.json", flush=True)
