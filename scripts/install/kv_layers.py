#!/usr/bin/env python3
"""Rewrite the TableFormer `decoder_kv.onnx` into `decoder_kv_layers.onnx` (#871).

`decoder_kv.onnx` threads the self-attention cache as two stacked tensors,
`cache_k`/`cache_v` `[L, B, H, past, hd]` in and `out_cache_k`/`out_cache_v`
`[L, B, H, past+1, hd]` out. Every step then copies the whole cache several
times over: ONNX Runtime splits the stacked input into the six layers, each
layer concatenates its new token, K is transposed for q·Kᵀ, and the layers
are stacked and concatenated again into the outputs. Each of those buffers is
one token longer than the step before, and with the CPU arena off
(`DOCLING_RS_NO_ARENA`) each is a `malloc` — on linux/arm64 glibc reused so
few of them that a long table ran a 6 GiB container out of memory.

The rewritten graph takes the cache per layer, K already transposed:

    cache_kt_<i> [B, H, hd, past]   ->  out_cache_kt_<i> [B, H, hd, past+1]
    cache_v_<i>  [B, H, past, hd]   ->  out_cache_v_<i>  [B, H, past+1, hd]

so a step's only cache-sized work is the one `Concat` per layer and tensor
that appends the new token — and those `Concat`s are the graph outputs, which
the Rust loop binds to buffers it owns (two per tensor, used alternately), so
ONNX Runtime allocates nothing that grows with the table. The Split, the K
Transpose and the re-stacking are gone; the attention reads exactly the values
it read before (`Kᵀ` is the same floats in the same layout, scaled and
multiplied by the same `FusedMatMul`), so logits and hidden states are
bit-identical to `decoder_kv.onnx` — `--verify` checks that, step by step.

Only the graph changes: the weights stay in `decoder_kv.onnx.data`, which the
new file references, so it is a ~0.35 MB download next to the existing one.
Usage:

    python scripts/install/kv_layers.py .models/tableformer/decoder_kv.onnx \\
        [.models/tableformer/decoder_kv_layers.onnx] [--verify ENCODER.onnx]

The output must sit in the same directory as `decoder_kv.onnx.data`. The
script refuses (non-zero exit) when the graph does not have the expected
shape — a future export with a different cache layout is not silently broken.
"""
import os
import sys

import onnx
from onnx import helper, numpy_helper


def _const_int(g, name):
    for t in g.initializer:
        if t.name == name:
            return int(numpy_helper.to_array(t).reshape(-1)[0])
    for n in g.node:
        if n.op_type == "Constant" and n.output[0] == name:
            for a in n.attribute:
                if a.name == "value":
                    return int(numpy_helper.to_array(a.t).reshape(-1)[0])
    raise SystemExit(f"{name} is not a constant")


def _attr(n, name, default=None):
    for a in n.attribute:
        if a.name == name:
            return helper.get_attribute_value(a)
    return default


def rewrite(src, dst):
    model = onnx.load(src, load_external_data=False)
    g = model.graph
    consumers = {}
    for n in g.node:
        for i in n.input:
            consumers.setdefault(i, []).append(n)
    producer = {o: n for n in g.node for o in n.output}

    def only(name, what):
        cs = consumers.get(name, [])
        if len(cs) != 1:
            raise SystemExit(f"{name}: expected one consumer ({what}), found {len(cs)}")
        return cs[0]

    inputs = {i.name: i for i in g.input}
    for name in ("cache_k", "cache_v"):
        if name not in inputs:
            raise SystemExit(f"{src}: no `{name}` input — not a stacked-cache decoder_kv graph")
    dims = [d.dim_param or d.dim_value for d in inputs["cache_k"].type.tensor_type.shape.dim]
    n_layers, heads, head_dim = dims[0], dims[2], dims[4]
    if not (isinstance(n_layers, int) and isinstance(heads, int) and isinstance(head_dim, int)):
        raise SystemExit(f"{src}: cache_k {dims} — expected static [L, batch, H, past, hd]")

    rename = {}  # tensor name -> replacement, applied to every node input
    drop = set()  # node names
    new_nodes = []
    seen = {"cache_k": set(), "cache_v": set()}
    for n in g.node:
        if n.op_type != "Gather" or n.input[0] not in seen:
            continue
        stacked = n.input[0]
        if _attr(n, "axis", 0) != 0:
            raise SystemExit(f"{n.name}: Gather on {stacked} along a non-layer axis")
        layer = _const_int(g, n.input[1])
        seen[stacked].add(layer)
        cat = only(n.output[0], "the per-layer Concat")
        if cat.op_type != "Concat" or _attr(cat, "axis") != 2 or cat.input[0] != n.output[0]:
            raise SystemExit(f"{cat.name}: expected Concat(cache[{layer}], new, axis=2)")
        new_tok = cat.input[1]
        drop.update({n.name, cat.name})
        if stacked == "cache_v":
            rename[cat.output[0]] = f"out_cache_v_{layer}"
            new_nodes.append(
                helper.make_node(
                    "Concat", [f"cache_v_{layer}", new_tok], [f"out_cache_v_{layer}"],
                    name=f"kv_layers/cat_v_{layer}", axis=2,
                )
            )
            continue
        # K: cat -> Reshape [B*H, past+1, hd] -> Transpose(0,2,1) -> Reshape
        # [B, H, hd, past+1] -> the attention. The same tensor is now the
        # appended Kᵀ cache itself; the shape plumbing around it goes dead.
        reshape1 = [c for c in consumers[cat.output[0]] if c.op_type == "Reshape"]
        if len(reshape1) != 1:
            raise SystemExit(f"{cat.name}: expected one Reshape consumer")
        tr = only(reshape1[0].output[0], "the Kᵀ Transpose")
        if tr.op_type != "Transpose" or list(_attr(tr, "perm")) != [0, 2, 1]:
            raise SystemExit(f"{tr.name}: expected Transpose(perm=[0,2,1])")
        reshape2 = only(tr.output[0], "the Kᵀ Reshape")
        if reshape2.op_type != "Reshape":
            raise SystemExit(f"{reshape2.name}: expected Reshape back to 4-D")
        drop.update({reshape1[0].name, tr.name, reshape2.name})
        rename[reshape2.output[0]] = f"out_cache_kt_{layer}"
        new_nodes += [
            helper.make_node(
                "Transpose", [new_tok], [f"kv_layers/kt_new_{layer}"],
                name=f"kv_layers/kt_new_{layer}", perm=[0, 1, 3, 2],
            ),
            helper.make_node(
                "Concat", [f"cache_kt_{layer}", f"kv_layers/kt_new_{layer}"],
                [f"out_cache_kt_{layer}"], name=f"kv_layers/cat_kt_{layer}", axis=3,
            ),
        ]
    for name, layers in seen.items():
        if layers != set(range(n_layers)):
            raise SystemExit(f"{src}: {name} gathered for layers {sorted(layers)}, expected 0..{n_layers - 1}")

    # The positional encoding reads `past` off the stacked cache's shape
    # (`Shape(cache_*, start=3, end=4)`); layer 0's V cache has it on axis 2.
    for n in g.node:
        if n.op_type == "Shape" and n.input[0] in ("cache_k", "cache_v"):
            start, end = _attr(n, "start", 0), _attr(n, "end", None)
            if start < 1 or end is None:
                raise SystemExit(f"{n.name}: Shape over the layer axis of {n.input[0]}")
            n.input[0] = "cache_v_0"
            del n.attribute[:]
            n.attribute.extend(
                [helper.make_attribute("start", start - 1), helper.make_attribute("end", end - 1)]
            )

    nodes = [n for n in g.node if n.name not in drop] + new_nodes
    for n in nodes:
        for k, i in enumerate(n.input):
            if i in rename and not n.name.startswith("kv_layers/"):
                n.input[k] = rename[i]

    out_names = []
    for o in g.output:
        if o.name not in ("out_cache_k", "out_cache_v"):
            out_names.append(o)
    f32 = onnx.TensorProto.FLOAT
    new_inputs, new_outputs = [], []
    for i in range(n_layers):
        new_inputs.append(helper.make_tensor_value_info(f"cache_kt_{i}", f32, ["batch", heads, head_dim, "past"]))
    for i in range(n_layers):
        new_inputs.append(helper.make_tensor_value_info(f"cache_v_{i}", f32, ["batch", heads, "past", head_dim]))
    for i in range(n_layers):
        new_outputs.append(helper.make_tensor_value_info(f"out_cache_kt_{i}", f32, ["batch", heads, head_dim, "past_1"]))
    for i in range(n_layers):
        new_outputs.append(helper.make_tensor_value_info(f"out_cache_v_{i}", f32, ["batch", heads, "past_1", head_dim]))
    kept_inputs = [i for i in g.input if i.name not in ("cache_k", "cache_v")]
    graph_inputs = kept_inputs[:1] + new_inputs + kept_inputs[1:]

    # Dead-code elimination: the shape plumbing of the dropped K chain and the
    # re-stacking of the old outputs feed nothing any more.
    live = {o.name for o in out_names} | {o.name for o in new_outputs}
    changed = True
    while changed:
        changed = False
        needed = set(live)
        for n in nodes:
            needed.update(n.input)
        keep = [n for n in nodes if any(o in needed for o in n.output)]
        if len(keep) != len(nodes):
            nodes, changed = keep, True
    # Topological order: the appended nodes must precede their consumers.
    available = {i.name for i in graph_inputs} | {t.name for t in g.initializer}
    ordered, pending = [], list(nodes)
    while pending:
        rest = []
        for n in pending:
            if all((not i) or i in available for i in n.input):
                ordered.append(n)
                available.update(n.output)
            else:
                rest.append(n)
        if len(rest) == len(pending):
            raise SystemExit(f"cannot order {[n.name for n in rest[:5]]}")
        pending = rest
    used = {i for n in ordered for i in n.input}
    initializers = [t for t in g.initializer if t.name in used]

    del g.node[:]
    g.node.extend(ordered)
    del g.input[:]
    g.input.extend(graph_inputs)
    del g.output[:]
    g.output.extend(out_names + new_outputs)
    del g.initializer[:]
    g.initializer.extend(initializers)
    # Keep the exporter's intermediate shapes: ONNX Runtime's fusions need
    # them (without, Add + LayerNormalization stay unfused and the residual
    # stream rounds differently from `decoder_kv.onnx`).
    alive = {o for n in ordered for o in n.output}
    kept_info = [v for v in g.value_info if v.name in alive and v.name not in rename]
    del g.value_info[:]
    g.value_info.extend(kept_info)
    onnx.save(model, dst)
    onnx.checker.check_model(dst)
    return n_layers


def verify(old_path, new_path, encoder_path, steps=1024):
    """Roll both graphs greedily from <start> over a real encoder memory, one
    table alone and two tables batched: logits and hidden states must be
    bit-identical at every step, and the per-layer caches must equal the
    stacked ones (K transposed)."""
    import numpy as np
    import onnxruntime as ort

    def sess(p):
        so = ort.SessionOptions()
        so.intra_op_num_threads = 1  # the Rust decoder's setting
        so.enable_mem_pattern = False
        return ort.InferenceSession(p, so, providers=["CPUExecutionProvider"])

    enc, old, new = sess(encoder_path), sess(old_path), sess(new_path)
    n_layers = sum(1 for i in new.get_inputs() if i.name.startswith("cache_kt_"))
    cross_names = [i.name for i in old.get_inputs() if i.name.startswith("cross_")]
    enc_out = {o.name: k for k, o in enumerate(enc.get_outputs())}
    rng = np.random.default_rng(871)
    mems = []
    for _ in range(2):
        img = rng.standard_normal((1, 3, 448, 448)).astype(np.float32)
        res = enc.run(None, {"image": img})
        mems.append({n: res[enc_out[n]] for n in cross_names})
    start = 2  # wordmap.json's <start>
    for rows in ([0], [1], [0, 1]):
        b = len(rows)
        cross = {n: np.concatenate([mems[r][n] for r in rows], 0) for n in cross_names}
        heads, head_dim = new.get_inputs()[1].shape[1:3]  # cache_kt_0 [B, H, hd, past]
        ck = np.zeros((n_layers, b, heads, 0, head_dim), np.float32)
        cv = ck.copy()
        kts = [np.zeros((b, heads, head_dim, 0), np.float32) for _ in range(n_layers)]
        vs = [np.zeros((b, heads, 0, head_dim), np.float32) for _ in range(n_layers)]
        tag = np.full((b, 1), start, np.int64)
        for step in range(steps):
            lo, ho, ck, cv = old.run(None, {"tag": tag, "cache_k": ck, "cache_v": cv, **cross})
            feeds = {"tag": tag, **cross}
            feeds.update({f"cache_kt_{i}": kts[i] for i in range(n_layers)})
            feeds.update({f"cache_v_{i}": vs[i] for i in range(n_layers)})
            out = new.run(None, feeds)
            ln, hn = out[0], out[1]
            kts, vs = out[2 : 2 + n_layers], out[2 + n_layers : 2 + 2 * n_layers]
            if not (np.array_equal(lo, ln) and np.array_equal(ho, hn)):
                raise SystemExit(
                    f"rows {rows}, step {step}: logits/hidden differ "
                    f"(max |d| {np.abs(lo - ln).max():.3e} / {np.abs(ho - hn).max():.3e})"
                )
            if step % 128 == 0 or step == steps - 1:
                for i in range(n_layers):
                    if not (
                        np.array_equal(ck[i].transpose(0, 1, 3, 2), kts[i])
                        and np.array_equal(cv[i], vs[i])
                    ):
                        raise SystemExit(f"rows {rows}, step {step}: layer {i} cache differs")
            # Greedy, but never stopping at <end>: the whole `past` range is
            # exercised, as on the longest table the loop allows.
            tag = lo.argmax(1)[:, None].astype(np.int64)
        print(f"  rows {rows}: {steps} steps bit-identical (logits, hidden, caches)")


if __name__ == "__main__":
    args = sys.argv[1:]
    encoder = None
    if "--verify" in args:
        k = args.index("--verify")
        encoder = args[k + 1]
        del args[k : k + 2]
    if not args:
        raise SystemExit(__doc__)
    src = args[0]
    dst = args[1] if len(args) > 1 else os.path.join(os.path.dirname(src), "decoder_kv_layers.onnx")
    layers = rewrite(src, dst)
    print(f"{dst}: per-layer K^T/V caches for {layers} layers ({os.path.getsize(dst) / 2**20:.2f} MiB graph)")
    if encoder:
        verify(src, dst, encoder)
