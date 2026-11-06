//! Port of docling-parse's line-cell sanitizer
//! (`src/parse/page_item_sanitators/cells.h` → `create_line_cells` /
//! `contract_cells_into_lines_v1`). It merges per-glyph char cells into line
//! cells via a 3-pass contraction — left-to-right, right-to-left, then
//! left-to-right with reverse — using corner-distance adjacency and inserting at
//! most one space per merge. This reproduces docling-parse's inter-word spacing
//! (justified double spaces, the space before a `:`, and RTL ordering) that an
//! ad-hoc gap-heuristic reconstruction can't.
//!
//! Geometry uses native PDF coordinates (y increases upward); each cell carries
//! its four transformed corners r0=bottom-left, r1=bottom-right, r2=top-right,
//! r3=top-left, exactly like `page_cell.h`.

use crate::pdfium_backend::{Glyph, TextCell};

// config.h: the factors that actually bind for line cells.
const MERGE: f64 = 1.0; // line_space_width_factor_for_merge (adjacency gate)
const MERGE_WITH_SPACE: f64 = 0.33; // line_space_width_factor_for_merge_with_space

// create_word_cells: words contract under their own, tighter factors — the
// adjacency gate is word_space_width_factor_for_merge (0.33) and the space
// threshold is twice that (2.0 * 0.33), which the 0.33 gate can never exceed,
// so a word cell never contains an inserted space. Space glyphs are hard
// word-boundary barriers during the contraction (`applicable`'s
// `block_spaces`) and are erased only afterwards, as docling-parse does.
const WORD_MERGE: f64 = 0.33; // word_space_width_factor_for_merge
const WORD_MERGE_WITH_SPACE: f64 = 2.0 * WORD_MERGE;
const H_TOL: f64 = 1.0; // horizontal_cell_tolerance (ligature eps_d1 relaxation)

#[derive(Clone)]
struct Cell {
    text: String,
    rx0: f64,
    ry0: f64, // bottom-left
    rx1: f64,
    ry1: f64, // bottom-right
    rx2: f64,
    ry2: f64, // top-right
    rx3: f64,
    ry3: f64, // top-left
    ltr: bool,
    active: bool,
    lig_carry: bool, // last_merged_cell_was_ligature
    font: u64,       // hash of docling-parse's font name (for enforce_same_font)
    // Cached invariants of `text` / the quad, maintained by `build_cells` and
    // `merge_with`. The contraction is quadratic in merge attempts, and
    // recomputing these per attempt (trim/char-count/ligature scans over the
    // *growing* line text, min/max over the quad) dominated large-page
    // parsing — the caches turn every attempt into flag reads.
    blank: bool,   // text.trim().is_empty()
    lig: bool,     // is_ligature(&text)
    fb: bool,      // any char in U+FB00..=U+FB06 (the range half of is_ligature)
    nchars: usize, // text.chars().count()
    b_l: f64,      // bounds(): axis-aligned quad extremes
    b_r: f64,
    b_b: f64,
    b_t: f64,
    /// The pen's origin and the font size, `[x, y, size]` ([`Glyph::pen`]),
    /// for the word contraction's baseline test (#921).
    pen: Option<[f64; 3]>,
    /// The glyph's font is a math face ([`Glyph::math_font`]).
    math_font: bool,
}

impl Cell {
    /// Length of the bottom edge (baseline advance) — `page_cell.h::length`.
    fn length(&self) -> f64 {
        ((self.rx1 - self.rx0).powi(2) + (self.ry1 - self.ry0).powi(2)).sqrt()
    }

    /// Running mean glyph advance over the whole accumulated cell.
    fn avg_char_width(&self) -> f64 {
        if self.nchars > 0 {
            self.length() / self.nchars as f64
        } else {
            0.0
        }
    }

    /// Distance from this cell's bottom-right corner to `other`'s bottom-left.
    fn gap(&self, other: &Cell) -> f64 {
        ((self.rx1 - other.rx0).powi(2) + (self.ry1 - other.ry0).powi(2)).sqrt()
    }

    /// `is_adjacent_to`: both the bottom-corner gap (`< eps0`) and the top-corner
    /// gap (`< eps1`) must be small. The vertical component keeps different
    /// baselines/lines from merging.
    fn adjacent(&self, other: &Cell, eps0: f64, eps1: f64) -> bool {
        let d0 = self.gap(other);
        let d1 = ((self.rx2 - other.rx3).powi(2) + (self.ry2 - other.ry3).powi(2)).sqrt();
        d0 < eps0 && d1 < eps1
    }

    /// Punctuation/space cells are bidi-neutral bridges.
    fn same_orientation(&self, other: &Cell) -> bool {
        self.ltr == other.ltr || is_punct_or_space(&self.text) || is_punct_or_space(&other.text)
    }

    /// `merge_with`: absorb `other` (which lies to this cell's right). Insert at
    /// most one separator space when the gap exceeds `delta`. RTL prepends.
    ///
    /// `euclidean` picks the gap measure: docling-parse uses the **Euclidean
    /// corner distance** `d0` (the same one `is_adjacent_to` uses). The pure-Rust
    /// parser produces clean advance boxes, so it uses `d0` to match docling
    /// byte-for-byte. pdfium's loose boxes overhang (an `f` extends left and
    /// overlaps its neighbour), which a Euclidean distance reads as a false
    /// positive gap and over-inserts spaces (`Self` → `Sel f`); that path keeps
    /// the **signed horizontal gap** instead.
    fn merge_with(&mut self, other: &Cell, delta: f64, euclidean: bool) {
        let gap = if euclidean {
            self.gap(other)
        } else {
            other.rx0 - self.rx1
        };
        let space = delta < gap;
        if !self.ltr || !other.ltr {
            if space {
                self.text.insert(0, ' ');
            }
            self.text = format!("{}{}", other.text, self.text);
            self.ltr = false;
        } else {
            if space {
                self.text.push(' ');
            }
            self.text.push_str(&other.text);
            self.ltr = true;
        }
        // Extend the right edge to `other`.
        self.rx1 = other.rx1;
        self.ry1 = other.ry1;
        self.rx2 = other.rx2;
        self.ry2 = other.ry2;
        // Cache upkeep. Blankness and the ligature char-range scan distribute
        // over concatenation (the inserted separator is whitespace and not in
        // the range); the equality patterns ("ff", "fi", …) do NOT — "fi" is a
        // ligature cell, "fiction" is not — so a short merged text recomputes
        // exactly and a longer one (which no equality pattern can match) falls
        // back to the distributed range flag alone.
        self.blank = self.blank && other.blank;
        self.nchars += other.nchars + usize::from(space);
        self.fb = self.fb || other.fb;
        self.lig = if self.text.len() <= 3 {
            is_ligature(&self.text)
        } else {
            self.fb
        };
        let (l, r, b, t) = quad_bounds(self);
        self.b_l = l;
        self.b_r = r;
        self.b_b = b;
        self.b_t = t;
    }
}

/// Axis-aligned bounds of a cell's quad, `(l, r, b, t)` in PDF points (y-up),
/// from the cache `merge_with` maintains.
fn bounds(c: &Cell) -> (f64, f64, f64, f64) {
    (c.b_l, c.b_r, c.b_b, c.b_t)
}

/// The cached bounds' source of truth: min/max over the quad corners.
fn quad_bounds(c: &Cell) -> (f64, f64, f64, f64) {
    let xs = [c.rx0, c.rx1, c.rx2, c.rx3];
    let ys = [c.ry0, c.ry1, c.ry2, c.ry3];
    let fold = |it: &[f64], f: fn(f64, f64) -> f64| it.iter().copied().reduce(f).unwrap();
    (
        fold(&xs, f64::min),
        fold(&xs, f64::max),
        fold(&ys, f64::min),
        fold(&ys, f64::max),
    )
}

/// Is another active cell painted inside the horizontal gap between `i` and
/// `j`? The contraction walks cells in **stream** order, and a generator that
/// draws a line's bold runs after its regular text leaves them as later cells
/// — the space tolerance would then stitch `C.[ ]Zur Wahrung …` straight
/// across the hole where the bold `6.` sits, and the stranded token ends up at
/// the line's end ("… wenn Sie die Mitteilung 6."). An occupied gap is not a
/// gap. Space-only cells never block (they *are* the gap), and the scan is
/// skipped entirely for glyph-adjacent merges (no room for anything).
fn gap_occupied(cells: &[Cell], i: usize, j: usize) -> bool {
    let (al, ar, ab, at) = bounds(&cells[i]);
    let (bl, br, bb, bt) = bounds(&cells[j]);
    let (gl, gr) = if ar <= bl { (ar, bl) } else { (br, al) };
    if gr - gl < 0.5 {
        return false; // touching or overlapping — nothing fits in between
    }
    let (band_b, band_t) = (ab.min(bb), at.max(bt));
    cells.iter().enumerate().any(|(k, c)| {
        if k == i || k == j || !c.active || c.blank {
            return false;
        }
        let (cl, cr, cb, ct) = bounds(c);
        // Vertically on this line: most of the candidate inside the pair's band.
        let overlap = (ct.min(band_t) - cb.max(band_b)).max(0.0);
        overlap > 0.5 * (ct - cb).max(f64::EPSILON)
            // Horizontally: real ink inside the gap interval.
            && cr.min(gr) - cl.max(gl) > 0.1
    })
}

/// `applicable_for_merge`: both active and same reading orientation. A different
/// font normally blocks the merge (keeps a bold label and its value as separate
/// line cells). On the clean-box parser path, **punctuation/space cells bridge
/// fonts** so a sentence period set in a separate punctuation font joins its word
/// instead of fragmenting (`العمل .` → `العمل.`); letters still enforce the font.
fn applicable(a: &Cell, b: &Cell, parser: bool, block_spaces: bool) -> bool {
    if !a.active || !b.active {
        return false;
    }
    // Word mode (`block_spaces`): a space glyph is a hard word-boundary barrier
    // that never merges in either direction; the space cells themselves are
    // erased after the contraction (`create_word_cells`).
    if block_spaces && (is_all_space(&a.text) || is_all_space(&b.text)) {
        return false;
    }
    // A lone punctuation glyph (not a space) set in a separate punctuation font
    // bridges fonts so it joins its word — but only next to RTL text. In LTR a
    // different-font punctuation (e.g. a bold `:`) is a real run boundary docling
    // keeps spaced (`Laboratories :`); in Arabic the sentence period sits in a
    // Latin punctuation font yet attaches (`العمل.`). Parser path only.
    let lone_punct = |s: &str| {
        let mut ch = s.chars();
        matches!(ch.next(), Some(c) if c != ' ' && is_punct_or_space(&c.to_string()))
            && ch.next().is_none()
    };
    let punct_bridge =
        parser && ((lone_punct(&a.text) && !b.ltr) || (lone_punct(&b.text) && !a.ltr));
    let font_neutral = a.lig || b.lig || punct_bridge;
    if a.font != 0 && b.font != 0 && a.font != b.font && !font_neutral {
        return false;
    }
    a.same_orientation(b)
}

/// Left-to-right pass: `i` ascending accumulates cells to its right.
fn pass_ltr(cells: &mut [Cell], allow_reverse: bool, euclidean: bool, p: Factors) {
    for i in 0..cells.len() {
        if !cells[i].active {
            continue;
        }
        let mut j = i + 1;
        while j < cells.len() {
            if !applicable(&cells[i], &cells[j], euclidean, p.block_spaces) {
                break;
            }
            let i_lig = cells[i].lig || cells[i].lig_carry;
            let j_lig = cells[j].lig || cells[j].lig_carry;
            let d0 = cells[i].avg_char_width() * p.merge;
            let d1 = cells[i].avg_char_width() * p.merge_with_space;
            let adj_d1 = d0 + if i_lig || j_lig { H_TOL } else { 0.0 };
            if cells[i].adjacent(&cells[j], d0, adj_d1) && !gap_occupied(cells, i, j) {
                let other = cells[j].clone();
                cells[i].merge_with(&other, d1, euclidean);
                cells[i].lig_carry = other.lig;
                cells[j].active = false;
                j += 1; // i keeps absorbing the next cell to its right
            } else if allow_reverse
                && cells[j].adjacent(&cells[i], d0, adj_d1)
                && !gap_occupied(cells, j, i)
            {
                let other = cells[i].clone();
                cells[j].merge_with(&other, d1, euclidean);
                cells[j].lig_carry = other.lig;
                cells[i].active = false;
                break; // i is consumed
            } else {
                break;
            }
        }
    }
}

/// Right-to-left pass: `i` descending; its immediate left neighbour `i-1`
/// absorbs it (then the outer loop continues leftward through the absorber).
fn pass_rtl(cells: &mut [Cell], euclidean: bool, p: Factors) {
    let n = cells.len();
    for k in 0..n {
        let i = n - 1 - k;
        if !cells[i].active || i == 0 {
            continue;
        }
        let j = i - 1;
        if !applicable(&cells[i], &cells[j], euclidean, p.block_spaces) {
            continue;
        }
        let i_lig = cells[i].lig || cells[i].lig_carry;
        let j_lig = cells[j].lig || cells[j].lig_carry;
        let d0 = cells[i].avg_char_width() * p.merge;
        let d1 = cells[i].avg_char_width() * p.merge_with_space;
        let adj_d1 = d0 + if i_lig || j_lig { H_TOL } else { 0.0 };
        if cells[j].adjacent(&cells[i], d0, adj_d1) && !gap_occupied(cells, j, i) {
            let other = cells[i].clone();
            cells[j].merge_with(&other, d1, euclidean);
            cells[j].lig_carry = other.lig;
            cells[i].active = false;
        }
    }
}

/// The contraction's tuning: the adjacency-gate and space-insertion factors
/// (per `sanitize_bbox`'s callers) plus the word mode's space barrier.
#[derive(Clone, Copy)]
struct Factors {
    merge: f64,
    merge_with_space: f64,
    block_spaces: bool,
}

const LINE_FACTORS: Factors = Factors {
    merge: MERGE,
    merge_with_space: MERGE_WITH_SPACE,
    block_spaces: false,
};
const WORD_FACTORS: Factors = Factors {
    merge: WORD_MERGE,
    merge_with_space: WORD_MERGE_WITH_SPACE,
    block_spaces: true,
};

/// True when the cell's text is entirely whitespace (`utils::string::is_space`).
fn is_all_space(s: &str) -> bool {
    !s.is_empty() && s.chars().all(char::is_whitespace)
}

fn contract(cells: &mut Vec<Cell>, euclidean: bool, p: Factors) {
    pass_ltr(cells, false, euclidean, p);
    cells.retain(|c| c.active);
    pass_rtl(cells, euclidean, p);
    cells.retain(|c| c.active);
    pass_ltr(cells, true, euclidean, p);
    cells.retain(|c| c.active);
}

/// Build per-glyph char cells from a page's glyph stream (shared by the line and
/// word paths): drop degenerate spaces, recompose ligatures, init word segments.
fn build_cells(glyphs: &[Glyph], euclidean: bool) -> Vec<Cell> {
    let mut cells: Vec<Cell> = Vec::new();
    for g in glyphs {
        // Use the loose box (uniform font ascent/descent + advance) so adjacent
        // glyphs share a top edge, matching docling-parse's `compute_rect`.
        if !g.ll.is_finite() {
            continue;
        }
        // The char cell's quad: the loose rectangle for upright text, the
        // glyph's own rotated quad otherwise (#528) — the contraction below
        // works on corners and edge lengths, so it reads a 90°/180°/270° line
        // in its own reading order, exactly like docling-parse.
        let q = g.quad.map(|q| q.map(f64::from)).unwrap_or_else(|| {
            let (l, b, r, t) = (g.ll as f64, g.lb as f64, g.lr as f64, g.lt as f64);
            [l, b, r, b, r, t, l, t]
        });
        // Drop *degenerate* space glyphs (zero-width loose box): pdfium's generated
        // spaces get a zero-width box at the wrong baseline that breaks the
        // corner-distance adjacency. Without them the inter-word gap drives
        // `merge_with`'s space insertion. Spaces with a real width are kept (they
        // carry justified double-space information). The width is the baseline
        // edge — a rotated glyph's x extent is its em height.
        if g.ch == ' ' && (q[2] - q[0]).hypot(q[3] - q[1]) < 0.5 {
            continue;
        }
        // Recompose a ligature: pdfium decomposes one font glyph (Latin fi/ffi,
        // Arabic lam-alef) into several chars at the *same* loose box. Append them
        // into one cell so the contraction never inserts a space inside it.
        // "Same box": both baseline corners match as points, upright or
        // rotated. A ligature's chars share one box, baseline included; glyphs
        // stacked one under another at the same x (a chart's y-axis ticks
        // `3`…`8`, each placed by its own `cm`) are separate cells — comparing
        // the x span alone glued those into one `345678` cell carrying the
        // first tick's box (#609), where docling-parse keeps six.
        if let Some(last) = cells.last_mut() {
            let near =
                |ax: f64, ay: f64, bx: f64, by: f64| (ax - bx).abs() < 0.5 && (ay - by).abs() < 0.5;
            if near(last.rx0, last.ry0, q[0], q[1]) && near(last.rx1, last.ry1, q[2], q[3]) {
                // Overprint duplicate: the *same* character re-stamped, offset by a
                // fraction of its width (a kashida/elongation segment re-drawn for
                // weight). docling-parse drops it; appending over-counts
                // (right_to_left_02's `قويووووة` vs `قويوووة`). Require a real offset
                // (> 0.1) so a ligature expansion — which decomposes one glyph into
                // several chars at the *identical* box (`ﬀ`→`ff`, diff ≈ 0) — is still
                // recomposed; real doubled letters sit a full advance apart (> 0.5).
                let offset = match g.quad {
                    None => (q[0] - last.rx0).abs(),
                    Some(_) => (q[0] - last.rx0).hypot(q[1] - last.ry0),
                };
                if euclidean && offset > 0.1 && last.text.ends_with(g.ch) {
                    continue;
                }
                last.text.push(g.ch);
                last.ltr = !is_right_to_left(&last.text);
                last.blank = last.blank && g.ch.is_whitespace();
                last.nchars += 1;
                // Recomposed ligature cells stay tiny; recomputing is exact.
                last.fb = last.fb || (0xFB00..=0xFB06).contains(&(g.ch as u32));
                last.lig = is_ligature(&last.text);
                continue;
            }
        }
        let text = g.ch.to_string();
        let ltr = !is_right_to_left(&text);
        let blank = g.ch.is_whitespace();
        let lig = is_ligature(&text);
        let fb = (0xFB00..=0xFB06).contains(&(g.ch as u32));
        cells.push(Cell {
            text,
            rx0: q[0],
            ry0: q[1],
            rx1: q[2],
            ry1: q[3],
            rx2: q[4],
            ry2: q[5],
            rx3: q[6],
            ry3: q[7],
            ltr,
            active: true,
            lig_carry: false,
            font: g.font,
            blank,
            lig,
            fb,
            nchars: 1,
            b_l: (g.ll as f64).min(g.lr as f64),
            b_r: (g.ll as f64).max(g.lr as f64),
            b_b: (g.lb as f64).min(g.lt as f64),
            b_t: (g.lb as f64).max(g.lt as f64),
            pen: g.pen.map(|p| p.map(f64::from)),
            math_font: g.math_font,
        });
    }
    cells
}

/// Build line cells from a page's glyph stream under the process's word-gap
/// rule (`DOCLING_RS_PDF_WORD_GAP`, #921).
pub(crate) fn line_cells(glyphs: &[Glyph], page_h: f32, euclidean: bool) -> Vec<TextCell> {
    line_and_word_cells_with(glyphs, page_h, euclidean, WordGap::from_env()).0
}

/// Build **word** cells from a page's glyph stream via docling-parse's
/// `create_word_cells`: a second contraction over the same char cells under the
/// word factors — adjacency gate 0.33 (vs the line's 1.0), so a gap wide enough
/// to become a line-internal space still merges glyphs into one spaceless word
/// when it stays under the gate (tight-set Korean: line `1군 감염병`, word
/// `1군감염병`); real space glyphs are hard barriers and are erased afterwards.
/// These are the per-word tokens TableFormer matches table-grid cells against.
pub(crate) fn word_cells(glyphs: &[Glyph], page_h: f32, euclidean: bool) -> Vec<TextCell> {
    line_and_word_cells_with(glyphs, page_h, euclidean, WordGap::from_env()).1
}

/// Build the line cells **and** the word cells from one shared glyph build
/// under docling-parse ≤ 6's contraction ([`WordGap::Legacy`]): the char
/// cells are constructed once and contracted twice, under the line factors
/// and the word factors respectively — exactly docling-parse's
/// `create_line_cells` + `create_word_cells` pair. The tests pin the legacy
/// contraction through it; production goes through the process's rule.
#[cfg(test)]
pub(crate) fn line_and_word_cells(
    glyphs: &[Glyph],
    page_h: f32,
    euclidean: bool,
) -> (Vec<TextCell>, Vec<TextCell>) {
    line_and_word_cells_with(glyphs, page_h, euclidean, WordGap::Legacy)
}

/// [`line_and_word_cells`] under the word-gap rule `gap` (#921): the legacy
/// contraction, or docling-parse ≥ 7's line runs.
pub(crate) fn line_and_word_cells_with(
    glyphs: &[Glyph],
    page_h: f32,
    euclidean: bool,
    gap: WordGap,
) -> (Vec<TextCell>, Vec<TextCell>) {
    let built = build_cells(glyphs, euclidean);
    if gap != WordGap::Legacy {
        return contract_runs(&built, gap, page_h);
    }
    let to_text_cell = |c: Cell| {
        let l = c.rx0.min(c.rx1).min(c.rx2).min(c.rx3) as f32;
        let r = c.rx0.max(c.rx1).max(c.rx2).max(c.rx3) as f32;
        let top = c.ry0.max(c.ry1).max(c.ry2).max(c.ry3) as f32;
        let bot = c.ry0.min(c.ry1).min(c.ry2).min(c.ry3) as f32;
        TextCell {
            text: c.text,
            l,
            t: page_h - top,
            r,
            b: page_h - bot,
        }
    };
    // Word run: docling-parse's `create_word_cells` order — copy the char
    // cells, contract (`sanitize_bbox`), *then* erase the spaces. The space
    // glyphs stay in the stream during the contraction, where `applicable`'s
    // `block_spaces` makes each one a hard word boundary; they are dropped
    // afterwards by the blank filter below. Filtering them out up front let
    // the 0.33 gate alone decide word breaks, which glues tight-set Latin
    // (`MODE` + `to`: 1.8 pt apart under a 2.0 pt gate → `MODEto`) and the
    // thin-spaced Korean docling-parse ≥ 7 keeps apart (`1군` `감염병`).
    let mut word_run: Vec<Cell> = built.clone();
    let mut cells = built;
    contract(&mut cells, euclidean, LINE_FACTORS);
    let lines = to_text_cells(cells, to_text_cell);
    contract(&mut word_run, euclidean, WORD_FACTORS);
    word_run.retain(|c| !c.text.trim().is_empty());
    let words = to_text_cells(word_run, to_text_cell);
    (lines, words)
}

/// Map the contracted cells into a freshly allocated, exactly sized vector.
///
/// A plain `into_iter().map(..).collect()` would take std's in-place-collect
/// path and hand back the *input's* allocation: one ~150-byte [`Cell`] slot per
/// glyph of the page, reinterpreted as ~4× as many 40-byte [`TextCell`] slots,
/// while the contraction leaves a few glyphs per cell. Callers keep these
/// vectors for the whole document (the text-layer path holds every page), so
/// the dead capacity added up to ~30× the cells' real size — gigabytes on a
/// long table-heavy PDF.
fn to_text_cells(cells: Vec<Cell>, f: impl Fn(Cell) -> TextCell) -> Vec<TextCell> {
    let mut out = Vec::with_capacity(cells.len());
    out.extend(cells.into_iter().map(f));
    out
}

fn is_rtl_char(c: char) -> bool {
    let ch = c as u32;
    (0x0600..=0x06FF).contains(&ch)
        || (0x0750..=0x077F).contains(&ch)
        || (0x08A0..=0x08FF).contains(&ch)
        || (0xFB50..=0xFDFF).contains(&ch)
        || (0xFE70..=0xFEFF).contains(&ch)
        || (0x0590..=0x05FF).contains(&ch)
        || (0xFB1D..=0xFB4F).contains(&ch)
        || (0x0700..=0x074F).contains(&ch)
        || (0x0780..=0x07BF).contains(&ch)
        || (0x07C0..=0x07FF).contains(&ch)
}

/// All codepoints are RTL-script (matches `string.h::is_right_to_left`).
fn is_right_to_left(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_rtl_char)
}

/// A single-codepoint punctuation/space cell (matches `string.h`).
fn is_punct_or_space(s: &str) -> bool {
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return false;
    };
    if matches!(
        c,
        ' ' | '\t'
            | '\n'
            | '\r'
            | '\u{0c}'
            | '\u{0b}'
            | '.'
            | ','
            | ';'
            | ':'
            | '!'
            | '?'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '\''
            | '"'
            | '`'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{201c}'
            | '\u{201d}'
            | '-'
            | '\u{2013}'
            | '\u{2014}'
            | '_'
            | '/'
            | '\\'
            | '|'
            | '@'
            | '#'
            | '%'
            | '&'
            | '*'
            | '+'
            | '='
            | '<'
            | '>'
    ) {
        return true;
    }
    let ch = c as u32;
    (0x2000..=0x206F).contains(&ch)
        || (0x3000..=0x303F).contains(&ch)
        || (0xFE50..=0xFE6F).contains(&ch)
        || (0xFF00..=0xFF0F).contains(&ch)
        || (0xFF1A..=0xFF1F).contains(&ch)
        || (0xFF3B..=0xFF5E).contains(&ch)
}

/// Ligature glyph or its ASCII spelling (matches `string.h::is_ligature`).
fn is_ligature(s: &str) -> bool {
    matches!(s, "ff" | "fi" | "fl" | "ffi" | "ffl")
        || s.chars().any(|c| (0xFB00..=0xFB06).contains(&(c as u32)))
}

// ------------------------------------------------------------ word gaps (#921)

/// How a word boundary is decided where the PDF draws no space glyph — a
/// word set apart only by moving the text cursor (#921).
///
/// [`WordGap::Adaptive`], the default, and [`WordGap::Factor`] follow
/// docling-parse ≥ 7's contractor (`cells.h::contract`, 2026-09, PR #351 —
/// what docling ≥ 2.142 runs on, `docling-parse>=7.22`): glyphs are grouped
/// into line runs by geometry alone (no font boundary), every gap between
/// two visible glyphs is normalized by the mean advance of those two
/// glyphs, and a normalized gap above the threshold starts a new word; an
/// explicit space glyph is always a boundary. Line cells are the run's
/// words joined by single spaces. Letter spacing is detected first
/// ([`tracked_gaps`]) — our addition, where docling-parse cuts letter-spaced
/// words mid-word.
///
/// [`WordGap::Legacy`] is the contraction above: the docling-parse ≤ 6 rule
/// this port started from, where a gap splits two words when it exceeds
/// 0.33 × the running mean advance of the cell accumulated so far — a cell
/// that restarts at every font change, so the threshold drifts along a line
/// (`und dem` in a second font resource read `unddem`: 1.955 pt against
/// 0.33 × 5.925 pt, the mean of `und` alone).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum WordGap {
    /// docling-parse ≤ 6's running-mean contraction.
    Legacy,
    /// A fixed normalized threshold for every line.
    Factor(f64),
    /// docling-parse ≥ 7's line-local threshold: a two-cluster k-means over
    /// the line's normalized gaps (character spacing vs word spacing), at
    /// least 0.18, capped at 0.35, 0.25 when the gaps do not separate.
    #[default]
    Adaptive,
}

impl WordGap {
    /// The accepted spellings, for error messages.
    pub const ACCEPTED: &'static str = "legacy, adaptive, or a factor in (0, 4]";

    /// `legacy` / `adaptive` / a positive factor up to 4 (case-insensitive);
    /// `Err` with the reason for anything else.
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "legacy" => Ok(WordGap::Legacy),
            "adaptive" | "auto" | "default" => Ok(WordGap::Adaptive),
            _ => match t.parse::<f64>() {
                Ok(f) if f.is_finite() && f > 0.0 && f <= 4.0 => Ok(WordGap::Factor(f)),
                _ => Err(format!(
                    "pdf_word_gap must be {}, got {s:?}",
                    WordGap::ACCEPTED
                )),
            },
        }
    }

    /// `DOCLING_RS_PDF_WORD_GAP`, or the default ([`WordGap::Adaptive`])
    /// when it is unset (an unreadable value warns once and keeps the
    /// default).
    pub fn from_env() -> Self {
        let Some(v) = docling_core::env::nonempty("DOCLING_RS_PDF_WORD_GAP") else {
            return WordGap::default();
        };
        WordGap::parse(&v).unwrap_or_else(|e| {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!("docling-pdf: DOCLING_RS_PDF_WORD_GAP: {e}; using adaptive")
            });
            WordGap::default()
        })
    }
}

/// One glyph cell of a line run, with its writing axis precomputed.
struct RunCell<'a> {
    cell: &'a Cell,
    /// Unit vector along the bottom edge (the writing direction).
    ux: f64,
    uy: f64,
    /// Bottom-edge length: the advance the glyph's loose box spans.
    advance: f64,
    height: f64,
    space: bool,
}

impl<'a> RunCell<'a> {
    fn new(cell: &'a Cell) -> Self {
        let (dx, dy) = (cell.rx1 - cell.rx0, cell.ry1 - cell.ry0);
        let advance = dx.hypot(dy);
        let (ux, uy) = if advance > 1e-9 {
            (dx / advance, dy / advance)
        } else {
            (1.0, 0.0)
        };
        let h0 = (cell.rx3 - cell.rx0).hypot(cell.ry3 - cell.ry0);
        let h1 = (cell.rx2 - cell.rx1).hypot(cell.ry2 - cell.ry1);
        RunCell {
            cell,
            ux,
            uy,
            advance,
            height: 0.5 * (h0 + h1),
            space: cell.blank,
        }
    }

    /// The normalization length of a gap next to this glyph: its advance,
    /// else half its height (docling-parse's `cell_space_width` fallback).
    fn scale(&self) -> f64 {
        if self.advance > 1e-6 {
            self.advance
        } else {
            (0.5 * self.height).max(1e-6)
        }
    }
}

/// Signed distance along `lhs`'s writing direction from where its pen
/// advance ends to where `rhs`'s starts (`forward_gap`, the
/// `has_text_placement` form docling-parse takes for every rendered glyph).
/// Our quads run in pen order — `r0 → r1` is the advance — so this is the
/// trailing edge of `lhs` to the leading edge of `rhs` whatever the script:
/// right-to-left text drawn in visual order still advances along `+u`, and
/// flipping the axis for it (the geometric fallback) read every Arabic or
/// Hebrew glyph pair as a step back of two advances, a new line each.
fn forward_gap(lhs: &RunCell, rhs: &RunCell) -> f64 {
    let (l, r) = (lhs.cell, rhs.cell);
    let (tx, ty) = (0.5 * (l.rx1 + l.rx2), 0.5 * (l.ry1 + l.ry2));
    let (hx, hy) = (0.5 * (r.rx0 + r.rx3), 0.5 * (r.ry0 + r.ry3));
    (hx - tx) * lhs.ux + (hy - ty) * lhs.uy
}

/// Overlap, across the writing direction, of `lhs`'s trailing edge and
/// `rhs`'s leading edge — positive for glyphs on one text line.
fn facing_edge_overlap(lhs: &RunCell, rhs: &RunCell) -> f64 {
    let (l, r) = (lhs.cell, rhs.cell);
    let (ux, uy) = if l.ltr {
        (lhs.ux, lhs.uy)
    } else {
        (-lhs.ux, -lhs.uy)
    };
    let (nx, ny) = (-uy, ux);
    let proj = |x: f64, y: f64| x * nx + y * ny;
    let (a0, a1) = if l.ltr {
        (proj(l.rx1, l.ry1), proj(l.rx2, l.ry2))
    } else {
        (proj(l.rx0, l.ry0), proj(l.rx3, l.ry3))
    };
    let (b0, b1) = if l.ltr {
        (proj(r.rx0, r.ry0), proj(r.rx3, r.ry3))
    } else {
        (proj(r.rx1, r.ry1), proj(r.rx2, r.ry2))
    };
    a0.max(a1).min(b0.max(b1)) - a0.min(a1).max(b0.min(b1))
}

/// A glyph of a mathematical layout by its symbols — the symbol half of
/// `has_math_positioning_semantics`; the font-name half is
/// [`Glyph::math_font`].
fn is_math_glyph(text: &str) -> bool {
    const MATH: &str = "∑∏∫√∞∈∉≠≤≥⊂⊃⊆⊇⊥∪∩⎛⎜⎝⎞⎟⎠⎡⎢⎣⎤⎥⎦";
    text.chars().any(|c| MATH.contains(c))
}

/// A sub- or superscript placed by moving the cursor next to its base in a
/// mathematical context (`is_local_positioned_attachment`).
fn is_local_attachment(lhs: &RunCell, rhs: &RunCell) -> bool {
    let scale = lhs.scale().max(rhs.scale());
    if forward_gap(lhs, rhs) > 0.10 * scale {
        return false;
    }
    let (off, (lh, rh)) = baseline_offset(lhs, rhs);
    let math = |c: &RunCell| c.cell.math_font || is_math_glyph(&c.cell.text);
    off <= 2.0 * lh.max(rh).max(1e-6) && (math(lhs) || math(rhs))
}

/// How far `rhs` sits from `lhs`'s baseline, across `lhs`'s writing axis,
/// and the two glyphs' nominal heights: the pens' origins and the font sizes
/// when the parser knows them (docling-parse's `text_origin` /
/// `nominal_text_height`), else the loose boxes' bottom corners and heights.
/// The boxes overstate a size change — a superscript's box bottom sits only
/// its own, smaller descent below a raised baseline — so `Lysak` + its ORCID
/// read as two words where docling-parse 7 keeps one.
fn baseline_offset(lhs: &RunCell, rhs: &RunCell) -> (f64, (f64, f64)) {
    let (nx, ny) = (-lhs.uy, lhs.ux);
    match (lhs.cell.pen, rhs.cell.pen) {
        (Some(a), Some(b)) => (
            ((b[0] - a[0]) * nx + (b[1] - a[1]) * ny).abs(),
            (a[2], b[2]),
        ),
        _ => (
            ((rhs.cell.rx0 - lhs.cell.rx0) * nx + (rhs.cell.ry0 - lhs.cell.ry0) * ny).abs(),
            (lhs.height, rhs.height),
        ),
    }
}

/// Whether `rhs` continues `lhs`'s text line (`continues_line`): the same
/// writing direction, facing edges that overlap across it, and a cursor
/// move between a step back and four advances forward.
fn continues_line(lhs: &RunCell, rhs: &RunCell) -> bool {
    if lhs.ux * rhs.ux + lhs.uy * rhs.uy < 0.985 {
        return false;
    }
    let scale = lhs.scale().max(rhs.scale());
    let gap = forward_gap(lhs, rhs);
    if facing_edge_overlap(lhs, rhs) <= 1e-6 && !is_local_attachment(lhs, rhs) {
        return false;
    }
    -scale <= gap && gap <= 4.0 * scale
}

/// Whether two glyphs sit on one baseline (`word_baselines_compatible`): a
/// baseline shift beyond 0.35 × the smaller height starts a new word,
/// unless it is a mathematical attachment.
fn baselines_compatible(lhs: &RunCell, rhs: &RunCell) -> bool {
    let (off, (lh, rh)) = baseline_offset(lhs, rhs);
    off <= 0.35 * lh.min(rh).max(1e-6) || is_local_attachment(lhs, rhs)
}

/// The gap between two visible glyphs over their mean advance.
fn normalized_gap(lhs: &RunCell, rhs: &RunCell) -> f64 {
    forward_gap(lhs, rhs).max(0.0) / (0.5 * (lhs.scale() + rhs.scale())).max(1e-6)
}

/// What a glyph is to the letter-spacing detector.
#[derive(Clone, Copy, PartialEq)]
enum TrackClass {
    /// Letters and digits — what letter-spaced (Sperrsatz) words are set in.
    Letter,
    /// Leader dots.
    Dot,
    Other,
}

fn track_class(text: &str) -> TrackClass {
    if matches!(text, "." | "·" | "…") {
        TrackClass::Dot
    } else if !text.is_empty() && text.chars().all(char::is_alphanumeric) {
        TrackClass::Letter
    } else {
        TrackClass::Other
    }
}

/// Points two letter-spacing gaps may differ by and still be one tracking:
/// producers set a letter-spaced word with one constant cursor move (#921's
/// fixtures: within 0.17 pt), which rounding and kerning blur slightly.
fn tracking_tolerance(gap: f64) -> f64 {
    (0.08 * gap).max(0.25)
}

/// Letter spacing (Sperrsatz, tracked capitals, leader dots) in a line run
/// (#921): `true` at index `i` when the gap from the previous visible glyph
/// to `run[i]` is tracking inside one word rather than a word boundary.
///
/// docling-parse ≥ 7 judges each gap by its width over the flanking glyphs'
/// mean advance, and a letter-spaced word defeats that: its gaps are one
/// constant cursor move in points, but divided by the advances of `i`, `l`
/// or `t` beside `M` or `D` they spread over 0.25–0.75, straddling any
/// threshold, so the word is cut mid-word (`Di ens tk lasse`, `Heimarbei
/// tstari f`). Constancy in points is the signal instead: a chain of three
/// or more consecutive gaps between letters that are open (≥ 0.1 of an
/// advance), at most one advance wide (not table columns) and equal within
/// [`tracking_tolerance`] is tracking — when nothing on either side of it is
/// tighter. That last test is what tells it from justified prose, whose
/// word gaps are equal along a line too and chain up across one-letter
/// words (`Peter W J Staar`, `Abs 1 Z 3 UrlG`): there the chain is flanked
/// by abutting letters, while a letter-spaced word starts and ends at the
/// line's ends, a space glyph or a wider gap; and one character repeated
/// (`C C C C`, a figure's token row) is no word. Once a run has such a chain,
/// shorter stretches at the same tracking with the same open flanks join
/// it: the short words of the heading (`für`, `und`, `II`) and closing
/// punctuation (`:`). Four or more dots in a row, each within an advance of
/// the last, are one leader whatever their spacing.
fn tracked_gaps(run: &[RunCell]) -> Vec<bool> {
    struct Gap {
        left: usize,
        right: usize,
        pt: f64,
        n: f64,
        left_class: TrackClass,
        right_class: TrackClass,
    }
    let mut gaps: Vec<Gap> = Vec::new();
    let mut prev: Option<usize> = None;
    let mut crossed_space = false;
    for (i, c) in run.iter().enumerate() {
        if c.space {
            crossed_space = true;
            continue;
        }
        if let (Some(p), false) = (prev, crossed_space) {
            gaps.push(Gap {
                left: p,
                right: i,
                pt: forward_gap(&run[p], c),
                n: normalized_gap(&run[p], c),
                left_class: track_class(&run[p].cell.text),
                right_class: track_class(&c.cell.text),
            });
        }
        prev = Some(i);
        crossed_space = false;
    }
    // `gaps[k]` and `gaps[k + 1]` share a glyph — no space glyph between.
    let joined = |k: usize| gaps[k + 1].left == gaps[k].right;
    // The longest stretch from `k` of joined gaps that `fits`, as its end.
    let stretch = |k: usize, fits: &dyn Fn(&Gap) -> bool| {
        let mut end = k;
        while end < gaps.len() && fits(&gaps[end]) && (end == k || joined(end - 1)) {
            end += 1;
        }
        end
    };
    // No gap right before or after `k..end` tighter than `floor` points.
    let flanks_open = |k: usize, end: usize, floor: f64| {
        let before = k > 0 && joined(k - 1) && gaps[k - 1].pt < floor;
        let after = end < gaps.len() && joined(end - 1) && gaps[end].pt < floor;
        !before && !after
    };

    let mut tracked = vec![false; run.len()];
    let mut leader = vec![false; run.len()];
    let mut letter_tracking: Vec<f64> = Vec::new();
    let mut k = 0;
    while k < gaps.len() {
        let dots = |g: &Gap| {
            g.left_class == TrackClass::Dot && g.right_class == TrackClass::Dot && g.n <= 1.0
        };
        let end = stretch(k, &dots);
        if end - k >= 3 {
            for g in &gaps[k..end] {
                tracked[g.right] = true;
                leader[g.left] = true;
                leader[g.right] = true;
            }
            k = end;
            continue;
        }
        let first = &gaps[k];
        let letters = |g: &Gap| {
            g.left_class == TrackClass::Letter
                && g.right_class == TrackClass::Letter
                && (0.1..=1.0).contains(&g.n)
        };
        if !letters(first) {
            k += 1;
            continue;
        }
        let (mut lo, mut hi) = (first.pt, first.pt);
        let mut end = k + 1;
        while end < gaps.len()
            && joined(end - 1)
            && letters(&gaps[end])
            && hi.max(gaps[end].pt) - lo.min(gaps[end].pt) <= tracking_tolerance(lo)
        {
            lo = lo.min(gaps[end].pt);
            hi = hi.max(gaps[end].pt);
            end += 1;
        }
        // One character repeated (`C C C C` in a figure, `x x x x`) is a row
        // of tokens, not a word.
        let one_char = gaps[k..end]
            .iter()
            .all(|g| run[g.right].cell.text == run[first.left].cell.text);
        if end - k >= 3 && !one_char && flanks_open(k, end, lo - tracking_tolerance(lo)) {
            for g in &gaps[k..end] {
                tracked[g.right] = true;
            }
            letter_tracking.push(0.5 * (lo + hi));
            k = end;
        } else {
            k += 1;
        }
    }

    for &t in &letter_tracking {
        let tol = tracking_tolerance(t);
        let same = |g: &Gap| {
            (g.left_class == TrackClass::Letter || g.right_class == TrackClass::Letter)
                && g.left_class != TrackClass::Dot
                && g.right_class != TrackClass::Dot
                && g.n <= 1.0
                && (g.pt - t).abs() <= tol
        };
        let mut k = 0;
        while k < gaps.len() {
            let end = stretch(k, &same);
            if end == k {
                k += 1;
                continue;
            }
            if flanks_open(k, end, t - tol) && !gaps[k..end].iter().any(|g| leader[g.left]) {
                for g in &gaps[k..end] {
                    tracked[g.right] = true;
                }
            }
            k = end;
        }
    }
    tracked
}

/// Where a gap enters [`infer_word_gap`]'s k-means at most. A gap this wide
/// is a word boundary under any threshold the rule can pick (it caps at
/// 0.35), so its exact width carries no information — but one wide gap
/// after a comma or before a column (0.8–5 advances) dragged the high
/// cluster's mean up until the midpoint hit the 0.35 cap, and the line's
/// real word gaps (0.31–0.33 in tight justified text) fell under it:
/// `jedochwerdenweiterlaufendeQuartierkosten`, `KhurramAzeemHashmi`.
/// **Divergence** from docling-parse 7, which glues those (#921: 68 → 7
/// glues on the EVU agreement, measured against `pdftotext`'s words).
const GAP_CLUSTER_CLIP: f64 = 0.5;

/// docling-parse ≥ 7's `infer_word_gap`: the line's normalized gaps (not
/// across an explicit space) split by an eight-step two-cluster k-means;
/// 0.25 when there are fewer than two or they do not separate, else the
/// midpoint of the two cluster means, at least 0.18. Letter-spacing gaps
/// (`tracked`, #921) stay out of the sample: they are settled already, and
/// their spread would drag the low cluster up into the word gaps. Each gap
/// enters at most [`GAP_CLUSTER_CLIP`] wide.
fn infer_word_gap(run: &[RunCell], tracked: &[bool]) -> f64 {
    let mut gaps = Vec::new();
    let mut prev: Option<&RunCell> = None;
    let mut crossed_space = false;
    for (i, c) in run.iter().enumerate() {
        if c.space {
            crossed_space = true;
            continue;
        }
        if let (Some(p), false, false) = (prev, crossed_space, tracked[i]) {
            gaps.push(normalized_gap(p, c).min(GAP_CLUSTER_CLIP));
        }
        prev = Some(c);
        crossed_space = false;
    }
    if gaps.len() < 2 {
        return 0.25;
    }
    let mut low = gaps.iter().copied().fold(f64::INFINITY, f64::min);
    let mut high = gaps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if high < 0.18 || high - low < 0.08 {
        return 0.25;
    }
    for _ in 0..8 {
        let mid = 0.5 * (low + high);
        let (mut ls, mut ln, mut hs, mut hn) = (0.0, 0usize, 0.0, 0usize);
        for &g in &gaps {
            if g <= mid {
                ls += g;
                ln += 1;
            } else {
                hs += g;
                hn += 1;
            }
        }
        if ln == 0 || hn == 0 {
            return 0.25;
        }
        low = ls / ln as f64;
        high = hs / hn as f64;
    }
    if high < 0.18 || high - low < 0.08 {
        return 0.25;
    }
    (0.5 * (low + high)).max(0.18)
}

/// A word or line being assembled: its text and its rotation-aligned box.
struct Agg {
    text: String,
    ltr: bool,
    q: [f64; 8],
    /// A word that starts only because its first glyph sits on another
    /// baseline than the glyph before it, with no horizontal gap between
    /// them, *inside a bracket* — a raised footnote marker after `(`, the
    /// `)` after it, a citation's `]` — joins the line text without a space
    /// (#992, a deliberate divergence from docling-parse's `( 1 )`; the
    /// word cell is still its own).
    glued: bool,
}

impl Agg {
    fn of(c: &Cell, glued: bool) -> Self {
        Agg {
            text: c.text.clone(),
            ltr: c.ltr,
            q: [c.rx0, c.ry0, c.rx1, c.ry1, c.rx2, c.ry2, c.rx3, c.ry3],
            glued,
        }
    }

    /// `append_cell`: the text in reading order (an RTL piece prepends), and
    /// the smallest rectangle aligned with the writing direction holding
    /// both boxes.
    fn append(&mut self, text: &str, ltr: bool, q: &[f64; 8], space: bool) {
        if self.ltr && ltr {
            if space {
                self.text.push(' ');
            }
            self.text.push_str(text);
        } else {
            let sep = if space { " " } else { "" };
            self.text = format!("{text}{sep}{}", self.text);
            self.ltr = false;
        }
        let (mut ux, mut uy) = (self.q[2] - self.q[0], self.q[3] - self.q[1]);
        if ux.hypot(uy) <= 1e-9 {
            (ux, uy) = (q[2] - q[0], q[3] - q[1]);
        }
        let n = ux.hypot(uy);
        if n <= 1e-9 {
            return;
        }
        let (ux, uy) = (ux / n, uy / n);
        let (nx, ny) = (-uy, ux);
        let (mut u0, mut u1, mut n0, mut n1) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for k in 0..4 {
            for (x, y) in [(self.q[2 * k], self.q[2 * k + 1]), (q[2 * k], q[2 * k + 1])] {
                let (u, v) = (x * ux + y * uy, x * nx + y * ny);
                u0 = u0.min(u);
                u1 = u1.max(u);
                n0 = n0.min(v);
                n1 = n1.max(v);
            }
        }
        let pt = |u: f64, v: f64| (u * ux + v * nx, u * uy + v * ny);
        let (a, b, c, d) = (pt(u0, n0), pt(u1, n0), pt(u1, n1), pt(u0, n1));
        self.q = [a.0, a.1, b.0, b.1, c.0, c.1, d.0, d.1];
    }

    fn into_cell(self, page_h: f32) -> TextCell {
        let xs = [self.q[0], self.q[2], self.q[4], self.q[6]];
        let ys = [self.q[1], self.q[3], self.q[5], self.q[7]];
        let (l, r) = (
            xs.iter().copied().fold(f64::INFINITY, f64::min),
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        );
        let (b, t) = (
            ys.iter().copied().fold(f64::INFINITY, f64::min),
            ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        );
        TextCell {
            text: self.text,
            l: l as f32,
            t: page_h - t as f32,
            r: r as f32,
            b: page_h - b as f32,
        }
    }
}

/// Whether the page's producer draws its word spaces as glyphs: at least
/// half of the lines long enough to hold several words (eight glyphs or
/// more) carry a space glyph. Producers are consistent about it — a TeX or
/// Arbortext page has none, a Word or EUR-Lex page has them on every line —
/// so this tells a space-less line on a spaced page (a lone word) from the
/// cursor-move pages #921 is about.
fn page_draws_spaces(runs: &[Vec<RunCell>]) -> bool {
    let long = |r: &&Vec<RunCell>| r.iter().filter(|c| !c.space).count() >= 8;
    let lines = runs.iter().filter(long).count();
    let spaced = runs
        .iter()
        .filter(long)
        .filter(|r| r.iter().any(|c| c.space))
        .count();
    lines > 0 && 2 * spaced >= lines
}

/// docling-parse ≥ 7's contraction (`cells.h::contract`) over our char
/// cells, with the word threshold `gap` picks: the line runs, their words,
/// and the lines as the words joined by single spaces.
fn contract_runs(cells: &[Cell], gap: WordGap, page_h: f32) -> (Vec<TextCell>, Vec<TextCell>) {
    // Line runs, in source order. Explicit spaces belong to the open run as
    // boundary markers and take no part in the geometry test.
    let mut runs: Vec<Vec<RunCell>> = Vec::new();
    for c in cells.iter().filter(|c| !c.text.is_empty()) {
        let rc = RunCell::new(c);
        if rc.space {
            match runs.last_mut() {
                Some(run) => run.push(rc),
                None => runs.push(vec![rc]),
            }
            continue;
        }
        let starts = match runs.last().and_then(|r| r.iter().rev().find(|c| !c.space)) {
            Some(prev) => !continues_line(prev, &rc),
            None => runs.is_empty(),
        };
        if starts {
            runs.push(Vec::new());
        }
        runs.last_mut().expect("a run is open").push(rc);
    }

    let (mut words_out, mut lines_out) = (Vec::new(), Vec::new());
    let page_spaced = page_draws_spaces(&runs);
    for run in &runs {
        // A line on a page whose producer draws its word spaces as glyphs is
        // judged like a line that draws them (below), even when it has none
        // of its own: a lone word in a table cell or a heading carries no
        // space glyph, and its only open gap is kerning — EUR-Lex's
        // `Offenlegungspf|lichten` at 0.22–0.25, cut by the line-local
        // threshold of 0.18 (docling-parse 7 cuts it too; #921).
        let has_spaces = page_spaced || run.iter().any(|c| c.space);
        let tracked = tracked_gaps(run);
        // A line that draws its word spaces as glyphs marks its boundaries
        // itself; docling-parse 7 splits an unmarked gap there only above
        // 0.35 (`min(0.35, 1.0)`). A fixed factor keeps that floor: its job
        // is the space-less lines (#921), and below it a spaced line's
        // loosely kerned pairs fall apart — EUR-Lex's `f|l` at 0.24 read
        // `Verpf lichtungen` under 0.2.
        let threshold = match gap {
            WordGap::Factor(f) if has_spaces => f.max(0.35),
            WordGap::Factor(f) => f,
            _ => (if has_spaces {
                1.0
            } else {
                infer_word_gap(run, &tracked)
            })
            .min(0.35),
        };
        let mut words: Vec<Agg> = Vec::new();
        let mut prev: Option<&RunCell> = None;
        let mut boundary = false;
        for (i, c) in run.iter().enumerate() {
            if c.space {
                boundary = true;
                continue;
            }
            let mut starts = words.is_empty() || boundary;
            let mut glued = false;
            // A TOC leader may omit the boundary before its first dot while
            // spacing every later one: keep that dot with the leader.
            if !starts
                && c.cell.text == "."
                && i + 4 < run.len()
                && run[i + 1].space
                && run[i + 2].cell.text == "."
                && run[i + 3].space
                && run[i + 4].cell.text == "."
                && prev.is_some_and(|p| p.cell.text != ".")
            {
                starts = true;
            }
            if let (false, Some(p)) = (starts, prev) {
                let shifted = !baselines_compatible(p, c);
                let gapped = !tracked[i] && normalized_gap(p, c) > threshold;
                starts = shifted || gapped;
                // A baseline shift with the glyphs touching, inside a
                // bracket: docling-parse starts a word
                // (`word_baselines_compatible`) and its line text then reads
                // `Richtlinie 2003/71/EG ( 1 )` for a raised footnote marker
                // where the page shows `(1)` and pdftotext / pdfium read it
                // so (#992). The word stays its own cell; the line text
                // joins it as the page does. Only after an opening bracket
                // or before a closing one: elsewhere docling's space is the
                // more useful reading — `SynthTabNet 1 .`, `1 https://…`,
                // `상해 1) 나`, the `1 /4` fraction — and joining them cost
                // five groundtruth files when this rule had no bracket test.
                let opens = |s: &str| matches!(s.chars().last(), Some('(' | '[' | '{'));
                let closes = |s: &str| matches!(s.chars().next(), Some(')' | ']' | '}'));
                glued = shifted
                    && !gapped
                    && (words.last().is_some_and(|w| opens(&w.text)) || closes(&c.cell.text));
            }
            if starts {
                words.push(Agg::of(c.cell, glued));
            } else {
                let q = [
                    c.cell.rx0, c.cell.ry0, c.cell.rx1, c.cell.ry1, c.cell.rx2, c.cell.ry2,
                    c.cell.rx3, c.cell.ry3,
                ];
                words.last_mut().expect("a word is open").append(
                    &c.cell.text,
                    c.cell.ltr,
                    &q,
                    false,
                );
            }
            prev = Some(c);
            boundary = false;
        }
        let mut it = words.into_iter();
        let Some(first) = it.next() else {
            continue;
        };
        let mut line = Agg {
            text: first.text.clone(),
            ltr: first.ltr,
            q: first.q,
            glued: false,
        };
        words_out.push(first.into_cell(page_h));
        for w in it {
            line.append(&w.text, w.ltr, &w.q, !w.glued);
            words_out.push(w.into_cell(page_h));
        }
        lines_out.push(line.into_cell(page_h));
    }
    (lines_out, words_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(ch: char, l: f32, r: f32) -> Glyph {
        Glyph {
            ch,
            l,
            b: 100.0,
            r,
            t: 110.0,
            ll: l,
            lb: 98.0,
            lr: r,
            lt: 110.0,
            font: 0,
            quad: None,
            pen: None,
            math_font: false,
        }
    }

    /// Tight-set Latin (a table cell in a born-digital manual): the gap between
    /// `MODE` and `to` (1.84 pt) is under the 0.33 × average-width word gate (2.0 pt), so
    /// only the space glyph between them keeps the words apart. docling-parse
    /// erases spaces after the word contraction, not before.
    #[test]
    fn space_glyph_separates_tight_words() {
        let glyphs = [
            glyph('M', 137.66, 143.74),
            glyph('O', 143.74, 149.82),
            glyph('D', 149.82, 155.90),
            glyph('E', 155.90, 161.98),
            glyph(' ', 161.84, 164.09),
            glyph('t', 163.82, 166.60),
            glyph('o', 166.60, 171.60),
        ];
        let (lines, words) = line_and_word_cells(&glyphs, 792.0, true);
        let words: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(words, ["MODE", "to"]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "MODE to");
    }

    /// #609: a chart's y-axis ticks set one under another at the same x (each
    /// tick placed by its own `cm`, same `Tm`) are six line cells with their
    /// own boxes, as in docling-parse — not one `345678` cell with the first
    /// tick's box. A ligature decomposed at one box still recomposes.
    #[test]
    fn stacked_glyphs_at_one_x_stay_separate_cells() {
        let at = |ch: char, b: f32| Glyph {
            lb: b,
            lt: b + 10.0,
            b,
            t: b + 10.0,
            ..glyph(ch, 98.0, 103.0)
        };
        let ticks: Vec<Glyph> = "345678"
            .chars()
            .enumerate()
            .map(|(k, ch)| at(ch, 316.0 + 25.92 * k as f32))
            .collect();
        let lines = line_cells(&ticks, 792.0, true);
        let texts: Vec<&str> = lines.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["3", "4", "5", "6", "7", "8"]);
        assert!(lines.windows(2).all(|w| w[0].t > w[1].t));

        let lig = [glyph('f', 10.0, 16.0), glyph('i', 10.0, 16.0)];
        let lines = line_cells(&lig, 792.0, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "fi");
    }

    /// The returned vectors own no more than their cells. Collecting straight
    /// from the per-glyph `Cell` buffer reused its allocation (std's in-place
    /// collect), so a page kept ~4 slots per *glyph* — on a dense table page,
    /// 30× the cells' size, for every page the text-layer path held.
    #[test]
    fn cells_do_not_keep_the_glyph_buffer() {
        // Ten rows of `777 777 …`: abutting glyphs, words split by space glyphs.
        let glyphs: Vec<Glyph> = (0..400)
            .map(|i| {
                let x = (i % 40) as f32 * 6.0;
                let ch = if i % 4 == 3 { ' ' } else { '7' };
                let mut g = glyph(ch, x, x + 6.0);
                let row = (i / 40) as f32 * 14.0;
                (g.b, g.t, g.lb, g.lt) = (100.0 + row, 110.0 + row, 98.0 + row, 110.0 + row);
                g
            })
            .collect();
        let (lines, words) = line_and_word_cells(&glyphs, 792.0, true);
        assert_eq!((lines.len(), words.len()), (10, 100));
        assert_eq!(lines.capacity(), lines.len());
        assert_eq!(words.capacity(), words.len());
    }

    /// #921, page 8 of the reporter's collective agreement: `und dem` sits in
    /// a second font resource between runs of the body font, word gaps are
    /// cursor moves (no space glyph). Glyph boxes as the parser reads them.
    fn und_dem_line() -> Vec<Glyph> {
        let at = |ch: char, l: f32, r: f32, font: u64| Glyph {
            ch,
            l,
            b: 682.056,
            r,
            t: 691.05,
            ll: l,
            lb: 682.056,
            lr: r,
            lt: 691.05,
            font,
            quad: None,
            pen: None,
            math_font: false,
        };
        let mut g = Vec::new();
        let mut run = |text: &str, edges: &[f32], font: u64| {
            for (i, ch) in text.chars().enumerate() {
                g.push(at(ch, edges[i], edges[i + 1], font));
            }
        };
        run(
            "schaft",
            &[
                328.976, 333.216, 338.572, 344.325, 350.486, 353.920, 357.248,
            ],
            1,
        );
        run("und", &[359.261, 365.121, 370.875, 377.035], 2);
        run("dem", &[378.990, 385.151, 390.701, 399.287], 2);
        run("Öst", &[401.214, 409.092, 413.332, 416.660], 1);
        g
    }

    #[test]
    fn legacy_rule_keeps_the_borderline_und_dem_glued() {
        // The baselines' rule: the cell restarts at the font change, so the
        // threshold is 0.33 × the mean advance of `und` alone (1.9553 pt)
        // and the 1.9550 pt gap stays inside the word.
        let (lines, words) = line_and_word_cells(&und_dem_line(), 792.0, true);
        assert!(lines.iter().any(|c| c.text == "unddem"), "{lines:?}");
        assert!(words.iter().any(|c| c.text == "unddem"), "{words:?}");
    }

    #[test]
    fn line_local_word_gaps_split_und_dem() {
        for gap in [WordGap::Adaptive, WordGap::Factor(0.2)] {
            let (lines, words) = line_and_word_cells_with(&und_dem_line(), 792.0, true, gap);
            let texts: Vec<&str> = lines.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(texts, ["schaft und dem Öst"], "{gap:?}");
            let texts: Vec<&str> = words.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(texts, ["schaft", "und", "dem", "Öst"], "{gap:?}");
            // The word boxes are the glyphs' own extent.
            assert!((words[1].l - 359.261).abs() < 1e-3 && (words[1].r - 377.035).abs() < 1e-3);
        }
        // A factor above the normalized gaps (0.234 … 0.438) keeps them all.
        let (lines, _) =
            line_and_word_cells_with(&und_dem_line(), 792.0, true, WordGap::Factor(0.5));
        assert_eq!(lines[0].text, "schaftunddemÖst");
    }

    #[test]
    fn a_superscript_attaches_by_pen_origin_and_math_face() {
        // `Lysak` at 10 pt, then `[0000` at 7 pt raised `rise` pt with no gap.
        // The baseline test reads the pens' origins and font sizes, as
        // docling-parse 7 does (`pen: None` falls back to the loose boxes).
        let at = |ch: char, l: f32, y: f32, size: f32, math: bool, pen: bool| Glyph {
            lb: y - 0.2 * size,
            lt: y + 0.8 * size,
            b: y - 0.2 * size,
            t: y + 0.8 * size,
            pen: pen.then_some([l, y, size]),
            math_font: math,
            ..glyph(ch, l, l + 0.5 * size)
        };
        let line = |rise: f32, math: bool, pen: bool| {
            let mut g: Vec<Glyph> = "Lysak"
                .chars()
                .enumerate()
                .map(|(k, ch)| at(ch, 100.0 + 5.0 * k as f32, 600.0, 10.0, math, pen))
                .collect();
            g.extend(
                "[0000"
                    .chars()
                    .enumerate()
                    .map(|(k, ch)| at(ch, 125.0 + 3.5 * k as f32, 600.0 + rise, 7.0, math, pen)),
            );
            let (lines, words) = line_and_word_cells_with(&g, 792.0, true, WordGap::Adaptive);
            (
                lines[0].text.clone(),
                words.iter().map(|w| w.text.clone()).collect::<Vec<_>>(),
            )
        };
        let one = |s: &str| (s.to_string(), vec![s.to_string()]);
        let two = |s: &str| {
            (
                s.to_string(),
                vec!["Lysak".to_string(), "[0000".to_string()],
            )
        };
        // 2305.03393v1's ORCID: 3.6 pt up is more than 0.35 × 7 pt, a new word
        // unless it is a local attachment, which a TeX face (`cmr`) makes it.
        // Not a math face: two words, spaced — the raised run *opens* a
        // bracket, which is not the #992 join (that is for a run after an
        // opening bracket or before a closing one).
        assert_eq!(line(3.6, true, true), one("Lysak[0000"));
        assert_eq!(line(3.6, false, true), two("Lysak [0000"));
        // 2 pt up: on the pens 2.0 ≤ 0.35 × 7, one word; the boxes' bottoms
        // differ by the descents too (2.6 pt), which read it as two.
        assert_eq!(line(2.0, false, true), one("Lysak[0000"));
        assert_eq!(line(2.0, false, false), two("Lysak [0000"));
    }

    /// #992: a raised footnote marker between parentheses — `(`, a 6.8 pt
    /// `1` 3.5 pt up, `)` — is three word cells (the baseline rule) but the
    /// line reads `(1)` as the page does; with a real gap before the marker
    /// (`EG (1)`) the space is the gap's, as before. The same raised `1`
    /// right after a letter keeps docling's space (`EG 1`): only a bracket
    /// joins.
    #[test]
    fn a_touching_superscript_joins_the_line_text_inside_brackets_only() {
        let at = |ch: char, l: f32, w: f32, y: f32, size: f32| Glyph {
            lb: y - 0.2 * size,
            lt: y + 0.8 * size,
            b: y - 0.2 * size,
            t: y + 0.8 * size,
            pen: Some([l, y, size]),
            ..glyph(ch, l, l + w)
        };
        let g = [
            at('E', 100.0, 6.0, 600.0, 10.5),
            at('G', 106.0, 6.9, 600.0, 10.5),
            at('(', 115.3, 3.2, 600.0, 10.5),
            at('1', 118.5, 3.1, 603.5, 6.8),
            at(')', 121.6, 3.2, 600.0, 10.5),
            at('e', 127.2, 5.0, 600.0, 10.5),
        ];
        let (lines, words) = line_and_word_cells_with(&g, 792.0, true, WordGap::Adaptive);
        assert_eq!(lines[0].text, "EG (1) e");
        let words: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(words, ["EG", "(", "1", ")", "e"]);
        // `EG` then the raised `1` touching the `G`: docling's `EG 1`.
        let g = [
            at('E', 100.0, 6.0, 600.0, 10.5),
            at('G', 106.0, 6.9, 600.0, 10.5),
            at('1', 112.9, 3.1, 603.5, 6.8),
        ];
        let (lines, _) = line_and_word_cells_with(&g, 792.0, true, WordGap::Adaptive);
        assert_eq!(lines[0].text, "EG 1");
    }

    #[test]
    fn a_factor_keeps_docling_parse_s_floor_on_lines_with_space_glyphs() {
        // `Verpf` + `lichtungen` 0.6 pt apart (0.24 of the flanking advances)
        // on a line whose word spaces are glyphs: below the 0.35 floor
        // docling-parse 7 keeps for such lines, so it is not a boundary even
        // under a factor of 0.2. The same gap on a space-less line splits.
        let mut g = Vec::new();
        let mut x = 100.0f32;
        let put = |g: &mut Vec<Glyph>, s: &str, x: &mut f32| {
            for ch in s.chars() {
                g.push(glyph(ch, *x, *x + 2.5));
                *x += 2.5;
            }
        };
        put(&mut g, "Die", &mut x);
        g.push(glyph(' ', x, x + 2.5));
        x += 2.5;
        put(&mut g, "Verpf", &mut x);
        x += 0.6;
        put(&mut g, "lichtungen", &mut x);
        let (lines, _) = line_and_word_cells_with(&g, 792.0, true, WordGap::Factor(0.2));
        assert_eq!(lines[0].text, "Die Verpflichtungen");
        let spaceless: Vec<Glyph> = g.into_iter().filter(|c| c.ch != ' ').collect();
        let (lines, _) = line_and_word_cells_with(&spaceless, 792.0, true, WordGap::Factor(0.2));
        assert_eq!(lines[0].text, "Die Verpf lichtungen");
    }

    #[test]
    fn right_to_left_glyphs_in_visual_order_stay_one_line() {
        // Arabic drawn in visual order (the pen moves left to right, as most
        // producers store it): the gap runs along the pen, not against the
        // script — reversed, every glyph pair read as a step back of two
        // advances and each glyph became a line of its own.
        let mut g = Vec::new();
        let mut x = 100.0f32;
        for (k, word) in [
            "\u{0631}\u{0627}\u{0632}\u{0627}\u{0628}",
            "\u{0631}\u{062f}",
        ]
        .iter()
        .enumerate()
        {
            if k > 0 {
                x += 3.0;
            }
            for ch in word.chars() {
                g.push(glyph(ch, x, x + 5.0));
                x += 5.0;
            }
        }
        for gap in [WordGap::Adaptive, WordGap::Factor(0.2)] {
            let (lines, words) = line_and_word_cells_with(&g, 792.0, true, gap);
            assert_eq!(
                lines.len(),
                1,
                "{gap:?}: {:?}",
                lines.iter().map(|c| &c.text).collect::<Vec<_>>()
            );
            assert_eq!(words.len(), 2, "{gap:?}");
        }
    }

    #[test]
    fn letter_spacing_stays_one_word_under_the_adaptive_rule() {
        // Sperrsatz: every letter 1.5 pt apart, the words 6 pt apart. The
        // line's gaps form two clusters, so the letters stay together.
        let mut g = Vec::new();
        let mut x = 100.0f32;
        for (k, word) in ["Allgemeine", "Bestimmungen"].iter().enumerate() {
            if k > 0 {
                x += 6.0;
            }
            for ch in word.chars() {
                g.push(glyph(ch, x, x + 5.0));
                x += 5.0 + 1.5;
            }
            x -= 1.5;
        }
        let (lines, _) = line_and_word_cells_with(&g, 792.0, true, WordGap::Adaptive);
        assert_eq!(lines[0].text, "Allgemeine Bestimmungen");
    }

    /// Glyphs 2.5 pt wide on a line at `y`, each `(text, gap before)` piece
    /// set after the previous one; `' '` pieces are space glyphs.
    fn set_line(pieces: &[(&str, f32)], y: f32) -> Vec<Glyph> {
        let mut g = Vec::new();
        let mut x = 100.0f32;
        for (text, gap) in pieces {
            x += gap;
            for ch in text.chars() {
                g.push(Glyph {
                    b: y,
                    t: y + 10.0,
                    lb: y - 2.0,
                    lt: y + 10.0,
                    ..glyph(ch, x, x + 2.5)
                });
                x += 2.5;
            }
        }
        g
    }

    #[test]
    fn one_wide_gap_does_not_raise_the_line_s_word_gap() {
        // #921, EVU p. 3: word gaps of 0.32 advances in tight justified text
        // and one gap of 3.2 before a tab stop. Unclipped, the wide gap is
        // the high cluster alone, the midpoint caps at 0.35 and the line
        // reads `jedochwerdenweiterlaufende`.
        let g = set_line(
            &[
                ("jedoch", 0.0),
                ("werden", 0.8),
                ("weiterlaufende", 0.8),
                ("Quartierkosten", 8.0),
            ],
            600.0,
        );
        let (_, words) = line_and_word_cells_with(&g, 792.0, true, WordGap::Adaptive);
        let texts: Vec<&str> = words.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            ["jedoch", "werden", "weiterlaufende", "Quartierkosten"]
        );
    }

    #[test]
    fn a_lone_word_on_a_page_that_draws_spaces_keeps_its_kerning() {
        // #921, EUR-Lex: `Offenlegungspf` + `lichten` 0.8 pt apart (0.32
        // advances) in a one-word table cell. On a page whose lines carry
        // space glyphs the gap is kerning, not a word boundary; on a
        // space-less page (TeX, Arbortext) the same line splits.
        let lone = || set_line(&[("Offenlegungspf", 0.0), ("lichten", 0.8)], 500.0);
        let spaced = |y| {
            set_line(
                &[
                    ("Die", 0.0),
                    (" ", 0.0),
                    ("Taxonomie", 0.0),
                    (" ", 0.0),
                    ("gilt", 0.0),
                ],
                y,
            )
        };
        let mut page = lone();
        page.extend(spaced(600.0));
        page.extend(spaced(620.0));
        let texts = |g: &[Glyph]| {
            let (lines, _) = line_and_word_cells_with(g, 792.0, true, WordGap::Adaptive);
            lines.into_iter().map(|c| c.text).collect::<Vec<_>>()
        };
        assert!(texts(&page).contains(&"Offenlegungspflichten".to_string()));
        assert_eq!(texts(&lone()), ["Offenlegungspf lichten"]);
    }

    #[test]
    fn word_gap_spellings() {
        assert_eq!(WordGap::parse("adaptive"), Ok(WordGap::Adaptive));
        assert_eq!(WordGap::parse(" Legacy "), Ok(WordGap::Legacy));
        assert_eq!(WordGap::parse("0.2"), Ok(WordGap::Factor(0.2)));
        for bad in ["0", "-1", "5", "nan", "wide"] {
            assert!(WordGap::parse(bad).is_err(), "{bad}");
        }
    }
}
