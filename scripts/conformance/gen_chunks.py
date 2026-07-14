#!/usr/bin/env python3
"""Generate chunking groundtruth with live docling's chunkers.

For each corpus source, converts with the installed Python docling, then runs
docling-core's HierarchicalChunker and HybridChunker (default MiniLM tokenizer,
max_tokens 256) over the resulting DoclingDocument and dumps the chunks as JSON:

    tests/data/chunks/groundtruth/<source-name>.hierarchical.json
    tests/data/chunks/groundtruth/<source-name>.hybrid.json

Each file is a JSON array of chunk records:

    {"text": ..., "headings": [...] | null, "doc_items": ["#/texts/0", ...],
     "contextualize": ...}

Run inside the comparison venv (scripts/conformance/setup-docling.sh):

    .venv-compare/bin/python scripts/conformance/gen_chunks.py [source ...]

With no arguments, the default corpus (html + docx + md + xlsx + pptx sources)
is processed.
"""

import json
import sys
import warnings
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
OUT_DIR = REPO / "tests" / "data" / "chunks" / "groundtruth"

DEFAULT_GLOBS = [
    "tests/data/html/sources/*.html",
    "tests/data/docx/sources/*.docx",
    "tests/data/md/sources/*.md",
    "tests/data/xlsx/sources/*.xlsx",
    "tests/data/pptx/sources/*.pptx",
]


def chunk_records(chunker, doc):
    records = []
    for chunk in chunker.chunk(dl_doc=doc):
        records.append(
            {
                "text": chunk.text,
                "headings": chunk.meta.headings,
                "doc_items": [it.self_ref for it in chunk.meta.doc_items],
                "contextualize": chunker.contextualize(chunk=chunk),
            }
        )
    return records


def main() -> int:
    warnings.filterwarnings("ignore")
    from docling.document_converter import DocumentConverter
    from docling_core.transforms.chunker import HierarchicalChunker
    from docling_core.transforms.chunker.hybrid_chunker import HybridChunker

    sources = [Path(a) for a in sys.argv[1:]]
    if not sources:
        for pattern in DEFAULT_GLOBS:
            sources.extend(sorted(REPO.glob(pattern)))

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    converter = DocumentConverter()
    hierarchical = HierarchicalChunker()
    hybrid = HybridChunker()

    ok = failed = 0
    for src in sources:
        try:
            doc = converter.convert(src).document
            for name, chunker in [("hierarchical", hierarchical), ("hybrid", hybrid)]:
                records = chunk_records(chunker, doc)
                out = OUT_DIR / f"{src.name}.{name}.json"
                out.write_text(json.dumps(records, ensure_ascii=False, indent=1) + "\n")
            print(f"ok   {src.name}")
            ok += 1
        except Exception as e:  # noqa: BLE001 — report and continue
            print(f"FAIL {src.name}: {type(e).__name__}: {e}")
            failed += 1
    print(f"\n{ok} ok, {failed} failed -> {OUT_DIR}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
