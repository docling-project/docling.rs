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
//! callers pass top-left page coordinates and the page height. The `l2r`/`r2l`
//! maps are omitted because docling disables them (a `False and …` guard).

const EPS: f32 = 1.0e-3;
/// Horizontal-dilation threshold, normalized by page width
/// (`_horizontal_dilation_threshold_norm`).
const DILATION_THRESHOLD_NORM: f32 = 0.15;

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

/// Build the up/down adjacency maps (`_init_ud_maps`). `up[j]` lists elements
/// directly above `j`; `dn[i]` lists elements directly below `i`. The rtree of
/// the original is replaced by a brute-force scan (pages carry few regions) with
/// the identical predicates, so the edge set matches.
fn init_ud(elems: &[Bl]) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let n = elems.len();
    let mut up = vec![Vec::new(); n];
    let mut dn = vec![Vec::new(); n];
    for j in 0..n {
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
        if !skip {
            dil[i].l = x0;
            dil[i].r = x1;
        }
    }
    dil
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

/// Reading order of one group of page elements (already in bottom-left origin).
/// Returns the permutation of input indices in reading order.
fn predict(orig: &[Bl], page_w: f32) -> Vec<usize> {
    let n = orig.len();
    if n == 0 {
        return Vec::new();
    }
    // Adjacency from the dilated boxes, but head/child sorting from the original
    // geometry (docling's `_find_heads`/`_sort_ud_maps` take `page_elements`).
    let (up0, dn0) = init_ud(orig);
    let dil = dilate(orig, &up0, &dn0, page_w);
    let (up, mut dn) = init_ud(&dil);

    let by_geom = |a: &usize, b: &usize| {
        if orig[*a].before(&orig[*b]) {
            std::cmp::Ordering::Less
        } else if orig[*b].before(&orig[*a]) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    };

    let mut heads: Vec<usize> = (0..n).filter(|&i| up[i].is_empty()).collect();
    heads.sort_by(by_geom);
    for children in dn.iter_mut() {
        children.sort_by(by_geom);
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
        rest.sort_by(by_geom);
        order.extend(rest);
    }
    order
}

/// Order one page's elements (top-left coords) into reading order, returning the
/// input-index permutation. `headers`/`footers` are ordered as their own groups
/// and placed first/last, matching docling's per-page header→body→footer split.
pub fn order_page(
    boxes: &[(f32, f32, f32, f32)],
    is_header: &[bool],
    is_footer: &[bool],
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
        for local in predict(&bl, page_w) {
            out.push(group[local]);
        }
    }
    out
}
