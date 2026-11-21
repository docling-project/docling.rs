//! OCR for a standalone picture (#645): the text the recognizer reads off an
//! image embedded in a non-PDF document — a DOCX/PPTX screenshot, an HTML
//! figure, a sampled video frame — so the picture-OCR enrichment in
//! `docling` can attach it to the picture as docling's description
//! annotation. The models are the ML pipeline's own (the recognizer pair,
//! the text detector, Tesseract under `ocr_engine`), loaded lazily on the
//! first picture exactly as they are for a scanned page, so a converter that
//! OCRs pictures and scans alike holds one set of sessions.
//!
//! The picture is treated as the image input `Pipeline::convert_image` sees:
//! its own scale-1.0 page whose one text region is the whole image, read at
//! docling's effective OCR resolution for images (3 px/pt shrunk to
//! RapidOCR's 2000 px longer side, #570) — no layout pass, since the caller
//! already knows the whole image is the picture. The detector's boxes are
//! the line crops when `ocr_det.onnx` is installed (#570); the ink-projection
//! strips otherwise. Lines under `DOCLING_RS_OCR_TEXT_SCORE` are dropped by
//! the recognizer, as on a page.

use image::RgbImage;

use crate::layout::Region;
use docling_core::{Node, PictureDescription};

use crate::pdfium_backend::{PdfPage, TextCell};
use crate::{ocr, ocr_input, page_ocr_scale, EnrichSlot, PdfError, Pipeline, RAPIDOCR_MAX_SIDE};

/// What the OCR read off one picture: the lines in reading order (top to
/// bottom, left to right within a row, rows joined by `\n`) and the engine
/// that read them — docling's `PictureDescriptionData.provenance`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PictureText {
    pub text: String,
    /// `ppocr` or `tesseract` — the [`ocr::OcrEngine`] that ran.
    pub provenance: &'static str,
}

impl Pipeline {
    /// Decode an embedded picture's bytes (PNG/JPEG/GIF/BMP/TIFF/WebP) under
    /// the standalone-image pixel cap (`DOCLING_RS_MAX_IMAGE_PIXELS`, 30000
    /// per side) — a crafted header never allocates a multi-GB bitmap.
    pub fn decode_picture(&self, bytes: &[u8]) -> Result<RgbImage, PdfError> {
        crate::decode_image_limited(bytes)
    }

    /// The DocumentFigureClassifier's predictions for a picture (descending
    /// confidence), loading the model on first use. `None` when the model is
    /// not installed (warned once by the loader) or inference failed — the
    /// caller decides what an unclassifiable picture means (the picture-OCR
    /// class filter lets it through).
    pub fn classify_picture(&mut self, img: &RgbImage) -> Option<Vec<docling_core::PictureClass>> {
        let mut guard = self.classifier.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*guard, EnrichSlot::Unloaded) {
            *guard = match crate::enrich::PictureClassifier::load_with(crate::intra_threads()) {
                Some(m) => EnrichSlot::Ready(m),
                None => EnrichSlot::Missing,
            };
        }
        let EnrichSlot::Ready(model) = &mut *guard else {
            return None;
        };
        match model.classify(img) {
            Ok(classes) => Some(classes),
            Err(e) => {
                eprintln!("docling-pdf: picture classifier: {e}");
                None
            }
        }
    }

    /// OCR a picture: `Ok(None)` when the engine read no text (or could not
    /// load — the recognizer's one-time warning says so, and `skip_ocr`
    /// short-circuits without one), `Err` only for a failed inference.
    pub fn ocr_picture(&mut self, img: &RgbImage) -> Result<Option<PictureText>, PdfError> {
        let provenance = match self.ocr_engine {
            ocr::OcrEngine::PpOcr => "ppocr",
            ocr::OcrEngine::Tesseract => "tesseract",
        };
        let (w, h) = (img.width() as f32, img.height() as f32);
        if w < 1.0 || h < 1.0 {
            return Ok(None);
        }
        // The image is its own page at 1 px per point; the OCR reads it at
        // `picture_ocr_scale` under RapidOCR's 2000 px cap (#666), else at
        // docling's image resolution unless `ocr_scale` says otherwise.
        let ocr_scale = match self.picture_ocr_scale {
            Some(p) => picture_ocr_scale(p, w, h),
            None => page_ocr_scale(self.ocr_scale, w, h, 1.0),
        };
        let mut cache = None;
        let (view, scale) = ocr_input(&mut cache, img, 1.0, ocr_scale);
        let region = Region {
            label: "text",
            score: 1.0,
            l: 0.0,
            t: 0.0,
            r: w,
            b: h,
        };
        let worker = self.primary()?;
        let detected = worker
            .with_det(|det| det.detect(view))
            .transpose()
            .map_err(PdfError::Ocr)?;
        let Some(model) = worker.ocr_model()? else {
            return Ok(None);
        };
        let cells = model
            .ocr_page_with(
                view,
                std::slice::from_ref(&region),
                scale,
                detected.as_deref(),
            )
            .map_err(PdfError::Ocr)?;
        let text = lines_text(cells.into_iter().map(|(c, _)| c).collect());
        Ok((!text.is_empty()).then_some(PictureText { text, provenance }))
    }
}

/// The px/px a picture reads at under `picture_ocr_scale` = `p` (#666):
/// `p`, shrunk so the longer side stays within RapidOCR's 2000 px
/// (`max_side_len`) — at `1.0`, RapidOCR's own rule: native pixels, only
/// shrinking what exceeds the cap. `None` when that is the picture's own
/// resolution (no resample).
fn picture_ocr_scale(p: f32, w: f32, h: f32) -> Option<f32> {
    let s = p.min(RAPIDOCR_MAX_SIDE / w.max(h));
    ((s - 1.0).abs() > 1e-3).then_some(s)
}

/// Join OCR line cells into text in reading order: rows top to bottom, cells
/// left to right within a row. A cell joins the row whose vertical extent
/// contains its centre — two columns of a screenshot's dialog read across,
/// the way a human scans it; a line that sits lower starts a new row. Empty
/// cells are dropped.
pub(crate) fn lines_text(mut cells: Vec<TextCell>) -> String {
    cells.retain(|c| !c.text.trim().is_empty());
    cells.sort_by(|a, b| a.t.total_cmp(&b.t).then(a.l.total_cmp(&b.l)));
    let mut rows: Vec<(f32, f32, Vec<TextCell>)> = Vec::new();
    for cell in cells {
        let mid = (cell.t + cell.b) / 2.0;
        match rows.last_mut() {
            Some((t, b, row)) if mid >= *t && mid <= *b => {
                *t = t.min(cell.t);
                *b = b.max(cell.b);
                row.push(cell);
            }
            _ => rows.push((cell.t, cell.b, vec![cell])),
        }
    }
    rows.iter_mut()
        .map(|(_, _, row)| {
            row.sort_by(|a, b| a.l.total_cmp(&b.l));
            row.iter()
                .map(|c| c.text.trim())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether an OCR line belongs to a picture (#1219): its centre lies inside
/// the picture's box.
pub(crate) fn text_inside(c: &TextCell, r: &Region) -> bool {
    let (cx, cy) = ((c.l + c.r) / 2.0, (c.t + c.b) / 2.0);
    cx >= r.l && cx <= r.r && cy >= r.t && cy <= r.b
}

/// Hang a text-layer page's in-picture OCR lines (#1219, `picture_ocr`) on
/// the assembled pictures as docling's `description` annotation — the
/// shape the picture OCR of the other formats writes. The pictures carry
/// only their 0–511 `location` by now, so each is matched back by that box
/// (widened by one grid step for the rounding); a line inside two nested
/// pictures goes to the smaller one. Pictures inside table cells are
/// reached too; a picture that already has a description keeps it.
pub(crate) fn describe_pictures(
    nodes: &mut [Node],
    page: &PdfPage,
    cells: &[TextCell],
    provenance: &str,
) {
    let (w, h) = (page.width.max(1.0), page.height.max(1.0));
    let to_region = |loc: &[u16; 4]| {
        let x = |v: u16| v as f32 * w / 512.0;
        let y = |v: u16| v as f32 * h / 512.0;
        Region {
            label: "picture",
            score: 1.0,
            l: x(loc[0].saturating_sub(1)),
            t: y(loc[1].saturating_sub(1)),
            r: x(loc[2].saturating_add(1)),
            b: y(loc[3].saturating_add(1)),
        }
    };
    // Every picture's box, in document order, to settle the nesting.
    let mut boxes: Vec<Region> = Vec::new();
    visit_pictures(nodes, &mut |loc, _| boxes.push(to_region(loc)));
    let area = |r: &Region| (r.r - r.l) * (r.b - r.t);
    let owner: Vec<Option<usize>> = cells
        .iter()
        .map(|c| {
            (0..boxes.len())
                .filter(|&i| text_inside(c, &boxes[i]))
                .min_by(|&a, &b| area(&boxes[a]).total_cmp(&area(&boxes[b])))
        })
        .collect();
    let mut i = 0usize;
    visit_pictures(nodes, &mut |_, description| {
        let mine: Vec<TextCell> = cells
            .iter()
            .zip(&owner)
            .filter(|(_, o)| **o == Some(i))
            .map(|(c, _)| c.clone())
            .collect();
        i += 1;
        if description.is_some() {
            return;
        }
        let text = lines_text(mine);
        if !text.is_empty() {
            *description = Some(PictureDescription {
                text,
                provenance: provenance.to_string(),
            });
        }
    });
}

/// Every located picture under `nodes`, page level and inside table cells,
/// in document order.
fn visit_pictures(
    nodes: &mut [Node],
    f: &mut dyn FnMut(&[u16; 4], &mut Option<PictureDescription>),
) {
    for node in nodes {
        match node {
            Node::Located { location, inner } => match inner.as_mut() {
                Node::Picture { description, .. } => f(location, description),
                other => visit_pictures(std::slice::from_mut(other), f),
            },
            Node::Table(t) => {
                let Some(blocks) = t.cell_blocks.as_mut() else {
                    continue;
                };
                for row in blocks.iter_mut() {
                    for cell in row.iter_mut() {
                        visit_pictures(cell, f);
                    }
                }
            }
            Node::Group { children, .. } | Node::PictureChildren(children) => {
                visit_pictures(children, f)
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, l: f32, t: f32, r: f32, b: f32) -> TextCell {
        TextCell {
            text: text.into(),
            l,
            t,
            r,
            b,
        }
    }

    /// #1219: each OCR line goes to the picture whose box holds its centre
    /// — the smaller of two nested ones, a picture inside a table cell too —
    /// and a picture that already has a description keeps it.
    #[test]
    fn describe_pictures_matches_lines_by_box() {
        use docling_core::{CaptionParent, Table};
        let pic = |loc: [u16; 4], description: Option<PictureDescription>| Node::Located {
            location: loc,
            inner: Box::new(Node::Picture {
                caption: None,
                caption_href: None,
                image: None,
                classification: None,
                description,
                caption_parent: CaptionParent::default(),
                caption_location: None,
            }),
        };
        // A 512 × 512 pt page: grid units are points.
        let page = PdfPage::from_cells(512.0, 512.0, 2.0, Vec::new());
        let table = Node::Table(Table {
            cell_blocks: Some(vec![vec![vec![pic([300, 300, 400, 400], None)]]]),
            ..Table::default()
        });
        let kept = PictureDescription {
            text: "alt".into(),
            provenance: PictureDescription::ALT_TEXT.into(),
        };
        let mut nodes = vec![
            pic([0, 0, 200, 200], None),
            pic([50, 50, 100, 100], None),
            table,
            pic([450, 0, 500, 50], Some(kept.clone())),
        ];
        let cells = [
            cell("outer", 10.0, 10.0, 40.0, 20.0),
            cell("inner", 60.0, 60.0, 90.0, 70.0),
            cell("in table", 310.0, 310.0, 390.0, 320.0),
            cell("ignored", 455.0, 5.0, 495.0, 15.0),
            cell("nowhere", 250.0, 250.0, 260.0, 260.0),
        ];
        describe_pictures(&mut nodes, &page, &cells, "ppocr");
        let mut got = Vec::new();
        visit_pictures(&mut nodes, &mut |_, d| got.push(d.clone().map(|d| d.text)));
        assert_eq!(
            got,
            [
                Some("outer".to_string()),
                Some("inner".to_string()),
                Some("in table".to_string()),
                Some("alt".to_string()),
            ]
        );
    }

    /// Rows read across, then down; a cell whose centre sits below the
    /// current row starts a new one, and empty cells vanish.
    #[test]
    fn lines_text_reads_rows_across_then_down() {
        let cells = vec![
            cell("right", 200.0, 10.0, 260.0, 30.0),
            cell("below", 10.0, 40.0, 80.0, 60.0),
            cell("left", 10.0, 12.0, 60.0, 32.0),
            cell("  ", 10.0, 70.0, 20.0, 80.0),
        ];
        assert_eq!(lines_text(cells), "left right\nbelow");
        assert_eq!(lines_text(Vec::new()), "");
    }

    /// #666: `picture_ocr_scale(1.0)` is RapidOCR's rule — a 720 × 180
    /// screenshot reads at its own 720 × 180 (no resample, where the default
    /// upscales it to 2000 × 500), a 3000 × 1000 one shrinks to 2000 × 667.
    #[test]
    fn picture_scale_reads_native_pixels_under_the_2000_px_cap() {
        let dims = |p: f32, w: f32, h: f32| {
            let s = picture_ocr_scale(p, w, h).unwrap_or(1.0);
            ((w * s).round(), (h * s).round())
        };
        assert_eq!(picture_ocr_scale(1.0, 720.0, 180.0), None);
        assert_eq!(dims(1.0, 3000.0, 1000.0), (2000.0, 667.0));
        assert_eq!(dims(2.0, 720.0, 180.0), (1440.0, 360.0));
        assert_eq!(dims(2.0, 1280.0, 720.0), (2000.0, 1125.0));
        // The default rule, for comparison: 3 px/pt under the cap.
        let default = page_ocr_scale(None, 720.0, 180.0, 1.0).unwrap();
        assert_eq!(
            ((720.0 * default).round(), (180.0 * default).round()),
            (2000.0, 500.0)
        );
    }
}
