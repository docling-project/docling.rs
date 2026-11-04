//! #939: the layout-free recognition API — `OcrModel::recognize_crops` /
//! `recognize_det_boxes` on a detector's boxes, reachable from the crate
//! root. Needs the OCR detector + recognizer, so it skips on a model-free
//! CI checkout.

#![cfg(feature = "ml")]

use docling_pdf::ocr_det::{DetBox, DetModel};
use docling_pdf::ocr_prep::det_box_crop;
use docling_pdf::{OcrLang, OcrModel, RecognizedLine};

#[test]
fn detector_boxes_recognize_index_aligned_without_layout() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rec = [".models/ocr_rec_v6.onnx", ".models/ocr_rec_en.onnx"];
    if !root.join(".models/ocr_det.onnx").exists() || !rec.iter().any(|r| root.join(r).exists()) {
        eprintln!("skipping crop recognition: OCR detector/recognizer not found");
        return;
    }
    // Model resolution is CWD-relative; tests run from the crate dir.
    std::env::set_current_dir(&root).expect("chdir to repo root");
    let img = image::open(root.join("tests/data/scanned/sources/qr_bill_example.jpg"))
        .expect("fixture")
        .to_rgb8();

    let mut det = DetModel::load(1).expect("detector");
    let boxes = det.detect(&img).expect("detect");
    assert!(boxes.len() > 10, "{} boxes", boxes.len());

    let mut ocr = OcrModel::load_with(OcrLang::En, 1).expect("recognizer");
    let read = ocr.recognize_det_boxes(&img, &boxes).expect("recognize");
    assert_eq!(read.len(), boxes.len());
    let kept: Vec<&RecognizedLine> = read.iter().filter(|r| r.passes_text_score()).collect();
    assert!(kept.len() * 2 > read.len(), "{read:?}");
    assert!(read.iter().any(|r| r.text.contains("Receipt")), "{read:?}");
    for r in &read {
        assert!((0.0..=1.0).contains(&r.confidence), "{r:?}");
        assert_eq!(r.text, r.text.trim());
    }

    // The same pixels through the plain crop entry point read identically.
    let crops: Vec<_> = boxes
        .iter()
        .map(|d| det_box_crop(&img, d).expect("crop"))
        .collect();
    assert_eq!(ocr.recognize_crops(&crops).expect("crops"), read);

    // A box outside the image keeps its slot (empty, 0.0) and moves nothing;
    // more recognition lanes change nothing either.
    let mut with_gap = boxes.clone();
    let off = img.width() as f32 + 10.0;
    with_gap.insert(1, DetBox::rect(off, 0.0, off + 40.0, 20.0, 0.9));
    let mut lanes = OcrModel::load_with(OcrLang::En, 3).expect("recognizer");
    let gapped = lanes
        .recognize_det_boxes(&img, &with_gap)
        .expect("recognize");
    assert_eq!(gapped.len(), with_gap.len());
    assert_eq!(
        gapped[1],
        RecognizedLine {
            text: String::new(),
            confidence: 0.0
        }
    );
    let mut rest = gapped;
    rest.remove(1);
    assert_eq!(rest, read);

    // RapidOCR's oriented crops (#666): one result per box all the same.
    ocr.set_textline_orientation(true);
    let oriented = ocr.recognize_det_boxes(&img, &boxes).expect("oriented");
    assert_eq!(oriented.len(), boxes.len());
    assert!(
        oriented.iter().any(|r| r.text.contains("Receipt")),
        "{oriented:?}"
    );
}
