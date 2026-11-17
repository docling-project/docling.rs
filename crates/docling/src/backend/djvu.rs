//! DjVu input (`.djvu` / `.djv`) — a docling.rs extension (#434).
//!
//! DjVu is a multi-page scanned-document format (JB2 bilevel masks + IW44
//! wavelet colour layers) that almost always carries a hidden per-page OCR
//! **text layer** — that is the whole point of DjVu: small, searchable scans.
//! Upstream docling has no DjVu reader, so this is a docling.rs extension in the
//! spirit of Visio (#214), SVG (#212) and RTF (#209).
//!
//! Decoding is pure Rust through the `djvu-rs` crate (MIT, no GPL
//! dependencies, written from the public DjVu v3 spec): no DjVuLibre binary,
//! no subprocess, no temp files — so it runs identically in every build,
//! including wasm. Mirroring the pdf / pdf-text split, the **text layer is the
//! default**: it is the DjVu-native content, deterministic, and needs no
//! models. When no selected page carries a text layer and the ML pipeline is
//! built, the caller rasterizes ([`rasterize_pages`]) and OCRs instead; without
//! ML a scan-only DjVu degrades to an empty document with a warning.
//!
//! Like the PDF paths, every page opens with a [`Node::PageInfo`] marker and
//! every paragraph is wrapped in the [`Node::Located`] box of the text-layer
//! zones it came from, so the JSON export carries docling's `pages` map and a
//! per-item `prov` (page number + bbox in points) — what docling-core's
//! `pages` filter, `page_break_placeholder` and bbox consumers key on.
//! Markdown / LaTeX skip both wrappers, so those exports are unchanged.

use djvu_rs::{DjVuDocument, DjVuPage, Rotation, TextLayer, TextZone, TextZoneKind};
use docling_core::{DoclingDocument, Node};

use crate::error::ConversionError;
use crate::source::SourceDocument;

/// Whether `bytes` is a DjVu document, by its `AT&TFORM` IFF85 wrapper and a
/// DjVu form type — single page (`DJVU`), multi-page bundle (`DJVM`), included
/// component (`DJVI`) or thumbnails (`THUM`). Checked by content, not
/// extension, like the other magic sniffs (serve uses it for uploads whose
/// name and Content-Type say nothing).
pub fn looks_like_djvu(bytes: &[u8]) -> bool {
    bytes.len() >= 16
        && &bytes[0..8] == b"AT&TFORM"
        && matches!(&bytes[12..16], b"DJVM" | b"DJVU" | b"DJVI" | b"THUM")
}

fn parse(bytes: &[u8]) -> Result<DjVuDocument, ConversionError> {
    DjVuDocument::parse(bytes).map_err(|e| ConversionError::Parse(format!("djvu: {e}")))
}

/// Resolve a 1-based inclusive page window against the document, mirroring the
/// PDF `pages` semantics (#80): `None` = all, the end clamps, a start past the
/// end errors.
fn resolve_range(
    range: Option<(usize, usize)>,
    total: usize,
) -> Result<(usize, usize), ConversionError> {
    match range {
        None => Ok((1, total)),
        Some((first, last)) => {
            if first == 0 || last < first {
                return Err(ConversionError::Parse(format!(
                    "djvu: invalid page range {first}-{last} (pages are 1-based, first <= last)"
                )));
            }
            if first > total {
                return Err(ConversionError::Parse(format!(
                    "djvu: page range {first}-{last} is outside the document ({total} page(s))"
                )));
            }
            Ok((first, last.min(total)))
        }
    }
}

/// One blank-line block of a page's text layer — a paragraph — with the two
/// counts that align it to the zone tree ([`block_boxes`]).
struct Block {
    /// The paragraph text: physical lines re-joined with single spaces.
    text: String,
    /// Whitespace-separated tokens — the page's `Word` zones, when the layer
    /// has them.
    words: usize,
    /// Non-blank physical lines — the page's `Line` zones.
    lines: usize,
}

/// Split a page's text layer into paragraphs. The layer is a zone tree
/// (page → region → paragraph → line → word); its plain-text rendering
/// separates paragraphs by a blank line and physical lines by `\n`. A
/// paragraph is those lines re-joined with a single space, whitespace
/// collapsed — which reflows the scan's line breaks back into running prose.
fn blocks(page: &str) -> Vec<Block> {
    page.split("\n\n")
        .filter_map(|block| {
            let words: Vec<&str> = block.split_whitespace().collect();
            if words.is_empty() {
                return None;
            }
            let lines = block.split('\n').filter(|l| !l.trim().is_empty()).count();
            Some(Block {
                text: words.join(" "),
                words: words.len(),
                lines,
            })
        })
        .collect()
}

/// The paragraph texts of a page (see [`blocks`]).
#[cfg(test)]
fn paragraphs(page: &str) -> Vec<String> {
    blocks(page).into_iter().map(|b| b.text).collect()
}

/// The `[l, t, r, b]` pixel boxes of every zone of `kind`, in stream order.
fn zone_boxes(zones: &[TextZone], kind: TextZoneKind, out: &mut Vec<[u32; 4]>) {
    for z in zones {
        if z.kind == kind {
            let r = &z.rect;
            out.push([r.x, r.y, r.x + r.width, r.y + r.height]);
        }
        zone_boxes(&z.children, kind, out);
    }
}

/// Per-paragraph bounding boxes in display pixels (top-left origin), or `None`
/// when the zone tree cannot be aligned with the text.
///
/// The paragraphs are cut from the flat text, so they are matched to zones by
/// *count*, never by the zones' own text slices (which some encoders leave
/// empty or misaligned): the finest level whose zone count equals the text's
/// token count wins — `Word` zones against whitespace tokens, else `Line` zones
/// against non-blank lines, else `Para` zones against the paragraphs — and each
/// paragraph's box is the union of its run of zones. A layer that aligns at no
/// level (word zones split differently from whitespace, lines with no zone…)
/// yields no boxes rather than wrong ones: the JSON then has `pages` but no
/// `prov`, as it does today.
fn block_boxes(layer: &TextLayer, blocks: &[Block]) -> Option<Vec<[u32; 4]>> {
    /// How many zones of a level one paragraph spans.
    type Count = fn(&Block) -> usize;
    let levels: [(TextZoneKind, Count); 3] = [
        (TextZoneKind::Word, |b| b.words),
        (TextZoneKind::Line, |b| b.lines),
        (TextZoneKind::Para, |_| 1),
    ];
    for (kind, count) in levels {
        let mut boxes = Vec::new();
        zone_boxes(&layer.zones, kind, &mut boxes);
        if boxes.is_empty() || boxes.len() != blocks.iter().map(count).sum::<usize>() {
            continue;
        }
        let mut boxes = boxes.into_iter();
        return Some(
            blocks
                .iter()
                .map(|b| {
                    (&mut boxes)
                        .take(count(b))
                        .fold([u32::MAX, u32::MAX, 0, 0], |a, z| {
                            [
                                a[0].min(z[0]),
                                a[1].min(z[1]),
                                a[2].max(z[2]),
                                a[3].max(z[3]),
                            ]
                        })
                })
                .collect(),
        );
    }
    None
}

/// The page's size after its INFO-chunk rotation, in pixels — the frame the
/// (rotated) text zones and the `PageInfo` marker share.
fn display_dims(page: &DjVuPage) -> (u32, u32) {
    let (w, h) = (page.width() as u32, page.height() as u32);
    match page.rotation() {
        Rotation::Cw90 | Rotation::Ccw90 => (h, w),
        _ => (w, h),
    }
}

/// A DjVu page's dpi, with the spec's 300 standing in for an unset `0`.
fn dpi(page: &DjVuPage) -> f32 {
    match page.dpi() {
        0 => 300.0,
        d => f32::from(d),
    }
}

/// One pixel coordinate onto DocLang's 0–511 location grid — the same
/// `clamp(round(512 · v / dim), 0, 511)` the PDF assembler applies to points.
fn grid(v: u32, dim: u32) -> u16 {
    if dim == 0 {
        return 0;
    }
    let g = (512.0 * f64::from(v) / f64::from(dim)).round() as i64;
    g.clamp(0, 511) as u16
}

/// Extract the DjVu text layer into a document: a `PageInfo` marker per page,
/// each page's paragraphs as `Paragraph` nodes in their text-layer `Located`
/// box, `PageBreak` between pages. The returned document has no paragraphs
/// when no selected page carries a text layer — the caller decides whether to
/// fall back to rasterize + OCR.
pub fn convert_text_layer(
    source: &SourceDocument,
    range: Option<(usize, usize)>,
) -> Result<DoclingDocument, ConversionError> {
    let djvu = parse(&source.bytes)?;
    let (first, last) = resolve_range(range, djvu.page_count())?;

    let mut doc = DoclingDocument::new(&source.name);
    for page_no in first..=last {
        if page_no > first {
            doc.push(Node::PageBreak);
        }
        let page = djvu
            .page(page_no - 1)
            .map_err(|e| ConversionError::Parse(format!("djvu: page {page_no}: {e}")))?;
        // The page in PDF points, like a PDF `PageInfo`: pixels over dpi. The
        // real 1-based number keeps a `--pages` window's JSON `pages` keyed
        // like the PDF path's.
        let (dw, dh) = display_dims(page);
        let scale = 72.0 / dpi(page);
        doc.push(Node::PageInfo {
            page_no,
            width: dw as f32 * scale,
            height: dh as f32 * scale,
        });
        let layer = page
            .text_layer()
            .map_err(|e| ConversionError::Parse(format!("djvu: page {page_no} text layer: {e}")))?;
        let Some(layer) = layer else {
            continue;
        };
        // Zones come in the unrotated frame; bring them into the display frame
        // the marker describes (an identity for the usual unrotated page).
        let layer = match page.rotation() {
            Rotation::None => layer,
            rot => layer.transform(page.width() as u32, page.height() as u32, rot, dw, dh),
        };
        let blocks = blocks(&layer.text);
        let boxes = block_boxes(&layer, &blocks);
        for (i, block) in blocks.into_iter().enumerate() {
            let para = Node::Paragraph { text: block.text };
            match boxes.as_ref().map(|b| b[i]) {
                Some([l, t, r, b]) => doc.push(Node::Located {
                    location: [grid(l, dw), grid(t, dh), grid(r, dw), grid(b, dh)],
                    inner: Box::new(para),
                }),
                None => doc.push(para),
            }
        }
    }
    Ok(doc)
}

/// True when a [`convert_text_layer`] document has no text content — every
/// node is a page marker or break (a scan-only DjVu). Signals the caller to try OCR.
pub fn is_text_layer_empty(doc: &DoclingDocument) -> bool {
    fn is_paragraph(n: &Node) -> bool {
        match n {
            Node::Paragraph { .. } => true,
            Node::Located { inner, .. } => is_paragraph(inner),
            _ => false,
        }
    }
    !doc.nodes.iter().any(is_paragraph)
}

/// Rasterize the selected pages to PNG for the image/OCR pipeline (the fallback
/// for a scan-only DjVu), each with its real 1-based page number so the caller
/// can stamp the OCR'd page's `PageInfo` like the text-layer path does. Each page renders through `djvu-rs` fitted into a
/// `DOCLING_RS_DJVU_RENDER_PX` box (2500 px, ~OCR's sweet spot) so a 600-dpi
/// scan does not hand OCR a 30-megapixel bitmap. Only built with the ML
/// pipeline (`pdf`), its sole consumer.
#[cfg(feature = "pdf")]
pub fn rasterize_pages(
    bytes: &[u8],
    range: Option<(usize, usize)>,
) -> Result<Vec<(usize, Vec<u8>)>, ConversionError> {
    use djvu_rs::djvu_render::{render_pixmap, RenderOptions};
    use std::io::Cursor;

    let max_side: u32 = docling_core::env::parse("DOCLING_RS_DJVU_RENDER_PX").unwrap_or(2500);

    let djvu = parse(bytes)?;
    let (first, last) = resolve_range(range, djvu.page_count())?;

    let mut pngs = Vec::with_capacity(last - first + 1);
    for page_no in first..=last {
        let page = djvu
            .page(page_no - 1)
            .map_err(|e| ConversionError::Parse(format!("djvu: page {page_no}: {e}")))?;
        let opts = RenderOptions::fit_to_box(page, max_side, max_side);
        let pm = render_pixmap(page, &opts)
            .map_err(|e| ConversionError::Parse(format!("djvu: rendering page {page_no}: {e}")))?;
        let rgba = image::RgbaImage::from_raw(pm.width, pm.height, pm.data).ok_or_else(|| {
            ConversionError::Parse(format!("djvu: page {page_no}: pixmap size mismatch"))
        })?;
        // OCR/layout are trained on opaque paper: drop the alpha channel.
        let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
        let mut png = Vec::new();
        rgb.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|e| ConversionError::Parse(format!("djvu: encoding page {page_no}: {e}")))?;
        pngs.push((page_no, png));
    }
    Ok(pngs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn magic_recognizes_the_djvu_form_types() {
        let mut multi = b"AT&TFORM\x00\x00\x03\x3cDJVM".to_vec();
        multi.extend_from_slice(b"rest");
        assert!(looks_like_djvu(&multi));
        assert!(looks_like_djvu(b"AT&TFORM\x00\x00\x00\x10DJVUxxxx"));
        assert!(looks_like_djvu(b"AT&TFORM\x00\x00\x00\x10DJVIxxxx"));
        // Not DjVu: PDF, a bare IFF, too short.
        assert!(!looks_like_djvu(b"%PDF-1.7"));
        assert!(!looks_like_djvu(b"AT&TFORM\x00\x00\x00\x10PORTxxxx"));
        assert!(!looks_like_djvu(b"AT&TFORM"));
    }

    #[test]
    fn paragraphs_reflow_blank_line_blocks() {
        // Blank-line-separated blocks become paragraphs; physical line breaks
        // inside a block collapse to single spaces; trailing spaces vanish.
        let page = "First line \nsecond line \n\nNext paragraph \n\n\n  \n";
        assert_eq!(
            paragraphs(page),
            vec![
                "First line second line".to_string(),
                "Next paragraph".to_string()
            ]
        );
        assert!(paragraphs("   \n\n \n").is_empty());
    }

    #[test]
    fn range_resolution_matches_pdf_semantics() {
        assert_eq!(resolve_range(None, 5).unwrap(), (1, 5));
        assert_eq!(resolve_range(Some((2, 3)), 5).unwrap(), (2, 3));
        assert_eq!(resolve_range(Some((2, 99)), 5).unwrap(), (2, 5)); // end clamps
        assert!(resolve_range(Some((0, 3)), 5).is_err()); // 0 is not a page
        assert!(resolve_range(Some((3, 2)), 5).is_err()); // reversed
        assert!(resolve_range(Some((9, 9)), 5).is_err()); // start past end
    }

    /// The bundled 3-page fixture: the text layer comes out as the
    /// blank-line-delimited paragraphs of each page, `--pages` narrows it, and
    /// a corrupt file is a clean error, all in-process (no external binary).
    /// #1137: a page whose INFO chunk turns it by 90° rasterizes in its
    /// display orientation. djvu-rs < 0.45 fitted the *unrotated* page into
    /// the render box, so the OCR fallback got a portrait-shaped bitmap of a
    /// landscape page (a 192 × 256 page in 300 × 300 → 225 × 300, not
    /// 300 × 225), squashing the scan before layout and OCR saw it.
    #[cfg(feature = "pdf")]
    #[test]
    fn rotated_page_rasterizes_in_display_orientation() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/djvu/sources/sample1.djvu"
        );
        let mut bytes = std::fs::read(path).expect("fixture exists");
        // INFO payload: w(2) h(2) minor major dpi(2) gamma flags — the low
        // three flag bits are the rotation; 6 = 90° counter-clockwise.
        let info = bytes
            .windows(4)
            .position(|w| w == b"INFO")
            .expect("INFO chunk");
        bytes[info + 8 + 9] = 6;
        let page = parse(&bytes).unwrap();
        let page = page.page(0).unwrap();
        assert!(matches!(page.rotation(), Rotation::Ccw90));
        let (dw, dh) = display_dims(page);
        assert!(dw > dh, "portrait page turned 90° displays landscape");

        let pngs = rasterize_pages(&bytes, Some((1, 1))).expect("rasterizes");
        assert_eq!(pngs.len(), 1);
        let img = image::load_from_memory(&pngs[0].1).expect("png");
        let (w, h) = (img.width(), img.height());
        assert_eq!(w, 2500, "longer display side fills the default box");
        // Same aspect as the display frame, not the unrotated page's.
        let expected_h = (f64::from(dh) * 2500.0 / f64::from(dw)).round() as u32;
        assert!(
            h.abs_diff(expected_h) <= 1,
            "{w}×{h}, expected 2500×{expected_h}"
        );
    }

    #[test]
    fn text_layer_of_the_fixture_extracts_in_process() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/djvu/sources/example.djvu"
        );
        let bytes = std::fs::read(path).expect("fixture exists");
        let src = SourceDocument::from_bytes("example", InputFormat::Djvu, bytes);

        let doc = convert_text_layer(&src, None).expect("converts");
        assert!(!is_text_layer_empty(&doc));
        let md = doc.export_to_markdown();
        assert!(md.contains("RGB Image CMYK Image CMYK Graphic"), "{md:?}");
        assert!(md.contains("Black RGB Gray RGB Neutral CIELAB"), "{md:?}");
        assert_eq!(
            doc.nodes
                .iter()
                .filter(|n| matches!(n, Node::PageBreak))
                .count(),
            2,
            "3 pages -> 2 page breaks"
        );

        // Provenance: every page opens with its marker (5100×6600 px at 600
        // dpi = US Letter in points) and every paragraph sits in a zone box.
        let markers: Vec<(usize, f32, f32)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::PageInfo {
                    page_no,
                    width,
                    height,
                } => Some((*page_no, *width, *height)),
                _ => None,
            })
            .collect();
        assert_eq!(
            markers,
            vec![(1, 612.0, 792.0), (2, 612.0, 792.0), (3, 612.0, 792.0)]
        );
        assert!(matches!(
            doc.nodes.first(),
            Some(Node::PageInfo { page_no: 1, .. })
        ));
        let mut located = 0;
        for n in &doc.nodes {
            match n {
                Node::Located { location, inner } => {
                    assert!(matches!(inner.as_ref(), Node::Paragraph { .. }));
                    let [l, t, r, b] = *location;
                    assert!(l < r && t < b && r <= 511 && b <= 511, "{location:?}");
                    located += 1;
                }
                Node::Paragraph { .. } => panic!("a bare paragraph escaped its zone box"),
                _ => {}
            }
        }
        assert_eq!(located, 3, "one reflowed paragraph per page, each located");

        // Page 2 alone: no page break, not page 1's specimen table, and the
        // marker keeps the real page number for a windowed conversion.
        let p2 = convert_text_layer(&src, Some((2, 2))).expect("converts");
        assert!(!p2.nodes.iter().any(|n| matches!(n, Node::PageBreak)));
        assert!(!p2.export_to_markdown().contains("RGB Image CMYK Image"));
        assert!(matches!(
            p2.nodes.first(),
            Some(Node::PageInfo { page_no: 2, .. })
        ));

        assert!(convert_text_layer(&src, Some((9, 9))).is_err());
        let junk = SourceDocument::from_bytes("junk", InputFormat::Djvu, b"AT&TFORM junk".to_vec());
        assert!(convert_text_layer(&junk, None).is_err());
    }

    fn zone(
        kind: TextZoneKind,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        children: Vec<TextZone>,
    ) -> TextZone {
        TextZone {
            kind,
            rect: djvu_rs::text::Rect {
                x,
                y,
                width: w,
                height: h,
            },
            // Deliberately empty: the alignment must never read zone text.
            text: String::new(),
            children,
        }
    }

    /// A two-paragraph page whose zone tree has lines and words: the word level
    /// aligns (5 tokens ↔ 5 word zones) and each paragraph's box is the union of
    /// its words — not its lines, which here are drawn wider on purpose.
    #[test]
    fn block_boxes_align_words_first() {
        let text = "Hello big world\n\nBye now\n";
        let layer = TextLayer {
            text: text.into(),
            zones: vec![zone(
                TextZoneKind::Page,
                0,
                0,
                1000,
                1000,
                vec![
                    zone(
                        TextZoneKind::Line,
                        0,
                        100,
                        1000,
                        50,
                        vec![
                            zone(TextZoneKind::Word, 100, 100, 100, 50, vec![]),
                            zone(TextZoneKind::Word, 210, 100, 100, 50, vec![]),
                            zone(TextZoneKind::Word, 320, 105, 100, 40, vec![]),
                        ],
                    ),
                    zone(
                        TextZoneKind::Line,
                        0,
                        300,
                        1000,
                        50,
                        vec![
                            zone(TextZoneKind::Word, 100, 300, 80, 50, vec![]),
                            zone(TextZoneKind::Word, 190, 300, 80, 50, vec![]),
                        ],
                    ),
                ],
            )],
        };
        let blocks = blocks(text);
        assert_eq!((blocks[0].words, blocks[0].lines), (3, 1));
        assert_eq!((blocks[1].words, blocks[1].lines), (2, 1));
        assert_eq!(
            block_boxes(&layer, &blocks),
            Some(vec![[100, 100, 420, 150], [100, 300, 270, 350]])
        );
    }

    /// Word zones that split differently from whitespace (an encoder that
    /// boxes "Hello big" as one word) fall back to the line level; a layer
    /// with no aligning level yields no boxes at all rather than wrong ones.
    #[test]
    fn block_boxes_fall_back_by_level() {
        let text = "Hello big\nworld\n\nBye\n";
        let lines = |words: Vec<TextZone>| {
            vec![zone(
                TextZoneKind::Page,
                0,
                0,
                1000,
                1000,
                vec![
                    zone(TextZoneKind::Line, 10, 100, 500, 50, words),
                    zone(TextZoneKind::Line, 10, 160, 300, 50, vec![]),
                    zone(TextZoneKind::Line, 10, 400, 200, 50, vec![]),
                ],
            )]
        };
        let blocks = blocks(text);
        // 2 word zones for 3 tokens: not the word level; 3 lines ↔ 3 lines.
        let layer = TextLayer {
            text: text.into(),
            zones: lines(vec![
                zone(TextZoneKind::Word, 10, 100, 200, 50, vec![]),
                zone(TextZoneKind::Word, 220, 100, 200, 50, vec![]),
            ]),
        };
        assert_eq!(
            block_boxes(&layer, &blocks),
            Some(vec![[10, 100, 510, 210], [10, 400, 210, 450]])
        );
        // Neither words nor lines nor paragraphs line up.
        let layer = TextLayer {
            text: text.into(),
            zones: vec![zone(
                TextZoneKind::Page,
                0,
                0,
                1000,
                1000,
                vec![zone(TextZoneKind::Line, 0, 0, 10, 10, vec![])],
            )],
        };
        assert_eq!(block_boxes(&layer, &blocks), None);
        // No text layer zones at all.
        let layer = TextLayer {
            text: text.into(),
            zones: vec![],
        };
        assert_eq!(block_boxes(&layer, &blocks), None);
    }

    #[test]
    fn grid_matches_the_pdf_assembler() {
        assert_eq!(grid(0, 1000), 0);
        assert_eq!(grid(500, 1000), 256);
        assert_eq!(grid(1000, 1000), 511, "the far edge clamps onto the grid");
        assert_eq!(grid(7, 0), 0, "a zero-sized page cannot divide");
    }
}
