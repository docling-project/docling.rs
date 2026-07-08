# Pure-Rust PDF text parser — WIP notes & roadmap

Goal: replace pdfium's text-extraction layer with a pure-Rust parser whose
character cells match docling's `docling-parse` C++ parser, so the PDF pipeline
can reach docling byte-conformance (and eventually drop pdfium for text — pdfium
would stay only for page rasterisation).

## Why (the measured case)

docling-parse and pdfium disagree on glyph geometry at exactly the points that
break conformance: pdfium gives generated **spaces a zero-width box**, gives
**combining diacritics a real-width box**, and lands ligature/fraction glyphs at
different x. A ceiling experiment — injecting docling-parse's own cells into our
pipeline (keeping our layout + TableFormer) — measured:

| Cells used | Exact |
|---|---|
| pdfium (baseline) | 4/14 |
| docling-parse cells injected | **6/14** (amt + right_to_left_01 flip to exact) |
| + the one `right_to_left_02` `11`-page-number layout fix | **7/14 = 50%** |

So the text parser is the lever; 50% is reachable.

## What's built (`crates/docling-pdf/src/textparse.rs`)

The **default** text layer (opt out with `DOCLING_PDFIUM_TEXT=1`). Pdfium still
provides page rasters + word/code cells; the parser replaces prose line cells,
fed through the existing `dp_lines` sanitizer.

- Content-stream interpreter: `cm/q/Q`, `BT/ET`, `Tf/Td/TD/Tm/T*/Tc/Tw/Tz/TL/Ts`,
  `Tj/TJ/'/"` with text + graphics matrices.
- **Advance-width geometry** from the font (spaces get real width; combining
  marks get zero advance) — the whole point.
- Fonts: Type0/CID + Identity-H (`/W`, `/DW`), simple Type1/TrueType
  (`/FirstChar`+`/Widths`, `/MissingWidth`), FontDescriptor ascent/descent.
- Encodings: ToUnicode CMap (`bfchar`, `bfrange` scalar **and** array forms,
  structural tokenizer for back-to-back `<..><..>` hex); WinAnsi + MacRoman base
  encodings; `/Differences` via a small Adobe-glyph-name subset.

## Current result: 6/14 strict, 7/14 whitespace-normalized

Byte-exact: `code_and_formula`, `multi_page`, `picture_classification`,
`2305.03393v1-pg9`, **`right_to_left_01`**, **`right_to_left_02`** (pdfium gives
4/14). The parser is the default text layer; set `DOCLING_PDFIUM_TEXT=1` to fall
back to pdfium. A page with no parseable text layer falls back automatically, so
scanned/edge-case pages are unaffected.

`amt` is the 7th under the **whitespace-normalized** metric: its only diff is
docling's spurious double space before the `1⁄4` fraction, where our single-space
rendering is the more faithful one (blocker B). The scoring scripts now report
both **strict** and **whitespace-normalized** counts (`conformance.sh`,
`pdf_groundtruth.sh`; `compare.sh` notes spacing-only diffs).

The rest are heavy multi-column docs, not byte-exact for layout/table reasons
independent of the text parser (`normal_4pages` improved 74→54 after the Korean
quote fix below).

## Blocker A — DONE (commit a036133)

A lone punctuation glyph set in a separate punctuation font now bridges fonts
next to RTL text, so the Arabic sentence period attaches (`العمل.`).
`right_to_left_01` is **EXACT**.

## Completeness validation — "nothing is skipped"

`scripts/parser_completeness.py` compares, per PDF, the *multiset* of characters
docling-parse emits against the parser's (alignment-free, so garbled RTL doesn't
confuse it). It surfaced two whole classes of silently-dropped text, both fixed:

1. **Form XObject text** (`Do` operator). Bulk body text in heavy PDFs lives
   inside a Form XObject, reached only via `Do`; the parser walked just the page
   content stream and dropped it (2206 p1 dropped ~9000 chars). `page_glyphs` is
   now a recursive `run_content` that decodes the form's stream, concatenates its
   `/Matrix`, and recurses with the form's own `/Resources` (depth-guarded).

2. **Glyph-name fallback.** docling emits an unresolvable `/Differences` glyph
   name verbatim (`/g115`, `/SM590000`) when a subsetted font has no usable
   Unicode mapping (redp5110's bulleted list, IBM BookMaster). The parser dropped
   them (low codes outside WinAnsi). `decode_code` now mirrors docling for
   synthetic GID-style names; `glyph_name_to_char` was widened to the AGL
   algorithmic subset (single letters, digit/punctuation names, `.suffix`).

After both fixes every previously text-exact fixture stays `dropped=0
invented=0`, and the heavy docs are near-complete (redp5110 33070/33073 chars).
The residue is the punctuation-normalization class below.

## Blocker B — amt fraction double space (ROOT-CAUSED; blocked on font metrics)

Diff: `up to  1 / 4` / `from  1 / 4` have a **double** space; `1 / 6` and
`3 / 8` stay single. Fully traced through docling's contraction:

- The fractions are separate glyphs (`1`, `⁄`, `4`); the `⁄` (U+2044) is in a
  **different font**, so the contraction fragments there. The numerator `1` is a
  small **raised** glyph (~4 pt above the baseline).
- docling **absorbs** the raised `1` into the preceding line. Because the
  Euclidean corner gap (≈4.0, dominated by the vertical raise) exceeds
  `delta = avg·0.33`, `merge_with` inserts a *generated* space — on top of the
  explicit space char → **double**. Whether it absorbs hinges on `eps0 = avg·1.0`
  vs that ≈4.0 gap, a knife-edge that flips per line on `avg_char_width`. ¼'s
  lines clear it; ⅙/⅜'s don't (their numerator stays a standalone cell → single).

- **Why the parser misses it:** docling boxes every glyph with the embedded
  font's *typographic* ascent/descent (TrueType **OS/2 sTypoAscender/Descender**,
  e.g. Times 693/−216), proven by every glyph on a line sharing one box height
  (8.47 pt) while the raised fraction digit gets its own (4.7 pt). The parser
  uses the PDF descriptor's `/Ascent 897 /Descent −250` (≈30 % taller), so the
  loose box hangs ~0.3 pt lower and the gap reads 4.30 instead of 4.00 — just
  past `eps0`, so nothing absorbs and every fraction stays single.

- **Attempted fix + why reverted:** reading OS/2 metrics from `/FontFile2` (a
  compact sfnt reader) moved the gap to 4.17 and flipped *one* of the two ¼'s to
  double — but it **regressed `right_to_left_01`** (Arabic box geometry shifted)
  and still didn't fix the second ¼. A faithful fix needs the embedded font's
  exact per-font metrics *and* a way to keep the Arabic path stable — i.e. the
  box-geometry layer has to match docling globally, not per-case. Left for a
  dedicated font-metrics effort; a magic-number nudge is too fragile to ship.

## Blocker C — right_to_left_02 — DONE (byte-exact)

`right_to_left_02` went 8 → **0** (exact) over two fixes:

1. **Kashida over-emission.** The parser emitted ~25 extra `و` (`قويووووة` vs
   `قويوووة`): the scanned-garbled Arabic re-stamps a waw elongation segment
   offset by ≪ its width (overprint for weight), and the line sanitizer's
   ligature-recompose was appending the duplicate. `line_cells` now drops a
   same-character glyph re-stamped at an *offset* overlapping box (>0.1 offset so
   a ligature expansion at the *identical* box — `ﬀ`→`ff` — is still recomposed;
   verified 2305-pg9 stays exact).
2. **Layout `11`.** The page false-detected an empty right-margin picture
   (score 0.40) and ordered the orphan-recovered bottom page number `11` last,
   while docling emits no picture and floats `11` to the front.
   `drop_false_pictures` removes an empty picture with score < 0.5 (real empty
   figures are all ≥ 0.86, so none are touched), and `assemble_page` stable-sorts
   a small digit-only margin region (`is_page_number`) to the front of reading
   order. Both are corpus-safe (only this fixture has a page number emitted as a
   line; the rest are filtered furniture) and verified non-regressing.

## Korean quote normalization — DONE (normal_4pages 74→54)

docling renders the Korean (Hangul) font's double curly-quote glyph as a single
straight `'` (`‘코로나’`), not the Latin `"`, while keeping `"` for genuine
`quotedbl` glyphs (2305). `clean_text` now keys on Hangul syllables: `“ ”`→`'`
in Hangul text, `"` otherwise — so normal_4pages's quotes match without
disturbing 2305. (normal_4pages is still non-exact for layout reasons: heading
numbering and footnote reading order.)

## Future improvements (validated by the completeness pass)

- **amt fraction double space** (blocker B) — needs the embedded font's OS/2 typo
  metrics to reproduce docling's box geometry, but that globally entangles with
  RTL geometry (regressed rtl_01 when trialled). Our single-spaced output is the
  more faithful rendering; the whitespace-normalized conformance metric credits
  it. A dedicated font-metrics layer is the real fix.
- **Embedded TrueType `cmap`/`post` recovery.** Identity-H fonts with a *stub*
  ToUnicode (only a codespacerange) need the embedded font program's cmap to
  recover Unicode (2206 p1 drops ~591 caps). Requires a TrueType table reader.

## Roadmap
1. ~~Blocker A~~ — DONE (rtl_01 exact).
2. ~~Make the parser the conformance default~~ — DONE (opt-out via
   `DOCLING_PDFIUM_TEXT`).
3. ~~Blocker C (right_to_left_02 kashida + `11` layout)~~ — DONE (exact).
4. ~~Korean quote normalization~~ — DONE (normal_4pages 74→54).
5. **Now: 6/14 strict, 7/14 whitespace-normalized.** Blocker B (amt) needs a
   font-metrics layer for strict 7/14.
6. **Drop pdfium's text path — DONE (parser is the sole text source).** The
   parser was already the sole *prose* source (item 2); **word** and **code**
   cells now come from it too, so pdfium does only rasterisation + links.
   *Word cells:* the insight is that docling-parse's `word_cells` are exactly the line
   contraction's runs, split at the points where `merge_with` inserts a separator
   space (`delta < gap`). So instead of the legacy gap-heuristic
   (`words_from_glyphs`, which blob-joined TJ-spaced runs like
   `highlydiversesetoftables…`), `dp_lines::word_cells` tracks per-word segments
   *through the same proven contraction* that already reproduces `textline_cells`.
   On `2305` pg9 it now emits **377/377 words byte-identical to the docling-parse
   `word_cells` oracle** (x-coords matching to 0.01 pt; the only residue is the
   ~3 pt-taller vertical box from the blocker-B font-metrics gap, which the
   TableFormer matcher tolerates).

   Result vs the docling **groundtruth**: strictly *better* on the heavy
   multi-column docs (`redp5110` +22, `2206.01062` +16, `2203.01017v2` +7
   groundtruth-matching lines), neutral on the rest, **no regression** anywhere —
   so it's the default (opt out with `DOCLING_PDFIUM_WORDS`, or `DOCLING_PDFIUM_TEXT`
   for full pdfium text). The 7 affected committed snapshots were regenerated to
   the now docling-faithful output (e.g. `bold ,` / `x 2`, which docling-parse's
   own `word_cells` confirm — the old `bold,` / `x2` were pdfium punct-gluing).

   **Code cells off pdfium too — DONE (pdfium's text path fully retired).** The
   first attempt at parser code cells used the space-glyph-only grouping, which
   dropped the inter-token spaces pdfium recovers (`function add` → `functionadd`)
   because the parser emits no space glyphs — a source space is a positioning
   *gap*. The fix is a third grouping mode, `Grouping::CodeGap`: split on the
   inter-glyph gap (a space wherever it exceeds ~0.25× the line height) but with
   **no punctuation glue**, so a real gap always splits (`et al. 2000`, not
   `et al.2000` — the prose glue rule is wrong for code) while genuinely touching
   tokens stay joined (`add(a,` / `b)`). The parser's clean advance boxes make the
   gap reliable here, where pdfium's overhanging loose boxes would over-split
   (`f un c t i o n`) — which is why pdfium keeps the space-glyph path.
   `code_and_formula` is byte-exact (`function add(a, b) { return a + b; }
   console.log(add(3, 5));`); it's the default (opt out with `DOCLING_PDFIUM_WORDS`
   or `DOCLING_PDFIUM_TEXT`). Two snapshots (`2305` llncsdoc, `redp5110`) drift in
   garbled multi-column LaTeX/SQL "code" regions — conformance-**neutral** vs the
   groundtruth (337/337, 175/175), regenerated to the parser output.

   pdfium now does **only** page rasterisation + link annotations; all text
   (prose, words, code) comes from the pure-Rust parser.

## Tooling (under `scripts/`)

- `parser_completeness.py` — per-PDF char-frequency diff docling-parse vs the
  parser; the "nothing skipped" validator that surfaced the Form-XObject and
  glyph-name drops. Run after `cargo build --example textparse_glyphs`.
- `dump_parse_cells.py` — docling-parse textline cells → JSON/TSV (the oracle).
- `docling_dump_all.py` — full docling items (label/page/bbox/text) per PDF.
- `textparse_dump` example — the Rust parser's cells; `TSV_OUT=1` emits the
  injection TSV for ceiling experiments.
- `textparse_glyphs` example — `<pdf> <page>`: raw glyph chars (stdout) + boxes
  (stderr), for char-cell comparison.
- `probe_page` example — `<pdf> <page>`: operator histogram, fonts (with
  BaseFont), and XObject subtypes for a page (debugging dropped text).

Also in this branch: `assemble::add_orphan_regions` — docling-parity orphan-cell
clustering (emits text the layout detector missed, e.g. amt's stray `.`).
