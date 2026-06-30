//! Minimal CLI: convert a file and print Markdown or JSON to stdout.
//!
//! A stand-in for `docling.cli.main`; the full Typer-style CLI (batch mode,
//! pipeline options) is a later phase.
//!
//! Usage: fleischwolf [--strict] [--to md|json] [--images MODE] [--fetch-images] <input-file>
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

use std::path::Path;
use std::process::ExitCode;

use fleischwolf::{DocumentConverter, ImageMode, InputFormat, Pipeline, SourceDocument};

fn main() -> ExitCode {
    let mut strict = false;
    let mut to = "md".to_string();
    let mut images = "placeholder".to_string();
    let mut fetch_images = false;
    let mut bench_warm: Option<usize> = None;
    let mut path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => strict = true,
            "--fetch-images" => fetch_images = true,
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

    if !matches!(to.as_str(), "md" | "markdown" | "json") {
        eprintln!("error: unknown --to '{to}' (expected: md, json)");
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
        eprintln!("usage: fleischwolf [--strict] [--to md|json] [--images MODE] [--fetch-images] <input-file>");
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
        return match bench_warm_conversion(&source, runs) {
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

    let document = match DocumentConverter::new()
        .strict(strict)
        .fetch_images(fetch_images)
        .convert(source)
    {
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
fn bench_warm_conversion(source: &SourceDocument, runs: usize) -> Result<f64, String> {
    let mut pipeline = Pipeline::new().map_err(|e| e.to_string())?;
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
