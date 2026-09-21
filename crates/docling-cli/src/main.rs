//! Minimal CLI: convert a file and print Markdown or JSON to stdout.
//!
//! The docling.rs counterpart of `docling.cli.main`; `docling-rs serve`
//! (with `--features serve`) starts the HTTP conversion API.
//!
//! `--skip-ocr` (#244) keeps layout + TableFormer but never runs OCR
//! (docling's independent `do_ocr=False`); `--no-ocr` remains the
//! skip-everything fast path.
//!
//! `--help` prints the full flag list and `--version` the version plus the
//! optional features the binary carries (execution providers, `serve`,
//! chunking) — both answer without models present.
//!
//! Usage: docling-rs [--strict] [--page-break-placeholder TEXT] [--to md|json|dclx|chunks|images|latex] [--pages A-B] [--scale X] [--images MODE] [--input GLOB --output DIR [--jobs N]] [--fetch-images] [--list-attachments] [--skip-empty-cells] [--compact-tables] [--ebcdic-layout JSON|PATH] [--encoding LABEL] [--no-stream] [--no-table-former] [--no-ocr] [--skip-ocr] [--force-full-page-ocr] [--no-text-panels] [--heading-hierarchy] [--ocr-lang LANG] [--ocr-mode MODE] [--ocr-scale X] [--chunker hierarchical|hybrid] [--chunk-tokenizer PATH] [--chunk-max-tokens N] [--no-chunk-merge-peers] [--pipeline standard|vlm] [--vlm-endpoint URL] [--vlm-model NAME] [--vlm-api-key TOKEN] [--vlm-prompt TEXT] [--vlm-max-tokens N] [--asr-model PRESET] [--asr-lang CODE] [--video-frames N] [--use-web-browser] [--enrich-picture-classes] [--enrich-code] [--enrich-formula] <input-file>
//!   --input GLOB|DIR   batch mode (#205): convert every file the glob matches
//!                      (`--input '/data/reports/**/*.pdf'` — quote it so the
//!                      shell doesn't expand it) instead of one positional file.
//!                      A plain directory sweeps it recursively, taking every
//!                      file with a convertible extension. One warm process
//!                      converts them all: the PDF/image ML pipeline loads its
//!                      models once and is reused for every file, like
//!                      docling-serve's warm pipeline.
//!   --output DIR       where batch results land. The directory structure below
//!                      the pattern's static prefix is preserved (`a/b/x.pdf`
//!                      under `--input '/data/**/*.pdf'` becomes
//!                      `DIR/a/b/x.md`); extensions follow `--to` (`.md`,
//!                      `.json`, `.dclx`, `.chunks.json`). Also works with a
//!                      single positional input file. Output paths print to
//!                      stdout one per line; progress goes to stderr. A failed
//!                      file is reported and skipped (exit code 1 at the end).
//!   --jobs N           batch workers (default 1). Declarative formats convert
//!                      in parallel; PDF/image files share the one warm ML
//!                      pipeline (which parallelizes internally per document).
//!   --to md|json       output format (default: md). `json` emits docling-core's
//!                      native DoclingDocument JSON (export_to_dict); `images`
//!                      (#243) skips conversion and rasterizes a PDF's pages to
//!                      `<stem>_page_NNNN.png` files (combines with `--pages`).
//!   --scale X          `--to images` render scale in pixels per PDF point:
//!                      0.1-4.0, default 2.0 (144 dpi, the ML pipeline's own
//!                      render scale).
//!   --pages A-B        convert only PDF pages A through B (1-based, inclusive;
//!                      a single page number also works). Skipped pages are
//!                      never rasterized, so a small window over a huge PDF is
//!                      cheap. Non-PDF inputs ignore this.
//!   --images MODE      picture handling for Markdown (mirrors docling's
//!                      image_mode): placeholder (default) | embedded | referenced.
//!                      `referenced` writes image files under ./artifacts/ —
//!                      streamed to disk page by page, so image-heavy PDFs stay
//!                      memory-bounded. JSON always embeds extracted images as
//!                      data URIs.
//!   --fetch-images     for HTML/EPUB/MHTML/JATS, resolve external <img src> (data:
//!                      URIs, local files, http(s) URLs, EPUB/MHTML archive parts,
//!                      JATS <graphic> files) and embed the bytes. Off by default;
//!                      fetches over the network.
//!   --strict           cleaner, more conformant Markdown instead of byte-for-byte
//!                      docling-legacy output (Markdown only).
//!   --page-break-placeholder TEXT
//!                      insert TEXT between pages in the Markdown output
//!                      (docling's `export_to_markdown(page_break_placeholder=…)`,
//!                      e.g. "<!-- page break -->"). Pages come from the PDF /
//!                      image pipeline, slides, sheets and DjVu pages; a break
//!                      lands only between two rendered blocks on different
//!                      pages — never first or last, empty pages collapse. Off
//!                      by default: docling's Markdown carries no page breaks.
//!   --no-stream        build the whole document before printing Markdown instead
//!                      of streaming it page by page. Streaming is the default for
//!                      Markdown (placeholder/embedded images); JSON and referenced
//!                      images always use the buffered path.
//!   --no-table-former  skip loading/running the TableFormer table-structure
//!                      model for PDF/image input; tables fall back to simple
//!                      geometric reconstruction from cell positions. Faster
//!                      (no model load, no per-table inference) at the cost of
//!                      table fidelity — helps most in streaming mode.
//!   --video-frames N   Max frames sampled from a video input as timestamped
//!                      pictures (needs the ffmpeg binary; 0 = transcript
//!                      only). Default 8.
//!   --asr-model NAME   Whisper preset for audio inputs: whisper_tiny_en,
//!                      whisper_base_en, whisper_small_en, whisper_distil_small_en
//!                      (models under .models/asr/<preset>/; fetch them with
//!                      download_dependencies.sh --asr-model=<preset>)
//!   --asr-lang CODE    transcription language for audio/video input: a Whisper
//!                      code (en, de, zh, ...) or auto (the default) to detect
//!                      it from the first 30 seconds. English-only presets
//!                      always transcribe English.
//!   --encoding LABEL   decode text inputs (Markdown, CSV, AsciiDoc, WebVTT,
//!                      LaTeX, XML, …) with this character encoding — a WHATWG
//!                      label such as shift_jis, koi8-r, windows-1251 or
//!                      latin1 — instead of detecting one (BOM, UTF-8, then
//!                      windows-1252); bytes it cannot decode fail the
//!                      conversion (docling's TextBackendOptions.encoding)
//!   --force-full-page-ocr  OCR every PDF page even when it has a text layer
//!                      (docling's force_full_page_ocr) — for layers that lie:
//!                      broken encodings, forms with a few typed-in fields
//!   --no-text-panels   keep every detected picture as a picture — disable the
//!                      #157 demotion of uncaptioned text-panel pictures into
//!                      paragraphs (the escape hatch for image-extraction
//!                      workflows, #173)
//!   --heading-hierarchy  infer PDF/image section-header levels after assembly
//!                      (#302, docling's HeadingHierarchyModel): PDF bookmarks
//!                      are authoritative, legal/outline numbering covers the
//!                      rest, font style breaks the ties. Off by default —
//!                      headings then keep the flat level docling emits
//!   --no-ocr           skip layout detection, OCR, and TableFormer entirely for
//!                      PDF/image input — no model load or inference at all.
//!                      Emits the embedded text layer as flat paragraphs in
//!                      reading order (no headings/lists/tables/pictures). The
//!                      fastest option, but a scanned/image-only PDF (no
//!                      embedded text layer) yields no text — convert those
//!                      without this flag. Also works when pdfium/the models
//!                      aren't installed at all (e.g. a bare `cargo install`):
//!                      a digital PDF falls back to the pure-Rust text-layer
//!                      extraction.
//!   --use-web-browser  pre-render HTML/MHTML/EPUB in the system Chromium (driven
//!                      from Rust) so stylesheet-driven `display:none` elements
//!                      (e.g. a collapsed nav menu) are dropped before parsing.
//!                      Requires building with `--features web-browser`.
//!   --enrich-picture-classes
//!                      classify each detected picture (PDF/image input) with the
//!                      DocumentFigureClassifier model; the 26-class prediction
//!                      distribution lands in the JSON picture item (docling's
//!                      do_picture_classification). Needs
//!                      .models/picture_classifier.onnx.
//!   --enrich-code      rewrite detected code blocks (and detect their language)
//!                      with the CodeFormulaV2 VLM (docling's do_code_enrichment).
//!                      Needs .models/code_formula/. Slow on CPU: an autoregressive
//!                      generation per code block.
//!   --enrich-formula   decode display formulas to LaTeX with CodeFormulaV2
//!                      (docling's do_formula_enrichment); Markdown then renders
//!                      $$latex$$ instead of the formula placeholder comment.

use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use docling::chunks::{ChunkOptions, ChunkerKind};
use docling::{DocumentConverter, ImageMode, InputFormat, Pipeline, SourceDocument};

/// `--version` output: the crate version plus the optional features this
/// binary was actually built with. The feature list is the useful half — the
/// execution provider a build can select, whether `serve` is compiled in, and
/// whether tokenizer-backed chunking is available all follow from it, and all
/// three are routine support questions (#324 debugging started exactly here).
fn version_line() -> String {
    let mut features: Vec<&str> = Vec::new();
    if cfg!(feature = "chunking") {
        features.push("chunking");
    }
    if cfg!(feature = "serve") {
        features.push("serve");
    }
    if cfg!(feature = "heif") {
        features.push("heif");
    }
    if cfg!(feature = "web-browser") {
        features.push("web-browser");
    }
    if cfg!(feature = "cuda") {
        features.push("cuda");
    }
    if cfg!(feature = "tensorrt") {
        features.push("tensorrt");
    }
    if cfg!(feature = "directml") {
        features.push("directml");
    }
    if cfg!(feature = "coreml") {
        features.push("coreml");
    }
    if cfg!(feature = "xnnpack") {
        features.push("xnnpack");
    }
    let version = env!("CARGO_PKG_VERSION");
    if features.is_empty() {
        format!("docling-rs {version}")
    } else {
        format!("docling-rs {version} ({})", features.join(", "))
    }
}

/// One-line synopsis — the `usage:` prefix an argument error prints.
const USAGE: &str = "usage: docling-rs [OPTIONS] <input-file>\n       docling-rs --input GLOB|DIR --output DIR [OPTIONS]\n       docling-rs serve [SERVE OPTIONS]";

/// `--help`: the synopsis plus every flag, grouped. Kept in sync with the
/// module doc comment above, which carries the long-form rationale.
const HELP: &str = "\
Convert documents to Markdown, JSON, DocLang, LaTeX or chunks.

OUTPUT
  --to md|json|dclx|chunks|images|latex   output format (default: md)
  --strict                cleaner, more conformant Markdown (Markdown only)
  --page-break-placeholder TEXT   insert TEXT between pages (Markdown only, e.g. <!-- page break -->)
  --images MODE           picture handling: placeholder (default) | embedded | referenced
  --compact-tables        render Markdown tables without width padding
  --no-stream             build the whole document before printing

INPUT SELECTION
  --input GLOB|DIR        batch mode: convert everything the glob/directory matches
  --output DIR            where batch (or single-file) results are written
  --jobs N                batch workers (default 1)
  --pages A-B             convert only PDF pages A..B (1-based, inclusive)
  --scale X               `--to images` render scale, px per PDF point (0.1-4.0, default 2.0)

FORMAT OPTIONS
  --fetch-images          resolve external <img src> for HTML/EPUB/MHTML/JATS (network access)
  --list-attachments      append an Attachments section for .eml/.msg
  --skip-empty-cells      omit empty cells from XLSX/XLS grids
  --ebcdic-layout JSON|PATH   EBCDIC copybook layout
  --encoding LABEL        character encoding of text inputs (shift_jis, koi8-r,
                          windows-1251, …); default: detect (BOM, UTF-8, cp1252)
  --use-web-browser       pre-render HTML with a headless browser (feature `web-browser`)

PDF / IMAGE PIPELINE
  --no-table-former       skip the TableFormer model (geometric tables instead)
  --no-ocr                skip OCR entirely (text-layer only)
  --skip-ocr              keep layout + tables, never run OCR
  --force-full-page-ocr   OCR the whole page, discarding the text layer
  --no-text-panels        disable the text-panel heuristic
  --heading-hierarchy     infer heading levels from font weight/slant/case
  --ocr-lang LANG         OCR recognition model (default: en): en | ch, or a
                          BCP-47 tag for English/Chinese (en-US, eng, zh,
                          zh-Hans, zh-TW; docling's iso: prefix accepted)
  --ocr-mode MODE         auto (default) | full_page | layout_regions
  --ocr-scale X           OCR input scale in px per point
  --enrich-picture-classes | --enrich-code | --enrich-formula
                          optional enrichment models (off by default)

CHUNKING (`--to chunks`)
  --chunker hierarchical|hybrid
  --chunk-tokenizer PATH  tokenizer.json for the hybrid chunker
  --chunk-max-tokens N    chunk budget
  --no-chunk-merge-peers  keep sibling chunks separate

VLM PIPELINE
  --pipeline standard|vlm
  --vlm-endpoint URL | --vlm-model NAME | --vlm-api-key TOKEN
  --vlm-prompt TEXT | --vlm-max-tokens N

AUDIO / VIDEO
  --asr-model PRESET      Whisper preset for audio/video transcription
  --asr-lang CODE         force a transcription language
  --video-frames N        sample N key frames from a video

OTHER
  -h, --help              print this help
  -V, --version           print the version and compiled-in features

Environment knobs (execution providers, model paths, worker counts) are
documented in the README: https://github.com/docling-project/docling.rs";

fn main() -> ExitCode {
    // `--help` / `--version` before anything else: they must answer on a
    // binary whose models are missing, and a smoke test that runs
    // `docling-rs --version` should not be told to convert a file named
    // `--version` (issue-#333's CUDA image test tripped over exactly that).
    // `serve` keeps its own `--help`, so only scan the global position here.
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let is_serve = args.first().map(String::as_str) == Some("serve");
        if !is_serve {
            if args.iter().any(|a| a == "--version" || a == "-V") {
                println!("{}", version_line());
                return ExitCode::SUCCESS;
            }
            if args.iter().any(|a| a == "--help" || a == "-h") {
                println!("{}", version_line());
                println!();
                println!("{USAGE}");
                println!();
                println!("{HELP}");
                return ExitCode::SUCCESS;
            }
        }
    }

    // `docling-rs serve …` — the HTTP conversion API (issue-#78 analogue of
    // docling-serve). Compiled in only with `--features serve`; the flags
    // after `serve` are the `docling-serve` binary's (see that crate).
    {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some("serve") {
            // #263: a long-lived server defaults the ONNX CPU arena OFF — measured
            // here, a warm server's retained RSS drops ~3x (2.0 GB -> 0.7 GB after
            // large-PDF requests) at no measurable latency cost, and stops ratcheting
            // with every new page shape. Explicit DOCLING_RS_NO_ARENA=0 restores the
            // arena. Set before any session loads; the process is single-threaded
            // this early.
            if std::env::var_os("DOCLING_RS_NO_ARENA").is_none() {
                std::env::set_var("DOCLING_RS_NO_ARENA", "1");
            }

            return run_serve(args.collect());
        }
    }

    let mut strict = false;
    let mut to = "md".to_string();
    let mut images = "placeholder".to_string();
    let mut fetch_images = false;
    let mut list_attachments = false;
    let mut skip_empty_cells = false;
    let mut compact_tables = false;
    let mut page_break_placeholder: Option<String> = None;
    let mut ebcdic_layout: Option<String> = None;
    let mut no_stream = false;
    let mut no_table_former = false;
    let mut no_ocr = false;
    let mut skip_ocr = false;
    let mut force_full_page_ocr = false;
    let mut no_text_panels = false;
    let mut heading_hierarchy = false;
    let mut use_web_browser = false;
    let mut asr_model: Option<String> = None;
    let mut asr_lang: Option<String> = None;
    let mut encoding: Option<String> = None;
    let mut video_frames: Option<usize> = None;
    let mut enrich_picture_classes = false;
    let mut enrich_code = false;
    let mut enrich_formula = false;
    let mut bench_warm: Option<usize> = None;
    let mut pages: Option<(usize, usize)> = None;
    let mut scale: f32 = 2.0;
    let mut ocr_lang: Option<String> = None;
    let mut ocr_mode: Option<String> = None;
    let mut ocr_scale: Option<f32> = None;
    let mut chunk_opts = docling::chunks::ChunkOptions::default();
    let mut pipeline: Option<String> = None;
    let mut vlm_endpoint: Option<String> = None;
    let mut vlm_model: Option<String> = None;
    let mut vlm_api_key: Option<String> = None;
    let mut vlm_prompt: Option<String> = None;
    let mut vlm_max_tokens: Option<usize> = None;
    let mut path: Option<String> = None;
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut jobs: usize = 1;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => strict = true,
            "--fetch-images" => fetch_images = true,
            // #251: append an Attachments section to converted emails
            // (.eml/.msg) — names and content types only.
            "--list-attachments" => list_attachments = true,
            // Sparse-spreadsheet compaction (#271, docling.rs extensions):
            // omit empty cells from XLSX/XLS table grids, and/or render all
            // Markdown tables compact (no width padding).
            "--skip-empty-cells" => skip_empty_cells = true,
            "--compact-tables" => compact_tables = true,
            // docling's `page_break_placeholder`: the text that separates
            // pages in Markdown (an empty TEXT is allowed — upstream then
            // leaves a doubled blank line between pages).
            "--page-break-placeholder" => match args.next() {
                Some(v) => page_break_placeholder = Some(v),
                None => {
                    eprintln!("error: --page-break-placeholder needs the text to insert");
                    return ExitCode::from(2);
                }
            },
            // #252: EBCDIC copybook layout — inline JSON or a file path
            // (default: the <stem>.layout.json sidecar next to the source).
            "--ebcdic-layout" => match args.next() {
                Some(v) => ebcdic_layout = Some(v),
                None => {
                    eprintln!("error: --ebcdic-layout needs a JSON string or file path");
                    return ExitCode::from(2);
                }
            },
            "--no-stream" => no_stream = true,
            "--no-table-former" => no_table_former = true,
            "--no-ocr" => no_ocr = true,
            // #244: keep layout + TableFormer, never OCR (docling's
            // independent do_ocr=False) — unlike --no-ocr, which skips the
            // whole ML stack.
            "--skip-ocr" => skip_ocr = true,
            "--force-full-page-ocr" => force_full_page_ocr = true,
            "--no-text-panels" => no_text_panels = true,
            "--heading-hierarchy" => heading_hierarchy = true,
            "--use-web-browser" => use_web_browser = true,
            // Opt-in enrichment models (docling CLI flag names): picture
            // classification, code rewrite + language, formula LaTeX.
            "--enrich-picture-classes" => enrich_picture_classes = true,
            "--enrich-code" => enrich_code = true,
            "--enrich-formula" => enrich_formula = true,
            "--input" => match args.next() {
                Some(v) => input = Some(v),
                None => {
                    eprintln!("error: --input needs a glob pattern");
                    return ExitCode::from(2);
                }
            },
            "--output" => match args.next() {
                Some(v) => output = Some(v),
                None => {
                    eprintln!("error: --output needs a directory");
                    return ExitCode::from(2);
                }
            },
            "--jobs" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) if n >= 1 => jobs = n,
                _ => {
                    eprintln!("error: --jobs needs a positive integer");
                    return ExitCode::from(2);
                }
            },
            "--to" => to = args.next().unwrap_or_default(),
            // Named Whisper preset for audio inputs (English-only /
            // Distil-Whisper variants under .models/asr/<preset>/; fetch with
            // download_dependencies.sh --asr-model=<preset>).
            "--asr-model" => asr_model = args.next(),
            // Transcription language (or "auto"); validated against the model's
            // vocabulary at conversion time.
            "--asr-lang" => asr_lang = args.next(),
            // Character encoding of text inputs; validated when a text backend
            // first decodes the file.
            "--encoding" => match args.next() {
                Some(label) => encoding = Some(label),
                None => {
                    eprintln!("error: --encoding needs an encoding label (e.g. shift_jis)");
                    std::process::exit(2);
                }
            },
            // Max frames sampled from a video input (needs the ffmpeg binary;
            // 0 = transcript only). Default 8.
            "--video-frames" => video_frames = args.next().and_then(|v| v.parse().ok()),
            "--images" => images = args.next().unwrap_or_default(),
            // `--to images` render scale, pixels per PDF point (#243).
            "--scale" => match args.next().and_then(|v| v.parse::<f32>().ok()) {
                Some(v) if (0.1..=4.0).contains(&v) => scale = v,
                _ => {
                    eprintln!(
                        "error: --scale needs a number in 0.1-4.0 \
                         (pixels per PDF point; default 2.0 = 144 dpi)"
                    );
                    return ExitCode::from(2);
                }
            },
            // PDF page window, 1-based inclusive: `--pages 3-7` or `--pages 3`.
            "--pages" => match args.next().as_deref().map(docling::parse_page_range) {
                Some(Ok(range)) => pages = Some(range),
                Some(Err(e)) => {
                    eprintln!("error: --pages: {e}");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --pages needs a range like 1-10 (or a single page)");
                    return ExitCode::from(2);
                }
            },
            // OCR recognition language for scanned PDF/image pages: en
            // (default; proper Latin word spacing) | ch (the multilingual
            // docling-conformance model).
            "--ocr-lang" => match args.next() {
                Some(v) if docling::OcrLang::parse(&v).is_some() => ocr_lang = Some(v),
                Some(v) => {
                    eprintln!(
                        "error: --ocr-lang {v:?} names no language the OCR models read \
                         (accepted: {})",
                        docling::OcrLang::ACCEPTED
                    );
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --ocr-lang needs a value (en | ch | a BCP-47 tag)");
                    return ExitCode::from(2);
                }
            },
            // Which regions feed the OCR (docling's OcrMode, #254):
            // full_page/layout_regions discard the text layer like
            // --force-full-page-ocr; pdf_aware_layout_regions (= default) is
            // the standard text-layer-aware behavior.
            "--ocr-mode" => match args.next() {
                Some(v)
                    if matches!(
                        v.trim(),
                        "default" | "full_page" | "layout_regions" | "pdf_aware_layout_regions"
                    ) =>
                {
                    ocr_mode = Some(v)
                }
                Some(v) => {
                    eprintln!(
                        "error: --ocr-mode {v:?} is not \
                         default|full_page|layout_regions|pdf_aware_layout_regions"
                    );
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --ocr-mode needs a value");
                    return ExitCode::from(2);
                }
            },
            // OCR render scale in px per PDF point (docling's
            // OcrOptions.scale, #254): unset reads the pipeline's own 2.0
            // px/pt render; docling's default is 3 (216 dpi).
            "--ocr-scale" => match args.next().map(|v| v.trim().parse::<f32>()) {
                Some(Ok(s)) if s > 0.0 && s.is_finite() => ocr_scale = Some(s),
                Some(_) => {
                    eprintln!("error: --ocr-scale needs a positive number");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --ocr-scale needs a value");
                    return ExitCode::from(2);
                }
            },
            // Per-run `--to chunks` configuration (#256, mirrors the serve
            // fields / docling's service-datamodel `HybridChunkerOptions`);
            // the DOCLING_CHUNK_* env knobs stay the defaults.
            "--chunker" => match args.next().as_deref().map(ChunkerKind::parse) {
                Some(Ok(k)) => chunk_opts.chunker = Some(k),
                Some(Err(e)) => {
                    eprintln!("error: --chunker: {e}");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --chunker needs a value (hierarchical|hybrid)");
                    return ExitCode::from(2);
                }
            },
            "--chunk-tokenizer" => match args.next() {
                Some(v) => chunk_opts.tokenizer = Some(v),
                None => {
                    eprintln!("error: --chunk-tokenizer needs a tokenizer.json path");
                    return ExitCode::from(2);
                }
            },
            "--chunk-max-tokens" => match args.next().map(|v| v.trim().parse::<usize>()) {
                Some(Ok(n)) if n > 0 => chunk_opts.max_tokens = Some(n),
                _ => {
                    eprintln!("error: --chunk-max-tokens needs a positive integer");
                    return ExitCode::from(2);
                }
            },
            "--no-chunk-merge-peers" => chunk_opts.merge_peers = Some(false),
            // Pipeline selection (#77): `standard` (default, the ML stack) or
            // `vlm` — render pages and convert them through a remote
            // OpenAI-compatible vision endpoint returning DocLang.
            "--pipeline" => match args.next() {
                Some(v) if matches!(v.trim(), "standard" | "vlm") => pipeline = Some(v),
                Some(v) => {
                    eprintln!("error: --pipeline {v:?} is not standard|vlm");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --pipeline needs a value (standard|vlm)");
                    return ExitCode::from(2);
                }
            },
            "--vlm-endpoint" => vlm_endpoint = args.next(),
            "--vlm-model" => vlm_model = args.next(),
            // #312: the remaining VlmOptions knobs, for parity with the Node/
            // Python bindings and serve (previously env-only, or — for
            // max_tokens — not settable at all). Inert without --pipeline vlm,
            // like every other --vlm-* flag.
            "--vlm-api-key" => vlm_api_key = args.next(),
            "--vlm-prompt" => vlm_prompt = args.next(),
            "--vlm-max-tokens" => match args.next().map(|v| v.trim().parse::<usize>()) {
                // 0 would have every page come back empty and surface as a
                // model error — reject it here, like the other bindings do.
                Some(Ok(n)) if n > 0 => vlm_max_tokens = Some(n),
                _ => {
                    eprintln!("error: --vlm-max-tokens needs a positive integer");
                    return ExitCode::from(2);
                }
            },
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
            _ if arg.starts_with("--") => {
                eprintln!("error: unknown flag '{arg}'");
                eprintln!("run `docling-rs --help` for the full flag list");
                return ExitCode::from(2);
            }
            _ => path = Some(arg),
        }
    }

    if !matches!(
        to.as_str(),
        "md" | "markdown" | "json" | "dclx" | "chunks" | "images" | "latex"
    ) {
        eprintln!("error: unknown --to '{to}' (expected: md, json, dclx, chunks, images, latex)");
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

    // Batch mode (#205): `--input <glob>` fans one warm process over many
    // files, writing results under `--output` and preserving the directory
    // structure below the pattern's static prefix. A positional input file
    // with `--output` routes through the same writer (a batch of one).
    if input.is_some() || output.is_some() {
        if bench_warm.is_some() {
            eprintln!("error: --bench-warm is a single-file mode; drop --input/--output");
            return ExitCode::from(2);
        }
        let Some(outdir) = output else {
            eprintln!("error: --input needs --output DIR for the converted files");
            return ExitCode::from(2);
        };
        let (files, base) = if let Some(pattern) = &input {
            if path.is_some() {
                eprintln!("error: --input and a positional input file are mutually exclusive");
                return ExitCode::from(2);
            }
            match expand_glob(pattern) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(2);
                }
            }
        } else {
            let Some(p) = path else {
                eprintln!("error: --output needs --input GLOB or an input file");
                return ExitCode::from(2);
            };
            let file = std::path::PathBuf::from(&p);
            let base = file.parent().map(Path::to_path_buf).unwrap_or_default();
            (vec![file], base)
        };
        let vlm = if pipeline.as_deref() == Some("vlm") {
            match resolve_vlm_flags(
                vlm_endpoint,
                vlm_model,
                vlm_api_key,
                vlm_prompt,
                vlm_max_tokens,
                pages,
            ) {
                Ok(o) => Some(o),
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(2);
                }
            }
        } else {
            None
        };
        let cfg = BatchCfg {
            to,
            image_mode,
            strict,
            fetch_images,
            list_attachments,
            skip_empty_cells,
            compact_tables,
            page_break_placeholder,
            ebcdic_layout,
            no_table_former,
            no_ocr,
            skip_ocr,
            force_full_page_ocr,
            no_text_panels,
            heading_hierarchy,
            use_web_browser,
            enrich_picture_classes,
            enrich_code,
            enrich_formula,
            asr_model,
            asr_lang,
            encoding,
            video_frames,
            pages,
            ocr_lang,
            ocr_mode,
            ocr_scale,
            scale,
            chunk: chunk_opts.clone(),
            vlm,
        };
        return run_batch(files, &base, Path::new(&outdir), jobs, &cfg);
    }

    let Some(path) = path else {
        eprintln!("error: no input file");
        eprintln!("{USAGE}");
        eprintln!("run `docling-rs --help` for the full flag list");
        return ExitCode::from(2);
    };

    let source = match SourceDocument::from_file(&path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let is_pdf = source.format == InputFormat::Pdf;

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

    // `--to images` (#243): rasterization is pdfium-only — no conversion, no
    // models, no pipeline (a `--pipeline vlm` selection has nothing to do and
    // is ignored). Files land in the CWD like `--to dclx`'s archive.
    if to == "images" {
        if !is_pdf {
            eprintln!("error: --to images rasterizes PDF inputs only ('{path}' is not a PDF)");
            return ExitCode::from(2);
        }
        let stem = Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        return match write_page_images(&source.bytes, pages, scale, Path::new(""), &stem) {
            Ok(written) => {
                // Humans read stderr; stdout stays the bare paths for scripts
                // (the dclx convention).
                eprintln!("images: {} page(s) written", written.len());
                for p in &written {
                    println!("{}", p.display());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // #77: the remote-VLM pipeline replaces the whole ML stack — convert,
    // then fall through to the regular output selection (md/json/dclx/chunks
    // all work; there is no page-streaming, the endpoint is the bottleneck).
    if pipeline.as_deref() == Some("vlm") {
        let opts = match resolve_vlm_flags(
            vlm_endpoint,
            vlm_model,
            vlm_api_key,
            vlm_prompt,
            vlm_max_tokens,
            pages,
        ) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        };
        let mut document = match docling::vlm::convert_vlm(&source, &opts) {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        document.strict_markdown = strict;
        document.page_break_placeholder = page_break_placeholder.clone();
        return output_document(document, &to, image_mode, &path, &chunk_opts);
    }

    let mut converter = DocumentConverter::new()
        .strict(strict)
        .asr_model(asr_model.clone())
        .asr_lang(asr_lang.clone())
        .encoding(encoding.clone())
        .fetch_images(fetch_images)
        .list_attachments(list_attachments)
        .skip_empty_cells(skip_empty_cells)
        .compact_tables(compact_tables)
        .page_break_placeholder(page_break_placeholder.clone())
        .ebcdic_layout_opt(ebcdic_layout.clone())
        .no_table_former(no_table_former)
        .skip_ocr(skip_ocr)
        .no_ocr(no_ocr)
        .force_full_page_ocr(force_full_page_ocr)
        .no_text_panels(no_text_panels)
        .heading_hierarchy(heading_hierarchy)
        .use_web_browser(use_web_browser)
        .do_picture_classification(enrich_picture_classes)
        .do_code_enrichment(enrich_code)
        .do_formula_enrichment(enrich_formula);
    if let Some(max) = video_frames {
        converter = converter.video_frames(max);
    }
    if let Some((first, last)) = pages {
        converter = converter.page_range(first, last);
    }
    if let Some(lang) = &ocr_lang {
        converter = converter.ocr_lang(lang.clone());
    }
    if let Some(mode) = &ocr_mode {
        converter = converter.ocr_mode(mode.clone());
    }
    if let Some(s) = ocr_scale {
        converter = converter.ocr_scale(s);
    }

    // Stream Markdown by default: print each chunk as the converter produces it
    // (page by page for PDF). Referenced images stream too (#80): each page's
    // files land under ./artifacts/ as that page is printed, so image bytes
    // never accumulate. JSON needs the whole tree, so it keeps the buffered
    // path. `--no-stream` opts back into buffering.
    let is_markdown = matches!(to.as_str(), "md" | "markdown");
    if is_markdown && !no_stream {
        let stream = match converter.convert_streaming_images(source, image_mode) {
            Ok(s) => s,
            Err(e) => {
                if let Some(mut doc) =
                    pdf_no_ocr_fallback(&e.to_string(), is_pdf, no_ocr, strict, &path, pages)
                {
                    doc.page_break_placeholder = page_break_placeholder.clone();
                    return output_document(doc, &to, image_mode, &path, &chunk_opts);
                }
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        let mut wrote_any = false;
        for chunk in stream {
            match chunk {
                Ok(s) => {
                    if let Err(e) = out.write_all(s.as_bytes()) {
                        eprintln!("error: writing output: {e}");
                        return ExitCode::FAILURE;
                    }
                    wrote_any = wrote_any || !s.is_empty();
                }
                Err(e) => {
                    let _ = out.flush();
                    // The ML pipeline binds pdfium lazily, so the missing-assets
                    // error can surface here — but only fall back while nothing
                    // has been printed, to never emit a document twice.
                    if !wrote_any {
                        if let Some(mut doc) = pdf_no_ocr_fallback(
                            &e.to_string(),
                            is_pdf,
                            no_ocr,
                            strict,
                            &path,
                            pages,
                        ) {
                            doc.page_break_placeholder = page_break_placeholder.clone();
                            return output_document(doc, &to, image_mode, &path, &chunk_opts);
                        }
                    }
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if let Err(e) = out.flush() {
            eprintln!("error: writing output: {e}");
            return ExitCode::FAILURE;
        }
        if image_mode == ImageMode::Referenced {
            eprintln!("referenced images (if any) written to ./artifacts/ as pages completed");
        }
        return ExitCode::SUCCESS;
    }

    let document = match converter.convert(source) {
        Ok(result) => result.document,
        Err(e) => {
            if let Some(mut doc) =
                pdf_no_ocr_fallback(&e.to_string(), is_pdf, no_ocr, strict, &path, pages)
            {
                doc.page_break_placeholder = page_break_placeholder.clone();
                return output_document(doc, &to, image_mode, &path, &chunk_opts);
            }
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    output_document(document, &to, image_mode, &path, &chunk_opts)
}

/// Launch-blocker fallback: a bare `cargo install docling-cli` ships neither
/// pdfium nor the ONNX models, so the first PDF a new user tries dies at
/// pipeline startup. Under `--no-ocr` the pure-Rust text-layer path needs no
/// runtime assets at all — when the failure is exactly "assets missing"
/// (matched on the markers docling-pdf's enriched errors carry), convert the
/// embedded text layer instead of failing. Any other error, or a run without
/// `--no-ocr`, returns `None` and the (actionable) error prints as usual.
fn pdf_no_ocr_fallback(
    err: &str,
    is_pdf: bool,
    no_ocr: bool,
    strict: bool,
    path: &str,
    pages: Option<(usize, usize)>,
) -> Option<docling::DoclingDocument> {
    let assets_missing =
        err.contains("pdfium library is not installed") || err.contains("model not found at");
    if !is_pdf || !no_ocr || !assets_missing {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let name = Path::new(path).file_name()?.to_string_lossy().into_owned();
    match docling::pdf_text_layer_pages(&bytes, &name, pages) {
        Ok(mut doc) if !doc.nodes.is_empty() => {
            eprintln!(
                "warning: pdfium/models unavailable — --no-ocr extracted the embedded text \
                 layer only (run scripts/install/download_dependencies.sh for the full pipeline)"
            );
            doc.strict_markdown = strict;
            Some(doc)
        }
        // A scanned PDF has no text layer; the original error explains the
        // missing assets better than an empty document would.
        _ => None,
    }
}

/// Resolve `--pipeline vlm`'s options from the flags (#77, #312): endpoint
/// and model fall back to `DOCLING_RS_VLM_ENDPOINT` / `DOCLING_RS_VLM_MODEL`,
/// and `--vlm-api-key` / `--vlm-prompt` / `--vlm-max-tokens` override their
/// env-or-default values when given (max_tokens is validated > 0 at parse).
/// Blank values count as unset, matching the env helpers and the other
/// bindings; `--pages` composes exactly as with the ML pipeline.
fn resolve_vlm_flags(
    endpoint: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    prompt: Option<String>,
    max_tokens: Option<usize>,
    page_range: Option<(usize, usize)>,
) -> Result<docling::vlm::VlmOptions, String> {
    let set = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    let mut o =
        docling::vlm::VlmOptions::resolve(set(endpoint), set(model)).map_err(|e| e.to_string())?;
    if let Some(k) = set(api_key) {
        o.api_key = Some(k);
    }
    if let Some(p) = set(prompt) {
        o.prompt = Some(p);
    }
    if let Some(n) = max_tokens {
        o.max_tokens = n;
    }
    o.page_range = page_range;
    Ok(o)
}

/// The buffered output tail shared by the standard (non-streaming) and VLM
/// paths: `--to` selection, image sidecars, exit code.
/// The CLI flags a batch run freezes for every file (#205).
struct BatchCfg {
    to: String,
    image_mode: ImageMode,
    strict: bool,
    fetch_images: bool,
    list_attachments: bool,
    /// Omit empty cells from sparse spreadsheet grids (#271).
    skip_empty_cells: bool,
    /// Compact (unpadded) Markdown tables (#271).
    compact_tables: bool,
    /// docling's `page_break_placeholder`: text between pages in Markdown.
    page_break_placeholder: Option<String>,
    ebcdic_layout: Option<String>,
    no_table_former: bool,
    no_ocr: bool,
    skip_ocr: bool,
    force_full_page_ocr: bool,
    no_text_panels: bool,
    heading_hierarchy: bool,
    use_web_browser: bool,
    enrich_picture_classes: bool,
    enrich_code: bool,
    enrich_formula: bool,
    asr_model: Option<String>,
    asr_lang: Option<String>,
    encoding: Option<String>,
    video_frames: Option<usize>,
    pages: Option<(usize, usize)>,
    ocr_lang: Option<String>,
    /// Which regions feed the OCR (docling's `OcrMode`, #254).
    ocr_mode: Option<String>,
    /// OCR render scale in px/pt (docling's `OcrOptions.scale`, #254).
    ocr_scale: Option<f32>,
    /// `--to images` render scale (pixels per PDF point, #243).
    scale: f32,
    /// Per-run `--to chunks` configuration (#256).
    chunk: ChunkOptions,
    vlm: Option<docling::vlm::VlmOptions>,
}

/// Expand an `--input` glob into (matched files, static base directory). The
/// base — every path component before the first one containing a glob
/// metacharacter — is what output paths are made relative to, so
/// `--input '/data/reports/**/*.pdf'` mirrors the tree under `/data/reports`
/// into `--output`.
fn expand_glob(pattern: &str) -> Result<(Vec<std::path::PathBuf>, std::path::PathBuf), String> {
    // A plain directory is the most natural thing to hand a flag named
    // `--input`: sweep it recursively, keeping only files whose extension maps
    // to a known input format (a stray `.log`/`.DS_Store` must not fail the
    // batch). A glob stays verbatim — the user chose the files explicitly.
    let dir = Path::new(pattern);
    if dir.is_dir() {
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let entries =
                std::fs::read_dir(&d).map_err(|e| format!("--input '{}': {e}", d.display()))?;
            for entry in entries {
                let p = entry.map_err(|e| format!("--input: {e}"))?.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| docling::InputFormat::from_extension(e).is_some())
                {
                    files.push(p);
                }
            }
        }
        if files.is_empty() {
            return Err(format!(
                "--input '{pattern}' contains no files with a convertible extension"
            ));
        }
        files.sort();
        return Ok((files, dir.to_path_buf()));
    }
    let base = glob_base(pattern);
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for entry in glob::glob(pattern).map_err(|e| format!("--input: {e}"))? {
        match entry {
            Ok(p) if p.is_file() => files.push(p),
            Ok(_) => {} // directories the pattern happens to match
            Err(e) => eprintln!("warning: {e}"),
        }
    }
    if files.is_empty() {
        return Err(format!("--input '{pattern}' matches no files"));
    }
    files.sort();
    Ok((files, base))
}

/// The static prefix of a glob pattern: every path component before the first
/// one containing a metacharacter. A metachar-free pattern is a literal file
/// path, whose base is its parent directory.
fn glob_base(pattern: &str) -> std::path::PathBuf {
    let mut base = std::path::PathBuf::new();
    for comp in Path::new(pattern).components() {
        let text = comp.as_os_str().to_string_lossy();
        if text.contains(['*', '?', '[']) {
            break;
        }
        base.push(comp);
    }
    if base == Path::new(pattern) {
        base.pop();
    }
    base
}

/// Where a converted file lands: `--output` + the input's path relative to the
/// glob base, with the extension swapped per `--to`.
fn batch_out_path(file: &Path, base: &Path, output: &Path, to: &str) -> std::path::PathBuf {
    let rel = file
        .strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| Path::new(file.file_name().unwrap_or_default()).to_path_buf());
    let ext = match to {
        "json" => "json",
        "dclx" => "dclx",
        "chunks" => "chunks.json",
        "latex" => "tex",
        "images" => "png", // a stem carrier: pages land as `<stem>_page_NNNN.png`
        _ => "md",
    };
    output.join(rel).with_extension(ext)
}

/// Mirror of the single-file converter construction for batch workers.
fn batch_converter(cfg: &BatchCfg) -> DocumentConverter {
    let mut converter = DocumentConverter::new()
        .strict(cfg.strict)
        .asr_model(cfg.asr_model.clone())
        .asr_lang(cfg.asr_lang.clone())
        .encoding(cfg.encoding.clone())
        .fetch_images(cfg.fetch_images)
        .list_attachments(cfg.list_attachments)
        .skip_empty_cells(cfg.skip_empty_cells)
        .compact_tables(cfg.compact_tables)
        .page_break_placeholder(cfg.page_break_placeholder.clone())
        .ebcdic_layout_opt(cfg.ebcdic_layout.clone())
        .no_table_former(cfg.no_table_former)
        .no_ocr(cfg.no_ocr)
        .force_full_page_ocr(cfg.force_full_page_ocr)
        .no_text_panels(cfg.no_text_panels)
        .heading_hierarchy(cfg.heading_hierarchy)
        .use_web_browser(cfg.use_web_browser)
        .do_picture_classification(cfg.enrich_picture_classes)
        .do_code_enrichment(cfg.enrich_code)
        .do_formula_enrichment(cfg.enrich_formula);
    if let Some(max) = cfg.video_frames {
        converter = converter.video_frames(max);
    }
    if let Some((first, last)) = cfg.pages {
        converter = converter.page_range(first, last);
    }
    if let Some(lang) = &cfg.ocr_lang {
        converter = converter.ocr_lang(lang.clone());
    }
    if let Some(mode) = &cfg.ocr_mode {
        converter = converter.ocr_mode(mode.clone());
    }
    if let Some(s) = cfg.ocr_scale {
        converter = converter.ocr_scale(s);
    }
    converter
}

/// The lazily-built warm PDF/image pipeline shared by every batch worker —
/// models load once and every subsequent PDF/image reuses the sessions, the
/// way docling-serve's warm pipeline does. Flags are frozen for the run, so
/// unlike serve there is nothing to rebuild per file.
fn batch_pipeline<'a>(
    slot: &'a mut Option<Pipeline>,
    cfg: &BatchCfg,
) -> Result<&'a mut Pipeline, String> {
    if slot.is_none() {
        let mut p = Pipeline::new()
            .map_err(|e| e.to_string())?
            .no_table_former(cfg.no_table_former)
            .no_ocr(cfg.no_ocr)
            .skip_ocr(cfg.skip_ocr)
            .force_full_page_ocr(cfg.force_full_page_ocr)
            .no_text_panels(cfg.no_text_panels)
            .heading_hierarchy(docling::HeadingHierarchyOptions::enabled(
                cfg.heading_hierarchy,
            ))
            .ocr_mode(cfg.ocr_mode.as_deref().and_then(docling::OcrMode::parse))
            .ocr_scale(cfg.ocr_scale)
            .enrichments(docling::EnrichmentOptions {
                picture_classification: cfg.enrich_picture_classes,
                code: cfg.enrich_code,
                formula: cfg.enrich_formula,
            });
        p.set_pages(cfg.pages);
        p.set_ocr_lang(cfg.ocr_lang.as_deref().and_then(docling::OcrLang::parse));
        // Dot-progress on stderr: one dot per 10 finished pages, newline when
        // the document completes (only if any dots were printed).
        p.set_progress(Some(std::sync::Arc::new(|done: usize, total: usize| {
            use std::io::Write;
            if done.is_multiple_of(10) {
                eprint!(".");
                let _ = std::io::stderr().flush();
            }
            if done == total && total >= 10 {
                eprintln!();
            }
        })));
        *slot = Some(p);
    }
    Ok(slot.as_mut().expect("just filled"))
}

/// `--to images` (#243): rasterize a PDF to per-page PNGs,
/// `<dir>/<stem>_page_NNNN.png` — absolute 1-based page numbers, so a
/// `--pages` window keeps the source document's numbering. Returns the
/// written paths in page order.
fn write_page_images(
    bytes: &[u8],
    pages: Option<(usize, usize)>,
    scale: f32,
    dir: &Path,
    stem: &str,
) -> Result<Vec<std::path::PathBuf>, String> {
    let rendered =
        docling::render_pdf_pages(bytes, None, pages, scale).map_err(|e| e.to_string())?;
    let mut written = Vec::with_capacity(rendered.len());
    for page in &rendered {
        let out = dir.join(format!("{stem}_page_{:04}.png", page.page_no));
        std::fs::write(&out, &page.png).map_err(|e| format!("writing {}: {e}", out.display()))?;
        written.push(out);
    }
    Ok(written)
}

/// Convert one batch file and write its output; returns the output path.
fn batch_convert_one(
    file: &Path,
    base: &Path,
    output: &Path,
    cfg: &BatchCfg,
    converter: &DocumentConverter,
    pipe: &std::sync::Mutex<Option<Pipeline>>,
) -> Result<(std::path::PathBuf, f64, Option<usize>), String> {
    let source = SourceDocument::from_file(file).map_err(|e| e.to_string())?;
    // Announce the document up front — with its page count for PDFs, so long
    // conversions are attributable while the dots tick.
    let pages = (source.format == InputFormat::Pdf)
        .then(|| docling::pdf_page_count(&source.bytes, None).ok())
        .flatten()
        .map(|n| match cfg.pages {
            // A --pages window converts only its slice of the document.
            Some((first, last)) => (last.min(n) + 1).saturating_sub(first).min(n),
            None => n,
        });
    match pages {
        Some(1) => eprintln!("start: {} (1 page)", file.display()),
        Some(n) => eprintln!("start: {} ({n} pages)", file.display()),
        None => eprintln!("start: {}", file.display()),
    }
    let started = std::time::Instant::now();
    if cfg.to == "images" {
        // #243: rasterize instead of converting — PDF-only, like the serve
        // endpoint. A non-PDF file fails its item, not the batch.
        if source.format != InputFormat::Pdf {
            return Err(format!(
                "--to images rasterizes PDF inputs only ({} is not a PDF)",
                file.display()
            ));
        }
        let out = batch_out_path(file, base, output, &cfg.to);
        let dir = out.parent().unwrap_or(Path::new("")).to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let stem = out
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        // pdfium is not thread-safe: the shared pipeline mutex is this
        // process's "who owns pdfium" lock, held here even though no models
        // run — a render must not race a concurrent PDF conversion.
        let _pdfium_owner = pipe.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let written = write_page_images(&source.bytes, cfg.pages, cfg.scale, &dir, &stem)?;
        let shown = written.first().cloned().unwrap_or(out);
        return Ok((shown, started.elapsed().as_secs_f64(), pages));
    }
    let mut document = if let Some(vlm) = &cfg.vlm {
        docling::vlm::convert_vlm(&source, vlm).map_err(|e| e.to_string())?
    } else if matches!(source.format, InputFormat::Pdf | InputFormat::Image) {
        // One warm pipeline for the whole run: workers serialize on it (its
        // internal page workers already use the machine), declarative files
        // keep converting in parallel around it.
        let mut guard = pipe.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let p = batch_pipeline(&mut guard, cfg)?;
        match source.format {
            InputFormat::Pdf => p.convert(&source.bytes, None, &source.name),
            _ => p.convert_image(&source.bytes, &source.name),
        }
        .map_err(|e| e.to_string())?
    } else {
        converter
            .convert(source)
            .map_err(|e| e.to_string())?
            .document
    };
    document.strict_markdown = cfg.strict;
    document.page_break_placeholder = cfg.page_break_placeholder.clone();

    let out = batch_out_path(file, base, output, &cfg.to);
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    match cfg.to.as_str() {
        "json" => std::fs::write(&out, document.export_to_json())
            .map_err(|e| format!("writing {}: {e}", out.display()))?,
        "chunks" => std::fs::write(&out, chunks_json(&document, &cfg.chunk)?)
            .map_err(|e| format!("writing {}: {e}", out.display()))?,
        // #317: the upstream CLI writes the serializer's text verbatim.
        "latex" => std::fs::write(&out, document.export_to_latex())
            .map_err(|e| format!("writing {}: {e}", out.display()))?,
        "dclx" => docling::dclx::save_as_dclx(&document, &out).map_err(|e| e.to_string())?,
        _ => {
            if cfg.image_mode == ImageMode::Placeholder {
                std::fs::write(&out, document.export_to_markdown())
                    .map_err(|e| format!("writing {}: {e}", out.display()))?;
            } else {
                // `referenced` images land next to the output file, in a
                // per-document `<stem>_artifacts/` dir the links point into.
                let stem = out
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "document".into());
                let art = format!("{stem}_artifacts");
                let (md, artifacts) = document.export_to_markdown_with_images(cfg.image_mode, &art);
                let parent = out.parent().unwrap_or(Path::new(""));
                for (rel, bytes) in &artifacts {
                    let target = parent.join(rel);
                    if let Some(dir) = target.parent() {
                        std::fs::create_dir_all(dir)
                            .map_err(|e| format!("creating {}: {e}", dir.display()))?;
                    }
                    std::fs::write(&target, bytes)
                        .map_err(|e| format!("writing {}: {e}", target.display()))?;
                }
                std::fs::write(&out, md).map_err(|e| format!("writing {}: {e}", out.display()))?;
            }
        }
    }
    Ok((out, started.elapsed().as_secs_f64(), pages))
}

/// Convert every matched file, `--jobs` workers wide. Output paths print to
/// stdout (one per line, for scripts); progress and errors go to stderr. A
/// failed file is reported and skipped — the batch keeps going, and the exit
/// code is non-zero if anything failed.
fn run_batch(
    files: Vec<std::path::PathBuf>,
    base: &Path,
    output: &Path,
    jobs: usize,
    cfg: &BatchCfg,
) -> ExitCode {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let succeeded = AtomicUsize::new(0);
    // Fail fast on a broken execution provider: an explicit DOCLING_RS_EP
    // whose runtime libraries are missing fails *every* PDF/image identically
    // — the first such error aborts the rest of the batch instead of
    // repeating itself per file.
    let abort = AtomicBool::new(false);
    let pipe: std::sync::Mutex<Option<Pipeline>> = std::sync::Mutex::new(None);
    let workers = jobs.min(files.len()).max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                // Converter construction is cheap configuration; one per
                // worker keeps the loop borrow-free.
                let converter = batch_converter(cfg);
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(i) else { break };
                    // A backend that panics on one file must not take the
                    // batch down with it (#395/#396): the documented contract
                    // here is "a failed file is reported and skipped". The
                    // panic still prints its own message and backtrace from
                    // the unwind; this only decides what happens next. The
                    // shared pipeline slot already recovers from a lock
                    // poisoned by such a panic, and the per-worker converter
                    // is plain configuration, so the next file starts clean.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        batch_convert_one(file, base, output, cfg, &converter, &pipe)
                    }))
                    .unwrap_or_else(|_| {
                        Err("the conversion panicked (its message and backtrace are above)".into())
                    });
                    match outcome {
                        Ok((out, secs, pages)) => {
                            match pages {
                                Some(n) if n > 0 => eprintln!(
                                    "ok: {} -> {} ({secs:.1}s, {:.0} ms/page)",
                                    file.display(),
                                    out.display(),
                                    secs * 1000.0 / n as f64
                                ),
                                _ => eprintln!(
                                    "ok: {} -> {} ({secs:.1}s)",
                                    file.display(),
                                    out.display()
                                ),
                            }
                            println!("{}", out.display());
                            succeeded.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            eprintln!("error: {}: {e}", file.display());
                            if e.contains("execution provider") {
                                abort.store(true, Ordering::Relaxed);
                                eprintln!(
                                    "fatal: the requested execution provider is \
                                     unavailable — aborting the batch (fix the \
                                     DOCLING_RS_EP runtime libraries or unset it)"
                                );
                            }
                            // Missing pdfium/models fails every PDF/image the
                            // same way — one report is enough (the error above
                            // already says how to install the assets).
                            if e.contains("pdfium library is not installed")
                                || e.contains("model not found at")
                            {
                                abort.store(true, Ordering::Relaxed);
                                eprintln!(
                                    "fatal: the PDF runtime assets are missing — \
                                     aborting the batch (every PDF/image would \
                                     fail identically)"
                                );
                            }
                        }
                    }
                }
            });
        }
    });
    let nf = failed.load(Ordering::Relaxed);
    let ok = succeeded.load(Ordering::Relaxed);
    let skipped = files.len() - ok - nf;
    if skipped > 0 {
        eprintln!("batch: {ok} converted, {nf} failed, {skipped} skipped");
    } else {
        eprintln!("batch: {ok} converted, {nf} failed");
    }
    if nf > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn output_document(
    document: docling::DoclingDocument,
    to: &str,
    image_mode: ImageMode,
    path: &str,
    chunk: &ChunkOptions,
) -> ExitCode {
    if to == "json" {
        println!("{}", document.export_to_json());
        return ExitCode::SUCCESS;
    }

    // #317: docling 2.124's `--to latex`. The serializer's text carries no
    // trailing newline (upstream writes it verbatim to `<stem>.tex`); stdout
    // gets one so a shell prompt doesn't land on `\end{document}`.
    if to == "latex" {
        println!("{}", document.export_to_latex());
        return ExitCode::SUCCESS;
    }

    if to == "chunks" {
        // Chunking conformance/debug dump: a JSON object with the hierarchical
        // chunk records and, when a tokenizer is configured, the hybrid ones.
        // `DOCLING_CHUNK_TOKENIZER` points at a HuggingFace tokenizer.json
        // (`DOCLING_CHUNK_MAX_TOKENS` overrides the default budget of 256);
        // `--chunker`/`--chunk-*` override per run (#256).
        match chunks_json(&document, chunk) {
            Ok(json) => print!("{json}"),
            Err(e) => {
                eprintln!("error: chunks: {e}");
                return ExitCode::FAILURE;
            }
        }
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
        // Humans read stderr ("where did my file go?"); stdout stays the bare
        // path for scripts.
        eprintln!("dclx: archive written to {}", out.display());
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

/// `docling-rs serve …`: parse the serve flags and run the HTTP server.
#[cfg(feature = "serve")]
fn run_serve(args: Vec<String>) -> ExitCode {
    use docling_serve::ServeConfig;
    let mut cfg = ServeConfig::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--addr" => match it.next() {
                Some(v) => cfg.addr = v,
                None => return serve_usage("--addr needs HOST:PORT"),
            },
            "--concurrency" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) if v >= 1 => cfg.concurrency = v,
                _ => return serve_usage("--concurrency needs a positive integer"),
            },
            "--max-body-mb" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v >= 1 => cfg.max_body_bytes = v * 1024 * 1024,
                _ => return serve_usage("--max-body-mb needs a positive integer"),
            },
            "--queue-size" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) if v >= 1 => cfg.queue_size = v,
                _ => return serve_usage("--queue-size needs a positive integer"),
            },
            "--result-ttl" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) if v >= 1 => cfg.result_ttl_secs = v,
                _ => return serve_usage("--result-ttl needs a positive number of seconds"),
            },
            // #263: memory ceiling for admission control. 0 disables; unset =
            // auto-detect the container's cgroup limit.
            "--max-memory-mb" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => cfg.max_memory_mb = Some(v),
                None => return serve_usage("--max-memory-mb needs a number (0 disables)"),
            },
            "--warmup" => cfg.warmup = true,
            "--allow-url-fetch" => cfg.allow_url_fetch = true,
            "--no-url-fetch" => cfg.allow_url_fetch = false,
            "--strict" => cfg.strict = true,
            other => return serve_usage(&format!("unknown argument '{other}'")),
        }
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(docling_serve::serve(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "serve")]
fn serve_usage(err: &str) -> ExitCode {
    eprintln!("error: {err}");
    eprintln!("usage: docling-rs serve [--addr HOST:PORT] [--concurrency N] [--max-body-mb N] [--queue-size N] [--result-ttl SECS] [--warmup] [--allow-url-fetch] [--strict]");
    ExitCode::from(2)
}

/// Without the `serve` feature the subcommand explains how to get it.
#[cfg(not(feature = "serve"))]
fn run_serve(_args: Vec<String>) -> ExitCode {
    eprintln!(
        "error: this binary was built without the HTTP server.\n\
         Rebuild with `cargo build -p docling-cli --features serve`, or use the\n\
         standalone server: `cargo run -p docling-serve --release -- --help`."
    );
    ExitCode::from(2)
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

/// Serialize the chunk records `--to chunks` prints (see
/// [`docling::chunks::chunk_records_with`] for the tokenizer resolution
/// rules). Errors only when an explicitly requested configuration can't be
/// honored (`--chunker hybrid` without a usable tokenizer).
fn chunks_json(
    document: &docling::DoclingDocument,
    chunk: &ChunkOptions,
) -> Result<String, String> {
    let mut warn = |msg: String| eprintln!("warning: {msg}");
    let out = docling::chunks::chunk_records_with(document, chunk, &mut warn)?;
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(&out).expect("chunks are serializable")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_base_stops_at_the_first_metachar_component() {
        assert_eq!(
            glob_base("/data/reports/**/*.pdf"),
            Path::new("/data/reports")
        );
        assert_eq!(glob_base("docs/*.md"), Path::new("docs"));
        assert_eq!(glob_base("*.md"), Path::new(""));
        assert_eq!(glob_base("a/b[12]/c/*.pdf"), Path::new("a"));
        // A literal file path (no metachars) bases at its parent, so a batch of
        // one lands directly under --output.
        assert_eq!(glob_base("dir/file.pdf"), Path::new("dir"));
    }

    #[test]
    fn batch_out_path_mirrors_structure_and_swaps_extension() {
        let out = |file: &str, to: &str| {
            batch_out_path(
                Path::new(file),
                Path::new("/data/reports"),
                Path::new("/out"),
                to,
            )
        };
        assert_eq!(
            out("/data/reports/a/b/x.pdf", "md"),
            Path::new("/out/a/b/x.md")
        );
        assert_eq!(out("/data/reports/x.pdf", "json"), Path::new("/out/x.json"));
        assert_eq!(out("/data/reports/x.pdf", "dclx"), Path::new("/out/x.dclx"));
        assert_eq!(out("/data/reports/x.pdf", "latex"), Path::new("/out/x.tex"));
        assert_eq!(
            out("/data/reports/a/x.pdf", "chunks"),
            Path::new("/out/a/x.chunks.json")
        );
        // A file outside the base still lands under --output by file name.
        assert_eq!(out("/elsewhere/y.docx", "md"), Path::new("/out/y.md"));
    }
}
