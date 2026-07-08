//! Port of docling-parse's line-cell sanitizer
//! (`src/parse/page_item_sanitators/cells.h` → `create_line_cells` /
//! `contract_cells_into_lines_v1`). It merges per-glyph char cells into line
//! cells via a 3-pass contraction — left-to-right, right-to-left, then
//! left-to-right with reverse — using corner-distance adjacency and inserting at
//! most one space per merge. This reproduces docling-parse's inter-word spacing
//! (justified double spaces, the space before a `:`, and RTL ordering) that the
//! ad-hoc `lines_from_glyphs` reconstruction can't.
//!
//! Geometry uses native PDF coordinates (y increases upward); each cell carries
//! its four transformed corners r0=bottom-left, r1=bottom-right, r2=top-right,
//! r3=top-left, exactly like `page_cell.h`.

use crate::pdfium_backend::{Glyph, TextCell};

// config.h: the factors that actually bind for line cells.
const MERGE: f64 = 1.0; // line_space_width_factor_for_merge (adjacency gate)
const MERGE_WITH_SPACE: f64 = 0.33; // line_space_width_factor_for_merge_with_space
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
    font: u64,       // hash of the PDF font name+flags (for enforce_same_font)
    /// Sub-word segments accumulated during contraction, in final logical order.
    /// A word boundary is recorded wherever `merge_with` inserts a separator space
    /// (`delta < gap`); within a boundary the glyphs share one segment. Flattening
    /// these across all cells yields docling-parse's `word_cells` (item 6). The
    /// line path ignores this; only [`word_cells`] reads it.
    words: Vec<WordSeg>,
}

/// One word's accumulated text and native-coordinate bounding box (y up).
#[derive(Clone)]
struct WordSeg {
    text: String,
    l: f64,
    b: f64,
    r: f64,
    t: f64,
}

impl WordSeg {
    fn from_glyph(text: String, l: f64, b: f64, r: f64, t: f64) -> Self {
        WordSeg { text, l, b, r, t }
    }
    /// Absorb `o` into this segment (same word): union the box, append text.
    fn absorb(&mut self, o: &WordSeg) {
        self.text.push_str(&o.text);
        self.l = self.l.min(o.l);
        self.b = self.b.min(o.b);
        self.r = self.r.max(o.r);
        self.t = self.t.max(o.t);
    }
    /// Extend the box to cover a single glyph (ligature recompose into one word).
    fn extend(&mut self, l: f64, b: f64, r: f64, t: f64) {
        self.l = self.l.min(l);
        self.b = self.b.min(b);
        self.r = self.r.max(r);
        self.t = self.t.max(t);
    }
}

/// Concatenate two word runs (in final logical order). With a separator space
/// the runs stay distinct (a word boundary); without one, `left`'s last word and
/// `right`'s first word are the same word and merge. Mirrors `merge_with`'s
/// space decision so word grouping tracks the line contraction exactly.
fn merge_word_runs(mut left: Vec<WordSeg>, mut right: Vec<WordSeg>, space: bool) -> Vec<WordSeg> {
    if left.is_empty() {
        return right;
    }
    if right.is_empty() {
        return left;
    }
    if !space {
        let first = right.remove(0);
        left.last_mut().unwrap().absorb(&first);
    }
    left.extend(right);
    left
}

impl Cell {
    /// Length of the bottom edge (baseline advance) — `page_cell.h::length`.
    fn length(&self) -> f64 {
        ((self.rx1 - self.rx0).powi(2) + (self.ry1 - self.ry0).powi(2)).sqrt()
    }

    /// Running mean glyph advance over the whole accumulated cell.
    fn avg_char_width(&self) -> f64 {
        let n = self.text.chars().count();
        if n > 0 {
            self.length() / n as f64
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
            // RTL: `other` is logically first, so its words precede self's. The
            // junction is between other's last word and self's first.
            self.words =
                merge_word_runs(other.words.clone(), std::mem::take(&mut self.words), space);
        } else {
            if space {
                self.text.push(' ');
            }
            self.text.push_str(&other.text);
            self.ltr = true;
            self.words =
                merge_word_runs(std::mem::take(&mut self.words), other.words.clone(), space);
        }
        // Extend the right edge to `other`.
        self.rx1 = other.rx1;
        self.ry1 = other.ry1;
        self.rx2 = other.rx2;
        self.ry2 = other.ry2;
    }
}

/// `applicable_for_merge`: both active and same reading orientation. A different
/// font normally blocks the merge (keeps a bold label and its value as separate
/// line cells). On the clean-box parser path, **punctuation/space cells bridge
/// fonts** so a sentence period set in a separate punctuation font joins its word
/// instead of fragmenting (`العمل .` → `العمل.`); letters still enforce the font.
fn applicable(a: &Cell, b: &Cell, parser: bool) -> bool {
    if !a.active || !b.active {
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
    let font_neutral = is_ligature(&a.text) || is_ligature(&b.text) || punct_bridge;
    if a.font != 0 && b.font != 0 && a.font != b.font && !font_neutral {
        return false;
    }
    a.same_orientation(b)
}

/// Left-to-right pass: `i` ascending accumulates cells to its right.
fn pass_ltr(cells: &mut [Cell], allow_reverse: bool, euclidean: bool) {
    for i in 0..cells.len() {
        if !cells[i].active {
            continue;
        }
        let mut j = i + 1;
        while j < cells.len() {
            if !applicable(&cells[i], &cells[j], euclidean) {
                break;
            }
            let i_lig = is_ligature(&cells[i].text) || cells[i].lig_carry;
            let j_lig = is_ligature(&cells[j].text) || cells[j].lig_carry;
            let d0 = cells[i].avg_char_width() * MERGE;
            let d1 = cells[i].avg_char_width() * MERGE_WITH_SPACE;
            let adj_d1 = d0 + if i_lig || j_lig { H_TOL } else { 0.0 };
            if cells[i].adjacent(&cells[j], d0, adj_d1) {
                let other = cells[j].clone();
                cells[i].merge_with(&other, d1, euclidean);
                cells[i].lig_carry = is_ligature(&other.text);
                cells[j].active = false;
                j += 1; // i keeps absorbing the next cell to its right
            } else if allow_reverse && cells[j].adjacent(&cells[i], d0, adj_d1) {
                let other = cells[i].clone();
                cells[j].merge_with(&other, d1, euclidean);
                cells[j].lig_carry = is_ligature(&other.text);
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
fn pass_rtl(cells: &mut [Cell], euclidean: bool) {
    let n = cells.len();
    for k in 0..n {
        let i = n - 1 - k;
        if !cells[i].active || i == 0 {
            continue;
        }
        let j = i - 1;
        if !applicable(&cells[i], &cells[j], euclidean) {
            continue;
        }
        let i_lig = is_ligature(&cells[i].text) || cells[i].lig_carry;
        let j_lig = is_ligature(&cells[j].text) || cells[j].lig_carry;
        let d0 = cells[i].avg_char_width() * MERGE;
        let d1 = cells[i].avg_char_width() * MERGE_WITH_SPACE;
        let adj_d1 = d0 + if i_lig || j_lig { H_TOL } else { 0.0 };
        if cells[j].adjacent(&cells[i], d0, adj_d1) {
            let other = cells[i].clone();
            cells[j].merge_with(&other, d1, euclidean);
            cells[j].lig_carry = is_ligature(&other.text);
            cells[i].active = false;
        }
    }
}

fn contract(cells: &mut Vec<Cell>, euclidean: bool) {
    pass_ltr(cells, false, euclidean);
    cells.retain(|c| c.active);
    pass_rtl(cells, euclidean);
    cells.retain(|c| c.active);
    pass_ltr(cells, true, euclidean);
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
        // Drop *degenerate* space glyphs (zero-width loose box): pdfium's generated
        // spaces get a zero-width box at the wrong baseline that breaks the
        // corner-distance adjacency. Without them the inter-word gap drives
        // `merge_with`'s space insertion. Spaces with a real width are kept (they
        // carry justified double-space information).
        if g.ch == ' ' && (g.lr - g.ll).abs() < 0.5 {
            continue;
        }
        // Recompose a ligature: pdfium decomposes one font glyph (Latin fi/ffi,
        // Arabic lam-alef) into several chars at the *same* loose box. Append them
        // into one cell so the contraction never inserts a space inside it.
        if let Some(last) = cells.last_mut() {
            if (last.rx0 - g.ll as f64).abs() < 0.5 && (last.rx1 - g.lr as f64).abs() < 0.5 {
                // Overprint duplicate: the *same* character re-stamped, offset by a
                // fraction of its width (a kashida/elongation segment re-drawn for
                // weight). docling-parse drops it; appending over-counts
                // (right_to_left_02's `قويووووة` vs `قويوووة`). Require a real offset
                // (> 0.1) so a ligature expansion — which decomposes one glyph into
                // several chars at the *identical* box (`ﬀ`→`ff`, diff ≈ 0) — is still
                // recomposed; real doubled letters sit a full advance apart (> 0.5).
                let offset = (g.ll as f64 - last.rx0).abs();
                if euclidean && offset > 0.1 && last.text.ends_with(g.ch) {
                    continue;
                }
                last.text.push(g.ch);
                last.ltr = !is_right_to_left(&last.text);
                if let Some(w) = last.words.last_mut() {
                    w.text.push(g.ch);
                    w.extend(g.ll as f64, g.lb as f64, g.lr as f64, g.lt as f64);
                }
                continue;
            }
        }
        let text = g.ch.to_string();
        let ltr = !is_right_to_left(&text);
        cells.push(Cell {
            words: vec![WordSeg::from_glyph(
                text.clone(),
                g.ll as f64,
                g.lb as f64,
                g.lr as f64,
                g.lt as f64,
            )],
            text,
            rx0: g.ll as f64,
            ry0: g.lb as f64,
            rx1: g.lr as f64,
            ry1: g.lb as f64,
            rx2: g.lr as f64,
            ry2: g.lt as f64,
            rx3: g.ll as f64,
            ry3: g.lt as f64,
            ltr,
            active: true,
            lig_carry: false,
            font: g.font,
        });
    }
    cells
}

/// Build line cells from a page's glyph stream via the docling-parse contraction.
pub(crate) fn line_cells(glyphs: &[Glyph], page_h: f32, euclidean: bool) -> Vec<TextCell> {
    line_and_word_cells(glyphs, page_h, euclidean).0
}

/// Build **word** cells from a page's glyph stream via the same contraction as
/// [`line_cells`]: each line splits into its constituent words at exactly the
/// points where the contraction inserted a separator space. This reproduces
/// docling-parse's `word_cells` (the per-word tokens TableFormer matches against
/// table-grid cells), letting the pipeline drop pdfium's text path entirely
/// (roadmap item 6). Empty words (overprint-cleared) are skipped.
pub(crate) fn word_cells(glyphs: &[Glyph], page_h: f32, euclidean: bool) -> Vec<TextCell> {
    line_and_word_cells(glyphs, page_h, euclidean).1
}

/// Build the line cells **and** the word cells from one shared contraction — the
/// build+contract pass is the expensive step and is identical for both views, so
/// callers that need both (the default text layer) pay it once. Line cells come
/// from each contracted cell's text/box; word cells from its recorded word
/// segments.
pub(crate) fn line_and_word_cells(
    glyphs: &[Glyph],
    page_h: f32,
    euclidean: bool,
) -> (Vec<TextCell>, Vec<TextCell>) {
    let mut cells = build_cells(glyphs, euclidean);
    contract(&mut cells, euclidean);
    let mut words = Vec::new();
    let lines = cells
        .into_iter()
        .map(|c| {
            for w in c.words {
                if w.text.trim().is_empty() {
                    continue;
                }
                words.push(TextCell {
                    text: w.text,
                    l: w.l as f32,
                    t: page_h - w.t as f32,
                    r: w.r as f32,
                    b: page_h - w.b as f32,
                });
            }
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
        })
        .collect();
    (lines, words)
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
