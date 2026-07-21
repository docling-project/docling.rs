//! XLS (Excel 97–2004, BIFF8) backend — issue #127.
//!
//! Native parsing of the legacy binary workbook: `calamine`'s `Xls` reader
//! decodes the CFB container and BIFF records, and the sheet content then goes
//! through the *same* region detection as the XLSX backend ([`find_tables`]),
//! so a workbook produces identical tables whether it arrives as `.xls` or
//! `.xlsx`. docling proper reaches these files by shelling out to LibreOffice
//! (`docling` PR #3804); here the format is a first-class input instead.
//!
//! Scope: cell data (tables), merged regions, sheet order/visibility and page
//! breaks. Drawings, charts and comments are OOXML-part features in the XLSX
//! backend and have no BIFF equivalent here (calamine does not expose them).

use std::io::Cursor;

use calamine::{Reader, Xls};
use docling_core::{DoclingDocument, Node};

use crate::backend::xlsx::{find_tables, location_value, Merges};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct XlsBackend;

impl DeclarativeBackend for XlsBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let cursor = Cursor::new(source.bytes.clone());
        let mut workbook: Xls<_> =
            Xls::new(cursor).map_err(|e| ConversionError::Parse(format!("xls: {e}")))?;

        let metas: Vec<(String, calamine::SheetType, calamine::SheetVisible)> = workbook
            .sheets_metadata()
            .iter()
            .map(|s| (s.name.clone(), s.typ, s.visible))
            .collect();

        let mut doc = DoclingDocument::new(&source.name);
        let mut prev_item_page = false;
        for (name, typ, visible) in &metas {
            if !matches!(typ, calamine::SheetType::WorkSheet) {
                continue;
            }
            let Ok(range) = workbook.worksheet_range(name) else {
                continue;
            };
            let merges: Merges = workbook
                .worksheet_merge_cells(name)
                .unwrap_or_default()
                .iter()
                .map(|d| (d.start, d.end))
                .collect();

            // Same shaping as the XLSX backend: absolute cell coordinates, a
            // merge-covered cell counts as content and renders its top-left.
            let (rs_r, rs_c) = range.start().unwrap_or((0, 0));
            let mut merge_of = std::collections::HashMap::new();
            for &((sr, sc), (er, ec)) in &merges {
                let tl = ((sr - rs_r) as usize, (sc - rs_c) as usize);
                for r in sr..=er {
                    for c in sc..=ec {
                        merge_of.insert(((r - rs_r) as usize, (c - rs_c) as usize), tl);
                    }
                }
            }
            let (rh, rw) = range.get_size();
            let height = rh.max(merge_of.keys().map(|(r, _)| r + 1).max().unwrap_or(0));
            let width = rw.max(merge_of.keys().map(|(_, c)| c + 1).max().unwrap_or(0));
            let (or, oc) = (rs_r as usize, rs_c as usize);

            let mut items: Vec<((usize, usize, usize, usize), Node)> = Vec::new();
            for t in find_tables(&range, &merge_of, height, width) {
                items.push((
                    (
                        oc + t.min_c,
                        or + t.min_r,
                        oc + t.max_c + 1,
                        or + t.max_r + 1,
                    ),
                    Node::Table(t.table),
                ));
            }
            if items.is_empty() {
                continue;
            }
            items.sort_by_key(|((_, t, _, _), _)| *t);
            let page_w = items.iter().map(|((_, _, r, _), _)| *r).max().unwrap_or(1);
            let page_h = items.iter().map(|((_, _, _, b), _)| *b).max().unwrap_or(1);
            let hidden = !matches!(visible, calamine::SheetVisible::Visible);
            for ((l, t, r, b), mut node) in items {
                if let Node::Table(table) = &mut node {
                    table.location = Some([
                        location_value(l, page_w),
                        location_value(t, page_h),
                        location_value(r, page_w),
                        location_value(b, page_h),
                    ]);
                }
                let node = if hidden {
                    Node::Furniture {
                        layer: docling_core::ContentLayer::Invisible,
                        inner: Box::new(node),
                    }
                } else {
                    node
                };
                doc.push(node);
            }
            if prev_item_page {
                doc.push(Node::PageBreak);
            }
            prev_item_page = true;
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputFormat;

    fn fixture(name: &str) -> SourceDocument {
        let path = format!(
            "{}/tests/data/xls/sources/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).expect("fixture exists");
        SourceDocument::from_bytes(name, InputFormat::Xls, bytes)
    }

    #[test]
    fn parses_xls_tables_like_the_xlsx_twin() {
        let doc = XlsBackend
            .convert(&fixture("xlsx_01.xls"))
            .expect("converts");
        let tables: Vec<_> = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Table(_)))
            .collect();
        assert!(!tables.is_empty(), "expected tables, got: {:?}", doc.nodes);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let src = SourceDocument::from_bytes("x.xls", InputFormat::Xls, vec![0u8; 64]);
        assert!(XlsBackend.convert(&src).is_err());
    }
}
