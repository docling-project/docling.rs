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

use crate::backend::numfmt::{self, SheetFormats};
use crate::backend::xlsx::{find_tables, location_value, sheet_frame, Merges};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

#[derive(Default)]
pub struct XlsBackend {
    /// Omit empty cells from sparse-sheet table grids (#271, opt-in) — see
    /// [`find_tables`].
    pub skip_empty: bool,
}

impl DeclarativeBackend for XlsBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let cursor = Cursor::new(source.bytes.clone());
        let mut workbook: Xls<_> = Xls::new(cursor).map_err(|e| match e {
            // A FILEPASS record (#624): the same error every format gives.
            calamine::XlsError::Password => crate::backend::offcrypto::encrypted("xls"),
            e => ConversionError::with_source("xls", e),
        })?;

        let metas: Vec<(String, calamine::SheetType, calamine::SheetVisible)> = workbook
            .sheets_metadata()
            .iter()
            .map(|s| (s.name.clone(), s.typ, s.visible))
            .collect();

        // Number formats (#634): the Workbook stream's FORMAT/XF records and
        // each sheet's cell `ixfe`s, in BOUNDSHEET order — calamine's.
        let is_1904 = workbook.has_1904_epoch();
        let (xfs, sheet_cells) = crate::backend::cfb::CompoundFile::open(&source.bytes)
            .and_then(|cfb| cfb.stream("Workbook").or_else(|| cfb.stream("Book")))
            .map(|stream| numfmt::biff_formats(&stream))
            .unwrap_or_default();
        let xfs = std::sync::Arc::new(xfs);
        let mut sheet_cells = sheet_cells.into_iter();

        let mut doc = DoclingDocument::new(&source.name);
        let mut prev_item_page = false;
        for (name, typ, visible) in &metas {
            let formats = SheetFormats {
                xfs: xfs.clone(),
                cells: sheet_cells.next().unwrap_or_default(),
                // BIFF has no rich values.
                images: Default::default(),
            };
            if !matches!(typ, calamine::SheetType::WorkSheet) {
                continue;
            }
            let Ok(range) = workbook.worksheet_range(name) else {
                continue;
            };
            let merges: Merges = workbook
                .merge_cells_by_sheet_name(name)
                .unwrap_or_default()
                .iter()
                .map(|d| (d.start, d.end))
                .collect();

            // Same shaping as the XLSX backend: absolute cell coordinates, a
            // merge-covered cell counts as content and renders its top-left.
            let frame = sheet_frame(&range, &merges);
            let (or, oc) = frame.origin;

            let mut items: Vec<((usize, usize, usize, usize), Node)> = Vec::new();
            for t in find_tables(&range, &frame, self.skip_empty, &formats, is_1904) {
                if let Some(label) = t.label {
                    items.push((
                        (
                            oc + t.min_c,
                            or + t.min_r.saturating_sub(1),
                            oc + t.max_c + 1,
                            or + t.min_r,
                        ),
                        Node::Paragraph { text: label },
                    ));
                }
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

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let src = SourceDocument::from_bytes("x.xls", InputFormat::Xls, vec![0u8; 64]);
        assert!(XlsBackend::default().convert(&src).is_err());
    }

    /// RUSTSEC-2026-0436 (#979, calamine < 0.38): the CFB/OLE2 reader panicked
    /// on a directory stream whose length is not a multiple of 128 and grew
    /// the DIFAT without bound along a self-referential chain. Both must be
    /// plain errors now — crafted `.xls` input is a DoS vector otherwise.
    fn cfb_header(num_fat: u32, dir_start: u32, difat_start: u32, num_difat: u32) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
        h[24..26].copy_from_slice(&0x003Eu16.to_le_bytes());
        h[26..28].copy_from_slice(&3u16.to_le_bytes());
        h[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
        h[30..32].copy_from_slice(&9u16.to_le_bytes());
        h[32..34].copy_from_slice(&6u16.to_le_bytes());
        h[44..48].copy_from_slice(&num_fat.to_le_bytes());
        h[48..52].copy_from_slice(&dir_start.to_le_bytes());
        h[56..60].copy_from_slice(&4096u32.to_le_bytes());
        h[60..64].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        h[68..72].copy_from_slice(&difat_start.to_le_bytes());
        h[72..76].copy_from_slice(&num_difat.to_le_bytes());
        for i in 0..109 {
            h[76 + i * 4..80 + i * 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        h
    }

    #[test]
    fn crafted_ole2_is_an_error_not_a_panic_or_oom() {
        // Self-referential DIFAT: sector 0 chains to itself.
        let mut selfref = cfb_header(1, 0, 0, u32::MAX);
        let mut sector = vec![0xFFu8; 512];
        sector[508..512].copy_from_slice(&0u32.to_le_bytes());
        selfref.extend(sector);
        // Directory stream (one FAT sector at 0 → dir at 1) cut to 64 bytes:
        // a short final `chunks(128)` entry.
        let mut shortdir = cfb_header(1, 1, 0xFFFF_FFFE, 0);
        shortdir[76..80].copy_from_slice(&0u32.to_le_bytes());
        let mut fat = vec![0xFFu8; 512];
        fat[0..4].copy_from_slice(&0xFFFF_FFFDu32.to_le_bytes());
        fat[4..8].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
        shortdir.extend(fat);
        shortdir.extend(vec![0u8; 64]);
        for bytes in [selfref, shortdir] {
            let src = SourceDocument::from_bytes("x.xls", InputFormat::Xls, bytes);
            let r = std::panic::catch_unwind(|| XlsBackend::default().convert(&src).is_err());
            assert_eq!(r.ok(), Some(true), "crafted OLE2 must be a plain error");
        }
    }
}
