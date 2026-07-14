"""Tests for ``docling_rs.chunking`` — the Rust-native HierarchicalChunker /
HybridChunker exposed to Python. Declarative path only, no ML models; the
hybrid tests use the MiniLM ``tokenizer.json`` checked in for the chunking
conformance suite."""

from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[3]
TOKENIZER = REPO / "tests/data/chunks/tokenizer.json"

docling_rs = pytest.importorskip("docling_rs")

from docling_rs.chunking import (  # noqa: E402
    DocChunk,
    HierarchicalChunker,
    HybridChunker,
    WindowChunker,
)

MD = b"# Guide\n\n## Setup\n\nInstall the tools.\n\n- clone\n- build\n\n## Usage\n\nRun it.\n"


def _document():
    return docling_rs.DocumentConverter().convert_bytes("guide.md", MD).document


def test_hierarchical_chunks_carry_headings_and_doc_items():
    chunks = list(HierarchicalChunker().chunk(_document()))
    assert len(chunks) >= 3
    setup = next(c for c in chunks if "Install" in c.text)
    assert isinstance(setup, DocChunk)
    assert setup.meta.headings == ["Guide", "Setup"]
    assert setup.meta.doc_items and setup.meta.doc_items[0].startswith("#/")
    # Lists stay whole: one chunk for both items.
    assert any(c.text == "- clone\n- build" for c in chunks)


def test_contextualize_prefixes_the_heading_path():
    chunker = HierarchicalChunker()
    setup = next(c for c in chunker.chunk(_document()) if "Install" in c.text)
    assert chunker.contextualize(setup) == "Guide\nSetup\nInstall the tools."


def test_chunk_accepts_dict_and_json_string():
    doc = _document()
    from_doc = [c.text for c in HierarchicalChunker().chunk(doc)]
    from_dict = [c.text for c in HierarchicalChunker().chunk(doc.export_to_dict())]
    import json

    from_str = [c.text for c in HierarchicalChunker().chunk(json.dumps(doc.export_to_dict()))]
    assert from_doc == from_dict == from_str


@pytest.mark.skipif(not TOKENIZER.exists(), reason="MiniLM tokenizer.json not checked out")
def test_hybrid_splits_against_the_token_budget():
    long_md = ("# Doc\n\n" + " ".join(f"Sentence number {i} padding words here." for i in range(40))).encode()
    doc = docling_rs.DocumentConverter().convert_bytes("l.md", long_md).document
    hier = list(HierarchicalChunker().chunk(doc))
    chunker = HybridChunker(tokenizer=str(TOKENIZER), max_tokens=64)
    hybrid = list(chunker.chunk(doc))
    assert len(hybrid) > len(hier)
    assert all(c.meta.headings == ["Doc"] for c in hybrid)
    assert chunker.contextualize(hybrid[0]).startswith("Doc\n")


def test_hybrid_rejects_non_path_tokenizers():
    with pytest.raises(TypeError):
        HybridChunker(tokenizer=123)


@pytest.mark.skipif(not TOKENIZER.exists(), reason="MiniLM tokenizer.json not checked out")
def test_hybrid_default_tokenizer_path(tmp_path, monkeypatch):
    # HybridChunker() with no tokenizer resolves models/chunk/tokenizer.json —
    # the location scripts/install/download_dependencies.sh populates.
    doc_json = _document().export_to_dict()
    (tmp_path / "models/chunk").mkdir(parents=True)
    (tmp_path / "models/chunk/tokenizer.json").write_bytes(TOKENIZER.read_bytes())
    monkeypatch.chdir(tmp_path)
    chunks = list(HybridChunker(max_tokens=64).chunk(doc_json))
    assert chunks and any("Install" in c.text for c in chunks)


def test_bad_tokenizer_path_raises_conversion_error():
    chunker = HybridChunker(tokenizer="/nonexistent/tokenizer.json")
    with pytest.raises(docling_rs.ConversionError):
        list(chunker.chunk(_document()))


def test_window_chunker_cuts_word_windows_with_overlap():
    body = " ".join(f"w{i}" for i in range(25))
    md = f"# A\n\n{body}\n\n# B\n\nshort tail\n".encode()
    doc = docling_rs.DocumentConverter().convert_bytes("w.md", md).document

    chunker = WindowChunker(max_words=10, overlap=0.2)  # step 8: windows of 10, 2 shared
    chunks = list(chunker.chunk(doc))

    a = [c for c in chunks if c.meta.headings == ["A"]]
    assert len(a) == 3  # 25 words -> [0..10), [8..18), [16..25)
    assert a[0].text.startswith("w0 ") and a[0].text.endswith(" w9")
    assert a[1].text.startswith("w8 ")  # the 2-word overlap carries over
    assert a[2].text.endswith(" w24")
    # Windows never cross a heading; contextualize is docling-rag's rendering.
    b = [c for c in chunks if c.meta.headings == ["B"]]
    assert len(b) == 1 and b[0].text == "short tail"
    assert chunker.contextualize(b[0]) == "# B\n\nshort tail"
    assert b[0].meta.doc_items == []


def test_window_chunker_defaults_and_validation():
    # Defaults follow docling-rag: 300-word windows, 5% overlap.
    chunker = WindowChunker()
    assert (chunker.max_words, chunker.overlap) == (300, 0.05)
    long_md = ("# Doc\n\n" + " ".join(f"word{i}" for i in range(650))).encode()
    doc = docling_rs.DocumentConverter().convert_bytes("l.md", long_md).document
    chunks = list(chunker.chunk(doc))
    # 650 words, step 285 -> 3 windows, each within the budget.
    assert len(chunks) == 3
    assert all(len(c.text.split()) <= 300 for c in chunks)

    with pytest.raises(ValueError):
        WindowChunker(max_words=0)
    with pytest.raises(ValueError):
        WindowChunker(overlap=1.0)


def _big_document(paragraphs=3_000):
    big_md = "# Doc\n\n" + "\n\n".join(
        " ".join(f"word{s}x{w}" for w in range(60)) for s in range(paragraphs)
    )
    return docling_rs.DocumentConverter().convert_bytes("big.md", big_md.encode()).document


@pytest.mark.skipif(not TOKENIZER.exists(), reason="MiniLM tokenizer.json not checked out")
def test_chunk_streams_lazily():
    # chunk() is fed by a native background thread: the first chunk must arrive
    # long before the whole document is chunked.
    import time

    doc = _big_document()
    chunker = HybridChunker(tokenizer=str(TOKENIZER), max_tokens=64)

    t0 = time.monotonic()
    it = chunker.chunk(doc)
    first = next(it)
    first_at = time.monotonic() - t0
    n = 1 + sum(1 for _ in it)
    total = time.monotonic() - t0

    assert isinstance(first, DocChunk) and n > 1000
    assert first_at * 4 < total, f"first chunk at {first_at:.2f}s of {total:.2f}s total"


@pytest.mark.skipif(not TOKENIZER.exists(), reason="MiniLM tokenizer.json not checked out")
def test_abandoning_the_iterator_cancels_the_stream():
    from itertools import islice

    doc = _big_document()
    chunker = HybridChunker(tokenizer=str(TOKENIZER), max_tokens=64)

    # islice consumes only 5 chunks; dropping the generator must cancel the
    # background chunking without hanging (join happens on GC of the stream).
    preview = list(islice(chunker.chunk(doc), 5))
    assert len(preview) == 5
    # The preview is the prefix of a full run.
    full_first5 = list(islice(chunker.chunk(doc), 5))
    assert [c.text for c in preview] == [c.text for c in full_first5]


@pytest.mark.skipif(not TOKENIZER.exists(), reason="MiniLM tokenizer.json not checked out")
def test_ctrl_c_interrupts_native_chunking():
    # A SIGINT arriving while the Rust side is chunking must raise
    # KeyboardInterrupt promptly, not stall until the native call returns.
    import signal
    import threading
    import time

    doc = _big_document()

    timer = threading.Timer(0.5, lambda: signal.raise_signal(signal.SIGINT))
    timer.start()
    try:
        t0 = time.monotonic()
        with pytest.raises(KeyboardInterrupt):
            list(HybridChunker(tokenizer=str(TOKENIZER), max_tokens=64).chunk(doc))
        assert time.monotonic() - t0 < 5.0
    finally:
        timer.cancel()
