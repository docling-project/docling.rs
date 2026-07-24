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
    let mut regions = crate::assemble::resolve(regions);
    crate::assemble::add_orphan_regions(&mut regions, cells);
    crate::assemble::drop_false_pictures(&mut regions, cells, page_w, page_h);
    crate::assemble::drop_contained_regulars(&mut regions);
    regions
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
    mut table_rows: Vec<Option<Vec<Vec<String>>>>,
) -> AssembledPage {
    table_rows.resize(regions.len(), None);
    let enrich = vec![None; regions.len()];
    crate::assemble::assemble_page(page, regions, &table_rows, &enrich)
}

/// Stitch per-page results into a document: cross-page paragraph
/// continuations merge exactly like the native pipeline's final pass.
pub fn finish_document(name: &str, pages: Vec<AssembledPage>) -> DoclingDocument {
    let mut doc = DoclingDocument::new(name);
    for (nodes, links) in pages {
        doc.nodes.extend(nodes);
        doc.links.extend(links);
    }
    crate::assemble::merge_continuations(&mut doc.nodes);
    doc
}
