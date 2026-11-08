//! Vector checkboxes (#609): the small squares a generator *draws* in front of
//! a checklist's lines, found in the page's path painting.
//!
//! docling knows a checkbox only from the layout model's `checkbox_selected` /
//! `checkbox_unselected` labels, and Heron gives them to drawn squares
//! sparingly — on the #609 reporter's ReportLab checklist it labels the four
//! options one `text` region, which docling then serializes as two garbled
//! paragraphs (`First option Third option` / `Second option Fourth option`,
//! its cell assignment alternating between two overlapping text clusters).
//! The squares themselves are unambiguous in the content stream, so the text
//! parser records the strokes it walks past ([`Ink`]) and [`find`] turns them
//! into [`CheckBox`]es; assembly then gives each boxed line its own checkbox
//! item ([`crate::assemble::split_checkbox_lines`]).
//!
//! A square is an outline — a stroked `re`, or four stroked axis-aligned
//! edges meeting at the corners (ReportLab draws each edge as its own
//! `m … l S` path) — between [`MIN_SIDE`] and [`MAX_SIDE`] points with sides
//! within 15 % of each other. Anything else painted inside it (a tick, a
//! cross, a filled dot) marks it checked. A square filled with colour — a
//! stroked-and-filled `re`, or one under a fill of its own size — is a chart
//! legend's swatch, not a checkbox; a white fill (a form's box background)
//! draws nothing and changes nothing.

/// A checkbox square, top-left page points (the [`TextCell`] frame).
///
/// [`TextCell`]: crate::pdfium_backend::TextCell
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckBox {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
    pub checked: bool,
}

/// One painted piece of a path, y-up page points (the glyphs' frame).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Ink {
    /// A stroked straight segment.
    Seg { x0: f64, y0: f64, x1: f64, y1: f64 },
    /// A stroked (or stroked-and-filled) rectangle, axis-aligned on the page.
    Rect { l: f64, b: f64, r: f64, t: f64 },
    /// Anything else painted — a fill, a curve: its bounding box, which can
    /// only ever be a mark inside a square.
    Blot { l: f64, b: f64, r: f64, t: f64 },
    /// An image placement (#999): the text parser hands these on as the
    /// page's bitmaps and drops them before [`find`] sees the inks.
    Bitmap { l: f64, b: f64, r: f64, t: f64 },
}

/// Smallest and largest checkbox side, points: below 5 pt a square is a
/// bullet or a table hairline's corner, above 24 pt a frame or a swatch.
pub(crate) const MIN_SIDE: f64 = 5.0;
pub(crate) const MAX_SIDE: f64 = 24.0;
/// Corner/edge matching slack, points (line caps, rounding).
const TOL: f64 = 0.75;

impl Ink {
    fn bbox(&self) -> (f64, f64, f64, f64) {
        match *self {
            Ink::Seg { x0, y0, x1, y1 } => (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)),
            Ink::Rect { l, b, r, t } | Ink::Blot { l, b, r, t } | Ink::Bitmap { l, b, r, t } => {
                (l, b, r, t)
            }
        }
    }

    /// Whether the text parser should keep this piece at all: only what can
    /// be a checkbox edge or fit inside one. Keeps a vector-heavy page (a
    /// chart, a map) from buffering every path it draws.
    pub(crate) fn worth_keeping(&self) -> bool {
        let (l, b, r, t) = self.bbox();
        let reach = MAX_SIDE + TOL;
        (r - l) <= reach && (t - b) <= reach
    }
}

/// A square's `(l, b, r, t)` and the ink indices that draw it.
type Square = ((f64, f64, f64, f64), Vec<usize>);

fn square_sides(w: f64, h: f64) -> bool {
    let (lo, hi) = (w.min(h), w.max(h));
    lo >= MIN_SIDE && hi <= MAX_SIDE && hi - lo <= 0.15 * hi
}

/// The checkboxes among a page's painted pieces, top-left page points
/// (`page_h` flips y). Duplicates (a square stroked twice) collapse.
pub(crate) fn find(inks: &[Ink], page_h: f32) -> Vec<CheckBox> {
    let mut squares: Vec<Square> = Vec::new();
    for (i, ink) in inks.iter().enumerate() {
        if let Ink::Rect { l, b, r, t } = *ink {
            if square_sides(r - l, t - b) {
                squares.push(((l, b, r, t), vec![i]));
            }
        }
    }
    // Four separate edges. Horizontal edges pair up by a shared x span, and
    // the two verticals must close the sides at both ends.
    let near = |a: f64, b: f64| (a - b).abs() <= TOL;
    let mut hs: Vec<(usize, f64, f64, f64)> = Vec::new(); // (ink, y, xl, xr)
    let mut vs: Vec<(usize, f64, f64, f64)> = Vec::new(); // (ink, x, yb, yt)
    for (i, ink) in inks.iter().enumerate() {
        if let Ink::Seg { x0, y0, x1, y1 } = *ink {
            if near(y0, y1) && (x1 - x0).abs() >= MIN_SIDE - TOL {
                hs.push((i, (y0 + y1) / 2.0, x0.min(x1), x0.max(x1)));
            } else if near(x0, x1) && (y1 - y0).abs() >= MIN_SIDE - TOL {
                vs.push((i, (x0 + x1) / 2.0, y0.min(y1), y0.max(y1)));
            }
        }
    }
    // A page drawn from thousands of short strokes (hatching, a plotted
    // curve) is not a checklist; the pairing below is quadratic.
    if hs.len() <= 4000 && vs.len() <= 4000 {
        let side = |lo: f64, hi: f64, a: f64, b: f64| near(lo, a) && near(hi, b);
        for (x, &(hi, ya, xl, xr)) in hs.iter().enumerate() {
            for &(hj, yb, xl2, xr2) in &hs[x + 1..] {
                if !(near(xl, xl2) && near(xr, xr2) && square_sides(xr - xl, (ya - yb).abs())) {
                    continue;
                }
                let (bot, top) = (ya.min(yb), ya.max(yb));
                let left = vs
                    .iter()
                    .find(|&&(_, vx, y0, y1)| near(vx, xl) && side(y0, y1, bot, top));
                let right = vs
                    .iter()
                    .find(|&&(_, vx, y0, y1)| near(vx, xr) && side(y0, y1, bot, top));
                if let (Some(&(lv, ..)), Some(&(rv, ..))) = (left, right) {
                    squares.push(((xl, bot, xr, top), vec![hi, hj, lv, rv]));
                }
            }
        }
    }
    let mut out: Vec<CheckBox> = Vec::new();
    for (k, &((l, b, r, t), ref own)) in squares.iter().enumerate() {
        let same =
            |o: &Square| near(o.0 .0, l) && near(o.0 .1, b) && near(o.0 .2, r) && near(o.0 .3, t);
        if squares[..k].iter().any(same) {
            continue;
        }
        // The square's own edges (and any re-stroke of them) are not a mark;
        // a mark is other ink lying within the square, centred well inside.
        let edges: Vec<usize> = squares
            .iter()
            .filter(|o| same(o))
            .flat_map(|o| o.1.iter().copied())
            .chain(own.iter().copied())
            .collect();
        // A square under a coloured fill of its own size is a chart legend's
        // swatch (matplotlib strokes and fills each patch), not a checkbox.
        let swatch = inks.iter().any(|ink| {
            matches!(ink, Ink::Blot { .. }) && {
                let (il, ib, ir, it) = ink.bbox();
                let fit = 1.5;
                (il - l).abs() <= fit
                    && (ir - r).abs() <= fit
                    && (ib - b).abs() <= fit
                    && (it - t).abs() <= fit
            }
        });
        if swatch {
            continue;
        }
        let inset = 0.2 * (r - l);
        let checked = inks.iter().enumerate().any(|(i, ink)| {
            if edges.contains(&i) {
                return false;
            }
            let (il, ib, ir, it) = ink.bbox();
            let (cx, cy) = ((il + ir) / 2.0, (ib + it) / 2.0);
            il >= l - TOL
                && ir <= r + TOL
                && ib >= b - TOL
                && it <= t + TOL
                && cx > l + inset
                && cx < r - inset
                && cy > b + inset
                && cy < t - inset
        });
        out.push(CheckBox {
            l: l as f32,
            t: page_h - t as f32,
            r: r as f32,
            b: page_h - b as f32,
            checked,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(x0: f64, y0: f64, x1: f64, y1: f64) -> Ink {
        Ink::Seg { x0, y0, x1, y1 }
    }

    /// ReportLab's checkbox: four `m … l S` edges of a 12.96 pt square.
    fn four_edges(x: f64, y: f64) -> Vec<Ink> {
        let s = 12.96;
        vec![
            seg(x, y + s, x + s, y + s),
            seg(x, y, x + s, y),
            seg(x, y, x, y + s),
            seg(x + s, y, x + s, y + s),
        ]
    }

    #[test]
    fn four_stroked_edges_make_one_square() {
        let boxes = find(&four_edges(119.52, 449.04), 792.0);
        assert_eq!(boxes.len(), 1);
        let b = boxes[0];
        assert!((b.l - 119.52).abs() < 0.01 && (b.r - 132.48).abs() < 0.01);
        assert!((b.t - (792.0 - 462.0)).abs() < 0.01, "{b:?}");
        assert!(!b.checked);
    }

    #[test]
    fn a_stroked_rect_is_a_square_and_a_tick_inside_checks_it() {
        let mut inks = vec![Ink::Rect {
            l: 100.0,
            b: 100.0,
            r: 110.0,
            t: 110.0,
        }];
        assert!(!find(&inks, 800.0)[0].checked);
        inks.push(seg(102.0, 105.0, 104.5, 102.0));
        inks.push(seg(104.5, 102.0, 108.5, 108.5));
        assert!(find(&inks, 800.0)[0].checked);
    }

    #[test]
    fn frames_bullets_and_open_shapes_are_not_checkboxes() {
        let rect = |l, b, r, t| Ink::Rect { l, b, r, t };
        // Too big (a frame), too small (a bullet), not square (a bar).
        assert!(find(&[rect(0.0, 0.0, 40.0, 40.0)], 800.0).is_empty());
        assert!(find(&[rect(0.0, 0.0, 3.0, 3.0)], 800.0).is_empty());
        assert!(find(&[rect(0.0, 0.0, 20.0, 8.0)], 800.0).is_empty());
        // Three edges only.
        let mut open = four_edges(50.0, 50.0);
        open.pop();
        assert!(find(&open, 800.0).is_empty());
    }

    /// A legend swatch — the square filled with colour by a separate fill of
    /// its size — is not a checkbox.
    #[test]
    fn a_colour_filled_square_is_a_swatch() {
        let mut inks = four_edges(10.0, 10.0);
        inks.push(Ink::Blot {
            l: 10.0,
            b: 10.0,
            r: 22.96,
            t: 22.96,
        });
        assert!(find(&inks, 800.0).is_empty());
    }

    #[test]
    fn a_square_stroked_twice_is_one_unchecked_box() {
        let mut inks = four_edges(10.0, 10.0);
        inks.extend(four_edges(10.0, 10.0));
        let boxes = find(&inks, 800.0);
        assert_eq!(boxes.len(), 1);
        assert!(!boxes[0].checked);
    }
}
