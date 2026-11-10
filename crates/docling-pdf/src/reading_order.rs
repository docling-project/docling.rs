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
//! height are linked upper→lower. docling#4276's page separators are not part
//! of this.

const EPS: f32 = 1.0e-3;
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

/// `_init_l2r_map`: the same-row partner to the right of each element
/// (`l2r[i]`) and to the left (`r2l[j]`). Two elements pair when they are
/// consecutive in assembly order (`follows_maintext_order`: `cid + 1`), the
/// first is strictly left of the second, and they share a row (vertical IoU
/// above [`ROW_IOU`]). Computed once, on the undilated geometry.
fn init_l2r(elems: &[Bl], cids: &[usize]) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let n = elems.len();
    let mut l2r = vec![None; n];
    let mut r2l = vec![None; n];
    for i in 0..n {
        for j in 0..n {
            if cids[i] + 1 == cids[j]
                && elems[i].strictly_left_of(&elems[j])
                && elems[i].overlaps_v_iou(&elems[j], ROW_IOU)
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
fn init_ud(
    elems: &[Bl],
    l2r: &[Option<usize>],
    r2l: &[Option<usize>],
    cids: &[usize],
    graphic: &[bool],
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
            if has_interruption(elems, i, j) {
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
        if graphic[i] {
            continue;
        }
        let Some(j) = (0..n).find(|&j| cids[j] == cids[i] + 1) else {
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
/// horizontally breaks the direct `i → j` link.
fn has_interruption(elems: &[Bl], i: usize, j: usize) -> bool {
    for (w, ew) in elems.iter().enumerate() {
        if w == i || w == j {
            continue;
        }
        if (elems[i].overlaps_h(ew) || elems[j].overlaps_h(ew))
            && elems[i].strictly_above(ew)
            && ew.strictly_above(&elems[j])
        {
            return true;
        }
    }
    false
}

/// `_do_horizontal_dilation`: widen each element toward its first up- and
/// down-neighbour's horizontal extent, but only while the growth on each side
/// stays under the page-width threshold (else the element is left untouched).
fn dilate(orig: &[Bl], up: &[Vec<usize>], dn: &[Vec<usize>], page_w: f32) -> Vec<Bl> {
    let th = DILATION_THRESHOLD_NORM * page_w;
    let mut dil = orig.to_vec();
    for i in 0..orig.len() {
        let mut x0 = orig[i].l;
        let mut x1 = orig[i].r;
        let mut skip = false;
        if let Some(&u) = up[i].first() {
            let x0d = x0.min(orig[u].l);
            let x1d = x1.max(orig[u].r);
            if (x0 - x0d) > th || (x1d - x1) > th {
                skip = true;
            } else {
                x0 = x0d;
                x1 = x1d;
            }
        }
        if !skip {
            if let Some(&d) = dn[i].first() {
                let x0d = x0.min(orig[d].l);
                let x1d = x1.max(orig[d].r);
                if (x0 - x0d) > th || (x1d - x1) > th {
                    skip = true;
                } else {
                    x0 = x0d;
                    x1 = x1d;
                }
            }
        }
        // docling#5131: a widening that would make the box overlap any other
        // element is dropped — the widened box would reach over a neighbouring
        // column and become a parent of its elements. (Upstream's check ran
        // after the box was already written, so it never fired; our earlier
        // port had no check at all, matching that.) The test is against the
        // undilated boxes, as upstream's `page_elems`.
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
/// not-yet-visited ancestor of `j`.
fn dfs_up(j: usize, up: &[Vec<usize>], visited: &[bool]) -> usize {
    let mut k = j;
    loop {
        let mut moved = false;
        for &ind in &up[k] {
            if !visited[ind] {
                k = ind;
                moved = true;
                break;
            }
        }
        if !moved {
            return k;
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
/// with their assembly-order `cids` and which are graphics (table, picture,
/// code). Returns the permutation of input indices in reading order.
fn predict(orig: &[Bl], cids: &[usize], graphic: &[bool], page_w: f32, page_h: f32) -> Vec<usize> {
    let n = orig.len();
    if n == 0 {
        return Vec::new();
    }
    // Same-row pairs from the original geometry, once; adjacency from the
    // dilated boxes, but head/child sorting from the original geometry
    // (docling's `_find_heads`/`_sort_ud_maps` take `page_elements`).
    let (l2r, r2l) = init_l2r(orig, cids);
    let (up0, dn0) = init_ud(orig, &l2r, &r2l, cids, graphic, page_w, page_h);
    let dil = dilate(orig, &up0, &dn0, page_w);
    let (up, mut dn) = init_ud(&dil, &l2r, &r2l, cids, graphic, page_w, page_h);

    let by_geom = |a: usize, b: usize| orig[a].before(&orig[b]);

    // Heads sort by `_find_heads`' comparator (docling#4438): a head separated
    // above the other by spanning text reads first, else the geometric
    // `__lt__`. Children keep the plain `__lt__` (`_sort_ud_maps`).
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
    for children in dn.iter_mut() {
        py_sort(children, by_geom);
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
pub fn order_page(
    boxes: &[(f32, f32, f32, f32)],
    cids: &[usize],
    is_header: &[bool],
    is_footer: &[bool],
    is_graphic: &[bool],
    page_w: f32,
    page_h: f32,
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
    let mut out = Vec::with_capacity(boxes.len());
    for group in groups {
        // Convert this group to bottom-left origin.
        let bl: Vec<Bl> = group
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
        let group_cids: Vec<usize> = group.iter().map(|&i| cids[i]).collect();
        let group_graphic: Vec<bool> = group.iter().map(|&i| is_graphic[i]).collect();
        for local in predict(&bl, &group_cids, &group_graphic, page_w, page_h) {
            out.push(group[local]);
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
