//! METS / Google Books backend — a port of docling's `MetsGbsDocumentBackend`.
//!
//! The source is a `.tar.gz` scan package: for each page an hOCR `.html` (word
//! cells with pixel bounding boxes) and a `.tif` page image. Unlike a PDF the
//! text layer is already known (the OCR was done upstream), so we build the
//! per-page cells from the hOCR and run the shared layout + assembly pipeline —
//! no OCR stage. Pages are ordered by their numeric basename.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::OnceLock;

use flate2::read::GzDecoder;
use regex::Regex;
use tar::Archive;

use fleischwolf_core::DoclingDocument;

use crate::pdfium_backend::{PdfPage, TextCell};
use crate::{convert_pages, PdfError};

pub fn convert_mets_gbs(bytes: &[u8], name: &str) -> Result<DoclingDocument, PdfError> {
    let mut html: BTreeMap<String, String> = BTreeMap::new();
    let mut tiff: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    let mut archive = Archive::new(GzDecoder::new(bytes));
    let entries = archive
        .entries()
        .map_err(|e| PdfError::Pdfium(format!("mets tar: {e}")))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| PdfError::Pdfium(format!("mets tar: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| PdfError::Pdfium(format!("mets tar: {e}")))?
            .to_string_lossy()
            .into_owned();
        let base = path.rsplit('/').next().unwrap_or(&path);
        if let Some(stem) = base
            .strip_suffix(".html")
            .or_else(|| base.strip_suffix(".hocr"))
        {
            let mut s = String::new();
            if entry.read_to_string(&mut s).is_ok() {
                html.insert(stem.to_string(), s);
            }
        } else if let Some(stem) = base
            .strip_suffix(".tif")
            .or_else(|| base.strip_suffix(".tiff"))
        {
            let mut v = Vec::new();
            if entry.read_to_end(&mut v).is_ok() {
                tiff.insert(stem.to_string(), v);
            }
        }
    }

    // BTreeMap iterates stems in sorted (page) order.
    let mut pages = Vec::new();
    for (stem, hocr) in &html {
        let Some(img_bytes) = tiff.get(stem) else {
            continue;
        };
        let image = image::load_from_memory(img_bytes)
            .map_err(|e| PdfError::Pdfium(format!("mets image {stem}: {e}")))?
            .into_rgb8();
        let (width, height, cells) = parse_hocr(hocr, &image);
        pages.push(PdfPage {
            width,
            height,
            scale: 1.0,
            cells,
            code_cells: Vec::new(),
            word_cells: Vec::new(),
            image,
            links: Vec::new(),
        });
    }
    if pages.is_empty() {
        return Err(PdfError::Pdfium(
            "mets: no hOCR/TIFF page pairs found in archive".into(),
        ));
    }
    convert_pages(pages, name)
}

/// Parse an hOCR page: the `ocr_page` bbox gives the page geometry (cells are in
/// those pixel coordinates, top-left origin) and each `ocrx_word` is a cell.
fn parse_hocr(hocr: &str, image: &image::RgbImage) -> (f32, f32, Vec<TextCell>) {
    static PAGE_RE: OnceLock<Regex> = OnceLock::new();
    static WORD_RE: OnceLock<Regex> = OnceLock::new();
    let page_re = PAGE_RE
        .get_or_init(|| Regex::new(r"ocr_page[^>]*?bbox\s+\d+\s+\d+\s+(\d+)\s+(\d+)").unwrap());
    let word_re = WORD_RE.get_or_init(|| {
        Regex::new(r"ocrx_word[^>]*?bbox\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)[^>]*>([^<]*)</").unwrap()
    });

    let mut width = image.width() as f32;
    let mut height = image.height() as f32;
    if let Some(c) = page_re.captures(hocr) {
        width = c[1].parse().unwrap_or(width);
        height = c[2].parse().unwrap_or(height);
    }

    let mut cells = Vec::new();
    for c in word_re.captures_iter(hocr) {
        let text = unescape(&c[5]);
        if text.trim().is_empty() {
            continue;
        }
        cells.push(TextCell {
            text,
            l: c[1].parse().unwrap_or(0.0),
            t: c[2].parse().unwrap_or(0.0),
            r: c[3].parse().unwrap_or(0.0),
            b: c[4].parse().unwrap_or(0.0),
        });
    }
    (width, height, cells)
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hocr_extracts_page_geometry_and_word_cells() {
        let hocr = "<div class='ocr_page' title='bbox 0 0 100 200;ppageno 1'>\
            <span class='ocrx_word' title='bbox 10 20 30 40;x_wconf 99'>Hello</span>\
            <span class='ocrx_word' title='bbox 35 20 60 40'>R&amp;D</span>\
            <span class='ocrx_word' title='bbox 1 1 2 2'>   </span></div>";
        let img = image::RgbImage::new(50, 50);
        let (w, h, cells) = parse_hocr(hocr, &img);
        // page geometry comes from ocr_page, not the (differently sized) image
        assert_eq!((w, h), (100.0, 200.0));
        // the whitespace-only word is dropped; entities are unescaped
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].text, "Hello");
        assert_eq!(
            (cells[0].l, cells[0].t, cells[0].r, cells[0].b),
            (10.0, 20.0, 30.0, 40.0)
        );
        assert_eq!(cells[1].text, "R&D");
    }
}
