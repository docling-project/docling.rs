//! `convert_text_layer` (the `pdf-text` / wasm32 path) must produce the same
//! extraction as the full pipeline's `no_ocr` flag: both run the pure-Rust
//! text parser through the orphan-region assembly, differing only in the
//! entry point. Runs under the default (ml) feature so
//! both entries exist to compare. (The scanned-table e2e below shares this
//! binary rather than its own: every docling-pdf test target statically links
//! onnxruntime, so one target carries all the pipeline e2es.)

#![cfg(feature = "ml")]

#[test]
fn text_layer_matches_no_ocr() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/pdf/sources/code_and_formula.pdf"
    ))
    .expect("corpus pdf");

    let text_layer = docling_pdf::convert_text_layer(&bytes, "code_and_formula.pdf")
        .expect("text-layer conversion");
    let no_ocr = docling_pdf::convert_with_options(
        &bytes,
        None,
        "code_and_formula.pdf",
        true,  // no_table_former (moot: no_ocr skips it anyway)
        true,  // no_ocr — the path convert_text_layer mirrors
        false, // force_full_page_ocr (ignored under no_ocr)
        false, // no_text_panels (moot: no_ocr never demotes)
        docling_pdf::EnrichmentOptions::default(),
        None,
        None,
    )
    .expect("no_ocr conversion");

    let a = text_layer.export_to_markdown();
    assert!(!a.trim().is_empty(), "text layer should extract");
    assert_eq!(a, no_ocr.export_to_markdown());
}

/// #173: a scanned (image-only) page with an uncaptioned bar chart above a
/// bordered data table. The chart must stay a picture (line-height uniformity
/// gate) and the table must extract with its OCR'd cell text — region-scoped
/// OCR skips table interiors, so the words come from the dedicated
/// `ocr_table_words` pass; without it the cell matcher saw no words and the
/// table dissolved. Needs the layout/OCR/TableFormer models, so it skips on a
/// model-free CI checkout.
#[test]
fn scanned_page_extracts_table_and_keeps_chart() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    // The page renders through the pure-Rust raster; pdfium is not needed.
    for needed in [
        ".models/layout_heron_int8.onnx",
        ".models/ocr_rec_en.onnx",
        ".models/tableformer/encoder.onnx",
    ] {
        if !root.join(needed).exists() {
            eprintln!("skipping scanned-table e2e: {needed} not found");
            return;
        }
    }
    // Model resolution is CWD-relative; tests run from the crate dir.
    std::env::set_current_dir(&root).expect("chdir to repo root");
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/scanned/sources/scanned_chart_table.pdf"
    ))
    .expect("scanned fixture");
    let doc =
        docling_pdf::convert(&bytes, None, "scanned_chart_table.pdf").expect("scanned conversion");
    let md = doc.export_to_markdown();
    assert!(
        md.contains("<!-- image -->"),
        "uncaptioned chart must keep its picture crop:\n{md}"
    );
    // The 8x4 bordered table: header + 7 body rows, cells carrying OCR text.
    let rows: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
    assert!(
        rows.len() >= 9,
        "expected a header + 7-row table, got {} pipe rows:\n{md}",
        rows.len()
    );
    for cell in ["Aquifer", "Guarani", "Nubian", "380", "540"] {
        assert!(md.contains(cell), "table cell {cell:?} missing:\n{md}");
    }
}

#[test]
fn scanned_pdf_yields_empty_document() {
    // An image-only page — the no-text-layer contract is an empty doc, not
    // an error (callers decide whether to fall back to OCR).
    let scan = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/scanned/sources/ocr_test_raster.pdf"),
    )
    .unwrap();
    let doc = docling_pdf::convert_text_layer(&scan, "scan.pdf").expect("no error");
    assert!(doc.nodes.is_empty());
    // (Bytes no reader opens are an error, not an empty document:
    // `lib.rs`'s `text_layer_path_reports_an_unreadable_file`.)
}

/// #211: HEIC is detected by content, and without the `heif` feature the
/// error names the fix instead of a generic decode failure. Runs before any
/// model loads, so it needs no assets. (With the feature on, the same bytes
/// fail in libheif instead — a 12-byte file is not a real container.)
#[test]
fn heif_without_feature_reports_clearly() {
    let stub = b"\x00\x00\x00\x18ftypheic\0\0\0\0";
    let err = docling_pdf::convert_image(stub, "photo.heic").unwrap_err();
    let msg = err.to_string();
    #[cfg(not(feature = "heif"))]
    assert!(msg.contains("--features heif"), "unexpected error: {msg}");
    #[cfg(feature = "heif")]
    assert!(msg.contains("heif"), "unexpected error: {msg}");
}

/// #517: ONNX Runtime 1.26–1.28's x86 NCHWc rewrite collapsed the picture
/// classifier onto one distribution for every input (`table` 0.091 first).
/// Three unambiguous figures from the corpus must each win their own class
/// clearly (ONNX Runtime 1.29 gives them 0.985–0.999; the bar is 0.9).
#[test]
fn picture_classifier_tells_pictures_apart() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let model = root.join(".models/picture_classifier.onnx");
    if !model.exists() {
        eprintln!(
            "skipping picture-classifier check: {} not found",
            model.display()
        );
        return;
    }
    std::env::set_var("DOCLING_PICTURE_CLASSIFIER_ONNX", &model);
    let mut classifier =
        docling_pdf::enrich::PictureClassifier::load_with(1).expect("classifier loads");
    for (file, class) in [
        (
            "latex/sources/2310.06825/images/230927_bars.png",
            "bar_chart",
        ),
        (
            "latex/sources/1706.03762/Figures/ModalNet-21.png",
            "flow_chart",
        ),
        ("scanned/sources/qr_bill_example.jpg", "qr_code"),
    ] {
        let img = image::open(root.join("tests/data").join(file))
            .expect("fixture image")
            .to_rgb8();
        let preds = classifier.classify(&img).expect("classify");
        let top = &preds[0];
        assert!(
            top.class_name == class && top.confidence > 0.9,
            "{file}: expected {class}, got {} {:.3}",
            top.class_name,
            top.confidence
        );
    }
}

/// #915: the per-cell recognition confidence the OCR road computes reaches
/// the document's confidence report (beside, not inside, the docling JSON).
/// A one-word Helvetica page read by full-page OCR yields one cell whose
/// confidence is the page's `ocr_score`; a text-layer conversion has none.
#[test]
fn ocr_cells_carry_their_confidence() {
    if !std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.models/ocr_det.onnx"
    ))
    .exists()
        || !std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../.models/layout_heron.onnx"
        ))
        .exists()
    {
        eprintln!("skip: OCR/layout models not present");
        return;
    }
    let pdf = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n\
4 0 obj\n<< /Length 46 >>\nstream\nBT /F1 18 Tf 1 0 0 1 30 240 Tm (VISIBLE) Tj ET\nendstream\nendobj\n\
5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n\
trailer\n<< /Size 6 /Root 1 0 R >>\n%%EOF\n";

    let convert = |no_ocr: bool| {
        docling_pdf::convert_with_options(
            pdf,
            None,
            "fixture.pdf",
            true,
            no_ocr,
            !no_ocr, // force_full_page_ocr
            false,
            docling_pdf::EnrichmentOptions::default(),
            None,
            None,
        )
        .expect("conversion")
    };

    let doc = convert(false);
    let report = doc.confidence.as_ref().expect("confidence report");
    let cells = report.ocr_cells(1);
    assert!(!cells.is_empty(), "OCR cells expected");
    assert!(
        cells.iter().any(|c| c.text.contains("VISIBLE")),
        "{cells:?}"
    );
    assert!(cells.iter().all(|c| (0.0..=1.0).contains(&c.confidence)));
    // The page aggregate is still the mean of the same figures.
    let mean = cells.iter().map(|c| f64::from(c.confidence)).sum::<f64>() / cells.len() as f64;
    let ocr_score = report.pages[&1].ocr_score.expect("ocr_score");
    assert!((mean - ocr_score).abs() < 1e-6, "{mean} vs {ocr_score}");
    // Not in the document JSON, which stays docling's schema.
    assert!(!doc
        .export_to_json_value()
        .to_string()
        .contains("confidence"));

    let digital = convert(true);
    assert!(digital.confidence.as_ref().unwrap().ocr_cells(1).is_empty());
}

/// A one-page PDF drawing English prose in Helvetica whose `/ToUnicode` map
/// moves every letter by three code points — the broken font mapping
/// docling#4654's `replace_broken_text_layer` targets (#1006): the page shows
/// "The quarterly report…", its text layer reads "Wkh txduwhuo|…".
fn shifted_tounicode_pdf() -> Vec<u8> {
    let line = "The quarterly report shows that revenue increased across regions";
    let mut ops = String::from("BT /F1 11 Tf 14 TL 40 760 Td\n");
    for _ in 0..12 {
        ops.push_str(&format!("({line}) Tj T*\n"));
    }
    ops.push_str("ET");
    let cmap = "/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n\
/CMapName /Shift def 1 begincodespacerange <00> <FF> endcodespacerange\n\
2 beginbfrange <41> <5A> <0044> <61> <7A> <0064> endbfrange\n\
endcmap CMapName currentdict /CMap defineresource pop end end";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!("<< /Length {} >>\nstream\n{ops}\nendstream", ops.len() + 1),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode 6 0 R >>".to_string(),
        format!(
            "<< /Length {} >>\nstream\n{cmap}\nendstream",
            cmap.len() + 1
        ),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// #1006: the text layer of the shifted-ToUnicode page reads as moved
/// letters, and the detector judges it broken on exactly the cells the
/// pipeline extracts — no models needed.
#[test]
fn shifted_tounicode_text_layer_is_judged_broken() {
    let pdf = shifted_tounicode_pdf();
    let md = docling_pdf::convert_text_layer(&pdf, "shifted.pdf")
        .expect("text-layer conversion")
        .export_to_markdown();
    assert!(md.contains("Wkh txduwhuo"), "{md}");
    assert!(docling_pdf::quality::is_broken_text_layer(&md), "{md}");
    // The same page with its real text is kept.
    assert!(!docling_pdf::quality::is_broken_text_layer(
        &"The quarterly report shows that revenue increased across regions\n".repeat(12)
    ));
}

/// #1006 end to end: with `replace_broken_text_layer` the broken page is
/// OCR'd (its drawn words come back) and its `parse_score` is 0; without
/// it the page keeps its (moved) text layer. Needs the layout + OCR models.
#[test]
fn broken_text_layer_is_replaced_by_ocr() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join(".models/ocr_det.onnx").exists()
        || !(root.join(".models/layout_heron.onnx").exists()
            || root.join(".models/layout_heron_int8.onnx").exists())
    {
        eprintln!("skip: OCR/layout models not present");
        return;
    }
    // Model resolution is CWD-relative; tests run from the crate dir.
    std::env::set_current_dir(&root).expect("chdir to repo root");
    let pdf = shifted_tounicode_pdf();
    let convert = |replace: bool| {
        docling_pdf::Pipeline::new()
            .expect("pipeline")
            .no_table_former(true)
            .replace_broken_text_layer(replace)
            .convert(&pdf, None, "shifted.pdf")
            .expect("conversion")
    };
    let kept = convert(false);
    assert!(kept.export_to_markdown().contains("Wkh txduwhuo"));
    let parse = |d: &docling_core::DoclingDocument| {
        d.confidence.as_ref().expect("confidence").pages[&1].parse_score
    };
    assert!(parse(&kept).is_some_and(|s| s > 0.0));

    let replaced = convert(true);
    let md = replaced.export_to_markdown();
    // OCR reads the drawn words (its word spacing varies with the host font that stands in for the non-embedded Helvetica).
    assert!(md.contains("quar") && md.contains("hat"), "{md}");
    assert!(!md.contains("Wkh txduwhuo"), "{md}");
    assert_eq!(parse(&replaced), Some(0.0));
}
