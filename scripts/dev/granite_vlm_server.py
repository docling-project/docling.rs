#!/usr/bin/env python3
"""Minimal OpenAI-compatible server for granite-docling — DocTags intact.

Why this exists: every llama.cpp-based server (Ollama, LM Studio) and default
vLLM sampling detokenize model output with special tokens stripped — and
granite-docling's DocTags *structure* tags (<text>, <section_header_level_N>,
<otsl>, ...) are special tokens in its vocabulary, so what reaches the client
is bare text plus <loc_N> noise. This shim runs the model through
`transformers` and decodes with `skip_special_tokens=False`, which is the
whole point; it exists for testing docling.rs's `--pipeline vlm` (issue #77)
and for corpus comparison against Python docling's VLM pipeline, not for
production serving (single-threaded, one request at a time).

Usage:
    pip install torch transformers pillow accelerate
    python scripts/dev/granite_vlm_server.py [--port 8000] [--model ID] [--cpu]

Then:
    docling-rs --pipeline vlm --vlm-endpoint http://localhost:8000/v1 \
               --vlm-model granite-docling <file.pdf>
"""

import argparse
import base64
import io
import json
import re
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

DEFAULT_MODEL = "ibm-granite/granite-docling-258M"


def load(model_id: str, force_cpu: bool):
    import torch
    from transformers import AutoProcessor

    # transformers v5 renamed AutoModelForVision2Seq; support both names so
    # the shim runs on whatever the venv has.
    try:
        from transformers import AutoModelForImageTextToText as AutoVlm
    except ImportError:
        from transformers import AutoModelForVision2Seq as AutoVlm

    device = "cpu" if force_cpu or not torch.cuda.is_available() else "cuda"
    dtype = torch.bfloat16 if device == "cuda" else torch.float32
    print(f"loading {model_id} on {device} ({dtype}) ...", flush=True)
    processor = AutoProcessor.from_pretrained(model_id)
    model = AutoVlm.from_pretrained(model_id, torch_dtype=dtype).to(device)
    model.eval()
    print("ready", flush=True)
    return processor, model, device


def generate(processor, model, device, image, prompt: str, max_new_tokens: int) -> str:
    import torch
    from PIL import Image

    img = Image.open(io.BytesIO(image)).convert("RGB")
    messages = [
        {
            "role": "user",
            "content": [{"type": "image"}, {"type": "text", "text": prompt}],
        }
    ]
    templated = processor.apply_chat_template(messages, add_generation_prompt=True)
    inputs = processor(text=templated, images=[img], return_tensors="pt").to(device)
    with torch.no_grad():
        out = model.generate(**inputs, max_new_tokens=max_new_tokens, do_sample=False)
    generated = out[:, inputs["input_ids"].shape[1] :]
    # skip_special_tokens=False keeps the DocTags structure tags; only the
    # chat-template terminators are ours to remove.
    text = processor.batch_decode(generated, skip_special_tokens=False)[0]
    return re.sub(r"<\|[a-z_]+\|>", "", text).strip()


class Handler(BaseHTTPRequestHandler):
    processor = None
    model = None
    device = "cpu"

    def do_POST(self):  # noqa: N802 (http.server API)
        if not self.path.endswith("/chat/completions"):
            self.send_error(404)
            return
        length = int(self.headers.get("content-length", 0))
        req = json.loads(self.rfile.read(length))
        prompt, image = "Convert this page to docling.", None
        for part in req.get("messages", [{}])[-1].get("content", []):
            if part.get("type") == "text":
                prompt = part["text"]
            elif part.get("type") == "image_url":
                url = part["image_url"]["url"]
                image = base64.b64decode(url.split(",", 1)[1])
        if image is None:
            self.send_error(400, "no image_url part")
            return
        started = time.time()
        text = generate(
            self.processor,
            self.model,
            self.device,
            image,
            prompt,
            int(req.get("max_tokens", 8192)),
        )
        print(f"page converted in {time.time() - started:.1f}s, {len(text)} chars", flush=True)
        payload = json.dumps(
            {
                "object": "chat.completion",
                "model": req.get("model", "granite-docling"),
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": text},
                        "finish_reason": "stop",
                    }
                ],
            }
        ).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):  # quiet the default per-request line
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--model", default=DEFAULT_MODEL)
    ap.add_argument("--cpu", action="store_true", help="force CPU inference")
    ap.add_argument(
        "--warmup",
        action="store_true",
        help="run one dummy generation at startup so the first real page "
        "doesn't pay the kernel-compilation cost (useful before corpus runs)",
    )
    args = ap.parse_args()
    # Persist triton's JIT kernel cache across restarts — without it every
    # server start (and on some setups every request) recompiles, which
    # dominated per-page latency in the #77 bring-up.
    import os

    os.environ.setdefault(
        "TRITON_CACHE_DIR",
        os.path.expanduser("~/.cache/docling-rs/triton"),
    )
    Handler.processor, Handler.model, Handler.device = load(args.model, args.cpu)
    if args.warmup:
        from PIL import Image

        buf = io.BytesIO()
        Image.new("RGB", (64, 64), "white").save(buf, format="PNG")
        started = time.time()
        generate(
            Handler.processor,
            Handler.model,
            Handler.device,
            buf.getvalue(),
            "Convert this page to docling.",
            8,
        )
        print(f"warmup done in {time.time() - started:.1f}s", flush=True)
    print(f"serving on http://127.0.0.1:{args.port}/v1/chat/completions", flush=True)
    HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
