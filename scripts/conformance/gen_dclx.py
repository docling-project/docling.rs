#!/usr/bin/env python3
"""Generate .dclx (DocLang OPC archive) groundtruth from the latest published
Python docling, for every source under tests/data/<fmt>/sources/.

Output lands next to the existing groundtruth as
``tests/data/<fmt>/groundtruth_dclx/<source-name>.dclx`` — the reference the
Rust `--to dclx` output is scored against (scripts/conformance/dclx_conformance.sh).

Declarative formats go through the format backend directly (no torch), the
PDF/image/audio formats through docling's full pipeline — the same split
scripts/conformance/docling_convert.py uses. Page images are NOT generated for PDFs (the
default pipeline keeps them off), so archives stay small and the comparison
focuses on document structure.

Usage:
    .venv-compare/bin/python scripts/conformance/gen_dclx.py [fmt ...]   # default: all
"""

from __future__ import annotations

import importlib.util
import sys
import traceback
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent

_spec = importlib.util.spec_from_file_location("dc", REPO / "scripts/conformance/docling_convert.py")
_dc = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_dc)

from docling.datamodel.base_models import InputFormat  # noqa: E402
from docling.datamodel.document import InputDocument  # noqa: E402

# Extensions docling_convert.py's map doesn't carry (it routes these through
# other code paths); .xml is sniffed into USPTO / XBRL / JATS below.
_EXTRA_EXT = {
    "xml": InputFormat.XML_JATS,  # placeholder — sniffed
    "tex": InputFormat.LATEX,
    "json": InputFormat.JSON_DOCLING,
    "gz": InputFormat.METS_GBS,
    "mhtml": InputFormat.HTML,
    "mht": InputFormat.HTML,
}
# Audio needs docling's ASR pipeline (multi-GB Whisper download) — out of scope
# for the structural conformance corpus; skipped with a note.
_SKIP_EXT = {"wav", "mp3", "flac", "ogg", "aac", "m4a", "mp4", "mov", "avi"}

_CONVERTER = None


def _converter():
    """A DocumentConverter for the pipeline formats, with OCR on RapidOCR
    (EasyOCR's default weight host is unreachable from CI-like environments;
    RapidOCR pulls from Hugging Face and matches docling.rs's OCR stack)."""
    global _CONVERTER
    if _CONVERTER is None:
        from docling.datamodel.pipeline_options import PdfPipelineOptions, RapidOcrOptions
        from docling.document_converter import DocumentConverter, PdfFormatOption

        opts = PdfPipelineOptions(ocr_options=RapidOcrOptions())
        _CONVERTER = DocumentConverter(
            format_options={
                InputFormat.PDF: PdfFormatOption(pipeline_options=opts),
                InputFormat.IMAGE: PdfFormatOption(pipeline_options=opts),
            }
        )
    return _CONVERTER


def _sniff_xml(path: Path) -> InputFormat:
    head = path.read_text(encoding="utf-8", errors="ignore")[:4000]
    if any(
        k in head
        for k in ("us-patent", "patent-application-publication", "PATDOC", "<pap-v1")
    ):
        return InputFormat.XML_USPTO
    if any(k in head for k in ("us-gaap", "xbrl", "dei:")):
        return InputFormat.XML_XBRL
    return InputFormat.XML_JATS


def build_document(path: Path):
    """The DoclingDocument for a source file, backend-direct where possible
    (mirrors scripts/conformance/docling_convert.py's routing, extended to every format)."""
    ext = path.suffix.lower().lstrip(".")
    if ext in _SKIP_EXT:
        raise ValueError("audio (ASR pipeline) — skipped for the dclx corpus")
    fmt = _dc.EXT_TO_FORMAT.get(ext) or _EXTRA_EXT.get(ext)
    if ext == "xml":
        fmt = _sniff_xml(path)
    elif ext == "txt" and path.read_text(encoding="utf-8", errors="ignore").startswith("PATN"):
        # docling's format detection: a text/plain file opening with a PATN
        # record is a legacy APS patent (`pftaps*.txt`), not Markdown.
        fmt = InputFormat.XML_USPTO
    if fmt is None:
        raise ValueError(f"unrecognized extension .{ext}")
    if fmt in (InputFormat.PDF, InputFormat.IMAGE, InputFormat.METS_GBS):
        return _converter().convert(path).document
    try:
        backend_cls = _dc.backend_for(fmt)
    except (Exception, SystemExit):
        backend_cls = None
    if backend_cls is None:
        # No lightweight backend wired for this format (JSON/LaTeX/XBRL/…):
        # let the full DocumentConverter route it.
        return _converter().convert(path).document
    in_doc = InputDocument(
        path_or_stream=path, format=fmt, backend=backend_cls, filename=path.name
    )
    return backend_cls(path_or_stream=path, in_doc=in_doc).convert()


def main() -> int:
    wanted = set(sys.argv[1:])
    failures = 0
    total = 0
    for fmt_dir in sorted((REPO / "tests/data").iterdir()):
        sources = fmt_dir / "sources"
        if not sources.is_dir():
            continue
        if wanted and fmt_dir.name not in wanted:
            continue
        out_dir = fmt_dir / "groundtruth_dclx"
        for src in sorted(sources.iterdir()):
            if not src.is_file():
                continue
            total += 1
            out = out_dir / f"{src.name}.dclx"
            if out.exists():
                print(f"  = {out.relative_to(REPO)} (present)")
                continue
            try:
                doc = build_document(src)
                out_dir.mkdir(parents=True, exist_ok=True)
                doc.save_as_doclang_archive(out)
                print(f"  > {out.relative_to(REPO)}")
            except Exception as e:  # noqa: BLE001 — record and continue
                failures += 1
                print(f"  ! {src.relative_to(REPO)}: {type(e).__name__}: {e}")
                traceback.print_exc(limit=1)
    print(f"done: {total - failures}/{total} generated, {failures} failed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
