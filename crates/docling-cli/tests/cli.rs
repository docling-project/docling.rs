//! CLI surface tests: the flags a script or a container smoke test reaches for
//! before any document exists. They run the real binary, so they also pin that
//! `--help`/`--version` answer with **no models and no arguments** — the case
//! that broke the CUDA image smoke test in issue #333.

use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .args(args)
        .output()
        .expect("run docling-rs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `--version` / `-V`: exit 0, the crate version on stdout, nothing on stderr.
#[test]
fn version_flag_reports_the_crate_version() {
    for flag in ["--version", "-V"] {
        let (code, stdout, stderr) = run(&[flag]);
        assert_eq!(code, 0, "{flag}: stderr: {stderr}");
        assert!(
            stdout.starts_with(&format!("docling-rs {}", env!("CARGO_PKG_VERSION"))),
            "{flag}: {stdout:?}"
        );
        assert!(stderr.is_empty(), "{flag}: stderr: {stderr:?}");
    }
}

/// The version line names the optional features the binary carries, so a bug
/// report says which execution providers were even compiled in.
#[test]
fn version_lists_compiled_features() {
    let (_, stdout, _) = run(&["--version"]);
    #[cfg(feature = "chunking")]
    assert!(stdout.contains("chunking"), "{stdout:?}");
    #[cfg(feature = "mcp")]
    assert!(stdout.contains("mcp"), "{stdout:?}");
    #[cfg(not(any(feature = "chunking", feature = "mcp")))]
    assert!(!stdout.contains('('), "{stdout:?}");
}

/// `--list-input-formats` (#603, Pandoc's flag): one extension per line,
/// sorted and unique, exit 0, nothing on stderr — and every listed extension
/// is one the converter actually routes (plus the CLI's own `zip` batches).
#[test]
fn list_input_formats_prints_sorted_extensions() {
    let (code, stdout, stderr) = run(&["--list-input-formats"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    let lines: Vec<&str> = stdout.lines().collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(lines, sorted, "sorted and unique");
    // The default build carries the PDF and ASR pipelines.
    for ext in ["docx", "pdf", "rtf", "png", "mp3", "mp4", "pages", "zip"] {
        assert!(lines.contains(&ext), "{ext} missing: {stdout}");
    }
    for ext in &lines {
        assert!(
            *ext == "zip" || docling::InputFormat::from_extension(ext).is_some(),
            "{ext} listed but not routed"
        );
        assert!(!ext.starts_with('.') && *ext == ext.to_ascii_lowercase());
    }
    assert_eq!(lines.contains(&"heic"), cfg!(feature = "heif"));
    // The rest of the command line is ignored, as Pandoc does.
    let (code, again, _) = run(&["--to", "json", "--list-input-formats", "missing.pdf"]);
    assert_eq!(code, 0);
    assert_eq!(again, stdout);
}

/// `--list-output-formats` (#603): the `--to` values, sorted, one per line —
/// each of which `--to` accepts.
#[test]
fn list_output_formats_prints_the_to_values() {
    let (code, stdout, stderr) = run(&["--list-output-formats"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr:?}");
    assert_eq!(
        stdout,
        "chunks\ndclx\nhtml\nimages\njson\nlatex\nmd\npandoc\ntext\nvtt\n"
    );
}

/// `--help` / `-h`: exit 0, the flag list on stdout (not stderr — it is the
/// requested output, not a diagnostic).
#[test]
fn help_flag_prints_the_flag_list() {
    for flag in ["--help", "-h"] {
        let (code, stdout, stderr) = run(&[flag]);
        assert_eq!(code, 0, "{flag}: stderr: {stderr}");
        assert!(stdout.contains("usage: docling-rs"), "{flag}: {stdout:?}");
        for expected in ["--to md|json", "--pages A-B", "--pipeline standard|vlm"] {
            assert!(stdout.contains(expected), "{flag}: missing {expected}");
        }
        assert!(stderr.is_empty(), "{flag}: stderr: {stderr:?}");
    }
}

/// No arguments is a usage error, not a panic or a silent success.
#[test]
fn no_arguments_is_a_usage_error() {
    let (code, stdout, stderr) = run(&[]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(stderr.contains("no input file"), "{stderr:?}");
    assert!(stderr.contains("--help"), "{stderr:?}");
}

/// An unknown flag names itself and points at `--help`.
#[test]
fn unknown_flag_points_at_help() {
    let (code, _, stderr) = run(&["--no-such-flag"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--no-such-flag"), "{stderr:?}");
    assert!(stderr.contains("--help"), "{stderr:?}");
}

/// `--help` after other flags still prints help rather than treating the flag
/// as a file name — a smoke test may append it to a canned argument list.
#[test]
fn help_is_recognized_in_any_position() {
    let (code, stdout, _) = run(&["--strict", "--to", "json", "--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("usage: docling-rs"), "{stdout:?}");
}

/// `--page-break-placeholder TEXT` (docling's `page_break_placeholder`):
/// TEXT lands between two pages' blocks and nowhere else. The DjVu fixture
/// converts on every build — no models, no pdfium — and streams through the
/// default Markdown path, so this also pins the streamer's output.
#[test]
fn page_break_placeholder_separates_pages() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling/tests/data/djvu/sources/example.djvu"
    );
    // The flag reaches the serializer: three pages → two breaks. Where the
    // breaks go is the library's (`crates/docling/tests/page_breaks.rs`).
    let (code, with, stderr) = run(&["--page-break-placeholder", "<!-- page break -->", fixture]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(with.matches("<!-- page break -->").count(), 2, "{with}");
    // The default streamed output and `--no-stream` (the buffered
    // serializer) agree byte for byte.
    let (_, buffered, _) = run(&[
        "--page-break-placeholder",
        "<!-- page break -->",
        "--no-stream",
        fixture,
    ]);
    assert_eq!(buffered, with);
}

/// The flag needs its text — a bare flag is a usage error, like the other
/// value-taking flags.
#[test]
fn page_break_placeholder_requires_a_value() {
    let (code, _, stderr) = run(&["--page-break-placeholder"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--page-break-placeholder"), "{stderr}");
}

/// `--video-frames` rejects a missing or non-numeric value with a usage error
/// instead of silently falling back to the default (used to apply 8 frames).
#[test]
fn video_frames_requires_a_number() {
    for args in [
        &["--video-frames"][..],
        &["--video-frames", "1O", "x.mp4"][..],
    ] {
        let (code, _, stderr) = run(args);
        assert_eq!(code, 2, "args {args:?}, stderr: {stderr}");
        assert!(stderr.contains("--video-frames"), "{stderr}");
    }
}

/// `--xbrl-taxonomy DIR` (#466) needs its directory, and with it an XBRL
/// instance's JSON carries the fact graph with the presentation hierarchy the
/// taxonomy's linkbases give it (`to_child` links), where the instance alone
/// yields only the facts' own `to_value` links.
#[test]
fn xbrl_taxonomy_flag_feeds_the_fact_graph() {
    let (code, _, stderr) = run(&["--xbrl-taxonomy"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--xbrl-taxonomy"), "{stderr}");

    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/xbrl/sources/mlac-20251231.xml"
    );
    let taxonomy = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/xbrl/sources/mlac-taxonomy"
    );
    let (code, json, stderr) = run(&["--to", "json", "--xbrl-taxonomy", taxonomy, fixture]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        json.contains("\"self_ref\": \"#/key_value_items/0\""),
        "{}",
        &json[..200]
    );
    assert!(
        json.contains("\"label\": \"to_child\""),
        "no hierarchy links"
    );
    assert!(
        json.contains("\"text\": \"weight: 1.0\""),
        "no calculation weights"
    );
    // The instance's own directory holds no schema: the facts and their
    // concepts, but none of the linkbases' ancestors or weights.
    let (code, json, stderr) = run(&["--to", "json", fixture]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(json.contains("\"label\": \"to_value\""));
    assert!(!json.contains("\"text\": \"weight: 1.0\""));
    assert!(
        !json.contains("CoverAbstract"),
        "presentation ancestor without a taxonomy"
    );
}

/// #460: `--ocr-engine` takes ppocr | tesseract, and `--ocr-lang` is checked
/// against the engine whichever order the flags come in — `deu` is a
/// Tesseract language, not a PP-OCR model.
#[test]
fn ocr_engine_and_lang_validate_together() {
    let (code, _, stderr) = run(&["--ocr-engine", "easyocr", "x.pdf"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--ocr-engine"), "{stderr}");

    let (code, _, stderr) = run(&["--ocr-engine", "ppocr", "--ocr-lang", "deu", "x.pdf"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--ocr-lang"), "{stderr}");

    // Accepted under Tesseract in either flag order: the run then fails on
    // the missing input, not on the option.
    for args in [
        &[
            "--ocr-engine",
            "tesseract",
            "--ocr-lang",
            "deu+fra",
            "missing.pdf",
        ][..],
        &[
            "--ocr-lang",
            "iso:de",
            "--ocr-engine",
            "tesseract",
            "missing.pdf",
        ][..],
    ] {
        let (_, _, stderr) = run(args);
        assert!(!stderr.contains("--ocr-lang"), "args {args:?}: {stderr}");
        assert!(!stderr.contains("--ocr-engine"), "args {args:?}: {stderr}");
    }
}

/// A scratch directory under the system temp dir, unique per test, removed on
/// drop so a failed assertion doesn't leave outputs behind for the next run.
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "docling-cli-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
    fn path(&self, rel: &str) -> String {
        self.0.join(rel).to_string_lossy().into_owned()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const MD_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/md/sources");

/// Several positional sources are one batch (#489, Python's
/// `docling convert a.md b.md --output out/`): every file converts in the one
/// process, lands in `--output` by stem, its path prints on stdout, and the
/// batch summary counts them.
#[test]
fn several_positional_sources_convert_into_the_output_dir() {
    let out = Scratch::new("multi");
    let a = format!("{MD_FIXTURES}/duck.md");
    let b = format!("{MD_FIXTURES}/blocks.md");
    let (code, stdout, stderr) = run(&[&a, &b, "--output", &out.path("")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for stem in ["duck", "blocks"] {
        let written = out.0.join(format!("{stem}.md"));
        assert!(
            written.is_file(),
            "{} missing; stderr: {stderr}",
            written.display()
        );
        assert!(stdout.contains(&format!("{stem}.md")), "stdout: {stdout}");
    }
    assert!(
        stderr.contains("batch: 2 converted, 0 failed"),
        "stderr: {stderr}"
    );
    // The single-file contract is untouched: one positional source without
    // `--output` still prints the Markdown itself.
    let (code, stdout, _) = run(&[&a]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout,
        std::fs::read_to_string(out.0.join("duck.md")).unwrap()
    );
}

/// A positional directory sweeps its tree like `--input DIR`, and mixes with
/// plain files in the same batch; more than one source without `--output` is
/// a usage error (stdout can hold only one document).
#[test]
fn positional_directories_and_files_mix_and_need_an_output_dir() {
    let src = Scratch::new("tree");
    std::fs::create_dir_all(src.0.join("deep/er")).unwrap();
    std::fs::write(src.0.join("deep/er/x.md"), "# x\n").unwrap();
    std::fs::write(
        src.0.join("deep/blob.bin"),
        "ignored: not a convertible extension\n",
    )
    .unwrap();
    let out = Scratch::new("tree-out");
    let single = format!("{MD_FIXTURES}/duck.md");
    let (code, stdout, stderr) = run(&[&src.path("deep"), &single, "--output", &out.path("")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    // The directory's structure below it is kept; the file lands by stem.
    assert!(
        out.0.join("er/x.md").is_file(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        out.0.join("duck.md").is_file(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("batch: 2 converted, 0 failed"),
        "stderr: {stderr}"
    );

    let (code, _, stderr) = run(&[&single, &src.path("deep/er/x.md")]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("needs --output DIR"), "stderr: {stderr}");

    let (code, _, stderr) = run(&[
        &src.path("deep/missing.md"),
        &single,
        "--output",
        &out.path(""),
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("no such file or directory"),
        "stderr: {stderr}"
    );
}

/// Two files with the same stem from different directories would overwrite
/// each other in `--output`; the batch refuses up front (Python overwrites
/// silently) and names both files and the remedy.
#[test]
fn same_stem_sources_are_refused_before_converting() {
    let src = Scratch::new("collide");
    std::fs::create_dir_all(src.0.join("a")).unwrap();
    std::fs::create_dir_all(src.0.join("b")).unwrap();
    std::fs::write(src.0.join("a/report.md"), "# a\n").unwrap();
    std::fs::write(src.0.join("b/report.md"), "# b\n").unwrap();
    let out = Scratch::new("collide-out");
    let (code, stdout, stderr) = run(&[
        &src.path("a/report.md"),
        &src.path("b/report.md"),
        "--output",
        &out.path(""),
    ]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("would both be written to"),
        "stderr: {stderr}"
    );
    assert!(stdout.is_empty(), "nothing converted: {stdout}");
    assert!(!out.0.join("report.md").exists());
    // The remedy: the common parent directory, whose tree is kept.
    let (code, _, stderr) = run(&[&src.path(""), "--output", &out.path("")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.0.join("a/report.md").is_file(), "stderr: {stderr}");
    assert!(out.0.join("b/report.md").is_file(), "stderr: {stderr}");
}

/// `--abort-on-error` (Python's flag): the first failed file ends the batch
/// and the rest are reported as skipped; without it the failure is reported,
/// the batch continues, and the exit code is 1.
#[test]
fn abort_on_error_stops_at_the_first_failure() {
    let src = Scratch::new("abort");
    // Unknown extension and binary content (text would read as plain text
    // since #671): `SourceDocument::from_file` rejects it at convert time,
    // which is a per-file failure rather than an expansion error.
    std::fs::write(
        src.0.join("bad.xyz"),
        b"\x00\x01\x02\xff\xfe not a document\x00",
    )
    .unwrap();
    std::fs::write(src.0.join("good.md"), "# good\n").unwrap();
    let out = Scratch::new("abort-out");
    let (code, _, stderr) = run(&[
        &src.path("bad.xyz"),
        &src.path("good.md"),
        "--output",
        &out.path(""),
        "--abort-on-error",
    ]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(
        stderr.contains("aborting the batch (--abort-on-error)"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("batch: 0 converted, 1 failed, 1 skipped"),
        "stderr: {stderr}"
    );
    assert!(!out.0.join("good.md").exists(), "stderr: {stderr}");

    let (code, _, stderr) = run(&[
        &src.path("bad.xyz"),
        &src.path("good.md"),
        "--output",
        &out.path(""),
    ]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(
        stderr.contains("batch: 1 converted, 1 failed"),
        "stderr: {stderr}"
    );
    assert!(out.0.join("good.md").is_file(), "stderr: {stderr}");
}

/// `--to` is repeatable (#491, Python's `docling convert --to md --to json`):
/// one conversion, every format written under `--output`, each path on
/// stdout; the Markdown is byte-identical to the single-format stdout run,
/// so existing `--to md` callers see no change.
#[test]
fn repeated_to_writes_every_format_from_one_conversion() {
    let out = Scratch::new("to-multi");
    let src = format!("{MD_FIXTURES}/duck.md");
    let (code, stdout, stderr) = run(&[
        "--to",
        "md",
        "--to",
        "json",
        "--output",
        &out.path(""),
        &src,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.0.join("duck.md").is_file(), "stderr: {stderr}");
    assert!(out.0.join("duck.json").is_file(), "stderr: {stderr}");
    assert!(
        stdout.contains("duck.md") && stdout.contains("duck.json"),
        "stdout: {stdout}"
    );
    assert_eq!(stdout.lines().count(), 2, "one path per format: {stdout}");
    assert!(
        stderr.contains("batch: 1 converted, 0 failed"),
        "stderr: {stderr}"
    );
    let (code, single, _) = run(&["--to", "md", &src]);
    assert_eq!(code, 0);
    assert_eq!(
        single,
        std::fs::read_to_string(out.0.join("duck.md")).unwrap()
    );
    let (code, single_json, _) = run(&["--to", "json", &src]);
    assert_eq!(code, 0);
    assert_eq!(
        single_json.trim_end(),
        std::fs::read_to_string(out.0.join("duck.json"))
            .unwrap()
            .trim_end()
    );
}

/// `--to text` (#613): docling's plain text — on stdout for one file (a
/// newline appended, as for LaTeX), `<stem>.txt` verbatim in batch mode.
#[test]
fn to_text_prints_plain_text_and_writes_txt() {
    let dir = Scratch::new("to-text");
    let src = dir.path("note.md");
    std::fs::write(
        &src,
        "# Title\n\nSome **bold** and *italic* text.\n\n- one\n- two\n",
    )
    .unwrap();
    let (code, stdout, stderr) = run(&["--to", "text", &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        stdout,
        "Title\n\nSome bold and italic text.\n\n- one\n- two\n"
    );

    let (code, stdout, stderr) = run(&["--to", "text", "--output", &dir.path("out"), &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let written = dir.0.join("out").join("note.txt");
    assert_eq!(stdout.trim(), written.to_string_lossy());
    assert_eq!(
        std::fs::read_to_string(&written).unwrap(),
        "Title\n\nSome bold and italic text.\n\n- one\n- two"
    );
}

/// #614: `--to vtt` — docling's WebVTT export; a `.vtt` input's cues come
/// back (voice end tag of a lone voice span omitted, like `save_as_vtt`).
#[test]
fn to_vtt_prints_cues_and_writes_vtt() {
    let dir = Scratch::new("to-vtt");
    let src = dir.path("talk.vtt");
    std::fs::write(
        &src,
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\n<v Ann>Hello</v>\n",
    )
    .unwrap();
    let want = "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\n<v Ann>Hello";
    let (code, stdout, stderr) = run(&["--to", "vtt", &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, format!("{want}\n"));
    let (code, _, stderr) = run(&["--to", "vtt", "--output", &dir.path("out"), &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.0.join("out/talk.vtt")).unwrap(),
        want
    );
}

/// Several sources × several formats in one run; the comma form
/// (`--to md,json`) and `markdown` spell the same list, and a format named
/// twice is written once.
#[test]
fn repeated_to_covers_every_source_and_dedupes() {
    let out = Scratch::new("to-matrix");
    let a = format!("{MD_FIXTURES}/duck.md");
    let b = format!("{MD_FIXTURES}/blocks.md");
    let (code, stdout, stderr) = run(&[
        "--to",
        "markdown,json",
        "--to",
        "latex",
        "--to",
        "md",
        "--output",
        &out.path(""),
        &a,
        &b,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for stem in ["duck", "blocks"] {
        for ext in ["md", "json", "tex"] {
            assert!(
                out.0.join(format!("{stem}.{ext}")).is_file(),
                "{stem}.{ext}; stderr: {stderr}"
            );
        }
    }
    assert_eq!(
        stdout.lines().count(),
        6,
        "2 documents × 3 formats, md once: {stdout}"
    );
    assert!(
        stderr.contains("batch: 2 converted, 0 failed"),
        "stderr: {stderr}"
    );
}

/// Several formats without `--output` is a usage error (stdout carries one
/// document), and an unknown entry anywhere in the list is rejected.
#[test]
fn repeated_to_needs_an_output_dir_and_validates_each_entry() {
    let src = format!("{MD_FIXTURES}/duck.md");
    let (code, _, stderr) = run(&["--to", "md", "--to", "json", &src]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("several --to formats need --output DIR"),
        "stderr: {stderr}"
    );
    let (code, _, stderr) = run(&["--to", "md,pdf", &src]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("unknown --to 'pdf'"), "stderr: {stderr}");
    let (code, _, stderr) = run(&["--to"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--to needs a format"), "stderr: {stderr}");
}

/// A failed document fails all of its formats at once, and `--abort-on-error`
/// still stops the batch before the next document writes anything.
#[test]
fn repeated_to_respects_abort_on_error() {
    let src = Scratch::new("to-abort");
    std::fs::write(
        src.0.join("bad.xyz"),
        b"\x00\x01\x02\xff\xfe not a document\x00",
    )
    .unwrap();
    std::fs::write(src.0.join("good.md"), "# good\n").unwrap();
    let out = Scratch::new("to-abort-out");
    let (code, stdout, stderr) = run(&[
        "--to",
        "md",
        "--to",
        "json",
        "--abort-on-error",
        "--output",
        &out.path(""),
        &src.path("bad.xyz"),
        &src.path("good.md"),
    ]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(
        stderr.contains("batch: 0 converted, 1 failed, 1 skipped"),
        "stderr: {stderr}"
    );
    assert!(!out.0.join("good.md").exists() && !out.0.join("good.json").exists());
    let (code, stdout, stderr) = run(&[
        "--to",
        "md",
        "--to",
        "json",
        "--output",
        &out.path(""),
        &src.path("bad.xyz"),
        &src.path("good.md"),
    ]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(stdout.lines().count(), 2, "stdout: {stdout}");
    assert!(out.0.join("good.md").is_file() && out.0.join("good.json").is_file());
}

/// `--to html` (#492): a complete HTML document on stdout for one file, and
/// `<stem>.html` per document in batch mode.
#[test]
fn to_html_writes_a_complete_document() {
    let src = format!("{MD_FIXTURES}/duck.md");
    let (code, stdout, stderr) = run(&["--to", "html", &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.starts_with("<!DOCTYPE html>\n<html>\n<head>\n"),
        "{stdout:.80}"
    );
    assert!(stdout.contains("<title>duck</title>"), "{stdout:.400}");
    assert!(stdout.contains("<div class='page'>"), "{stdout:.400}");
    assert!(stdout.trim_end().ends_with("</html>"), "{stdout:.80}");
    let out = Scratch::new("html");
    let (code, paths, stderr) = run(&["--to", "html", "--output", &out.path(""), &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(paths.trim_end().ends_with("duck.html"), "{paths}");
    let written = std::fs::read_to_string(out.0.join("duck.html")).unwrap();
    assert_eq!(written.trim_end(), stdout.trim_end());
}

/// `--to pandoc` (#515): Pandoc's JSON AST on stdout, `<stem>.pandoc.json`
/// in batch mode; `--pandoc-api-version` other than 1.23 is a usage error
/// named before anything converts.
#[test]
fn to_pandoc_writes_the_ast_and_checks_the_api_version() {
    let src = format!("{MD_FIXTURES}/duck.md");
    let (code, stdout, stderr) = run(&["--to", "pandoc", &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.starts_with("{\"pandoc-api-version\":[1,23,1,1],\"meta\":{},\"blocks\":["),
        "{stdout}"
    );

    let out = Scratch::new("to-pandoc");
    let (code, _, stderr) = run(&["--to", "pandoc", "--output", &out.path(""), &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let written = std::fs::read_to_string(out.0.join("duck.pandoc.json")).unwrap();
    assert_eq!(written.trim_end(), stdout.trim_end());

    let (code, _, stderr) = run(&["--to", "pandoc", "--pandoc-api-version", "1.23", &src]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (code, stdout, stderr) = run(&["--to", "pandoc", "--pandoc-api-version", "1.22", &src]);
    assert_eq!(code, 2, "stdout: {stdout}");
    assert!(
        stderr.contains("unsupported Pandoc API version '1.22'"),
        "stderr: {stderr}"
    );
}

/// `--images-scale` (#520) takes the same 0.1-4.0 window as `--scale`; a
/// value outside it is a usage error before anything converts.
#[test]
fn images_scale_out_of_range_is_a_usage_error() {
    for bad in ["0", "9", "abc"] {
        let (code, _, err) = run(&["--images-scale", bad, "x.pdf"]);
        assert_eq!(code, 2, "--images-scale {bad}");
        assert!(err.contains("--images-scale"), "{err}");
    }
}

/// A `.zip` source expands into the documents it holds (#557): each lands
/// under `out/<archive>/<entry path>`, what does not convert is reported and
/// left out, nothing escapes `--output`, and a lone archive without
/// `--output` is one merged document on stdout (#677) — or, under
/// `--no-archive-merge`, a usage error (it is many documents). A directory
/// sweep still takes only convertible extensions — archives in it stay
/// untouched.
#[test]
fn zip_sources_convert_each_document_inside() {
    let src = Scratch::new("zip");
    let zip = docling::dclx::zip_bytes(vec![
        ("a.md", b"# A\n\nalpha\n".as_slice()),
        ("sub/b.md", b"# B\n".as_slice()),
        ("tool.exe", b"MZ".as_slice()),
        ("../evil.md", b"# evil\n".as_slice()),
    ]);
    std::fs::write(src.0.join("bundle.zip"), &zip).unwrap();
    let out = Scratch::new("zip-out");
    let plain = format!("{MD_FIXTURES}/duck.md");
    let (code, stdout, stderr) = run(&[&src.path("bundle.zip"), &plain, "--output", &out.path("")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for rel in ["bundle/a.md", "bundle/sub/b.md", "duck.md"] {
        assert!(out.0.join(rel).is_file(), "{rel} missing; stderr: {stderr}");
        // The CLI prints the output path as the host spells it.
        let native = rel.replace('/', std::path::MAIN_SEPARATOR_STR);
        assert!(stdout.contains(&native), "stdout: {stdout}");
    }
    assert!(!out.0.join("bundle/tool.md").exists());
    assert!(!out.0.join("evil.md").exists() && !src.0.join("evil.md").exists());
    assert!(
        stderr.contains("tool.exe: unsupported file type"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("../evil.md: unsafe path"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("batch: 3 converted, 0 failed, 2 archive entries not converted"),
        "stderr: {stderr}"
    );

    // #677: a lone archive on stdout is one document, its skips on stderr.
    let (code, stdout, stderr) = run(&[&src.path("bundle.zip")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "## a.md\n\n# A\n\nalpha\n\n## sub/b.md\n\n# B\n");
    assert!(
        stderr.contains("skip: ") && stderr.contains("tool.exe: unsupported file type"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("../evil.md: unsafe path"),
        "stderr: {stderr}"
    );
    let (code, _, stderr) = run(&[&src.path("bundle.zip"), "--no-archive-merge"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("needs --output DIR"), "stderr: {stderr}");

    // A directory sweep keeps to convertible extensions: the archive in it
    // is not expanded (no new outputs for an existing `--input DIR` run).
    let swept = Scratch::new("zip-sweep-out");
    std::fs::write(src.0.join("c.md"), "# C\n").unwrap();
    let (code, _, stderr) = run(&[&src.path(""), "--output", &swept.path("")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(swept.0.join("c.md").is_file());
    assert!(!swept.0.join("bundle").exists(), "stderr: {stderr}");
}

/// `--archive-merge` (#677) under `--output DIR` and `--output-file`: the
/// archive is one item, `DIR/<archive>.<ext>` — every `--to` format — with
/// no per-entry tree; a failed entry is reported and left out, an archive
/// holding nothing convertible fails.
#[test]
fn archive_merge_is_one_output_per_archive() {
    let src = Scratch::new("zip-merge");
    let zip = docling::dclx::zip_bytes(vec![
        ("a.md", b"# A\n\nalpha\n".as_slice()),
        ("broken.docx", b"not a zip at all".as_slice()),
        ("sub/b.md", b"# B\n".as_slice()),
    ]);
    std::fs::write(src.0.join("bundle.zip"), &zip).unwrap();
    let out = Scratch::new("zip-merge-out");
    let (code, stdout, stderr) = run(&[
        &src.path("bundle.zip"),
        "--archive-merge",
        "--output",
        &out.path(""),
        "--to",
        "md,json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("bundle.md") && stdout.contains("bundle.json"),
        "stdout: {stdout}"
    );
    assert!(!out.0.join("bundle").exists());
    let md = std::fs::read_to_string(out.0.join("bundle.md")).unwrap();
    assert_eq!(md, "## a.md\n\n# A\n\nalpha\n\n## sub/b.md\n\n# B\n");
    let json = std::fs::read_to_string(out.0.join("bundle.json")).unwrap();
    assert!(
        json.contains("\"section_header\"") && json.contains("\"sub/b.md\""),
        "{json}"
    );
    assert!(
        stderr.contains("error: ") && stderr.contains("bundle.zip:broken.docx"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("batch: 1 converted, 0 failed"),
        "stderr: {stderr}"
    );

    // `--output-file` merges by default, as stdout does.
    let target = out.path("one/bundle.markdown");
    let (code, stdout, stderr) = run(&[&src.path("bundle.zip"), "--output-file", &target]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), md);

    // Nothing convertible: an error, not an empty document.
    let empty = docling::dclx::zip_bytes(vec![("tool.exe", b"MZ".as_slice())]);
    std::fs::write(src.0.join("empty.zip"), &empty).unwrap();
    let (code, stdout, stderr) = run(&[&src.path("empty.zip")]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(
        stderr.contains("holds no document to convert"),
        "stderr: {stderr}"
    );
}

/// #677's acceptance: MarkItDown's `test_files.zip` on stdout is one Markdown
/// with a `## <entry>` section per entry — `test.docx`'s holding the AutoGen
/// paper — in archive order; a nested ZIP is read under
/// `DOCLING_RS_ZIP_MAX_DEPTH`, its entries as `inner.zip/<path>`.
#[test]
fn markitdown_zip_is_one_markdown_on_stdout() {
    let zip = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling-py/tests/data/markitdown/test_files.zip"
    );
    let (code, stdout, stderr) = run(&[zip]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let sections: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("## test"))
        .collect();
    assert_eq!(
        sections,
        [
            "## test.docx",
            "## test.pptx",
            "## test.xlsx",
            "## test_blog.html",
            "## test_serp.html",
            "## test_wikipedia.html",
        ],
        "{sections:?}"
    );
    let docx = stdout.find("## test.docx\n").unwrap();
    let pptx = stdout.find("## test.pptx\n").unwrap();
    assert!(
        stdout[docx..pptx]
            .contains("AutoGen: Enabling Next-Gen LLM Applications via Multi-Agent Conversation"),
        "{}",
        &stdout[docx..pptx.min(docx + 400)]
    );
    for expected in [
        "314b0a30-5b04-470b-b9f7-eed2c2bec74a",
        "6ff4173b-42a5-4784-9b19-f49caff4d93d",
        "Microsoft was founded by [Bill Gates](/wiki/Bill_Gates)",
    ] {
        assert!(stdout.contains(expected), "missing {expected}");
    }

    // Nested: the fixture inside another archive, read one level down.
    let src = Scratch::new("zip-nested");
    let outer = docling::dclx::zip_bytes(vec![
        ("readme.md", b"# Outer\n".as_slice()),
        (
            "inner/test_files.zip",
            std::fs::read(zip).unwrap().as_slice(),
        ),
    ]);
    std::fs::write(src.0.join("outer.zip"), &outer).unwrap();
    let (code, stdout, stderr) = run(&[&src.path("outer.zip")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(stdout, "## readme.md\n\n# Outer\n");
    assert!(
        stderr.contains("inner/test_files.zip: nested archive"),
        "stderr: {stderr}"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .arg(src.path("outer.zip"))
        .env("DOCLING_RS_ZIP_MAX_DEPTH", "2")
        .output()
        .expect("run docling-rs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        stdout.starts_with("## readme.md\n\n# Outer\n\n## inner/test_files.zip/test.docx\n"),
        "{}",
        &stdout[..stdout.len().min(300)]
    );
    assert!(stdout.contains("## inner/test_files.zip/test_wikipedia.html\n"));
}

const PDF_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/data/pdf/sources");

/// #611: `--output-file PATH` (docling's flag) writes the one result to
/// exactly PATH — the bytes stdout would carry, directories created, nothing
/// on stdout — and refuses, with docling's messages, anything but one input
/// document in one format.
#[test]
fn output_file_writes_exactly_that_path() {
    let out = Scratch::new("output-file");
    let a = format!("{MD_FIXTURES}/duck.md");
    let b = format!("{MD_FIXTURES}/blocks.md");
    let target = out.path("nested/dir/result.markdown");
    let (code, stdout, stderr) = run(&[&a, "--output-file", &target]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout}");
    let (_, expected, _) = run(&[&a]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
    // Only the file: no scratch directory or stem-named copy left behind.
    let dir = out.0.join("nested/dir");
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["result.markdown"]);

    let (code, _, stderr) = run(&[&a, &b, "--output-file", &target]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("requires exactly one input document"),
        "{stderr}"
    );
    let (code, _, stderr) = run(&[&a, "--to", "md", "--to", "json", "--output-file", &target]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("requires exactly one output format"),
        "{stderr}"
    );
    let (code, _, stderr) = run(&[MD_FIXTURES, "--output-file", &target]);
    assert_eq!(code, 2, "a directory is several documents: {stderr}");
}

/// #611: Python `docling convert`'s spellings are the same switches —
/// `--page-range` is `--pages`, `--image-export-mode` is `--images`,
/// `--no-tables` is `--no-table-former` — byte for byte.
#[test]
fn docling_flag_spellings_match_ours() {
    let pdf = format!("{PDF_FIXTURES}/multi_page.pdf");
    let ours = run(&[
        &pdf,
        "--text-layer-only",
        "--pages",
        "2-3",
        "--images",
        "embedded",
    ]);
    let theirs = run(&[
        &pdf,
        "--text-layer-only",
        "--page-range",
        "2-3",
        "--image-export-mode",
        "embedded",
    ]);
    assert_eq!(ours.0, 0, "stderr: {}", ours.2);
    assert_eq!(ours.1, theirs.1);
    let full = run(&[&pdf, "--text-layer-only"]);
    assert_ne!(ours.1, full.1, "the window applies");
    let a = run(&[&pdf, "--text-layer-only", "--no-table-former"]);
    let b = run(&[&pdf, "--text-layer-only", "--no-tables"]);
    assert_eq!(a.0, 0, "stderr: {}", a.2);
    assert_eq!(a.1, b.1);
}

/// #781 (docling-core#772): a referenced picture's HTML `src` is the
/// Markdown destination, HTML-escaped — `&`, `,` and `+` in the document's
/// name stay as they are instead of `%26`, `%2C`, `%2B`.
#[test]
fn referenced_image_src_is_encoded_like_markdown() {
    let dir = std::env::temp_dir().join(format!("docling-rs-781-{}", std::process::id()));
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let doc = dir.join("Q&A, v2+final.docx");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/docx/sources/word_image_anchors.docx"
        ),
        &doc,
    )
    .unwrap();
    let (code, _, stderr) = run(&[
        doc.to_str().unwrap(),
        "--to",
        "html",
        "--to",
        "md",
        "--images",
        "referenced",
        "--output",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let html = std::fs::read_to_string(out.join("Q&A, v2+final.html")).unwrap();
    let md = std::fs::read_to_string(out.join("Q&A, v2+final.md")).unwrap();
    assert!(
        html.contains("<img src=\"Q&amp;A,%20v2+final_artifacts/image_000000.png\""),
        "{html}"
    );
    assert!(
        md.contains("](Q&A,%20v2+final_artifacts/image_000000.png)"),
        "{md}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #611: an encrypted PDF converts with docling's `--pdf-password` (or
/// `--password`, the spelling PDF_CONFORMANCE.md used); without one it fails
/// with docling's message. (Text-layer path: no models needed.)
#[test]
fn pdf_password_opens_an_encrypted_pdf() {
    let pdf = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/pdf_password/sources/2206.01062_pg3.pdf"
    );
    let (code, _, stderr) = run(&[pdf, "--text-layer-only"]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("the PDF is encrypted: a password is required"),
        "{stderr}"
    );
    let (code, with, stderr) = run(&[pdf, "--text-layer-only", "--pdf-password", "1234"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(with.contains("DocLayNet"), "{with}");
    let (_, alias, _) = run(&[pdf, "--text-layer-only", "--password", "1234"]);
    assert_eq!(alias, with);
    let (code, _, stderr) = run(&[pdf, "--text-layer-only", "--pdf-password"]);
    assert_eq!(code, 2, "a bare flag is a usage error: {stderr}");
}

/// #625: the same password opens an encrypted Office document, given on the
/// command line or — out of the process list — as `--password-file`.
#[test]
fn password_opens_an_encrypted_office_document() {
    let doc = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling/tests/data/encrypted/min_encrypted.doc"
    );
    let (code, _, stderr) = run(&[doc]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("doc: document is encrypted (a password is required to open it)"),
        "{stderr}"
    );
    let (code, with, stderr) = run(&[doc, "--password", "1234"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(with.contains("Hello encrypted legacy doc"), "{with}");
    let scratch = Scratch::new("password");
    let file = scratch.0.join("pw.txt");
    std::fs::write(&file, "1234\n").unwrap();
    let (code, from_file, stderr) = run(&[doc, "--password-file", file.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(from_file, with);
    let (code, _, stderr) = run(&[doc, "--password-file", "/no/such/file"]);
    assert_eq!(
        code, 2,
        "an unreadable password file is a usage error: {stderr}"
    );
}

/// A one-thread HTTP fixture server for the URL-source tests (#668): a
/// handful of routes, each answering with the status, headers and body the
/// test needs. It serves until dropped; the binary under test reaches it on
/// 127.0.0.1, which the SSRF guard blocks unless the test lifts it.
struct FixtureServer {
    url: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FixtureServer {
    fn start() -> Self {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let docx = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/data/docx/sources/Strict.docx"
        ))
        .unwrap();
        std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, headers, body): (&str, Vec<String>, Vec<u8>) = match path.as_str() {
                    // A bare path and a generic type: only the bytes decide.
                    "/doc" => (
                        "200 OK",
                        vec!["Content-Type: application/octet-stream".into()],
                        docx.clone(),
                    ),
                    // A bare path typed by Content-Type.
                    "/page" => (
                        "200 OK",
                        vec!["Content-Type: text/html; charset=utf-8".into()],
                        b"<html><body><h1>Fetched</h1><p>over http</p></body></html>".to_vec(),
                    ),
                    // Content-Disposition names (and so types) the document.
                    "/download?id=7" => (
                        "200 OK",
                        vec![
                            "Content-Type: application/octet-stream".into(),
                            "Content-Disposition: attachment; filename=\"notes.md\"".into(),
                        ],
                        b"# From disposition\n\ntext\n".to_vec(),
                    ),
                    // Nothing to go on: the hints must decide.
                    "/opaque" => (
                        "200 OK",
                        vec!["Content-Type: application/octet-stream".into()],
                        b"plain words only\n".to_vec(),
                    ),
                    "/redirect" => ("302 Found", vec!["Location: /page".into()], Vec::new()),
                    "/loop" => ("302 Found", vec!["Location: /loop".into()], Vec::new()),
                    "/accept" => (
                        "200 OK",
                        vec!["Content-Type: text/plain".into()],
                        request
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("accept:"))
                            .unwrap_or("accept: (none)")
                            .as_bytes()
                            .to_vec(),
                    ),
                    _ => ("404 Not Found", vec![], b"no such route".to_vec()),
                };
                let mut response = format!("HTTP/1.1 {status}\r\n");
                for h in headers {
                    response.push_str(&h);
                    response.push_str("\r\n");
                }
                response.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ));
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        Self { url, stop }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Run the binary with `stdin` piped in and the private-address block lifted
/// (the fixture server is on 127.0.0.1).
fn run_with(args: &[&str], stdin: &[u8], allow_private: bool) -> (i32, String, String) {
    use std::io::Write as _;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_docling-rs"));
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if allow_private {
        cmd.env("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH", "1");
    } else {
        cmd.env_remove("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH");
    }
    let mut child = cmd.spawn().expect("run docling-rs");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().expect("wait");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const STRICT_DOCX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/data/docx/sources/Strict.docx"
);

/// #668: `cat test.docx | docling-rs --from docx -` is `docling-rs test.docx`
/// byte for byte; without the hint the OOXML package signature decides;
/// bytes nobody can type are a usage error with the way out named.
#[test]
fn stdin_converts_with_a_format_hint_or_by_signature() {
    let (code, expected, stderr) = run(&[STRICT_DOCX]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let bytes = std::fs::read(STRICT_DOCX).unwrap();
    let (code, from_hint, stderr) = run_with(&["--from", "docx", "-"], &bytes, false);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(from_hint, expected);
    let (code, sniffed, stderr) = run_with(&["-"], &bytes, false);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(sniffed, expected);
    // A media type stands in for the missing extension.
    let (code, md, stderr) = run_with(
        &["--mime-type", "text/markdown", "-"],
        b"# From stdin\n\nbody\n",
        false,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(md.starts_with("# From stdin"), "{md:?}");
    // Text with nothing else to go on is plain text (#671); binary bytes
    // are refused: exit 2, the hint flags named.
    let (code, out, stderr) = run_with(&["-"], b"plain words only\n", false);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, "plain words only\n");
    let (code, _, stderr) = run_with(&["-"], b"\x00\x01\x02\xff\xfe\x00", false);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("cannot determine the format of stdin")
            && stderr.contains("--from FORMAT or --mime-type TYPE"),
        "{stderr}"
    );
    // The hint is validated like every other flag value.
    let (code, _, stderr) = run_with(&["--from", "nope", "-"], b"", false);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("unknown input format 'nope'"), "{stderr}");
    let (code, _, stderr) = run_with(&["--mime-type", "application/x-what", "-"], b"", false);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("not a media type docling converts"),
        "{stderr}"
    );
}

/// #668: `--from` also retypes a file whose extension says otherwise, and
/// `--output-file` / `--output` take stdin like any other source.
/// Content detection (#672): a PPTX renamed to a name with no extension
/// converts as the original, a PDF saved as `.bin` converts, and a
/// correctly named file is read by its name.
#[test]
fn nameless_and_misnamed_files_convert_by_content() {
    let scratch = Scratch::new("detect");
    let pptx = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/pptx/sources/powerpoint_bad_text.pptx"
    );
    let deck = scratch.path("deck");
    std::fs::copy(pptx, &deck).unwrap();
    let (code, want, stderr) = run(&[pptx]);
    assert_eq!(code, 0, "{stderr}");
    let (code, got, stderr) = run(&[&deck]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(got, want);
    assert!(!got.trim().is_empty());
    let pdf = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/pdf/sources/base14_fonts.pdf"
    );
    let bin = scratch.path("scan.bin");
    std::fs::copy(pdf, &bin).unwrap();
    let (code, out, stderr) = run(&["--text-layer-only", &bin]);
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("quick brown fox"), "{out}");
}

/// Charset detection (#675): MarkItDown's cp932 CSV converts with its
/// letters and a warning naming the encoding; `--encoding` wins.
#[test]
fn legacy_charsets_are_guessed() {
    let csv = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling/tests/data/csv/sources/test_mskanji.csv"
    );
    let (code, out, stderr) = run(&["--compact-tables", csv]);
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("| 髙橋淳 | 35 | 名古屋 |"), "{out}");
    assert!(stderr.contains("Shift_JIS"), "{stderr}");
    let (code, forced, stderr) = run(&["--compact-tables", "--encoding", "shift_jis", csv]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(forced, out);
    assert!(!stderr.contains("warning"), "{stderr}");
}

/// Picture alt text (#676): MarkItDown's `test.pptx` alt string is in the
/// JSON, in Markdown under `--picture-alt-text` or `--strict`, not by
/// default.
/// Document metadata (#679): nothing by default; `--metadata frontmatter`
/// opens the Markdown with the file's own; `json` puts it in the JSON; a
/// wrong spelling is refused.
#[test]
fn metadata_renders_on_request() {
    let epub = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling-py/tests/data/markitdown/test.epub"
    );
    let (code, out, stderr) = run(&[epub]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!out.starts_with("---"), "{out}");
    let (code, out, _) = run(&["--metadata", "frontmatter", epub]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with("---\ntitle: Test EPUB Document\nauthors: [Test Author]\n"),
        "{out}"
    );
    assert!(out.contains("\n---\n\n"), "{out}");
    let (code, out, _) = run(&["--metadata", "json", "--to", "json", epub]);
    assert_eq!(code, 0);
    assert!(
        out.contains("\"metadata\": {") && out.contains("\"Test Author\""),
        "{out}"
    );
    let (code, _, stderr) = run(&["--metadata", "yaml", epub]);
    assert_ne!(code, 0);
    assert!(
        stderr.contains("--metadata must be one of none, json, frontmatter"),
        "{stderr}"
    );
}

#[test]
fn picture_alt_text_renders_on_request() {
    let pptx = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling-py/tests/data/markitdown/test.pptx"
    );
    let alt = "This phrase of the caption is Human-written.";
    let (code, out, stderr) = run(&[pptx]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!out.contains(alt), "{out}");
    let (code, out, _) = run(&["--picture-alt-text", pptx]);
    assert_eq!(code, 0);
    assert!(out.contains(alt), "{out}");
    let (code, out, _) = run(&["--strict", pptx]);
    assert_eq!(code, 0);
    assert!(out.contains(alt), "{out}");
    let (code, out, _) = run(&["--to", "json", pptx]);
    assert_eq!(code, 0);
    assert!(
        out.contains(alt) && out.contains("\"created_by\": \"alt_text\""),
        "{out}"
    );
}

#[test]
fn hints_apply_to_files_and_stdin_lands_in_output_files() {
    let scratch = Scratch::new("hints");
    let txt = scratch.path("notes.txt");
    std::fs::write(&txt, "<h1>Typed as HTML</h1><p>not markdown</p>").unwrap();
    let (code, out, stderr) = run(&["--from", "html", &txt]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.starts_with("# Typed as HTML"), "{out:?}");
    // No extension: the media type decides; text bytes read as plain text
    // (#671), so the heading is a paragraph until a hint says Markdown;
    // binary bytes are refused with the hints named.
    let bare = scratch.path("bare");
    std::fs::write(&bare, "# Bare\n").unwrap();
    let (code, out, stderr) = run(&[&bare]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, "# Bare\n");
    let (code, out, stderr) = run(&["--mime-type", "text/markdown", &bare]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.starts_with("# Bare"), "{out:?}");
    let blob = scratch.path("blob");
    std::fs::write(&blob, b"\x00\x01\x02\xff\xfe\x00").unwrap();
    let (code, _, stderr) = run(&[&blob]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("--from FORMAT or --mime-type TYPE"),
        "{stderr}"
    );
    // stdin into --output-file …
    let target = scratch.path("out/result.md");
    let bytes = std::fs::read(STRICT_DOCX).unwrap();
    let (code, _, stderr) = run_with(
        &["--from", "docx", "-", "--output-file", &target],
        &bytes,
        false,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let (_, expected, _) = run(&[STRICT_DOCX]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), expected);
    // … and into a batch, named after the document.
    let outdir = scratch.path("batch");
    let (code, stdout, stderr) =
        run_with(&["-", "--to", "json", "--output", &outdir], &bytes, false);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.trim().ends_with("document.json"), "{stdout:?}");
    assert!(std::path::Path::new(&outdir)
        .join("document.json")
        .is_file());
}

/// #668: `data:` URLs convert from their payload, typed by the media type
/// (or the bytes), `file:` URLs are the path they name.
#[test]
fn data_and_file_urls_convert() {
    let (code, out, stderr) = run(&["data:text/html;base64,PGgxPkhpPC9oMT48cD50aGVyZTwvcD4="]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, "# Hi\n\nthere\n");
    // An untyped text payload is plain text (#671): two paragraphs, the
    // `#` a character; `--from md` reads it as Markdown. Untyped binary
    // bytes are refused with the hints named.
    let (code, out, stderr) = run(&["data:,%23%20Percent%0A%0Aplain"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, "# Percent\n\nplain\n");
    let (code, out, stderr) = run(&["data:;base64,AAECA//+AA=="]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("--from FORMAT or --mime-type TYPE"),
        "{stderr}"
    );
    assert!(out.is_empty());
    let (code, out, stderr) = run(&["--from", "md", "data:,%23%20Percent%0A%0Aplain"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.starts_with("# Percent"), "{out:?}");
    let (code, _, stderr) = run(&["data:text/html;base64,@@@"]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("invalid base64"), "{stderr}");
    let (_, expected, _) = run(&[STRICT_DOCX]);
    let (code, out, stderr) = run(&[&format!("file://{STRICT_DOCX}")]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, expected);
}

/// #668: an http(s) source is fetched — named by `Content-Disposition`, then
/// the URL path; typed by that name, then `Content-Type`, then the bytes —
/// with redirects followed and every hop checked; `--no-url-fetch` and the
/// SSRF guard refuse before any byte moves.
#[test]
fn http_sources_are_fetched_named_and_typed() {
    let server = FixtureServer::start();
    let base = server.url.as_str();
    let (_, expected, _) = run(&[STRICT_DOCX]);
    // Bare path, generic Content-Type: the OOXML signature decides.
    let (code, out, stderr) = run_with(&[&format!("{base}/doc")], b"", true);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, expected);
    // Bare path, typed by Content-Type.
    let (code, out, stderr) = run_with(&[&format!("{base}/page")], b"", true);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, "# Fetched\n\nover http\n");
    // Content-Disposition names it `notes.md`; a batch lands it by that stem.
    let scratch = Scratch::new("http");
    let outdir = scratch.path("out");
    let (code, stdout, stderr) = run_with(
        &[&format!("{base}/download?id=7"), "--output", &outdir],
        b"",
        true,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.trim().ends_with("notes.md"), "{stdout:?}");
    assert!(
        std::fs::read_to_string(std::path::Path::new(&outdir).join("notes.md"))
            .unwrap()
            .starts_with("# From disposition")
    );
    // Nothing but the bytes decides, and they are text: plain text (#671);
    // a hint still types it.
    let (code, out, stderr) = run_with(&[&format!("{base}/opaque")], b"", true);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("plain words only"), "{out:?}");
    let (code, out, stderr) = run_with(&["--from", "txt", &format!("{base}/opaque")], b"", true);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("plain words only"), "{out:?}");
    // A redirect is followed; a loop is not.
    let (code, out, stderr) = run_with(&[&format!("{base}/redirect")], b"", true);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(out, "# Fetched\n\nover http\n");
    let (code, _, stderr) = run_with(&[&format!("{base}/loop")], b"", true);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("too many redirects"), "{stderr}");
    // The document Accept header rides along.
    let (code, out, stderr) = run_with(&["--from", "txt", &format!("{base}/accept")], b"", true);
    assert_eq!(code, 0, "stderr: {stderr}");
    // (`*/*` is Markdown-escaped on the way out; the prefix is the check.)
    assert!(
        out.contains("accept: text/markdown, text/html;q=0.9"),
        "{out:?}"
    );
    // A 404 is a failed fetch, not a usage error.
    let (code, _, stderr) = run_with(&[&format!("{base}/missing")], b"", true);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("404"), "{stderr}");
    // Refused before fetching: the flag, and the private-address guard.
    let (code, _, stderr) = run_with(&["--no-url-fetch", &format!("{base}/page")], b"", true);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("URL sources are disabled"), "{stderr}");
    let (code, _, stderr) = run_with(&[&format!("{base}/page")], b"", false);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(
        stderr.contains("private/loopback address")
            && stderr.contains("DOCLING_RS_ALLOW_PRIVATE_IP_FETCH"),
        "{stderr}"
    );
}

/// `--picture-description-mode` takes `caption` or `ocr`;
/// `--picture-description-concurrency` a positive integer — the usage
/// error names the flag, before any conversion (#670).
#[test]
fn picture_description_flags_validate() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../docling/sample.html");
    for args in [
        &[
            "--picture-description",
            "--picture-description-mode",
            "summary",
            fixture,
        ][..],
        &["--picture-description-concurrency", "0", fixture][..],
        &["--picture-description-concurrency", "two", fixture][..],
    ] {
        let (code, _, stderr) = run(args);
        assert_eq!(code, 2, "args {args:?}, stderr: {stderr}");
        assert!(stderr.contains("--picture-description"), "{stderr}");
    }
}

/// Enabled without any endpoint, the CLI refuses up front and names the
/// flag that would supply one.
#[test]
fn picture_description_needs_an_endpoint() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../docling/sample.html");
    let out = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
        .args(["--picture-description", fixture])
        .env_remove("DOCLING_RS_VLM_ENDPOINT")
        .output()
        .expect("run docling-rs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("--picture-description-endpoint"),
        "{stderr}"
    );
}

/// `--redact-allow-pattern` must compile and `--redact-ner-min-score` lie
/// in 0–1 — usage errors naming the flag (#694).
#[test]
fn redact_precision_flags_validate() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../docling/sample.html");
    for args in [
        &["--redact-pii", "--redact-allow-pattern", "(", fixture][..],
        &["--redact-ner-min-score", "1.5", fixture][..],
        &["--redact-ner-min-score", "high", fixture][..],
        &["--redact-allow-pattern"][..],
    ] {
        let (code, _, stderr) = run(args);
        assert_eq!(code, 2, "args {args:?}, stderr: {stderr}");
        assert!(stderr.contains("--redact-"), "{stderr}");
    }
}

/// #671: a source file converts to a fenced code block, a `.json` that is
/// no DoclingDocument to a `json` block, and `--list-input-formats` names
/// the text extensions.
/// Jupyter notebooks (#673): MarkItDown's `test_notebook.ipynb` prints its
/// heading, the code fences and the sections; `--ipynb-outputs text` adds
/// the stream output; a wrong spelling is refused; `ipynb` is listed.
#[test]
fn jupyter_notebooks_convert() {
    let nb = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling/tests/data/ipynb/sources/test_notebook.ipynb"
    );
    let (code, stdout, stderr) = run(&[nb]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("# Test Notebook\n"), "{stdout}");
    assert!(
        stdout.contains("```\nprint(\"markitdown\")\n```"),
        "{stdout}"
    );
    assert!(stdout.contains("## Code Cell Below"), "{stdout}");
    assert!(!stdout.contains("\n42\n"), "{stdout}");
    let (_, strict, _) = run(&["--strict", nb]);
    assert!(
        strict.contains("```python\nprint(\"markitdown\")"),
        "{strict}"
    );
    let (code, stdout, stderr) = run(&["--ipynb-outputs", "text", nb]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("print(42)\n```\n\n```\n42\n```"),
        "{stdout}"
    );
    let (code, _, stderr) = run(&["--ipynb-outputs", "all", nb]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("--ipynb-outputs must be one of none, text"),
        "{stderr}"
    );
    let (code, stdout, _) = run(&["--list-input-formats"]);
    assert_eq!(code, 0);
    assert!(stdout.lines().any(|l| l == "ipynb"), "{stdout}");
}

#[test]
fn plain_text_and_code_inputs_convert() {
    let py = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling/tests/data/text/sources/script.py"
    );
    let (code, stdout, stderr) = run(&[py]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("```\n#!/usr/bin/env python3"),
        "{stdout}"
    );
    let (_, strict, _) = run(&["--strict", py]);
    assert!(
        strict.starts_with("```python\n#!/usr/bin/env python3"),
        "{strict}"
    );
    let json = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docling/tests/data/text/sources/test.json"
    );
    let (code, stdout, _) = run(&[json]);
    assert_eq!(code, 0);
    assert!(
        stdout.starts_with("```\n{") && stdout.contains("5b64c88c-b3c3-4510-bcb8-da0b200602d8"),
        "{stdout}"
    );
    let (code, stdout, _) = run(&["--list-input-formats"]);
    assert_eq!(code, 0);
    let listed: Vec<&str> = stdout.lines().collect();
    for e in ["py", "jsonl", "log", "xml"] {
        assert!(listed.contains(&e), "{e} not listed");
    }
}

/// #783: `--progress` prints the document's progress to stderr — off a
/// terminal, the plain lines and counts; the default off a terminal is
/// silence, `--no-progress` likewise.
#[test]
fn progress_flag_prints_the_pages_to_stderr() {
    let pdf = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/data/pdf/sources/skipped_1page.pdf"
    );
    let (code, _, err) = run(&["--progress", "--text-layer-only", "--pages", "2-3", pdf]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        err,
        "[1] Converting skipped_1page\n  pages 2/2\nFinished skipped_1page: success\n"
    );

    // A spent budget: the cut-off pages still count, the status says partial.
    let (code, _, err) = run(&[
        "--progress",
        "--text-layer-only",
        "--document-timeout",
        "0.000000001",
        pdf,
    ]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("  pages 3/3\nFinished skipped_1page: partial_success\n"),
        "{err}"
    );

    // Off a terminal the default is quiet, and so is --no-progress.
    let (code, _, err) = run(&["--text-layer-only", pdf]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(err, "");
    let (_, _, err) = run(&["--progress", "--no-progress", "--text-layer-only", pdf]);
    assert_eq!(err, "");
}

/// `--text-layer-only=auto`: on a digital PDF the text-layer output of
/// `--text-layer-only`, byte for byte (no model needed); the `=` form takes
/// the bools too, and any other value is a usage error.
#[test]
fn text_layer_only_auto_flag() {
    let pdf = format!("{PDF_FIXTURES}/multi_page.pdf");
    let on = run(&[&pdf, "--text-layer-only"]);
    assert_eq!(on.0, 0, "stderr: {}", on.2);
    let auto = run(&[&pdf, "--text-layer-only=auto"]);
    assert_eq!(auto.0, 0, "stderr: {}", auto.2);
    assert_eq!(on.1, auto.1);
    let on_eq = run(&[&pdf, "--text-layer-only=true"]);
    assert_eq!(on_eq.1, on.1);
    let (code, _, err) = run(&[&pdf, "--text-layer-only=sometimes"]);
    assert_eq!(code, 2);
    assert!(err.contains("true, false, auto"), "stderr: {err}");
}

/// #896: `--no-picture-images` reaches a batch's PDFs. The batch converts
/// them on its own warm pipeline, which skipped the picture enrichment pass
/// the single-file path runs, so the option held only there. Needs the
/// layout model (the pictures come from it); skips without it.
#[test]
fn batch_drops_picture_images_on_request() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let models = root.join(".models");
    if !models.join("layout_heron.onnx").exists() && !models.join("layout_heron_int8.onnx").exists()
    {
        eprintln!("skipping: models not found");
        return;
    }
    let pdf = root.join("tests/data/pdf/sources/picture_classification.pdf");
    let mut counts = Vec::new();
    for drop in [false, true] {
        let out = Scratch::new(if drop { "nopic" } else { "pic" });
        let out_dir = out.path("");
        let mut args = vec![
            pdf.to_str().unwrap(),
            "--output",
            &out_dir,
            "--to",
            "json",
            "--no-ocr",
        ];
        if drop {
            args.push("--no-picture-images");
        }
        // Tests run with CWD = the crate dir: point the batch at the repo's
        // assets (#285), as a deployment would.
        let run = Command::new(env!("CARGO_BIN_EXE_docling-rs"))
            .args(&args)
            .env("DOCLING_RS_MODELS_DIR", &models)
            .output()
            .expect("run docling-rs");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(run.status.success(), "{stderr}");
        let json = std::fs::read_to_string(out.0.join("picture_classification.json"))
            .expect("batch output");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pictures = v["pictures"].as_array().expect("pictures");
        assert!(!pictures.is_empty(), "the fixture has pictures");
        counts.push((
            pictures.len(),
            pictures.iter().filter(|p| p.get("image").is_some()).count(),
        ));
    }
    assert_eq!(counts[0].0, counts[0].1, "kept by default: {counts:?}");
    assert_eq!(counts[1].1, 0, "dropped on request: {counts:?}");
}
