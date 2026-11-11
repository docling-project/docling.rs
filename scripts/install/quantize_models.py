#!/usr/bin/env python3
"""Quantize the PDF-pipeline ONNX models to INT8 for faster CPU inference.

Two quantizations, both validated against the PDF corpus (see
docs/PERFORMANCE.md for the measured speed/quality numbers):

* **layout** — static QDQ INT8 of the RT-DETR layout model, **Conv ops only**
  (the HGNetv2 backbone). Calibrated on real corpus pages rendered exactly the
  way `layout.rs::predict` preprocesses them. The transformer decoder and
  detection heads stay fp32: quantizing their MatMuls shifts class scores near
  the 0.3 threshold and visibly degrades output (headers demoted to text,
  page-footers leaking in), while conv-only keeps groundtruth conformance at
  fp32 level. ~2.4x faster layout inference on AVX-512-VNNI CPUs, 172 -> 68 MB.
  Weights are 7-bit (`reduce_range`), which costs nothing here and keeps the
  model correct on CPUs *without* VNNI — see `check_weight_range`. BatchNorm
  is folded into the conv weights first (`fold_conv_affine`; the quantizer's
  Q/DQ pairs otherwise block ONNX Runtime's runtime fusion and the backbone's
  110 normalizations run as fp32 passes — ~40% of int8 layout time), the two
  stem convs stay fp32 (`stem_convs`), and activation ranges come from a
  per-page moving average (`CalibMovingAverage`).

* **tableformer-encoder-fp16** — the TableFormer encoder with its weights
  stored as fp16 and cast back to fp32 at load (`encoder_fp16.onnx`, #374).
  Compute stays fp32, so this is a *size* lever (~103 → ~52 MiB after the
  export's zero attention masks are stripped), not a speed one; the only
  numeric change is fp16 rounding of the weights. Self-validates against the
  fp32 encoder on the calibration pages (cross-K/V and enc_out cosine ≥
  0.9999, relative L2 error ≤ 0.5 %) and deletes its output on failure; the real
  gate is the PDF snapshot corpus (`scripts/conformance/pdf_conformance.sh`
  with `DOCLING_TABLEFORMER_ENCODER` pointed at it).
* **tableformer-decoder** — dynamic INT8 (weights-only MatMul) of the
  legacy autoregressive tag decoder (~10% faster than its fp32 file,
  78 -> 50 MB). Byte-exactness is quantizer-environment-sensitive:
  redp5110's TOC decode has near-tie tokens that a re-quantization can
  flip (the currently shipped asset is corpus-exact; a fresh one drifted
  that single fixture) — always re-gate pdf_conformance.sh after
  re-quantizing and keep the previously validated asset if it drifts.
  Since #97 the Rust loop prefers the byte-exact fp32 decoder_kv over
  this file anyway, so it only serves setups without the KV export.

* **code-formula-decoder** — dynamic INT8 of the CodeFormulaV2 KV-cache
  decoder step (the enrichment VLM; needs the --enrich models). Not in the
  default target list because the fp32 export is opt-in too. ~655 -> ~165 MB
  (4x less decoder RAM). NEAR-exact, not byte-exact: greedy decoding has
  occasional near-tie tokens the weight rounding can flip - on the
  conformance fixture the only drift is one extra blank line inside the
  code block (per-channel and fp32-lm_head variants flip it identically,
  so per-tensor is kept for the smaller file). DOCLING_RS_FP32=1 restores
  the byte-exact fp32 decoder.

Usage (from the repo root, models fetched by scripts/install/download_dependencies.sh):

    uv venv .venv-quant && uv pip install --python .venv-quant/bin/python \
        onnx onnxruntime sympy pypdfium2 pillow numpy
    .venv-quant/bin/python scripts/install/quantize_models.py layout tableformer-decoder

Then point the pipeline at the quantized files:

    export DOCLING_LAYOUT_ONNX=$PWD/.models/layout_heron_int8.onnx
    export DOCLING_TABLEFORMER_DECODER=$PWD/.models/tableformer/decoder_int8.onnx

Re-run scripts/conformance/pdf_conformance.sh (or diff Markdown against
tests/data/pdf/groundtruth) after re-quantizing to re-verify quality.
"""

import glob
import os
import sys

import numpy as np

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
# Where the fp32 models live and the int8 outputs go (a checkout's .models/ by
# default); DOCLING_RS_MODELS_DIR relocates it (e.g. /opt/models in a Docker
# models stage).
MODELS = os.environ.get("DOCLING_RS_MODELS_DIR", f"{REPO}/.models")
# DOCLING_RS_CALIBRATION_DIR: a directory scanned recursively for calibration
# *.pdf files; defaults to the repo's PDF + scanned corpus (the set the
# published quality numbers were measured with).
CALIB = os.environ.get("DOCLING_RS_CALIBRATION_DIR")
SIDE = 640  # layout model input side (layout.rs)


def calibration_pages(side=SIDE):
    """Render up to 3 pages of every calibration PDF the way layout.rs
    preprocesses: pdfium at scale 2.0, resize to `side`×`side` bilinear (640
    for the layout model, 448 for the TableFormer encoder), /255, CHW
    float32."""
    import pypdfium2 as pdfium
    from PIL import Image

    if CALIB:
        pdfs = sorted(glob.glob(f"{CALIB}/**/*.pdf", recursive=True))
    else:
        pdfs = sorted(glob.glob(f"{REPO}/tests/data/pdf/sources/*.pdf")) + sorted(
            glob.glob(f"{REPO}/tests/data/scanned/sources/*.pdf")
        )
    if not pdfs:
        sys.exit("no calibration PDFs found (set DOCLING_RS_CALIBRATION_DIR)")
    for path in pdfs:
        try:
            doc = pdfium.PdfDocument(path)
        except Exception:
            continue
        for i in range(min(3, len(doc))):
            bmp = doc[i].render(scale=2.0)
            img = bmp.to_pil().convert("RGB").resize((side, side), Image.BILINEAR)
            arr = np.asarray(img, dtype=np.float32) / 255.0
            yield np.transpose(arr, (2, 0, 1))[None, ...]
        doc.close()


def quantize_layout():
    from onnxruntime.quantization import (
        CalibrationDataReader,
        QuantFormat,
        QuantType,
        quantize_static,
    )
    from onnxruntime.quantization.shape_inference import quant_pre_process

    src = f"{MODELS}/layout_heron.onnx"
    pre = f"{MODELS}/layout_heron_pre.onnx"
    dst = f"{MODELS}/layout_heron_int8.onnx"

    class Reader(CalibrationDataReader):
        def __init__(self):
            self.data = [{"pixel_values": x} for x in calibration_pages()]
            print(f"layout: {len(self.data)} calibration samples", flush=True)
            self.it = iter(self.data)

        def get_next(self):
            return next(self.it, None)

    print("layout: pre-processing (shape inference)...", flush=True)
    # skip_symbolic_shape: the #73 dynamic-batch graph makes ORT's symbolic
    # shape inference bail ("Incomplete symbolic shape inference"); the
    # ONNX-level inference quant_pre_process falls back to is enough for the
    # Conv-only QDQ pass.
    quant_pre_process(src, pre, skip_symbolic_shape=True)
    folded = fold_conv_affine(pre)
    print(f"layout: folded {folded} BatchNorm Mul/Add pairs into their Conv weights", flush=True)
    stem = stem_convs(pre)
    print(f"layout: keeping {len(stem)} stem convs in fp32", flush=True)
    print("layout: static QDQ INT8 quantization (Conv only)...", flush=True)
    quantize_static(
        pre,
        dst,
        Reader(),
        quant_format=QuantFormat.QDQ,
        activation_type=QuantType.QUInt8,
        weight_type=QuantType.QInt8,
        per_channel=True,
        # 7-bit weights. u8s8 convolutions run through VPMADDUBSW on CPUs
        # without VNNI, whose int16 pair accumulator saturates at 32767:
        # full-range weights reach 255*127*2 = 64770 and silently clip, which
        # is what wrecked a publish run (83 of 505 confident detections lost,
        # two of them tables, on a runner this recipe passes on elsewhere).
        # 7 bits caps the pair product at 255*64*2 = 32640 — below the
        # saturation point, so the model behaves the same on every x86 CPU.
        reduce_range=True,
        # Conv-only: the RT-DETR decoder/head MatMuls are threshold-sensitive.
        op_types_to_quantize=["Conv"],
        nodes_to_exclude=stem,
        # Activation ranges as the moving average of per-page min/max rather
        # than the corpus-wide extremes: a single outlier page no longer sets
        # the u8 grid for everyone. Measured against fp32 over the calibration
        # pages (BN-folded graph, 1045 fp32 detections above the pipeline
        # threshold): mean |score delta| 0.051 -> 0.037, detections lost
        # 48 -> 28, p95 delta 0.19 -> 0.14 — also better than the previous
        # unfolded recipe on every one of those (0.049 / 38 / 0.17). Entropy
        # and percentile calibration were tried on the same graph and landed
        # in between (0.046 / 0.043), on top of needing all activation samples
        # in RAM, which the full 52-page set does not fit in 15 GB.
        extra_options={"CalibMovingAverage": True},
    )
    os.remove(pre)
    print(f"layout: done -> {dst} ({os.path.getsize(dst) / 1e6:.1f} MB)")
    check_weight_range(dst)
    validate_layout(src, dst)


def fold_conv_affine(path):
    """Fold `Conv -> Mul(c) -> Add(c)` (BatchNorm in eval mode, as the HGNetv2
    export spells it — per-channel constants of shape (1,C,1,1)) into the Conv's
    weights and bias, in place. Returns the number of Conv nodes folded.

    The fp32 session gets this for free: ONNX Runtime's ConvMulFusion /
    ConvAddFusion turn the triple into one FusedConv at load, so fp32 inference
    never executes the Mul/Add. The QDQ int8 graph does not: the quantizer
    wraps the Conv in Quantize/Dequantize pairs, those block the fusion, and
    every one of the backbone's 110 normalizations then runs as Dequantize ->
    Mul -> Add -> Quantize — four fp32 passes over the full activation tensor
    per conv, ~40% of int8 layout time on the ORT node profiler (Mul 13%, Add
    12%, DequantizeLinear 12%, QuantizeLinear 4%). Folding first hands the
    quantizer the same conv the fp32 path effectively runs, per-channel weight
    scales absorb the BatchNorm scales, and the graph collapses to back-to-back
    QLinearConvs. Algebra: (W*x)·s + t == (W·s)*x + t, exact up to fp32
    rounding of W·s — which is what the fp32 fusion computes as well."""
    import onnx
    from onnx import numpy_helper

    m = onnx.load(path)
    g = m.graph
    ini = {i.name: i for i in g.initializer}
    consumers = {}
    for n in g.node:
        for x in n.input:
            consumers.setdefault(x, []).append(n)

    def affine_const(node, conv_out_channels):
        # The non-activation input must be a per-channel constant.
        for x in node.input:
            if x in ini:
                arr = numpy_helper.to_array(ini[x]).astype(np.float32)
                if arr.size == conv_out_channels:
                    return arr.reshape(-1)
        return None

    folded = 0
    drop = set()
    for conv in [n for n in g.node if n.op_type == "Conv"]:
        if conv.input[1] not in ini:
            continue
        w = numpy_helper.to_array(ini[conv.input[1]]).astype(np.float32)
        c_out = w.shape[0]
        b = (
            numpy_helper.to_array(ini[conv.input[2]]).astype(np.float32)
            if len(conv.input) > 2 and conv.input[2] in ini
            else np.zeros(c_out, dtype=np.float32)
        )
        scale = np.ones(c_out, dtype=np.float32)
        shift = np.zeros(c_out, dtype=np.float32)
        tail = conv
        chain = []
        # Walk a single-consumer Mul then Add (either alone also folds).
        for op in ("Mul", "Add"):
            nxt = consumers.get(tail.output[0], [])
            if len(nxt) != 1 or nxt[0].op_type != op:
                continue
            k = affine_const(nxt[0], c_out)
            if k is None:
                break
            if op == "Mul":
                scale = scale * k
                shift = shift * k
            else:
                shift = shift + k
            chain.append(nxt[0])
            tail = nxt[0]
        if not chain:
            continue
        w_name = conv.input[1] + "_bnfold"
        b_name = conv.input[1] + "_bnfold_bias"
        g.initializer.append(
            numpy_helper.from_array((w * scale.reshape(-1, 1, 1, 1)).astype(np.float32), w_name)
        )
        g.initializer.append(numpy_helper.from_array((b * scale + shift).astype(np.float32), b_name))
        conv.input[1] = w_name
        if len(conv.input) > 2:
            conv.input[2] = b_name
        else:
            conv.input.append(b_name)
        # The Conv now produces what the chain's last node produced.
        conv.output[0] = tail.output[0]
        drop.update(id(n) for n in chain)
        folded += 1
    if folded:
        keep = [n for n in g.node if id(n) not in drop]
        del g.node[:]
        g.node.extend(keep)
        onnx.save(m, path)
    return folded


def stem_convs(path):
    """The two stem convolutions (`embedder.0`, `embedder.1`) stay fp32.

    Quantization error concentrates at the front of the backbone: those two
    convs see the raw 640×640 pixels and one ReLU later, at 320×320 the
    largest activation maps in the graph, and their u8 rounding rides through
    every later stage. Leaving just the two of them in fp32 takes the int8
    model's mean |score delta| against fp32 over the calibration pages from
    0.037 to 0.022 and the lost detections from 28 to 15 (p95 delta 0.14 ->
    0.06) — nearly all of what keeping the whole stem (embedder.2 too) buys
    (0.020 / 17) — for an inference cost inside the run-to-run noise on the
    ORT bench (the third conv already halves the map, so it stays int8).
    Matched by node name so the list follows the export."""
    import onnx

    return [
        n.name
        for n in onnx.load(path).graph.node
        if n.op_type == "Conv"
        and ("embedder.0/convolution" in n.name or "embedder.1/convolution" in n.name)
    ]


# u8 activations times 7-bit weights, two lanes per VPMADDUBSW int16 slot.
SATURATION_SAFE_MAX = 64  # 255 * 64 * 2 = 32640 <= int16 max (32767)


def check_weight_range(dst):
    """Portability gate: every quantized weight must stay within 7 bits.

    The accuracy gate below only proves the model right on the machine that
    ran it — int8 convolutions take a different kernel per ISA, and the AVX2
    one (no VNNI) accumulates u8*s8 products pairwise in int16. A full-range
    weight saturates there, so a model that looks perfect on the quantizing
    box can lose whole detections on a plain AVX2 CPU. This check is
    hardware-independent: it reads the weights, so it fails on the publishing
    machine rather than on a user's laptop."""
    import onnx
    from onnx import numpy_helper

    over = []
    for init in onnx.load(dst).graph.initializer:
        arr = numpy_helper.to_array(init)
        if arr.dtype == np.int8 and arr.size:
            peak = int(np.abs(arr).max())
            if peak > SATURATION_SAFE_MAX:
                over.append((init.name, peak))
    if over:
        os.remove(dst)
        worst = max(p for _, p in over)
        sys.exit(
            f"layout: {len(over)} weight tensor(s) exceed {SATURATION_SAFE_MAX}"
            f" (worst |w| = {worst}, e.g. {over[0][0]}) — int16 saturation on"
            f" non-VNNI CPUs would corrupt inference; {dst} deleted."
            " Quantize with reduce_range=True."
        )
    print("layout: weights within 7 bits — no int16 saturation on any x86 CPU")


# Mirror of layout.rs::LABELS — for the gate's reporting and the table check.
LAYOUT_LABELS = [
    "caption", "footnote", "formula", "list_item", "page_footer",
    "page_header", "picture", "section_header", "table", "text", "title",
    "document_index", "code", "checkbox_selected", "checkbox_unselected",
    "form", "key_value_region",
]
TABLE = LAYOUT_LABELS.index("table")


def _layout_detections(logits, boxes, score_min):
    """Decode one page the way layout.rs does: sigmoid over every
    (query, class), keep the top num_queries scores, threshold, and convert
    cxcywh -> xyxy (normalized). Returns [(class_id, score, (l, t, r, b))]."""
    q, c = logits.shape
    scores = 1.0 / (1.0 + np.exp(-logits.reshape(-1)))
    top = np.argsort(-scores)[:q]
    dets = []
    for idx in top:
        s = float(scores[idx])
        if s <= score_min:
            continue
        qi, ci = divmod(int(idx), c)
        cx, cy, w, h = boxes[qi]
        dets.append((ci, s, (cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2)))
    return dets


def _iou(a, b):
    il = max(a[0], b[0])
    it = max(a[1], b[1])
    ir = min(a[2], b[2])
    ib = min(a[3], b[3])
    inter = max(0.0, ir - il) * max(0.0, ib - it)
    if inter == 0.0:
        return 0.0
    area = lambda r: max(0.0, r[2] - r[0]) * max(0.0, r[3] - r[1])  # noqa: E731
    return inter / (area(a) + area(b) - inter)


def validate_layout(src, dst):
    """Agreement gate: every confident fp32 detection (score >= 0.6) must
    survive quantization — an int8 detection of the same class with IoU >= 0.5
    at the pipeline's base threshold (0.3). Zero tolerance for lost *tables*
    (a dropped table silently degrades to `<!-- image -->` downstream — the
    exact regression this gate exists to stop) and <= 2% for the rest.
    On failure the int8 file is DELETED so a publish run stages fp32 only
    (download_dependencies.sh falls back gracefully — int8 is fetch_optional).

    Machine-dependent by construction: it runs both graphs on *this* CPU, so
    it catches quantization error but not ISA-specific kernel behaviour —
    that is what check_weight_range covers."""
    import onnxruntime as ort

    for path, kind in ((src, "fp32"), (dst, "int8")):
        if not os.path.exists(path):
            sys.exit(
                f"layout gate: {path} not found — nothing to validate. "
                + (
                    "Fetch the models first (scripts/install/download_dependencies.sh)."
                    if kind == "fp32"
                    else "No int8 model on disk: the release ships fp32-only when a "
                    "previous gate failed; build one with "
                    "`python scripts/install/quantize_models.py layout` (it re-runs "
                    "this gate on the result)."
                )
            )

    print("layout: validating int8 against fp32 (agreement gate)...", flush=True)
    opts = ort.SessionOptions()
    ref = ort.InferenceSession(src, opts, providers=["CPUExecutionProvider"])
    qnt = ort.InferenceSession(dst, opts, providers=["CPUExecutionProvider"])

    total = missed = tables_missed = 0
    for page_no, x in enumerate(calibration_pages()):
        rl, rb = ref.run(["logits", "pred_boxes"], {"pixel_values": x})
        ql, qb = qnt.run(["logits", "pred_boxes"], {"pixel_values": x})
        ref_dets = _layout_detections(rl[0], rb[0], 0.6)
        qnt_dets = _layout_detections(ql[0], qb[0], 0.3)
        for ci, s, bb in ref_dets:
            total += 1
            if any(cj == ci and _iou(bb, b2) >= 0.5 for cj, _, b2 in qnt_dets):
                continue
            missed += 1
            tables_missed += ci == TABLE
            print(
                f"layout gate: page {page_no}: lost {LAYOUT_LABELS[ci]}"
                f" (fp32 score {s:.2f}, box {tuple(round(v, 3) for v in bb)})"
            )

    rate = missed / total if total else 1.0
    print(
        f"layout gate: {total} confident fp32 detections,"
        f" {missed} lost by int8 ({rate:.2%}), {tables_missed} tables"
    )
    if total == 0 or tables_missed > 0 or rate > 0.02:
        os.remove(dst)
        sys.exit(
            f"layout gate FAILED — {dst} deleted"
            " (int8 disagrees with fp32; the fp32 model remains usable)"
        )
    print("layout gate: PASSED")


def quantize_tableformer_decoder():
    import onnx
    from onnxruntime.quantization import QuantType, quantize_dynamic

    # Quantize the legacy layer-output-cache decoder only. The #97 hoisted-KV
    # decoder_kv.onnx is deliberately NOT quantized: weights-only INT8 of that
    # graph drifts the heavy-table fixtures off the fp32 snapshots (redp5110's
    # TOC decode flips even with per-channel scales; 2206 flips per-tensor),
    # and the measured gain over its fp32 file was only ~2.5% — the Rust loop
    # prefers the byte-exact fp32 decoder_kv instead.
    for stem in ("decoder",):
        src = f"{MODELS}/tableformer/{stem}.onnx"
        if not os.path.exists(src):
            continue
        tmp = f"{MODELS}/tableformer/{stem}_clean.onnx"
        dst = f"{MODELS}/tableformer/{stem}_int8.onnx"

        # The export carries stale value_info shapes that break ORT's shape
        # inference; strip them (external weights get folded into the output).
        m = onnx.load(src)
        del m.graph.value_info[:]
        onnx.save(m, tmp, save_as_external_data=True, location=f"{stem}_clean.onnx.data")
        print(f"tableformer-decoder: dynamic INT8 quantization ({stem})...", flush=True)
        quantize_dynamic(
            tmp,
            dst,
            weight_type=QuantType.QInt8,
            extra_options={"MatMulConstBOnly": True},
        )
        os.remove(tmp)
        os.remove(f"{tmp}.data")
        print(f"tableformer-decoder: done -> {dst} ({os.path.getsize(dst) / 1e6:.1f} MB)")


def repack_tableformer_encoder_fp16():
    """`encoder.onnx` → `encoder_fp16.onnx`: every fp32 weight tensor of at
    least 4096 elements is stored as fp16 behind a `Cast(to=FLOAT)`, so ORT
    folds it back to fp32 at session creation and the graph computes exactly
    as before — only the file (and the download) shrinks, ~2×. Small tensors
    (biases, LayerNorm scales) stay fp32: they cost nothing and their rounding
    would be pure noise. The exporter's baked zero attention masks are
    stripped first (strip_zero_masks.py), so the result is small even from a
    pre-#374 encoder."""
    import onnx
    import onnxruntime as ort
    from onnx import helper, numpy_helper

    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from strip_zero_masks import strip_zero_mask_adds

    src = f"{MODELS}/tableformer/encoder.onnx"
    dst = f"{MODELS}/tableformer/encoder_fp16.onnx"
    if not os.path.exists(src):
        sys.exit(f"tableformer-encoder-fp16: {src} not found")
    removed, freed = strip_zero_mask_adds(src, dst)
    if removed:
        print(f"tableformer-encoder-fp16: stripped {removed} zero-mask Add(s), {freed / 2**20:.1f} MiB", flush=True)

    MIN_ELEMS = 4096
    model = onnx.load(dst)
    g = model.graph
    converted = 0
    nodes = []
    for n in g.node:
        if n.op_type == "Constant":
            attr = next((a for a in n.attribute if a.name == "value"), None)
            if attr is not None and attr.t.data_type == onnx.TensorProto.FLOAT:
                arr = numpy_helper.to_array(attr.t)
                if arr.size >= MIN_ELEMS:
                    half = n.output[0] + "_fp16"
                    nodes.append(helper.make_node("Constant", [], [half], value=numpy_helper.from_array(arr.astype(np.float16), half)))
                    nodes.append(helper.make_node("Cast", [half], [n.output[0]], to=onnx.TensorProto.FLOAT))
                    converted += arr.nbytes // 2
                    continue
        nodes.append(n)
    casts = []
    for t in list(g.initializer):
        if t.data_type == onnx.TensorProto.FLOAT:
            arr = numpy_helper.to_array(t)
            if arr.size >= MIN_ELEMS:
                half = t.name + "_fp16"
                g.initializer.remove(t)
                g.initializer.append(numpy_helper.from_array(arr.astype(np.float16), half))
                casts.append(helper.make_node("Cast", [half], [t.name], to=onnx.TensorProto.FLOAT))
                converted += arr.nbytes // 2
    del g.node[:]
    g.node.extend(casts + nodes)
    onnx.checker.check_model(model)
    onnx.save(model, dst)
    print(
        f"tableformer-encoder-fp16: {converted / 2**20:.1f} MiB of weights halved -> {dst} "
        f"({os.path.getsize(dst) / 1e6:.1f} MB vs {os.path.getsize(src) / 1e6:.1f} MB)",
        flush=True,
    )

    # Fidelity gate against the fp32 encoder: the calibration pages resized
    # to the encoder's 448×448 input plus two synthetic inputs. Cosine and
    # relative error over every output (cross K/V per layer, enc_out).
    so = ort.SessionOptions()
    so.intra_op_num_threads = max(1, (os.cpu_count() or 2) // 2)
    ref = ort.InferenceSession(src, so, providers=["CPUExecutionProvider"])
    out = ort.InferenceSession(dst, so, providers=["CPUExecutionProvider"])
    rng = np.random.default_rng(0)
    inputs = [rng.standard_normal((1, 3, 448, 448), dtype=np.float32) for _ in range(2)]
    inputs += list(calibration_pages(side=448))
    # Gate on the relative L2 error per output tensor (‖a−b‖/‖a‖, the metric
    # that tracks how far the decoder's attention inputs moved) plus cosine;
    # the worst single element is reported for information only — fp16
    # rounding of one weight can move one element of a 3.2M-element tensor
    # by ~1 % of its range without the tensor moving at all.
    min_cos, max_rel_l2, max_elem = 1.0, 0.0, 0.0
    for x in inputs:
        for a, b in zip(ref.run(None, {"image": x}), out.run(None, {"image": x})):
            a = a.ravel().astype(np.float64)
            b = b.ravel().astype(np.float64)
            na = np.linalg.norm(a) + 1e-30
            min_cos = min(min_cos, float(a @ b / (na * (np.linalg.norm(b) + 1e-30))))
            max_rel_l2 = max(max_rel_l2, float(np.linalg.norm(a - b) / na))
            max_elem = max(max_elem, float(np.abs(a - b).max() / (np.abs(a).max() + 1e-30)))
    print(
        f"tableformer-encoder-fp16: {len(inputs)} inputs, min cosine {min_cos:.6f}, "
        f"max relative L2 error {max_rel_l2:.3g}, worst element {max_elem:.3g} of the tensor's range",
        flush=True,
    )
    if min_cos < 0.9999 or max_rel_l2 > 0.005:
        os.remove(dst)
        sys.exit("tableformer-encoder-fp16: fidelity gate FAILED — output deleted")


def quantize_code_formula_decoder():
    """Dynamic INT8 (weights-only MatMul) of the CodeFormulaV2 KV-cache decoder
    step — the autoregressive stage that dominates enrichment latency. Same
    recipe as the TableFormer decoder. Near-exact (see the module docstring);
    re-run scripts/conformance/enrich_conformance.sh after re-quantizing.
    ~655 -> ~165 MB."""
    import onnx
    from onnxruntime.quantization import QuantType, quantize_dynamic

    src = f"{MODELS}/code_formula/decoder_kv.onnx"
    if not os.path.exists(src):
        print(f"code-formula-decoder: {src} not found — skipping")
        return
    tmp = f"{MODELS}/code_formula/decoder_kv_fold.onnx"
    dst = f"{MODELS}/code_formula/decoder_kv_int8.onnx"

    # torch's exporter emits the Linear weights as `Constant` *nodes*, but
    # `MatMulConstBOnly` only quantizes MatMuls whose B is an *initializer* —
    # so fold every tensor-valued Constant into an initializer first (the
    # unfolded graph quantizes to a byte-identical no-op).
    m = onnx.load(src)
    keep = []
    for node in m.graph.node:
        t = next((a.t for a in node.attribute if a.name == "value"), None)
        if node.op_type == "Constant" and t is not None:
            t.name = node.output[0]
            m.graph.initializer.append(t)
        else:
            keep.append(node)
    del m.graph.node[:]
    m.graph.node.extend(keep)
    del m.graph.value_info[:]
    onnx.save(m, tmp, save_as_external_data=True, location="decoder_kv_fold.onnx.data")

    print("code-formula-decoder: dynamic INT8 quantization...", flush=True)
    quantize_dynamic(
        tmp, dst, weight_type=QuantType.QInt8, extra_options={"MatMulConstBOnly": True}
    )
    os.remove(tmp)
    os.remove(f"{tmp}.data")
    print(f"code-formula-decoder: done -> {dst} ({os.path.getsize(dst) / 1e6:.1f} MB)")


def main():
    targets = sys.argv[1:] or ["layout", "tableformer-decoder"]
    for t in targets:
        if t == "layout":
            quantize_layout()
        elif t == "validate-layout":
            # Gate an existing fp32/int8 pair without re-quantizing (e.g. to
            # vet already-downloaded release assets).
            validate_layout(f"{MODELS}/layout_heron.onnx", f"{MODELS}/layout_heron_int8.onnx")
        elif t == "tableformer-decoder":
            quantize_tableformer_decoder()
        elif t == "tableformer-encoder-fp16":
            repack_tableformer_encoder_fp16()
        elif t == "code-formula-decoder":
            quantize_code_formula_decoder()
        else:
            sys.exit(
                f"unknown target {t!r} "
                "(expected: layout, validate-layout, tableformer-decoder, "
                "tableformer-encoder-fp16, code-formula-decoder)"
            )


if __name__ == "__main__":
    main()
