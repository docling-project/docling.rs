//! Minimal CLI: convert a file and print Markdown or JSON to stdout.
//!
//! A stand-in for `docling.cli.main`; the full Typer-style CLI (batch mode,
//! pipeline options) is a later phase.
//!
//! Usage: docling-rs [--strict] [--to md|json] [--images MODE] [--fetch-images] [--no-stream] [--no-table-former] [--no-ocr] [--use-web-browser] [--enrich-picture-classes] [--enrich-code] [--enrich-formula] <input-file>
//!   --to md|json       output format (default: md). `json` emits docling-core's
//!                      native DoclingDocument JSON (export_to_dict).
//!   --images MODE      picture handling for Markdown (mirrors docling's
//!                      image_mode): placeholder (default) | embedded | referenced.
//!                      `referenced` writes image files under ./artifacts/.
//!                      JSON always embeds extracted images as data URIs.
//!   --fetch-images     for HTML/EPUB, resolve external <img src> (data: URIs,
//!                      local files, http(s) URLs, EPUB archive entries) and embed
//!                      the bytes. Off by default; fetches over the network.
//!   --strict           cleaner, more conformant Markdown instead of byte-for-byte
//!                      docling-legacy output (Markdown only).
//!   --no-stream        build the whole document before printing Markdown instead
//!                      of streaming it page by page. Streaming is the default for
//!                      Markdown (placeholder/embedded images); JSON and referenced
//!                      images always use the buffered path.
//!   --no-table-former  skip loading/running the TableFormer table-structure
//!                      model for PDF/image input; tables fall back to simple
//!                      geometric reconstruction from cell positions. Faster
//!                      (no model load, no per-table inference) at the cost of
//!                      table fidelity — helps most in streaming mode.
//!   --no-ocr           skip layout detection, OCR, and TableFormer entirely for
//!                      PDF/image input — no model load or inference at all.
//!                      Emits the embedded text layer as flat paragraphs in
//!                      reading order (no headings/lists/tables/pictures). The
//!                      fastest option, but a scanned/image-only PDF (no
//!                      embedded text layer) yields no text — convert those
//!                      without this flag.
//!   --use-web-browser  pre-render HTML/MHTML/EPUB in the system Chromium (driven
//!                      from Rust) so stylesheet-driven `display:none` elements
//!                      (e.g. a collapsed nav menu) are dropped before parsing.
//!                      Requires building with `--features web-browser`.
//!   --enrich-picture-classes
//!                      classify each detected picture (PDF/image input) with the
//!                      DocumentFigureClassifier model; the 26-class prediction
//!                      distribution lands in the JSON picture item (docling's
//!                      do_picture_classification). Needs
//!                      models/picture_classifier.onnx.
//!   --enrich-code      rewrite detected code blocks (and detect their language)
//!                      with the CodeFormulaV2 VLM (docling's do_code_enrichment).
//!                      Needs models/code_formula/. Slow on CPU: an autoregressive
//!                      generation per code block.
//!   --enrich-formula   decode display formulas to LaTeX with CodeFormulaV2
//!                      (docling's do_formula_enrichment); Markdown then renders
//!                      $$latex$$ instead of the formula placeholder comment.

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use docling::{DocumentConverter, ImageMode, InputFormat, Pipeline, SourceDocument};

fn main() -> ExitCode {
    let mut strict = false;
    let mut to = "md".to_string();
    let mut images = "placeholder".to_string();
    let mut fetch_images = false;
    let mut no_stream = false;
    let mut no_table_former = false;
    let mut no_ocr = false;
    let mut use_web_browser = false;
    let mut enrich_picture_classes = false;
    let mut enrich_code = false;
    let mut enrich_formula = false;
    let mut bench_warm: Option<usize> = None;
    let mut path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => strict = true,
            "--fetch-images" => fetch_images = true,
            "--no-stream" => no_stream = true,
            "--no-table-former" => no_table_former = true,
            "--no-ocr" => no_ocr = true,
            "--use-web-browser" => use_web_browser = true,
            // Opt-in enrichment models (docling CLI flag names): picture
            // classification, code rewrite + language, formula LaTeX.
            "--enrich-picture-classes" => enrich_picture_classes = true,
            "--enrich-code" => enrich_code = true,
            "--enrich-formula" => enrich_formula = true,
            "--to" => to = args.next().unwrap_or_default(),
            "--images" => images = args.next().unwrap_or_default(),
            // Hidden benchmarking aid: load the PDF/image pipeline once, then time
            // N warm conversions (models already loaded), printing the avg seconds
            // per conversion to stdout. This is the startup-excluded counterpart to
            // Python docling's in-process "warm" measurement, for a fair head-to-head.
            "--bench-warm" => {
                bench_warm = args.next().and_then(|n| n.parse::<usize>().ok());
                if bench_warm.is_none() {
                    eprintln!("error: --bench-warm needs a positive run count");
                    return ExitCode::from(2);
                }
            }
            _ => path = Some(arg),
        }
    }

    if !matches!(to.as_str(), "md" | "markdown" | "json" | "dclx" | "chunks") {
        eprintln!("error: unknown --to '{to}' (expected: md, json, dclx, chunks)");
        return ExitCode::from(2);
    }
    let image_mode = match images.as_str() {
        "placeholder" => ImageMode::Placeholder,
        "embedded" => ImageMode::Embedded,
        "referenced" => ImageMode::Referenced,
        other => {
            eprintln!(
                "error: unknown --images '{other}' (expected: placeholder, embedded, referenced)"
            );
            return ExitCode::from(2);
        }
    };

    let Some(path) = path else {
        eprintln!("usage: docling-rs [--strict] [--to md|json] [--images MODE] [--fetch-images] [--no-stream] [--no-table-former] [--no-ocr] [--use-web-browser] <input-file>");
        return ExitCode::from(2);
    };

    let source = match SourceDocument::from_file(&path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(runs) = bench_warm {
        return match bench_warm_conversion(&source, runs, no_table_former, no_ocr) {
            Ok(avg) => {
                // Bare seconds on stdout for the benchmark harness; a human line on stderr.
                println!("{avg:.6}");
                eprintln!(
                    "warm conversion: {:.4}s/doc over {runs} runs (startup excluded)",
                    avg
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let converter = DocumentConverter::new()
        .strict(strict)
        .fetch_images(fetch_images)
        .no_table_former(no_table_former)
        .no_ocr(no_ocr)
        .use_web_browser(use_web_browser)
        .do_picture_classification(enrich_picture_classes)
        .do_code_enrichment(enrich_code)
        .do_formula_enrichment(enrich_formula);

    // Stream Markdown by default: print each chunk as the converter produces it
    // (page by page for PDF). JSON needs the whole tree, and the referenced image
    // mode writes sidecar files, so both keep the buffered path. `--no-stream` opts
    // back into buffering for the streamable cases too.
    let is_markdown = matches!(to.as_str(), "md" | "markdown");
    if is_markdown && image_mode != ImageMode::Referenced && !no_stream {
        let stream = match converter.convert_streaming_images(source, image_mode) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        for chunk in stream {
            match chunk {
                Ok(s) => {
                    if let Err(e) = out.write_all(s.as_bytes()) {
                        eprintln!("error: writing output: {e}");
                        return ExitCode::FAILURE;
                    }
                }
                Err(e) => {
                    let _ = out.flush();
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if let Err(e) = out.flush() {
            eprintln!("error: writing output: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let document = match converter.convert(source) {
        Ok(result) => result.document,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if to == "json" {
        println!("{}", document.export_to_json());
        return ExitCode::SUCCESS;
    }

    if to == "chunks" {
        // Chunking conformance/debug dump: a JSON object with the hierarchical
        // chunk records and, when a tokenizer is configured, the hybrid ones.
        // `DOCLING_CHUNK_TOKENIZER` points at a HuggingFace tokenizer.json
        // (`DOCLING_CHUNK_MAX_TOKENS` overrides the default budget of 256).
        print!("{}", chunks_json(&document));
        return ExitCode::SUCCESS;
    }

    if to == "dclx" {
        // Binary OPC archive: written next to the CWD as `<input-stem>.dclx`
        // (stdout stays clean for terminals); the path is printed for scripts.
        let stem = Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let out = std::path::PathBuf::from(format!("{stem}.dclx"));
        if let Err(e) = docling::dclx::save_as_dclx(&document, &out) {
            eprintln!("error: dclx: {e}");
            return ExitCode::FAILURE;
        }
        println!("{}", out.display());
        return ExitCode::SUCCESS;
    }

    if image_mode == ImageMode::Placeholder {
        print!("{}", document.export_to_markdown());
        return ExitCode::SUCCESS;
    }

    let (md, artifacts) = document.export_to_markdown_with_images(image_mode, "artifacts");
    for (rel, bytes) in &artifacts {
        let rel = Path::new(rel);
        if let Some(dir) = rel.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("error: creating {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }
        if let Err(e) = std::fs::write(rel, bytes) {
            eprintln!("error: writing {}: {e}", rel.display());
            return ExitCode::FAILURE;
        }
    }
    if !artifacts.is_empty() {
        eprintln!("wrote {} image(s) to ./artifacts/", artifacts.len());
    }
    print!("{md}");
    ExitCode::SUCCESS
}

/// Build the PDF/image pipeline once (loading the ONNX models), then time `runs`
/// warm conversions and return the average seconds per conversion. The first
/// conversion is a discarded warm-up that triggers the lazy model loads, so the
/// timed runs reuse them — the startup-excluded figure comparable to docling's
/// in-process warm number.
fn bench_warm_conversion(
    source: &SourceDocument,
    runs: usize,
    no_table_former: bool,
    no_ocr: bool,
) -> Result<f64, String> {
    let mut pipeline = Pipeline::new()
        .map_err(|e| e.to_string())?
        .no_table_former(no_table_former)
        .no_ocr(no_ocr);
    let once = |p: &mut Pipeline| -> Result<(), String> {
        match source.format {
            InputFormat::Pdf => p
                .convert(&source.bytes, None, &source.name)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            InputFormat::Image => p
                .convert_image(&source.bytes, &source.name)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            other => Err(format!(
                "--bench-warm supports PDF/image only, not {other:?}"
            )),
        }
    };
    once(&mut pipeline)?; // warm-up: load models, prime caches
    let mut total = 0.0f64;
    for _ in 0..runs {
        let t = std::time::Instant::now();
        once(&mut pipeline)?;
        total += t.elapsed().as_secs_f64();
    }
    Ok(total / runs as f64)
}

/// Serialize the chunk records `--to chunks` prints: the hierarchical chunker's
/// output always, plus the hybrid chunker's when a tokenizer is available —
/// `DOCLING_CHUNK_TOKENIZER`, or `models/chunk/tokenizer.json` as populated by
/// `scripts/install/download_dependencies.sh` (requires the `chunking` build
/// feature).
fn chunks_json(document: &docling::DoclingDocument) -> String {
    use docling::chunker::{contextualize, DocChunk, HierarchicalChunker};

    fn records(chunks: &[DocChunk]) -> serde_json::Value {
        serde_json::Value::Array(
            chunks
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "text": c.text,
                        "headings": c.headings,
                        "doc_items": c.doc_items.iter().map(|i| i.self_ref.clone()).collect::<Vec<_>>(),
                        "contextualize": contextualize(c),
                    })
                })
                .collect(),
        )
    }

    let hierarchical = HierarchicalChunker.chunk(document);
    #[cfg_attr(not(feature = "chunking"), allow(unused_mut))]
    let mut out = serde_json::json!({ "hierarchical": records(&hierarchical) });

    #[cfg(feature = "chunking")]
    if let Ok(tok_path) = std::env::var("DOCLING_CHUNK_TOKENIZER").or_else(|_| {
        docling::chunker::resolve_tokenizer_path(None).map_err(|_| std::env::VarError::NotPresent)
    }) {
        let max_tokens = std::env::var("DOCLING_CHUNK_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256);
        match docling::chunker::HuggingFaceTokenizer::from_file(&tok_path, max_tokens) {
            Ok(tok) => {
                let hybrid = docling::chunker::HybridChunker::new(tok).chunk(document);
                out["hybrid"] = records(&hybrid);
            }
            Err(e) => eprintln!("warning: {e}"),
        }
    }
    format!(
        "{}\n",
        serde_json::to_string_pretty(&out).expect("chunks are serializable")
    )
}
