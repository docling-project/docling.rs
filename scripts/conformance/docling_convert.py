#!/usr/bin/env python3
"""Convert a document to Markdown using docling.

For declarative/text formats (HTML, Markdown, CSV, AsciiDoc, Office, VTT) this
deliberately bypasses ``docling.document_converter.DocumentConverter`` and calls
the format's backend directly: importing the converter eagerly pulls in the ML
pipeline (``torch`` and friends), which is irrelevant to those formats. Calling
the backend directly keeps the comparison with the Rust port apples-to-apples
and avoids paying torch import cost on every run.

PDFs and images have no such lightweight path — they need the full pipeline
(layout + table structure + OCR). For those we use the real ``DocumentConverter``,
which is exactly what docling does to a PDF and the honest thing to time against
the Rust pipeline.

Usage:
    docling_convert.py <input> [output.md]
If no output path is given, Markdown is written to stdout.
"""

import mimetypes
import re
import sys
from pathlib import Path

# Minimal containers ship no /etc/mime.types, so Python's mimetypes registry
# doesn't know epub and docling's InputDocument rejects the file with a
# validation error — which conformance.sh then silently masks by falling back
# to the committed groundtruth. Register it explicitly so the live-docling
# reference is always the one measured.
mimetypes.add_type("application/epub+zip", ".epub")

from docling.datamodel.base_models import InputFormat
from docling.datamodel.document import InputDocument

EXT_TO_FORMAT = {
    "html": InputFormat.HTML,
    "htm": InputFormat.HTML,
    "xhtml": InputFormat.HTML,
    "md": InputFormat.MD,
    "markdown": InputFormat.MD,
    "txt": InputFormat.MD,
    "csv": InputFormat.CSV,
    "asciidoc": InputFormat.ASCIIDOC,
    "adoc": InputFormat.ASCIIDOC,
    "asc": InputFormat.ASCIIDOC,
    "docx": InputFormat.DOCX,
    "dotx": InputFormat.DOCX,
    "docm": InputFormat.DOCX,
    "dotm": InputFormat.DOCX,
    "pptx": InputFormat.PPTX,
    "potx": InputFormat.PPTX,
    "ppsx": InputFormat.PPTX,
    "pptm": InputFormat.PPTX,
    "potm": InputFormat.PPTX,
    "ppsm": InputFormat.PPTX,
    "xlsx": InputFormat.XLSX,
    "xlsm": InputFormat.XLSX,
    "vtt": InputFormat.VTT,
    "eml": InputFormat.EMAIL,
    "epub": InputFormat.EPUB,
    "odt": InputFormat.ODT,
    "ods": InputFormat.ODS,
    "odp": InputFormat.ODP,
    "nxml": InputFormat.XML_JATS,
    "pdf": InputFormat.PDF,
    "png": InputFormat.IMAGE,
    "jpg": InputFormat.IMAGE,
    "jpeg": InputFormat.IMAGE,
    "tif": InputFormat.IMAGE,
    "tiff": InputFormat.IMAGE,
    "bmp": InputFormat.IMAGE,
    "gif": InputFormat.IMAGE,
    "webp": InputFormat.IMAGE,
    "dclg": InputFormat.XML_DOCLANG,
    "dclx": InputFormat.DCLX,
}

# PDF and images have no lightweight backend-only path: they go through docling's
# full pipeline (layout + table structure + OCR), which needs torch and model
# weights. We reuse a single DocumentConverter so warm-loop timings don't re-pay
# model loading.
_PIPELINE_FORMATS = {InputFormat.PDF, InputFormat.IMAGE}
_PIPELINE_CONVERTER = None


def _pipeline_converter():
    global _PIPELINE_CONVERTER
    if _PIPELINE_CONVERTER is None:
        from docling.document_converter import DocumentConverter

        _PIPELINE_CONVERTER = DocumentConverter()
    return _PIPELINE_CONVERTER


# A DeepSeek-OCR annotation token line (drives the special VLM markdown path).
_DEEPSEEK_RE = re.compile(
    r"^(?:<\|ref\|>)?(\w+)(?:<\|/ref\|>)?(?:<\|det\|>)?\[\[([0-9., ]+)\]\](?:<\|/det\|>)?\s*$"
)


def backend_for(fmt: InputFormat):
    if fmt == InputFormat.HTML:
        from docling.backend.html_backend import HTMLDocumentBackend

        return HTMLDocumentBackend
    if fmt == InputFormat.MD:
        from docling.backend.md_backend import MarkdownDocumentBackend

        return MarkdownDocumentBackend
    if fmt == InputFormat.CSV:
        from docling.backend.csv_backend import CsvDocumentBackend

        return CsvDocumentBackend
    if fmt == InputFormat.ASCIIDOC:
        from docling.backend.asciidoc_backend import AsciiDocBackend

        return AsciiDocBackend
    if fmt == InputFormat.DOCX:
        from docling.backend.msword_backend import MsWordDocumentBackend

        return MsWordDocumentBackend
    if fmt == InputFormat.PPTX:
        from docling.backend.mspowerpoint_backend import MsPowerpointDocumentBackend

        return MsPowerpointDocumentBackend
    if fmt == InputFormat.XLSX:
        from docling.backend.msexcel_backend import MsExcelDocumentBackend

        return MsExcelDocumentBackend
    if fmt == InputFormat.VTT:
        from docling.backend.webvtt_backend import WebVTTDocumentBackend

        return WebVTTDocumentBackend
    if fmt == InputFormat.EMAIL:
        from docling.backend.email_backend import EmailDocumentBackend

        return EmailDocumentBackend
    if fmt == InputFormat.EPUB:
        from docling.backend.epub_backend import EpubDocumentBackend

        return EpubDocumentBackend
    if fmt == InputFormat.ODT:
        from docling.backend.opendocument_backend import OdtDocumentBackend

        return OdtDocumentBackend
    if fmt == InputFormat.ODS:
        from docling.backend.opendocument_backend import OdsDocumentBackend

        return OdsDocumentBackend
    if fmt == InputFormat.ODP:
        from docling.backend.opendocument_backend import OdpDocumentBackend

        return OdpDocumentBackend
    if fmt == InputFormat.XML_JATS:
        from docling.backend.xml.jats_backend import JatsDocumentBackend

        return JatsDocumentBackend
    if fmt == InputFormat.XML_USPTO:
        from docling.backend.xml.uspto_backend import PatentUsptoDocumentBackend

        return PatentUsptoDocumentBackend
    if fmt == InputFormat.XML_DOCLANG:
        from docling.backend.xml.doclang_backend import DocLangDocumentBackend

        return DocLangDocumentBackend
    if fmt == InputFormat.DCLX:
        from docling.backend.xml.doclang_archive_backend import DocLangArchiveBackend

        return DocLangArchiveBackend
    raise SystemExit(f"no declarative backend wired for format: {fmt}")


def _is_deepseek(text: str) -> bool:
    return any(_DEEPSEEK_RE.match(line.strip()) for line in text.splitlines())


def _convert_deepseek(path: Path) -> str:
    """DeepSeek-OCR annotated markdown uses a dedicated parser (mock page)."""
    from docling_core.types.doc import Size
    from PIL import Image

    from docling.utils.deepseekocr_utils import parse_deepseekocr_markdown

    doc = parse_deepseekocr_markdown(
        content=path.read_text(encoding="utf-8"),
        original_page_size=Size(width=612, height=792),
        page_image=Image.new("RGB", (612, 792), color="white"),
        page_no=1,
        filename=path.name,
    )
    return doc.export_to_markdown()


def _sniff_xml(path: Path) -> InputFormat:
    head = path.read_text(encoding="utf-8", errors="ignore")[:4000]
    if any(
        k in head
        for k in ("us-patent", "patent-application-publication", "PATDOC", "<pap-v1")
    ):
        return InputFormat.XML_USPTO
    return InputFormat.XML_JATS


def convert_to_markdown(path: Path) -> str:
    ext = path.suffix.lower().lstrip(".")
    fmt = EXT_TO_FORMAT.get(ext)
    if fmt == InputFormat.XML_JATS and ext == "xml":
        fmt = _sniff_xml(path)
    if fmt is None:
        raise SystemExit(f"unrecognized extension '.{ext}' for {path}")
    if fmt in _PIPELINE_FORMATS:
        return _pipeline_converter().convert(path).document.export_to_markdown()
    if fmt == InputFormat.MD and _is_deepseek(path.read_text(encoding="utf-8")):
        return _convert_deepseek(path)
    backend_cls = backend_for(fmt)
    in_doc = InputDocument(
        path_or_stream=path,
        format=fmt,
        backend=backend_cls,
        filename=path.name,
    )
    backend = backend_cls(path_or_stream=path, in_doc=in_doc)
    return backend.convert().export_to_markdown()


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit("usage: docling_convert.py <input> [output.md]")
    path = Path(sys.argv[1])
    markdown = convert_to_markdown(path)
    if len(sys.argv) >= 3:
        Path(sys.argv[2]).write_text(markdown, encoding="utf-8")
    else:
        sys.stdout.write(markdown)


if __name__ == "__main__":
    main()
