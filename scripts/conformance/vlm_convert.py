#!/usr/bin/env python3
"""Convert a PDF to Markdown through Python docling's **VlmPipeline** (#153).

The reference side of the VLM corpus comparison: docling drives the *same*
OpenAI-compatible endpoint the Rust `--pipeline vlm` uses (ApiVlmOptions,
DOCTAGS response format), so the model and server are held constant and the
diff isolates what #153 is after — page rendering, DocTags parsing and
document assembly differences between the two implementations. (One known,
accepted asymmetry: each side renders pages with its own scale/DPI, which can
nudge the model's output; triage such diffs as "render", not "parser".)

Usage:
    vlm_convert.py --endpoint http://localhost:8000/v1 --model granite-docling \
                   <input.pdf> [output.md]
"""

import argparse
import sys
from pathlib import Path

from docling.datamodel.base_models import InputFormat

# ApiVlmOptions moved between docling minor versions; support both homes.
try:
    from docling.datamodel.pipeline_options_vlm_model import (
        ApiVlmOptions,
        ResponseFormat,
    )
except ImportError:  # older docling
    from docling.datamodel.pipeline_options import ApiVlmOptions, ResponseFormat

from docling.datamodel.pipeline_options import VlmPipelineOptions
from docling.document_converter import DocumentConverter, PdfFormatOption
from docling.pipeline.vlm_pipeline import VlmPipeline


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--endpoint", required=True, help="…/v1 base or full …/chat/completions URL")
    ap.add_argument("--model", required=True)
    ap.add_argument("--max-tokens", type=int, default=8192)
    ap.add_argument("input")
    ap.add_argument("output", nargs="?")
    args = ap.parse_args()

    url = args.endpoint.rstrip("/")
    if not url.endswith("/chat/completions"):
        url = f"{url}/chat/completions"

    vlm_options = ApiVlmOptions(
        url=url,
        params={"model": args.model, "max_tokens": args.max_tokens, "temperature": 0},
        prompt="Convert this page to docling.",
        timeout=600,
        response_format=ResponseFormat.DOCTAGS,
    )
    pipeline_options = VlmPipelineOptions(vlm_options=vlm_options, enable_remote_services=True)
    converter = DocumentConverter(
        format_options={
            InputFormat.PDF: PdfFormatOption(
                pipeline_cls=VlmPipeline, pipeline_options=pipeline_options
            )
        }
    )
    md = converter.convert(Path(args.input)).document.export_to_markdown()
    if args.output:
        Path(args.output).write_text(md + ("\n" if not md.endswith("\n") else ""))
    else:
        sys.stdout.write(md)
    return 0


if __name__ == "__main__":
    sys.exit(main())
