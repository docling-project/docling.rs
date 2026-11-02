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
  falls back to ``.models/chunk/tokenizer.json``, the MiniLM tokenizer
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

import html
import json
import re
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

    def export_json_dict(self) -> dict:
        """docling's ``DocMeta.export_json_dict()`` shape, with the items as
        ``{"self_ref": ...}`` stubs (this class holds refs, not items) and
        ``None`` fields left out. The LangChain loader resolves the refs into
        docling-core's full ``DocMeta`` whenever a chunk has items."""
        out: dict = {
            "schema_name": "docling_core.transforms.chunker.DocMeta",
            "version": "1.0.0",
            "doc_items": [{"self_ref": ref} for ref in self.doc_items],
        }
        if self.headings is not None:
            out["headings"] = list(self.headings)
        return out


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


def _norm(text: str) -> str:
    """Comparable form of an item text vs. its Markdown rendering: entities
    decoded, escapes dropped, whitespace collapsed."""
    return " ".join(html.unescape(text).replace("\\", "").split())


_MD_LINK = re.compile(r"!?\[([^\]]*)\]\([^)]*\)")
_MD_PREFIX = re.compile(r"^(#{1,6}(\s+|$)|[-*+](\s+\[[ xX]\])?(\s+|$)|\d+[.)](\s+|$))")


def _plain(md: str) -> str:
    """A Markdown item rendering reduced toward the item's plain text: link
    targets, emphasis/code markers and a heading/list prefix dropped."""
    text = _MD_LINK.sub(r"\1", md)
    for marker in ("**", "~~", "`", "*"):
        text = text.replace(marker, "")
    return _MD_PREFIX.sub("", text).strip()


class _RefMapper:
    """Maps the engine's chunk items back to the caller's document refs.

    The native chunker re-imports the document JSON and numbers items the way
    the engine would *re-export* them — which drifts from the caller's
    document whenever the import is not a perfect round trip (empty items are
    dropped, HTML/DOCX exports come from docling's item tree, PDF layouts
    regroup, inline runs share their paragraph's number). The items still
    arrive in (nearly) reading order with their kind and Markdown text, so
    each is matched against the caller's items walked in the same order
    (``body`` tree; pictures and tables not descended): a table/picture to a
    table/picture, a text to a text item it renders — exactly (``Section 2``
    for ``[Section 2](#section-2)``, ``x`` for ``- x``, empty for ``- ``),
    else by containment. Candidates are the items not yet taken in a window
    from the earliest untaken item (bounded to ``WINDOW`` before the last
    match) to ``WINDOW`` past the last match, earliest first — so an item
    emitted out of tree order (a picture caption the engine places next to
    its picture) neither strands the rest nor goes unmatched. An item the
    engine repeats (docling repeats an oversized item across the chunks it is
    split into) is recognised by its native ref and text and maps to the
    same item again. An item that matches nothing is left out rather than
    pointed at the wrong item."""

    WINDOW = 64
    NEAR = 16
    MIN_CONTAINED = 4

    def __init__(self, doc: dict):
        index = {}
        for key in ("texts", "tables", "pictures", "groups"):
            for it in doc.get(key) or ():
                index[it.get("self_ref")] = it
        order = []
        stack = [iter((doc.get("body") or {}).get("children") or ())]
        while stack:
            child = next(stack[-1], None)
            if child is None:
                stack.pop()
                continue
            ref = child.get("$ref") or child.get("cref")
            it = index.get(ref)
            if it is None:
                continue
            if ref.startswith("#/texts/"):
                order.append((ref, "text", _norm(it.get("text") or "")))
            elif ref.startswith("#/tables/"):
                order.append((ref, "table", ""))
                continue
            elif ref.startswith("#/pictures/"):
                order.append((ref, "picture", ""))
                continue
            stack.append(iter(it.get("children") or ()))
        self.order = order
        self.taken: set = set()
        self.low = 0  # earliest untaken item
        self.last = 0  # one past the latest match
        self.seen: dict = {}

    def _runs_after(self, hit: int, hi: int, used: set, text: str) -> List[int]:
        """The text items following a containment match that the same
        rendering also contains, in order — an inline paragraph the engine
        reports as one item is a run of items in docling's document."""
        first = self.order[hit][2]
        pos = text.find(first) + len(first)
        extra = []
        j = hit + 1
        while j < hi and j not in self.taken and j not in used and self.order[j][1] == "text":
            t = self.order[j][2]
            at = text.find(t, pos) if t else -1
            if at < 0:
                break
            extra.append(j)
            pos = at + len(t)
            j += 1
        return extra

    def _near(self, lo: int, hi: int, used: set, kind: str):
        """Untaken items of ``kind`` in ``[lo, hi)``, nearest-first: forward
        from the last match (the natural next item), then backward (an item
        the engine emitted out of tree order)."""
        last = min(max(self.last, lo), hi)
        for i in list(range(last, hi)) + list(range(last - 1, lo - 1, -1)):
            if i not in self.taken and i not in used and self.order[i][1] == kind:
                yield i

    def _find(self, lo: int, hi: int, used: set, kind: str, text: str) -> Optional[int]:
        if kind != "text":
            return next(self._near(lo, hi, used, kind), None)
        plain = _plain(text)
        for i in self._near(lo, hi, used, kind):
            if self.order[i][2] in (text, plain):
                return i
        # Containment only close to the last match, and only for texts long
        # enough to mean something: "duck" far away, or ")" / "." anywhere,
        # is contained in many renderings and would pull the mapping out of
        # sync (short texts still match exactly above).
        near_lo = max(lo, self.last - self.NEAR)
        near_hi = min(hi, self.last + self.NEAR)
        for i in self._near(near_lo, near_hi, used, kind):
            if len(self.order[i][2]) >= self.MIN_CONTAINED and self.order[i][2] in text:
                return i
        return None

    def remap(self, native_refs: List[str], kinds: List[str], texts: List[str]) -> List[str]:
        lo = max(self.low, self.last - self.WINDOW)
        hi = min(len(self.order), self.last + self.WINDOW + len(kinds))
        used: set = set()
        refs: List[str] = []
        for native, kind, raw in zip(native_refs, kinds, texts):
            text = _norm(raw)
            key = (native, kind, text)
            runs = self.seen.get(key)
            if runs is None or any(h in used for h in runs):
                runs = None
                hit = self._find(lo, hi, used, kind, text)
                if hit is not None:
                    runs = [hit]
                    if kind == "text" and self.order[hit][2] not in (text, _plain(text)):
                        runs += self._runs_after(hit, hi, used, text)
                    self.seen[key] = runs
            for h in runs or ():
                used.add(h)
                self.taken.add(h)
                refs.append(self.order[h][0])
                self.last = max(self.last, h + 1)
        while self.low in self.taken:
            self.low += 1
        return refs


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
    doc_json = _document_json(dl_doc)
    mapper = _RefMapper(json.loads(doc_json))
    stream = _chunk_document(doc_json, chunker, tokenizer, size, merge_peers, overlap)
    for record in stream:
        r = json.loads(record)
        doc_items = r["doc_items"]
        if "doc_item_kinds" in r:
            doc_items = mapper.remap(doc_items, r["doc_item_kinds"], r["doc_item_texts"])
        yield DocChunk(
            text=r["text"],
            meta=DocMeta(headings=r["headings"], doc_items=doc_items),
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
        ``.models/chunk/tokenizer.json`` (all-MiniLM-L6-v2's, as fetched by
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
        # The document goes to the engine as docling-JSON (``_document_json``)
        # and is re-imported there, so a picture's ``meta.description`` — a
        # vision model's or the picture OCR's text — reaches the chunks only
        # because the JSON backend reads it back (#975): it is the picture's
        # chunk text, after its caption.
        tokenizer = self.tokenizer
        if tokenizer is None and not Path(".models/chunk/tokenizer.json").exists():
            # The native resolver checks ./.models/chunk/tokenizer.json; when
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
