"""Rust-native chunkers — docling's chunking API backed by ``docling::chunker``.

The Rust port of ``docling_core.transforms.chunker`` runs the chunking; this
module mirrors docling's chunker API shape so call sites translate directly::

    from docling_rs import DocumentConverter
    from docling_rs.chunking import HierarchicalChunker, HybridChunker, WindowChunker

    doc = DocumentConverter().convert("report.docx").document

    for chunk in HierarchicalChunker().chunk(doc):          # structure-driven
        print(chunk.meta.headings, chunk.text)

    chunker = HybridChunker(tokenizer="tokenizer.json", max_tokens=256)
    for chunk in chunker.chunk(doc):                         # tokenization-aware
        embed_me = chunker.contextualize(chunk)              # heading path + text

    chunker = WindowChunker(max_words=300, overlap=0.05)     # word-window, no tokenizer
    for chunk in chunker.chunk(doc):                         # docling-rag's window chunker
        embed_me = chunker.contextualize(chunk)              # '# path' line + body

All chunkers **stream**: ``chunk()`` returns a lazy iterator fed by a native
background thread — each chunk is handed to Python as the Rust side produces
it, so the full chunk list is never materialized and the first chunk arrives
before the last one is computed. Abandoning the iterator early (``break``,
``itertools.islice``, dropping the generator) cancels the background chunking;
Ctrl-C interrupts a pending ``next()``.

Differences from docling's ``docling.chunking``:

* ``HybridChunker(tokenizer=...)`` takes a **path to a HuggingFace
  ``tokenizer.json``** (e.g. ``sentence-transformers/all-MiniLM-L6-v2``'s),
  not a tokenizer object — the Rust side loads it with the ``tokenizers``
  crate, so no Python ``transformers`` install is needed. When omitted it
  falls back to ``models/chunk/tokenizer.json``, the MiniLM tokenizer
  ``scripts/install/download_dependencies.sh`` fetches alongside the ML
  models.
* ``chunk.meta.doc_items`` holds the items' JSON-pointer refs (``"#/texts/12"``)
  rather than resolved item objects.

All chunkers accept any ``docling_core.types.doc.DoclingDocument`` (or a
plain docling-JSON ``dict``/``str``). Since this package's ``result.document``
*is* a genuine ``DoclingDocument``, docling's own Python chunkers also keep
working on it — these classes are the faster, dependency-free native path.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Iterator, List, Optional

from pathlib import Path

from . import models
from ._native import chunk_document as _chunk_document

__all__ = [
    "DocMeta",
    "DocChunk",
    "BaseChunk",
    "BaseChunker",
    "HierarchicalChunker",
    "HybridChunker",
    "WindowChunker",
]


@dataclass
class DocMeta:
    """Chunk metadata — the analogue of docling's ``DocMeta``."""

    #: Heading path above the chunk, outermost first; ``None`` above any heading.
    headings: Optional[List[str]] = None
    #: JSON-pointer refs of the document items the chunk was built from.
    doc_items: List[str] = field(default_factory=list)


@dataclass
class DocChunk:
    """One chunk — the analogue of docling's ``DocChunk``."""

    text: str
    meta: DocMeta
    #: The embedding-ready rendering (heading path + text), precomputed by the
    #: Rust side; read it via ``chunker.contextualize(chunk)``.
    _contextualized: str = ""


def _document_json(dl_doc: Any) -> str:
    """Accept a DoclingDocument, a docling-JSON dict, or a JSON string."""
    if isinstance(dl_doc, str):
        return dl_doc
    if isinstance(dl_doc, dict):
        return json.dumps(dl_doc)
    if hasattr(dl_doc, "export_to_dict"):
        return json.dumps(dl_doc.export_to_dict())
    raise TypeError(
        "expected a DoclingDocument, a docling-JSON dict, or a JSON string; "
        f"got {type(dl_doc).__name__}"
    )


def _run(
    dl_doc: Any,
    chunker: str,
    tokenizer: Optional[str] = None,
    size: int = 256,
    merge_peers: bool = True,
    overlap: float = 0.05,
) -> Iterator[DocChunk]:
    # `size` is the chunk budget in the chunker's unit: tokens for "hybrid"
    # (max_tokens), words for "window" (max_words); "hierarchical" ignores it.
    # The native side streams: a background Rust thread parses the document and
    # chunks it, handing over one record at a time — chunks are consumed as
    # they are produced, never materialized as a whole. Abandoning the
    # iterator early cancels the background chunking.
    stream = _chunk_document(
        _document_json(dl_doc), chunker, tokenizer, size, merge_peers, overlap
    )
    for record in stream:
        r = json.loads(record)
        yield DocChunk(
            text=r["text"],
            meta=DocMeta(headings=r["headings"], doc_items=r["doc_items"]),
            _contextualized=r["contextualize"],
        )


class _BaseChunker:
    def contextualize(self, chunk: DocChunk) -> str:
        """The text to embed: heading path + chunk body, newline-joined."""
        return chunk._contextualized


# docling.chunking-parity aliases: docling exports its chunk type and chunker
# base under these names too, so `from docling_rs.chunking import BaseChunk,
# BaseChunker` works for isinstance checks and type hints after a
# docling → docling_rs package swap.
BaseChunk = DocChunk
BaseChunker = _BaseChunker


class HierarchicalChunker(_BaseChunker):
    """docling's structure-driven chunker: one chunk per document item (whole
    lists, triplet-serialized tables, picture captions), heading path as
    metadata."""

    def chunk(self, dl_doc: Any) -> Iterator[DocChunk]:
        return _run(dl_doc, "hierarchical")


class HybridChunker(_BaseChunker):
    """docling's tokenization-aware chunker: hierarchical chunks split against
    a token budget and undersized same-heading neighbours merged.

    :param tokenizer: path to a HuggingFace ``tokenizer.json``. Defaults to
        ``models/chunk/tokenizer.json`` (all-MiniLM-L6-v2's, as fetched by
        ``scripts/install/download_dependencies.sh``); raises at ``chunk()``
        time if neither is available.
    :param max_tokens: token budget per chunk (docling's default for the
        MiniLM embedding model is 256).
    :param merge_peers: merge undersized peer chunks with identical headings
        (docling's default ``True``).
    """

    def __init__(
        self, tokenizer: Optional[str] = None, max_tokens: int = 256, merge_peers: bool = True
    ):
        if tokenizer is not None and not isinstance(tokenizer, str):
            raise TypeError(
                "HybridChunker(tokenizer=...) takes a path to a HuggingFace "
                "tokenizer.json (docling_rs loads it natively)"
            )
        self.tokenizer = tokenizer
        self.max_tokens = max_tokens
        self.merge_peers = merge_peers

    def chunk(self, dl_doc: Any) -> Iterator[DocChunk]:
        tokenizer = self.tokenizer
        if tokenizer is None and not Path("models/chunk/tokenizer.json").exists():
            # The native resolver checks ./models/chunk/tokenizer.json; when
            # that's absent, fall back to the package cache populated by
            # docling_rs.download_models().
            cached = models.cache_dir() / "models/chunk/tokenizer.json"
            if cached.exists():
                tokenizer = str(cached)
        return _run(
            dl_doc, "hybrid", tokenizer, size=self.max_tokens, merge_peers=self.merge_peers
        )


class WindowChunker(_BaseChunker):
    """docling-rag's Markdown **window chunker**: the document's Markdown is cut
    into heading-bounded sections of plain words (markup stripped), and a
    fixed-size window of ``max_words`` words slides over each section with
    ``overlap`` fractional overlap. A chunk never crosses a heading boundary.

    No tokenizer and no ML models are involved — the budget is *words*, making
    this the zero-dependency choice when an approximate chunk size is enough.

    Two deltas from the docling-style chunkers above:

    * ``chunk.text`` is plain words joined by single spaces (markdown markup
      does not survive), and ``chunk.meta.doc_items`` is always empty — the
      window chunker works on the rendered Markdown, not the document tree.
    * ``contextualize(chunk)`` renders docling-rag style: a ``# Outer > Inner``
      heading-context line, a blank line, then the chunk body.

    :param max_words: window size in words (docling-rag's default 300).
    :param overlap: fractional overlap between consecutive windows, ``0.0`` to
        ``<1.0`` (docling-rag's default 0.05 = 5%).
    """

    def __init__(self, max_words: int = 300, overlap: float = 0.05):
        if not isinstance(max_words, int) or isinstance(max_words, bool) or max_words < 1:
            raise ValueError("WindowChunker(max_words=...) must be a positive integer")
        if not 0.0 <= float(overlap) < 1.0:
            raise ValueError("WindowChunker(overlap=...) must be in [0.0, 1.0)")
        self.max_words = max_words
        self.overlap = float(overlap)

    def chunk(self, dl_doc: Any) -> Iterator[DocChunk]:
        return _run(dl_doc, "window", size=self.max_words, overlap=self.overlap)
