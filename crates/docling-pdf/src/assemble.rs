//! Layout-driven assembly: map detected [`Region`]s + text cells to a
//! [`DoclingDocument`], mirroring docling's page-assembly + reading-order.
//!
//! Overlapping detections are resolved greedily by score, each text cell is
//! assigned to its best-containing region, regions are ordered in reading order
//! (two-column aware), and each becomes a typed node by its layout label.

use docling_core::{CaptionParent, ListItemDclx, Node, PictureClass, PictureImage, Table};
#[cfg(any(feature = "ml", feature = "ocr-prep"))]
use image::RgbImage;

use crate::layout::Region;
use crate::pdfium_backend::{PdfPage, TextCell};

fn area(l: f32, t: f32, r: f32, b: f32) -> f32 {
    ((r - l).max(0.0)) * ((b - t).max(0.0))
}

/// Intersection area of two boxes.
fn inter(a: &Region, l: f32, t: f32, r: f32, b: f32) -> f32 {
    let il = a.l.max(l);
    let it = a.t.max(t);
    let ir = a.r.min(r);
    let ib = a.b.min(b);
    area(il, it, ir, ib)
}

/// Wrapper (structured-region) labels, ported from docling
/// `LayoutPostprocessor.WRAPPER_TYPES`: a region that *contains* other regions
/// and renders as a structured block (a table / table-of-contents index), not as
/// its own flat text.
fn is_wrapper(label: &str) -> bool {
    matches!(
        label,
        "table" | "document_index" | "form" | "key_value_region"
    )
}

/// Labels docling's table-structure (TableFormer) model runs on and that render
/// as a Markdown table: a plain `table` and a `document_index` (a table of
/// contents), which docling assembles as a `TableItem` too.
pub fn is_table_like(label: &str) -> bool {
    matches!(label, "table" | "document_index")
}

/// Greedily keep regions by descending score, dropping a region that is mostly
/// covered by an already-kept one (RT-DETR emits overlapping duplicates).
fn greedy(mut regions: Vec<Region>) -> Vec<Region> {
    regions.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Region> = Vec::new();
    for r in regions {
        let ra = area(r.l, r.t, r.r, r.b).max(1.0);
        let covered = kept.iter().any(|k| {
            let i = inter(&r, k.l, k.t, k.r, k.b);
            let ka = area(k.l, k.t, k.r, k.b).max(1.0);
            // drop if most of r is inside k, or they strongly mutually overlap
            i / ra > 0.7 || i / (ra + ka - i) > 0.5
        });
        if !covered {
            kept.push(r);
        }
    }
    kept
}

/// Resolve overlapping RT-DETR detections, ported from the bucket structure of
/// docling's `LayoutPostprocessor`: regular, picture and wrapper clusters live in
/// **separate** spatial indexes and are de-overlapped independently, so a
/// high-score picture never suppresses a lower-score table or table-of-contents
/// index (the redp5110 TOC that was otherwise replaced by a picture box). A
/// cross-type pass first drops a picture that nearly coincides with a table
/// (`_handle_cross_type_overlaps`), keeping the structured table.
/// docling's `_remove_overlapping_clusters("picture")`: same-label picture
/// detections whose boxes heavily overlap (IoU > 0.8, or either box > 80 %
/// contained in the other) form one group, and a single survivor is kept per
/// group. Survivor selection ports `_should_prefer_cluster` /
/// `_select_best_cluster_from_group` with the picture params
/// (`area_threshold` 2.0, `conf_threshold` 0.3): a candidate is rejected only
/// when a rival is both comparable in size (candidate ≤ 2× its area) and
/// clearly more confident (> 0.3); among the survivors the *larger* box wins
/// unless it is > 0.3 less confident. Net effect on the corpus: a figure the
/// detector proposes both whole and as its sub-panels (2206's four-thumbnail
/// Figure 1) collapses to the whole-figure box, exactly like docling.
pub(crate) fn dedup_pictures(regions: &mut Vec<Region>) {
    remove_overlapping_specials(regions, |l| l == "picture", 2.0, 0.3);
}

/// docling's `_remove_overlapping_clusters` for one special bucket (the
/// regions whose label satisfies `in_bucket`), with that bucket's
/// `OVERLAP_PARAMS`: picture (2.0, 0.3) — see [`dedup_pictures`] — or wrapper
/// (2.0, 0.2) for the table bucket in [`resolve`]. Regions outside the bucket
/// are untouched; a bucket member overlapping nothing is always kept.
fn remove_overlapping_specials(
    regions: &mut Vec<Region>,
    in_bucket: impl Fn(&str) -> bool,
    area_threshold: f32,
    conf_threshold: f32,
) {
    let idx: Vec<usize> = (0..regions.len())
        .filter(|&i| in_bucket(regions[i].label))
        .collect();
    if idx.len() < 2 {
        return;
    }
    // Union-find over the bucket.
    let mut parent: Vec<usize> = (0..idx.len()).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut root = i;
        while parent[root] != root {
            root = parent[root];
        }
        let mut cur = i;
        while parent[cur] != root {
            let next = parent[cur];
            parent[cur] = root;
            cur = next;
        }
        root
    }
    let boxed = |r: &Region| (r.l, r.t, r.r, r.b);
    for a in 0..idx.len() {
        for b in (a + 1)..idx.len() {
            let (ra, rb) = (&regions[idx[a]], &regions[idx[b]]);
            let (al, at, ar, ab_) = boxed(ra);
            let (bl, bt, br, bb) = boxed(rb);
            let ix = (ar.min(br) - al.max(bl)).max(0.0);
            let iy = (ab_.min(bb) - at.max(bt)).max(0.0);
            let inter = ix * iy;
            let aa = area(al, at, ar, ab_).max(f32::EPSILON);
            let ba = area(bl, bt, br, bb).max(f32::EPSILON);
            let iou = inter / (aa + ba - inter).max(f32::EPSILON);
            if iou > 0.8 || inter / aa > 0.8 || inter / ba > 0.8 {
                let (pa, pb) = (find(&mut parent, a), find(&mut parent, b));
                if pa != pb {
                    parent[pa] = pb;
                }
            }
        }
    }
    // Per group, run docling's pairwise preference + larger-wins selection.
    let mut groups: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
    for i in 0..idx.len() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    let mut drop = vec![false; regions.len()];
    for group in groups.values() {
        if group.len() < 2 {
            continue;
        }
        let area_of = |i: usize| {
            let r = &regions[idx[i]];
            area(r.l, r.t, r.r, r.b).max(f32::EPSILON)
        };
        let mut best: Option<usize> = None;
        for &cand in group {
            let passes = group.iter().all(|&other| {
                if other == cand {
                    return true;
                }
                let area_ratio = area_of(cand) / area_of(other);
                let conf_diff = regions[idx[other]].score - regions[idx[cand]].score;
                !(area_ratio <= area_threshold && conf_diff > conf_threshold)
            });
            if passes {
                best = Some(match best {
                    None => cand,
                    Some(cur) => {
                        if area_of(cand) > area_of(cur)
                            && regions[idx[cur]].score - regions[idx[cand]].score <= conf_threshold
                        {
                            cand
                        } else {
                            cur
                        }
                    }
                });
            }
        }
        // Every candidate rejected can't happen with docling's rule (rejection
        // needs a strictly better rival); guard with highest score anyway.
        let keep = best.unwrap_or_else(|| {
            *group
                .iter()
                .max_by(|&&a, &&b| regions[idx[a]].score.total_cmp(&regions[idx[b]].score))
                .expect("non-empty group")
        });
        for &i in group {
            if i != keep {
                drop[idx[i]] = true;
            }
        }
    }
    let mut keep_iter = drop.into_iter();
    regions.retain(|_| !keep_iter.next().expect("aligned"));
}

/// docling's `_remove_overlapping_clusters("regular")` for what [`greedy`]
/// leaves standing, run on OCR'd pages before the region-scoped OCR (found
/// with #471's synthetic scan: every line of the page came out twice).
///
/// `greedy` keeps regions by descending score and drops a candidate mostly
/// inside an already-kept one — so a *lower*-score block that contains
/// several higher-score line boxes (RT-DETR's favourite reading of a sparse
/// scanned page: every line, plus the paragraph) survives alongside them.
/// On a digital page that is harmless: [`fit_regions_to_cells`] hands each
/// text cell to one owner and drops the regions left empty. On a scanned
/// page the cells don't exist yet — they come from OCR of *each* region's
/// crop — so the block and its lines were each recognized, and the block,
/// which then owned both cell sets, read every line twice.
///
/// Upstream never has this problem because its OCR runs over the bitmap
/// before layout postprocessing and each cell is assigned once; the
/// postprocessor's regular pass then groups clusters that overlap (IoU > 0.8,
/// or either > 80 % contained in the other) with a union-find, keeps one
/// survivor per group via `_should_prefer_cluster` /
/// `_select_best_cluster_from_group` (`area_threshold` 1.3, `conf_threshold`
/// 0.05; a LIST_ITEM beats a same-sized TEXT, a CODE box beats what it
/// contains) and merges the losers' cells into it, whose box is then fitted
/// to those cells. The equivalent for region-scoped OCR: one survivor per
/// group, keeping its label and score, with the group's **union** box so the
/// single crop still covers every merged line. Pictures and wrappers are not
/// regulars and are left alone ([`dedup_pictures`], [`resolve`]).
pub(crate) fn merge_overlapping_regulars(regions: &mut Vec<Region>) {
    let idx: Vec<usize> = (0..regions.len())
        .filter(|&i| regions[i].label != "picture" && !is_wrapper(regions[i].label))
        .collect();
    if idx.len() < 2 {
        return;
    }
    let mut parent: Vec<usize> = (0..idx.len()).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut root = i;
        while parent[root] != root {
            root = parent[root];
        }
        let mut cur = i;
        while parent[cur] != root {
            let next = parent[cur];
            parent[cur] = root;
            cur = next;
        }
        root
    }
    for a in 0..idx.len() {
        for b in (a + 1)..idx.len() {
            let (ra, rb) = (&regions[idx[a]], &regions[idx[b]]);
            let i = inter(ra, rb.l, rb.t, rb.r, rb.b);
            let aa = area(ra.l, ra.t, ra.r, ra.b).max(f32::EPSILON);
            let ba = area(rb.l, rb.t, rb.r, rb.b).max(f32::EPSILON);
            if i / (aa + ba - i).max(f32::EPSILON) > 0.8 || i / aa > 0.8 || i / ba > 0.8 {
                let (pa, pb) = (find(&mut parent, a), find(&mut parent, b));
                if pa != pb {
                    parent[pa] = pb;
                }
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..idx.len() {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }
    const AREA_THRESHOLD: f32 = 1.3;
    const CONF_THRESHOLD: f32 = 0.05;
    let area_of = |i: usize| {
        let r = &regions[idx[i]];
        area(r.l, r.t, r.r, r.b).max(f32::EPSILON)
    };
    // `_should_prefer_cluster(candidate, other)` with the regular params.
    let prefer = |cand: usize, other: usize| -> bool {
        let (c, o) = (&regions[idx[cand]], &regions[idx[other]]);
        let area_ratio = area_of(cand) / area_of(other);
        if c.label == "list_item" && o.label == "text" && (1.0 - area_ratio).abs() < 0.2 {
            return true;
        }
        if c.label == "code" && inter(o, c.l, c.t, c.r, c.b) / area_of(other) > 0.8 {
            return true;
        }
        !(area_ratio <= AREA_THRESHOLD && o.score - c.score > CONF_THRESHOLD)
    };
    let mut drop = vec![false; regions.len()];
    let mut unions: Vec<(usize, (f32, f32, f32, f32))> = Vec::new();
    for group in groups.values() {
        if group.len() < 2 {
            continue;
        }
        let mut best: Option<usize> = None;
        for &cand in group {
            if group
                .iter()
                .all(|&other| other == cand || prefer(cand, other))
            {
                best = Some(match best {
                    None => cand,
                    Some(cur)
                        if area_of(cand) > area_of(cur)
                            && regions[idx[cur]].score - regions[idx[cand]].score
                                <= CONF_THRESHOLD =>
                    {
                        cand
                    }
                    Some(cur) => cur,
                });
            }
        }
        // docling falls back to the group's first cluster; the highest score
        // is the deterministic equivalent for a set with no insertion order.
        let keep = best.unwrap_or_else(|| {
            *group
                .iter()
                .max_by(|&&a, &&b| regions[idx[a]].score.total_cmp(&regions[idx[b]].score))
                .expect("non-empty group")
        });
        let mut u = (
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        );
        for &i in group {
            let r = &regions[idx[i]];
            u = (u.0.min(r.l), u.1.min(r.t), u.2.max(r.r), u.3.max(r.b));
            if i != keep {
                drop[idx[i]] = true;
            }
        }
        unions.push((idx[keep], u));
    }
    for (i, (l, t, r, b)) in unions {
        let k = &mut regions[i];
        (k.l, k.t, k.r, k.b) = (l, t, r, b);
    }
    let mut keep_iter = drop.into_iter();
    regions.retain(|_| !keep_iter.next().expect("aligned"));
}

/// `intersection_over_union` of two regions.
fn iou(a: &Region, b: &Region) -> f32 {
    let i = inter(a, b.l, b.t, b.r, b.b);
    let u = area(a.l, a.t, a.r, a.b) + area(b.l, b.t, b.r, b.b) - i;
    if u > 0.0 {
        i / u
    } else {
        0.0
    }
}

/// docling's `_resolve_coincident_pairs` (#4059, 2.122): for every (loser,
/// winner) pair at a near-identical box (IoU > 0.8) whose confidences are
/// within 0.1 (`loser.score - winner.score < 0.1`), the loser label is dropped
/// so the label with the richer downstream semantic survives. Nothing else —
/// containment, area — is considered; a clearly more confident loser stays.
fn coincident_losers(regions: &[Region], losers: &[usize], winners: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    for &li in losers {
        for &wi in winners {
            if iou(&regions[li], &regions[wi]) > 0.8 && regions[li].score - regions[wi].score < 0.1
            {
                out.push(li);
                break;
            }
        }
    }
    out
}

/// docling's `_handle_cross_type_overlaps` (2.122/2.123 shape): the layout
/// model can emit one grounded region under several labels, and the picture /
/// table / container buckets are de-overlapped independently, so such a region
/// survives twice. Elect a winner for the near-identical pairs:
///
/// | pair                                  | loser     | winner              |
/// |---------------------------------------|-----------|---------------------|
/// | TABLE vs DOCUMENT_INDEX               | table     | document_index      |
/// | PICTURE vs TABLE / DOCUMENT_INDEX     | picture   | the table-like      |
/// | FORM / KEY_VALUE_REGION vs *surviving* TABLE / DOCUMENT_INDEX / PICTURE | container | structured element |
///
/// IoU (not containment) so a genuine small figure inside a large table region
/// is not removed; the confidence tolerance keeps a clearly more confident
/// loser (an earlier port dropped every coincident picture regardless).
fn handle_cross_type_overlaps(regions: Vec<Region>) -> Vec<Region> {
    let by = |pred: &dyn Fn(&str) -> bool| -> Vec<usize> {
        (0..regions.len())
            .filter(|&i| pred(regions[i].label))
            .collect()
    };
    let tables = by(&|l| l == "table");
    let doc_indices = by(&|l| l == "document_index");
    let pictures = by(&|l| l == "picture");
    let containers = by(&|l| matches!(l, "form" | "key_value_region"));
    let mut drop = vec![false; regions.len()];
    for i in coincident_losers(&regions, &tables, &doc_indices) {
        drop[i] = true;
    }
    let table_like: Vec<usize> = tables.iter().chain(&doc_indices).copied().collect();
    for i in coincident_losers(&regions, &pictures, &table_like) {
        drop[i] = true;
    }
    let structured: Vec<usize> = table_like
        .iter()
        .chain(&pictures)
        .copied()
        .filter(|&i| !drop[i])
        .collect();
    for i in coincident_losers(&regions, &containers, &structured) {
        drop[i] = true;
    }
    let mut drop = drop.into_iter();
    let mut regions = regions;
    regions.retain(|_| !drop.next().expect("aligned"));
    regions
}

pub fn resolve(regions: Vec<Region>) -> Vec<Region> {
    let regions = handle_cross_type_overlaps(regions);
    // De-overlap each bucket on its own.
    let pictures = greedy(
        regions
            .iter()
            .filter(|r| r.label == "picture")
            .cloned()
            .collect(),
    );
    // Tables and containers are separate buckets since docling 2.123
    // (`TABLE_TYPES` vs `CONTAINER_TYPES`, docling#4064): a form drawn around a
    // table no longer competes with it for survival — the table nests inside
    // the container instead (`order_with_containers`).
    let mut tables = greedy(
        regions
            .iter()
            .filter(|r| is_table_like(r.label))
            .cloned()
            .collect(),
    );
    // `greedy` only drops a table mostly inside a *more* confident one, so a
    // low-score whole-page table proposed over the column tables it contains
    // (a two-column glossary page: 0.53 over 0.71/0.67/0.66) survived next to them and every
    // cell was emitted twice. docling's `_remove_overlapping_clusters(tables,
    // "wrapper")` groups tables whose boxes overlap (IoU > 0.8, or either one
    // > 80 % inside the other) and keeps one per group: run it on what
    // `greedy` leaves, like `merge_overlapping_regulars` does for regulars.
    remove_overlapping_specials(&mut tables, |_| true, 2.0, 0.2);
    let containers = greedy(
        regions
            .iter()
            .filter(|r| matches!(r.label, "form" | "key_value_region"))
            .cloned()
            .collect(),
    );
    let mut kept = greedy(
        regions
            .iter()
            .filter(|r| r.label != "picture" && !is_wrapper(r.label))
            .cloned()
            .collect(),
    );
    dedup_nested_code(&mut kept);
    kept.extend(pictures);
    kept.extend(tables);
    kept.extend(containers);
    kept
}

/// Drop a regular region that is >80% contained in a surviving special region we
/// render **as a single unit** — a table/table-of-contents index — ported from
/// docling's "Remove regular clusters that are included in wrappers" step: the
/// special absorbs it as a child (a table cell), so it must not also be emitted
/// as its own paragraph/list-item. This stops the survey list-items from
/// appearing both inside the detected table and again as bullets
/// (`table_mislabeled_as_picture`).
///
/// `picture` regions are **not** in the swallow set: docling keeps a picture's
/// contained clusters as the `PictureItem`'s *children* in the document JSON
/// (`_set_cluster_children`, `ReadingOrderModel._add_child_elements`), while
/// its `MarkdownPictureSerializer` prints only the caption and the image — the
/// children never reach the Markdown (verified against the corpus groundtruth:
/// `amt_handbook`'s in-figure callout labels are absent). So the regulars
/// inside a picture stay in the region list, claim their cells and are fitted
/// like any regular, and [`assemble_page`] lifts them out of the page's
/// reading order into a [`Node::PictureChildren`] after their picture (see
/// [`picture_parents`]). A line only partially under a figure box (straddling
/// its border, ≤80 % contained) is no picture's child and is emitted, as
/// docling emits it (#165).
///
/// `form` / `key_value_region` wrappers are deliberately **excluded**: this
/// pipeline does not render them as a structured block (they are skipped), so
/// their textual content comes precisely from the contained regular regions —
/// A `page_footer` that is really the body of the last heading on the page
/// becomes `text`.
///
/// The layout model labels the bottom margin by position as much as by
/// content: a one-line paragraph that happens to sit where a running footer
/// would — a CV's `Languages` line under its `## Languages` heading, the last
/// entry of a section that runs to the page edge — comes back as
/// `page_footer` (0.88 against 0.49 for `text` on the reporting file, with
/// the fp32 model and either renderer alike), and Markdown drops furniture,
/// so the document loses real content while the JSON keeps it under the
/// wrong label. docling fails the same way. Deliberate deviation, gated so a
/// real footer keeps its label: the nearest `section_header` above the footer
/// must overlap it horizontally, end within 2.5 footer heights of its top,
/// and be the **last body element** of its column — no other non-furniture
/// region starts at or below the heading's bottom edge over the same span —
/// and the footer must be a line of text, not a page number: at least 40 %
/// of the page width. A running footer never follows a heading that has no
/// body of its own, so the combination is the misread heading body.
pub fn reclaim_heading_body_footers(regions: &mut [Region], page_w: f32) {
    let n = regions.len();
    let overlap_x = |a: &Region, b: &Region| a.r.min(b.r) - a.l.max(b.l) > 0.0;
    for fi in 0..n {
        let f = regions[fi].clone();
        if f.label != "page_footer" || f.r - f.l < 0.4 * page_w {
            continue;
        }
        let fh = (f.b - f.t).max(1.0);
        // The nearest heading above the footer, over the footer's span.
        let heading = (0..n)
            .filter(|&j| {
                let h = &regions[j];
                j != fi && h.label == "section_header" && h.b <= f.t + 0.5 * fh && overlap_x(h, &f)
            })
            .min_by(|&a, &b| regions[b].b.total_cmp(&regions[a].b));
        let Some(hi) = heading else {
            continue;
        };
        let h = regions[hi].clone();
        if f.t - h.b > 2.5 * fh {
            continue;
        }
        // The heading must have no body of its own: nothing but the footer
        // starts at or below its bottom edge over the heading's or footer's
        // span (a heading whose paragraph follows is not this case, and a
        // heading with the footer far below it was filtered above).
        let has_body = (0..n).any(|j| {
            let r = &regions[j];
            j != fi
                && j != hi
                && !matches!(r.label, "page_footer" | "page_header")
                && r.t >= h.b - 0.5 * fh
                && (overlap_x(r, &h) || overlap_x(r, &f))
        });
        if has_body {
            continue;
        }
        regions[fi].label = "text";
    }
}

/// dropping those would erase the page (e.g. `right_to_left_03`'s form-heavy
/// pages). Runs *after* [`drop_false_pictures`] so a phantom picture can't
/// swallow real text on its way out.
pub fn drop_contained_regulars(regions: &mut Vec<Region>) {
    let specials: Vec<(f32, f32, f32, f32)> = regions
        .iter()
        .filter(|r| is_table_like(r.label))
        .map(|r| (r.l, r.t, r.r, r.b))
        .collect();
    if specials.is_empty() {
        return;
    }
    regions.retain(|r| {
        if r.label == "picture" || is_wrapper(r.label) {
            return true;
        }
        let ra = area(r.l, r.t, r.r, r.b).max(1.0);
        !specials
            .iter()
            .any(|&(l, t, rr, b)| inter(r, l, t, rr, b) / ra > 0.8)
    });
}

/// docling's `_set_cluster_children` for pictures: a regular region (one that
/// claims cells) whose box is > 80 % inside a picture's box
/// (`intersection_over_self`) is that picture's child — not a page element
/// (it leaves the reading order and the layout score), but an item under the
/// `PictureItem` in the JSON. Returns, per region, the index of its parent
/// picture: the smallest containing one when pictures nest (upstream would
/// attach it to each).
pub fn picture_parents(regions: &[Region]) -> Vec<Option<usize>> {
    regions
        .iter()
        .map(|r| {
            if !claims_cells(r) {
                return None;
            }
            let ra = area(r.l, r.t, r.r, r.b).max(1.0);
            regions
                .iter()
                .enumerate()
                .filter(|(_, p)| p.label == "picture" && inter(r, p.l, p.t, p.r, p.b) / ra > 0.8)
                .min_by(|(_, a), (_, b)| {
                    area(a.l, a.t, a.r, a.b).total_cmp(&area(b.l, b.t, b.r, b.b))
                })
                .map(|(i, _)| i)
        })
        .collect()
}

/// True for a bare, single-token source-code language label (`XML`, `C#`, `JSON`,
/// `bash`, …) — the little header the docs render above a code block. Matched
/// case-insensitively; anything with whitespace or longer than a token is out.
fn is_code_language(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() || t.chars().any(char::is_whitespace) || t.chars().count() > 12 {
        return false;
    }
    const LANGS: &[&str] = &[
        "xml",
        "html",
        "xhtml",
        "json",
        "jsonc",
        "yaml",
        "yml",
        "toml",
        "ini",
        "c#",
        "csharp",
        "f#",
        "fsharp",
        "vb",
        "c",
        "c++",
        "cpp",
        "java",
        "kotlin",
        "scala",
        "go",
        "golang",
        "rust",
        "swift",
        "javascript",
        "js",
        "typescript",
        "ts",
        "jsx",
        "tsx",
        "python",
        "py",
        "ruby",
        "rb",
        "php",
        "perl",
        "lua",
        "r",
        "dart",
        "bash",
        "sh",
        "shell",
        "powershell",
        "zsh",
        "batch",
        "cmd",
        "sql",
        "tsql",
        "plsql",
        "graphql",
        "dockerfile",
        "makefile",
        "css",
        "scss",
        "sass",
        "less",
        "markdown",
        "md",
        "tex",
        "latex",
        "diff",
        "proto",
        "razor",
        "cshtml",
        "xaml",
        "aspx",
        "http",
    ];
    let lower = t.to_ascii_lowercase();
    LANGS.contains(&lower.as_str())
}

/// Mark the region indices that are a code block's **language label** — a bare
/// `XML`/`C#`/… token sitting directly above a `code` region — so they are consumed
/// rather than emitted as their own stray paragraph/heading. The label may also be
/// captured inside a wider code box (rendered as the fence's first line); dropping
/// the standalone copy just removes the duplicate.
fn code_language_labels(regions: &[Region], cells: &[TextCell]) -> Vec<bool> {
    let mut drop = vec![false; regions.len()];
    for (i, r) in regions.iter().enumerate() {
        if matches!(r.label, "code" | "picture" | "table") {
            continue;
        }
        if !is_code_language(&region_text(r, cells)) {
            continue;
        }
        // The label sits just above the code (a blank line's gap) or is swallowed
        // into the top of a wider code box; either way it is that block's label.
        // The window is generous because the label's own font is small, so a
        // one-line gap is several times its height.
        let line_h = (r.b - r.t).abs().max(1.0);
        let window = (line_h * 4.0).max(28.0);
        let labels_code = regions.iter().enumerate().any(|(j, c)| {
            if j == i || c.label != "code" {
                return false;
            }
            let gap = c.t - r.b; // >0 when the code is below the label
            let h_overlap = (r.r.min(c.r) - r.l.max(c.l)).max(0.0);
            gap > -line_h * 3.0 && gap < window && h_overlap > 0.0
        });
        if labels_code {
            drop[i] = true;
        }
    }
    drop
}

/// Collapse `code` regions where one is nested inside another, keeping the larger.
///
/// RT-DETR sometimes emits a tight code box *and* a wider near-duplicate that also
/// captures the block's language label (`XML`, `C#`, …). When the tight box scores
/// higher it is kept first, and the wider container — not "mostly inside" the tight
/// box — survives [`resolve`]'s greedy pass, so the block is emitted twice. Keeping
/// the **larger** box (rather than dropping it) collapses the pair without leaking
/// the container's extra cells back out as orphan text, since the larger box still
/// covers every cell. Restricted to `code` so genuinely distinct nested regions of
/// other kinds are untouched.
fn dedup_nested_code(kept: &mut Vec<Region>) {
    let mut drop = vec![false; kept.len()];
    for i in 0..kept.len() {
        if kept[i].label != "code" {
            continue;
        }
        let ai = area(kept[i].l, kept[i].t, kept[i].r, kept[i].b).max(1.0);
        for j in 0..kept.len() {
            if i == j || drop[j] || kept[j].label != "code" {
                continue;
            }
            let aj = area(kept[j].l, kept[j].t, kept[j].r, kept[j].b).max(1.0);
            // Drop i when it is mostly inside a strictly larger code box j.
            let overlap = inter(&kept[i], kept[j].l, kept[j].t, kept[j].r, kept[j].b);
            if aj > ai && overlap / ai > 0.7 {
                drop[i] = true;
                break;
            }
        }
    }
    let mut keep = drop.iter();
    kept.retain(|_| !*keep.next().unwrap());
}

/// Fraction of the page's non-empty text cells that some detected region
/// claims (>0.2 intersection-over-self, docling's assignment rule). 1.0 for a
/// page without text cells.
///
/// The int8-layout guard keys off this: a dense digital page whose detections
/// cover almost none of its text is the signature of quantized confidences
/// flipping under the 0.5 label thresholds on this CPU's kernels — not of a
/// genuinely empty layout — and is worth re-running on the fp32 graph.
pub fn layout_cell_coverage(regions: &[Region], cells: &[TextCell]) -> f32 {
    let mut total = 0usize;
    let mut covered = 0usize;
    for c in cells {
        if c.text.trim().is_empty() {
            continue;
        }
        total += 1;
        let ca = area(c.l, c.t, c.r, c.b).max(1.0);
        if regions
            .iter()
            .any(|r| inter(r, c.l, c.t, c.r, c.b) / ca > 0.2)
        {
            covered += 1;
        }
    }
    if total == 0 {
        1.0
    } else {
        covered as f32 / total as f32
    }
}

/// Whether a run reads as a note call: a number or reference mark
/// (`1`, `12`, `*`, `†`, `¹`), or a short bracketed note (`(Anm. 1)`, `[3]`).
/// Geometry alone is not enough: a figure's labels come in every size, and
/// moving a smaller word behind the larger one beside it scrambled the
/// figure text (`The Law will will never perfect be`, the OTSL rows run
/// together) — the PDF snapshots of three figures.
fn looks_like_note_call(text: &str) -> bool {
    let t = text.trim();
    let n = t.chars().count();
    if n == 0 || n > 16 {
        return false;
    }
    let mark = |c: char| c.is_ascii_digit() || "*†‡§¶#,⁰¹²³⁴⁵⁶⁷⁸⁹".contains(c);
    // `1,2` is two calls; a lone comma is punctuation.
    (t.chars().all(mark) && t.chars().any(|c| c != ','))
        || matches!(
            (t.chars().next(), t.chars().last()),
            (Some('('), Some(')')) | (Some('['), Some(']'))
        )
}

/// Does `note` — a small run raised above its line (a footnote call, `(Anm. 1)`)
/// — sit at the right end of `host`'s line?
fn is_note_of(note: &TextCell, host: &TextCell) -> bool {
    if !looks_like_note_call(&note.text) {
        return false;
    }
    let (nt, nb) = (note.t.min(note.b), note.t.max(note.b));
    let (ht, hb) = (host.t.min(host.b), host.t.max(host.b));
    let (nh, hh) = ((nb - nt).max(1.0), (hb - ht).max(1.0));
    nh <= 0.8 * hh
        && nb.min(hb) - nt.max(ht) >= 0.5 * nh
        && note.l >= host.r - 0.5 * hh
        && note.l <= host.r + hh
}

/// Reading order of the orphan cells `sorted` by (top, left), with a raised
/// note moved behind its host line (#923). The note's top edge is above the
/// host's, so the plain sort emitted `(Anm. 1)` ahead of the line it ends
/// (`…besteht.` is the end of the line starting `und die Belegschaft`). The
/// flags mark the cells that were moved, for the same-line merge below.
fn place_raised_notes(sorted: Vec<&TextCell>) -> (Vec<&TextCell>, Vec<bool>) {
    let n = sorted.len();
    let mut out: Vec<&TextCell> = Vec::with_capacity(n);
    let mut flags: Vec<bool> = Vec::with_capacity(n);
    let mut held: Vec<&TextCell> = Vec::new();
    for (i, &c) in sorted.iter().enumerate() {
        // Release a held note the scan has passed without finding its host.
        let mut k = 0;
        while k < held.len() {
            if c.t >= held[k].t.max(held[k].b) {
                out.push(held.remove(k));
                flags.push(false);
            } else {
                k += 1;
            }
        }
        // A cell with a host line ahead of it (starting within its own band)
        // waits for that host.
        let waits = sorted[i + 1..]
            .iter()
            .take_while(|h| h.t < c.t.max(c.b))
            .any(|h| is_note_of(c, h));
        if waits {
            held.push(c);
            continue;
        }
        out.push(c);
        flags.push(false);
        let mut k = 0;
        while k < held.len() {
            if is_note_of(held[k], c) {
                out.push(held.remove(k));
                flags.push(true);
            } else {
                k += 1;
            }
        }
    }
    for c in held {
        out.push(c);
        flags.push(false);
    }
    (out, flags)
}

/// Append `text` regions for cells the layout left uncovered ("orphan cells"),
/// the way docling's `LayoutPostprocessor` does (`create_orphan_clusters`): any
/// non-empty cell that no kept region covers (>50% of the cell's area) becomes a
/// text region of its own, so text the detector missed (a stray `.`, a small
/// label) is still emitted instead of silently dropped. Adjacent orphan cells on a
/// line are merged so a missed paragraph doesn't shatter into one block per line.
pub fn add_orphan_regions(regions: &mut Vec<Region>, cells: &[TextCell]) {
    // docling assigns each cell to its single best-overlapping cluster at
    // intersection-over-self > 0.2 and serializes exactly the assigned cells —
    // and since [`region_texts_exclusive`] now emits under that very rule, the
    // claim test here matches it: any cell over 0.2 will actually render in
    // its best region, everything else becomes an orphan. Completeness by
    // construction, with no (0.2, 0.5] hole (the old > 0.5 serializer needed
    // the claim test raised to > 0.5 to keep right_to_left_03's `20300` from
    // vanishing; the exclusive port closes that structurally).
    //
    // Only *regular* clusters claim cells: docling's `_find_unassigned_cells`
    // walks `regular_clusters` alone, so a cell under a `picture` or a wrapper
    // (`table`/`document_index`/`form`/`key_value_region`) that no regular
    // cluster covers still becomes an orphan text cluster (#165). The orphans
    // that end up *fully* inside the special are re-dropped by
    // [`drop_contained_regulars`] (docling's Markdown drops them the same way
    // — a picture's children never reach its `MarkdownPictureSerializer`
    // output, a table's text renders through the reconstructed grid). The
    // observable fix is the border-straddlers: a line only partially under a
    // figure box used to lose its cells to the picture's 0.2 claim and vanish
    // — now it forms an orphan region and is emitted, as docling does.
    let assigned = |c: &TextCell| {
        let ca = area(c.l, c.t, c.r, c.b).max(1.0);
        regions
            .iter()
            .filter(|r| r.label != "picture" && !is_wrapper(r.label))
            .any(|r| inter(r, c.l, c.t, c.r, c.b) / ca > 0.2)
    };
    // Collect orphan cells (non-empty, unassigned), in page order.
    let mut orphans: Vec<&TextCell> = cells
        .iter()
        .filter(|c| !c.text.trim().is_empty() && !assigned(c))
        .collect();
    if orphans.is_empty() {
        return;
    }
    orphans.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l)));
    let (orphans, notes) = place_raised_notes(orphans);
    // Merge cells that sit on the same line and nearly touch into one region, so a
    // dropped multi-word line stays one block (docling's refinement merges these).
    let mut merged: Vec<Region> = Vec::new();
    for (k, c) in orphans.into_iter().enumerate() {
        let mut h = (c.b - c.t).abs().max(1.0);
        if let Some(last) = merged.last_mut() {
            // A raised note is judged by its host line's height, not its own.
            if notes[k] {
                h = h.max((last.b - last.t).abs());
            }
            let same_line = (last.t - c.t).abs() < h * 0.5;
            let touching = c.l <= last.r + h && c.l >= last.l - h;
            // Both tolerances scale with the cell's own height, so a run set
            // vertically (the arXiv stamp up the margin, #528 — a cell a few
            // points wide and hundreds tall) would read as "on the line" of
            // whatever precedes it and glue a whole column into one region.
            // Lines of one row differ by a drop cap's few multiples at most.
            let lh = (last.b - last.t).abs().max(1.0);
            let comparable = h <= 4.0 * lh && lh <= 4.0 * h;
            if same_line && touching && comparable {
                last.l = last.l.min(c.l);
                last.r = last.r.max(c.r);
                last.t = last.t.min(c.t);
                last.b = last.b.max(c.b);
                continue;
            }
        }
        merged.push(Region {
            label: "text",
            score: 0.0,
            l: c.l,
            t: c.t,
            r: c.r,
            b: c.b,
        });
    }
    regions.extend(merged);
}

/// Demote a `picture` region that is really a **text panel** — a paragraph block
/// the layout model boxed as a figure because it is typeset on a colored
/// background (terms-and-conditions callouts, quote boxes) — into ordinary
/// `text` regions, one per paragraph, so its words are read instead of shipped
/// as pixels. docling loses this text the same way (cells assigned to a picture
/// cluster are never serialized); this is a deliberate improvement, not parity.
///
/// The gate is conservative so a genuine figure keeps its crop: the region must
/// contain at least three text lines whose median width spans most of the panel
/// (axis labels and chat bubbles are narrow and varied) and whose cells cover a
/// substantial fraction of its area (a photo or chart with sparse labels does
/// not). Paragraph boundaries are re-derived from the line pitch: a vertical gap
/// clearly larger than the panel's own leading starts a new `text` region, so
/// the panel doesn't collapse into one giant paragraph.
///
/// Works on any cell source — the digital text layer or OCR lines recognized
/// from the picture crop — so the native and browser paths, with or without
/// force-OCR, demote identically.
pub fn recover_text_panels(regions: &mut Vec<Region>, cells: &[TextCell]) {
    // A *captioned* picture is a genuine figure whatever it contains — the
    // corpus is full of document screenshots ("Figure 3: …" above a page
    // image) that are exactly as dense and wide as a text panel. Only an
    // uncaptioned picture is a demotion candidate.
    let captioned: Vec<bool> = regions
        .iter()
        .map(|r| {
            r.label == "picture"
                && regions.iter().any(|c| {
                    c.label == "caption" && c.r.min(r.r) - c.l.max(r.l) > 0.0 && {
                        let gap = if c.t >= r.b {
                            c.t - r.b
                        } else if r.t >= c.b {
                            r.t - c.b
                        } else {
                            f32::MAX // vertically overlapping: not a caption
                        };
                        gap <= 25.0
                    }
                })
        })
        .collect();
    let mut out: Vec<Region> = Vec::with_capacity(regions.len());
    // Synthesized paragraphs and the demoted panels' boxes are kept separate
    // from `out` until the end: the dedup filter below must not confuse a
    // paragraph we just built with a pre-existing region inside the panel.
    let mut demoted_paras: Vec<Region> = Vec::new();
    let mut demoted_boxes: Vec<(f32, f32, f32, f32)> = Vec::new();
    for (i, r) in regions.drain(..).enumerate() {
        if r.label != "picture" || captioned[i] {
            out.push(r);
            continue;
        }
        let inside: Vec<&TextCell> = cells
            .iter()
            .filter(|c| {
                !c.text.trim().is_empty() && {
                    let ca = area(c.l, c.t, c.r, c.b).max(1.0);
                    inter(&r, c.l, c.t, c.r, c.b) / ca > 0.5
                }
            })
            .collect();
        // Group the contained cells into lines by vertical overlap (the same
        // rule region_text orders by), tracking each line's union box.
        let mut lines: Vec<(f32, f32, f32, f32)> = Vec::new(); // (t, b, l, r)
        for c in &inside {
            let (ct, cb) = (c.t.min(c.b), c.t.max(c.b));
            match lines.iter_mut().find(|(lt, lb, _, _)| {
                let ov = cb.min(*lb) - ct.max(*lt);
                ov > 0.5 * (cb - ct).min(*lb - *lt).max(1.0)
            }) {
                Some((lt, lb, ll, lr)) => {
                    *lt = lt.min(ct);
                    *lb = lb.max(cb);
                    *ll = ll.min(c.l);
                    *lr = lr.max(c.r);
                }
                None => lines.push((ct, cb, c.l, c.r)),
            }
        }
        if lines.len() < 3 {
            out.push(r);
            continue;
        }
        let panel_w = (r.r - r.l).max(1.0);
        let coverage = inside.iter().map(|c| area(c.l, c.t, c.r, c.b)).sum::<f32>()
            / area(r.l, r.t, r.r, r.b).max(1.0);
        let mut widths: Vec<f32> = lines.iter().map(|(_, _, l, rr)| rr - l).collect();
        widths.sort_by(f32::total_cmp);
        // A figure's text is ragged: a title line, small axis/tick labels, and
        // OCR boxes over the plot area come out at wildly different heights,
        // whereas a real text panel is set in one face with constant leading.
        // Require near-uniform line heights (median absolute deviation ≤ 35%
        // of the median) so an uncaptioned chart keeps its crop even when its
        // labels are dense enough to pass the coverage gate (#173) — garbled
        // OCR of its bars is not content.
        let mut heights: Vec<f32> = lines.iter().map(|(t, b, _, _)| b - t).collect();
        heights.sort_by(f32::total_cmp);
        let h_med = heights[heights.len() / 2].max(1.0);
        let mut devs: Vec<f32> = heights.iter().map(|h| (h - h_med).abs()).collect();
        devs.sort_by(f32::total_cmp);
        let uniform = devs[devs.len() / 2] <= 0.35 * h_med;
        let text_panel = coverage >= 0.2 && widths[widths.len() / 2] >= 0.45 * panel_w && uniform;
        if !text_panel {
            out.push(r);
            continue;
        }
        lines.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut heights: Vec<f32> = lines.iter().map(|(t, b, _, _)| b - t).collect();
        heights.sort_by(f32::total_cmp);
        let h = heights[heights.len() / 2].max(1.0);
        let mut gaps: Vec<f32> = lines
            .windows(2)
            .map(|w| (w[1].0 - w[0].1).max(0.0))
            .collect();
        gaps.sort_by(f32::total_cmp);
        let leading = if gaps.is_empty() {
            0.0
        } else {
            gaps[gaps.len() / 2]
        };
        let brk = (1.8 * leading).max(0.75 * h);
        let mut para: Option<(f32, f32, f32, f32)> = None; // (l, t, r, b) union
        for (t, b, l, rr) in &lines {
            match &mut para {
                Some((pl, _, pr, pb)) if *t - *pb <= brk => {
                    *pl = pl.min(*l);
                    *pr = pr.max(*rr);
                    *pb = pb.max(*b);
                }
                _ => {
                    if let Some((pl, pt, pr, pb)) = para.take() {
                        demoted_paras.push(Region {
                            label: "text",
                            score: r.score,
                            l: pl,
                            t: pt,
                            r: pr,
                            b: pb,
                        });
                    }
                    para = Some((*l, *t, *rr, *b));
                }
            }
        }
        if let Some((pl, pt, pr, pb)) = para {
            demoted_paras.push(Region {
                label: "text",
                score: r.score,
                l: pl,
                t: pt,
                r: pr,
                b: pb,
            });
        }
        demoted_boxes.push((r.l, r.t, r.r, r.b));
    }
    // The paragraphs are rebuilt from *all* of the panel's cells, so any
    // surviving text region inside a demoted panel (an orphan cluster or a
    // layout-detected fragment — pictures no longer swallow them, #165) would
    // say the same words twice. Consume those; wrappers and pictures stay.
    if !demoted_boxes.is_empty() {
        out.retain(|r| {
            r.label == "picture" || is_wrapper(r.label) || {
                let ra = area(r.l, r.t, r.r, r.b).max(1.0);
                !demoted_boxes
                    .iter()
                    .any(|&(l, t, rr, b)| inter(r, l, t, rr, b) / ra > 0.5)
            }
        });
    }
    // docling's "Remove regular clusters that are included in wrappers" (a
    // regular > 80 % inside a table is absorbed by it) already ran as
    // [`drop_contained_regulars`], but before this demotion created new
    // regulars. Apply it to them too: a panel that coincides with a table (a
    // dense data table detected as picture 0.80 and table 0.62 on one box;
    // `_handle_cross_type_overlaps` keeps both once the picture is ≥ 0.1 more
    // confident) rebuilds the table's words as a paragraph the grid already
    // renders. A panel inside another picture is left as it was.
    demoted_paras.retain(|p| {
        let pa = area(p.l, p.t, p.r, p.b).max(1.0);
        !out.iter()
            .any(|s| is_table_like(s.label) && inter(p, s.l, s.t, s.r, s.b) / pa > 0.8)
    });
    out.extend(demoted_paras);
    *regions = out;
}

/// Drop a `picture` detection covering more than 90 % of the page — docling's
/// `LayoutPostprocessor._process_special_clusters` "Filter out full-page
/// pictures" (upstream since 2.15), applied to the thresholded detections
/// before overlap resolution. A box that big is the page itself, not a figure
/// on it: the layout model emits one for a whole-page diagram (a LaTeX figure
/// PDF cropped to its drawing), a plate, or a scan, and keeping it would swallow
/// every text cell on the page as picture children — the diagram's labels and
/// caption vanish behind a lone `<!-- image -->`, where docling reads them out as
/// text. `page_w`/`page_h` is the display-frame page box.
pub fn drop_full_page_pictures(regions: &mut Vec<Region>, page_w: f32, page_h: f32) {
    let page_area = (page_w * page_h).max(1.0);
    regions.retain(|r| r.label != "picture" || area(r.l, r.t, r.r, r.b) / page_area <= 0.90);
}

/// Drop a `picture` detection that is a small, empty, low-confidence margin box on
/// a **text page** — a false positive the RT-DETR layout sometimes emits (e.g.
/// `right_to_left_02`'s phantom right-column picture, score 0.40); docling does not
/// emit it. The gate is deliberately narrow so a genuine figure is never dropped:
/// (1) only on pages with a digital text layer — image/scanned/figure pages have
/// no `cells` yet at this point (OCR runs later), so their pictures, which *are*
/// the content, are kept; (2) only a box covering < 25 % of the page (a margin
/// artifact, not a dominant figure); (3) only when it contains no text and scores
/// below 0.5 (real empty figures in the corpus all score ≥ 0.86).
pub fn drop_false_pictures(
    regions: &mut Vec<Region>,
    cells: &[TextCell],
    page_w: f32,
    page_h: f32,
) {
    if cells.iter().all(|c| c.text.trim().is_empty()) {
        return; // no digital text layer (image/scanned page) — keep all pictures
    }
    // A text-document page carries several text-bearing non-picture regions (so a
    // spurious margin picture is clearly extra). A slide / figure page has at most
    // one — there the picture is the content, so never drop it.
    let content_regions = regions
        .iter()
        .filter(|r| r.label != "picture" && !region_text(r, cells).trim().is_empty())
        .count();
    if content_regions < 2 {
        return;
    }
    let page_area = (page_w * page_h).max(1.0);
    regions.retain(|r| {
        if r.label != "picture" || r.score >= 0.5 {
            return true;
        }
        if area(r.l, r.t, r.r, r.b) / page_area >= 0.25 {
            return true; // a dominant figure, not a margin artifact
        }
        // Keep it if any text cell falls mostly inside (a real captioned/labelled
        // figure); drop only the genuinely empty low-confidence boxes.
        cells.iter().any(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            !c.text.trim().is_empty() && inter(r, c.l, c.t, c.r, c.b) / ca > 0.5
        })
    });
}

/// A small digit-only region in the top/bottom margin: a page number. docling
/// emits `right_to_left_02`'s bottom `11` as the page's *first* text item (its
/// reading-order model floats the page number to the front), whereas our
/// position-based ordering would place a bottom region last.
fn is_page_number(region: &Region, cells: &[TextCell], page_h: f32) -> bool {
    let t = region_text(region, cells);
    let t = t.trim();
    !t.is_empty()
        && t.chars().all(|c| c.is_ascii_digit())
        && (region.b - region.t).abs() < 30.0
        && (region.t < page_h * 0.12 || region.b > page_h * 0.88)
}

/// docling's `form` / `key_value_region` *containers* (2.123, docling#4064):
/// every region sitting > 0.8 inside one — text, list items, and since #4064
/// tables and pictures too — is that container's child. Children are
/// reading-ordered among themselves and emitted as one block where the
/// container falls in the page's top-level order (a `form_area` /
/// `key_value_area` group upstream), instead of interleaving with the text
/// around the form. A child inside several containers belongs to the smallest
/// (then most confident, then first); a container with children shrinks to
/// their union for the top-level ordering, like upstream's bbox adjustment.
///
/// The containers themselves are still not emitted (`is_skipped`), so the
/// Markdown is exactly upstream's — a group prints only its children.
///
/// `cids` are the items' positions in docling's assembly order
/// ([`cluster_cids`]) — the reading-order predictor's same-row rule (#424)
/// pairs consecutive ones, within the top level and within each container.
fn order_with_containers<T: Clone>(
    items: &mut Vec<T>,
    cids: &[usize],
    page_w: f32,
    page_h: f32,
    reg: impl Fn(&T) -> &Region,
) {
    let is_container = |r: &Region| matches!(r.label, "form" | "key_value_region");
    let containers: Vec<usize> = (0..items.len())
        .filter(|&i| is_container(reg(&items[i])))
        .collect();
    if containers.is_empty() {
        order_regions(items, cids, page_w, page_h, reg);
        return;
    }
    // Parent container per item (containers never nest in each other here —
    // upstream assigns regulars and tables/pictures only).
    let mut parent: Vec<Option<usize>> = vec![None; items.len()];
    for i in 0..items.len() {
        let r = reg(&items[i]);
        if is_container(r) {
            continue;
        }
        let ra = area(r.l, r.t, r.r, r.b).max(1.0);
        let mut best: Option<(usize, f32, f32)> = None; // (idx, area, -score)
        for &c in &containers {
            let cr = reg(&items[c]);
            if inter(r, cr.l, cr.t, cr.r, cr.b) / ra > 0.8 {
                let key = (area(cr.l, cr.t, cr.r, cr.b), -cr.score);
                if best.is_none_or(|(_, a, s)| key.0 < a || (key.0 == a && key.1 < s)) {
                    best = Some((c, key.0, key.1));
                }
            }
        }
        parent[i] = best.map(|(c, _, _)| c);
    }
    // Top-level pass: non-children plus the containers, the latter shrunk to
    // their children's union.
    let mut top: Vec<(usize, Region)> = Vec::new();
    for i in 0..items.len() {
        if parent[i].is_some() {
            continue;
        }
        let mut r = reg(&items[i]).clone();
        if is_container(&r) {
            let kids: Vec<&Region> = (0..items.len())
                .filter(|&k| parent[k] == Some(i))
                .map(|k| reg(&items[k]))
                .collect();
            if !kids.is_empty() {
                r.l = kids.iter().map(|k| k.l).fold(f32::INFINITY, f32::min);
                r.t = kids.iter().map(|k| k.t).fold(f32::INFINITY, f32::min);
                r.r = kids.iter().map(|k| k.r).fold(f32::NEG_INFINITY, f32::max);
                r.b = kids.iter().map(|k| k.b).fold(f32::NEG_INFINITY, f32::max);
            }
        }
        top.push((i, r));
    }
    let top_cids: Vec<usize> = top.iter().map(|(i, _)| cids[*i]).collect();
    order_regions(&mut top, &top_cids, page_w, page_h, |it| &it.1);
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for (i, _) in top {
        if is_container(reg(&items[i])) {
            let kid_idx: Vec<usize> = (0..items.len()).filter(|&k| parent[k] == Some(i)).collect();
            let mut kids: Vec<T> = kid_idx.iter().map(|&k| items[k].clone()).collect();
            let kid_cids: Vec<usize> = kid_idx.iter().map(|&k| cids[k]).collect();
            order_regions(&mut kids, &kid_cids, page_w, page_h, &reg);
            out.push(items[i].clone());
            out.extend(kids);
        } else {
            out.push(items[i].clone());
        }
    }
    *items = out;
}

/// Furniture / not-yet-emitted labels.
fn is_skipped(label: &str) -> bool {
    matches!(
        label,
        "page_header" | "page_footer" | "form" | "key_value_region"
    )
}

/// Reading-order sort of a page's regions, via the ported rule-based
/// [`reading_order`](crate::reading_order) predictor (docling's
/// `ReadingOrderPredictor`): an up/down geometry graph with same-row links
/// between `cids`-consecutive elements (#424), down links between
/// `cids`-consecutive text that overlaps slightly (docling#4276), horizontal
/// dilation and a depth-first traversal, with `page_header`/`page_footer` ordered as their own
/// groups (first/last) as docling does.
fn order_regions<T: Clone>(
    items: &mut Vec<T>,
    cids: &[usize],
    page_w: f32,
    page_h: f32,
    reg: impl Fn(&T) -> &Region,
) {
    let boxes: Vec<(f32, f32, f32, f32)> = items
        .iter()
        .map(|it| {
            let r = reg(it);
            (r.l, r.t, r.r, r.b)
        })
        .collect();
    let is_header: Vec<bool> = items
        .iter()
        .map(|it| reg(it).label == "page_header")
        .collect();
    let is_footer: Vec<bool> = items
        .iter()
        .map(|it| reg(it).label == "page_footer")
        .collect();
    let is_graphic: Vec<bool> = items
        .iter()
        .map(|it| matches!(reg(it).label, "table" | "picture" | "code"))
        .collect();
    let order = crate::reading_order::order_page(
        &boxes,
        cids,
        &is_header,
        &is_footer,
        &is_graphic,
        page_w,
        page_h,
    );
    *items = order.iter().map(|&i| items[i].clone()).collect();
}

/// docling's assembly order of a page's clusters (`LayoutPostprocessor`'s
/// final `_sort_clusters(mode="id")`, #424): each region's rank when sorted by
/// its first source cell, then by top edge, then left edge; a region with no
/// cells sorts after every one that has some. docling numbers its page
/// elements (`cid`) in this order, and the reading-order predictor's same-row
/// rule pairs elements with consecutive numbers, so the ranks are what
/// [`order_with_containers`] hands the predictor.
///
/// A regular region's first cell is the smallest index among the cells it
/// claims. A table, picture or container has no cells of its own upstream
/// either — its cells are its *children's*: the regular clusters > 0.8 inside
/// it, and upstream every cell no regular cluster claimed is an orphan cluster
/// of its own, so a table's interior text (which no regular cluster claims)
/// reaches the table through those orphans. Here that is the cells > 0.8
/// inside the region plus the claimed cells of the regular regions > 0.8
/// inside it. Without the interior cells every table would sort last, and two
/// side-by-side tables would then be consecutive and row-linked — reading the
/// right table's caption ahead of the left column's headings (2206 page 8).
pub fn cluster_cids(regions: &[Region], cells: &[TextCell]) -> Vec<usize> {
    let owned = assign_cells(regions, cells);
    let first_cell: Vec<usize> = regions
        .iter()
        .enumerate()
        .map(|(i, r)| {
            if claims_cells(r) {
                return owned[i].iter().copied().min().unwrap_or(usize::MAX);
            }
            let interior = cells
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    !c.text.trim().is_empty()
                        && inter(r, c.l, c.t, c.r, c.b) / area(c.l, c.t, c.r, c.b).max(1.0) > 0.8
                })
                .map(|(ci, _)| ci)
                .min();
            let children = regions
                .iter()
                .enumerate()
                .filter(|(j, child)| {
                    *j != i && claims_cells(child) && {
                        let ca = area(child.l, child.t, child.r, child.b).max(1.0);
                        inter(r, child.l, child.t, child.r, child.b) / ca > 0.8
                    }
                })
                .filter_map(|(j, _)| owned[j].iter().copied().min())
                .min();
            interior
                .into_iter()
                .chain(children)
                .min()
                .unwrap_or(usize::MAX)
        })
        .collect();
    let mut by_source: Vec<usize> = (0..regions.len()).collect();
    // Stable, like Python's `sorted`: full ties keep the layout order.
    by_source.sort_by(|&a, &b| {
        first_cell[a]
            .cmp(&first_cell[b])
            .then(regions[a].t.total_cmp(&regions[b].t))
            .then(regions[a].l.total_cmp(&regions[b].l))
    });
    let mut cids = vec![0; regions.len()];
    for (rank, &i) in by_source.iter().enumerate() {
        cids[i] = rank;
    }
    cids
}

/// Clean a region's assembled text: undo soft-hyphen line wraps, map curly
/// quotes and the ellipsis to ASCII (matching docling), and collapse runs of
/// whitespace. pdfium emits the line-wrap hyphen as U+0002 in this corpus
/// (U+00AD elsewhere), so `word\u{2} continuation` is one hyphenated word —
/// drop the hyphen + the joining space and merge (`com\u{2} pact` → `compact`,
/// `end-to\u{2} end` → `end-toend`), exactly as docling does.
///
/// Token spacing is otherwise left as the geometric join produced it. We do not
/// tighten punctuation spacing: docling preserves the PDF's own spaces (it keeps
/// `{ ahn }`, `Name 1 .`, `[ 9 ]`), and a geometric gap heuristic diverges from
/// it more than a plain single-space join does.
/// An ordered-list enumeration marker at the start of a list item: leading ASCII
/// digits followed by `.`, e.g. `1. Undo/Redo` → `(1, "Undo/Redo")`. Returns
/// `None` when the text doesn't start with `digits.`.
/// docling's `ListItemMarkerProcessor` bullet patterns
/// (`docling/models/postprocessing/list_marker_processor.py`), one glyph each.
/// The last eight are docling#5043's Symbol/Wingdings bullets as the fonts'
/// private-use code points ([`PUA_FONT_BULLETS`]).
const LIST_BULLET_MARKERS: &str = "\u{2022}\u{2023}\u{25E6}\u{2043}\u{204C}\u{204D}\u{2219}\u{25AA}\u{25AB}\u{25CF}\u{25CB}-*+•·‣⁃►▶▸➤➢✓✔✗✘\u{F0B7}\u{F0A7}\u{F06E}\u{F076}\u{F0D8}\u{F0E0}\u{F0FC}\u{F0A8}";

/// Symbol and Wingdings bullets as the fonts' private-use code points
/// (docling#5043) — what the text layer of a Word-exported PDF holds when its
/// ToUnicode CMap maps the symbol-font byte to `U+F000 + byte` (`U+F0B7` is
/// Symbol's `•`; `U+F0A7`/`U+F06E`/`U+F0A8` Wingdings' squares, `U+F076`
/// its `❖`, `U+F0D8`/`U+F0E0` its arrows, `U+F0FC` its tick). A bullet
/// class of their own, so the glued-bullet strip in [`list_item_node`]
/// drops them too.
const PUA_FONT_BULLETS: [char; 8] = [
    '\u{F0B7}', '\u{F0A7}', '\u{F06E}', '\u{F076}', '\u{F0D8}', '\u{F0E0}', '\u{F0FC}', '\u{F0A8}',
];

/// docling's numbered-marker patterns as byte-length scanners over the start
/// of the text, in its first-wins order (the compound ones first, as they are
/// the more specific). Each returns the marker's candidate lengths, longest
/// (greedy) first — the alternatives Python's regex would backtrack through
/// before the `\s(.+)` tail. Hand-rolled rather than `regex` because this file
/// is part of the `pdf-text` (wasm) build, where the `regex` crate is not.
/// `\d` is Unicode-aware in Python, hence `char::is_numeric`; the letters are
/// ASCII classes in both.
const LIST_NUMBERED_MARKERS: &[fn(&str) -> Vec<usize>] = &[
    // `\d+(?:\.\d+)+\.?` — 1.1  1.2.3  1.1.
    |s| {
        let mut i = digits(s, 0);
        if i == 0 {
            return Vec::new();
        }
        let mut groups = 0;
        while s[i..].starts_with('.') && digits(s, i + 1) > i + 1 {
            i = digits(s, i + 1);
            groups += 1;
        }
        if groups == 0 {
            return Vec::new();
        }
        if s[i..].starts_with('.') {
            vec![i + 1, i]
        } else {
            vec![i]
        }
    },
    // `\d+\.?[a-zA-Z]\.` — 9a. 3.a.
    |s| digits_dot_letter(s, 0, '.').into_iter().collect(),
    // `\d+\.?[a-zA-Z]\)` — 9a) 3.a)
    |s| digits_dot_letter(s, 0, ')').into_iter().collect(),
    // `\(\d+\.?[a-zA-Z]\)` — (9a) (3.a)
    |s| {
        if !s.starts_with('(') {
            return Vec::new();
        }
        digits_dot_letter(s, 1, ')').into_iter().collect()
    },
    // `\d+\.` — 1. 2. 3.
    |s| digits_then(s, 0, '.').into_iter().collect(),
    // `\d+\)` — 1) 2) 3)
    |s| digits_then(s, 0, ')').into_iter().collect(),
    // `\(\d+\)` — (1) (2) (3)
    |s| {
        if !s.starts_with('(') {
            return Vec::new();
        }
        digits_then(s, 1, ')').into_iter().collect()
    },
    // `\[\d+\]` — [1] [2] [3]
    |s| {
        if !s.starts_with('[') {
            return Vec::new();
        }
        digits_then(s, 1, ']').into_iter().collect()
    },
    // `[ivxlcdm]+\.` — i. ii. iii.
    |s| class_run_then(s, "ivxlcdm", '.').into_iter().collect(),
    // `[IVXLCDM]+\.` — I. II. III.
    |s| class_run_then(s, "IVXLCDM", '.').into_iter().collect(),
    // `[a-z]\.` / `[A-Z]\.` / `[a-z]\)` / `[A-Z]\)`
    |s| {
        letter_then(s, char::is_ascii_lowercase, '.')
            .into_iter()
            .collect()
    },
    |s| {
        letter_then(s, char::is_ascii_uppercase, '.')
            .into_iter()
            .collect()
    },
    |s| {
        letter_then(s, char::is_ascii_lowercase, ')')
            .into_iter()
            .collect()
    },
    |s| {
        letter_then(s, char::is_ascii_uppercase, ')')
            .into_iter()
            .collect()
    },
];

/// Byte offset just past the run of `\d` characters starting at `from`
/// (`from` itself when there is none).
fn digits(s: &str, from: usize) -> usize {
    s[from..]
        .char_indices()
        .find(|(_, c)| !c.is_numeric())
        .map_or(s.len(), |(i, _)| from + i)
}

/// `\d+<close>` from `from`: the length through `close`, if it matches.
fn digits_then(s: &str, from: usize, close: char) -> Option<usize> {
    let end = digits(s, from);
    (end > from && s[end..].starts_with(close)).then(|| end + close.len_utf8())
}

/// `\d+\.?[a-zA-Z]<close>` from `from`.
fn digits_dot_letter(s: &str, from: usize, close: char) -> Option<usize> {
    let mut i = digits(s, from);
    if i == from {
        return None;
    }
    if s[i..].starts_with('.') {
        i += 1;
    }
    let letter = s[i..].chars().next().filter(char::is_ascii_alphabetic)?;
    i += letter.len_utf8();
    s[i..].starts_with(close).then(|| i + close.len_utf8())
}

/// `[<class>]+<close>` at the start.
fn class_run_then(s: &str, class: &str, close: char) -> Option<usize> {
    let end = s
        .char_indices()
        .find(|(_, c)| !class.contains(*c))
        .map_or(s.len(), |(i, _)| i);
    (end > 0 && s[end..].starts_with(close)).then(|| end + close.len_utf8())
}

/// `[<letter class>]<close>` at the start.
fn letter_then(s: &str, class: fn(&char) -> bool, close: char) -> Option<usize> {
    let letter = s.chars().next().filter(class)?;
    let i = letter.len_utf8();
    s[i..].starts_with(close).then(|| i + close.len_utf8())
}

/// docling's `ListItemMarkerProcessor.process_list_item`: the item's original
/// text is matched against `^(marker)\s(.+)` (DOTALL) for the bullet patterns,
/// then the numbered ones in order; a hit splits it into
/// `(marker, text, enumerated)`. `\s` is one Unicode whitespace character and
/// `.+` everything after it, which must be non-empty.
fn split_list_marker(text: &str) -> Option<(&str, &str, bool)> {
    let tail_after = |len: usize| -> Option<&str> {
        let ws = text[len..].chars().next()?;
        if !ws.is_whitespace() {
            return None;
        }
        let rest = &text[len + ws.len_utf8()..];
        (!rest.is_empty()).then_some(rest)
    };
    let first = text.chars().next()?;
    if LIST_BULLET_MARKERS.contains(first) {
        if let Some(rest) = tail_after(first.len_utf8()) {
            return Some((&text[..first.len_utf8()], rest, false));
        }
    }
    for matcher in LIST_NUMBERED_MARKERS {
        for len in matcher(text) {
            if let Some(rest) = tail_after(len) {
                return Some((&text[..len], rest, true));
            }
        }
    }
    None
}

/// A PDF `list_item` region as docling emits it: `ListItemMarkerProcessor`
/// splits the marker off (see [`split_list_marker`]), and docling-core's
/// Markdown list serializer (default `orig_list_item_marker_mode = AUTO`,
/// `ensure_valid_list_item_marker`) prints it as `N. text` for an `N.` marker
/// (`case_already_valid`: the marker verbatim); as `- text` for a bullet glyph
/// (no letter or digit in the marker: only the `-` the serializer adds); and
/// as `- a) text` / `- 1.2 text` / `- [3] text` for any other marker
/// (`case_auto`: the serializer's `-`, then the original marker) — spelled
/// here as a bullet item whose text carries the marker, the way the DOCX and
/// DOC backends already spell theirs: the Markdown fields then carry no
/// marker (the serializer applies `case_auto` to a marker it is given, so it
/// would print it a second time, `- (1) (1) text`), and the DocLang
/// `<marker>` rides in the [`ListItemDclx`] overlay. An item without a recognizable marker is
/// a plain bullet. The symbol-font bullets docling-parse filters out of its
/// cells (`•◦▪·*` glued to the text) are stripped before the match, as before.
fn list_item_node(text: &str, loc: [u16; 4], first_in_list: bool) -> Node {
    // docling's match runs on the text as docling-parse hands it over; the
    // glued symbol-font bullets it never sees are stripped only when the raw
    // text matches no marker (`•Text` → `Text`, but `• Text` → marker `•`).
    // The private-use Symbol/Wingdings bullets (docling#5043) are stripped
    // the same way: glued to the text they match no marker either, and a
    // PUA code left in the item text prints as tofu.
    let stripped = text
        .trim_start_matches(|c| {
            matches!(c, '•' | '◦' | '▪' | '·' | '*') || PUA_FONT_BULLETS.contains(&c)
        })
        .trim_start();
    let split = split_list_marker(text).or_else(|| split_list_marker(stripped));
    let bullet = |text: String, marker: &str| Node::ListItem {
        ordered: false,
        number: 0,
        first_in_list,
        text: md_escape(&text),
        level: 0,
        // docling keeps the marker as the DocLang list marker
        // (`<ldiv><marker>·</marker></ldiv>`); Markdown prints its own `-`.
        marker: Some(marker.to_string()),
        location: Some(loc),
        dclx: None,
        href: None,
        layer: None,
    };
    // docling-core's `case_already_valid` is a *full* `\d+\.` match: `3.a.`
    // and `1.2` are `case_auto` markers (`- 3.a. text`), not item numbers.
    let is_number_dot = |m: &str| {
        m.strip_suffix('.')
            .is_some_and(|d| !d.is_empty() && d.chars().all(char::is_numeric))
    };
    match split {
        Some((marker, body, true)) if is_number_dot(marker) => {
            let number = parse_ordered_marker(marker).map_or(0, |(n, _)| n);
            Node::ListItem {
                ordered: true,
                number,
                first_in_list,
                text: md_escape(body),
                level: 0,
                marker: Some(marker.to_string()),
                location: Some(loc),
                dclx: None,
                href: None,
                layer: None,
            }
        }
        // `case_auto`: a marker holding a letter or digit rides in the text,
        // so not in the Markdown `marker` (docling-core's serializer would add
        // it again); DocLang keeps it through the overlay, whose text is the
        // node's own, so the DocLang output is what it was.
        Some((marker, body, true)) => {
            let text = md_escape(&format!("{marker} {body}"));
            Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list,
                text: text.clone(),
                level: 0,
                marker: None,
                location: Some(loc),
                dclx: Some(ListItemDclx {
                    ordered: false,
                    marker: Some(marker.to_string()),
                    text,
                    runs: Vec::new(),
                }),
                href: None,
                layer: None,
            }
        }
        Some((marker, body, false)) => bullet(body.to_string(), marker),
        None => bullet(stripped.to_string(), "·"),
    }
}

fn parse_ordered_marker(s: &str) -> Option<(u64, String)> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = s[digits.len()..].strip_prefix('.')?;
    let number = digits.parse().ok()?;
    Some((number, rest.trim_start().to_string()))
}

/// Escape markdown special characters the way docling-core's markdown serializer
/// does (`markdown.py` post_process): `_` → `\_`, then HTML-escape `&`, `<`, `>`
/// (quote=False, so quotes are left). Applied to prose (headings, list items,
/// paragraphs); code blocks, the formula placeholder, and table cells are left raw.
pub(crate) fn md_escape(text: &str) -> String {
    text.replace('_', "\\_")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// [`clean_text_keep_wrap`] without the trailing soft hyphen: for text that is
/// not a paragraph fragment (headings, captions, cells, code).
fn clean_text(text: &str) -> String {
    let mut out = clean_text_keep_wrap(text);
    if out.ends_with('\u{ad}') {
        out.pop();
    }
    out
}

/// [`clean_text`], except that a soft hyphen ending the text survives: it is a
/// word split across a line break, and the paragraph merge
/// ([`merge_continuations`]) needs it to fuse `Veröffent\u{ad}` + `lichung`
/// into one word (#919). Stripped here, the fragment ended mid-word with
/// nothing to say so, and the two halves were joined with a space.
fn clean_text_keep_wrap(text: &str) -> String {
    let wrap = text.trim_end().ends_with('\u{ad}');
    let unwrapped = text
        .replace("\u{2} ", "")
        .replace("\u{ad} ", "")
        // any stray wrap hyphens not at a join
        .replace(['\u{2}', '\u{ad}'], "");
    // `pdf_keep_typography` (#921): legal citations and quoted terms tell
    // `„…“` and `‚…‘` from ASCII quotes, and `–` ranges from hyphens; the
    // folding erases that, so the opt-out keeps the glyphs as drawn.
    let replaced = if KEEP_TYPOGRAPHY.with(std::cell::Cell::get) {
        unwrapped
    } else {
        fold_typography(&unwrapped)
    };
    // The docling-parse sanitizer already placed the correct spacing (e.g.
    // justified double spaces); preserve internal runs of spaces, only
    // normalizing line breaks/tabs and trimming the ends.
    let out = replaced.replace(['\n', '\r', '\t'], " ").trim().to_string();
    let out = fold_presentation_forms(&out);
    let mut out = fix_arabic_lam_alef(&out);
    if wrap && !out.is_empty() {
        out.push('\u{ad}');
    }
    out
}

thread_local! {
    /// Whether the page being assembled keeps its typographic punctuation
    /// ([`PdfPage::keep_typography`]); set for the length of one
    /// [`assemble_page`] by [`TypographyScope`], so the text cleaners deep in
    /// it need no extra parameter.
    static KEEP_TYPOGRAPHY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Sets [`KEEP_TYPOGRAPHY`] for one page's assembly and restores it after.
struct TypographyScope(bool);

impl TypographyScope {
    fn set(keep: bool) -> Self {
        Self(KEEP_TYPOGRAPHY.with(|k| k.replace(keep)))
    }
}

impl Drop for TypographyScope {
    fn drop(&mut self) {
        KEEP_TYPOGRAPHY.with(|k| k.set(self.0));
    }
}

/// `DOCLING_RS_PDF_KEEP_TYPOGRAPHY` (#921): whether this process keeps
/// typographic punctuation as drawn unless a conversion says otherwise.
pub fn keep_typography_from_env() -> bool {
    docling_core::env::flag("DOCLING_RS_PDF_KEEP_TYPOGRAPHY")
}

/// docling-parse's sanitizer table (`pdf_sanitators/constants.h`) applied to
/// assembled text: single curly quotes become the ASCII apostrophe `'`,
/// double ones the ASCII `"` (docling-parse#349, 7.x — before it folded both
/// to `'`, which docling's groundtruth carried until its docling-parse 7
/// refresh), `‚` a comma, the dash family a hyphen.
fn fold_typography(text: &str) -> String {
    text.replace(['\u{2018}', '\u{2019}', '\u{201b}'], "'") // ‘ ’ ‛ → '
        .replace(['\u{201c}', '\u{201d}', '\u{201e}', '\u{201f}'], "\"") // “ ” „ ‟ → "
        .replace('\u{201a}', ",") // ‚ → ,
        .replace(
            [
                '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
            ],
            "-",
        ) // hyphen/dash family → -
        .replace('\u{2044}', "/") // ⁄ fraction slash → /
        .replace('\u{2022}', "\u{b7}") // • → · (docling never emits •; inline CCS-concept separators)
        .replace('\u{2026}', "...") // … → ...
}

/// The character folds of docling's `normalize_extracted_text` (docling#3929,
/// docling#5069) beyond [`fold_typography`]: Latin ligatures spelled out
/// (`ﬁ` → `fi`, `ﬅ`/`ﬆ` → `st`, Dutch `Ĳ`/`ĳ` → `IJ`/`ij`), the spurious
/// private-use glyph U+F0A0 some fonts emit dropped, and runs of Arabic
/// Presentation Forms-A/B (U+FB50–FDFF, U+FE70–FEFF) NFKC-folded back to the
/// canonical letters, so shaped Arabic (`ﻣﺎﺮﺱ` → `مارس`) is searchable. The
/// NFKC is per run, never global, so `x²`, `H₂O` and full-width forms
/// survive. The text layer already spells out U+FB00–FB06
/// (`textparse::decompose_ligatures`); folding them here too covers the
/// other text sources. Not typography: `pdf_keep_typography` keeps these.
pub(crate) fn fold_presentation_forms(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let is_arabic_form = |c: char| matches!(c, '\u{FB50}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFF}');
    let needs = |c: char| {
        matches!(
            c,
            '\u{FB00}'..='\u{FB06}' | '\u{132}' | '\u{133}' | '\u{F0A0}'
        ) || is_arabic_form(c)
    };
    if !text.chars().any(needs) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if is_arabic_form(c) {
            run.push(c);
            continue;
        }
        if !run.is_empty() {
            out.extend(run.nfkc());
            run.clear();
        }
        match c {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            '\u{132}' => out.push_str("IJ"),
            '\u{133}' => out.push_str("ij"),
            '\u{F0A0}' => {}
            _ => out.push(c),
        }
    }
    if !run.is_empty() {
        out.extend(run.nfkc());
    }
    out
}

/// docling's `normalize_extracted_text` for a TableFormer cell (docling#5069):
/// cells get the same folds as body text — before, only `@ ` joined, so a
/// cell kept `it’s`, `“quoted”` and `• dot` where the paragraph beside it
/// read `it's`, `"quoted"`, `· dot`. The typography half honours
/// `pdf_keep_typography` like [`clean_text_keep_wrap`]; the Arabic lam-alef
/// reordering stays a body-text step (pdfium's visual-order runs).
fn normalize_table_text(text: &str) -> String {
    let text = if KEEP_TYPOGRAPHY.with(std::cell::Cell::get) {
        text.to_string()
    } else {
        fold_typography(text)
    };
    fold_presentation_forms(&text)
}

/// [`normalize_table_text`] over a TableFormer grid: the dense `rows` text and
/// the first-class `cells` the JSON / DocLang exports read.
fn normalize_grid(mut grid: TableGrid) -> TableGrid {
    for row in &mut grid.rows {
        for cell in row.iter_mut() {
            *cell = normalize_table_text(cell);
        }
    }
    for cell in &mut grid.cells {
        cell.text = normalize_table_text(&cell.text);
    }
    grid
}

/// pdfium decomposes the Arabic lam-alef ligature (لا / لإ / لأ / لآ) into its
/// glyph constituents in *visual* order — `alef-variant, lam` — but docling keeps
/// logical order, `lam, alef-variant`. Swap a mid-word `alef-variant + lam` back
/// to `lam + alef-variant`. "Mid-word" (the previous char is an Arabic letter)
/// distinguishes the ligature from the definite article `ال` (word-initial
/// `alef + lam`), which must stay. No-op for non-Arabic text.
fn fix_arabic_lam_alef(s: &str) -> String {
    let is_arabic_letter = |c: char| ('\u{0620}'..='\u{064A}').contains(&c);
    let chars: Vec<char> = s.chars().collect();
    if !chars.iter().any(|&c| is_arabic_letter(c)) {
        return s.to_string(); // no-op for non-Arabic text
    }
    // Pass 1: swap mid-word `alef-variant + lam` → `lam + alef-variant`. Only the
    // hamza/madda alef variants (إ أ آ) are safe: the definite article is always
    // plain `ا + ل`, so plain `alef + lam` is ambiguous (a legitimate `فعالة` vs a
    // reversed `لا` ligature look identical) — leaving plain alef alone avoids
    // corrupting legitimate words.
    let mut a: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '\u{0622}' | '\u{0623}' | '\u{0625}')
            && chars.get(i + 1) == Some(&'\u{0644}')
            && i > 0
            && is_arabic_letter(chars[i - 1])
            // A preceding lam means this alef-variant is *already* the logical
            // `lam + alef` ligature; the following lam is the next syllable's
            // letter, not a reversed ligature — swapping it corrupts `لآل` → `للآ`
            // (e.g. التعلم الآلي → الآلي, not اللآي).
            && chars[i - 1] != '\u{0644}'
        {
            a.push('\u{0644}');
            a.push(c);
            i += 2;
            continue;
        }
        a.push(c);
        i += 1;
    }
    // Pass 2: insert a space at Arabic↔Latin boundaries (bidi script switch) that
    // pdfium runs together — docling separates the embedded Latin run (`وPython`
    // → `و Python`).
    let mut out: Vec<char> = Vec::with_capacity(a.len());
    for (j, &c) in a.iter().enumerate() {
        if j > 0 {
            let p = a[j - 1];
            if (is_arabic_letter(p) && c.is_ascii_alphabetic())
                || (p.is_ascii_alphabetic() && is_arabic_letter(c))
            {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out.into_iter().collect()
}

/// docling's `PageAssembleModel._match_hyperlink`: the URI whose link
/// annotations cover at least half of the region's box, or `None`. Coverage is
/// intersection-over-region-area, **accumulated per URI** — a URL that wraps
/// across lines carries several annotation rects that sum toward the same
/// target. Ties resolve to the first-seen URI (Python's `max` over dict
/// insertion order); the winner still needs `>= 0.5`
/// (`_HYPERLINK_COVERAGE_THRESHOLD`).
pub(crate) fn region_hyperlink(
    region: &Region,
    links: &[crate::pdfium_backend::LinkAnnot],
) -> Option<String> {
    if links.is_empty() {
        return None;
    }
    let area = (region.r - region.l).max(0.0) * (region.b - region.t).max(0.0);
    if area <= 0.0 {
        return None;
    }
    let mut coverage: Vec<(&str, f32)> = Vec::new();
    for link in links {
        let ix = (region.r.min(link.r) - region.l.max(link.l)).max(0.0);
        let iy = (region.b.min(link.b) - region.t.max(link.t)).max(0.0);
        let c = ix * iy / area;
        match coverage.iter_mut().find(|(uri, _)| *uri == link.uri) {
            Some((_, acc)) => *acc += c,
            None => coverage.push((&link.uri, c)),
        }
    }
    let mut best: Option<(&str, f32)> = None;
    for (uri, c) in coverage {
        // Strictly greater keeps the first-seen URI on ties, like Python's max.
        if best.is_none_or(|(_, bc)| c > bc) {
            best = Some((uri, c));
        }
    }
    let (uri, c) = best?;
    (c >= 0.5).then(|| normalize_uri(uri))
}

/// The pydantic-`AnyUrl` normalization docling's hyperlink value passes
/// through on its way to the serializer: a URL with an authority but no path
/// gains a trailing `/` (`https://arxiv.org` → `https://arxiv.org/`). Other
/// AnyUrl canonicalizations (scheme/host lowercasing, percent-encoding) don't
/// occur in PDF link annotations in practice, so they are not reproduced.
fn normalize_uri(uri: &str) -> String {
    if let Some((_, rest)) = uri.split_once("://") {
        if !rest.is_empty() && !rest.contains(['/', '?', '#']) {
            return format!("{uri}/");
        }
    }
    uri.to_string()
}

/// Resolve each page hyperlink to the visible text it covers, as `(anchor, uri)`
/// in reading order. The anchor is the cells whose centre falls in the link rect,
/// joined left-to-right and cleaned the same way prose is (so it matches the
/// serialized text), deduped against the immediately-preceding link so pdfium's
/// occasional duplicate annotation doesn't double-list. Empty anchors are dropped.
pub(crate) fn resolve_link_anchors(page: &PdfPage) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    // Use per-word cells, not the line-merged `cells`: a link rect covers a few
    // words on a line, and a whole merged line cell would over-capture (its centre
    // lands in one link's rect, grabbing the entire line as that link's anchor).
    let words = if page.word_cells.is_empty() {
        &page.cells
    } else {
        &page.word_cells
    };
    for link in &page.links {
        // A cell participates when its centre row is inside the rect and it
        // overlaps the rect horizontally. A cell can be *wider* than the rect:
        // PDFs often draw a whole header line as one text run ("LinkedIn |
        // GitHub | Credly"), which docling-parse's word grouping keeps as one
        // cell even though each label carries its own link annotation —
        // centre-in-rect alone would hand the entire line to every link.
        // [`cell_text_in_rect`] clips such a cell to the tokens under the rect.
        let mut inside: Vec<(&TextCell, String)> = words
            .iter()
            .filter(|c| {
                let cy = (c.t + c.b) / 2.0;
                cy >= link.t && cy <= link.b && c.r.min(link.r) > c.l.max(link.l)
            })
            .filter_map(|c| {
                let text = cell_text_in_rect(c, link.l, link.r);
                (!text.is_empty()).then_some((c, text))
            })
            .collect();
        // Reading order: top band then left-to-right (link anchors are LTR).
        let band = inside
            .iter()
            .map(|(c, _)| (c.b - c.t).abs())
            .fold(0.0f32, f32::max)
            .max(1.0);
        inside.sort_by_key(|(c, _)| ((c.t / band).round() as i64, (c.l * 10.0) as i64));
        let anchor = clean_text(
            &inside
                .iter()
                .map(|(_, t)| t.trim())
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        );
        if anchor.is_empty() {
            continue;
        }
        if out
            .last()
            .is_some_and(|(a, u)| a == &anchor && u == &link.uri)
        {
            continue;
        }
        out.push((anchor, link.uri.clone()));
    }
    out
}

/// The part of a cell's text that lies under a link rect's x-range. A cell
/// fully inside the rect (by centre) returns its whole text. A wider cell is
/// split into whitespace tokens whose x-spans are estimated proportionally to
/// their character positions (kerning makes this approximate, so selection
/// snaps to whole tokens, never characters); tokens whose estimated centre
/// falls inside the rect are kept. Returns "" when nothing falls inside.
fn cell_text_in_rect(c: &TextCell, l: f32, r: f32) -> String {
    let cx = (c.l + c.r) / 2.0;
    if cx >= l && cx <= r && c.l >= l - (c.r - c.l) * 0.25 && c.r <= r + (c.r - c.l) * 0.25 {
        return c.text.trim().to_string();
    }
    let chars: Vec<char> = c.text.chars().collect();
    let n = chars.len();
    if n == 0 || c.r <= c.l {
        return String::new();
    }
    let per = (c.r - c.l) / n as f32;
    let mut out: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut start = 0usize;
    // A trailing sentinel space flushes the last token.
    for (i, &ch) in chars.iter().enumerate().chain(std::iter::once((n, &' '))) {
        if ch.is_whitespace() {
            if !token.is_empty() {
                let mid = c.l + (start as f32 + (i - start) as f32 / 2.0) * per;
                if mid >= l && mid <= r {
                    out.push(std::mem::take(&mut token));
                } else {
                    token.clear();
                }
            }
        } else {
            if token.is_empty() {
                start = i;
            }
            token.push(ch);
        }
    }
    out.join(" ")
}

/// Cells assigned to a region (best container), in reading order, joined.
fn region_text(region: &Region, cells: &[TextCell]) -> String {
    let inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .collect();
    cells_text(inside)
}

/// docling's exclusive cell assignment (`_assign_cells_to_clusters`): every
/// non-empty cell goes to the single best-overlapping *regular* region at
/// intersection-over-self > 0.2, and each region serializes exactly its
/// assigned cells. A cell under two overlapping boxes is emitted once (by the
/// better-covering one), and a cell only partially under its region — e.g.
/// normal_4pages' big section numeral, ~30 % inside the heading box — still
/// joins it (`## 들어가며 1`) instead of leaking as an orphan. Pictures and
/// wrappers never claim (docling walks regular clusters only); ties go to the
/// first region, like docling's strict `>` best-overlap scan.
pub fn region_texts_exclusive(regions: &[Region], cells: &[TextCell]) -> Vec<String> {
    let mut texts = region_texts_keep_wrap(regions, cells);
    for t in &mut texts {
        if t.ends_with('\u{ad}') {
            t.pop();
        }
    }
    texts
}

/// [`region_texts_exclusive`] with a region's trailing soft hyphen kept
/// (#919): the page assembler strips it and hands it to the paragraph it ends.
fn region_texts_keep_wrap(regions: &[Region], cells: &[TextCell]) -> Vec<String> {
    let owned = assign_cells(regions, cells);
    // Non-claimers (tables/wrappers/pictures) keep the inclusive > 0.5 text:
    // docling fills a special cluster's cells from its contained children, and
    // downstream table assembly gates on that text being non-empty.
    regions
        .iter()
        .zip(owned)
        .map(|(r, cs)| {
            if claims_cells(r) {
                cells_text_keep_wrap(cs.iter().map(|&i| &cells[i]).collect())
            } else {
                region_text(r, cells)
            }
        })
        .collect()
}

/// A *regular* region in docling's sense — one that claims cells. Pictures and
/// the wrappers (`table`, `document_index`, `form`, `key_value_region`) fill
/// their cells from contained children instead.
fn claims_cells(r: &Region) -> bool {
    r.label != "picture" && !is_wrapper(r.label)
}

/// docling's `_assign_cells_to_clusters`: each non-empty cell's index goes to
/// the single best-overlapping regular region at intersection-over-self > 0.2
/// (ties to the first region, like docling's strict `>` scan). One entry per
/// region, in region order.
fn assign_cells(regions: &[Region], cells: &[TextCell]) -> Vec<Vec<usize>> {
    let mut owned: Vec<Vec<usize>> = vec![Vec::new(); regions.len()];
    for (ci, c) in cells.iter().enumerate() {
        if c.text.trim().is_empty() {
            continue;
        }
        let ca = area(c.l, c.t, c.r, c.b).max(1.0);
        let mut best: Option<(usize, f32)> = None;
        for (i, r) in regions.iter().enumerate() {
            if !claims_cells(r) {
                continue;
            }
            let ov = inter(r, c.l, c.t, c.r, c.b) / ca;
            if ov > 0.2 && best.is_none_or(|(_, b)| ov > b) {
                best = Some((i, ov));
            }
        }
        if let Some((i, _)) = best {
            owned[i].push(ci);
        }
    }
    owned
}

/// The ballot-box glyph a checkbox line can open with (#609): `Some(checked)`.
/// Empty boxes — `☐`, the white squares `□ ▢ ◻` and the shadowed `❏ ❐ ❑ ❒`
/// Word's checkbox bullets use — are unchecked; `☑ ☒ ⊠ ⌧ ▣ 🗹 🗷 🗵` are
/// checked. A bare tick or cross (`✓ ✗`) is a list bullet
/// ([`LIST_BULLET_MARKERS`]), not a box. Symbol-font boxes that reach the text
/// layer as Private Use Area codes or as their ASCII byte (Wingdings `o`,
/// `þ`) are not recognized: without the font the code is ambiguous (`U+F06F`
/// is Symbol's omicron too). The private-use codes docling#5043 made list
/// bullets ([`PUA_FONT_BULLETS`] — Wingdings' `U+F0A8` square and `U+F0FC`
/// tick among them) stay bullets here as well, as docling reads them.
pub(crate) fn checkbox_glyph(c: char) -> Option<bool> {
    match c {
        '☐' | '□' | '▢' | '◻' | '❏' | '❐' | '❑' | '❒' => Some(false),
        '☑' | '☒' | '⊠' | '⌧' | '▣' | '\u{1F5F9}' | '\u{1F5F7}' | '\u{1F5F5}' => {
            Some(true)
        }
        _ => None,
    }
}

/// `text` without its leading ballot-box glyph and the space after it — a
/// checkbox item's label is the option text, the box is its state.
pub(crate) fn strip_checkbox_glyph(text: &str) -> &str {
    let t = text.trim_start();
    match t.chars().next() {
        Some(c) if checkbox_glyph(c).is_some() => t[c.len_utf8()..].trim_start(),
        _ => text,
    }
}

/// Give every checklist line its own checkbox region (#609). The layout
/// model often reads a checklist as one `text` (or `list_item`) block — on
/// the reporter's ReportLab page all four options are one region, which then
/// printed as `First option Second option …`, and docling itself splits it
/// into two garbled paragraphs. What marks an item is on the page: a drawn
/// square just left of the line ([`crate::checkbox`]), or a ballot-box glyph
/// the line opens with ([`checkbox_glyph`]: `☐ Yes`, `☒ Done`). A region
/// whose lines carry either splits into a `checkbox_selected` /
/// `checkbox_unselected` region per marked line (its box spans the square
/// and the line; a following unmarked line — a wrapped label — stays with
/// it), so assembly emits [`Node::CheckboxItem`]s exactly as for the model's
/// own checkbox labels, the glyph stripped from the label. Lines above the
/// first mark stay one region of the original label. The first piece
/// replaces the region in place and the rest are appended, so per-region
/// slices indexed by the original order (table grids, enrichments — neither
/// applies to text) stay aligned.
///
/// A square counts for a line when its vertical centre lies on the line and
/// it sits just left of the line's first cell: no more than 1.5 sides (at
/// least 6 pt) of gap, at most 1 pt of overlap. Text set *inside* squares (a
/// comb field) never matches. A glyph counts when it opens the line and is
/// the line's only box glyph; a line with several (`☐ Yes ☐ No`) stays a text
/// piece of its own rather than become one item labelled `Yes ☐ No`. A
/// region the model already labelled a checkbox is not split, but its state
/// follows its first line's mark when it has one — the glyph or the square
/// is what the page says, the model's label a guess from the pixels (it read
/// a `☑ Bread` line as unselected). With neither on the page this is a no-op.
pub fn split_checkbox_lines(
    regions: &mut Vec<Region>,
    cells: &[TextCell],
    boxes: &[crate::checkbox::CheckBox],
) {
    let has_glyph = || {
        cells
            .iter()
            .any(|c| c.text.chars().any(|ch| checkbox_glyph(ch).is_some()))
    };
    if boxes.is_empty() && !has_glyph() {
        return;
    }
    let owned = assign_cells(regions, cells);
    let mut used = vec![false; boxes.len()];
    let mut appended: Vec<Region> = Vec::new();
    for (i, mine) in owned.iter().enumerate() {
        let region = regions[i].clone();
        let model_checkbox = matches!(region.label, "checkbox_selected" | "checkbox_unselected");
        if !(model_checkbox || matches!(region.label, "text" | "list_item")) || mine.is_empty() {
            continue;
        }
        // The region's lines, top down: cells sharing a vertical centre band,
        // with the row's text in left-to-right order (for the glyph check).
        let mut sorted: Vec<&TextCell> = mine.iter().map(|&c| &cells[c]).collect();
        sorted.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l)));
        // (l, t, r, b) of each line, and its cells.
        type Row<'c> = ((f32, f32, f32, f32), Vec<&'c TextCell>);
        let mut rows: Vec<Row> = Vec::new();
        for c in sorted {
            let mid = (c.t + c.b) / 2.0;
            match rows.last_mut() {
                Some((row, members)) if mid >= row.1 && mid <= row.3 => {
                    *row = (
                        row.0.min(c.l),
                        row.1.min(c.t),
                        row.2.max(c.r),
                        row.3.max(c.b),
                    );
                    members.push(c);
                }
                _ => rows.push(((c.l, c.t, c.r, c.b), vec![c])),
            }
        }
        // What each row does to the grouping.
        enum Mark {
            /// Opens a checkbox item (`square`: the drawn one, if any).
            Item {
                square: Option<usize>,
                checked: bool,
            },
            /// Several box glyphs on one line (`☐ Yes ☐ No`): a text piece of
            /// its own, closing the open item.
            Plain,
            /// Unmarked: continues the open piece (a wrapped label).
            Continue,
        }
        let mut marks: Vec<Mark> = Vec::with_capacity(rows.len());
        for ((l, t, _, b), members) in &rows {
            let square = boxes
                .iter()
                .enumerate()
                .filter(|&(k, sq)| {
                    let side = sq.r - sq.l;
                    let mid = (sq.t + sq.b) / 2.0;
                    let gap = l - sq.r;
                    !used[k]
                        && mid >= t - 2.0
                        && mid <= b + 2.0
                        && gap >= -1.0
                        && gap <= (1.5 * side).max(6.0)
                })
                .min_by(|a, b| (l - a.1.r).total_cmp(&(l - b.1.r)))
                .map(|(k, _)| k);
            let mark = match square {
                Some(k) => {
                    used[k] = true;
                    Mark::Item {
                        square: Some(k),
                        checked: boxes[k].checked,
                    }
                }
                None => {
                    let mut members = members.clone();
                    members.sort_by(|a, b| a.l.total_cmp(&b.l));
                    let text: String = members.iter().map(|c| c.text.as_str()).collect();
                    let first = text.trim_start().chars().next().and_then(checkbox_glyph);
                    let glyphs = text
                        .chars()
                        .filter(|&c| checkbox_glyph(c).is_some())
                        .count();
                    match (first, glyphs) {
                        (Some(checked), 1) => Mark::Item {
                            square: None,
                            checked,
                        },
                        (_, 0) => Mark::Continue,
                        _ => Mark::Plain,
                    }
                }
            };
            marks.push(mark);
        }
        // A region the model already labelled a checkbox keeps its extent;
        // only its state follows the mark — a glyph or a drawn square is the
        // page's own record, where the model reads it off the pixels (on a
        // `☑ Bread` line Heron says unselected).
        if model_checkbox {
            if let Some(Mark::Item { checked, .. }) = marks.first() {
                regions[i].label = if *checked {
                    "checkbox_selected"
                } else {
                    "checkbox_unselected"
                };
            }
            continue;
        }
        if !marks.iter().any(|m| matches!(m, Mark::Item { .. })) {
            continue;
        }
        // Group rows: a marked row opens an item; an unmarked one continues
        // the open group (the leading group keeps the region's own label).
        let mut pieces: Vec<Region> = Vec::new();
        for ((row, _), mark) in rows.iter().zip(&marks) {
            match mark {
                Mark::Item { square, checked } => {
                    let (mut l, mut t, mut r, mut b) = *row;
                    if let Some(s) = square.map(|k| &boxes[k]) {
                        (l, t, r, b) = (l.min(s.l), t.min(s.t), r.max(s.r), b.max(s.b));
                    }
                    pieces.push(Region {
                        label: if *checked {
                            "checkbox_selected"
                        } else {
                            "checkbox_unselected"
                        },
                        score: region.score,
                        l,
                        t,
                        r,
                        b,
                    });
                }
                Mark::Plain => pieces.push(Region {
                    l: row.0,
                    t: row.1,
                    r: row.2,
                    b: row.3,
                    ..region.clone()
                }),
                Mark::Continue => match pieces.last_mut() {
                    Some(p) => {
                        p.l = p.l.min(row.0);
                        p.t = p.t.min(row.1);
                        p.r = p.r.max(row.2);
                        p.b = p.b.max(row.3);
                    }
                    None => pieces.push(Region {
                        l: row.0,
                        t: row.1,
                        r: row.2,
                        b: row.3,
                        ..region.clone()
                    }),
                },
            }
        }
        let mut pieces = pieces.into_iter();
        if let Some(first) = pieces.next() {
            regions[i] = first;
        }
        appended.extend(pieces);
    }
    regions.extend(appended);
}

/// docling's regular-cluster refinement after cell assignment
/// (`LayoutPostprocessor._process_regular_clusters`, #419), run once the page's
/// cells are final and before reading order:
///
/// 1. every regular region's box becomes the union of the cells it claimed
///    (`_adjust_cluster_bboxes` — a regular cluster's bbox *is* its cells'
///    bbox; a table's is the union with the model box, and pictures keep
///    theirs, so neither is touched here);
/// 2. a regular region that claimed no cell is dropped (`keep_empty_clusters`
///    is off; a `formula` is kept, as upstream keeps it);
/// 3. an orphan text region (`score == 0.0`, from [`add_orphan_regions`]) that
///    now sits > 0.8 inside another regular region's fitted box is folded into
///    it (`_remove_overlapping_clusters` at containment 0.8, the larger box
///    winning the group) — up to three rounds, like upstream's loop.
///
/// Why it matters: the layout model's box can end partway through a line. That
/// line fails the 0.2 claim and becomes an orphan — recoverable — but the
/// *model* box still overlaps the orphan's line by a few points, so the
/// reading-order graph, which links only strictly-above pairs, gets no edge
/// between them and may emit the next paragraph first, stranding the line
/// after the paragraph it belongs in (1540 of 6050 text blocks on the #419
/// book began mid-sentence). Fitted to its cells, the box ends on a line
/// boundary and the orphan slots in between; an orphan the fitted box
/// swallows joins the paragraph outright. Cell assignment is untouched: a
/// region's fitted box contains every cell it claimed, so
/// [`region_texts_exclusive`] hands it the same cells afterwards.
///
/// A page with no cells yet (a scan before OCR) is left alone: dropping every
/// text region for want of cells would be wrong, and the OCR paths call this
/// again once the cells exist.
pub fn fit_regions_to_cells(regions: &mut Vec<Region>, cells: &[TextCell]) {
    if !cells.iter().any(|c| !c.text.trim().is_empty()) {
        return;
    }
    for _ in 0..3 {
        let owned = assign_cells(regions, cells);
        let mut fitted: Vec<Region> = Vec::with_capacity(regions.len());
        for (r, own) in regions.iter().zip(&owned) {
            if !claims_cells(r) {
                fitted.push(r.clone());
                continue;
            }
            if own.is_empty() {
                if r.label == "formula" {
                    fitted.push(r.clone());
                }
                continue;
            }
            let mut f = r.clone();
            f.l = own
                .iter()
                .map(|&i| cells[i].l)
                .fold(f32::INFINITY, f32::min);
            f.t = own
                .iter()
                .map(|&i| cells[i].t)
                .fold(f32::INFINITY, f32::min);
            f.r = own
                .iter()
                .map(|&i| cells[i].r)
                .fold(f32::NEG_INFINITY, f32::max);
            f.b = own
                .iter()
                .map(|&i| cells[i].b)
                .fold(f32::NEG_INFINITY, f32::max);
            fitted.push(f);
        }
        let mut changed = fitted.len() != regions.len();
        // Fold orphans into the regular region whose fitted box holds them.
        let mut drop = vec![false; fitted.len()];
        for i in 0..fitted.len() {
            let o = &fitted[i];
            if !(o.score == 0.0 && o.label == "text") {
                continue;
            }
            let oa = area(o.l, o.t, o.r, o.b).max(1.0);
            let mut best: Option<(usize, f32)> = None;
            for (j, r) in fitted.iter().enumerate() {
                if j == i || drop[j] || r.score == 0.0 || !claims_cells(r) {
                    continue;
                }
                let ov = inter(r, o.l, o.t, o.r, o.b) / oa;
                if ov > 0.8 && best.is_none_or(|(_, b)| ov > b) {
                    best = Some((j, ov));
                }
            }
            if let Some((j, _)) = best {
                let (l, t, r, b) = (o.l, o.t, o.r, o.b);
                let host = &mut fitted[j];
                host.l = host.l.min(l);
                host.t = host.t.min(t);
                host.r = host.r.max(r);
                host.b = host.b.max(b);
                drop[i] = true;
                changed = true;
            }
        }
        let mut drop = drop.into_iter();
        fitted.retain(|_| !drop.next().expect("aligned"));
        *regions = fitted;
        if !changed {
            break;
        }
    }
}

/// Join a prefiltered cell list into the region's text (docling's
/// `sanitize_text` over the sanitizer's cell order).
fn cells_text(inside: Vec<&TextCell>) -> String {
    let mut t = cells_text_keep_wrap(inside);
    if t.ends_with('\u{ad}') {
        t.pop();
    }
    t
}

/// [`cells_text`] keeping a trailing soft hyphen (see [`clean_text_keep_wrap`]).
fn cells_text_keep_wrap(inside: Vec<&TextCell>) -> String {
    // docling orders a cluster's cells by their docling-parse cell index
    // alone (`LayoutPostprocessor._sort_cells`: `sorted(cells, key=c.index)`)
    // — the sanitizer's output order, which our `cells` slice already is.
    // No geometric re-sort: normal_4pages' big section numerals paint
    // *after* their heading text, and docling's `## 들어가며 1` (numeral
    // last) only falls out of pure index order — a band sort dragged the
    // numeral to the front. The overlap-grouped line restore this replaced
    // measured strictly worse on the corpus (it fixed nothing the index
    // order broke, and broke the numerals).
    let joined = {
        // docling's `PageAssembleModel.sanitize_text`, ported verbatim over the
        // parse-index-ordered lines: append a separating space to a line —
        // unless it ends with `-`. A dash-ending line whose last word and the
        // next line's first word are both alphanumeric is a wrapped word: the
        // dash is dropped and the lines fuse (`platforms-` + `reflects` →
        // `platformsreflects`, `pp. 545-` + `561` → `545561`). Any other
        // dash-ending line — e.g. the *bare* `-` cell a superscript ORCID or an
        // inline `–` bullet splits off (its word list is empty, so the fuse
        // test fails) — keeps its dash and still takes no trailing space:
        // `[0000` `-` `0002` joins as docling's `[0000 -0002`, and the OTSL
        // list's `-` + `"C" cell -` + `a new table cell` collapses to
        // `-"C" cell a new table cell`. Our cells still carry the raw dash
        // family (docling-parse normalizes to `-` before this; clean_text does
        // it after), so the endswith test matches them all.
        let texts: Vec<&str> = inside
            .iter()
            .map(|c| c.text.trim())
            // Skip whitespace-only cells (a justified line's trailing space
            // glyph): an empty line would double the separator.
            .filter(|t| !t.is_empty())
            .collect();
        let last_word_alnum = |s: &str| {
            s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .rfind(|w| !w.is_empty())
                .is_some_and(|w| w.chars().all(char::is_alphanumeric))
        };
        let first_word_alnum = |s: &str| {
            s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .find(|w| !w.is_empty())
                .is_some_and(|w| w.chars().all(char::is_alphanumeric))
        };
        let mut out = String::new();
        for (i, t) in texts.iter().enumerate() {
            if i > 0 {
                let prev = texts[i - 1];
                let dashish = matches!(
                    prev.chars().last(),
                    Some(
                        '-' | '\u{2010}'
                            | '\u{2011}'
                            | '\u{2012}'
                            | '\u{2013}'
                            | '\u{2014}'
                            | '\u{2015}'
                            | '\u{2212}'
                    )
                );
                // docling#4052 (2.122): a dash only splits a word when it is
                // *attached* to one — the character before it is alphanumeric.
                // A dash that follows whitespace (a separator dash, a bullet
                // marker, a wrapped `-prefixed` token, the bare `-` cell an
                // ORCID splits off) is a literal character: it is kept and the
                // lines join with the ordinary space.
                let attached = prev.chars().rev().nth(1).is_some_and(char::is_alphanumeric);
                if dashish && attached && starts_with_conjunction(t) {
                    // `Warte-` / `und Behaltefristen`: an elliptical compound
                    // keeps its dash and the line break's space (#920).
                    out.push(' ');
                } else if dashish && attached {
                    if last_word_alnum(prev) && first_word_alnum(t) {
                        out.pop(); // wrapped word: fuse without the dash
                    }
                    // an attached dash never takes a separating space
                } else {
                    out.push(' ');
                }
            }
            out.push_str(t);
        }
        out
    };
    clean_text_keep_wrap(&joined)
}

/// Tighten the spaces pdfium leaves around tight punctuation in a code line
/// (`console .log` → `console.log`, `add (3 , 5)` → `add(3, 5)`), matching
/// docling-parse's source spacing.
fn tighten_code_punct(s: &str) -> String {
    s.replace(" .", ".")
        .replace(" ,", ",")
        .replace(" ;", ";")
        .replace(" )", ")")
        .replace(" (", "(")
}

/// Assemble a **code** region's text with its line structure preserved.
///
/// Unlike [`region_text`] — which joins every cell with a single space, the right
/// thing for prose reflow — a code block's line breaks and indentation are
/// significant. The `code_cells` are already one physical source line each
/// (grouped space-glyph-only, so monospace runs keep their spacing), so this:
///
/// 1. groups the cells into vertical line bands and orders them top→bottom,
///    left→right;
/// 2. joins the lines with `\n` (rather than spaces), keeping the carriage
///    returns; and
/// 3. reconstructs each line's leading indentation from its left offset, in units
///    of the block's estimated monospace character width, so nesting survives.
///
/// Typography is normalized per line via [`clean_text`] (smart quotes, dashes,
/// ellipsis), which never merges lines. Returns an empty string if the region has
/// no code cells (the caller falls back to the prose text).
fn code_region_text(region: &Region, cells: &[TextCell]) -> String {
    let mut inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .filter(|c| !c.text.trim().is_empty())
        .collect();
    if inside.is_empty() {
        return String::new();
    }

    // Quantize the top edge into ~line bands (like `region_text`), then order the
    // cells by band (top→bottom) and, within a band, by left edge.
    let band = inside
        .iter()
        .map(|c| (c.b - c.t).abs())
        .fold(0.0f32, f32::max)
        .max(1.0);
    let line_of = |c: &TextCell| (c.t / band).round() as i64;
    inside.sort_by_key(|c| (line_of(c), (c.l * 10.0) as i64));

    // Estimate one monospace character's width (total ink width / total glyphs) to
    // convert a line's left offset into a count of leading spaces. Measured over
    // all lines so a single short line can't skew it.
    let (mut total_w, mut total_chars) = (0.0f32, 0usize);
    for c in &inside {
        let n = c.text.trim().chars().count();
        if n > 0 {
            total_w += (c.r - c.l).max(0.0);
            total_chars += n;
        }
    }
    let char_w = if total_chars > 0 {
        (total_w / total_chars as f32).max(1.0)
    } else {
        1.0
    };
    // The block's own left margin is the zero-indent baseline.
    let base_l = inside.iter().map(|c| c.l).fold(f32::INFINITY, f32::min);

    let mut lines: Vec<String> = Vec::new();
    let mut cur: Option<i64> = None;
    for c in &inside {
        // Tighten pdfium's spaced punctuation per line (on the trimmed content, so
        // the reconstructed leading indentation is never nibbled).
        let text = tighten_code_punct(&clean_text(c.text.trim()));
        if Some(line_of(c)) == cur {
            // A second cell sharing this band (rare — e.g. split columns): keep it
            // on the same source line, separated by a space.
            if let Some(last) = lines.last_mut() {
                last.push(' ');
                last.push_str(&text);
            }
            continue;
        }
        let indent = ((c.l - base_l) / char_w).round().max(0.0) as usize;
        lines.push(format!("{}{}", " ".repeat(indent), text));
        cur = Some(line_of(c));
    }
    lines.join("\n")
}

/// Reconstruct a table's grid geometrically from the text cells inside its
/// region: cluster cells into rows (by vertical centre) and columns (by clustered
/// left edges), then place each cell. A model-free stand-in for TableFormer that
/// recovers grid-aligned tables from the precise PDF text layer (it does not
/// resolve row/column spans).
pub fn reconstruct_table(region: &Region, cells: &[TextCell]) -> Vec<Vec<String>> {
    let mut inside: Vec<&TextCell> = cells
        .iter()
        .filter(|c| {
            let ca = area(c.l, c.t, c.r, c.b).max(1.0);
            inter(region, c.l, c.t, c.r, c.b) / ca > 0.5
        })
        .collect();
    if inside.is_empty() {
        return Vec::new();
    }
    inside.sort_by(|a, b| a.t.total_cmp(&b.t));

    // Rows: consecutive cells whose vertical centre is within ~0.7 line height.
    let mut rows: Vec<(f32, Vec<&TextCell>)> = Vec::new();
    for c in &inside {
        let cyc = (c.t + c.b) / 2.0;
        let lh = (c.b - c.t).abs().max(1.0);
        if let Some((ryc, row)) = rows.last_mut() {
            if (cyc - *ryc).abs() < lh * 0.7 {
                row.push(c);
                continue;
            }
        }
        rows.push((cyc, vec![c]));
    }

    // Columns: cluster left edges (merge those within a tolerance).
    let tol = {
        let mut hs: Vec<f32> = inside.iter().map(|c| (c.b - c.t).abs()).collect();
        hs.sort_by(f32::total_cmp);
        hs[hs.len() / 2].max(4.0) * 1.5
    };
    let mut lefts: Vec<f32> = inside.iter().map(|c| c.l).collect();
    lefts.sort_by(f32::total_cmp);
    let mut col_starts: Vec<f32> = Vec::new();
    for l in lefts {
        if col_starts.last().is_none_or(|&last| l - last > tol) {
            col_starts.push(l);
        }
    }
    let ncols = col_starts.len().max(1);
    let col_of = |l: f32| -> usize {
        col_starts
            .iter()
            .rposition(|&s| l + tol * 0.5 >= s)
            .unwrap_or(0)
            .min(ncols - 1)
    };

    let mut grid = Vec::with_capacity(rows.len());
    for (_, mut row) in rows {
        row.sort_by(|a, b| a.l.total_cmp(&b.l));
        let mut cols = vec![String::new(); ncols];
        for c in row {
            let ci = col_of(c.l);
            // Strip the wrap-hyphen control char so it never lands in a cell.
            let t = c.text.trim().replace(['\u{2}', '\u{ad}'], "");
            if cols[ci].is_empty() {
                cols[ci] = t;
            } else {
                cols[ci].push(' ');
                cols[ci].push_str(&t);
            }
        }
        grid.push(cols);
    }
    grid
}

/// Does the geometric reconstruction of a table look trustworthy enough to use
/// as-is, instead of paying for TableFormer?
///
/// [`reconstruct_table`] derives columns by clustering cell **left edges**. On a
/// clean grid that is exact, but when a column's entries are not left-aligned
/// (or the OCR boxes wobble) the clustering splits one real column into several,
/// and the result is a wide, mostly-empty grid — the "spurious empty columns"
/// failure TableFormer exists to fix.
///
/// Two symptoms separate the two cases, and both are properties of the grid
/// alone (no model needed):
/// * **density** — a real table is mostly full; a split-up one is mostly holes;
/// * **thin columns** — a column carrying at most one entry across several rows
///   is almost always a split artefact rather than a real column.
///
/// Deliberately conservative: it answers `true` only for grids that are plainly
/// well-formed, so the expensive path stays the default whenever there is doubt.
/// A caller that skips TableFormer on `true` trades no quality for the time.
pub fn geometric_table_is_reliable(rows: &[Vec<String>]) -> bool {
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
    // Fewer than two columns is not a grid this heuristic can vouch for: it is
    // exactly the shape a collapsed table takes, and TableFormer may recover
    // real structure from it.
    if rows.len() < 2 || ncols < 2 {
        return false;
    }
    let filled = |c: &String| !c.trim().is_empty();
    let total = rows.len() * ncols;
    let full = rows.iter().flatten().filter(|c| filled(c)).count();
    if (full as f32) < MIN_TABLE_FILL * total as f32 {
        return false;
    }
    // A column used by at most one row, when there are rows enough to tell.
    if rows.len() >= 3 {
        for ci in 0..ncols {
            let used = rows
                .iter()
                .filter(|r| r.get(ci).is_some_and(filled))
                .count();
            if used <= 1 {
                return false;
            }
        }
    }
    true
}

/// Share of a geometric grid's cells that must carry text for it to be trusted
/// without TableFormer. Chosen well above the density a left-edge split
/// produces (those land nearer a third) and below what a genuine table with a
/// few blank cells reaches.
const MIN_TABLE_FILL: f32 = 0.6;

/// The union bbox of the text cells assigned to a region (same >50%-overlap
/// rule as [`region_text`]), or `None` when no cell lands in it. docling's
/// LayoutPostprocessor shrinks a regular cluster's bbox to its cells, and the
/// enrichment crops are taken from that cell-tight box — cropping the raw
/// detector box instead hands the VLM surrounding chrome (e.g. the `Listing N:`
/// caption under a code block) that changes its output.
pub fn region_cell_bbox(region: &Region, cells: &[TextCell]) -> Option<[f32; 4]> {
    let mut bbox: Option<[f32; 4]> = None;
    for c in cells {
        let ca = area(c.l, c.t, c.r, c.b).max(1.0);
        if inter(region, c.l, c.t, c.r, c.b) / ca <= 0.5 {
            continue;
        }
        bbox = Some(match bbox {
            None => [c.l, c.t, c.r, c.b],
            Some([l, t, r, b]) => [l.min(c.l), t.min(c.t), r.max(c.r), b.max(c.b)],
        });
    }
    bbox
}

/// One region's enrichment-model result, produced by the pipeline's opt-in
/// passes (issue #76) and applied during assembly.
#[derive(Debug, Clone)]
pub enum Enrichment {
    /// DocumentPictureClassifier predictions, descending confidence.
    PictureClasses(Vec<PictureClass>),
    /// CodeFormulaV2 output for a `code` region: the rewritten source text and
    /// the `<_language_>` prefix (when the model emitted one).
    Code {
        language: Option<String>,
        text: String,
    },
    /// CodeFormulaV2 output for a `formula` region: the decoded LaTeX.
    Formula { latex: String },
}

/// Crop a region (page points, already expanded by the caller if needed) from
/// the rendered page image and resize it to `target_scale` pixels per point —
/// the enrichment-model equivalent of docling's
/// `page.get_image(scale=…, cropbox=…)`, sourced from the existing
/// [`crate::pdfium_backend::RENDER_SCALE`] render instead of a fresh pdfium
/// pass (the page bitmap is already the exact docling render at scale 2).
#[cfg(feature = "ml")]
pub fn crop_region_scaled(page: &PdfPage, bbox: [f32; 4], target_scale: f32) -> Option<RgbImage> {
    debug_assert!(
        !page.image_pending(),
        "a crop reached an unrendered page bitmap (#897)"
    );
    let s = page.scale;
    let [l, t, r, b] = bbox;
    let (iw, ih) = (page.image.width(), page.image.height());
    let x = (l * s).max(0.0) as u32;
    let y = (t * s).max(0.0) as u32;
    if x >= iw || y >= ih {
        return None;
    }
    let w = (((r - l.max(0.0)) * s) as u32).min(iw - x);
    let h = (((b - t.max(0.0)) * s) as u32).min(ih - y);
    if w == 0 || h == 0 {
        return None;
    }
    let crop = image::imageops::crop_imm(&page.image, x, y, w, h).to_image();
    // docling renders the crop at `target_scale` directly; from the scale-2
    // page render that is a resize to the same pixel geometry
    // (`round(width_points * scale)`, PIL's BICUBIC ≙ CatmullRom).
    let tw = ((w as f32 / s) * target_scale).round().max(1.0) as u32;
    let th = ((h as f32 / s) * target_scale).round().max(1.0) as u32;
    if (tw, th) == (w, h) {
        return Some(crop);
    }
    Some(image::imageops::resize(
        &crop,
        tw,
        th,
        image::imageops::FilterType::CatmullRom,
    ))
}

/// Resample `img` (rendered at `from` px/pt, covering `w_pt`×`h_pt` points)
/// to `to` px/pt — docling's `round(points * scale)` pixel geometry, PIL's
/// BICUBIC ≙ CatmullRom. Unchanged when the geometry already matches.
#[cfg(feature = "ocr-prep")]
fn rescale(img: RgbImage, w_pt: f32, h_pt: f32, to: f32) -> RgbImage {
    let tw = (w_pt * to).round().max(1.0) as u32;
    let th = (h_pt * to).round().max(1.0) as u32;
    if (tw, th) == img.dimensions() {
        return img;
    }
    image::imageops::resize(&img, tw, th, image::imageops::FilterType::CatmullRom)
}

/// Encode `img` as a PNG [`PictureImage`] rendered at `scale` px/pt.
#[cfg(feature = "ocr-prep")]
fn png_image(img: &RgbImage, scale: f32) -> Option<PictureImage> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(PictureImage {
        mimetype: "image/png".into(),
        width: img.width(),
        height: img.height(),
        data: buf.into_inner(),
        dpi: PictureImage::dpi_for_scale(scale),
        display_size: None,
    })
}

/// The whole page render as docling's `PageItem.image` (#520): at `scale`
/// px/pt (`None` = the render's own), `None` when the page has no bitmap.
#[cfg(feature = "ocr-prep")]
pub fn page_image(page: &PdfPage, scale: Option<f32>) -> Option<PictureImage> {
    #[cfg(feature = "ml")]
    debug_assert!(
        !page.image_pending(),
        "a crop reached an unrendered page bitmap (#897)"
    );
    if page.image.width() == 0 || page.image.height() == 0 || page.scale <= 0.0 {
        return None;
    }
    let scale = scale.unwrap_or(page.scale);
    let img = rescale(page.image.clone(), page.width, page.height, scale);
    png_image(&img, scale)
}

/// Crop a layout region from the rendered page image and encode it as PNG (the
/// figure bytes docling stores on a `PictureItem`). Region coordinates are page
/// points; the image is rendered at `page.scale` and resampled to `scale`
/// px/pt when one is given (docling's `images_scale`, #520). The image's `dpi`
/// is 72·scale (#519).
#[cfg(feature = "ocr-prep")]
fn crop_region(page: &PdfPage, region: &Region, scale: Option<f32>) -> Option<PictureImage> {
    // `use_embedded_picture_images` (#1007): the one safe image the region
    // holds, at its native size and docling's `dpi` of 72 — not resampled to
    // `images_scale`, which only sizes render crops (docling likewise).
    #[cfg(feature = "render")]
    if let Some(img) = crate::embedded::match_region(
        &page.embedded_images,
        &page.cells,
        [region.l, region.t, region.r, region.b],
    ) {
        return png_image(img, 1.0);
    }
    #[cfg(feature = "ml")]
    debug_assert!(
        !page.image_pending(),
        "a crop reached an unrendered page bitmap (#897)"
    );
    let s = page.scale;
    let (iw, ih) = (page.image.width(), page.image.height());
    let x = (region.l * s).max(0.0) as u32;
    let y = (region.t * s).max(0.0) as u32;
    if x >= iw || y >= ih {
        return None;
    }
    let w = (((region.r - region.l) * s) as u32).min(iw - x);
    let h = (((region.b - region.t) * s) as u32).min(ih - y);
    if w == 0 || h == 0 {
        return None;
    }
    let sub = image::imageops::crop_imm(&page.image, x, y, w, h).to_image();
    match scale {
        Some(to) if (to - s).abs() > f32::EPSILON => {
            // The crop's own point extent (the pixel box, back in points), so
            // the resampled geometry is `round(points * scale)`.
            let img = rescale(sub, w as f32 / s, h as f32 / s, to);
            png_image(&img, to)
        }
        _ => png_image(&sub, s),
    }
}

/// A `text` region that *says* it is a caption — `Table 5: …`, `Figure 2.
/// …`, `Fig. 3: …` — and sits right next to a graphic in reading order is one
/// (a docling.rs extension, `docs/PDF_CONFORMANCE.md`): the layout model
/// labels a long caption block `text` now and then (2206.01062's four-line
/// Table 5 caption: `text` 0.99 here, `caption` in docling's own run), and a
/// mislabelled caption stays a loose paragraph while its table goes
/// uncaptioned. The gate is the label's own pattern — the word, a number,
/// then `:` or `.` — so a paragraph that merely opens with `Table 5 shows`
/// is left alone, plus the geometry docling's matcher wants: the graphic is
/// the region's neighbour in reading order, overlaps it horizontally and
/// stands within 25 pt above or below. Relabelling before
/// [`pair_graphic_captions`] lets the regular matcher bind it.
fn promote_labelled_captions(regions: &mut [Region], cells: &[TextCell]) {
    let body: Vec<usize> = (0..regions.len())
        .filter(|&i| !is_skipped(regions[i].label))
        .collect();
    let mut promote = Vec::new();
    for (pos, &i) in body.iter().enumerate() {
        let r = &regions[i];
        if r.label != "text" || !is_caption_label(&region_text(r, cells)) {
            continue;
        }
        let beside = |g: &Region| {
            is_graphic(g.label) && g.r.min(r.r) - g.l.max(r.l) > 0.0 && {
                let gap = if r.t >= g.b {
                    r.t - g.b
                } else if g.t >= r.b {
                    g.t - r.b
                } else {
                    f32::MAX
                };
                gap <= 25.0
            }
        };
        let prev = pos.checked_sub(1).map(|p| &regions[body[p]]);
        let next = body.get(pos + 1).map(|&n| &regions[n]);
        if prev.is_some_and(beside) || next.is_some_and(beside) {
            promote.push(i);
        }
    }
    for i in promote {
        regions[i].label = "caption";
    }
}

/// `Table 5:`, `Figure 2.1.`, `Fig. 3:`, `Listing 4:` — a caption label: the
/// word, whitespace, a number (dotted or lettered: `2.1`, `3a`), then `:` or
/// `.`, as the publishers' styles print it. Case-sensitive on purpose.
fn is_caption_label(text: &str) -> bool {
    let t = text.trim_start();
    let Some(rest) = ["Table", "Figure", "Fig.", "Listing"]
        .iter()
        .find_map(|w| t.strip_prefix(w))
    else {
        return false;
    };
    let rest = rest.trim_start();
    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let num_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .unwrap_or(rest.len());
    let num = rest[..num_end].trim_end_matches('.');
    let after = &rest[num.len()..];
    after.starts_with(':') || after.starts_with('.')
}

/// docling's `GRAPHIC_LABELS` (`reading_order_rb.py`): the elements a
/// `caption` can be attached to. `document_index` rides along as table-like
/// (#265) so an index's title binds the way a table's does.
fn is_graphic(label: &str) -> bool {
    is_table_like(label) || matches!(label, "picture" | "code")
}

/// docling's `_shortest_box_gap`: the shortest distance between two boxes,
/// 0 once they touch or overlap — along either axis whatever the union has
/// left over once both boxes are laid down, combined as the hypotenuse.
fn shortest_box_gap(a: &Region, b: &Region) -> f32 {
    let dx = ((a.r.max(b.r) - a.l.min(b.l)) - (a.r - a.l) - (b.r - b.l)).max(0.0);
    let dy = ((a.b.max(b.b) - a.t.min(b.t)) - (a.b - a.t) - (b.b - b.t)).max(0.0);
    dx.hypot(dy)
}

/// Pair each graphic (`picture`, `table`/`document_index`, `code`) with its
/// `caption` region (#265, #813): a port of docling's
/// `ReadingOrderPredictor._find_to_captions` — `_rank_caption_candidates` +
/// `_match_caption` — over the page's regions in reading order. Returns, per
/// region, the index of the caption it owns.
///
/// A caption reaches the graphics in the **unbroken run** of graphics on
/// either side of it in reading order — above *and* below (#813: the
/// geometric predecessor only looked below a picture, so a `Figure N:` label
/// printed above its figure never bound), never across intervening text, and
/// never across the gutter of a two-column page however close the boxes are.
/// The candidates rank by the gap each leaves on the page (nearest first,
/// equal gaps to the preceding graphic), not by position in the run.
/// Graphics are then matched to captions by augmenting paths rather than
/// best-first: a caption with a second choice gives way to one that has none,
/// so a figure between two captions and a figure with one do not end up
/// sharing the former while the latter stays orphaned.
///
/// Furniture (page headers/footers, form chrome) is not part of docling's
/// body-element sequence here, so it neither bonds nor breaks a run. The
/// matched caption is consumed from its own reading-order slot and rides on
/// its graphic — docling's `captions` ref: Markdown prints it before a
/// picture or table and after a code block, JSON emits the `$ref`, DocLang
/// the `<caption>`.
fn pair_graphic_captions(regions: &[Region]) -> Vec<Option<usize>> {
    let body: Vec<usize> = (0..regions.len())
        .filter(|&i| !is_skipped(regions[i].label))
        .collect();
    // `_rank_caption_candidates`: caption index → its graphics, nearest first.
    let mut captions: Vec<usize> = Vec::new();
    let mut preferred: Vec<Vec<usize>> = vec![Vec::new(); regions.len()];
    for (pos, &ci) in body.iter().enumerate() {
        if regions[ci].label != "caption" {
            continue;
        }
        let graphic = |&&i: &&usize| is_graphic(regions[i].label);
        let preceding = body[..pos].iter().rev().take_while(graphic);
        let following = body[pos + 1..].iter().take_while(graphic);
        // Tagged 0 preceding / 1 following so equal gaps go to the graphic
        // above; a stable sort keeps run order among equals on one side.
        let mut ranked: Vec<(f32, u8, usize)> = preceding
            .map(|&g| (shortest_box_gap(&regions[ci], &regions[g]), 0, g))
            .chain(following.map(|&g| (shortest_box_gap(&regions[ci], &regions[g]), 1, g)))
            .collect();
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        preferred[ci] = ranked.into_iter().map(|(_, _, g)| g).collect();
        captions.push(ci);
    }
    // `_match_caption`, caption by caption in reading order: graphic → caption.
    let mut matched: Vec<Option<usize>> = vec![None; regions.len()];
    for &caption in &captions {
        if let Some(next) = augment_caption_match(caption, &preferred, &matched) {
            matched = next;
        }
    }
    // Side-by-side figures (a docling.rs extension, `docs/PDF_CONFORMANCE.md`):
    // a row of pictures reads as a run of graphics followed (or preceded) by
    // the run of their captions, and upstream's run rule serves only the
    // first caption — for the others the caption before them breaks the run,
    // and docling leaves them loose. A caption still loose whose only
    // separation from a graphic run is other captions takes the nearest
    // unclaimed graphic of that run it sits under or over (horizontal
    // overlap, within three line heights — the old geometric rule).
    for (pos, &ci) in body.iter().enumerate() {
        let is_caption = |i: &usize| regions[*i].label == "caption";
        if !is_caption(&ci) || matched.contains(&Some(ci)) {
            continue;
        }
        let c = &regions[ci];
        let line_h = (c.b - c.t).abs().max(1.0);
        let graphic = |i: &usize| is_graphic(regions[*i].label);
        let before = body[..pos]
            .iter()
            .rev()
            .skip_while(|i| is_caption(i))
            .take_while(|i| graphic(i));
        let after = body[pos + 1..]
            .iter()
            .skip_while(|i| is_caption(i))
            .take_while(|i| graphic(i));
        let best = before
            .chain(after)
            .filter(|&&g| matched[g].is_none())
            .filter_map(|&g| {
                let p = &regions[g];
                let h_overlap = p.r.min(c.r) - p.l.max(c.l);
                let gap = (c.t - p.b).max(p.t - c.b); // below or above the graphic
                (h_overlap > 0.0 && gap > -line_h && gap < line_h * 3.0).then_some((g, gap))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((g, _)) = best {
            matched[g] = Some(ci);
        }
    }
    matched
}

/// docling's `_match_caption`: a new matching in which `caption` owns one of
/// its graphics, displacing an earlier caption when that one can rehouse
/// itself; `None` when no graphic can be freed up. The displacement chain is
/// walked on an explicit stack (it can grow as long as the page): each frame
/// is a claimant and the graphics it has left to try, and the moves in
/// `chain` take effect only once it reaches a graphic nobody holds. `seen`
/// keeps a chain from revisiting a graphic.
fn augment_caption_match(
    caption: usize,
    preferred: &[Vec<usize>],
    matched: &[Option<usize>],
) -> Option<Vec<Option<usize>>> {
    let mut seen = vec![false; matched.len()];
    // (claimant, next candidate to try) frames; (graphic, claimant) moves.
    let mut stack: Vec<(usize, usize)> = vec![(caption, 0)];
    let mut chain: Vec<(usize, usize)> = Vec::new();
    while let Some(frame) = stack.last_mut() {
        let (claimant, cursor) = *frame;
        let candidates = &preferred[claimant];
        let Some(off) = candidates[cursor..].iter().position(|&g| !seen[g]) else {
            // Out of options: undo the move that got here and let the
            // caption below resume its own search.
            stack.pop();
            chain.pop();
            continue;
        };
        let graphic = candidates[cursor + off];
        frame.1 = cursor + off + 1;
        seen[graphic] = true;
        chain.push((graphic, claimant));
        match matched[graphic] {
            None => {
                let mut next = matched.to_vec();
                for &(g, c) in &chain {
                    next[g] = Some(c);
                }
                return Some(next);
            }
            // The graphic is taken; let its caption look for another one.
            Some(holder) => stack.push((holder, 0)),
        }
    }
    None
}

/// Assemble one page from its (already overlap-resolved) layout regions and
/// text cells.
/// Normalize a layout region (page points, top-left origin) to DocLang's 0–511
/// location grid: `clamp(round(512 · coord / page_dim), 0, 511)`, per axis,
/// order `[x0, y0, x1, y1]`. Mirrors docling_core's
/// `_doclang_utils._create_location_tokens_for_bbox` (resolution 512) so the
/// emitted `<location>` tokens line up with the Python groundtruth. Our heron
/// cluster boxes match docling's to within ~1 grid unit; the residual (mainly
/// the aspect-ratio-stretch vs letterbox preprocessing difference) is absorbed
/// by the conformance harness's geometry tolerance.
fn norm_loc(region: &Region, page_w: f32, page_h: f32) -> [u16; 4] {
    let q = |v: f32, dim: f32| -> u16 {
        if dim <= 0.0 {
            return 0;
        }
        let g = (512.0 * (v as f64) / (dim as f64)).round() as i64;
        g.clamp(0, 511) as u16
    };
    [
        q(region.l, page_w),
        q(region.t, page_h),
        q(region.r, page_w),
        q(region.b, page_h),
    ]
}

/// Wrap a node in its layout provenance so the DocLang serializer emits the four
/// `<location>` tokens as the element's head (Markdown/JSON render `inner`
/// unchanged).
fn located(loc: [u16; 4], inner: Node) -> Node {
    Node::Located {
        location: loc,
        inner: Box::new(inner),
    }
}

/// Stamp the real 1-based page number onto a page's leading marker (see
/// [`assemble_page`], which emits it with `page_no: 0` because only the
/// document-level collector knows the true index — `--pages` windows shift it).
pub fn stamp_page_no(nodes: &mut [Node], page_no: usize) {
    if let Some(Node::PageInfo { page_no: p, .. }) = nodes.first_mut() {
        *p = page_no;
    }
}

/// Text-layer path only (#984): put a page's running head back first. With no
/// layout model nothing is labelled `page_header`, and docling's rule-based
/// reading order treats a short line far to one side (the EU Official
/// Journal's right-aligned `02014R0537 — DE — … — 7`) as a separate head of
/// its own: the body column under the left-hand `▼B` marker is read through
/// first and the header lands at the end of its own page — in the text
/// export, after the page's body and ahead of the next page's. The line that
/// lies entirely above every other cell of the page is the first thing on it
/// whatever the graph says, so it is moved to the front of the page's nodes
/// (after the page marker). Pages whose top line is already first (the usual
/// case) and pages whose top line shares its rows with other text (columns)
/// are untouched.
pub fn float_running_head(nodes: &mut [Node], cells: &[TextCell]) {
    let mut lines = cells.iter().filter(|c| !c.text.trim().is_empty());
    let Some(first) = lines.next() else { return };
    let mut top = first;
    let mut rest_t = f32::INFINITY;
    for c in lines {
        if c.t < top.t {
            rest_t = rest_t.min(top.t);
            top = c;
        } else {
            rest_t = rest_t.min(c.t);
        }
    }
    if top.b > rest_t {
        return;
    }
    let key = |t: &str| -> String { t.chars().filter(|c| c.is_alphanumeric()).collect() };
    let want = key(&clean_text(&top.text));
    if want.is_empty() {
        return;
    }
    let body = usize::from(matches!(nodes.first(), Some(Node::PageInfo { .. })));
    let Some(at) = nodes
        .iter()
        .position(|n| as_paragraph(n).is_some_and(|t| key(t) == want))
    else {
        return;
    };
    // A margin page number already floated to the front (`is_page_number`,
    // docling parity) keeps the first slot.
    let numbered = nodes.get(body).and_then(as_paragraph).is_some_and(|t| {
        let t = t.trim();
        !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())
    });
    if !numbered && at > body && !nodes[body..at].iter().all(is_merge_trailer) {
        nodes[body..=at].rotate_right(1);
    }
}

/// A dense table grid plus its first-class cells (#240): `rows` is the text
/// grid every serializer renders (spans replicate their anchor's text);
/// `cells` are the docling-parity per-cell records (text, page-point bbox,
/// span rectangle, OTSL header roles). Produced by the TableFormer paths
/// (`tf_core`); lives in this always-compiled module so the pure-text (wasm
/// `pdf-text`) build sees the type.
#[derive(Clone, Debug)]
pub struct TableGrid {
    pub rows: Vec<Vec<String>>,
    pub cells: Vec<docling_core::TableCell>,
}

/// docling's `_RICH_CELL_PICTURE_COVERAGE_THRESHOLD`.
const RICH_CELL_PICTURE_COVERAGE: f32 = 0.8;

/// docling `ReadingOrderModel._match_table_pictures` (#3906, 2.118.1): every
/// picture ≥ 80 % inside a TableFormer-structured table on the page is matched
/// to the cell covering it, and returned per table as `cell index → pictures`.
/// A picture that pairs with a caption stays a standalone figure (upstream
/// would nest it and lose the caption; keeping the caption is the better
/// failure). Tables without first-class cells (geometric fallback) have no cell
/// boxes to match against and nest nothing.
fn match_table_pictures(
    regions: &[Region],
    table_rows: &[Option<TableGrid>],
    caption_for: &[Option<usize>],
) -> std::collections::HashMap<usize, Vec<(usize, Vec<usize>)>> {
    let mut out: std::collections::HashMap<usize, Vec<(usize, Vec<usize>)>> =
        std::collections::HashMap::new();
    for (p, pic) in regions.iter().enumerate() {
        if pic.label != "picture" || caption_for.get(p).is_some_and(Option::is_some) {
            continue;
        }
        let pa = area(pic.l, pic.t, pic.r, pic.b).max(1.0);
        let mut best: Option<(f32, usize, usize)> = None; // (coverage, table, cell)
        for (t, tbl) in regions.iter().enumerate() {
            if !is_table_like(tbl.label) {
                continue;
            }
            let Some(grid) = table_rows.get(t).and_then(Option::as_ref) else {
                continue;
            };
            if inter(pic, tbl.l, tbl.t, tbl.r, tbl.b) / pa < RICH_CELL_PICTURE_COVERAGE {
                continue;
            }
            if let Some((cov, cell)) = match_picture_to_cell(pic, &grid.cells) {
                if best.is_none_or(|(b, _, _)| cov > b) {
                    best = Some((cov, t, cell));
                }
            }
        }
        if let Some((_, t, cell)) = best {
            let entry = out.entry(t).or_default();
            match entry.iter_mut().find(|(c, _)| *c == cell) {
                Some((_, pics)) => pics.push(p),
                None => entry.push((cell, vec![p])),
            }
        }
    }
    out
}

/// docling `_match_picture_to_table_cell`: among the cells covering ≥ 80 % of
/// the picture, prefer the one at the picture's inferred grid position (the
/// row / column whose median cell center is nearest the picture's center —
/// cell boxes can overlap across logical rows and columns), else the best
/// coverage. Returns `(coverage, cell index)`.
fn match_picture_to_cell(pic: &Region, cells: &[docling_core::TableCell]) -> Option<(f32, usize)> {
    let pa = area(pic.l, pic.t, pic.r, pic.b).max(1.0);
    let cover = |b: &[f32; 4]| inter(pic, b[0], b[1], b[2], b[3]) / pa;
    let eligible: Vec<(f32, usize)> = cells
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let b = c.bbox.as_ref()?;
            let cov = cover(b);
            (cov >= RICH_CELL_PICTURE_COVERAGE).then_some((cov, i))
        })
        .collect();
    if eligible.is_empty() {
        return None;
    }
    let mut row_centers: std::collections::BTreeMap<usize, Vec<f32>> = Default::default();
    let mut col_centers: std::collections::BTreeMap<usize, Vec<f32>> = Default::default();
    for c in cells {
        let Some(b) = c.bbox.as_ref() else { continue };
        for r in c.start_row..c.start_row + c.row_span {
            row_centers.entry(r).or_default().push((b[1] + b[3]) / 2.0);
        }
        for k in c.start_col..c.start_col + c.col_span {
            col_centers.entry(k).or_default().push((b[0] + b[2]) / 2.0);
        }
    }
    let median = |v: &mut Vec<f32>| -> f32 {
        v.sort_by(f32::total_cmp);
        let n = v.len();
        if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        }
    };
    let (px, py) = ((pic.l + pic.r) / 2.0, (pic.t + pic.b) / 2.0);
    let nearest = |centers: &mut std::collections::BTreeMap<usize, Vec<f32>>, target: f32| {
        centers
            .iter_mut()
            .map(|(&i, v)| (i, (median(v) - target).abs()))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    };
    let row = nearest(&mut row_centers, py);
    let col = nearest(&mut col_centers, px);
    let logical: Vec<(f32, usize)> = eligible
        .iter()
        .copied()
        .filter(|&(_, i)| {
            let c = &cells[i];
            row.is_some_and(|r| c.start_row <= r && r < c.start_row + c.row_span)
                && col.is_some_and(|k| c.start_col <= k && k < c.start_col + c.col_span)
        })
        .collect();
    let pool = if logical.is_empty() {
        &eligible
    } else {
        &logical
    };
    // Python's `max` over `(coverage, cell_index, cell)` tuples: highest
    // coverage, ties to the higher index.
    pool.iter()
        .copied()
        .max_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)))
}

/// The DocLang structure overlay derived from first-class cells: span
/// continuations (`lcel`/`ucel`/`xcel`) and per-cell header roles, so the
/// PDF path's DCLX carries real spans instead of a flat grid.
fn structure_from_cells(
    cells: &[docling_core::TableCell],
    nrows: usize,
    ncols: usize,
) -> docling_core::TableStructure {
    let grid = || vec![vec![false; ncols]; nrows];
    let mut col_cont = grid();
    let mut row_cont = grid();
    let mut row_header = grid();
    let mut col_header = grid();
    for c in cells {
        for r in c.start_row..(c.start_row + c.row_span).min(nrows) {
            for k in c.start_col..(c.start_col + c.col_span).min(ncols) {
                col_cont[r][k] = k > c.start_col;
                row_cont[r][k] = r > c.start_row;
                row_header[r][k] = c.row_header;
                col_header[r][k] = c.column_header;
            }
        }
    }
    docling_core::TableStructure {
        header_row: Vec::new(),
        col_continuation: col_cont,
        row_continuation: row_cont,
        row_header,
        col_header,
    }
}

pub fn assemble_page(
    page: &PdfPage,
    mut regions: Vec<Region>,
    table_rows: &[Option<TableGrid>],
    enrichments: &[Option<Enrichment>],
    // Picture-crop scale in px/pt (docling's `images_scale`, #520); `None`
    // keeps the page render's own scale.
    picture_scale: Option<f32>,
) -> (Vec<Node>, Vec<(String, String)>) {
    // Without pixels (the text-layer-only wasm build) no picture is cropped.
    #[cfg(not(feature = "ocr-prep"))]
    let _ = picture_scale;
    let _typography = TypographyScope::set(page.keep_typography);
    let mut nodes: Vec<Node> = Vec::new();
    // Every page opens with an invisible page marker carrying its size in
    // points — what the JSON export needs to build docling's `pages` map and
    // denormalize the 0–511 `<location>` grid into point bboxes (#171). The
    // page *number* is stamped by the document-level collector (which knows
    // the real 1-based index, `--pages` windows included); every serializer
    // except JSON skips the marker, so Markdown/DocLang stay byte-identical.
    nodes.push(Node::PageInfo {
        page_no: 0,
        width: page.width,
        height: page.height,
    });
    // Recover this page's hyperlinks (anchor-precise pairs for strict
    // Markdown; whole-item docling-parity links are baked below and their
    // pairs dropped from this list so strict output doesn't double-wrap).
    let mut links = resolve_link_anchors(page);
    // Lines with a drawn checkbox square in front become checkbox items (#609).
    split_checkbox_lines(&mut regions, &page.cells, &page.checkboxes);
    // Pair each region with its precomputed TableFormer grid and enrichment
    // (indexed by original order) and order by reading order together, so they
    // stay aligned.
    // A picture's children (docling's `_set_cluster_children`: the regulars
    // > 80 % inside it) are not page elements — they leave the reading order
    // here and ride with their picture, to be written under it in the JSON.
    let parents = picture_parents(&regions);
    let mut kids: Vec<Vec<Region>> = vec![Vec::new(); regions.len()];
    let mut top: Vec<(usize, Region)> = Vec::with_capacity(regions.len());
    for (i, (r, parent)) in regions.into_iter().zip(parents).enumerate() {
        match parent {
            Some(p) => kids[p].push(r),
            None => top.push((i, r)),
        }
    }
    // docling's assembly order of the regions — what its reading-order
    // predictor knows as `cid` (#424) — before they are shuffled.
    let top_regions: Vec<Region> = top.iter().map(|(_, r)| r.clone()).collect();
    let cids = cluster_cids(&top_regions, &page.cells);
    type RegionItem = (Region, Option<TableGrid>, Option<Enrichment>, Vec<Region>);
    let mut items: Vec<RegionItem> = top
        .into_iter()
        .map(|(i, r)| {
            (
                r,
                table_rows.get(i).cloned().flatten(),
                enrichments.get(i).cloned().flatten(),
                std::mem::take(&mut kids[i]),
            )
        })
        .collect();
    order_with_containers(&mut items, &cids, page.width, page.height, |it| &it.0);
    // Float a margin page number to the front of reading order (docling parity:
    // right_to_left_02's bottom `11` is its first item). Stable, so everything
    // else keeps its order; no-op on pages without such a region.
    let page_h = page.height;
    items.sort_by_key(|(r, _, _, _)| !is_page_number(r, &page.cells, page_h));
    let table_rows: Vec<Option<TableGrid>> = items.iter().map(|(_, t, _, _)| t.clone()).collect();
    let enrichments: Vec<Option<Enrichment>> = items.iter().map(|(_, _, e, _)| e.clone()).collect();
    let mut picture_children: Vec<Vec<Region>> = items
        .iter_mut()
        .map(|it| std::mem::take(&mut it.3))
        .collect();
    let mut regions: Vec<Region> = items.into_iter().map(|(r, _, _, _)| r).collect();
    promote_labelled_captions(&mut regions, &page.cells);
    // Children in docling's `_sort_clusters(mode="id")` order: first source
    // cell, then top, then left.
    for kids in picture_children.iter_mut().filter(|k| k.len() > 1) {
        let rank = cluster_cids(kids, &page.cells);
        let mut ranked: Vec<(usize, Region)> = rank.into_iter().zip(kids.drain(..)).collect();
        ranked.sort_by_key(|(k, _)| *k);
        kids.extend(ranked.into_iter().map(|(_, r)| r));
    }
    // docling's `_find_to_captions` (#265, #813): each graphic claims its
    // caption by reading-order adjacency and gap; the caption is consumed
    // from its own slot and re-emitted with the graphic (before a picture or
    // table, after a code block, as docling renders a `captions` ref).
    let graphic_caption_for = pair_graphic_captions(&regions);
    let only = |pred: fn(&str) -> bool| -> Vec<Option<usize>> {
        regions
            .iter()
            .zip(&graphic_caption_for)
            .map(|(r, c)| c.filter(|_| pred(r.label)))
            .collect()
    };
    let caption_for = only(|l| l == "picture");
    let code_caption_for = only(|l| l == "code");
    let table_caption_for = only(is_table_like);
    let mut consumed = vec![false; regions.len()];
    for ci in graphic_caption_for.iter().flatten() {
        consumed[*ci] = true;
    }
    // Pictures inside a table become rich-cell content (docling#3906, 2.118.1):
    // the picture is nested in the cell it covers and not emitted standalone.
    let rich_cell_pictures = match_table_pictures(&regions, &table_rows, &caption_for);
    for (_, pics) in rich_cell_pictures.values().flatten() {
        for &p in pics {
            consumed[p] = true;
        }
    }
    // A code block's language label (`XML`, `C#`, …) is chrome, not content — the
    // detector emits it as its own region above the code; consume it.
    for (i, is_label) in code_language_labels(&regions, &page.cells)
        .into_iter()
        .enumerate()
    {
        if is_label {
            consumed[i] = true;
        }
    }

    // docling `ReadingOrderPredictor.predict_merges`: join a text fragment with a
    // following text fragment strictly to its right (an author column that wraps
    // into the next, a paragraph continuing in the next column) into one block —
    // the intra-page half of docling's reading-order merges (cross-page/vertical
    // continuations stay with [`merge_continuations`]). Already-consumed regions
    // (paired captions, code labels) are excluded.
    // Exclusive docling cell assignment: computed once for the ordered region
    // list and reused for every serialization below, so a cell can never render
    // in two regions. The picture children take part (docling assigns cells to
    // every regular cluster before it nests any); their texts are split off.
    let with_children: Vec<Region> = regions
        .iter()
        .chain(picture_children.iter().flatten())
        .cloned()
        .collect();
    let mut region_texts: Vec<String> = region_texts_keep_wrap(&with_children, &page.cells);
    // A trailing soft hyphen marks a word split across the line break; only a
    // plain paragraph hands it on to `merge_continuations` (#919).
    let wraps: Vec<bool> = region_texts
        .iter_mut()
        .map(|t| t.ends_with('\u{ad}') && t.pop().is_some())
        .collect();
    let mut kid_texts = region_texts.split_off(regions.len()).into_iter();
    let child_texts: Vec<Vec<String>> = picture_children
        .iter()
        .map(|k| kid_texts.by_ref().take(k.len()).collect())
        .collect();
    let is_text: Vec<bool> = regions
        .iter()
        .enumerate()
        .map(|(i, r)| r.label == "text" && !consumed[i])
        .collect();
    let is_skip: Vec<bool> = regions
        .iter()
        .enumerate()
        .map(|(i, r)| {
            consumed[i]
                || matches!(
                    r.label,
                    "page_header" | "page_footer" | "table" | "picture" | "caption" | "footnote"
                )
        })
        .collect();
    let boxes: Vec<(f32, f32, f32, f32)> = regions.iter().map(|r| (r.l, r.t, r.r, r.b)).collect();
    if docling_core::env::flag("DOCLING_RS_DEBUG_MERGES") {
        for (i, r) in regions.iter().enumerate() {
            eprintln!(
                "MRG {i:2} {} text={} skip={} [{:.0},{:.0},{:.0},{:.0}] {:?}",
                r.label,
                is_text[i],
                is_skip[i],
                r.l,
                r.t,
                r.r,
                r.b,
                region_texts[i].chars().take(40).collect::<String>()
            );
        }
    }
    let mut merge_suffix: Vec<String> = vec![String::new(); regions.len()];
    for (head, children) in
        crate::reading_order::predict_merges(&boxes, &region_texts, &is_text, &is_skip)
            .into_iter()
            .enumerate()
    {
        for c in children {
            let t = region_texts[c].trim();
            if !t.is_empty() {
                merge_suffix[head].push(' ');
                merge_suffix[head].push_str(t);
            }
            consumed[c] = true;
        }
    }

    // Whether each node comes from a region inside a form or key-value area,
    // for `continue_lists_past_asides`: filled up to the nodes so far at the
    // top of every iteration, with the previous region's flag.
    let in_container: Vec<bool> = regions
        .iter()
        .map(|r| {
            let ra = area(r.l, r.t, r.r, r.b).max(1.0);
            !matches!(r.label, "form" | "key_value_region")
                && regions.iter().any(|c| {
                    matches!(c.label, "form" | "key_value_region")
                        && inter(r, c.l, c.t, c.r, c.b) / ra > 0.8
                })
        })
        .collect();
    let mut node_in_container: Vec<bool> = Vec::new();
    let mut last_in_container = false;
    for (i, region) in regions.iter().enumerate() {
        node_in_container.resize(nodes.len(), last_in_container);
        last_in_container = in_container[i];
        if consumed[i] {
            continue;
        }
        // Page headers/footers: docling emits them as furniture blocks
        // (`<page_header>`/`<page_footer>` with a layer + location + text) at
        // their reading-order position, not as body — emit them, don't skip.
        if matches!(region.label, "page_header" | "page_footer") {
            let text = region_texts[i].clone();
            if !text.is_empty() {
                nodes.push(Node::PageFurniture {
                    footer: region.label == "page_footer",
                    location: norm_loc(region, page.width, page_h),
                    text: md_escape(&text),
                });
            }
            continue;
        }
        if is_skipped(region.label) {
            continue;
        }
        // Layout provenance for this region, normalized to docling's 0–511 grid.
        let loc = norm_loc(region, page.width, page_h);
        if region.label == "picture" {
            // The figure pixels are cropped from the page render for image export.
            // Captions are prose: markdown-escaped like a paragraph (the JSON
            // export unescapes back to the raw text, matching docling).
            let caption = caption_for[i]
                .map(|ci| md_escape(&region_texts[ci]))
                .filter(|t| !t.is_empty());
            // The caption's own region box (#609): docling gives every caption
            // the cluster it came from as its `prov`, not the picture's.
            let caption_location = caption_for[i]
                .filter(|_| caption.is_some())
                .map(|ci| norm_loc(&regions[ci], page.width, page_h));
            let classification = match &enrichments[i] {
                Some(Enrichment::PictureClasses(classes)) => Some(classes.clone()),
                _ => None,
            };
            // Without the page render (text-layer-only build) a picture keeps
            // its caption/classification but carries no cropped pixels.
            #[cfg(feature = "ocr-prep")]
            let image =
                crate::timing::timed("crop_region", || crop_region(page, region, picture_scale));
            #[cfg(not(feature = "ocr-prep"))]
            let image: Option<PictureImage> = None;
            nodes.push(located(
                loc,
                Node::Picture {
                    caption,
                    caption_href: None,
                    image,
                    classification,
                    description: None,
                    // docling's layout pipeline parents a figure's caption to
                    // the picture itself (#390) — the one backend that does.
                    caption_parent: CaptionParent::Item,
                    caption_location,
                },
            ));
            let children: Vec<Node> = picture_children[i]
                .iter()
                .zip(&child_texts[i])
                .filter_map(|(r, text)| {
                    picture_child_node(r, text, norm_loc(r, page.width, page_h))
                })
                .collect();
            if !children.is_empty() {
                nodes.push(Node::PictureChildren(children));
            }
            continue;
        }
        let mut text = region_texts[i].clone();
        text.push_str(&merge_suffix[i]);
        if text.is_empty() {
            continue;
        }
        match region.label {
            // docling assembles checkboxes as TEXT_ELEM items (the region's
            // cells are the option label, e.g. right_to_left_03's بلی/خير)
            // and its Markdown serializer renders them as task-list lines
            // (`- [x] …`) — mirrored by [`Node::CheckboxItem`].
            // Located like every other text item, so the JSON item carries
            // its page and box (#609) — a chunk of checkboxes has a page.
            // The label without a leading ballot-box glyph: the item's state
            // already says what `☐` / `☒` drew (#609).
            "checkbox_selected" | "checkbox_unselected" => nodes.push(located(
                loc,
                Node::CheckboxItem {
                    checked: region.label == "checkbox_selected",
                    text: md_escape(strip_checkbox_glyph(&text)),
                },
            )),
            // docling renders both the document title and section headers as
            // `##` (it never emits a top-level `#` for PDFs), so match that.
            "title" | "section_header" => nodes.push(located(
                loc,
                Node::Heading {
                    level: 2,
                    text: md_escape(&text),
                },
            )),
            // docling's `ListItemMarkerProcessor.process_list_item` runs on
            // every PDF list item: a leading bullet glyph or enumeration marker
            // followed by whitespace is split off into the item's `marker`, and
            // docling-core's Markdown then prints `- text` for a bullet, `N. text`
            // for an `N.` marker and `- a) text` for any other marker holding a
            // letter or digit (see [`list_item_node`]). The symbol-font bullets
            // docling-parse filters out of its cells are stripped first.
            "list_item" => nodes.push(list_item_node(&text, loc, false)),
            // TableFormer structure (cells + spans, text matched from word cells)
            // when available; otherwise geometric grid reconstruction; finally a
            // single cell.
            "table" | "document_index" => {
                // TableFormer grids carry first-class cells (#240: text +
                // page-point bbox + span rectangle + OTSL header roles) into
                // the public model, and the DocLang structure overlay derives
                // from them so DCLX emits real span/header tokens. The
                // geometric fallback has no per-cell records.
                let (mut rows, cells, structure) = match table_rows[i].clone().map(normalize_grid) {
                    Some(grid) => {
                        let nrows = grid.rows.len();
                        let ncols = grid.rows.first().map_or(0, Vec::len);
                        let structure = structure_from_cells(&grid.cells, nrows, ncols);
                        (grid.rows, Some(grid.cells), Some(structure))
                    }
                    None => {
                        let rows = reconstruct_table(region, &page.cells);
                        let rows = if rows.iter().any(|r| r.len() > 1) {
                            rows
                        } else {
                            vec![vec![text.clone()]]
                        };
                        (rows, None, None)
                    }
                };
                // The paired caption (#265) rides on the table — docling's
                // TableItem.captions ref; Markdown prints it above the grid,
                // the JSON export emits the $ref, DocLang the <caption>.
                let caption = table_caption_for[i]
                    .map(|ci| md_escape(&region_texts[ci]))
                    .filter(|t| !t.is_empty());
                // Its own region box becomes the caption item's `prov` (#609).
                let caption_location = table_caption_for[i]
                    .filter(|_| caption.is_some())
                    .map(|ci| norm_loc(&regions[ci], page.width, page_h));
                // Rich cells (docling#3906): the covering cell's blocks are its
                // text followed by the nested picture(s). docling's Markdown
                // renders a `RichTableCell` through the serializer — the
                // group's children joined by blank lines, newlines flattened
                // to spaces — so the flat `rows` text becomes
                // `text  <!-- image -->`; the first-class `cells` (the JSON
                // `table_cells` / `grid`) keep the plain text, as upstream.
                let mut cell_blocks: Option<Vec<Vec<Vec<Node>>>> = None;
                if let (Some(by_cell), Some(fc)) = (rich_cell_pictures.get(&i), cells.as_ref()) {
                    let nrows = rows.len();
                    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
                    let mut blocks = vec![vec![Vec::<Node>::new(); ncols]; nrows];
                    for (cell_idx, pics) in by_cell {
                        let cell = &fc[*cell_idx];
                        let (r, c) = (cell.start_row, cell.start_col);
                        if r >= nrows || c >= ncols {
                            continue;
                        }
                        let mut parts: Vec<String> = Vec::new();
                        let mut cell_nodes: Vec<Node> = Vec::new();
                        if !cell.text.trim().is_empty() {
                            parts.push(cell.text.clone());
                            cell_nodes.push(Node::Paragraph {
                                text: cell.text.clone(),
                            });
                        }
                        for &p in pics {
                            parts.push("<!-- image -->".to_string());
                            let classification = match &enrichments[p] {
                                Some(Enrichment::PictureClasses(classes)) => Some(classes.clone()),
                                _ => None,
                            };
                            #[cfg(feature = "ocr-prep")]
                            let image = crop_region(page, &regions[p], picture_scale);
                            #[cfg(not(feature = "ocr-prep"))]
                            let image: Option<PictureImage> = None;
                            cell_nodes.push(located(
                                norm_loc(&regions[p], page.width, page_h),
                                Node::Picture {
                                    caption: None,
                                    caption_href: None,
                                    image,
                                    classification,
                                    description: None,
                                    caption_parent: Default::default(),
                                    caption_location: None,
                                },
                            ));
                        }
                        let rendered = parts.join("  ");
                        for row in rows.iter_mut().skip(r).take(cell.row_span) {
                            for slot in row.iter_mut().skip(c).take(cell.col_span) {
                                *slot = rendered.clone();
                            }
                        }
                        blocks[r][c] = cell_nodes;
                    }
                    cell_blocks = Some(blocks);
                }
                nodes.push(located(
                    loc,
                    Node::Table(Table {
                        footnotes: Vec::new(),
                        rows,
                        location: None,
                        structure,
                        cell_blocks,
                        cells,
                        caption,
                        // As for pictures: the caption is the table's child.
                        caption_parent: CaptionParent::Item,
                        caption_location,
                    }),
                ));
            }
            // With formula enrichment the CodeFormula model decodes the region
            // to LaTeX; otherwise docling emits a placeholder comment rather
            // than the (garbled) raw glyph text.
            "formula" => match &enrichments[i] {
                Some(Enrichment::Formula { latex }) => nodes.push(Node::Formula {
                    latex: latex.clone(),
                    orig: text.clone(),
                    location: Some(loc),
                }),
                _ => nodes.push(Node::Paragraph {
                    text: "<!-- formula-not-decoded -->".into(),
                }),
            },
            // Code blocks: use the space-glyph-only grouping (monospace keeps its
            // source spacing) and emit a fenced block, preserving the line breaks
            // and indentation of the source (unlike prose, which reflows). pdfium
            // still inserts spaces around tight punctuation (`console .log`,
            // `add (3 , 5)`); tighten them to match docling-parse's source spacing.
            "code" => {
                // `code_region_text` preserves line breaks/indentation and tightens
                // each line itself; the fallback prose `text` is tightened here.
                let code = code_region_text(region, &page.code_cells);
                let code = if code.is_empty() {
                    tighten_code_punct(&text)
                } else {
                    code
                };
                // With code enrichment the CodeFormula model rewrites the block
                // (and names its language); `orig` keeps the raw extraction in
                // docling's shape — its parser has no line-preserving code
                // path, so its `orig` is the same code with the lines joined
                // by single spaces (indentation collapsed).
                // docling's parser has no line-preserving code path — its code
                // items carry the lines joined by single spaces. That flat
                // form is what every byte-conformance surface serializes
                // (legacy Markdown, JSON, DocLang); the line-preserving
                // extraction rides in `pretty` for strict Markdown only.
                let flat = code
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                let node = match &enrichments[i] {
                    Some(Enrichment::Code {
                        language,
                        text: enriched,
                    }) => Node::Code {
                        language: language.clone(),
                        text: enriched.clone(),
                        orig: Some(flat),
                        pretty: None,
                    },
                    _ => Node::Code {
                        language: None,
                        text: flat,
                        orig: None,
                        pretty: Some(code),
                    },
                };
                nodes.push(located(loc, node));
                // docling emits the `Listing N:` caption after the code block.
                if let Some(ci) = code_caption_for[i] {
                    let cap = md_escape(&region_texts[ci]);
                    if !cap.is_empty() {
                        // With its own region box, like every caption (#609).
                        nodes.push(located(
                            norm_loc(&regions[ci], page.width, page_h),
                            Node::Paragraph { text: cap },
                        ));
                    }
                }
            }
            // text, caption, footnote → paragraph
            _ => {
                // docling parity (`PageAssembleModel._match_hyperlink`): when
                // link annotations cover ≥ half of the region's box, the
                // hyperlink attaches to the item and the legacy Markdown
                // serializer wraps its full text — 2206.01062's footnote URLs
                // render as `[1 https://…](https://…)`. Sparse in-paragraph
                // citation links stay below the 0.5 coverage threshold and
                // remain plain text, exactly like docling.
                //
                // Scope: **footnote regions only.** Upstream's page_assemble
                // matches every TEXT_ELEM label, but published docling
                // observably carries the hyperlink into the document only for
                // footnote items — in both committed groundtruth generations
                // (docling-JSON and Markdown, independent runs) the fully
                // covered plain-text DOI line of 2206.01062 page 1 has
                // `hyperlink: None` while the equally covered footnotes carry
                // theirs. The corpus is the conformance reference, so match
                // the observed behavior; widen the label set if a future
                // groundtruth refresh starts linking plain text too.
                let escaped = md_escape(&text);
                let hyperlink = (region.label == "footnote")
                    .then(|| region_hyperlink(region, &page.links))
                    .flatten();
                if let Some(uri) = &hyperlink {
                    // The strict-mode anchor pairs this item covers are
                    // superseded by the whole-item link.
                    links.retain(|(anchor, href)| {
                        !(href == uri && region_texts[i].contains(anchor.as_str()))
                    });
                }
                // A footnote keeps docling's label and carries its link as
                // the item's `hyperlink` (the JSON's `text` stays the raw
                // footnote, not Markdown); every other serializer renders it
                // as the `[text](uri)` paragraph it was before.
                let node = if region.label == "footnote" {
                    Node::LabeledText {
                        label: "footnote".into(),
                        text: escaped,
                        href: hyperlink,
                    }
                } else {
                    // The wrap hyphen rides on the open paragraph (#919).
                    let mut escaped = escaped;
                    if wraps[i] {
                        escaped.push('\u{ad}');
                    }
                    Node::Paragraph { text: escaped }
                };
                nodes.push(located(loc, node))
            }
        }
    }
    node_in_container.resize(nodes.len(), last_in_container);
    continue_lists_past_asides(&mut nodes, &node_in_container);
    // A `/Rotate`-normalized scanned page (see `pdfium_backend`) was assembled
    // in upright space; rotate the finished geometry back so locations and the
    // page size are display-space, like docling and every viewer report them.
    if page.rotation != 0 {
        rotate_nodes_to_display(&mut nodes, page.rotation);
    }
    (nodes, links)
}

/// What a page node does to the list docling is filling while it builds the
/// document (`current_list` in `ReadingOrderModel`).
enum ListRole {
    /// A list item: it opens a list or joins the open one.
    Item,
    /// A picture (its children with it), chart, table or code block: docling
    /// adds it to the parent and leaves the list open.
    Aside,
    /// Anything else — a paragraph, heading, formula, caption, footnote, page
    /// header or footer: docling's text elements, each of which closes the
    /// list.
    Closes,
}

fn list_role(n: &Node) -> ListRole {
    let n = match n {
        Node::Located { inner, .. } => inner.as_ref(),
        other => other,
    };
    match n {
        Node::ListItem { .. } => ListRole::Item,
        Node::Picture { .. }
        | Node::PictureChildren(_)
        | Node::Chart { .. }
        | Node::Table(_)
        | Node::Code { .. } => ListRole::Aside,
        _ => ListRole::Closes,
    }
}

/// docling's list continuation (`_readingorder_elements_to_docling_doc`):
/// only a text element that is not a list item closes the open list, so a
/// picture, table or code block met between two list items goes into the
/// parent *after* the list group, and the items after it stay in the same
/// list — an icon beside a procedure reads after the whole procedure, not
/// between two of its steps. Lists here are runs of [`Node::ListItem`] the
/// exports group, so such a node moves past the list items that follow it.
/// In a form or key-value area (`in_container`, per node) docling splits
/// lists on any other element instead, so there it closes the list. Per
/// page: a list that runs on from the previous page with no page header or
/// footer between does not hold back the page's first pictures, as docling's
/// would. A form area inside a list does not close it upstream, but its
/// flattened children do here; a graphic's footnote, a node of its own here,
/// closes it too (upstream it hangs on the graphic).
fn continue_lists_past_asides(nodes: &mut Vec<Node>, in_container: &[bool]) {
    let mut out = Vec::with_capacity(nodes.len());
    let mut held = Vec::new();
    let mut open = false;
    for (n, &contained) in std::mem::take(nodes).into_iter().zip(in_container) {
        match list_role(&n) {
            ListRole::Item => {
                open = true;
                out.push(n);
            }
            ListRole::Aside if open && !contained => held.push(n),
            ListRole::Aside if !contained => out.push(n),
            ListRole::Aside | ListRole::Closes => {
                open = false;
                out.append(&mut held);
                out.push(n);
            }
        }
    }
    out.append(&mut held);
    *nodes = out;
}

/// One child of a picture as docling's `ReadingOrderModel._add_child_elements`
/// writes it under the `PictureItem`: a heading for a `section_header` /
/// `title` (upstream remaps title to section header), a list item for a
/// `list_item`, a furniture-layer text for a page header/footer, a caption,
/// otherwise a text item. `None` for a child that claimed no text.
fn picture_child_node(region: &Region, text: &str, loc: [u16; 4]) -> Option<Node> {
    if text.is_empty() {
        return None;
    }
    Some(match region.label {
        "title" | "section_header" => located(
            loc,
            Node::Heading {
                level: 2,
                text: md_escape(text),
            },
        ),
        // docling-core's `add_list_item` under a non-list parent opens a
        // list group per item, so every child item starts its own list;
        // `_add_child_elements` runs the marker processor on it too.
        "list_item" => list_item_node(text, loc, true),
        "page_header" | "page_footer" => Node::PageFurniture {
            footer: region.label == "page_footer",
            location: loc,
            text: md_escape(text),
        },
        "caption" => located(
            loc,
            Node::Caption {
                text: md_escape(text),
                href: None,
            },
        ),
        _ => located(
            loc,
            Node::Paragraph {
                text: md_escape(text),
            },
        ),
    })
}

/// Rotate one 0–511 location bbox 90° clockwise on the grid (top-left origin):
/// `(x, y) → (511 - y, x)`.
fn rot_loc_cw(l: [u16; 4]) -> [u16; 4] {
    [511 - l[3], l[0], 511 - l[1], l[2]]
}

/// Map upright-space geometry back to display space for a page whose `/Rotate`
/// was normalized away before inference: every `<location>` rotates `rot`°
/// clockwise on the 0–511 grid (the grid is per-axis normalized, so no page
/// dims are needed), and the `PageInfo` size returns to the display box. Node
/// text and order are untouched — reading order was decided upright, which is
/// the whole point.
fn rotate_nodes_to_display(nodes: &mut [Node], rot: u16) {
    let quarter_turns = (rot / 90) as usize;
    let rot_loc = |l: &mut [u16; 4]| {
        for _ in 0..quarter_turns {
            *l = rot_loc_cw(*l);
        }
    };
    fn walk(node: &mut Node, rot_loc: &impl Fn(&mut [u16; 4]), swap_dims: bool) {
        match node {
            Node::PageInfo { width, height, .. } => {
                if swap_dims {
                    std::mem::swap(width, height);
                }
            }
            Node::Located { location, inner } => {
                rot_loc(location);
                walk(inner, rot_loc, swap_dims);
            }
            Node::Furniture { inner, .. } => walk(inner, rot_loc, swap_dims),
            Node::Group { children, .. } | Node::PictureChildren(children) => {
                for c in children {
                    walk(c, rot_loc, swap_dims);
                }
            }
            Node::ListItem { location, .. }
            | Node::Formula { location, .. }
            | Node::Chart { location, .. } => {
                if let Some(l) = location {
                    rot_loc(l);
                }
            }
            Node::PageFurniture { location, .. } => rot_loc(location),
            Node::Table(t) => {
                if let Some(l) = &mut t.location {
                    rot_loc(l);
                }
            }
            _ => {}
        }
    }
    let swap_dims = quarter_turns % 2 == 1;
    for node in nodes {
        walk(node, &rot_loc, swap_dims);
    }
}

/// Merge paragraph fragments split across a column or page break. docling joins a
/// paragraph whose previous fragment ends mid-sentence (a letter, not sentence
/// punctuation) with a lowercase continuation: `…definition of` + `lists in…` →
/// `…definition of lists in…`. The fragments are consecutive paragraphs, or
/// separated only by figure(s) the text wraps around: a column whose body flows
/// past a figure resumes below it (`…The wing type that is` ⟶[figure]⟶ `the most
/// common…`), and docling emits the whole paragraph before the figure. A heading,
/// table, or list between them ends the paragraph (no merge).
/// A paragraph that is really a figure/table caption (`Fig. 1. …`, `Table 2 …`).
/// Used to skip an unpaired caption when stitching a paragraph that wraps around
/// a figure.
fn looks_like_caption(text: &str) -> bool {
    let head: String = text.trim_start().chars().take(14).collect();
    (head.starts_with("Fig") || head.starts_with("Table"))
        && head.contains(|c: char| c.is_ascii_digit())
}

/// A paragraph fragment is "open" — i.e. it might continue into the next
/// paragraph — when it ends mid-word (a letter) or with a wrap hyphen/dash.
/// docling joins `vocab-` + `ulary` → `vocab- ulary`.
fn paragraph_is_open(text: &str) -> bool {
    // docling's merge head test (`.+([a-z,\-\u00AD])\s*`): at least two chars,
    // ending in an ASCII lowercase letter, a comma, a hyphen, or a soft
    // hyphen. The comma matters: 2206's "…In phase four," resumes across the
    // page break. Uppercase/non-Latin endings do not merge, exactly as
    // upstream (the dash family is already `-` here — clean_text normalized).
    let t = text.trim_end();
    t.chars().count() >= 2
        && t.chars()
            .next_back()
            .is_some_and(|c| matches!(c, 'a'..='z' | ',' | '-' | '\u{ad}'))
}

/// The paragraph text inside a node, looking through a [`Node::Located`]
/// provenance wrapper (PDF body paragraphs are wrapped since they carry a
/// `<location>`). Returns `None` for non-paragraph nodes.
fn as_paragraph(n: &Node) -> Option<&str> {
    match n {
        Node::Paragraph { text } => Some(text),
        Node::Located { inner, .. } => match inner.as_ref() {
            Node::Paragraph { text } => Some(text),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a node is a picture, looking through a [`Node::Located`] wrapper.
fn is_picture_node(n: &Node) -> bool {
    match n {
        Node::Picture { .. } => true,
        Node::Located { inner, .. } => matches!(inner.as_ref(), Node::Picture { .. }),
        _ => false,
    }
}

/// A node a forward paragraph merge looks straight past: a figure or *table*
/// the text wraps around, or a page header/footer that falls between the two
/// fragments of a paragraph continuing across a page break (docling's merge
/// skip-labels: page_header, page_footer, table, picture, caption, footnote —
/// 2206's "…In phase four," resumes after a full caption+table+figure block).
fn is_merge_trailer(n: &Node) -> bool {
    is_picture_node(n)
        || matches!(
            n,
            Node::PageFurniture { .. }
                | Node::PageInfo { .. }
                | Node::Table(_)
                | Node::PictureChildren(_)
        )
        || matches!(n, Node::Located { inner, .. } if matches!(inner.as_ref(), Node::Table(_)))
        || is_footnote_node(n)
        || as_paragraph(n).is_some_and(looks_like_caption)
}

/// Whether a node is a footnote item (a [`Node::LabeledText`] labelled
/// `footnote`), looking through a [`Node::Located`] wrapper — one of
/// docling's merge skip-labels: a footnote between the two halves of a
/// paragraph is looked past, never merged into.
fn is_footnote_node(n: &Node) -> bool {
    let n = match n {
        Node::Located { inner, .. } => inner.as_ref(),
        other => other,
    };
    matches!(n, Node::LabeledText { label, .. } if label == "footnote")
}

/// Rebuild node `i` as a paragraph with `text`, preserving its `<location>`
/// wrapper (and thus provenance) if it had one.
fn reparagraph(node: &Node, text: String) -> Node {
    match node {
        Node::Located { location, .. } => located(*location, Node::Paragraph { text }),
        _ => Node::Paragraph { text },
    }
}

/// Whether `text` opens with a coordinating conjunction as a whole word
/// (German, English, French, Dutch, Spanish): after a line-final `-` it means
/// an elliptical compound, never the second half of a wrapped word.
fn starts_with_conjunction(text: &str) -> bool {
    let word: String = text
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>()
        .to_lowercase();
    matches!(
        word.as_str(),
        "und" | "oder" | "bzw" | "sowie" | "and" | "or" | "et" | "ou" | "en" | "y"
    )
}

/// Drop the soft hyphen an unmerged paragraph still ends with (see
/// [`clean_text_keep_wrap`]): a word split nothing continued is just a word.
fn strip_wrap_marks(nodes: &mut [Node]) {
    for n in nodes {
        let n = match n {
            Node::Located { inner, .. } => inner.as_mut(),
            other => other,
        };
        if let Node::Paragraph { text } = n {
            if text.ends_with('\u{ad}') {
                text.pop();
            }
        }
    }
}

pub(crate) fn merge_continuations(nodes: &mut Vec<Node>) {
    merge_open_paragraphs(nodes);
    strip_wrap_marks(nodes);
}

/// [`merge_continuations`] without the final wrap-mark strip: the streaming
/// buffer still holds open paragraphs a later page may continue.
fn merge_open_paragraphs(nodes: &mut Vec<Node>) {
    let mut i = 0;
    while i + 1 < nodes.len() {
        let Some(a) = as_paragraph(&nodes[i]) else {
            i += 1;
            continue;
        };
        // A figure/table caption is a self-contained unit; body text resuming
        // after a figure is the continuation case, not the caption itself. Never
        // stitch *from* a caption — otherwise a caption that ends in a lone glyph
        // (`Fig. 5. … PubTabNet. μ`) would swallow a following stray figure label
        // (a standalone `μ`) into `… μ μ`.
        if looks_like_caption(a) {
            i += 1;
            continue;
        }
        if !paragraph_is_open(a) {
            i += 1;
            continue;
        }
        // The continuation is the next paragraph, looking past any figures the
        // text wraps around — and a figure/table caption that was emitted as its
        // own paragraph (an above-the-figure caption that didn't pair), since the
        // body text resumes after the whole figure+caption block.
        let mut j = i + 1;
        while nodes.get(j).is_some_and(is_merge_trailer) {
            j += 1;
        }
        // docling's continuation regex allows either case, but its merge runs
        // over the pre-assembly element stream; at node level an uppercase
        // start is overwhelmingly a new sentence/heading fragment (allowing it
        // swallowed 2305's formula blocks and redp's chapter openers), so the
        // continuation stays lowercase-start here.
        let cont = nodes.get(j).and_then(as_paragraph).is_some_and(|b| {
            b.trim_start()
                .chars()
                .next()
                .is_some_and(char::is_lowercase)
        });
        if cont {
            let a = as_paragraph(&nodes[i]).unwrap().trim_end().to_string();
            let b = as_paragraph(&nodes[j]).unwrap().trim_start().to_string();
            // A soft hyphen -- or a hard hyphen followed by a lowercase
            // continuation (guaranteed lowercase by the `cont` gate above) --
            // is a word split across the break: strip it and join without a
            // space, docling#3888 ("vocab-" + "ulary" -> "vocabulary");
            // docling's older serializer kept the artifact ("vocab- ulary").
            // Everything else joins with the space, as before.
            // Exception (#920): a hard hyphen before a coordinating conjunction
            // is the elliptical compound (`Warte- und Behaltefristen`), not a
            // wrapped word -- no word is split as `Warte` + `und`. Kept with
            // its space; docling fuses these (`Warteund`), a deliberate
            // divergence (docs/MIGRATION.md).
            let elliptical = !a.ends_with('\u{ad}') && starts_with_conjunction(&b);
            let merged = match a.strip_suffix('\u{ad}').or_else(|| a.strip_suffix('-')) {
                Some(stem) if !elliptical => format!("{stem}{b}"),
                _ => format!("{a} {b}"),
            };
            // Keep node i's provenance wrapper; docling's merged paragraph keeps
            // the first fragment's geometry as its primary location.
            nodes[i] = reparagraph(&nodes[i], merged);
            nodes.remove(j);
            // Re-check i: the merged paragraph may continue further.
        } else {
            i += 1;
        }
    }
}

/// How many leading nodes of `nodes` are safe to flush now — i.e. cannot be
/// rewritten by a future [`merge_continuations`] once more pages are appended.
///
/// A forward merge can only start from an "open" paragraph (ends mid-word) and
/// only reaches across trailing pictures and figure/table captions. So we scan
/// from the end past those skippable trailers: if the first non-skippable node is
/// an open paragraph, it (and the trailers after it) must be held; anything else —
/// a closed paragraph, a heading, a table, a list — blocks any forward merge, so
/// the whole buffer is safe to flush.
fn hold_start(nodes: &[Node]) -> usize {
    for k in (0..nodes.len()).rev() {
        // Skippable trailers (figures, page furniture, captions): a forward merge
        // looks straight past them.
        if is_merge_trailer(&nodes[k]) {
            continue;
        }
        match as_paragraph(&nodes[k]) {
            // An open body paragraph might still pull a continuation off the next
            // page — hold from here to the end.
            Some(text) if paragraph_is_open(text) => return k,
            // A closed paragraph, heading, table, list, etc. ends the paragraph:
            // nothing after it can merge backwards across it. Flush everything.
            _ => return nodes.len(),
        }
    }
    // Only skippable trailers (or empty) and no open paragraph to anchor a merge.
    nodes.len()
}

/// Streaming counterpart of [`merge_continuations`]: feed per-page node batches in
/// document order and get back the prefix that is final (its cross-page merges are
/// resolved and no future page can change it), holding back only the small tail
/// that might still merge into the next page. Concatenating every flushed batch
/// (then [`finish`](Self::finish)) yields exactly the same nodes as running
/// [`merge_continuations`] once over the whole document.
pub(crate) struct StreamAssembler {
    pending: Vec<Node>,
}

impl StreamAssembler {
    pub(crate) fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Append one page's nodes, resolve merges within the buffer, and return the
    /// now-final prefix to emit (possibly empty).
    pub(crate) fn push(&mut self, mut nodes: Vec<Node>) -> Vec<Node> {
        self.pending.append(&mut nodes);
        merge_open_paragraphs(&mut self.pending);
        let cut = hold_start(&self.pending);
        let tail = self.pending.split_off(cut);
        let mut done = std::mem::replace(&mut self.pending, tail);
        strip_wrap_marks(&mut done);
        done
    }

    /// Flush whatever is left after the last page (the held tail is final once no
    /// more pages can follow).
    pub(crate) fn finish(mut self) -> Vec<Node> {
        strip_wrap_marks(&mut self.pending);
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cells_text, clean_text, is_note_of, looks_like_note_call, merge_overlapping_regulars,
        reclaim_heading_body_footers,
    };

    /// docling drops a picture covering > 90 % of the page (its labels then
    /// read out as text); a dominant-but-not-full figure and any other label
    /// stay whatever their size.
    #[test]
    fn full_page_pictures_are_dropped_like_docling() {
        use super::drop_full_page_pictures;
        use crate::layout::Region;
        let region = |label: &'static str, l, t, r, b| Region {
            label,
            score: 0.99,
            l,
            t,
            r,
            b,
        };
        let mut regions = vec![
            region("picture", 0.0, 0.5, 478.9, 241.8),
            region("picture", 10.0, 10.0, 400.0, 200.0),
            region("table", 0.0, 0.0, 480.0, 243.0),
            region("text", 5.0, 5.0, 100.0, 20.0),
        ];
        drop_full_page_pictures(&mut regions, 480.75, 243.75);
        let labels: Vec<_> = regions.iter().map(|r| (r.label, r.l)).collect();
        assert_eq!(
            labels,
            vec![("picture", 10.0), ("table", 0.0), ("text", 5.0)]
        );
    }
    use super::{
        code_region_text, float_running_head, merge_continuations, resolve_link_anchors,
        StreamAssembler,
    };
    use crate::layout::Region;
    use crate::pdfium_backend::{LinkAnnot, PdfPage, TextCell};
    use docling_core::Node;

    /// The int8-layout guard's coverage metric: cells under detections count,
    /// cells outside don't, whitespace cells are ignored, and a cell-less page
    /// reads as fully covered (nothing to rescue).
    #[test]
    fn layout_cell_coverage_counts_claimed_text_cells() {
        let cell = |text: &str, l: f32, t: f32| TextCell {
            text: text.into(),
            l,
            t,
            r: l + 40.0,
            b: t + 10.0,
        };
        let region = Region {
            label: "text",
            score: 0.9,
            l: 0.0,
            t: 0.0,
            r: 100.0,
            b: 50.0,
        };
        let cells = vec![
            cell("inside", 10.0, 10.0),
            cell("also inside", 10.0, 30.0),
            cell("outside", 10.0, 200.0),
            cell("   ", 10.0, 210.0), // whitespace: not counted at all
        ];
        let cov = super::layout_cell_coverage(std::slice::from_ref(&region), &cells);
        assert!((cov - 2.0 / 3.0).abs() < 1e-6, "got {cov}");
        assert_eq!(super::layout_cell_coverage(&[], &[]), 1.0);
        assert_eq!(super::layout_cell_coverage(&[], &cells), 0.0);
    }

    /// #165: a picture no longer claims cells at 0.2 intersection-over-self.
    /// A line straddling the figure border (≤80 % contained) becomes an orphan
    /// region and is emitted as page text — before the fix its cells were
    /// silently erased. A line fully inside the picture is the picture's child
    /// (docling's `_set_cluster_children`): it survives the containment drop,
    /// leaves the page's reading order, and is written only under the picture
    /// in the JSON — never in the Markdown, like docling's picture serializer.
    #[test]
    fn border_straddlers_are_page_text_picture_interior_is_a_picture_child() {
        let pic = Region {
            label: "picture",
            score: 0.9,
            l: 0.0,
            t: 0.0,
            r: 100.0,
            b: 100.0,
        };
        // ~35 % of this cell overlaps the picture (l=90..120 of 0..100): above
        // the old 0.2 claim (was swallowed), below full containment (survives).
        let straddler = TextCell {
            text: "axis label".into(),
            l: 90.0,
            t: 40.0,
            r: 120.0,
            b: 48.0,
        };
        let interior = TextCell {
            text: "in-figure callout".into(),
            l: 10.0,
            t: 10.0,
            r: 60.0,
            b: 18.0,
        };
        let cells = vec![straddler, interior];
        let mut regions = vec![pic];
        super::add_orphan_regions(&mut regions, &cells);
        super::drop_contained_regulars(&mut regions);
        assert_eq!(
            regions.iter().filter(|r| r.label == "text").count(),
            2,
            "both unclaimed lines become orphans, and a picture swallows neither"
        );
        let parents = super::picture_parents(&regions);
        let parent_of = |l: f32| {
            regions
                .iter()
                .zip(&parents)
                .find(|(r, _)| r.label == "text" && r.l == l)
                .and_then(|(_, p)| *p)
        };
        assert_eq!(
            parent_of(10.0),
            Some(0),
            "the callout is the picture's child"
        );
        assert_eq!(parent_of(90.0), None, "the straddler is a page element");

        let page = PdfPage::from_cells(200.0, 200.0, 2.0, cells);
        let n = regions.len();
        let (nodes, _) = super::assemble_page(&page, regions, &vec![None; n], &vec![None; n], None);
        let children: Vec<&Node> = nodes
            .iter()
            .filter_map(|n| match n {
                Node::PictureChildren(c) => Some(c),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            matches!(children.as_slice(), [Node::Located { inner, .. }]
                if matches!(inner.as_ref(), Node::Paragraph { text } if text == "in-figure callout")),
            "{children:?}"
        );
        let mut doc = docling_core::DoclingDocument::new("t");
        doc.nodes = nodes;
        let md = doc.export_to_markdown();
        assert!(md.contains("axis label"), "{md}");
        assert!(!md.contains("in-figure callout"), "{md}");
        let json = doc.export_to_json_value();
        let pic = &json["pictures"][0];
        let child = pic["children"][0]["$ref"].as_str().expect("a child ref");
        let idx: usize = child.rsplit('/').next().unwrap().parse().unwrap();
        assert_eq!(json["texts"][idx]["text"], "in-figure callout");
        assert_eq!(json["texts"][idx]["parent"]["$ref"], "#/pictures/0");
        assert_eq!(json["texts"][idx]["content_layer"], "body");
    }

    /// docling#3906's concern, pinned on our side: a picture detected fully
    /// inside a table region must survive the containment drop (upstream now
    /// attaches it to the table's cell; we keep it as a body sibling — either
    /// way it must not vanish). The text region inside the same table is the
    /// control: regulars are the ones the drop swallows.
    #[test]
    fn picture_inside_a_table_region_survives_the_containment_drop() {
        let mut regions = vec![
            region("table", 0.9, 0.0, 0.0, 200.0, 200.0),
            region("picture", 0.9, 20.0, 20.0, 120.0, 120.0),
            region("text", 0.9, 20.0, 140.0, 180.0, 180.0),
        ];
        super::drop_contained_regulars(&mut regions);
        let labels: Vec<&str> = regions.iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            ["table", "picture"],
            "the in-table picture stays; the in-table regular is the special's child"
        );
    }

    /// Caption pairing (#265, #813) is docling's `_find_to_captions`: a
    /// caption binds a graphic in the unbroken run next to it in the region
    /// sequence — above-caption and below-caption both work, and geometry
    /// only ranks (a same-page caption in the other column of a two-column
    /// layout is *not* adjacent, however close its box is). A caption
    /// separated from its graphic by a text paragraph stays unattached; one
    /// between two graphics goes to the nearer, a tie to the one above.
    #[test]
    fn captions_pair_by_reading_order_adjacency() {
        // caption → table (above-caption), then table → caption (below-caption),
        // then a caption fenced off by a paragraph, then one between two tables.
        let regions = vec![
            region("text", 0.9, 0.0, 0.0, 100.0, 10.0), // 0 body text
            region("caption", 0.9, 0.0, 12.0, 60.0, 20.0), // 1 above-caption
            region("table", 0.9, 20.0, 22.0, 90.0, 60.0), // 2 ← pairs with 1
            region("table", 0.9, 0.0, 70.0, 100.0, 110.0), // 3 ← pairs with 4
            region("caption", 0.9, 0.0, 112.0, 60.0, 120.0), // 4 below-caption
            region("text", 0.9, 0.0, 130.0, 100.0, 140.0), // 5 body text
            region("caption", 0.9, 0.0, 142.0, 60.0, 150.0), // 6 fenced by 5/7
            region("text", 0.9, 0.0, 152.0, 100.0, 162.0), // 7 body text
            region("table", 0.9, 0.0, 170.0, 100.0, 200.0), // 8 ← tie, above wins
            region("caption", 0.9, 0.0, 202.0, 60.0, 210.0), // 9 2 pt from both
            region("table", 0.9, 0.0, 212.0, 100.0, 240.0), // 10 unpaired
            region("page_footer", 0.9, 0.0, 250.0, 100.0, 260.0), // 11 furniture
        ];
        let pairs = super::pair_graphic_captions(&regions);
        assert_eq!(pairs[2], Some(1), "caption directly above its table pairs");
        assert_eq!(pairs[3], Some(4), "caption directly below its table pairs");
        assert!(
            !pairs.contains(&Some(6)),
            "a text paragraph between caption and table breaks the bond"
        );
        assert_eq!(pairs[8], Some(9), "equal gaps go to the preceding graphic");
        assert_eq!(pairs[10], None);
        assert!(
            pairs.iter().filter(|p| p.is_some()).count() == 3,
            "{pairs:?}"
        );
    }

    /// #813: a `Figure N:` label printed *above* its figure (DP-Bench
    /// 01030000000036: caption [40,255,287,266] over picture [118,277,423,469])
    /// binds the picture; the old geometric matcher only looked below. A
    /// caption below a code block binds too (docling's run is label-blind).
    #[test]
    fn a_caption_above_its_picture_binds() {
        let regions = vec![
            region("section_header", 0.9, 40.0, 200.0, 300.0, 215.0),
            region("text", 0.9, 40.0, 220.0, 500.0, 250.0),
            region("caption", 0.93, 40.0, 255.0, 287.0, 266.0), // 2
            region("picture", 0.98, 118.0, 277.0, 423.0, 469.0), // 3 ← 2
            region("text", 0.9, 40.0, 480.0, 500.0, 520.0),
            region("code", 0.9, 40.0, 530.0, 500.0, 600.0), // 5 ← 6
            region("caption", 0.9, 40.0, 604.0, 300.0, 614.0), // 6
        ];
        let pairs = super::pair_graphic_captions(&regions);
        assert_eq!(pairs[3], Some(2));
        assert_eq!(pairs[5], Some(6));
    }

    /// Within a run the *nearest* graphic wins, not the adjacent one in the
    /// sequence; and the matching augments: a caption whose first choice is
    /// held by a caption that can rehouse itself displaces it, so no figure
    /// ends up orphaned while another holds a caption it did not need.
    #[test]
    fn captions_rank_by_gap_and_displace_rehousable_claims() {
        // P1 and P2 are an unbroken run; C1 under it sits right below P2
        // (gap 2) and far from P1 (gap 62), so it takes P2, not the run's
        // first graphic.
        let regions = vec![
            region("picture", 0.9, 0.0, 0.0, 100.0, 50.0),   // 0 P1
            region("picture", 0.9, 0.0, 60.0, 100.0, 110.0), // 1 P2
            region("caption", 0.9, 0.0, 112.0, 100.0, 120.0), // 2 C1: P2 (2) < P1 (62)
        ];
        let pairs = super::pair_graphic_captions(&regions);
        assert_eq!(pairs, vec![None, Some(2), None]);

        // C1 above the run prefers P1 (gap 2) over P2 (62); C2 below the run
        // reaches P2 (2) then P1 (62). Each gets its nearest.
        let regions = vec![
            region("caption", 0.9, 0.0, 0.0, 100.0, 8.0),     // 0 C1
            region("picture", 0.9, 0.0, 10.0, 100.0, 50.0),   // 1 P1
            region("picture", 0.9, 0.0, 60.0, 100.0, 110.0),  // 2 P2
            region("caption", 0.9, 0.0, 112.0, 100.0, 120.0), // 3 C2
        ];
        let pairs = super::pair_graphic_captions(&regions);
        assert_eq!(pairs, vec![None, Some(0), Some(3), None]);

        // Augmenting path: C1 (between P1 and P2, nearer P2) takes P2 first;
        // C2 below reaches only P2 and displaces C1, which rehouses on P1.
        let regions = vec![
            region("picture", 0.9, 0.0, 0.0, 100.0, 50.0),   // 0 P1
            region("caption", 0.9, 0.0, 70.0, 100.0, 78.0),  // 1 C1: P2 (2) < P1 (20)
            region("picture", 0.9, 0.0, 80.0, 100.0, 120.0), // 2 P2
            region("caption", 0.9, 0.0, 122.0, 100.0, 130.0), // 3 C2: P2 only
        ];
        let pairs = super::pair_graphic_captions(&regions);
        assert_eq!(pairs, vec![Some(1), None, Some(3), None]);

        // No graphic can be freed: the first claim stands, the second stays loose.
        let regions = vec![
            region("caption", 0.9, 0.0, 0.0, 100.0, 8.0),   // 0 C1
            region("picture", 0.9, 0.0, 10.0, 100.0, 50.0), // 1 P1
            region("caption", 0.9, 0.0, 52.0, 100.0, 60.0), // 2 C2
        ];
        let pairs = super::pair_graphic_captions(&regions);
        assert_eq!(pairs, vec![None, Some(0), None]);
    }

    #[test]
    fn caption_labels_are_recognized() {
        for t in [
            "Table 5: Prediction Performance",
            "Figure 2.1: Surveyed MSMEs",
            "Fig. 3: Overview",
            "Table 1. Pour volume by month (m3)",
            "  Figure 12a. Left panel",
            "Listing 4: main.rs",
        ] {
            assert!(super::is_caption_label(t), "{t}");
        }
        for t in [
            "Table 5 shows the results",
            "Tables 5: and 6:",
            "Figure",
            "Figure: no number",
            "table 5: lowercase",
            "Table : no number",
        ] {
            assert!(!super::is_caption_label(t), "{t}");
        }
    }

    /// 2206.01062's Table 5 caption: a four-line `text` block the model did
    /// not call `caption`, 22 pt above its table, becomes one; the same block
    /// with a paragraph between it and the table, or without the label's
    /// punctuation, stays text.
    #[test]
    fn a_labelled_text_block_beside_a_graphic_becomes_a_caption() {
        let cells = vec![
            cell(
                "Table 5: Prediction Performance of a network",
                54.0,
                87.0,
                296.0,
                100.0,
            ),
            cell("across the datasets.", 54.0, 100.0, 200.0, 113.0),
            cell("In this section we discuss", 54.0, 400.0, 296.0, 412.0),
        ];
        let mut regions = vec![
            region("text", 0.99, 54.0, 87.0, 296.0, 150.0),
            region("table", 0.99, 73.0, 172.0, 275.0, 340.0),
            region("text", 0.99, 54.0, 400.0, 296.0, 500.0),
        ];
        super::promote_labelled_captions(&mut regions, &cells);
        assert_eq!(regions[0].label, "caption");
        assert_eq!(regions[2].label, "text");

        // A paragraph between the label and the table breaks the adjacency.
        let mut regions = vec![
            region("text", 0.99, 54.0, 87.0, 296.0, 150.0),
            region("text", 0.99, 54.0, 152.0, 296.0, 170.0),
            region("table", 0.99, 73.0, 172.0, 275.0, 340.0),
        ];
        super::promote_labelled_captions(&mut regions, &cells);
        assert_eq!(regions[0].label, "text");

        // `Table 5 shows …` is prose.
        let cells = vec![cell("Table 5 shows the results", 54.0, 87.0, 296.0, 100.0)];
        let mut regions = vec![
            region("text", 0.99, 54.0, 87.0, 296.0, 150.0),
            region("table", 0.99, 73.0, 172.0, 275.0, 340.0),
        ];
        super::promote_labelled_captions(&mut regions, &cells);
        assert_eq!(regions[0].label, "text");
    }

    /// Side-by-side figures (`tests/snapshots/latex/…/swa.pdf`): three
    /// pictures in a row, their three captions in a row under them. docling's
    /// run rule binds only the first caption (the one before each other
    /// caption breaks its run); the extension gives the other two the
    /// unclaimed picture each sits under. A caption whose run is broken by
    /// *text* (2206.01062's Figure 4 behind a hash line) stays loose, as
    /// docling leaves it.
    #[test]
    fn side_by_side_figure_captions_bind_their_own_picture() {
        let regions = vec![
            region("picture", 0.8, 9.0, 2.0, 367.0, 347.0),   // 0
            region("picture", 0.7, 423.0, 3.0, 718.0, 346.0), // 1
            region("picture", 0.7, 756.0, 13.0, 1040.0, 341.0), // 2
            region("caption", 0.6, 128.0, 371.0, 315.0, 392.0), // 3 ← under 0
            region("caption", 0.6, 425.0, 371.0, 713.0, 396.0), // 4 ← under 1
            region("caption", 0.6, 785.0, 371.0, 1069.0, 397.0), // 5 ← under 2
        ];
        let pairs = super::pair_graphic_captions(&regions);
        assert_eq!(pairs[..3], [Some(3), Some(4), Some(5)]);

        let regions = vec![
            region("picture", 0.9, 316.0, 82.0, 564.0, 459.0),
            region("text", 0.9, 400.0, 458.0, 476.0, 461.0), // a hash line
            region("caption", 0.9, 318.0, 473.0, 559.0, 504.0),
        ];
        assert_eq!(
            super::pair_graphic_captions(&regions),
            vec![None, None, None]
        );
    }

    /// A colored terms-and-conditions panel detected as `picture` demotes into
    /// per-paragraph `text` regions (the blank line between C.7 and C.8 splits
    /// them); a chart whose only text is a few narrow axis labels keeps its
    /// crop untouched.
    #[test]
    fn text_panels_demote_to_paragraphs_but_charts_keep_their_crop() {
        let cell = |text: &str, l: f32, t: f32, r: f32, b: f32| TextCell {
            text: text.to_string(),
            l,
            t,
            r,
            b,
        };
        let panel = Region {
            label: "picture",
            score: 0.9,
            l: 0.0,
            t: 0.0,
            r: 100.0,
            b: 100.0,
        };
        // Three tight lines, a blank-line gap, two more: two paragraphs.
        let cells = vec![
            cell(
                "C.7. Wenn Sie diesen Vertrag widerrufen,",
                5.0,
                10.0,
                95.0,
                18.0,
            ),
            cell(
                "haben wir Ihnen alle Zahlungen, die wir",
                5.0,
                20.0,
                95.0,
                28.0,
            ),
            cell(
                "von Ihnen erhalten haben, zurückzuzahlen.",
                5.0,
                30.0,
                90.0,
                38.0,
            ),
            cell(
                "C.8. Wir können die Rückzahlung verweigern,",
                5.0,
                52.0,
                95.0,
                60.0,
            ),
            cell(
                "bis wir die Waren wieder zurückerhalten haben.",
                5.0,
                62.0,
                92.0,
                70.0,
            ),
        ];
        let mut regions = vec![panel.clone()];
        super::recover_text_panels(&mut regions, &cells);
        assert_eq!(
            regions.iter().map(|r| r.label).collect::<Vec<_>>(),
            ["text", "text"],
            "dense panel must demote into one text region per paragraph"
        );
        assert!(regions[0].b < regions[1].t, "paragraphs split at the gap");
        // Sparse narrow labels (a chart): picture survives.
        let labels = vec![
            cell("0", 5.0, 90.0, 8.0, 95.0),
            cell("50", 5.0, 50.0, 10.0, 55.0),
            cell("100", 5.0, 10.0, 12.0, 15.0),
            cell("t, s", 45.0, 96.0, 55.0, 100.0),
        ];
        let mut regions = vec![panel];
        super::recover_text_panels(&mut regions, &labels);
        assert_eq!(
            regions.iter().map(|r| r.label).collect::<Vec<_>>(),
            ["picture"]
        );
    }

    /// An uncaptioned chart on a scanned page whose title, axis labels, and
    /// OCR boxes over the plot area are dense and wide enough to pass the
    /// coverage/width gates still keeps its crop: its line heights are ragged
    /// (title face vs tick labels vs bar-area OCR), failing the uniform-leading
    /// gate — a real text panel is set with constant leading (#173).
    #[test]
    fn dense_titled_chart_keeps_its_crop() {
        let cell = |text: &str, l: f32, t: f32, r: f32, b: f32| TextCell {
            text: text.to_string(),
            l,
            t,
            r,
            b,
        };
        let chart = Region {
            label: "picture",
            score: 0.9,
            l: 0.0,
            t: 0.0,
            r: 100.0,
            b: 100.0,
        };
        // Five wide lines at wildly different heights: a 12-pt title, 20-pt OCR
        // boxes over the bars, 4–5-pt tick/axis labels. Coverage and median
        // width both clear the panel thresholds.
        let cells = vec![
            cell("Underground Water Storage", 10.0, 5.0, 90.0, 17.0),
            cell("aquifer recharge zone", 15.0, 30.0, 75.0, 50.0),
            cell("confined | unconfined | perched", 12.0, 55.0, 80.0, 59.0),
            cell("saturated thickness", 8.0, 70.0, 60.0, 90.0),
            cell("distance from well, km", 20.0, 92.0, 85.0, 97.0),
        ];
        let mut regions = vec![chart];
        super::recover_text_panels(&mut regions, &cells);
        assert_eq!(
            regions.iter().map(|r| r.label).collect::<Vec<_>>(),
            ["picture"],
            "ragged line heights mark a figure, not a text panel"
        );
    }

    /// docling serializes a cluster's cells in docling-parse index order
    /// (`_sort_cells`) and joins them with `PageAssembleModel.sanitize_text`:
    /// a space after every line except one ending in `-`, which either fuses a
    /// wrapped word (alnum on both sides — dash dropped) or glues verbatim (a
    /// bare `-` cell: `[0000` `-` `0002` → `[0000 -0002`, the 2305 ORCID line;
    /// `-` + `"C" cell -` + `a new table cell` → `-"C" cell a new table cell`,
    /// its OTSL list). Verified against the corpus: pure index order beats any
    /// geometric re-sort (normal_4pages' heading numerals paint after their
    /// text and belong last: `## 들어가며 1`).
    #[test]
    fn cells_join_in_index_order_with_sanitize_text_rules() {
        let cell = |text: &str, l: f32, t: f32, r: f32, b: f32| TextCell {
            text: text.to_string(),
            l,
            t,
            r,
            b,
        };
        let region = Region {
            label: "text",
            score: 1.0,
            l: 0.0,
            t: 95.0,
            r: 200.0,
            b: 130.0,
        };
        // ORCID superscript: a bare dash cell is a *detached* dash — kept, and
        // since docling#4052 (2.122) it joins with the ordinary space on both
        // sides (`[0000 -0002 -6960]` before that fix).
        let orcid = vec![
            cell("[0000", 10.0, 100.0, 30.0, 110.0),
            cell("−", 30.0, 100.0, 34.0, 110.0),
            cell("0002", 34.0, 100.0, 50.0, 110.0),
            cell("−", 50.0, 100.0, 54.0, 110.0),
            cell("6960]", 54.0, 100.0, 70.0, 110.0),
        ];
        assert_eq!(super::region_text(&region, &orcid), "[0000 - 0002 - 6960]");
        // Wrapped word: dash dropped, lines fused (both boundary words alnum).
        let wrapped = vec![
            cell("platforms-", 10.0, 100.0, 60.0, 110.0),
            cell("reflects the design", 10.0, 112.0, 90.0, 122.0),
        ];
        assert_eq!(
            super::region_text(&region, &wrapped),
            "platformsreflects the design"
        );
        // Dash-ending lines that are *detached* dashes (a bare bullet cell, a
        // `cell -` separator): the dash stays and the lines join with a space
        // — docling#4052; before it they glued (`-"C" cell a new table cell`,
        // 2305's OTSL list bullets).
        let otsl = vec![
            cell("–", 10.0, 100.0, 14.0, 110.0),
            cell("\"C\" cell -", 16.0, 100.0, 60.0, 110.0),
            cell("a new table cell", 10.0, 112.0, 80.0, 122.0),
        ];
        assert_eq!(
            super::region_text(&region, &otsl),
            "- \"C\" cell - a new table cell"
        );
        // Index order is authoritative — no geometric re-sort.
        let numeral = vec![
            cell("들어가며", 30.0, 100.0, 80.0, 110.0),
            cell("1", 10.0, 98.0, 25.0, 112.0), // big numeral painted last
        ];
        assert_eq!(super::region_text(&region, &numeral), "들어가며 1");
    }

    /// The geometric-reliability gate, on the two shapes it has to tell apart.
    #[test]
    fn geometric_reliability_rejects_split_column_grids() {
        let g = |rows: &[&[&str]]| -> Vec<Vec<String>> {
            rows.iter()
                .map(|r| r.iter().map(|c| c.to_string()).collect())
                .collect()
        };
        // A genuine grid: dense, every column carrying entries. Nothing for
        // TableFormer to improve, so geometry is used as-is.
        assert!(super::geometric_table_is_reliable(&g(&[
            &["Datum", "Leistung", "Anzahl", "Kosten"],
            &["04.07", "Internet", "1", "40.30"],
            &["04.07", "Telefon", "2", "8.06"],
        ])));
        // The left-edge split artefact (the shape a scanned invoice produced):
        // one real label column plus values scattered across three sparse ones.
        assert!(!super::geometric_table_is_reliable(&g(&[
            &["www.magenta.at/faq", "", "", ""],
            &["Serviceteam", "", "", ""],
            &["Telefon", "0676/2000", "", ""],
            &["Kundennummer", "", "", "1.21699482"],
            &["Rechnungsnummer", "", "922769430725", ""],
            &["Rechnungsdatum", "", "", "04.07.2025"],
        ])));
        // A column only one row ever uses is a split artefact even when the
        // grid is otherwise dense.
        assert!(!super::geometric_table_is_reliable(&g(&[
            &["a", "b", ""],
            &["c", "d", ""],
            &["e", "f", "g"],
        ])));
        // Degenerate shapes are never vouched for — TableFormer may recover
        // structure a collapsed reconstruction lost.
        assert!(!super::geometric_table_is_reliable(&g(&[&[
            "only one column"
        ]])));
        assert!(!super::geometric_table_is_reliable(&[]));
    }

    /// A `picture` region is cropped out of the rendered page, whatever built
    /// that page. The browser pipeline (#157) has no pdfium but does hand over
    /// the rasterized bitmap through `from_cells_with_image`, so it must get
    /// the same figure bytes the native path does — that is what makes
    /// `images = "embedded"` inline real pixels instead of a placeholder.
    #[cfg(feature = "ocr-prep")]
    #[test]
    fn picture_regions_are_cropped_from_a_host_supplied_page_image() {
        let mut img = image::RgbImage::new(200, 200);
        // Paint the figure area so the crop is distinguishable from the page.
        for y in 100..160 {
            for x in 20..120 {
                img.put_pixel(x, y, image::Rgb([255, 0, 0]));
            }
        }
        // scale 2.0: the region is in page points, the bitmap in pixels.
        let page = PdfPage::from_cells_with_image(100.0, 100.0, 2.0, Vec::new(), img);
        let region = Region {
            label: "picture",
            score: 0.9,
            l: 10.0,
            t: 50.0,
            r: 60.0,
            b: 80.0,
        };
        let (nodes, _) = super::assemble_page(&page, vec![region], &[None], &[None], None);
        // Layout-derived nodes carry provenance, so the picture arrives wrapped.
        let image = nodes
            .iter()
            .find_map(|n| match n {
                Node::Located { inner, .. } => match &**inner {
                    Node::Picture { image, .. } => image.as_ref(),
                    _ => None,
                },
                Node::Picture { image, .. } => image.as_ref(),
                _ => None,
            })
            .expect("a picture node with cropped pixels");
        assert_eq!(image.mimetype, "image/png");
        assert_eq!((image.width, image.height), (100, 60), "region × scale");
        assert!(!image.data.is_empty(), "PNG bytes were encoded");
    }

    #[test]
    fn link_anchors_split_a_shared_word_cell_between_adjacent_links() {
        // A common header layout: one text run holds several pipe-separated
        // labels, each carrying its own link annotation. Every link must get
        // its own label as the anchor (and the "|" separators must belong to
        // none), not the whole run.
        let annot = |l: f32, r: f32, uri: &str| LinkAnnot {
            l,
            t: 100.0,
            r,
            b: 114.0,
            uri: uri.into(),
        };
        let page = PdfPage {
            width: 600.0,
            height: 800.0,
            scale: 2.0,
            cells: Vec::new(),
            code_cells: Vec::new(),
            checkboxes: Vec::new(),
            bitmaps: Vec::new(),
            // "LinkedIn | GitHub | Credly" = 26 chars over x 100..360.
            word_cells: vec![cell(
                "LinkedIn | GitHub | Credly",
                100.0,
                100.0,
                360.0,
                114.0,
            )],
            image: image::RgbImage::new(1, 1),
            image_layout: None,
            links: vec![
                annot(100.0, 180.0, "https://l"),
                annot(200.0, 260.0, "https://g"),
                annot(290.0, 360.0, "https://c"),
            ],
            rotation: 0,
            keep_typography: false,
            #[cfg(feature = "render")]
            embedded_images: Vec::new(),
            #[cfg(feature = "ml")]
            pending_image: None,
        };
        assert_eq!(
            resolve_link_anchors(&page),
            vec![
                ("LinkedIn".to_string(), "https://l".to_string()),
                ("GitHub".to_string(), "https://g".to_string()),
                ("Credly".to_string(), "https://c".to_string()),
            ]
        );
    }

    /// A one-line code cell at `[l, r] × [t, b]` (top-left coords).
    fn cell(text: &str, l: f32, t: f32, r: f32, b: f32) -> TextCell {
        TextCell {
            text: text.into(),
            l,
            t,
            r,
            b,
        }
    }

    /// OCR-path grouping (docling's `_remove_overlapping_clusters("regular")`):
    /// the low-score paragraph box RT-DETR draws over its own high-score line
    /// boxes collapses to one region — the group's union, with the survivor's
    /// label and score — so region-scoped OCR reads each line once. Regions
    /// that merely sit near each other, and specials, are untouched.
    #[test]
    fn merge_overlapping_regulars_collapses_a_block_over_its_lines() {
        let mut regions = vec![
            region("text", 0.84, 60.0, 186.0, 270.0, 198.0),
            region("text", 0.80, 60.0, 160.0, 294.0, 172.0),
            region("text", 0.79, 59.0, 107.0, 272.0, 119.0),
            // The paragraph box, lower score, containing all three lines.
            region("text", 0.52, 59.0, 107.0, 295.0, 200.0),
            // Elsewhere on the page: stays as is.
            region("section_header", 0.77, 60.0, 71.0, 253.0, 86.0),
            // A picture the block overlaps is not a regular — never grouped.
            region("picture", 0.9, 50.0, 100.0, 300.0, 210.0),
        ];
        merge_overlapping_regulars(&mut regions);
        assert_eq!(regions.len(), 3, "{regions:?}");
        let block = regions
            .iter()
            .find(|r| r.label == "text")
            .expect("one text");
        // docling keeps the largest passing candidate unless a rival is both
        // comparable in size and > 0.05 more confident; the 16× larger block
        // passes, and a smaller line never replaces a larger current best.
        // Either way the survivor spans the whole group.
        assert_eq!(
            (block.l, block.t, block.r, block.b),
            (59.0, 107.0, 295.0, 200.0)
        );
        assert!(regions.iter().any(|r| r.label == "section_header"));
        assert!(regions.iter().any(|r| r.label == "picture"));
    }

    /// The pairwise rules, each in the arrangement where it decides the
    /// outcome: docling seeds the survivor with the group's first passing
    /// cluster and a later one replaces it only when larger *and* within
    /// 0.05 confidence, so a rule that merely lets a cluster pass matters
    /// exactly when that cluster comes first — a same-sized list item ahead
    /// of a far more confident text box, a code box ahead of the text it
    /// contains. Without the rule either would be rejected outright (similar
    /// size, rival > 0.05 more confident) and the text box would win.
    #[test]
    fn merge_overlapping_regulars_follows_the_preference_rules() {
        let mut regions = vec![
            region("list_item", 0.6, 0.0, 0.0, 102.0, 20.0),
            region("text", 0.9, 0.0, 0.0, 100.0, 20.0),
        ];
        merge_overlapping_regulars(&mut regions);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].label, "list_item");

        let mut regions = vec![
            region("code", 0.6, 0.0, 0.0, 100.0, 100.0),
            region("text", 0.9, 2.0, 2.0, 98.0, 98.0),
        ];
        merge_overlapping_regulars(&mut regions);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].label, "code");

        // No rule applies: a near-identical rival that is > 0.05 more
        // confident rejects the candidate whatever the order.
        for order in [[0.9, 0.6], [0.6, 0.9]] {
            let mut regions = vec![
                region("text", order[0], 0.0, 0.0, 100.0, 20.0),
                region("text", order[1], 0.0, 0.0, 105.0, 21.0),
            ];
            merge_overlapping_regulars(&mut regions);
            assert_eq!(regions.len(), 1);
            assert_eq!(regions[0].score, 0.9, "the confident twin wins");
            assert_eq!(
                (regions[0].r, regions[0].b),
                (105.0, 21.0),
                "on the union box"
            );
        }

        // Side by side (no containment, IoU 0): nothing to merge.
        let mut regions = vec![
            region("text", 0.9, 0.0, 0.0, 100.0, 20.0),
            region("text", 0.9, 0.0, 22.0, 100.0, 42.0),
        ];
        merge_overlapping_regulars(&mut regions);
        assert_eq!(regions.len(), 2);
    }

    #[test]
    fn footer_under_a_body_less_heading_is_its_text() {
        // The reporting CV's last page: `## Languages` at t=773.7..783.4 and
        // its one-line body at 798.5..808.7, labelled page_footer (A4, 595 pt).
        let mut regions = vec![
            region("list_item", 0.9, 66.0, 754.0, 353.0, 765.0),
            region("section_header", 0.94, 43.0, 773.7, 89.5, 783.4),
            region("page_footer", 0.88, 43.0, 798.5, 527.8, 808.7),
        ];
        reclaim_heading_body_footers(&mut regions, 595.28);
        assert_eq!(regions[2].label, "text");
        assert_eq!(regions[1].label, "section_header");

        // A heading with its own paragraph and a running footer below: kept.
        let mut regions = vec![
            region("section_header", 0.9, 43.0, 700.0, 120.0, 710.0),
            region("text", 0.9, 43.0, 714.0, 520.0, 780.0),
            region("page_footer", 0.9, 43.0, 798.0, 520.0, 808.0),
        ];
        reclaim_heading_body_footers(&mut regions, 595.28);
        assert_eq!(regions[2].label, "page_footer");

        // A page number under a trailing heading is too narrow to be a body.
        let mut regions = vec![
            region("section_header", 0.9, 43.0, 773.0, 120.0, 783.0),
            region("page_footer", 0.9, 280.0, 798.0, 300.0, 808.0),
        ];
        reclaim_heading_body_footers(&mut regions, 595.28);
        assert_eq!(regions[1].label, "page_footer");

        // Too far below the heading (a real footer after a heading that ends
        // the page): kept.
        let mut regions = vec![
            region("section_header", 0.9, 43.0, 700.0, 120.0, 710.0),
            region("page_footer", 0.9, 43.0, 798.0, 520.0, 808.0),
        ];
        reclaim_heading_body_footers(&mut regions, 595.28);
        assert_eq!(regions[1].label, "page_footer");
    }

    fn region(label: &'static str, score: f32, l: f32, t: f32, r: f32, b: f32) -> Region {
        Region {
            label,
            score,
            l,
            t,
            r,
            b,
        }
    }

    #[test]
    fn resolve_collapses_nested_code_keeping_the_larger_box() {
        // A tight high-score `code` box and a taller lower-score near-duplicate that
        // contains it must collapse to one — the *larger* box, so every cell stays
        // covered and nothing leaks out as orphan text.
        let tight = region("code", 0.95, 78.0, 292.0, 300.0, 330.0);
        let wide = region("code", 0.66, 63.0, 260.0, 320.0, 346.0);
        let kept = super::resolve(vec![tight, wide]);
        assert_eq!(kept.len(), 1, "nested code boxes must collapse to one");
        assert!(
            kept[0].l == 63.0 && kept[0].b == 346.0,
            "the larger containing box is kept"
        );
    }

    #[test]
    fn resolve_keeps_distinct_and_differently_typed_regions() {
        // A text box fully inside a lower-score *table* must NOT be collapsed (the
        // code dedup is code-only), and two separate code blocks stay separate.
        let text = region("text", 0.95, 90.0, 210.0, 200.0, 230.0);
        let table = region("table", 0.60, 80.0, 200.0, 400.0, 500.0);
        assert_eq!(super::resolve(vec![text, table]).len(), 2);

        let code_a = region("code", 0.9, 78.0, 100.0, 300.0, 140.0);
        let code_b = region("code", 0.9, 78.0, 300.0, 300.0, 360.0); // far below, no overlap
        assert_eq!(super::resolve(vec![code_a, code_b]).len(), 2);
    }

    /// A two-column glossary page came out as three column
    /// tables *and* one low-score whole-page table over them. docling's wrapper
    /// `_remove_overlapping_clusters` keeps one table per overlapping group
    /// (here the whole-page one: > 2× every rival's area and ≤ 0.2 less
    /// confident than the running best); `greedy` alone kept all four and
    /// emitted every cell twice.
    #[test]
    fn resolve_keeps_one_table_per_nested_group() {
        let kept = super::resolve(vec![
            region("table", 0.71, 26.0, 203.0, 183.0, 558.0),
            region("table", 0.67, 26.0, 55.0, 183.0, 196.0),
            region("table", 0.66, 196.0, 56.0, 354.0, 561.0),
            region("table", 0.53, 25.0, 53.0, 354.0, 561.0),
        ]);
        assert_eq!(kept.len(), 1, "one survivor per overlapping group");
        assert_eq!((kept[0].l, kept[0].b), (25.0, 561.0));
        // Side-by-side tables that don't overlap stay separate.
        let kept = super::resolve(vec![
            region("table", 0.9, 26.0, 55.0, 183.0, 558.0),
            region("table", 0.9, 196.0, 56.0, 354.0, 561.0),
        ]);
        assert_eq!(kept.len(), 2);
    }

    /// A dense data table detected as both picture (0.80) and table (0.62) on one box.
    /// The picture is ≥ 0.1 more confident, so `_handle_cross_type_overlaps`
    /// keeps both, and the dense table text passed the text-panel gates: the
    /// demoted paragraph repeated every cell the table grid renders. A
    /// paragraph > 80 % inside a surviving table is the table's child and is
    /// not emitted; a panel with no table under it still demotes.
    #[test]
    fn text_panel_over_a_table_does_not_repeat_its_cells() {
        let lines = |t0: f32| -> Vec<TextCell> {
            (0..4)
                .map(|i| {
                    let t = t0 + 10.0 * i as f32;
                    cell("1 2 3 4 5 6 7 8 9 10 11 12", 5.0, t, 95.0, t + 8.0)
                })
                .collect()
        };
        let mut cells = lines(0.0);
        cells.extend(lines(200.0));
        let mut regions = vec![
            region("picture", 0.80, 0.0, 0.0, 100.0, 45.0),
            region("table", 0.62, 0.0, 0.0, 100.0, 45.0),
            region("picture", 0.80, 0.0, 200.0, 100.0, 245.0),
        ];
        super::recover_text_panels(&mut regions, &cells);
        assert_eq!(
            regions.iter().map(|r| r.label).collect::<Vec<_>>(),
            ["table", "text"]
        );
        assert_eq!(regions[1].t, 200.0, "the table-free panel still demotes");
    }

    /// docling's `ListItemMarkerProcessor` + docling-core's Markdown list rules
    /// (see `list_item_node`): a bullet glyph or `-` is split off and the
    /// serializer's own `-` printed (2305's OTSL list reads `- "C" cell`, not
    /// `- - "C" cell`); `N.` is an ordered item; any other marker with a letter
    /// or digit stays in the text behind the bullet; no marker → plain bullet.
    #[test]
    fn list_item_markers_split_like_docling() {
        // The marker as DocLang reads it: the overlay's, else the node's.
        let text_of = |n: &Node| match n {
            Node::ListItem {
                ordered,
                number,
                text,
                marker,
                dclx,
                ..
            } => (
                *ordered,
                *number,
                text.clone(),
                dclx.as_ref().map_or(marker.clone(), |d| d.marker.clone()),
            ),
            other => panic!("{other:?}"),
        };
        let loc = [0, 0, 100, 10];
        assert_eq!(
            text_of(&super::list_item_node(
                "- \"C\" cell - a new table cell",
                loc,
                false
            )),
            (
                false,
                0,
                "\"C\" cell - a new table cell".into(),
                Some("-".into())
            )
        );
        assert_eq!(
            text_of(&super::list_item_node("• Bullet text", loc, false)),
            (false, 0, "Bullet text".into(), Some("•".into()))
        );
        assert_eq!(
            text_of(&super::list_item_node("3. Third step", loc, false)),
            (true, 3, "Third step".into(), Some("3.".into()))
        );
        assert_eq!(
            text_of(&super::list_item_node("a) Option", loc, false)),
            (false, 0, "a) Option".into(), Some("a)".into()))
        );
        assert_eq!(
            text_of(&super::list_item_node("1.2 Nested outline", loc, false)),
            (false, 0, "1.2 Nested outline".into(), Some("1.2".into()))
        );
        // 2203's `3.a. If all IOU scores…`: a compound marker is not an item
        // number — docling prints `- 3.a. If all…`.
        assert_eq!(
            text_of(&super::list_item_node("3.a. If all IOU scores", loc, false)),
            (
                false,
                0,
                "3.a. If all IOU scores".into(),
                Some("3.a.".into())
            )
        );
        // A glued symbol-font bullet is stripped, a spaced one is the marker.
        assert_eq!(
            text_of(&super::list_item_node("•Glued", loc, false)),
            (false, 0, "Glued".into(), Some("·".into()))
        );
        // docling#5043's `test_private_use_font_bullets_are_markers`: the
        // Symbol/Wingdings bullets as private-use code points (a Word-exported
        // PDF's text layer) are bullet markers, never part of the item text.
        for bullet in ['\u{F0B7}', '\u{F0A7}', '\u{F0D8}'] {
            let input = format!("{bullet} Transcribo: converts audio to text");
            assert_eq!(
                text_of(&super::list_item_node(&input, loc, false)),
                (
                    false,
                    0,
                    "Transcribo: converts audio to text".into(),
                    Some(bullet.to_string())
                )
            );
            assert_eq!(super::split_list_marker(&input).map(|m| m.2), Some(false));
            assert_eq!(super::checkbox_glyph(bullet), None);
        }
        for bullet in super::PUA_FONT_BULLETS {
            assert!(super::LIST_BULLET_MARKERS.contains(bullet), "{bullet:?}");
            // Glued to the text it is stripped like `•Glued`.
            assert_eq!(
                text_of(&super::list_item_node(
                    &format!("{bullet}Glued"),
                    loc,
                    false
                )),
                (false, 0, "Glued".into(), Some("·".into()))
            );
        }
        // No whitespace after the glyph → not a marker (docling's `\s` is required).
        assert_eq!(
            text_of(&super::list_item_node("-5 degrees", loc, false)),
            (false, 0, "-5 degrees".into(), Some("·".into()))
        );
        // The remaining numbered shapes, first-wins like docling's list.
        for (input, marker, body) in [
            ("1.2.3. Deep", "1.2.3.", "Deep"),
            ("9a) Nine-a", "9a)", "Nine-a"),
            ("(3.a) Paren", "(3.a)", "Paren"),
            ("12) Twelve", "12)", "Twelve"),
            ("(4) Four", "(4)", "Four"),
            ("[7] Seven", "[7]", "Seven"),
            ("iv. Roman", "iv.", "Roman"),
            ("IX. Roman", "IX.", "Roman"),
            ("b. Letter", "b.", "Letter"),
            ("B) Letter", "B)", "Letter"),
        ] {
            assert_eq!(
                super::split_list_marker(input),
                Some((marker, body, true)),
                "{input}"
            );
        }
        // A `1.2.` whose optional dot would eat the separator backtracks like
        // Python's regex; a marker with nothing after the whitespace is none.
        assert_eq!(
            super::split_list_marker("1.2.\tx"),
            Some(("1.2.", "x", true))
        );
        assert_eq!(super::split_list_marker("1. "), None);
        assert_eq!(super::split_list_marker("• "), None);
        assert_eq!(
            text_of(&super::list_item_node("Plain item", loc, false)),
            (false, 0, "Plain item".into(), Some("·".into()))
        );
    }

    /// A `case_auto` marker is printed once: it rides in the item's text, so
    /// the Markdown serializer — which applies docling-core's AUTO rule to a
    /// marker it is handed — must not be handed it too (`- (1) (1) text`, the
    /// four drifted PDF snapshots of the docling 2.142 serializer change).
    /// The bullet glyph and `N.` shapes keep their Markdown marker.
    #[test]
    fn case_auto_marker_prints_once_in_markdown() {
        let loc = [0, 0, 100, 10];
        let nodes: Vec<Node> = [
            "(1) Human annotation",
            "[21] Peng Zhang",
            "a) Option",
            "• Bullet",
            "3. Third",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| super::list_item_node(t, loc, i == 0))
        .collect();
        for n in &nodes[..3] {
            let Node::ListItem { marker, dclx, .. } = n else {
                unreachable!()
            };
            assert_eq!(marker, &None, "{n:?}");
            assert!(dclx.as_ref().is_some_and(|d| d.marker.is_some()), "{n:?}");
        }
        let mut doc = docling_core::DoclingDocument::new("t");
        doc.nodes = nodes;
        let md = doc.export_to_markdown();
        for line in [
            "- (1) Human annotation",
            "- [21] Peng Zhang",
            "- a) Option",
            "- Bullet",
            "3. Third",
        ] {
            assert!(md.lines().any(|l| l == line), "missing {line:?} in:\n{md}");
        }
        assert!(!md.contains("(1) (1)") && !md.contains("[21] [21]"), "{md}");
    }

    #[test]
    fn code_language_label_above_code_is_detected() {
        // A bare "XML" token directly above a code box is a language label; a real
        // heading above the same code is not; a language word with no code below is
        // left alone.
        let label = region("section_header", 0.9, 76.0, 540.0, 96.0, 549.0);
        let code = region("code", 0.7, 77.0, 552.0, 290.0, 640.0);
        let heading = region("section_header", 0.9, 76.0, 500.0, 260.0, 512.0);
        let cells = vec![
            cell("XML", 78.0, 541.0, 94.0, 548.0),       // inside `label`
            cell("Overview", 78.0, 501.0, 250.0, 511.0), // inside `heading`
        ];
        let drop = super::code_language_labels(&[label, code, heading], &cells);
        assert_eq!(drop, vec![true, false, false], "only the label is consumed");

        // Same label with no code region present → not consumed.
        let label2 = region("section_header", 0.9, 76.0, 540.0, 96.0, 549.0);
        let only = vec![cell("XML", 78.0, 541.0, 94.0, 548.0)];
        assert_eq!(super::code_language_labels(&[label2], &only), vec![false]);

        // A label swallowed into the top of a wider code box (negative gap) is still
        // recognized.
        let inside_lbl = region("text", 0.9, 76.0, 540.0, 96.0, 549.0);
        let wide_code = region("code", 0.7, 63.0, 531.0, 320.0, 654.0);
        let cells2 = vec![cell("XML", 78.0, 541.0, 94.0, 548.0)];
        assert_eq!(
            super::code_language_labels(&[inside_lbl, wide_code], &cells2),
            vec![true, false]
        );

        assert!(super::is_code_language("XML") && super::is_code_language("c#"));
        assert!(!super::is_code_language("Configure") && !super::is_code_language("XML schema"));
    }

    #[test]
    fn code_region_text_keeps_lines_and_indentation() {
        // Three source lines; each glyph is 6 units wide (width / chars = 6), so the
        // `int X;` line indented to x=22 is (22-10)/6 = 2 spaces in.
        let region = Region {
            label: "code",
            score: 1.0,
            l: 0.0,
            t: -5.0,
            r: 100.0,
            b: 40.0,
        };
        let cells = vec![
            cell("struct P {", 10.0, 0.0, 70.0, 10.0),
            cell("int X;", 22.0, 12.0, 58.0, 22.0),
            cell("}", 10.0, 24.0, 16.0, 34.0),
        ];
        assert_eq!(code_region_text(&region, &cells), "struct P {\n  int X;\n}");
    }

    #[test]
    fn code_region_text_tightens_punctuation_without_eating_indentation() {
        // A fluent `.Foo()` line at x=22 (2 chars in). Per-line tightening must not
        // consume the leading indent space by matching " ." across it.
        let region = Region {
            label: "code",
            score: 1.0,
            l: 0.0,
            t: -5.0,
            r: 100.0,
            b: 40.0,
        };
        let cells = vec![
            cell("builder", 10.0, 0.0, 52.0, 10.0),
            // pdfium spaced the call: ".Foo (x)" tightens to ".Foo(x)", still 2-indented.
            cell(".Foo (x)", 22.0, 12.0, 70.0, 22.0),
        ];
        assert_eq!(code_region_text(&region, &cells), "builder\n  .Foo(x)");
    }

    #[test]
    fn code_region_text_orders_out_of_order_cells_and_ignores_blank_lines() {
        let region = Region {
            label: "code",
            score: 1.0,
            l: 0.0,
            t: -5.0,
            r: 100.0,
            b: 60.0,
        };
        // Fed bottom-up and with a whitespace-only cell; output is top-down, no blank.
        let cells = vec![
            cell("b();", 10.0, 24.0, 34.0, 34.0),
            cell("   ", 10.0, 12.0, 20.0, 22.0),
            cell("a();", 10.0, 0.0, 34.0, 10.0),
        ];
        assert_eq!(code_region_text(&region, &cells), "a();\nb();");
        // No code cells → empty, so the caller falls back to the prose text.
        assert_eq!(code_region_text(&region, &[]), "");
    }

    fn para(text: &str) -> Node {
        Node::Paragraph { text: text.into() }
    }

    /// Run a node sequence through [`StreamAssembler`] with the given page splits
    /// and assert the flushed result equals one-shot [`merge_continuations`].
    fn assert_stream_eq(nodes: &[Node], splits: &[usize]) {
        let mut want = nodes.to_vec();
        merge_continuations(&mut want);

        let mut asm = StreamAssembler::new();
        let mut got = Vec::new();
        let mut start = 0;
        for &end in splits {
            got.extend(asm.push(nodes[start..end].to_vec()));
            start = end;
        }
        got.extend(asm.push(nodes[start..].to_vec()));
        got.extend(asm.finish());
        assert_eq!(got, want, "stream assembly diverged (splits={splits:?})");
    }

    #[test]
    fn running_head_floats_to_page_front() {
        let cells = vec![
            cell("HEAD — 7", 350.0, 40.0, 500.0, 50.0),
            cell("marker", 70.0, 70.0, 90.0, 80.0),
            cell("body", 100.0, 90.0, 300.0, 100.0),
        ];
        let mut nodes = vec![
            Node::PageInfo {
                page_no: 1,
                width: 600.0,
                height: 800.0,
            },
            para("marker"),
            para("body"),
            para("HEAD — 7"),
        ];
        float_running_head(&mut nodes, &cells);
        assert_eq!(nodes[1], para("HEAD — 7"));
        assert_eq!(nodes[3], para("body"));
    }

    #[test]
    fn stream_assembler_matches_merge_continuations() {
        // Open fragment + lowercase continuation split across a page boundary.
        let cross = [para("the definition of"), para("lists in scope")];
        assert_stream_eq(&cross, &[1]);
        assert_stream_eq(&cross, &[]);

        // Continuation that wraps around a figure (+ its caption) on the boundary.
        let wrap = [
            para("the wing type that is"),
            Node::Picture {
                caption: None,
                caption_href: None,
                image: None,
                classification: None,
                description: None,
                caption_parent: Default::default(),
                caption_location: None,
            },
            para("Fig. 1. a diagram"),
            para("the most common kind"),
        ];
        for splits in [&[][..], &[1][..], &[2][..], &[3][..], &[1, 3][..]] {
            assert_stream_eq(&wrap, splits);
        }

        // A heading between fragments blocks the merge (must still flush correctly).
        let blocked = [
            para("ends mid word and"),
            Node::Heading {
                level: 2,
                text: "New Section".into(),
            },
            para("more body here"),
        ];
        for splits in [&[][..], &[1][..], &[2][..]] {
            assert_stream_eq(&blocked, splits);
        }

        // A chain across three pages: each page is one open lowercase fragment.
        let chain = [
            para("alpha beta"),
            para("gamma delta"),
            para("epsilon zeta"),
        ];
        assert_stream_eq(&chain, &[1, 2]);
    }

    fn item(text: &str) -> Node {
        Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: false,
            text: text.into(),
            level: 0,
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        }
    }

    fn icon(x: u16) -> Node {
        super::located(
            [x, 0, x + 10, 10],
            Node::Picture {
                caption: None,
                caption_href: None,
                image: None,
                classification: None,
                description: None,
                caption_parent: Default::default(),
                caption_location: None,
            },
        )
    }

    fn page(no: usize) -> Node {
        Node::PageInfo {
            page_no: no,
            width: 600.0,
            height: 800.0,
        }
    }

    /// docling keeps a list open past a picture, table or code block: the
    /// icon beside a procedure's second step reads after the procedure, which
    /// stays one list; a paragraph closes the list and puts the icon down.
    /// Without an open list nothing moves.
    #[test]
    fn a_list_continues_past_a_picture_as_in_docling() {
        let heading = || Node::Heading {
            level: 2,
            text: "Cleaning".into(),
        };
        let mut nodes = vec![
            page(1),
            heading(),
            item("Unplug the appliance."),
            icon(1),
            item("Wipe the base."),
            item("Dry it."),
            para("Then plug it in again."),
            icon(2),
            item("Store it."),
            icon(3),
        ];
        let flags = vec![false; nodes.len()];
        super::continue_lists_past_asides(&mut nodes, &flags);
        assert_eq!(
            nodes,
            vec![
                page(1),
                heading(),
                item("Unplug the appliance."),
                item("Wipe the base."),
                item("Dry it."),
                icon(1),
                para("Then plug it in again."),
                icon(2),
                item("Store it."),
                icon(3),
            ]
        );
    }

    /// In a form area docling splits a list on any other element: a picture
    /// or table between two of the area's items stays between them.
    #[test]
    fn a_form_area_list_stops_at_a_table() {
        let mut nodes = vec![page(1), item("22 | first"), icon(1), item("23 | second")];
        let want = nodes.clone();
        super::continue_lists_past_asides(&mut nodes, &[false, true, true, true]);
        assert_eq!(nodes, want);
    }

    #[test]
    fn clean_text_dehyphenates_and_normalizes_typography() {
        // U+0002 line-wrap hyphen + the join space → merged word (like docling).
        assert_eq!(clean_text("com\u{2} pact"), "compact");
        assert_eq!(clean_text("end-to\u{2} end deep"), "end-toend deep");
        // A stray wrap hyphen (no following join) is dropped.
        assert_eq!(clean_text("word\u{2}"), "word");
        // Typographic punctuation → ASCII (docling-parse's sanitizer table):
        // single curly quotes become `'`, double ones `"` (docling-parse#349).
        assert_eq!(
            clean_text("Graph\u{2019}s \u{201c}x\u{201d} \u{201e}y\u{201c}"),
            "Graph's \"x\" \"y\""
        );
        assert_eq!(clean_text("a\u{2026}"), "a...");
        // The docling-parse sanitizer's internal spacing is preserved as
        // placed; line breaks/tabs normalize to a space, ends trim.
        assert_eq!(clean_text("a   b\nc"), "a   b c");
    }

    /// docling#4064: a form's children are emitted together where the form
    /// sits in the top-level order, not interleaved with surrounding text.
    #[test]
    fn form_children_stay_together_in_reading_order() {
        let reg = |label: &'static str, l: f32, t: f32, r: f32, b: f32| Region {
            label,
            score: 0.9,
            l,
            t,
            r,
            b,
        };
        // Page: intro text, then a form spanning the left column with two
        // fields and a table inside, while a right-column paragraph sits
        // level with the form's first field (it would otherwise be read
        // between the form's children).
        let mut items = vec![
            reg("text", 50.0, 50.0, 550.0, 70.0),    // 0 intro
            reg("form", 50.0, 100.0, 300.0, 400.0),  // 1 container
            reg("text", 60.0, 110.0, 290.0, 130.0),  // 2 field A (child)
            reg("text", 320.0, 110.0, 550.0, 130.0), // 3 right column paragraph
            reg("table", 60.0, 150.0, 290.0, 300.0), // 4 table (child)
            reg("text", 60.0, 320.0, 290.0, 340.0),  // 5 field B (child)
            reg("text", 50.0, 450.0, 550.0, 470.0),  // 6 outro
        ];
        let cids = super::cluster_cids(&items, &[]);
        super::order_with_containers(&mut items, &cids, 600.0, 800.0, |r| r);
        let order: Vec<(&str, f32)> = items.iter().map(|r| (r.label, r.t)).collect();
        // The form block (container, then its children top-down) is one unit.
        let form_pos = order.iter().position(|(l, _)| *l == "form").unwrap();
        assert_eq!(
            &order[form_pos..form_pos + 4],
            &[
                ("form", 100.0),
                ("text", 110.0),
                ("table", 150.0),
                ("text", 320.0)
            ]
        );
        assert_eq!(order[0], ("text", 50.0));
        assert_eq!(order[order.len() - 1], ("text", 450.0));
        // Without a container the plain order interleaves by geometry.
        let mut flat: Vec<Region> = items
            .iter()
            .filter(|r| r.label != "form")
            .cloned()
            .collect();
        let cids = super::cluster_cids(&flat, &[]);
        super::order_regions(&mut flat, &cids, 600.0, 800.0, |r| r);
        assert_ne!(
            flat.iter().map(|r| r.t).collect::<Vec<_>>(),
            order
                .iter()
                .filter(|(l, _)| *l != "form")
                .map(|(_, t)| *t)
                .collect::<Vec<_>>()
        );
    }

    /// docling#3906: a picture inside a table lands in the covering cell,
    /// chosen by the picture's inferred grid position when cell boxes overlap.
    #[test]
    fn picture_matches_the_cell_at_its_grid_position() {
        let cell = |r: usize, c: usize, bbox: [f32; 4]| docling_core::TableCell {
            text: format!("r{r}c{c}"),
            bbox: Some(bbox),
            start_row: r,
            start_col: c,
            row_span: 1,
            col_span: 1,
            column_header: false,
            row_header: false,
            row_section: false,
        };
        // 2×2 grid; the (1,0) cell box is generous and also covers the picture.
        let cells = vec![
            cell(0, 0, [0.0, 0.0, 100.0, 50.0]),
            cell(0, 1, [100.0, 0.0, 200.0, 50.0]),
            cell(1, 0, [0.0, 50.0, 100.0, 100.0]),
            cell(1, 1, [100.0, 50.0, 200.0, 100.0]),
        ];
        let pic = Region {
            label: "picture",
            score: 0.9,
            l: 110.0,
            t: 60.0,
            r: 190.0,
            b: 95.0,
        };
        assert_eq!(super::match_picture_to_cell(&pic, &cells), Some((1.0, 3)));
        // A picture only half inside any cell is not nested.
        let straddling = Region {
            label: "picture",
            score: 0.9,
            l: 60.0,
            t: 60.0,
            r: 160.0,
            b: 95.0,
        };
        assert_eq!(super::match_picture_to_cell(&straddling, &cells), None);
    }

    /// docling#4052 (2.122): a line-final dash fuses the wrapped word only
    /// when attached to it; a detached dash is a literal and the lines join
    /// with a space.
    #[test]
    fn line_final_hyphen_fuses_only_when_attached_to_a_word() {
        let line = |text: &str, t: f32| TextCell {
            text: text.to_string(),
            l: 0.0,
            t,
            r: 100.0,
            b: t + 10.0,
        };
        // `algo-` / `rithms`: attached hyphen, alnum on both sides → fused.
        assert_eq!(
            cells_text(vec![&line("algo-", 0.0), &line("rithms", 12.0)]),
            "algorithms"
        );
        // `pp. 545-` / `561`: attached, digits count as alnum → `545561` (upstream).
        assert_eq!(
            cells_text(vec![&line("pp. 545-", 0.0), &line("561", 12.0)]),
            "pp. 545561"
        );
        // A dash after whitespace — a separator or a lone `-` cell — is kept and
        // the lines take the ordinary joining space.
        assert_eq!(
            cells_text(vec![&line("range -", 0.0), &line("wide", 12.0)]),
            "range - wide"
        );
        assert_eq!(
            cells_text(vec![&line("-", 0.0), &line("item", 12.0)]),
            "- item"
        );
        // Attached but the next line opens with no word (`x-` / `...`): dash
        // kept and, as before, no separating space.
        assert_eq!(
            cells_text(vec![&line("x-", 0.0), &line("...", 12.0)]),
            "x-..."
        );
    }

    #[test]
    fn lam_alef_only_swaps_a_genuinely_reversed_ligature() {
        // A mid-word `alef-variant + lam` is pdfium's reversed lam-alef ligature and
        // is swapped back to logical `lam + alef-variant` (`ب أ ل` → `ب ل أ`).
        assert_eq!(
            clean_text("\u{0628}\u{0623}\u{0644}"),
            "\u{0628}\u{0644}\u{0623}"
        );
        // But when the alef-variant is *already* preceded by a lam it is the logical
        // ligature `لآ`; the following lam is the next syllable's letter and must not
        // move. `التعلم الآلي` must stay `الآلي`, not become `اللآي`.
        assert_eq!(
            clean_text("\u{0627}\u{0644}\u{0622}\u{0644}\u{064a}"),
            "\u{0627}\u{0644}\u{0622}\u{0644}\u{064a}"
        );
    }

    /// The #419 page, in points: three layout boxes over one paragraph, two of
    /// them ending partway through a line. The sliced lines miss the 0.2 claim
    /// and become orphans; the third model box starts above the second orphan,
    /// so unfitted the reading order emits that box first and strands the line.
    fn sliced_paragraph() -> (Vec<Region>, Vec<TextCell>) {
        let line = |text: &str, t: f32, r: f32| cell(text, 60.0, t, r, t + 11.0);
        let cells = vec![
            line("The mission of this series is to improve", 135.0, 458.0),
            line("The books in this series are technical,", 147.0, 458.0),
            line("substantial. The authors are", 159.0, 458.0),
            line("highly experienced craftsmen and", 171.5, 458.0), // sliced: 1.5/11 under box A
            line("actually works in practice, as opposed", 185.0, 458.0),
            line("about what the author has done, not", 197.0, 458.0),
            line("about programming, there will be lots", 210.5, 458.0), // sliced: 1.5/11 under box B
            line("will be lots of case studies from real", 223.0, 206.0), // C's line
        ];
        let regions = vec![
            region("text", 0.9, 60.0, 132.0, 458.0, 173.0), // A: three lines + a sliver of the 4th
            region("text", 0.9, 60.0, 184.0, 458.0, 212.0), // B: two lines + a sliver of the 7th
            region("text", 0.9, 60.0, 216.0, 206.0, 227.0), // C: last line, box opening 5.5pt too early
        ];
        (regions, cells)
    }

    fn ordered_texts(regions: &[Region], cells: &[TextCell]) -> Vec<String> {
        let mut items: Vec<Region> = regions.to_vec();
        let cids = super::cluster_cids(&items, cells);
        super::order_regions(&mut items, &cids, 500.0, 700.0, |r| r);
        super::region_texts_exclusive(&items, cells)
            .into_iter()
            .map(|t| t.chars().take(9).collect())
            .collect()
    }

    /// #419: fitted to its cells, a model box that cut a line in half no longer
    /// overlaps the orphan that line became, so the orphan orders where it
    /// reads; unfitted, the same page strands the line after the paragraph.
    #[test]
    fn fitting_boxes_to_cells_puts_a_sliced_line_back_in_order() {
        let (mut regions, cells) = sliced_paragraph();
        super::add_orphan_regions(&mut regions, &cells);
        assert_eq!(regions.len(), 5, "two orphan lines");
        // The defect, for the record: C (top 216) is not strictly below the
        // orphan at 210.5–221.5, so the vertical pass alone orders C first.
        // The slivers are 1.5 pt, inside the 0.25 % of the page height
        // docling#4276 lets consecutive text overlap, so its link already
        // reads each orphan after the box above it, unfitted too.
        assert_eq!(
            ordered_texts(&regions, &cells),
            [
                "The missi",
                "highly ex",
                "actually ",
                "about pro",
                "will be l"
            ]
        );

        super::fit_regions_to_cells(&mut regions, &cells);
        assert_eq!(regions.len(), 5);
        // A ends on its last claimed line, C starts on its only one.
        assert_eq!((regions[0].t, regions[0].b), (135.0, 170.0));
        assert_eq!((regions[2].t, regions[2].b), (223.0, 234.0));
        assert_eq!(
            ordered_texts(&regions, &cells),
            [
                "The missi",
                "highly ex",
                "actually ",
                "about pro",
                "will be l"
            ]
        );
    }

    /// An orphan the fitted paragraph box surrounds (a short middle line the
    /// narrow model box missed while claiming the lines around it) is folded
    /// into the paragraph; an empty regular box goes away, a formula stays, a
    /// picture is never refitted, and a page with no cells is left untouched.
    #[test]
    fn fitting_folds_surrounded_orphans_and_drops_empty_regulars() {
        let wide = |text: &str, t: f32| cell(text, 60.0, t, 400.0, t + 11.0);
        let cells = vec![
            wide("first line of the paragraph", 100.0),
            cell("stray", 250.0, 112.0, 400.0, 123.0), // clear of the narrow box
            wide("third line of the paragraph", 124.0),
        ];
        let mut regions = vec![
            // Narrow box: claims the wide lines at 0.41, misses the short one.
            region("text", 0.9, 60.0, 98.0, 200.0, 136.0),
            region("section_header", 0.8, 60.0, 300.0, 200.0, 320.0), // no cells
            region("formula", 0.8, 60.0, 340.0, 200.0, 360.0),        // no cells, kept
            region("picture", 0.8, 0.0, 400.0, 500.0, 600.0),
        ];
        super::add_orphan_regions(&mut regions, &cells);
        assert_eq!(regions.len(), 5, "the short line became an orphan");
        super::fit_regions_to_cells(&mut regions, &cells);
        let labels: Vec<&str> = regions.iter().map(|r| r.label).collect();
        assert_eq!(labels, ["text", "formula", "picture"]);
        let para = &regions[0];
        assert_eq!(
            (para.l, para.t, para.r, para.b),
            (60.0, 100.0, 400.0, 135.0)
        );
        assert_eq!(
            super::region_texts_exclusive(&regions, &cells)[0],
            "first line of the paragraph stray third line of the paragraph"
        );
        assert_eq!(
            (regions[2].t, regions[2].b),
            (400.0, 600.0),
            "picture untouched"
        );

        let mut untouched = vec![region("text", 0.9, 0.0, 0.0, 10.0, 10.0)];
        super::fit_regions_to_cells(&mut untouched, &[]);
        assert_eq!(untouched.len(), 1, "no cells yet: nothing dropped");
    }

    /// #609: one `text` region over a checklist whose lines each have a drawn
    /// square in front splits into a checkbox region per boxed line — the
    /// wrapped second line of an option stays with it, the intro line above
    /// the first square keeps the region's label, and a ticked square gives
    /// `checkbox_selected`. Without squares nothing changes.
    #[test]
    fn checklist_lines_with_squares_split_into_checkbox_regions() {
        use crate::checkbox::CheckBox;
        let cells = vec![
            cell("Pick any:", 100.0, 90.0, 160.0, 100.0),
            cell("First option", 120.0, 110.0, 180.0, 120.0),
            cell("Second option that", 120.0, 129.0, 200.0, 139.0),
            cell("wraps onto a line", 120.0, 141.0, 196.0, 151.0),
            cell("Third option", 120.0, 160.0, 182.0, 170.0),
        ];
        let sq = |t: f32, checked| CheckBox {
            l: 100.0,
            t,
            r: 112.0,
            b: t + 12.0,
            checked,
        };
        let boxes = [sq(109.0, false), sq(128.0, true), sq(159.0, false)];
        let block = || vec![region("text", 0.6, 100.0, 90.0, 200.0, 170.0)];

        let mut regions = block();
        super::split_checkbox_lines(&mut regions, &cells, &boxes);
        let texts = super::region_texts_exclusive(&regions, &cells);
        let got: Vec<(&str, &str)> = regions
            .iter()
            .zip(&texts)
            .map(|(r, t)| (r.label, t.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("text", "Pick any:"),
                ("checkbox_unselected", "First option"),
                ("checkbox_selected", "Second option that wraps onto a line"),
                ("checkbox_unselected", "Third option"),
            ]
        );
        // A checkbox region spans its square and its line(s).
        assert_eq!((regions[2].l, regions[2].t), (100.0, 128.0));
        assert_eq!((regions[2].r, regions[2].b), (200.0, 151.0));

        let shape = |rs: &[Region]| -> Vec<(&'static str, [f32; 4])> {
            rs.iter().map(|r| (r.label, [r.l, r.t, r.r, r.b])).collect()
        };
        let mut untouched = block();
        super::split_checkbox_lines(&mut untouched, &cells, &[]);
        assert_eq!(shape(&untouched), shape(&block()));
        // A square far left of the text (a margin mark), or one the text
        // sits inside (a comb field), does not make a checkbox.
        for far in [
            CheckBox {
                l: 40.0,
                r: 52.0,
                ..sq(109.0, false)
            },
            CheckBox {
                l: 115.0,
                r: 127.0,
                ..sq(109.0, false)
            },
        ] {
            let mut regions = block();
            super::split_checkbox_lines(&mut regions, &cells, &[far]);
            assert_eq!(shape(&regions), shape(&block()));
        }
    }

    /// #609: a ballot-box glyph opening a line marks a checkbox item like a
    /// drawn square — `☐` unchecked, `☒` checked, the glyph stripped from the
    /// label at emission — while a line holding two boxes (`☐ Yes ☐ No`)
    /// stays text of its own.
    #[test]
    fn checklist_lines_opening_with_a_ballot_box_split_into_checkbox_regions() {
        let cells = vec![
            cell("Options:", 100.0, 90.0, 150.0, 100.0),
            cell("\u{2610} Tea", 100.0, 110.0, 140.0, 120.0),
            cell("\u{2612} Coffee", 100.0, 130.0, 150.0, 140.0),
            cell("\u{2610} Yes \u{2610} No", 100.0, 150.0, 170.0, 160.0),
        ];
        let mut regions = vec![region("text", 0.6, 100.0, 90.0, 170.0, 160.0)];
        super::split_checkbox_lines(&mut regions, &cells, &[]);
        let texts = super::region_texts_exclusive(&regions, &cells);
        let got: Vec<(&str, &str)> = regions
            .iter()
            .zip(&texts)
            .map(|(r, t)| match r.label {
                "text" => (r.label, t.as_str()),
                _ => (r.label, super::strip_checkbox_glyph(t)),
            })
            .collect();
        assert_eq!(
            got,
            [
                ("text", "Options:"),
                ("checkbox_unselected", "Tea"),
                ("checkbox_selected", "Coffee"),
                // Two boxes on one line: its own text, the item closed.
                ("text", "\u{2610} Yes \u{2610} No"),
            ]
        );
        assert_eq!(super::strip_checkbox_glyph("  \u{2611}  Done"), "Done");
        assert_eq!(super::strip_checkbox_glyph("No box"), "No box");
    }

    #[test]
    fn only_note_calls_move_behind_their_line() {
        for t in ["(Anm. 1)", "[3]", "12", "*", "†", "¹²", "1,2"] {
            assert!(looks_like_note_call(t), "{t}");
        }
        // Figure labels a size smaller than their neighbours stay put.
        for t in [
            "be",
            "perfect",
            "C",
            "apply",
            "RoPE",
            "",
            ",",
            "(this is a long aside)",
        ] {
            assert!(!looks_like_note_call(t), "{t}");
        }
        let cell = |text: &str, l: f32, t: f32, r: f32, b: f32| TextCell {
            text: text.into(),
            l,
            t,
            r,
            b,
        };
        let host = cell("und die Belegschaft besteht.", 100.0, 100.0, 300.0, 112.0);
        assert!(is_note_of(
            &cell("(Anm. 1)", 302.0, 98.0, 330.0, 106.0),
            &host
        ));
        assert!(!is_note_of(
            &cell("perfect", 302.0, 98.0, 330.0, 106.0),
            &host
        ));
    }

    /// docling#3929 `TestSanitizeTextArabicPresentationForms` (#1001): shaped
    /// Arabic folds to the canonical letters, per run — other scripts' NFKC
    /// compatibility forms are left alone.
    #[test]
    fn arabic_presentation_forms_fold_like_docling() {
        assert_eq!(clean_text("ﻣﺎﺮﺱ"), "مارس");
        assert_eq!(clean_text("ﻻ"), "لا");
        assert_eq!(clean_text("ﭘ"), "پ");
        assert!(clean_text("ﺍﻟﺤﺮﺱ").contains("الحرس"));
        assert_eq!(clean_text("مرحبا"), "مرحبا");
        assert_eq!(clean_text("x²"), "x²");
        assert_eq!(clean_text("H₂O"), "H₂O");
        let fullwidth: String = [0xFF46, 0xFF55, 0xFF4C, 0xFF4C]
            .iter()
            .map(|&c| char::from_u32(c).unwrap())
            .collect();
        assert_eq!(clean_text(&fullwidth), fullwidth);
        assert_eq!(clean_text("2024 ﻣﺎﺮﺱ x²"), "2024 مارس x²");
    }

    /// docling#5069's ligature map (#1001): `ﬅ` is `ſt` → `st`, Dutch `Ĳ`/`ĳ`
    /// spell out, the spurious U+F0A0 glyph goes and a real word boundary
    /// after it stays.
    #[test]
    fn ligatures_and_the_private_use_glyph_fold_like_docling() {
        assert_eq!(clean_text("\u{FB05}ar \u{FB06}op"), "star stop");
        assert_eq!(clean_text("\u{132} is \u{133}s"), "IJ is ijs");
        assert_eq!(clean_text("hello\u{F0A0} world"), "hello world");
        assert_eq!(clean_text("con\u{FB01}gure"), "configure");
    }

    /// docling#5069: TableFormer cells get body text's folds (they kept
    /// `it’s`, `“quoted”`, `• dot` and shaped Arabic), unless
    /// `pdf_keep_typography` keeps the typography.
    #[test]
    fn table_cells_are_normalized_like_body_text() {
        use super::{normalize_grid, TableGrid, TypographyScope};
        let grid = || TableGrid {
            rows: vec![vec![
                "it\u{2019}s".into(),
                "\u{201c}quoted\u{201d}".into(),
                "\u{2022} dot".into(),
                "ﻣﺎﺮﺱ".into(),
            ]],
            cells: vec![docling_core::TableCell {
                text: "\u{201c}a\u{FB01}\u{201d}".into(),
                bbox: None,
                start_row: 0,
                start_col: 0,
                row_span: 1,
                col_span: 1,
                column_header: false,
                row_header: false,
                row_section: false,
            }],
        };
        let g = normalize_grid(grid());
        assert_eq!(g.rows[0], ["it's", "\"quoted\"", "\u{b7} dot", "مارس"]);
        assert_eq!(g.cells[0].text, "\"afi\"");
        let _keep = TypographyScope::set(true);
        let g = normalize_grid(grid());
        assert_eq!(
            g.rows[0],
            [
                "it\u{2019}s",
                "\u{201c}quoted\u{201d}",
                "\u{2022} dot",
                "مارس"
            ]
        );
    }
}
