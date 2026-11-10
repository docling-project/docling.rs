//! Scanned-page assembly facade for the browser build (#157 stage 2).
//!
//! The native `Worker::process` chain, re-exposed without the `ml` feature so
//! `docling-wasm` can run it around JS-delegated inference: refine the raw
//! layout detections exactly like the native pipeline, then assemble the page
//! with the geometric table fallback (the lite profile — TableFormer is
//! stage 3) and no enrichments. One shared implementation, one behavior.

use docling_core::{DoclingDocument, Node};

/// One assembled page: its nodes and `(anchor, href)` hyperlink pairs.
pub type AssembledPage = (Vec<Node>, Vec<(String, String)>);

use crate::layout::{label_threshold, Region};
use crate::pdfium_backend::{PdfPage, TextCell};

/// The native pipeline's region-refinement chain, in its exact order:
/// per-label score thresholds → overlap resolution → orphan text regions for
/// detector-missed cells → false-picture drops → contained-regular drops.
/// `cells` is the page's (possibly empty, pre-OCR) text-cell set.
pub fn refine_regions(
    regions: Vec<Region>,
    cells: &[TextCell],
    page_w: f32,
    page_h: f32,
) -> Vec<Region> {
    let mut regions = regions;
    regions.retain(|r| r.score >= label_threshold(r.label));
    crate::assemble::drop_full_page_pictures(&mut regions, page_w, page_h);
    let mut regions = crate::assemble::resolve(regions);
    crate::assemble::add_orphan_regions(&mut regions, cells);
    crate::assemble::drop_false_pictures(&mut regions, cells, page_w, page_h);
    crate::assemble::drop_contained_regulars(&mut regions);
    crate::assemble::reclaim_heading_body_footers(&mut regions, page_w);
    // A "picture" that is really a colored text panel reads out as paragraphs
    // instead of shipping as pixels (needs a text layer; pre-OCR this is a
    // no-op and the OCR paths re-run it once cells exist).
    crate::assemble::recover_text_panels(&mut regions, cells);
    // Fit the regular regions to their cells (#419) — a no-op pre-OCR, when
    // there are none yet.
    crate::assemble::fit_regions_to_cells(&mut regions, cells);
    regions
}

/// Drop text regions that would read the same cells twice.
///
/// The layout model can emit overlapping detections — typically one region
/// covering a whole block *and* smaller ones covering its parts, when the
/// raster is noisier than the model likes (the browser's pdf.js render, where
/// this was observed as a paragraph printed twice; pdfium's render of the same
/// page did not trigger it). Assembly assigns each region the cells >50%
/// inside it without consuming them, so every such overlap duplicates text.
///
/// The rule is deliberately narrow: a *text-like* region is dropped only when
/// every cell it claims is also claimed by one *single* other region — i.e. it
/// is a strict re-reader of part of a larger region. The larger region
/// survives, which is also what the native raster produces on the observed
/// page (one merged paragraph). Ties (two regions claiming identical sets)
/// keep the higher-scoring one.
pub fn drop_duplicate_text_claims(regions: Vec<Region>, cells: &[TextCell]) -> Vec<Region> {
    let claimed: Vec<Vec<usize>> = regions
        .iter()
        .map(|r| {
            cells
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    let ca = ((c.r - c.l) * (c.b - c.t)).max(1.0);
                    let iw = (c.r.min(r.r) - c.l.max(r.l)).max(0.0);
                    let ih = (c.b.min(r.b) - c.t.max(r.t)).max(0.0);
                    iw * ih / ca > 0.5
                })
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    let text_like = |label: &str| matches!(label, "text" | "list_item" | "section_header");
    let mut drop = vec![false; regions.len()];
    for a in 0..regions.len() {
        if !text_like(regions[a].label) || claimed[a].is_empty() {
            continue;
        }
        for b in 0..regions.len() {
            if a == b || drop[b] || !text_like(regions[b].label) {
                continue;
            }
            let subset = claimed[a].iter().all(|i| claimed[b].contains(i));
            if !subset {
                continue;
            }
            // Identical sets: keep the higher score (stable on ties by index).
            let identical = claimed[a].len() == claimed[b].len();
            if identical && (regions[a].score, b) > (regions[b].score, a) {
                continue;
            }
            drop[a] = true;
            break;
        }
    }
    regions
        .into_iter()
        .zip(drop)
        .filter(|(_, d)| !d)
        .map(|(r, _)| r)
        .collect()
}

/// Assemble one refined page — geometric tables (no TableFormer), no
/// enrichments — into its nodes and hyperlink pairs.
pub fn assemble_page(page: &PdfPage, regions: Vec<Region>) -> AssembledPage {
    assemble_page_with_tables(page, regions, vec![None; 0].into_iter().collect())
}

/// Like [`assemble_page`] but with pre-computed table rows per region:
/// `table_rows[i] = Some(rows)` for a table region whose structure the browser
/// TableFormer path (#157 stage 3) resolved, `None` for the geometric fallback.
/// An empty `table_rows` means "all geometric" (what [`assemble_page`] passes).
/// Same assembly as the native pipeline.
pub fn assemble_page_with_tables(
    page: &PdfPage,
    regions: Vec<Region>,
    mut table_rows: Vec<Option<crate::tf_core::TableGrid>>,
) -> AssembledPage {
    table_rows.resize(regions.len(), None);
    let enrich = vec![None; regions.len()];
    crate::assemble::assemble_page(page, regions, &table_rows, &enrich, None)
}

/// Stitch per-page results into a document: cross-page paragraph
/// continuations merge exactly like the native pipeline's final pass.
pub fn finish_document(name: &str, pages: Vec<AssembledPage>) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    for (i, (mut nodes, links)) in pages.into_iter().enumerate() {
        crate::assemble::stamp_page_no(&mut nodes, i + 1);
        doc.nodes.extend(nodes);
        doc.links.extend(links);
    }
    crate::assemble::merge_continuations(
        &mut doc.nodes,
        crate::assemble::Continuations::AcrossPages,
    );
    doc
}

#[cfg(test)]
mod duplicate_claims {
    use crate::layout::Region;
    use crate::pdfium_backend::TextCell;

    fn cell(l: f32, t: f32, r: f32, b: f32) -> TextCell {
        TextCell {
            text: "x".into(),
            l,
            t,
            r,
            b,
        }
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

    /// The observed browser failure: the model detects a block *and* its two
    /// halves, so both halves re-read cells the block already claims and a
    /// paragraph prints twice. The parts must go, the block must stay — that
    /// matches what the native raster produced (one merged paragraph).
    #[test]
    fn part_regions_of_a_block_are_dropped() {
        let cells = vec![cell(10.0, 10.0, 90.0, 20.0), cell(10.0, 30.0, 90.0, 40.0)];
        let regions = vec![
            region("text", 0.9, 5.0, 5.0, 95.0, 45.0),  // the block
            region("text", 0.8, 5.0, 5.0, 95.0, 25.0),  // its top half
            region("text", 0.8, 5.0, 25.0, 95.0, 45.0), // its bottom half
        ];
        let kept = super::drop_duplicate_text_claims(regions, &cells);
        assert_eq!(kept.len(), 1, "kept: {kept:?}");
        assert_eq!((kept[0].t, kept[0].b), (5.0, 45.0), "the block survives");
    }

    /// Disjoint regions — the normal page shape — are untouched, and so are
    /// non-text labels even when they overlap text (a table region legitimately
    /// contains the same cells its caption/text neighbours touch).
    #[test]
    fn disjoint_and_non_text_regions_are_kept() {
        let cells = vec![cell(10.0, 10.0, 90.0, 20.0), cell(10.0, 50.0, 90.0, 60.0)];
        let regions = vec![
            region("text", 0.9, 5.0, 5.0, 95.0, 25.0),
            region("text", 0.9, 5.0, 45.0, 95.0, 65.0),
            region("table", 0.9, 5.0, 5.0, 95.0, 65.0), // overlaps both, not text
        ];
        assert_eq!(super::drop_duplicate_text_claims(regions, &cells).len(), 3);
    }

    /// Two identical claims keep exactly one — the higher score.
    #[test]
    fn identical_claims_keep_the_higher_score() {
        let cells = vec![cell(10.0, 10.0, 90.0, 20.0)];
        let regions = vec![
            region("text", 0.7, 5.0, 5.0, 95.0, 25.0),
            region("text", 0.9, 6.0, 6.0, 94.0, 24.0),
        ];
        let kept = super::drop_duplicate_text_claims(regions, &cells);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].score, 0.9);
    }
}
