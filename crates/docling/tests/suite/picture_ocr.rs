//! e2e for #645: the picture-OCR enrichment reads the text off the pictures
//! embedded in a non-PDF document — a generated DOCX with a 720 × 180
//! screenshot of two text lines and a 16 × 16 icon — and attaches it as the
//! picture's description: Markdown prints it between the caption and the
//! placeholder, the JSON picture item carries `meta.description` + the
//! `description` annotation, the chunks include it. Off (the default) the
//! output is byte-identical to the regression baseline; the class filter,
//! the size floor, `keep_picture_images(false)`, `no_ocr` and a missing
//! recognizer each degrade exactly as documented. Needs the OCR models
//! (`.models/ocr_rec*`), skipping cleanly without them like the other ML
//! tests; the class-filter test also needs the picture classifier.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/docx/sources/picture_ocr.docx")
}

/// The recognizer pair the engine would pick (the v6 model, else the English
/// v3 pair) is installed; CWD moves to the repo root, where `.models/` is.
fn ocr_models_ready() -> bool {
    let models = repo_root().join(".models");
    let v6 = models.join("ocr_rec_v6.onnx").exists() && models.join("ocr_rec_v6_dict.txt").exists();
    let en = models.join("ocr_rec_en.onnx").exists() && models.join("en_dict.txt").exists();
    (v6 || en) && std::env::set_current_dir(repo_root()).is_ok()
}

fn convert(converter: DocumentConverter) -> docling_core::DoclingDocument {
    let source = SourceDocument::from_file(fixture()).expect("fixture");
    converter.convert(source).expect("convert").document
}

fn json(doc: &docling_core::DoclingDocument) -> Value {
    serde_json::from_str(&doc.export_to_json()).expect("valid JSON")
}

/// `(meta.description.text, has an image)` per JSON picture item.
fn pictures(json: &Value) -> Vec<(Option<String>, bool)> {
    json["pictures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["meta"]["description"]["text"]
                    .as_str()
                    .map(str::to_string),
                p.get("image").is_some(),
            )
        })
        .collect()
}

const LINE_1: &str = "Quarterly revenue grew by 12 percent";
const LINE_2: &str = "Open the Settings dialog to continue";

/// The screenshot's two lines land on the picture in reading order; the
/// icon, under the 32 px floor, gets nothing — and every export carries the
/// text where docling puts a picture description.
#[test]
fn picture_text_is_read_into_the_description() {
    if !ocr_models_ready() {
        eprintln!("skipping: OCR models not found");
        return;
    }
    let doc = convert(DocumentConverter::new().do_picture_ocr(true));
    let md = doc.export_to_markdown();
    let expected = format!(
        "The screenshot below carries text the OCR can read.\n\n{LINE_1}  \n{LINE_2}\n\n\
         <!-- image -->\n\nA tiny icon follows; it is too small to read.\n\n<!-- image -->\n\n\
         The end.\n"
    );
    assert_eq!(md, expected);

    let j = json(&doc);
    assert_eq!(
        pictures(&j),
        vec![(Some(format!("{LINE_1}\n{LINE_2}")), true), (None, true)]
    );
    let shot = &j["pictures"][0];
    assert_eq!(shot["meta"]["description"]["created_by"], "ppocr");
    assert_eq!(
        shot["annotations"],
        serde_json::json!([{
            "kind": "description",
            "text": format!("{LINE_1}\n{LINE_2}"),
            "provenance": "ppocr",
        }])
    );
    assert_eq!(j["pictures"][1]["annotations"], serde_json::json!([]));

    // The chunker carries the description as the picture's chunk.
    let chunks = docling_core::chunker::HierarchicalChunker.chunk(&doc);
    assert!(
        chunks.iter().any(|c| c.text.contains(LINE_1)),
        "no chunk carries the OCR text: {:?}",
        chunks.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

/// Off (the default), nothing changes: the regression baseline is the
/// output, and no JSON picture carries a description.
#[test]
fn off_by_default_leaves_the_output_unchanged() {
    let doc = convert(DocumentConverter::new());
    let expected =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/docx/expected/picture_ocr.docx.md");
    let expected = std::fs::read_to_string(expected).expect("regression baseline");
    assert_eq!(doc.export_to_markdown(), expected);
    assert_eq!(pictures(&json(&doc)), vec![(None, true), (None, true)]);
}

/// The class filter reads a picture only when the classifier's top label is
/// one of the requested ones: the screenshot's own label passes it, any
/// other label skips it.
#[test]
fn class_filter_gates_on_the_top_prediction() {
    if !ocr_models_ready() || !repo_root().join(".models/picture_classifier.onnx").exists() {
        eprintln!("skipping: OCR models or the picture classifier not found");
        return;
    }
    let mut pipeline = docling_pdf::Pipeline::new().unwrap();
    let doc = convert(DocumentConverter::new());
    let shot = doc
        .nodes
        .iter()
        .find_map(|n| match n {
            docling_core::Node::Picture {
                image: Some(img), ..
            } if img.width > 16 => Some(img.data.clone()),
            _ => None,
        })
        .expect("the screenshot picture");
    let img = pipeline.decode_picture(&shot).unwrap();
    let top = pipeline.classify_picture(&img).expect("classifier loaded")[0]
        .class_name
        .clone();
    let other = if top == "logo" { "qr_code" } else { "logo" };

    let read = convert(
        DocumentConverter::new()
            .do_picture_ocr(true)
            .picture_ocr_classes([top.as_str()]),
    );
    assert_eq!(
        pictures(&json(&read))[0].0.as_deref(),
        Some(format!("{LINE_1}\n{LINE_2}").as_str())
    );
    let skipped = convert(
        DocumentConverter::new()
            .do_picture_ocr(true)
            .picture_ocr_classes([other]),
    );
    assert_eq!(pictures(&json(&skipped)), vec![(None, true), (None, true)]);
}

/// A size floor above the screenshot's height skips it too; `0` sends the
/// icon to the models as well (what they make of a black square is the
/// models' business — the screenshot is still read).
#[test]
fn min_side_floor_skips_small_pictures() {
    if !ocr_models_ready() {
        eprintln!("skipping: OCR models not found");
        return;
    }
    let doc = convert(
        DocumentConverter::new()
            .do_picture_ocr(true)
            .picture_ocr_min_side(200),
    );
    assert_eq!(pictures(&json(&doc)), vec![(None, true), (None, true)]);
    let doc = convert(
        DocumentConverter::new()
            .do_picture_ocr(true)
            .picture_ocr_min_side(0),
    );
    assert_eq!(
        pictures(&json(&doc))[0].0.as_deref(),
        Some(format!("{LINE_1}\n{LINE_2}").as_str())
    );
}

/// `keep_picture_images(false)` drops the image bytes after the pass: no
/// JSON `image`, the description kept, the Markdown unchanged.
#[test]
fn dropping_the_images_keeps_the_text() {
    if !ocr_models_ready() {
        eprintln!("skipping: OCR models not found");
        return;
    }
    let doc = convert(
        DocumentConverter::new()
            .do_picture_ocr(true)
            .keep_picture_images(false),
    );
    assert_eq!(
        pictures(&json(&doc)),
        vec![(Some(format!("{LINE_1}\n{LINE_2}")), false), (None, false)]
    );
    assert!(doc.export_to_markdown().contains(LINE_1));
    // The flat nodes lost their payload too.
    assert!(doc
        .nodes
        .iter()
        .all(|n| !matches!(n, docling_core::Node::Picture { image: Some(_), .. })));
}

/// With OCR disabled the conversion succeeds and no picture carries text.
#[test]
fn no_ocr_reads_nothing() {
    let doc = convert(DocumentConverter::new().do_picture_ocr(true).no_ocr(true));
    assert_eq!(pictures(&json(&doc)), vec![(None, true), (None, true)]);
    let doc = convert(
        DocumentConverter::new()
            .do_picture_ocr(true)
            .text_layer_only(true),
    );
    assert_eq!(pictures(&json(&doc)), vec![(None, true), (None, true)]);
}

/// A missing recognizer is a warning, not an error: the conversion succeeds
/// with no text. Run in a child process so the model-path override never
/// leaks into the other tests' environment.
#[test]
fn missing_model_warns_and_succeeds() {
    if std::env::var_os("DOCLING_RS_PICTURE_OCR_CHILD").is_some() {
        let doc = convert(DocumentConverter::new().do_picture_ocr(true));
        assert_eq!(pictures(&json(&doc)), vec![(None, true), (None, true)]);
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(test_name("missing_model_warns_and_succeeds"))
        .args(["--exact", "--nocapture"])
        .env("DOCLING_RS_PICTURE_OCR_CHILD", "1")
        .env("DOCLING_OCR_REC_ONNX", "/nonexistent/ocr_rec.onnx")
        .env("DOCLING_OCR_DICT", "/nonexistent/dict.txt")
        .output()
        .expect("spawn the child test");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stderr}");
    assert!(
        stderr.contains("OCR model unavailable"),
        "no one-time warning in:\n{stderr}"
    );
}

/// A clip with "Frame text sample" drawn on every frame: `seconds` long,
/// background white for the first half and grey for the second — one soft
/// cut whose two sides hash alike for the de-duplication (#647). `None`
/// when this ffmpeg cannot draw text, or the ASR models are missing (a
/// silent clip still goes through the ASR entry point).
fn text_clip(dir: &std::path::Path, seconds: u32) -> Option<(PathBuf, Option<&'static str>)> {
    let preset = [None, Some("parakeet_tdt_0.6b_v3")]
        .into_iter()
        .find(|p| docling_asr::models_available_for(*p));
    let Some(preset) = preset else {
        eprintln!("skipping: no ASR model installed");
        return None;
    };
    std::fs::create_dir_all(dir).unwrap();
    let clip = dir.join("frame_text.mp4");
    let ffmpeg = std::env::var("DOCLING_FFMPEG").unwrap_or_else(|_| "ffmpeg".into());
    let half = seconds / 2;
    let status = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg(format!("color=c=black:s=640x160:d={seconds}:r=10"))
        .args(["-vf"])
        .arg(format!(
            "geq=r='if(lt(T\\,{half})\\,255\\,176)':g='if(lt(T\\,{half})\\,255\\,176)':b='if(lt(T\\,{half})\\,255\\,176)',\
             drawtext=fontfile=/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf:text='Frame text sample':fontsize=40:fontcolor=black:x=20:y=60"
        ))
        .args(["-pix_fmt", "yuv420p", "-y"])
        .arg(&clip)
        .status();
    let Ok(status) = status else {
        eprintln!("skipping: ffmpeg could not run");
        return None;
    };
    if !status.success() {
        eprintln!("skipping: this ffmpeg cannot draw text");
        return None;
    }
    Some((clip, preset))
}

/// A sampled video frame is a picture like any other: the text drawn into a
/// generated clip comes back on its frame. Needs ffmpeg with `drawtext`.
#[test]
fn video_frames_are_read_too() {
    if !ocr_models_ready() || !docling::video::ffmpeg_available() {
        eprintln!("skipping: OCR models or ffmpeg not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("docling-picture-ocr-{}", std::process::id()));
    let Some((clip, preset)) = text_clip(&dir, 2) else {
        return;
    };
    let source = SourceDocument::from_file(&clip).expect("clip");
    let mut converter = DocumentConverter::new()
        .video_frames(1)
        .do_picture_ocr(true);
    if let Some(p) = preset {
        converter = converter.asr_model(Some(p.to_string()));
    }
    let result = converter.convert(source).expect("convert the clip");
    let _ = std::fs::remove_dir_all(&dir);
    let texts = pictures(&json(&result.document));
    assert!(
        texts
            .iter()
            .any(|(t, _)| t.as_deref().is_some_and(|t| t.contains("Frame text"))),
        "no frame carries the drawn text: {texts:?}"
    );
}

/// #647: the frames go through the picture enrichment one at a time —
/// with `keep_picture_images(false)` no frame reaches the document with
/// its bytes, the OCR text still does; `video_frame_dedupe` collapses the
/// re-lit second half onto the first frame, `None` keeps both.
#[test]
fn video_frames_stream_through_the_enrichment() {
    if !ocr_models_ready() || !docling::video::ffmpeg_available() {
        eprintln!("skipping: OCR models or ffmpeg not found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("docling-picture-ocr-647-{}", std::process::id()));
    let Some((clip, preset)) = text_clip(&dir, 4) else {
        return;
    };
    let run = |dedupe: Option<u32>| {
        let source = SourceDocument::from_file(&clip).expect("clip");
        let mut converter = DocumentConverter::new()
            .video_frames_all()
            .video_frame_dedupe(dedupe)
            .do_picture_ocr(true)
            .keep_picture_images(false);
        if let Some(p) = preset {
            converter = converter.asr_model(Some(p.to_string()));
        }
        let result = converter.convert(source).expect("convert the clip");
        pictures(&json(&result.document))
    };
    let both = run(None);
    let one = run(Some(5));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(both.len(), 2, "first frame + the re-lit cut: {both:?}");
    assert_eq!(one.len(), 1, "de-duplicated: {one:?}");
    for (text, has_image) in both.iter().chain(&one) {
        assert!(!has_image, "frame bytes dropped before the document");
        assert!(
            text.as_deref().is_some_and(|t| t.contains("Frame text")),
            "frame read before its bytes were dropped: {text:?}"
        );
    }
}

/// A test's name as libtest matches it with `--exact`: its path without the
/// crate, so `picture_ocr::…` inside the `suite` binary and the bare name in
/// a binary of its own.
fn test_name(name: &str) -> String {
    let path = format!("{}::{name}", module_path!());
    match path.split_once("::") {
        Some((_crate, rest)) => rest.to_string(),
        None => path,
    }
}

/// #1210: the picture-OCR pass reports what its reads met on the result —
/// `Available` when it read, nothing when it was off.
#[test]
fn the_pass_reports_its_ocr_status() {
    if !ocr_models_ready() {
        eprintln!("skipping: OCR models not found");
        return;
    }
    let source = || SourceDocument::from_file(fixture()).expect("fixture");
    let read = DocumentConverter::new()
        .do_picture_ocr(true)
        .convert(source())
        .expect("convert");
    assert_eq!(read.ocr_status, Some(docling::OcrStatus::Available));
    let off = DocumentConverter::new().convert(source()).expect("convert");
    assert_eq!(off.ocr_status, None);
}
