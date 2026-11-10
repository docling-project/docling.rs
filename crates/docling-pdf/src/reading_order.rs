//! Rule-based reading order, ported from docling-ibm-models
//! `reading_order/reading_order_rb.py` (`ReadingOrderPredictor`).
//!
//! For each page it builds an up/down adjacency graph between elements purely
//! from geometry — an element is "below" another that is *strictly above* it and
//! *horizontally overlapping*, unless a third element interrupts the vertical
//! run between them — then horizontally **dilates** narrow elements toward their
//! column neighbours (so a one-line box widens to its paragraph's column), redoes
//! the graph, and depth-first traverses from the top-most elements to produce the
//! reading sequence. This reproduces docling's multi-column reading order (author
//! blocks, two-column body text) that a purely geometric top-to-bottom sort gets
//! wrong.
//!
//! Everything runs in **bottom-left origin** (y grows upward), matching docling;
//! callers pass top-left page coordinates and the page height.
//!
//! **Same-row pairs** (`_init_l2r_map`, docling 2.127 — #424): two elements
//! that are *consecutive in the page's assembly order* (docling's `cid`, the
//! postprocessor's source-cell order), the left one strictly left of the right
//! one, and sharing a row (vertical IoU > 0.8) are linked left→right. The link
//! is an up/down edge in its own right, and a vertical edge that would land on
//! the left partner is redirected to the row's right-most element, so a page
//! reads `left, right, next row` instead of stranding the right-hand item
//! wherever the vertical graph happens to reach it. The row test needs the
//! callers' `cids`; without them (all distinct but non-consecutive) no row
//! links form, which is the pre-#424 behaviour.
//!
//! **Slightly overlapping column neighbours** (docling#4276's link in
//! `_init_ud_maps`): consecutive text boxes of a column can overlap by a
//! point or so at their edges, so the upper one is not *strictly* above the
//! lower one and the vertical pass draws no edge between them — the upper box
//! then flows into whatever it does reach, often the next column, and the
//! column reads its paragraphs pairwise swapped. Two `cids`-consecutive
//! elements, neither a graphic (table, picture, code), whose left edges agree
//! within 1 % of the page width and whose overlap is at most 0.25 % of the page
//! height are linked upper→lower.
//!
//! **Page separators** (docling#4276, `use_reading_order_separators`, on by
//! default upstream; #1092): the page's long, thin rules — stroked
//! axis-aligned segments and thin filled boxes, from the text parser's path
//! walk ([`crate::textparse`]'s shape record, docling-parse's
//! `get_shape_lines` / `get_connected_shape_bounding_boxes`) — become
//! [`Separator`]s when they cross no content and have content on both sides
//! ([`build_separators`], `build_page_separators`). A horizontal one is a node
//! of the up/down graph: what is above it is read before what is below it, so
//! a band divided from the next by a rule is finished first (rows of icon +
//! text in a manual read icon, text, icon, text). A vertical one blocks a
//! same-row link across it and clamps the dilation at it. Only the page's top
//! level gets them, as upstream; a form's children are ordered without.
//! `DOCLING_RS_PDF_ORDER_SEPARATORS=0` turns them off (upstream's option).
//!
//! The same port brings upstream's two other rules: a same-row link needs an
//! empty gap (`_has_page_element_between`), and the dilation widens a box
//! only along a one-to-one vertical edge (`_is_one_to_one_vertical_edge`).

const EPS: f32 = 1.0e-3;
/// A horizontal separator's least length, normalized by page width
/// (`_MIN_HORIZONTAL_SEPARATOR_LENGTH_NORM`).
const MIN_HORIZONTAL_SEPARATOR_NORM: f32 = 0.08;
/// A vertical separator's least length, normalized by page height
/// (`_MIN_VERTICAL_SEPARATOR_LENGTH_NORM`).
const MIN_VERTICAL_SEPARATOR_NORM: f32 = 0.05;
/// The thickest filled box that still reads as a rule, points
/// (`_MAX_FILLED_RULE_THICKNESS`); a stroked segment must be axis-aligned
/// within [`AXIS_ALIGNMENT_TOLERANCE`].
const MAX_FILLED_RULE_THICKNESS: f32 = 3.5;
const AXIS_ALIGNMENT_TOLERANCE: f32 = 1.0e-3;
/// How close two collinear pieces must come to merge, points
/// (`_SEPARATOR_MERGE_TOLERANCE`).
const SEPARATOR_MERGE_TOLERANCE: f32 = 1.0;
/// More raw candidates than this and the page gets no separators at all
/// (`_MAX_SEPARATOR_CANDIDATES`, docling#4632): the merge is quadratic, and
/// geometry that dense (a seat map of small rectangles) never yields
/// trustworthy rules.
const MAX_SEPARATOR_CANDIDATES: usize = 5000;
/// How far a lower box's top may reach into the upper box and still read
/// after it, normalized by page height (`_NEAR_VERTICAL_OVERLAP_THRESHOLD_NORM`).
const NEAR_VERTICAL_OVERLAP_NORM: f32 = 0.0025;
/// How far apart the two left edges may be, normalized by page width
/// (`_LEFT_EDGE_ALIGNMENT_THRESHOLD_NORM`).
const LEFT_EDGE_ALIGNMENT_NORM: f32 = 0.01;
/// Horizontal-dilation threshold, normalized by page width
/// (`_horizontal_dilation_threshold_norm`).
const DILATION_THRESHOLD_NORM: f32 = 0.15;
/// Vertical IoU two consecutive elements need to count as one row
/// (`overlaps_vertically_with_iou(pelem_j, 0.8)`).
const ROW_IOU: f32 = 0.8;

/// A page element's box in bottom-left origin: `t > b` (top edge higher).
#[derive(Clone, Copy)]
struct Bl {
    l: f32,
    b: f32,
    r: f32,
    t: f32,
}

impl Bl {
    /// `overlaps_horizontally` (docling_core `BoundingBox`).
    fn overlaps_h(&self, o: &Bl) -> bool {
        !(self.r <= o.l || o.r <= self.l)
    }
    /// `is_strictly_above` (bottom-left branch): self's bottom edge sits above
    /// other's top edge.
    fn strictly_above(&self, o: &Bl) -> bool {
        (self.b + EPS) > o.t
    }
    /// `is_strictly_left_of`: self ends before other starts.
    fn strictly_left_of(&self, o: &Bl) -> bool {
        (self.r + EPS) < o.l
    }
    /// `overlaps_vertically_with_iou` (bottom-left branch): the vertical
    /// intersection over the vertical union exceeds `iou`; disjoint spans fail.
    fn overlaps_v_iou(&self, o: &Bl, iou: f32) -> bool {
        if self.t <= o.b || o.t <= self.b {
            return false;
        }
        let (u0, u1) = (self.b.min(o.b), self.t.max(o.t));
        let (i0, i1) = (self.b.max(o.b), self.t.min(o.t));
        (i1 - i0) / (u1 - u0) > iou
    }
    /// `overlaps` (docling_core `BoundingBox`): the boxes share area — both
    /// spans overlap, touching edges do not count.
    fn overlaps(&self, o: &Bl) -> bool {
        self.overlaps_h(o) && !(self.t <= o.b || o.t <= self.b)
    }
    /// `PageElement.__lt__` for same-page elements: a horizontally-overlapping
    /// pair reads higher-first (larger bottom edge in bottom-left), otherwise the
    /// left-most reads first. Returns whether `self` reads before `other`.
    fn before(&self, o: &Bl) -> bool {
        if self.overlaps_h(o) {
            self.b > o.b
        } else {
            self.l < o.l
        }
    }
}

/// A page rule the reading order does not read across — docling's
/// `SeparatorElement`, in bottom-left page points. A horizontal one has
/// `b == t` (the rule's height), a vertical one `l == r`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Separator {
    pub horizontal: bool,
    pub l: f32,
    pub b: f32,
    pub r: f32,
    pub t: f32,
}

impl Separator {
    fn bl(&self) -> Bl {
        Bl {
            l: self.l,
            b: self.b,
            r: self.r,
            t: self.t,
        }
    }
}

/// One page element as [`build_separators`] sees it: its box (bottom-left),
/// whether it holds text, and whether it is a graphic (table, picture, code).
#[derive(Clone, Copy, Debug)]
pub struct SeparatorContext {
    pub l: f32,
    pub b: f32,
    pub r: f32,
    pub t: f32,
    pub has_text: bool,
    pub graphic: bool,
}

impl SeparatorContext {
    /// docling's structural elements: those with text, and the graphics.
    fn structural(&self) -> bool {
        self.has_text || self.graphic
    }
}

/// `_candidate_separator`: a box thin along one axis and longer along the
/// other, clipped to the page; a stroked segment (`allow_thickness` false)
/// must be axis-aligned, a filled box may be up to 3.5 pt thick.
fn candidate_separator(
    [l, b, r, t]: [f32; 4],
    page_w: f32,
    page_h: f32,
    allow_thickness: bool,
) -> Option<Separator> {
    let (w, h) = (r - l, t - b);
    let max_thickness = if allow_thickness {
        MAX_FILLED_RULE_THICKNESS
    } else {
        AXIS_ALIGNMENT_TOLERANCE
    };
    if h <= max_thickness && w > h {
        let (left, right) = (l.max(0.0), r.min(page_w));
        if left >= right {
            return None;
        }
        let y = (b + t) / 2.0;
        return Some(Separator {
            horizontal: true,
            l: left,
            r: right,
            b: y,
            t: y,
        });
    }
    if w <= max_thickness && h > w {
        let (bottom, top) = (b.max(0.0), t.min(page_h));
        if bottom >= top {
            return None;
        }
        let x = (l + r) / 2.0;
        return Some(Separator {
            horizontal: false,
            l: x,
            r: x,
            b: bottom,
            t: top,
        });
    }
    None
}

/// `_separator_candidates_can_merge`: collinear within the tolerance and
/// touching or overlapping along the rule.
fn can_merge(a: &Separator, c: &Separator) -> bool {
    let tol = SEPARATOR_MERGE_TOLERANCE;
    if a.horizontal != c.horizontal {
        return false;
    }
    if a.horizontal {
        (a.b - c.b).abs() <= tol && c.l <= a.r + tol && a.l <= c.r + tol
    } else {
        (a.l - c.l).abs() <= tol && c.b <= a.t + tol && a.b <= c.t + tol
    }
}

/// `_merge_separator_candidates_once`: in (orientation, position, start)
/// order, each candidate joins the first kept separator it can merge with,
/// which takes the union along the rule and the midpoint across it.
fn merge_once(candidates: &[Separator]) -> Vec<Separator> {
    let mut ordered = candidates.to_vec();
    // Python's sort is stable, and so is this one; "horizontal" < "vertical".
    ordered.sort_by(|a, c| {
        let key = |s: &Separator| {
            if s.horizontal {
                (0u8, s.b, s.l)
            } else {
                (1u8, s.l, s.b)
            }
        };
        let (ka, kc) = (key(a), key(c));
        ka.0.cmp(&kc.0)
            .then(ka.1.total_cmp(&kc.1))
            .then(ka.2.total_cmp(&kc.2))
    });
    let mut merged: Vec<Separator> = Vec::new();
    for c in ordered {
        match merged.iter_mut().find(|m| can_merge(m, &c)) {
            None => merged.push(c),
            Some(m) if c.horizontal => {
                let y = (m.b + c.b) / 2.0;
                m.l = m.l.min(c.l);
                m.r = m.r.max(c.r);
                m.b = y;
                m.t = y;
            }
            Some(m) => {
                let x = (m.l + c.l) / 2.0;
                m.b = m.b.min(c.b);
                m.t = m.t.max(c.t);
                m.l = x;
                m.r = x;
            }
        }
    }
    merged
}

/// `_separator_crosses_content`: the rule runs through an element with text,
/// or through a graphic (a chart axis, a table rule overshooting its box).
fn crosses_content(s: &Separator, elements: &[SeparatorContext]) -> bool {
    elements.iter().filter(|e| e.structural()).any(|e| {
        if s.horizontal {
            e.b + EPS < s.b && s.b < e.t - EPS && e.l < s.r && s.l < e.r
        } else {
            e.l + EPS < s.l && s.l < e.r - EPS && e.b < s.t && s.b < e.t
        }
    })
}

/// `_separator_is_inside_graphic`: the rule lies within a graphic's box (its
/// own frame).
fn inside_graphic(s: &Separator, elements: &[SeparatorContext]) -> bool {
    elements
        .iter()
        .filter(|e| e.graphic)
        .any(|e| e.l - EPS <= s.l && s.r <= e.r + EPS && e.b - EPS <= s.b && s.t <= e.t + EPS)
}

/// `_separator_has_content_on_both_sides`: something structural above and
/// below a horizontal rule, within its span; left and right of a vertical one.
fn content_on_both_sides(s: &Separator, elements: &[SeparatorContext]) -> bool {
    let structural = || elements.iter().filter(|e| e.structural());
    if s.horizontal {
        let spans = |e: &&SeparatorContext| e.l < s.r && s.l < e.r;
        structural().filter(spans).any(|e| e.b >= s.b)
            && structural().filter(spans).any(|e| e.t <= s.t)
    } else {
        let spans = |e: &&SeparatorContext| e.b < s.t && s.b < e.t;
        structural().filter(spans).any(|e| e.r <= s.l)
            && structural().filter(spans).any(|e| e.l >= s.r)
    }
}

/// `build_page_separators`: the page's trustworthy reading-order rules.
/// `lines` are the stroked axis-aligned segments and `boxes` the visible
/// shapes' boxes (`[l, b, r, t]`, bottom-left page points), `elements` the
/// page's elements. Candidates are merged until stable (two rules left
/// overlapping at one height would each be strictly above the other — a
/// cycle), then kept when long enough (8 % of the width, 5 % of the height),
/// crossing no content, not inside a graphic, and with content on both
/// sides. Upstream's document time budget is not ported: the candidate cap
/// already bounds the merge.
pub fn build_separators(
    page_w: f32,
    page_h: f32,
    elements: &[SeparatorContext],
    lines: &[[f32; 4]],
    boxes: &[[f32; 4]],
) -> Vec<Separator> {
    let mut candidates: Vec<Separator> = lines
        .iter()
        .map(|&b| (b, false))
        .chain(boxes.iter().map(|&b| (b, true)))
        .filter_map(|(b, thick)| candidate_separator(b, page_w, page_h, thick))
        .collect();
    if candidates.len() > MAX_SEPARATOR_CANDIDATES {
        docling_core::debug_log!(
            "reading order: {} separator candidates exceed {MAX_SEPARATOR_CANDIDATES}; \
             no separators on this page",
            candidates.len()
        );
        return Vec::new();
    }
    loop {
        let merged = merge_once(&candidates);
        let stable = merged.len() == candidates.len();
        candidates = merged;
        if stable {
            break;
        }
    }
    candidates
        .into_iter()
        .filter(|s| {
            let long_enough = if s.horizontal {
                s.r - s.l >= MIN_HORIZONTAL_SEPARATOR_NORM * page_w
            } else {
                s.t - s.b >= MIN_VERTICAL_SEPARATOR_NORM * page_h
            };
            long_enough
                && !crosses_content(s, elements)
                && !inside_graphic(s, elements)
                && content_on_both_sides(s, elements)
        })
        .collect()
}

/// `_has_vertical_separator_between`: a vertical rule in the gap between two
/// elements, along their shared height.
fn vertical_separator_between(lhs: &Bl, rhs: &Bl, vseps: &[Separator]) -> bool {
    let (ob, ot) = (lhs.b.max(rhs.b), lhs.t.min(rhs.t));
    if ob >= ot {
        return false;
    }
    let (gap_l, gap_r) = (lhs.r.min(rhs.r), lhs.l.max(rhs.l));
    vseps
        .iter()
        .any(|s| gap_l <= s.l && s.l <= gap_r && s.b < ot && ob < s.t)
}

/// `_has_page_element_between`: another page element (not a separator) in
/// the gap between two elements of a row, along their shared height.
fn page_element_between(elems: &[Bl], sep: &[bool], i: usize, j: usize) -> bool {
    let (left, right) = (&elems[i], &elems[j]);
    let (ob, ot) = (left.b.max(right.b), left.t.min(right.t));
    elems.iter().enumerate().any(|(w, e)| {
        w != i && w != j && !sep[w] && left.r < e.r && e.l < right.l && ob < e.t && e.b < ot
    })
}

/// `_clamp_dilation_to_vertical_separators`: a widening stops at a vertical
/// rule beside the element, along its height.
fn clamp_to_vertical_separators(
    e: &Bl,
    mut x0: f32,
    mut x1: f32,
    vseps: &[Separator],
) -> (f32, f32) {
    for s in vseps {
        if s.t <= e.b || e.t <= s.b {
            continue;
        }
        if x0 < s.l && s.l <= e.l {
            x0 = x0.max(s.l);
        }
        if e.r <= s.l && s.l < x1 {
            x1 = x1.min(s.l);
        }
    }
    (x0, x1)
}

/// `_init_l2r_map`: the same-row partner to the right of each element
/// (`l2r[i]`) and to the left (`r2l[j]`). Two elements pair when they are
/// consecutive in assembly order (`follows_maintext_order`: `cid + 1`), the
/// first is strictly left of the second, and they share a row (vertical IoU
/// above [`ROW_IOU`]). Computed once, on the undilated geometry. The pair
/// also needs an empty gap — no other element between them along their
/// shared height (`_has_page_element_between`) — and no vertical separator
/// between them (`_has_vertical_separator_between`); separators pair with
/// nothing.
fn init_l2r(
    elems: &[Bl],
    cids: &[usize],
    sep: &[bool],
    vseps: &[Separator],
) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let n = elems.len();
    let mut l2r = vec![None; n];
    let mut r2l = vec![None; n];
    for i in 0..n {
        // A separator follows nothing in the main text (its `cid` is
        // negative upstream).
        if sep[i] {
            continue;
        }
        for j in 0..n {
            if !sep[j]
                && cids[i].checked_add(1) == Some(cids[j])
                && elems[i].strictly_left_of(&elems[j])
                && elems[i].overlaps_v_iou(&elems[j], ROW_IOU)
                && !vertical_separator_between(&elems[i], &elems[j], vseps)
                && !page_element_between(elems, sep, i, j)
            {
                l2r[i] = Some(j);
                r2l[j] = Some(i);
            }
        }
    }
    (l2r, r2l)
}

/// Build the up/down adjacency maps (`_init_ud_maps`). `up[j]` lists elements
/// directly above `j`; `dn[i]` lists elements directly below `i`. The rtree of
/// the original is replaced by a brute-force scan (pages carry few regions) with
/// the identical predicates, so the edge set matches.
///
/// Same-row pairs (`l2r`/`r2l`, #424) shape the graph two ways, as upstream:
/// an element's left partner is linked as its first "up" neighbour, and a
/// vertical edge whose upper end has a right partner is redirected along the
/// row to its right-most element — so the row is read through before the
/// element below it.
///
/// Then the slightly-overlapping column neighbours of docling#4276 (see the
/// module docs): a `cids`-consecutive pair the strict-above test missed.
// One positional per upstream input (`page_elems`, the maps, the flags):
// bundling them would only rename what the callers already hold.
#[allow(clippy::too_many_arguments)]
fn init_ud(
    elems: &[Bl],
    l2r: &[Option<usize>],
    r2l: &[Option<usize>],
    cids: &[usize],
    graphic: &[bool],
    sep: &[bool],
    page_w: f32,
    page_h: f32,
) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let n = elems.len();
    let mut up = vec![Vec::new(); n];
    let mut dn = vec![Vec::new(); n];
    for j in 0..n {
        if let Some(left) = r2l[j] {
            if !dn[left].contains(&j) {
                dn[left].push(j);
            }
            if !up[j].contains(&left) {
                up[j].push(left);
            }
        }
        for i in 0..n {
            if i == j {
                continue;
            }
            if !(elems[i].strictly_above(&elems[j]) && elems[i].overlaps_h(&elems[j])) {
                continue;
            }
            if has_interruption(elems, sep, i, j) {
                continue;
            }
            // Follow the row to its right-most element (`cid`s strictly
            // increase along it, so the walk ends).
            let mut k = i;
            while let Some(next) = l2r[k] {
                k = next;
            }
            dn[k].push(j);
            up[j].push(k);
        }
    }
    // Consecutive text boxes in a column can overlap slightly at their edges:
    // keep their source sequence when the strict-above test missed the link.
    let tolerance = page_h * NEAR_VERTICAL_OVERLAP_NORM;
    for i in 0..n {
        if graphic[i] || sep[i] {
            continue;
        }
        let Some(j) = (0..n).find(|&j| !sep[j] && cids[i].checked_add(1) == Some(cids[j])) else {
            continue;
        };
        if graphic[j] {
            continue;
        }
        let (upper, lower) = (&elems[i], &elems[j]);
        if (upper.l - lower.l).abs() < page_w * LEFT_EDGE_ALIGNMENT_NORM
            && upper.b < lower.t
            && lower.t <= upper.b + tolerance
            && upper.t > lower.t
            && upper.b > lower.b
            && !dn[i].contains(&j)
        {
            dn[i].push(j);
            up[j].push(i);
        }
    }
    (up, dn)
}

/// `_has_sequence_interruption`: a third element `w` between `i` and `j`
/// vertically (strictly below `i`, strictly above `j`) that overlaps either
/// horizontally breaks the direct `i → j` link. When exactly one of the two
/// is a horizontal separator, only the other element's lane counts: a
/// page-wide rule is a synchronization point, not an extension of the
/// ordinary element's horizontal lane.
fn has_interruption(elems: &[Bl], sep: &[bool], i: usize, j: usize) -> bool {
    for (w, ew) in elems.iter().enumerate() {
        if w == i || w == j {
            continue;
        }
        let overlaps_sequence = if sep[i] != sep[j] {
            let lane = if sep[i] { j } else { i };
            elems[lane].overlaps_h(ew)
        } else {
            elems[i].overlaps_h(ew) || elems[j].overlaps_h(ew)
        };
        if overlaps_sequence && elems[i].strictly_above(ew) && ew.strictly_above(&elems[j]) {
            return true;
        }
    }
    false
}

/// `_do_horizontal_dilation`: widen each element toward its first up- and
/// down-neighbour's horizontal extent, but only while the growth on each side
/// stays under the page-width threshold (else the element is left untouched).
/// A neighbour widens it only along a one-to-one vertical edge — the upper
/// element's only child, the lower's only parent (`_is_one_to_one_vertical_edge`):
/// at a branch of the graph the neighbour belongs to several columns and
/// would merge them. A separator is never widened nor widens anything, and a
/// widening stops at a vertical separator.
fn dilate(
    orig: &[Bl],
    up: &[Vec<usize>],
    dn: &[Vec<usize>],
    sep: &[bool],
    vseps: &[Separator],
    page_w: f32,
) -> Vec<Bl> {
    let th = DILATION_THRESHOLD_NORM * page_w;
    let one_to_one = |upper: usize, lower: usize| dn[upper] == [lower] && up[lower] == [upper];
    let mut dil = orig.to_vec();
    for i in 0..orig.len() {
        if sep[i] {
            continue;
        }
        let mut x0 = orig[i].l;
        let mut x1 = orig[i].r;
        let mut skip = false;
        if let Some(&u) = up[i].first() {
            if !sep[u] && one_to_one(u, i) {
                let (x0d, x1d) = clamp_to_vertical_separators(
                    &orig[i],
                    x0.min(orig[u].l),
                    x1.max(orig[u].r),
                    vseps,
                );
                if (x0 - x0d) > th || (x1d - x1) > th {
                    skip = true;
                } else {
                    x0 = x0d;
                    x1 = x1d;
                }
            }
        }
        if !skip {
            if let Some(&d) = dn[i].first() {
                if !sep[d] && one_to_one(i, d) {
                    let (x0d, x1d) = clamp_to_vertical_separators(
                        &orig[i],
                        x0.min(orig[d].l),
                        x1.max(orig[d].r),
                        vseps,
                    );
                    if (x0 - x0d) > th || (x1d - x1) > th {
                        skip = true;
                    } else {
                        x0 = x0d;
                        x1 = x1d;
                    }
                }
            }
        }
        // docling#5131: a widening that would make the box overlap any other
        // element is dropped — the widened box would reach over a neighbouring
        // column and become a parent of its elements. (Upstream's check ran
        // after the box was already written, so it never fired; our earlier
        // port had no check at all, matching that.) The test is against the
        // undilated boxes, as upstream's `page_elems` — separators included.
        if !skip {
            let wide = Bl {
                l: x0,
                r: x1,
                ..orig[i]
            };
            let overlaps_rest = orig
                .iter()
                .enumerate()
                .any(|(j, o)| j != i && o.overlaps(&wide));
            if !overlaps_rest {
                dil[i].l = x0;
                dil[i].r = x1;
            }
        }
    }
    dil
}

/// `_is_separated_above` (docling#4438): `upper` reads before `lower` although
/// the two share no column, because a third element lies strictly between
/// them vertically and spans both horizontally — they sit in successive bands
/// of the page (centred headings over full-width legal text), not in
/// side-by-side columns. Upstream queries an rtree over the query box first;
/// the predicates alone already confine `w` to that box, so a scan matches.
fn is_separated_above(elems: &[Bl], upper: usize, lower: usize) -> bool {
    let (u, d) = (&elems[upper], &elems[lower]);
    if u.overlaps_h(d) || !u.strictly_above(d) {
        return false;
    }
    elems.iter().enumerate().any(|(w, ew)| {
        w != upper
            && w != lower
            && u.strictly_above(ew)
            && ew.strictly_above(d)
            && ew.overlaps_h(u)
            && ew.overlaps_h(d)
    })
}

/// Iterative `_depth_first_search_upwards`: climb `up` edges to the top-most
/// not-yet-visited ancestor of `j`, never through a node this walk already
/// passed — a cycle in `up` (two rules at one height each strictly above the
/// other) would otherwise loop forever.
fn dfs_up(j: usize, up: &[Vec<usize>], visited: &[bool]) -> usize {
    let mut k = j;
    let mut walked = vec![j];
    loop {
        let next = up[k]
            .iter()
            .copied()
            .find(|ind| !visited[*ind] && !walked.contains(ind));
        match next {
            Some(ind) => {
                k = ind;
                walked.push(ind);
            }
            None => return k,
        }
    }
}

/// Iterative `_depth_first_search_downwards` from `start`, appending to `order`.
fn dfs_down(
    start: usize,
    up: &[Vec<usize>],
    dn: &[Vec<usize>],
    order: &mut Vec<usize>,
    visited: &mut [bool],
) {
    // Each frame is (node, next child offset into dn[node]).
    let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
    while let Some(&(node, off)) = stack.last() {
        let mut found = false;
        let mut o = off;
        while o < dn[node].len() {
            let k = dfs_up(dn[node][o], up, visited);
            if !visited[k] {
                order.push(k);
                visited[k] = true;
                stack.last_mut().unwrap().1 = o + 1;
                stack.push((k, 0));
                found = true;
                break;
            }
            o += 1;
        }
        if !found {
            stack.pop();
        }
    }
}

/// Reading order of one group of page elements (already in bottom-left origin)
/// with their assembly-order `cids`, which are graphics (table, picture,
/// code) and which are horizontal separators (`sep`, nodes of the graph that
/// the caller drops from the order), and the page's vertical separators.
/// Returns the permutation of input indices in reading order.
fn predict(
    orig: &[Bl],
    cids: &[usize],
    graphic: &[bool],
    sep: &[bool],
    vseps: &[Separator],
    page_w: f32,
    page_h: f32,
) -> Vec<usize> {
    let n = orig.len();
    if n == 0 {
        return Vec::new();
    }
    // Same-row pairs from the original geometry, once; adjacency from the
    // dilated boxes, but head/child sorting from the original geometry
    // (docling's `_find_heads`/`_sort_ud_maps` take `page_elements`).
    let (l2r, r2l) = init_l2r(orig, cids, sep, vseps);
    let (up0, dn0) = init_ud(orig, &l2r, &r2l, cids, graphic, sep, page_w, page_h);
    let dil = dilate(orig, &up0, &dn0, sep, vseps, page_w);
    let (mut up, mut dn) = init_ud(&dil, &l2r, &r2l, cids, graphic, sep, page_w, page_h);

    let by_geom = |a: usize, b: usize| orig[a].before(&orig[b]);

    // Heads sort by `_find_heads`' comparator (docling#4438): a head separated
    // above the other by spanning text reads first, else the geometric
    // `__lt__`. Parents and children keep the plain `__lt__` (`_sort_ud_maps`).
    let mut heads: Vec<usize> = (0..n).filter(|&i| up[i].is_empty()).collect();
    py_sort(&mut heads, |a, b| {
        if is_separated_above(orig, a, b) {
            true
        } else if is_separated_above(orig, b, a) {
            false
        } else {
            by_geom(a, b)
        }
    });
    // `_sort_ud_maps` sorts both maps: the upward walk takes the first
    // unvisited parent, so a node several columns lead into (a separator
    // under them) climbs to them in geometric order.
    for neighbours in up.iter_mut().chain(dn.iter_mut()) {
        py_sort(neighbours, by_geom);
    }

    let mut order = Vec::with_capacity(n);
    let mut visited = vec![false; n];
    for &h in &heads {
        if !visited[h] {
            order.push(h);
            visited[h] = true;
            dfs_down(h, &up, &dn, &mut order, &mut visited);
        }
    }
    // A malformed graph could leave elements unreached; append them in geometric
    // order so nothing is dropped (docling logs an error and returns short).
    if order.len() != n {
        let mut rest: Vec<usize> = (0..n).filter(|&i| !visited[i]).collect();
        py_sort(&mut rest, by_geom);
        order.extend(rest);
    }
    order
}

/// CPython's list sort for the sizes a page produces, driven by a `<`
/// comparator: detect the leading natural run (reversing a strictly descending
/// prefix), then stable binary-insert the rest — exactly Timsort for fewer
/// than 64 elements (`minrun == n`). docling sorts with
/// `functools.cmp_to_key(PageElement.__lt__)`, whose fuzzy geometric relation
/// (`before`) is **not** a strict total order on mixed overlap groups: Python
/// quietly produces the Timsort order, while `slice::sort_by` panics on the
/// Ord violation (the Korean OCR pages trip it). For a consistent comparator
/// this is just another stable sort, so already-conformant fixtures are
/// unaffected.
fn py_sort(v: &mut [usize], mut lt: impl FnMut(usize, usize) -> bool) {
    let n = v.len();
    if n < 2 {
        return;
    }
    // count_run: extend an ascending (`!lt(next, prev)`) or strictly
    // descending run from the start; a descending run is reversed in place.
    let mut run = 1;
    if lt(v[1], v[0]) {
        while run + 1 < n && lt(v[run + 1], v[run]) {
            run += 1;
        }
        v[..=run].reverse();
    } else {
        while run + 1 < n && !lt(v[run + 1], v[run]) {
            run += 1;
        }
    }
    // binarysort: stable binary insertion of the remainder.
    for i in run + 1..n {
        let pivot = v[i];
        let (mut lo, mut hi) = (0, i);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if lt(pivot, v[mid]) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        v.copy_within(lo..i, lo + 1);
        v[lo] = pivot;
    }
}

/// `predict_merges` (docling `ReadingOrderPredictor`): given elements already in
/// reading order, return for each head the ordered list of following elements to
/// merge into it. A merge chain starts at a TEXT element and extends to the next
/// non-skipped TEXT element as long as the head is *strictly left of* it
/// (horizontally adjacent — an author column, a wrap into the next column) and
/// the running tail ends with a lowercase letter / comma / hyphen while the
/// candidate starts with a letter. `boxes` are top-left; `strictly_left_of` is
/// horizontal-only, so origin does not matter. The cross-page branch is omitted —
/// this runs per page, where cross-page continuations are handled separately.
pub fn predict_merges(
    boxes: &[(f32, f32, f32, f32)],
    texts: &[String],
    is_text: &[bool],
    is_skip: &[bool],
) -> Vec<Vec<usize>> {
    let n = boxes.len();
    let mut merges = vec![Vec::new(); n];
    let mut curr: isize = -1;
    for ind in 0..n {
        if ind as isize <= curr || !is_text[ind] {
            continue;
        }
        let mut check = ind;
        loop {
            let mut p1 = check + 1;
            while p1 < n && is_skip[p1] {
                p1 += 1;
            }
            if p1 < n
                && is_text[p1]
                && strictly_left_of(boxes[ind], boxes[p1])
                && ends_mergeable(&texts[check])
                && starts_mergeable(&texts[p1])
            {
                merges[ind].push(p1);
                curr = p1 as isize;
                check = p1;
            } else {
                break;
            }
        }
    }
    merges
}

/// `is_strictly_left_of` (horizontal-only): `a.r + eps < b.l`.
fn strictly_left_of(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.2 + EPS < b.0
}

/// Head/tail of a merge ends with a lowercase letter, comma, hyphen or soft
/// hyphen (docling regex `.+([a-z,\-­])(\s*)` — at least two chars).
fn ends_mergeable(t: &str) -> bool {
    let s = t.trim_end();
    s.chars().count() >= 2
        && matches!(
            s.chars().next_back(),
            Some('a'..='z' | ',' | '-' | '\u{ad}')
        )
}

/// Merge candidate starts with a (Latin) letter and has more after it (docling
/// regex `(\s*[a-zA-ZÀ-ɏ])(.+)`).
fn starts_mergeable(t: &str) -> bool {
    let s = t.trim_start();
    let mut ch = s.chars();
    match ch.next() {
        Some(c) if c.is_ascii_alphabetic() || ('\u{c0}'..='\u{24f}').contains(&c) => {
            ch.next().is_some()
        }
        _ => false,
    }
}

/// Order one page's elements (top-left coords) into reading order, returning the
/// input-index permutation. `cids` are the elements' positions in docling's
/// assembly order (see [`crate::assemble::cluster_cids`]) — the same-row rule
/// pairs consecutive ones, and so does the slightly-overlapping column link,
/// which leaves out the `is_graphic` elements (table, picture, code).
/// `headers`/`footers` are ordered as their own groups and placed first/last,
/// matching docling's per-page header→body→footer split.
#[cfg(test)]
pub fn order_page(
    boxes: &[(f32, f32, f32, f32)],
    cids: &[usize],
    is_header: &[bool],
    is_footer: &[bool],
    is_graphic: &[bool],
    page_w: f32,
    page_h: f32,
) -> Vec<usize> {
    order_page_with_separators(
        boxes,
        cids,
        is_header,
        is_footer,
        is_graphic,
        page_w,
        page_h,
        &[],
    )
}

/// [`order_page`] with the page's [`Separator`]s (bottom-left), which apply to
/// the body group only — page headers and footers are ordered without them,
/// as upstream's `predict_reading_order`. Horizontal ones join the body's
/// graph as nodes and are dropped from the order; vertical ones constrain the
/// row links and the dilation.
#[allow(clippy::too_many_arguments)]
pub fn order_page_with_separators(
    boxes: &[(f32, f32, f32, f32)],
    cids: &[usize],
    is_header: &[bool],
    is_footer: &[bool],
    is_graphic: &[bool],
    page_w: f32,
    page_h: f32,
    separators: &[Separator],
) -> Vec<usize> {
    // Split into the three groups, remembering original indices.
    let mut groups: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for i in 0..boxes.len() {
        let g = if is_header[i] {
            0
        } else if is_footer[i] {
            2
        } else {
            1
        };
        groups[g].push(i);
    }
    let vseps: Vec<Separator> = separators
        .iter()
        .filter(|s| !s.horizontal)
        .copied()
        .collect();
    let mut out = Vec::with_capacity(boxes.len());
    for (g, group) in groups.into_iter().enumerate() {
        // Convert this group to bottom-left origin.
        let mut bl: Vec<Bl> = group
            .iter()
            .map(|&i| {
                let (l, t, r, b) = boxes[i];
                Bl {
                    l,
                    r,
                    t: page_h - t,
                    b: page_h - b,
                }
            })
            .collect();
        let mut group_cids: Vec<usize> = group.iter().map(|&i| cids[i]).collect();
        let mut group_graphic: Vec<bool> = group.iter().map(|&i| is_graphic[i]).collect();
        let mut sep = vec![false; group.len()];
        let body = g == 1;
        if body {
            for s in separators.iter().filter(|s| s.horizontal) {
                bl.push(s.bl());
                // Never consecutive with anything: the row and near-overlap
                // links skip separators before reading their `cid`.
                group_cids.push(usize::MAX);
                group_graphic.push(false);
                sep.push(true);
            }
        }
        let group_vseps: &[Separator] = if body { &vseps } else { &[] };
        for local in predict(
            &bl,
            &group_cids,
            &group_graphic,
            &sep,
            group_vseps,
            page_w,
            page_h,
        ) {
            if let Some(&i) = group.get(local) {
                out.push(i);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::order_page;

    /// The #424 row (top-left points, a Pearson copyright page): the LCCN sits
    /// on the same line as the Dewey number, right of it, and is nothing's
    /// horizontal neighbour below — the vertical graph alone reads it last.
    /// Consecutive in source order with the Dewey number, the same-row rule
    /// links the two and hangs the next paragraph off the LCCN.
    fn copyright_rows() -> Vec<(f32, f32, f32, f32)> {
        vec![
            (36.9, 575.5, 85.3, 583.2),   // 0 "005.1-dc22"
            (258.2, 575.5, 300.7, 583.2), // 1 "2008024750" (LCCN)
            (36.9, 588.5, 181.5, 596.2),  // 2 "Copyright © 2009 Pearson…"
            (36.9, 601.5, 388.4, 609.2),  // 3 "All rights reserved…"
        ]
    }

    #[test]
    fn a_right_hand_item_reads_before_the_next_row_when_it_follows_in_source_order() {
        let boxes = copyright_rows();
        let flags = vec![false; boxes.len()];
        let order = order_page(
            &boxes,
            &[10, 11, 12, 13],
            &flags,
            &flags,
            &flags,
            517.6,
            666.4,
        );
        assert_eq!(order, [0, 1, 2, 3]);
        // Not consecutive in assembly order (the LCCN cell came from
        // elsewhere in the stream): no row link, the vertical graph decides —
        // the pre-#424 order, kept for the record.
        let order = order_page(
            &boxes,
            &[10, 20, 11, 12],
            &flags,
            &flags,
            &flags,
            517.6,
            666.4,
        );
        assert_eq!(order, [0, 2, 3, 1]);
    }

    /// The row rule needs a real row: a consecutive pair that is left-of but
    /// a line apart does not link, and the order is the vertical graph's —
    /// identical to what non-consecutive numbering gives.
    #[test]
    fn a_row_link_needs_vertical_overlap() {
        let boxes = vec![
            (36.9, 575.5, 85.3, 583.2),
            (258.2, 590.0, 300.7, 597.7), // right of 0 but a line lower
            (36.9, 601.5, 388.4, 609.2),
        ];
        let flags = vec![false; 3];
        let linked = order_page(&boxes, &[0, 1, 2], &flags, &flags, &flags, 517.6, 666.4);
        let unlinked = order_page(&boxes, &[0, 5, 1], &flags, &flags, &flags, 517.6, 666.4);
        assert_eq!(linked, unlinked);
        assert_eq!(linked, [0, 1, 2]);
    }

    /// Upstream's `_box(cid, label, b, t, l=, r=)` elements are bottom-left on
    /// a US-letter page; `order_page` takes top-left boxes, so flip them, and
    /// number the cids in input order as `_predicted_cids` does.
    fn predicted(bl: &[(f32, f32, f32, f32)]) -> Vec<usize> {
        const H: f32 = 792.0;
        let boxes: Vec<_> = bl
            .iter()
            .map(|&(b, t, l, r)| (l, H - t, r, H - b))
            .collect();
        let cids: Vec<usize> = (0..boxes.len()).collect();
        let flags = vec![false; boxes.len()];
        order_page(&boxes, &cids, &flags, &flags, &flags, 612.0, H)
    }

    /// docling#5131 `test_horizontal_dilation_skips_a_widening_that_overlaps_a_neighbour`.
    #[test]
    fn horizontal_dilation_skips_a_widening_that_overlaps_a_neighbour() {
        let order = predicted(&[
            (740.0, 760.0, 240.0, 360.0),
            (705.0, 720.0, 60.0, 220.0),
            (600.0, 690.0, 40.0, 290.0),
            (500.0, 590.0, 40.0, 290.0),
            (640.0, 730.0, 310.0, 560.0),
        ]);
        assert_eq!(order, [0, 1, 2, 3, 4]);
    }

    /// docling#5131 `test_horizontal_dilation_skips_a_widening_to_a_wide_caption_below`.
    #[test]
    fn horizontal_dilation_skips_a_widening_to_a_wide_caption_below() {
        let order = predicted(&[
            (600.0, 700.0, 40.0, 290.0),
            (480.0, 590.0, 40.0, 290.0),
            (620.0, 700.0, 310.0, 560.0),
            (540.0, 610.0, 310.0, 560.0),
            (450.0, 530.0, 350.0, 560.0),
            (420.0, 440.0, 40.0, 340.0),
        ]);
        assert_eq!(order, [0, 1, 2, 3, 4, 5]);
    }

    /// docling#4438 `test_heads_separated_by_full_width_text_are_read_top_down`
    /// (docling#4433, page 4 of OJ L 2025/492).
    #[test]
    fn heads_separated_by_full_width_text_are_read_top_down() {
        let order = predicted(&[
            (561.8, 570.1, 67.4, 482.7),
            (521.4, 528.8, 67.4, 194.5),
            (480.9, 488.4, 278.0, 317.3),
            (405.6, 414.2, 285.4, 309.8),
            (322.0, 387.9, 67.4, 529.9),
            (263.5, 272.1, 274.6, 320.7),
            (237.4, 245.8, 67.4, 202.0),
            (190.4, 219.7, 67.4, 529.9),
        ]);
        assert_eq!(order, [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    /// docling#4438 `test_heads_in_side_by_side_columns_keep_column_order`.
    #[test]
    fn heads_in_side_by_side_columns_keep_column_order() {
        let order = predicted(&[
            (700.0, 720.0, 320.0, 560.0),
            (450.0, 680.0, 320.0, 560.0),
            (100.0, 400.0, 50.0, 290.0),
        ]);
        assert_eq!(order, [2, 0, 1]);
    }

    /// docling#4276 `test_consecutive_text_overlapping_slightly_stays_in_sequence`:
    /// 1 continues 0 in the left column but its top pokes 1 pt into 0, so 0 is
    /// not strictly above it; without the link, 0 flows into the right column
    /// (2) first.
    #[test]
    fn consecutive_text_overlapping_slightly_stays_in_sequence() {
        let order = predicted(&[
            (600.0, 700.0, 40.0, 560.0),
            (200.0, 601.0, 40.0, 280.0),
            (500.0, 590.0, 320.0, 560.0),
        ]);
        assert_eq!(order, [0, 1, 2]);
    }

    /// The link is for text: a table that pokes into the box above keeps the
    /// order the geometry gives, and so does a box whose left edge is not the
    /// upper one's, or one that reaches more than 0.25 % of the page into it.
    #[test]
    fn the_overlap_link_needs_aligned_text_and_a_small_overlap() {
        const H: f32 = 792.0;
        let flip = |bl: &[(f32, f32, f32, f32)]| -> Vec<(f32, f32, f32, f32)> {
            bl.iter()
                .map(|&(b, t, l, r)| (l, H - t, r, H - b))
                .collect()
        };
        let column = [
            (600.0, 700.0, 40.0, 560.0),
            (200.0, 601.0, 40.0, 280.0),
            (500.0, 590.0, 320.0, 560.0),
        ];
        let flags = vec![false; 3];
        let geometry = order_page(&flip(&column), &[0, 1, 2], &flags, &flags, &flags, 612.0, H);
        assert_eq!(geometry, [0, 1, 2]);
        let table = vec![false, true, false];
        let order = order_page(&flip(&column), &[0, 1, 2], &flags, &flags, &table, 612.0, H);
        assert_eq!(order, [0, 2, 1]);
        let shifted = [column[0], (200.0, 601.0, 60.0, 280.0), column[2]];
        let order = order_page(
            &flip(&shifted),
            &[0, 1, 2],
            &flags,
            &flags,
            &flags,
            612.0,
            H,
        );
        assert_eq!(order, [0, 2, 1]);
        let deep = [column[0], (200.0, 603.0, 40.0, 280.0), column[2]];
        let order = order_page(&flip(&deep), &[0, 1, 2], &flags, &flags, &flags, 612.0, H);
        assert_eq!(order, [0, 2, 1]);
    }
}

/// docling#4276's separators and the two predictor rules that came with it,
/// ported from docling's `tests/test_reading_order.py`.
#[cfg(test)]
mod separator_tests {
    use super::*;

    /// One element as upstream's tests write it: `cid`, bottom-left box, and
    /// whether it is a graphic.
    type El = (usize, f32, f32, f32, f32, bool);

    fn el(cid: usize, l: f32, b: f32, r: f32, t: f32) -> El {
        (cid, l, b, r, t, false)
    }

    /// `predict_reading_order` over bottom-left elements: the `cid`s in
    /// reading order.
    fn ordered(page: (f32, f32), elements: &[El], separators: &[Separator]) -> Vec<usize> {
        let (w, h) = page;
        let boxes: Vec<(f32, f32, f32, f32)> = elements
            .iter()
            .map(|e| (e.1, h - e.4, e.3, h - e.2))
            .collect();
        let cids: Vec<usize> = elements.iter().map(|e| e.0).collect();
        let graphic: Vec<bool> = elements.iter().map(|e| e.5).collect();
        let flags = vec![false; elements.len()];
        order_page_with_separators(&boxes, &cids, &flags, &flags, &graphic, w, h, separators)
            .into_iter()
            .map(|i| cids[i])
            .collect()
    }

    fn hsep(l: f32, r: f32, y: f32) -> Separator {
        Separator {
            horizontal: true,
            l,
            r,
            b: y,
            t: y,
        }
    }

    fn vsep(x: f32, b: f32, t: f32) -> Separator {
        Separator {
            horizontal: false,
            l: x,
            r: x,
            b,
            t,
        }
    }

    const PAGE: (f32, f32) = (600.0, 800.0);
    const DUMMY: (f32, f32) = (612.0, 792.0);

    #[test]
    fn horizontal_separator_finishes_upper_band_before_lower_columns() {
        let elements = [
            el(0, 40.0, 500.0, 250.0, 600.0),
            el(1, 330.0, 470.0, 560.0, 600.0),
            el(2, 40.0, 200.0, 250.0, 400.0),
            el(3, 330.0, 200.0, 560.0, 400.0),
        ];
        assert_eq!(ordered(PAGE, &elements, &[]), [0, 2, 1, 3]);
        assert_eq!(
            ordered(PAGE, &elements, &[hsep(40.0, 560.0, 440.0)]),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn horizontal_separator_finishes_columns_in_spatial_order() {
        let elements = [
            el(0, 40.0, 700.0, 560.0, 740.0),
            el(1, 40.0, 500.0, 180.0, 650.0),
            el(2, 400.0, 500.0, 540.0, 650.0),
            el(3, 220.0, 500.0, 360.0, 650.0),
            el(4, 40.0, 200.0, 560.0, 350.0),
        ];
        assert_eq!(
            ordered(PAGE, &elements, &[hsep(40.0, 560.0, 400.0)]),
            [0, 1, 3, 2, 4]
        );
    }

    #[test]
    fn horizontal_separator_finishes_sidecar_before_next_band() {
        let elements = [
            el(0, 40.0, 620.0, 420.0, 680.0),
            el(1, 40.0, 580.0, 180.0, 610.0),
            el(2, 40.0, 540.0, 260.0, 570.0),
            el(3, 40.0, 200.0, 560.0, 350.0),
            (30, 480.0, 620.0, 560.0, 680.0, true),
        ];
        let separators = [hsep(40.0, 560.0, 700.0), hsep(40.0, 560.0, 500.0)];
        assert_eq!(ordered(PAGE, &elements, &separators), [0, 1, 2, 30, 3]);
    }

    #[test]
    fn horizontal_separator_is_not_a_dilation_anchor() {
        let element = Bl {
            l: 100.0,
            b: 300.0,
            r: 200.0,
            t: 350.0,
        };
        let separator = hsep(90.0, 210.0, 250.0).bl();
        let up = vec![vec![], vec![0]];
        let dn = vec![vec![1], vec![]];
        let dil = dilate(&[element, separator], &up, &dn, &[false, true], &[], PAGE.0);
        assert_eq!((dil[0].l, dil[0].r), (100.0, 200.0));
    }

    /// At a branch of the graph (a heading over two columns) the dilation
    /// widens nothing, so the columns stay apart.
    #[test]
    fn horizontal_dilation_preserves_columns_at_graph_branches() {
        let elements = [
            el(0, 40.0, 700.0, 360.0, 740.0),
            el(1, 40.0, 620.0, 180.0, 680.0),
            el(2, 40.0, 520.0, 180.0, 600.0),
            el(3, 220.0, 620.0, 360.0, 680.0),
            el(4, 220.0, 520.0, 360.0, 600.0),
            (30, 40.0, 400.0, 360.0, 500.0, true),
        ];
        assert_eq!(ordered(PAGE, &elements, &[]), [0, 1, 2, 3, 4, 30]);
    }

    #[test]
    fn graphics_continue_left_to_right_only_across_an_empty_gap() {
        let pic = |l: f32, r: f32| Bl {
            l,
            r,
            b: 200.0,
            t: 300.0,
        };
        let (l2r, _) = init_l2r(
            &[pic(40.0, 120.0), pic(300.0, 380.0)],
            &[0, 1],
            &[false; 2],
            &[],
        );
        assert_eq!(l2r, [Some(1), None]);
        let (l2r, _) = init_l2r(
            &[pic(40.0, 120.0), pic(160.0, 240.0), pic(300.0, 380.0)],
            &[0, 10, 1],
            &[false; 3],
            &[],
        );
        assert_eq!(l2r, [None, None, None]);
    }

    #[test]
    fn vertical_separator_blocks_left_to_right_link() {
        let elements = [
            el(0, 40.0, 600.0, 280.0, 700.0),
            el(1, 320.0, 600.0, 560.0, 700.0),
            el(2, 40.0, 400.0, 280.0, 580.0),
        ];
        assert_eq!(ordered(DUMMY, &elements, &[]), [0, 1, 2]);
        assert_eq!(
            ordered(DUMMY, &elements, &[vsep(300.0, 380.0, 720.0)]),
            [0, 2, 1]
        );
    }

    /// Two nodes each above the other, as two overlapping rules at one height
    /// were: the upward walk stops instead of looping.
    #[test]
    fn upward_walk_stops_on_up_map_cycle() {
        let up = vec![vec![1], vec![0]];
        assert_eq!(dfs_up(0, &up, &[false, false]), 1);
    }

    #[test]
    fn vertical_separator_clamps_horizontal_dilation() {
        let element = Bl {
            l: 100.0,
            b: 200.0,
            r: 200.0,
            t: 300.0,
        };
        let clamped =
            clamp_to_vertical_separators(&element, 80.0, 300.0, &[vsep(250.0, 100.0, 400.0)]);
        assert_eq!(clamped, (80.0, 250.0));
    }

    fn text(b: f32, t: f32, l: f32, r: f32) -> SeparatorContext {
        SeparatorContext {
            l,
            b,
            r,
            t,
            has_text: true,
            graphic: false,
        }
    }

    fn picture(b: f32, t: f32, l: f32, r: f32) -> SeparatorContext {
        SeparatorContext {
            l,
            b,
            r,
            t,
            has_text: false,
            graphic: true,
        }
    }

    fn line(l: f32, b: f32, r: f32, t: f32) -> [f32; 4] {
        [l, b, r, t]
    }

    fn built(
        page: (f32, f32),
        elements: &[SeparatorContext],
        lines: &[[f32; 4]],
    ) -> Vec<Separator> {
        build_separators(page.0, page.1, elements, lines, &[])
    }

    #[test]
    fn build_merges_visible_rules_and_rejects_text_crossing() {
        let elements = [
            text(500.0, 650.0, 40.0, 560.0),
            text(150.0, 300.0, 40.0, 560.0),
        ];
        // The top-left shapes of upstream's test, on the 800 pt page.
        let horizontal_line = line(40.0, 400.0, 560.0, 400.0);
        let filled_rule = line(40.0, 399.8, 560.0, 400.2);
        let crossing_vertical = line(300.0, 100.0, 300.0, 700.0);
        let separators = build_separators(
            PAGE.0,
            PAGE.1,
            &elements,
            &[horizontal_line, crossing_vertical],
            &[filled_rule],
        );
        assert_eq!(separators.len(), 1);
        assert!(separators[0].horizontal);
        assert_eq!((separators[0].l, separators[0].r), (40.0, 560.0));
    }

    fn two_bands() -> [SeparatorContext; 2] {
        [
            text(500.0, 600.0, 40.0, 560.0),
            text(200.0, 300.0, 40.0, 560.0),
        ]
    }

    fn hline(l: f32, r: f32, y: f32) -> [f32; 4] {
        [l, y, r, y]
    }

    fn vline(x: f32, b: f32, t: f32) -> [f32; 4] {
        [x, b, x, t]
    }

    #[test]
    fn build_joins_fragmented_rule() {
        let lines = [
            hline(381.0, 560.0, 400.0),
            hline(40.0, 200.0, 400.0),
            hline(200.5, 380.0, 400.0),
        ];
        let s = built(DUMMY, &two_bands(), &lines);
        assert_eq!(
            s.iter().map(|s| (s.l, s.r, s.b)).collect::<Vec<_>>(),
            [(40.0, 560.0, 400.0)]
        );
    }

    #[test]
    fn build_joins_short_horizontal_fragments() {
        let mut lines: Vec<[f32; 4]> = (0..13)
            .map(|i| hline(40.0 + i as f32 * 40.0, 79.5 + i as f32 * 40.0, 400.0))
            .collect();
        lines.extend([
            hline(100.0, 130.0, 390.0),
            hline(40.0, 70.0, 380.0),
            hline(72.0, 102.0, 380.0),
        ]);
        let s = built(DUMMY, &two_bands(), &lines);
        assert_eq!(
            s.iter().map(|s| (s.l, s.r, s.b)).collect::<Vec<_>>(),
            [(40.0, 559.5, 400.0)]
        );
    }

    #[test]
    fn build_joins_short_vertical_fragments() {
        let elements = [
            text(220.0, 580.0, 40.0, 250.0),
            text(220.0, 580.0, 350.0, 560.0),
        ];
        let mut lines: Vec<[f32; 4]> = (0..13)
            .map(|i| vline(300.0, 200.0 + i as f32 * 30.0, 229.5 + i as f32 * 30.0))
            .collect();
        lines.extend([
            vline(310.0, 300.0, 330.0),
            vline(320.0, 200.0, 230.0),
            vline(320.0, 232.0, 262.0),
        ]);
        let s = built(DUMMY, &elements, &lines);
        assert_eq!(
            s.iter().map(|s| (s.l, s.b, s.t)).collect::<Vec<_>>(),
            [(300.0, 200.0, 589.5)]
        );
    }

    /// One greedy pass left two rules overlapping at one height, each strictly
    /// above the other: the merge repeats until nothing changes.
    #[test]
    fn build_merges_until_no_rules_overlap() {
        let elements = [
            text(500.0, 600.0, 40.0, 360.0),
            text(200.0, 300.0, 40.0, 360.0),
        ];
        let lines = [
            hline(300.0, 360.0, 399.5),
            hline(40.0, 100.0, 400.0),
            hline(60.0, 280.0, 400.0),
            hline(250.0, 360.0, 400.5),
        ];
        let s = built(DUMMY, &elements, &lines);
        assert_eq!(
            s.iter().map(|s| (s.l, s.r, s.b)).collect::<Vec<_>>(),
            [(40.0, 360.0, 400.0)]
        );
    }

    /// 2206.01062 p9: a chart rule overshoots the picture box but runs
    /// through it.
    #[test]
    fn build_rejects_rule_through_graphic() {
        let elements = [
            picture(343.0, 707.0, 54.0, 549.0),
            text(720.0, 750.0, 54.0, 549.0),
            text(100.0, 320.0, 54.0, 549.0),
        ];
        let lines = [hline(53.0, 575.4, 530.9), hline(53.0, 575.4, 331.0)];
        let s = built(DUMMY, &elements, &lines);
        assert_eq!(s.iter().map(|s| s.b).collect::<Vec<_>>(), [331.0]);
    }

    #[test]
    fn build_rejects_graphic_frame_and_one_sided_rules() {
        let elements = [
            text(600.0, 700.0, 100.0, 500.0),
            picture(300.0, 500.0, 100.0, 500.0),
        ];
        let lines = [
            hline(100.0, 500.0, 550.0),
            hline(100.0, 500.0, 500.0),
            hline(100.0, 500.0, 200.0),
        ];
        let s = built(DUMMY, &elements, &lines);
        assert_eq!(s.iter().map(|s| s.b).collect::<Vec<_>>(), [550.0]);
    }

    /// docling#4632: a page of thousands of tiny thin rectangles (a seat map)
    /// gets no separators, quickly.
    #[test]
    fn build_skips_degenerate_candidate_counts() {
        // A deterministic spread of 6000 short thin boxes.
        let boxes: Vec<[f32; 4]> = (0..6000)
            .map(|k| {
                let x = (k * 37 % 580) as f32;
                let y = 150.0 + (k * 53 % 500) as f32;
                [x, y, x + 8.0 + (k % 20) as f32, y + 1.0 + (k % 3) as f32]
            })
            .collect();
        let s = build_separators(DUMMY.0, DUMMY.1, &two_bands(), &[], &boxes);
        assert!(s.is_empty());
    }

    #[test]
    fn build_merges_fragments_below_candidate_cap() {
        let elements = [
            text(700.0, 760.0, 40.0, 560.0),
            text(40.0, 100.0, 40.0, 560.0),
        ];
        let mut lines = Vec::new();
        for row in 0..5 {
            let y = 200.0 + row as f32 * 100.0;
            let mut x = 40.0f32;
            while x < 560.0 {
                lines.push(hline(x, x + 6.0, y));
                x += 6.1;
            }
        }
        let s = built(DUMMY, &elements, &lines);
        assert_eq!(s.len(), 5);
        assert!(s.iter().all(|s| s.horizontal));
    }
}
