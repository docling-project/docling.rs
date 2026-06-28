# PDF conformance roadmap

How close the Rust PDF pipeline gets to docling's **default** Markdown, measured
byte-for-byte against the committed groundtruth (`tests/data/pdf/groundtruth/*.md`),
and what it would take to close the remaining gap.

> Measure locally with `scripts/pdf_groundtruth.sh` (no docling install needed —
> it diffs against the checked-in reference). The numbers below are the current
> state.

## Current state

**1 / 14 groundtruth PDFs are byte-for-byte exact** (`picture_classification`);
the rest are blocked on one of the categories below. Diff = changed lines vs the
groundtruth (one changed line counts as 2).

| PDF | diff | dominant blocker |
|---|---:|---|
| picture_classification | **exact** | — |
| right_to_left_01 | 4 | RTL/bidi |
| code_and_formula | 6 | inter-run spacing + code fencing |
| right_to_left_02 | 8 | RTL/bidi |
| amt_handbook_sample | 12 | double-spaces, duplicate glyphs, fractions |
| 2305.03393v1-pg9 | 25 | table structure |
| right_to_left_03 | 74 | RTL/bidi |
| multi_page | 76 | inter-run spacing + line-wrap hyphens |
| normal_4pages | 108 | reading order (CJK) |
| 2305.03393v1 | 152 | table structure |
| table_mislabeled_as_picture | 151 | table structure |
| 2203.01017v2 | 346 | table structure (+ inter-run spacing) |
| 2206.01062 | 321 | table structure |
| redp5110_sampled | 342 | table structure |

Shipped in this PR (no regressions; `pdf_conformance` stays 76/76):
de-hyphenation + typography normalization, `<!-- formula-not-decoded -->`,
caption-before-image pairing, and (strict-mode only) punctuation tightening.

Reaching ~50% exact requires the two big items below: **text-stream extraction**
(unlocks the spacing-bound PDFs) and **TableFormer** (unlocks the six
table-bound PDFs).

---

## Blocker 1 — inter-run text spacing (a.k.a. "text-stream extraction")

**Symptom.** pdfium splits a visual line into multiple style *segments* (a
citation's superscripts, a code line's tokens, mixed fonts). We emit one cell
per segment and join them with single spaces, so the real inter-run spacing is
lost: `[ 37 , 36 ]` instead of `[37, 36]`, `function add ( a , b )` instead of
`function add(a, b)`. docling reads text via pypdfium2's `get_text_range`
(`FPDFText_GetText`), which inserts spaces from each glyph's *advance* and so
reproduces the PDF's real spacing.

**Eight approaches were tried in this PR; all regressed the aggregate and were
reverted.** The headline finding: real `FPDFText_GetText` **is reachable and
works in isolation**, but no whole-line reconstruction built on top of it beats
pdfium's native style segments.

1. **Raw char API** (`PdfPageText::chars()` → `unicode_char()` + `loose_bounds()`,
   concatenated per line). pdfium's per-char list is *unreliable*: some lines come
   back with no space characters at all (`Thiscontentisextremelyvaluablefor`) and
   the char order is occasionally scrambled.
2. **Raw char API + gap-based spacing** (drop pdfium's spaces, re-insert from
   glyph gaps). Fixes code perfectly but garbles prose (band mis-sort merges
   glyphs from adjacent lines: `valuablefor`, stray glyphs).
3. **`inside_rect()`** (`FPDFText_GetBoundedText`) over a whole line's bbox.
   `GetBoundedText` ≠ `GetText`: it *drops* inter-run spaces on multi-segment
   lines (`{ahn,nli,mly,taa}@zurich` vs docling's `{ ahn,nli,mly,taa } @zurich`)
   and *bleeds* glyphs from vertically adjacent lines.
4. **`inside_rect` hybrid** (segment text for single-segment lines only). Same
   `GetBoundedText` divergence on the lines that need fixing.
5. **Real `FPDFText_GetText`, char-detected lines.** Reached the raw call via the
   public `PdfiumLibraryBindings` trait — `bindings()` on the `Pdfium`/`PdfPage`
   exposes `FPDF_LoadMemDocument`/`FPDF_LoadPage`/`FPDFText_LoadPage`/`CountChars`/
   `GetCharBox`/`GetText`, so a *second raw-FFI handle on the same bytes* drives
   `GetText` directly (no fork, stays publishable). **Citations read correctly in
   isolation** (`[37, 36, 18, 20]`). But my char-box line detection diverges from
   pdfium's line structure, and `GetText` inserts letter-tracking spaces into
   display text (`Fi gures` for a tracked title) — net regression.
6. **+ U+FFFE de-hyphenation.** `GetText` encodes the wrap hyphen as **U+FFFE**
   (not the segment path's U+0002); handling it recovered most prose, but the
   title-tracking and line-detection issues remained.
7. **+ single-segment override** (replace a GetText line with segment text when
   one segment covers it). Helped marginally; line boundaries still diverged.
8. **Segment-defined lines + `GetText` per multi-segment range** (group *segments*
   into lines so the structure matches docling, `GetText` only the multi-segment
   lines via a bbox→char-index range). Preserved the exact PDFs and improved
   `multi_page`, but the bbox→range mapping mis-selects characters on dense
   two-column pages, so citation lines on `2203`/`2206` came back wrong and
   `normal_4pages` regressed (108→152).

**Conclusion.** The blocker is *not* the missing binding — `GetText` is callable
and correct. It is that **reconstructing docling's exact line + character ranges
from glyph/segment geometry is itself the hard problem** (docling uses
`docling-parse`, a purpose-built PDF text reconstructor, not raw `GetText`).
pdfium's own style segments are a better-structured unit than anything rebuilt on
top of them here, so the segment path stays in production. A real fix needs a
faithful line/cell reconstructor (port `docling-parse`, or use pdfium's
`FPDFText_GetTextObject`/structured APIs to get true line boundaries before
`GetText`), not just the call this PR proved reachable.

**Stopgap shipped:** `--strict` tightens the citation/parenthetical spacing at
serialization time, so strict Markdown reads cleanly even though default mode
mirrors the segment spacing.

Also needed alongside it:
- **Line-wrap de-hyphenation for real hyphens.** We already drop the U+0002 soft
  hyphen; `multi_page` wraps words with a real `-` (`professi-`/`onal`), which
  needs line-end-hyphen detection during the line join.
- **Double-space preservation.** docling keeps the PDF's wide justified spacing
  (`the stainless  steel  nuts`); `clean_text` currently collapses runs of
  whitespace. With `GetText` per line, stop collapsing intra-line spacing.

## Blocker 2 — table structure (TableFormer)

**Symptom.** Six PDFs (`2206.01062`, `2305.03393v1[-pg9]`, `redp5110_sampled`,
`table_mislabeled_as_picture`, and the table on `2203.01017v2`) are dominated by
table differences. We reconstruct grids *geometrically* (cluster cells into
rows/columns); docling runs **TableFormer**, an autoregressive transformer that
predicts the table structure as an OTSL/HTML tag sequence plus per-cell bounding
boxes, which recovers spanning headers and merged cells we cannot.

**Status — ONNX export + decode VERIFIED byte-exact ✅.**
`scripts/export_tableformer.py` loads `TableModel04_rs` (`accurate`, resnet18 +
6-layer encoder + 6-layer decoder) and exports two graphs, both verified against
PyTorch (max abs diff < 1e-5):

- `encoder.onnx` — `image[1,3,448,448] → memory[784,1,512]`
- `decoder.onnx` — `tags[seq,1] + memory → logits[1,13], hidden[1,512]`

The Rust loop (`crates/fleischwolf-pdf/src/tableformer.rs`) feeds the growing
token list back in and applies docling's two corrections (`xcel→lcel` on *every*
row — docling's `line_num` is never incremented; `ucel`-then-`lcel → fcel`).
**Verified: this reproduces docling's OTSL token sequence byte-exact** on
docling's own preprocessed table tensor (54-token sequence on `2305v1-pg9`).

Two findings that cost real debugging, recorded so they aren't re-hit:
- The model's decoder layer keeps only `tgt[-1:]` per layer and relies on a
  non-standard per-layer cache; re-running it cache-less loses deep context.
  Equivalent stateless form: apply each layer to the whole prefix under a causal
  mask. (Export uses the dynamo exporter so the `seq` axis stays symbolic — the
  legacy tracer bakes it into `nn.MultiheadAttention`'s reshape.)
- **Export from `docling-project/docling-models`, not `ds4sd/docling-models`** —
  both are cached, weights differ, and the old ones give a different OTSL.

**Remaining (the Rust integration, staged):**

1. **Preprocessing parity.** The decode is exact on docling's input tensor, but
   the pipeline's own crop differs (docling resizes the *page* to 1024px height,
   crops the table bbox, then resizes to 448²; we crop the 2× render directly).
   Match this so the live OTSL matches (currently 88 vs 54 tokens on `2305v1-pg9`
   purely from the crop).
2. **Bbox decoder** (`bbox.onnx`, not yet exported): per-cell hidden → box, for
   matching. Interim: skip it and match by grid geometry.
3. **OTSL → grid.** `docling_ibm_models/tableformer/otsl.py` is the reference
   (`fcel/ched/rhed/srow/ecel/lcel/ucel/xcel/nl`); port it to a `Table` with
   row/col spans (the Markdown serializer needs span support added).
4. **Cell content.** Map the PDF text cells we already extract onto the grid
   cells (docling does not OCR programmatic tables).

A cheaper interim improvement (not docling-exact, but closes some diff): better
geometric reconstruction — detect header rows, merge obvious spanning cells, and
handle the multi-line header cells that currently shatter into many columns.

## Blocker 3 — RTL / bidi (Arabic)

`right_to_left_01/02/03`. Two compounding issues: (a) reading order — Latin runs
embedded in RTL text and the overall right-to-left flow are emitted left-to-right
(`Python و ة R` vs `R و Python`); we'd need Unicode bidi reordering of each line.
(b) Arabic shaping — pdfium returns presentation-form / decomposed sequences that
differ from docling's (`اإل` vs `الإ`), needing NFC-ish normalization of the
Arabic block. Both are self-contained but specialized; lower priority than 1–2.

## Smaller items

- **Duplicate glyphs** (`amt_handbook`: `T he`, `F Figure 7-26 6`). pdfium emits
  doubled glyphs for some bold/overlapping text; needs de-duplication of
  overlapping cells.
- **Code regions** → fenced ```` ``` ```` blocks with the caption *after* (code
  captions trail; figure captions lead). Pairs with Blocker 1 for the code text.
