"""Typst markup export — ``docling-rs --to typst`` for Python (#1226).

docling's Python ``DoclingDocument`` has no Typst exporter; this runs the
Rust serializer on the document's JSON, so the output is byte-identical to the
CLI's. The result is Typst *body* markup (no preamble or template), ready to
``#include`` from your own ``.typ`` and compile with ``typst compile``::

    from docling_rs import DocumentConverter
    from docling_rs.typst import export_to_typst, save_as_typst

    doc = DocumentConverter().convert("report.docx").document
    markup = export_to_typst(doc)                    # str: Typst markup
    save_as_typst(doc, "report.typ", image_mode="referenced")
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional, Union

from ._native import typst_from_json as _typst_from_json

__all__ = ["export_to_typst", "save_as_typst"]


def _document_json(doc: Any) -> str:
    if isinstance(doc, str):
        return doc
    if isinstance(doc, dict):
        return json.dumps(doc)
    if hasattr(doc, "export_to_dict"):
        return json.dumps(doc.export_to_dict())
    raise TypeError(
        "expected a DoclingDocument, a docling-JSON dict, or a JSON string; "
        f"got {type(doc).__name__}"
    )


def _mode(image_mode: Any) -> str:
    # docling's ImageRefMode is a str enum; plain strings work too.
    return str(getattr(image_mode, "value", image_mode))


def export_to_typst(doc: Any, *, image_mode: Any = "placeholder") -> str:
    """The document as Typst body markup.

    ``doc`` is a ``DoclingDocument``, a docling-JSON dict or a JSON string.
    Pictures are ``/* image */`` placeholders (a captioned one a
    ``#figure``); use :func:`save_as_typst` for ``"referenced"`` images,
    which writes the files ``#image(...)`` links to. ``"embedded"`` is the
    placeholder too — Typst cannot decode a ``data:`` URI.
    """
    mode = _mode(image_mode)
    if mode == "referenced":
        raise ValueError("image_mode='referenced' writes files; use save_as_typst()")
    markup, _ = _typst_from_json(_document_json(doc), mode, "artifacts")
    return markup


def save_as_typst(
    doc: Any,
    filename: Union[str, Path],
    *,
    image_mode: Any = "placeholder",
    artifacts_dir: Optional[Union[str, Path]] = None,
) -> None:
    """Write the Typst markup to ``filename``. With
    ``image_mode="referenced"`` the pictures are written under
    ``artifacts_dir`` (default ``<stem>_artifacts`` next to the file) and
    linked by their path relative to the file, like the CLI's batch output."""
    filename = Path(filename)
    mode = _mode(image_mode)
    if artifacts_dir is None:
        artifacts_dir = filename.parent / f"{filename.stem}_artifacts"
    artifacts_dir = Path(artifacts_dir)
    try:
        link_dir = artifacts_dir.relative_to(filename.parent)
    except ValueError:
        link_dir = artifacts_dir
    markup, images = _typst_from_json(_document_json(doc), mode, link_dir.as_posix())
    for rel, data in images:
        path = filename.parent / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    filename.write_text(markup, encoding="utf-8")
