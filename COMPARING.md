# Comparing `docling` (Python) and `fleischwolf` (Rust)

This guide explains how to check the Rust port against the original Python
`docling` so you can track conformance as the migration proceeds.

The yardstick is **Markdown output**: both projects expose the same operation —
`convert(file).document.export_to_markdown()` — so diffing the two Markdown
strings is a direct, apples-to-apples comparison.

There are two axes to compare:

- **Correctness** — does the Markdown match? (§A, §B)
- **Performance** — time, CPU, and memory to convert? (§C)

### Local docling setup

The comparison scripts install the **latest published** `docling` from PyPI into
an isolated `fleischwolf/.venv-compare` (via `uv`) on first run:

```bash
scripts/setup-docling.sh      # optional; the other scripts call this automatically
```

Published docling 2.x bundles every format backend plus the full PDF pipeline
(torch + models), so the first install pulls a few hundred MB. For the
declarative formats the Python side still calls the format backend directly (see
`scripts/docling_convert.py`) rather than `DocumentConverter`, so it avoids
paying the `torch` import cost on every run — the same conversion work, kept
apples-to-apples with what `fleischwolf` does.

---

## A. Scoring against docling across a corpus

This repo ships a regression corpus under `tests/data/<format>/`:

```text
tests/data/html/sources/example_01.html          # input
tests/data/html/groundtruth/example_01.html.md    # older committed reference
```

`conformance.sh` scores the Rust port against the **latest published docling**
(installed from PyPI on first run — see `_common.sh`), per format:

```bash
scripts/conformance.sh html
scripts/conformance.sh docx
```

It prints a per-fixture diff-line count and a summary:

```text
FIXTURE                                        DIFF-LINES
example_01.html                                         5
example_02.html                                      EXACT
...
Exact (strict):                10 / 32
Whitespace-normalized matches: 12 / 32
```

The second metric ignores spacing-only differences (collapsing runs of
whitespace, trimming line ends) — useful when our output is the more faithful
one, e.g. dropping docling's spurious double space in a fraction. A row that
matches only after normalization is flagged `N (ws-ok)`.

> The reference is always the installed docling. The committed groundtruth `.md`
> is used only as a fallback for sources docling can't convert — it predates
> docling-core's current serializer (e.g. its compact `| - |` tables), so it is
> not the source of truth.

---

## B. Live, head-to-head on any file

To compare on a file that isn't in the corpus — or to confirm the groundtruth
hasn't drifted — run both implementations and diff:

```bash
scripts/compare.sh tests/data/html/sources/example_03.html
scripts/compare.sh /path/to/your/own.html
```

`compare.sh` runs the local Python docling backend and the Rust CLI on the same
file, normalizes trailing newlines, and shows a unified diff (or `✅ IDENTICAL`).
The local docling install is set up automatically on first run (see above).

Do it by hand if you prefer:

```bash
# Python (using the local install in .venv-compare)
.venv-compare/bin/python scripts/docling_convert.py in.html > py.md

# Rust
cargo run -p fleischwolf-cli -- in.html > rs.md

diff -u py.md rs.md
```

---

## C. Performance (time, CPU, memory)

`scripts/performance.sh` measures the processing cost of each engine on one
file — wall-clock time, CPU utilization, and peak resident memory — using GNU
`/usr/bin/time`. The Rust side is built in `--release`; the Python side runs the
installed docling (declarative backends, no `torch` import).

```bash
scripts/performance.sh tests/data/html/sources/wiki_duck.html 10   # 10 runs
```

```text
================ end-to-end (whole process) ================
ENGINE                     RUNS   TIME-min   TIME-avg      CPU     PEAK-MEM
docling (python)              6      1.39s      1.41s     363%     125.5 MB
fleischwolf (rust)           6   0.00755s   0.00755s     100%       4.8 MB

  wall-time speedup (avg):  186.8x faster (rust)
  peak-memory ratio:        26.4x less (rust)

================ conversion only (startup excluded) ========
  python (warm, in-process): 0.4736s/doc, peak 134.6 MB
  rust   (whole process incl. startup): 0.00755s/doc — startup is negligible
  warm-conversion speedup:   62.7x faster (rust)
```

**Reading the numbers fairly.** The end-to-end Python time includes interpreter
startup plus importing docling/beautifulsoup4/numpy (~0.3–0.6s), which dominates
on small inputs — a real cost for one-shot CLI use, but not representative of a
long-running service. The script therefore also reports a **warm** number:
Python imports once, then converts in a loop, isolating the actual parse work.
Rust's process startup is ~1 ms, so its end-to-end figure already *is* its warm
figure. Use larger inputs (e.g. `wiki_duck.html`) to see steady-state behavior;
tiny files mostly measure Python's startup.

---

## Worked example

`tests/data/html/sources/example_01.html` → Python (left) vs Rust (right):

```diff
  # Introduction

  This is the first paragraph of the introduction.

  ## Background

  Some background information here.

  Example image

  <!-- image -->

  - First item in unordered list
  - Second item in unordered list

  1. First item in ordered list
  2. Second item in ordered list
-
- 42. First item in ordered list with start
- 43. Second item in ordered list with start
+ 3. First item in ordered list with start
+ 4. Second item in ordered list with start
```

Headings, paragraphs, the image placeholder, unordered list, and the first
ordered list are byte-identical. The only difference is the `<ol start="42">`
case — see the divergence table below.

---

## Current conformance (vs **live** docling, byte-for-byte)

| Backend | Exact matches | Whitespace-normalized |
|---|---|---|
| **CSV** | **9 / 9** ✅ | 9 / 9 |
| **Markdown** | **10 / 10** ✅ | 10 / 10 |
| **AsciiDoc** | **4 / 4** ✅ | 4 / 4 |
| **DeepSeek-OCR md** | **3 / 3** ✅ | 3 / 3 |
| **XLSX** | **9 / 9** ✅ | 9 / 9 |
| **PPTX** | **7 / 7** ✅ | 7 / 7 |
| **DOCX** | **25 / 26** | 25 / 26 |
| **HTML** | **28 / 33** | 28 / 33 |
| **PDF** | **6 / 14** † | 7 / 14 |

> † The pure-parse backends above are scored against **live** docling. **PDF** is a
> discriminative ML reconstruction pipeline (not a deterministic parse), so it is
> scored against a committed groundtruth corpus (`tests/data/pdf/groundtruth`) that
> is **regenerated from live docling** and therefore matches `scripts/conformance.sh
> pdf` (padded GitHub tables, current docling text). The PDF score is reported two
> ways: **6 / 14 strict** byte-for-byte, and **7 / 14 whitespace-normalized** — the
> 7th (`amt_handbook_sample`) differs only by docling's spurious double space in a
> `1⁄4` fraction, where the Rust output's single space is the more faithful
> rendering.

**PDF** (`*.pdf`) ports docling's *standard* (discriminative) PDF pipeline. A
**pure-Rust text parser** (`textparse.rs`, on `lopdf`) reconstructs each glyph's
box from the font's own advance widths and the PDF text/graphics matrices — the
same information docling's `docling-parse` C++ parser uses, and a closer match
than pdfium's *rendered* boxes. It is the default text layer (`DOCLING_PDFIUM_TEXT=1`
falls back to pdfium; pages with no text layer fall back automatically). pdfium
still renders each page to a bitmap and supplies word/code cells for tables. An
ONNX model stack interprets the page — **layout detection** (the `heron`/RT-DETR
region model), **TableFormer** table-structure recognition (a full port: image
encoder + autoregressive OTSL structure decoder + cell-bbox decoder, exported to
ONNX — see `tableformer.rs`, with cv2-exact `INTER_AREA`/`INTER_LINEAR`
preprocessing in `resample.rs`), and **PaddleOCR** recognition for scanned /
image-only pages — and regions are assembled in reading order into a
`DoclingDocument`. The parser's cells feed the ported **docling-parse line
sanitizer** (`dp_lines.rs`, from `cells.h`): the 3-pass corner-distance
contraction with `merge_with` space insertion, `enforce_same_font`, ligature
recomposition, and loose-box geometry. Together they closed the text-run-boundary
gap that capped conformance (inter-run spacing like `LABEL :`, justified
double-spacing, lam-alef ordering, the RTL sentence period, kashida over-emission).
Byte-exact today: `picture_classification`, `code_and_formula`,
`2305.03393v1-pg9` (**including its TableFormer-reconstructed table, cell for
cell**), `multi_page`, `right_to_left_01`, and `right_to_left_02`. `amt` matches
once whitespace is normalized. The rest are structurally correct but not yet
byte-exact; the remaining gaps are model-level — TableFormer multi-row header/span
structure on dense papers, layout classification (a TOC read as a picture, a
survey read as tables), and title-page reading order.

**DOCX** (`*.docx`) is a core port of `MsWordDocumentBackend` (`roxmltree` over
the `ooxml` helper): paragraphs, headings (by style, incl. Title), **numbered
headings** with computed multilevel prefixes (`## 1 Section 1`), inline
bold/italic/strike/underline/sub-superscript + hyperlinks (runs grouped by format,
then space-joined), `<w:br>` line breaks, lists (numbering on the paragraph or
inherited from its style; `numId=0` overrides off; list level normalised to its
base `ilvl`), tables with `gridSpan`/`vMerge` merges, **rich table cells** (lists,
formatting, and flattened nested tables rendered as block Markdown with docling's
exact double-space serialization), **1×1 tables as furniture** (unwrapped to body
blocks), **checkboxes** (`<w14:checkbox>` → `- [x]`/`- [ ]`), `<w:sdt>` content,
DrawingML + VML images (emitted before paragraph text, deduped across
`<mc:AlternateContent>`), and **OMML equations → LaTeX** (a port of docling's
`omml.py` in `omml.rs`: fractions, scripts, radicals, functions, n-ary operators,
delimiters, matrices; standalone formulas render as `$$…$$`, inline/cell ones as
`$…$`). The 4 remaining misses each need a substantial subsystem: full Word
multilevel list/heading **shared numbering** with `lvlText` templates
(`unit_test_headers_numbered`), position-sorted **textbox / shape text** layout
(`textbox`, `drawingml`), and advanced OMML constructs plus inline-equation
spacing and equations-in-list-items (`equations`).

**PPTX** (`*.pptx`) ports docling's `MsPowerpointDocumentBackend` by walking each
slide's shape tree (`roxmltree`): titles → `#`, body/text-box paragraphs and
bullet/numbered lists (an explicit `buNone`/`buChar`/`buAutoNum` wins; otherwise
body placeholders default to a bullet, text boxes to a paragraph — matching the
master-style inheritance), tables (with `gridSpan`/`rowSpan` merges duplicated),
and pictures — emitting `<!-- image -->` only for loadable embedded images
(linked, missing-rel, and non-`image/*` blips are dropped, like python-pptx+PIL).
It even reproduces docling's subtitle bug (subtitles render as plain text).

**XLSX** (`*.xlsx`/`*.xlsm`) ports docling's `MsExcelDocumentBackend` on top of the
pure-Rust `calamine` reader: each visible worksheet is flood-filled (strict
adjacency, `gap_tolerance=0`) into contiguous data regions → padded tables, with
merged cells duplicated across their span, dates/numbers/booleans formatted to
match openpyxl's `str()`, CRLF normalised like the XML parser, chartsheets and
hidden sheets skipped, and one `<!-- image -->` per embedded picture (counted from
the drawing parts via the shared `ooxml` zip/rels helper). The one miss
(`xlsx_07`) is a calamine rich-text decoding subtlety plus a docling phantom
all-empty table.

CSV, Markdown, AsciiDoc, and the DeepSeek-OCR Markdown variant are fully
one-to-one. HTML's 5 remaining misses are a tail of docling-internal behaviours —
some requiring **headless-browser rendering**, others (a large Wikipedia page,
key-value form extraction) needing substantial structural work — see below. 28/33
is roughly the ceiling for a pure-parse port.

**AsciiDoc** (`*.asciidoc`/`*.adoc`) ports docling's line-oriented
`AsciiDocBackend`: titles/sections, nested bullet/numbered lists (all rendered
`-`, as docling does), bare and `|===` tables with cell-format-specifier
stripping, images and captions, and the doc-tree ordering quirk where images and
orphan (level-skipping) headings render after the whole title subtree.

**DeepSeek-OCR Markdown** (`md_deepseek`) is the VLM output format: Markdown
where each block is prefixed by a `<|ref|>label<|/ref|><|det|>[[bbox]]<|/det|>`
annotation. The backend (auto-detected from those tokens) splits on them, drops
content before the first token, and maps each label to a node — reusing the HTML
table parser for embedded `<table>` blocks. Bounding boxes are discarded (they
only feed page-image provenance, which a pure-text port doesn't model).

CSV is fully one-to-one (delimiter sniffing over `,;\t|:`, RFC-4180 quoting,
ragged rows, numeric right-align, `|`/newline cell escaping). Markdown and HTML
match the structural cases and most inline formatting; the remaining diffs are a
tail of docling-specific quirks (below), each typically 1–2 lines.

> These numbers are for **legacy** mode (`DocumentConverter::new()`), which aims
> for byte-for-byte docling output. The Rust-only `strict(true)` mode instead
> emits cleaner Markdown (code-fence languages kept, no `***x*** .` run-spacing,
> no `\_`/entity re-escaping, and **PDF hyperlink annotations rendered as
> `[anchor](href)`** — web/mail/tel targets that docling's pipeline drops) — it
> deliberately *diverges* from docling, so don't measure conformance against it.

### HTML

Against live docling: **10 / 32** exact, **12 / 32** whitespace-normalized. (The
older committed groundtruth would report a lower 6/32 — it predates docling's
padded-table serializer; see §A.) The remaining real differences trace to a
small number of *systematic* behaviours below, not to parsing errors — closing
each tends to fix several fixtures at once.

### Known divergences (tracked conformance gaps)

HTML — remaining gaps (4 of 32), all blocked or impractical:

| # | Difference | Example | Why it's blocked |
|---|---|---|---|
| 1 | Browser-rendering visibility / nav suppression | `wiki_duck` | docling renders the page in a headless browser to drop nav/menu/sidebar cruft — not replicable from parsing alone |
| 2 | Key-value-pair / form extraction | `kvp_data_example` | docling's `form_region` subsystem builds key/marker/value relations using rendered bounding boxes and synthesizes `<!-- missing-text -->` placeholders — browser-bbox-dependent |
| 3 | Browser-hidden image (mobile nav) dropped | `hyperlink_02` | docling drops it via rendered visibility |
| 4 | Deeply-nested-table padding when flattened into a cell | `table_06` | docling pads the rich-cell text using rendered table bounding boxes; shallow nesting matches, ≥3-level deep padding does not |

Markdown — **10/10**. Both former gaps fixed: `signature`/`stamp` → image blocks,
and the `2\.` escape via contiguity-aware text merging (pulldown splits both
`[11]` and `2\.`, but only the escape leaves an offset gap — merging only
*contiguous* text events reproduces both). CSV — **9/9**.

Already aligned with docling (previously diverged, now fixed):

- **CSV**: full delimiter sniffing + RFC-4180 quoting → 9/9 exact.
- **Markdown**: inline runs re-joined with spaces (`***x*** .`); `_`/`&<>` escaping;
  entity decode-then-re-escape; lone code span → code block; embedded raw-HTML
  blocks (tables/lists) parsed via the HTML backend; fenced code language dropped.
- Ordered lists numbered sequentially; sibling `<ul>`/`<ol>` separated by a blank
  line; nested items indented 4 spaces; block images as caption + `<!-- image -->`.
- **Tables** rendered exactly like docling-core's `tabulate(tablefmt="github")`:
  padded columns, header `MIN_PADDING=2`, numeric columns right-aligned, separator
  of `width+2` dashes, `|`→`&#124;` cell escaping. (The committed groundtruth still
  uses the older compact `| - |` style, so it serves only as a fallback.)

---

## How to read the numbers

`conformance.sh` counts **diff lines** (`diff` `<`/`>` markers): one changed line
shows as `2`. It reports two summary counts — **Exact (strict)** byte-for-byte and
**Whitespace-normalized matches** (spacing-only diffs ignored; a fixture that
matches only after normalization is flagged `N (ws-ok)`). The point isn't the
absolute score — it's the trend as gaps in the table get closed, and catching
regressions when a change makes a previously-matching fixture diverge.

For CI, gate on the summary (e.g. fail if the exact-match count drops): it
compares against the docling version actually installed, so it won't flag
differences that are really just a stale committed corpus.
