//! Output regression suite.
//!
//! Every source the suite covers is converted to legacy Markdown, strict
//! Markdown, docling JSON, LaTeX and HTML, and compared against the committed
//! fixtures in `tests/data/<format>/expected/`. This pins the Rust converter's
//! output so any unintended change is caught.
//!
//! Sources come from two places, so the upstream corpus is stored once:
//!
//! - `tests/data/<format>/mirror.txt` (in this crate) lists, one file name per
//!   line, the sources taken from the repository-root corpus mirrored from
//!   Python docling, `../../tests/data/<format>/sources/` — the files the
//!   conformance scripts compare against upstream groundtruth. Add a line to
//!   cover another upstream fixture; never copy the file here.
//! - `tests/data/<format>/sources/` (in this crate) holds the suite's **own**
//!   fixtures: formats docling has no backend for, and regression cases of our
//!   own. A file here must not exist in the mirrored corpus.
//!
//! The expected files are keyed by the source's file name, whichever place it
//! came from.
//!
//! The ML formats (PDF, images, METS) need the ONNX models, so they are
//! covered by the deterministic snapshot harness (`scripts/conformance/pdf_conformance.sh`)
//! instead of this pure-Rust test.
//!
//! Regenerate the fixtures after an *intentional* output change:
//!
//! ```bash
//! DOCLING_RS_REGEN=1 cargo test -p docling --test regression
//! ```
//!
//! `DOCLING_RS_REGRESSION_FORMATS=t602,print_file` restricts a run (checking or
//! regenerating) to those format directories — for working on one backend
//! without converting the whole corpus.

use std::fs;
use std::path::{Path, PathBuf};

use docling::{DocumentConverter, SourceDocument};

/// This crate's `tests/data`: the expected fixtures, the own sources and the
/// `mirror.txt` manifests.
fn data_dir() -> PathBuf {
    // `cargo test` runs with the working directory set to the package root, so
    // resolve there first — this stays correct even if `target/` was copied from
    // another checkout (which leaves env!("CARGO_MANIFEST_DIR"), baked at compile
    // time, pointing at a now-stale absolute path). Fall back to the baked path
    // for non-`cargo test` invocations.
    if let Ok(cwd) = std::env::current_dir() {
        let d = cwd.join("tests/data");
        if d.is_dir() {
            return d;
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// The repository-root corpus mirrored from upstream docling (`tests/data` two
/// levels above the crate), which `mirror.txt` entries refer to.
fn mirror_dir() -> PathBuf {
    let d = data_dir().join("../../../../tests/data");
    d.canonicalize().unwrap_or(d)
}

/// A covered source: where to read it, and the format directory whose
/// `expected/` holds its fixtures.
struct Source {
    path: PathBuf,
    fmt_dir: PathBuf,
    /// `<format>/<file>`, for messages.
    rel: String,
}

/// Every covered source, in a stable order: per format, the `mirror.txt`
/// entries (resolved into the mirrored corpus), then the crate's own files.
fn sources() -> Vec<Source> {
    let mut formats: Vec<PathBuf> = fs::read_dir(data_dir())
        .expect("tests/data missing")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    formats.sort();
    if let Some(only) = std::env::var_os("DOCLING_RS_REGRESSION_FORMATS") {
        let only = only.to_string_lossy().to_string();
        let keep: Vec<&str> = only.split(',').map(str::trim).collect();
        formats.retain(|p| {
            p.file_name()
                .is_some_and(|n| keep.contains(&n.to_string_lossy().as_ref()))
        });
    }

    let mirror = mirror_dir();
    let mut out = Vec::new();
    for fmt_dir in formats {
        let fmt = fmt_dir.file_name().unwrap().to_string_lossy().to_string();
        let mut names: Vec<(PathBuf, String)> = Vec::new();
        if let Ok(manifest) = fs::read_to_string(fmt_dir.join("mirror.txt")) {
            for name in manifest.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let path = mirror.join(&fmt).join("sources").join(name);
                assert!(
                    path.is_file(),
                    "{fmt}/mirror.txt names {name}, which is not in the mirrored corpus at {}",
                    path.display()
                );
                names.push((path, name.to_string()));
            }
        }
        let own = fmt_dir.join("sources");
        if own.is_dir() {
            let mut files: Vec<PathBuf> = fs::read_dir(&own)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            files.sort();
            for path in files {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                // An own fixture shadowing an upstream one is the duplication
                // this layout exists to prevent (and its expected files would
                // collide): keep it in one place only.
                let twin = mirror.join(&fmt).join("sources").join(&name);
                assert!(
                    !twin.exists(),
                    "{fmt}/sources/{name} also exists in the mirrored corpus — list it in \
                     {fmt}/mirror.txt instead of copying it"
                );
                assert!(
                    !names.iter().any(|(_, n)| *n == name),
                    "{fmt}/sources/{name} is also listed in {fmt}/mirror.txt"
                );
                names.push((path, name));
            }
        }
        for (path, name) in names {
            out.push(Source {
                path,
                rel: format!("{fmt}/{name}"),
                fmt_dir: fmt_dir.clone(),
            });
        }
    }
    out
}

/// `<fmt>/expected/<file><suffix>` for a source. A mirrored source inside a
/// directory (`1706.03762/main.tex`) takes upstream's fixture name for it,
/// the path with `_` for `/` (`1706.03762_main.tex`).
fn expected_path(src: &Source, suffix: &str) -> PathBuf {
    let name = src
        .rel
        .split_once('/')
        .map_or(src.rel.as_str(), |(_, name)| name)
        .replace('/', "_");
    src.fmt_dir.join("expected").join(format!("{name}{suffix}"))
}

/// A converter for `src`. An XBRL instance is paired with the `*-taxonomy`
/// directory beside it that holds the schema its `link:schemaRef` names — the
/// layout upstream's `tests/data/xbrl/sources/` keeps its fixtures in, whose
/// test passes that directory as the backend's `taxonomy` option.
fn converter_for(src: &Path) -> DocumentConverter {
    let mut converter = DocumentConverter::new();
    if let Some(taxonomy) = xbrl_taxonomy_dir(src) {
        converter = converter.xbrl_taxonomy(taxonomy);
    }
    converter
}

fn xbrl_taxonomy_dir(src: &Path) -> Option<PathBuf> {
    let dir = src.parent()?;
    if dir.parent()?.file_name()? != "xbrl" {
        return None;
    }
    let text = fs::read_to_string(src).ok()?;
    let href = text
        .split("schemaRef")
        .nth(1)?
        .split("href=\"")
        .nth(1)?
        .split('"')
        .next()?;
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.to_string_lossy().ends_with("-taxonomy"))
        .find(|p| p.join(href).is_file())
}

fn convert(src: &Path, strict: bool) -> Result<docling::DoclingDocument, String> {
    let source = SourceDocument::from_file(src).map_err(|e| e.to_string())?;
    converter_for(src)
        .strict(strict)
        .convert(source)
        .map(|r| r.document)
        .map_err(|e| e.to_string())
}

/// Convert via the streaming API and concatenate every chunk.
fn stream_to_string(src: &Path, strict: bool) -> Result<String, String> {
    let source = SourceDocument::from_file(src).map_err(|e| e.to_string())?;
    let stream = converter_for(src)
        .strict(strict)
        .convert_streaming(source)
        .map_err(|e| e.to_string())?;
    let mut out = String::new();
    for chunk in stream {
        out.push_str(&chunk.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// Every check the suite makes on one source, from a single pass over it.
///
/// The three corpus-wide tests below used to convert every source on their
/// own — eight conversions per file, each test walking the corpus serially —
/// and the `regression` binary had grown to ten minutes of the CI test job
/// (the corpus doubled in a month). One pass converts each source five times
/// (buffered legacy and strict, streamed legacy and strict, streamed with
/// options), runs across the sources in parallel (`par_map`), and keeps only
/// the verdicts; the tests read their share of them. Filtering `cargo test`
/// to one test name therefore still runs the whole pass once.
#[derive(Default)]
struct Verdicts {
    /// `outputs_match_fixtures`: outputs that differ from the committed
    /// fixtures (or fixtures that are missing).
    fixtures: Vec<String>,
    /// `streaming_matches_buffered_markdown`: streamed Markdown that differs
    /// from the buffered export, legacy or strict.
    streaming: Vec<String>,
    /// `streaming_with_options_matches_buffered_markdown`: the same with
    /// every layer and picture traversal on.
    streaming_with_options: Vec<String>,
    sources: usize,
    regen: bool,
}

fn verdicts() -> &'static Verdicts {
    static VERDICTS: std::sync::OnceLock<Verdicts> = std::sync::OnceLock::new();
    VERDICTS.get_or_init(|| {
        let regen = std::env::var_os("DOCLING_RS_REGEN").is_some();
        let srcs = sources();
        assert!(
            !srcs.is_empty(),
            "no sources under {}",
            data_dir().display()
        );
        let per_source = par_map(&srcs, |src| check_source(src, regen));
        let mut all = Verdicts {
            sources: srcs.len(),
            regen,
            ..Verdicts::default()
        };
        for v in per_source {
            all.fixtures.extend(v.fixtures);
            all.streaming.extend(v.streaming);
            all.streaming_with_options.extend(v.streaming_with_options);
        }
        all
    })
}

/// `f` over every item, one plain thread per core, the results in the
/// items' order (so the failure lists read as the serial loops' did).
///
/// Plain threads, not rayon: `stream_to_string` drains a producer that
/// runs on a thread of its own (`docling::stream`), and the PPTX / XLSX
/// backends that producer may run fan their slides / sheets out on rayon's
/// global pool. Walked on that pool, every worker ended up blocked in
/// `recv()` on a producer waiting for a free worker — the suite's first
/// parallel run hung that way after eight minutes of CPU.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(items.len()));
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(items.len().max(1));
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    done.lock().unwrap().push((i, r));
                })
            })
            .collect();
        // A worker's panic is the pass's.
        for h in handles {
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

/// Convert `src` every way the suite checks and record what differs.
fn check_source(src: &Source, regen: bool) -> Verdicts {
    let rel = &src.rel;
    let mut v = Verdicts::default();

    let legacy = convert(&src.path, false);
    let strict = convert(&src.path, true);

    // The committed fixtures: legacy and strict Markdown, JSON, LaTeX, HTML.
    match (&legacy, &strict) {
        (Err(e), _) => v.fixtures.push(format!("{rel}: convert error: {e}")),
        (_, Err(e)) => v.fixtures.push(format!("{rel}: strict convert error: {e}")),
        (Ok(legacy), Ok(strict)) => {
            let outputs = [
                (".md", legacy.export_to_markdown()),
                (".strict.md", strict.export_to_markdown()),
                (".json", legacy.export_to_json()),
                (".tex", legacy.export_to_latex()),
                (
                    ".html",
                    legacy
                        .export_to_html_with_images(docling::ImageMode::Embedded, "artifacts")
                        .0,
                ),
            ];
            for (suffix, got) in outputs {
                let path = expected_path(src, suffix);
                if regen {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, got).unwrap();
                    continue;
                }
                match fs::read_to_string(&path) {
                    Ok(want) if want == got => {}
                    Ok(want)
                        if HOST_FONT_PICTURES.contains(&rel.as_str())
                            && mask_png_payloads(&want) == mask_png_payloads(&got) => {}
                    Ok(_) => v.fixtures.push(format!(
                        "{rel}{suffix}: output changed (run DOCLING_RS_REGEN=1 to update)"
                    )),
                    Err(_) => v
                        .fixtures
                        .push(format!("{rel}{suffix}: missing fixture {}", path.display())),
                }
            }
        }
    }

    // Streaming Markdown must be byte-identical to the buffered export for
    // every (non-PDF) source — the streaming serializer and the buffered one
    // are held to the same output. (PDF, the format with real page-level
    // streaming, is covered by the snapshot harness and the `StreamAssembler`
    // unit tests in `docling-pdf`.)
    for (strict, doc) in [(false, &legacy), (true, &strict)] {
        let buffered = match doc {
            Ok(d) => d.export_to_markdown(),
            Err(e) => {
                v.streaming.push(format!("{rel}: convert error: {e}"));
                continue;
            }
        };
        match stream_to_string(&src.path, strict) {
            Ok(streamed) if streamed == buffered => {}
            Ok(_) => v.streaming.push(format!(
                "{rel} (strict={strict}): streamed Markdown != buffered export"
            )),
            Err(e) => v
                .streaming
                .push(format!("{rel} (strict={strict}): stream error: {e}")),
        }
    }

    // `convert_streaming_with_options` with every layer and picture traversal
    // on — the CLI's streamed `--layers all --traverse-pictures` — matches
    // the buffered `export_to_markdown_with_options` byte for byte.
    let options = docling::MarkdownExportOptions {
        layers: docling::ContentLayers::ALL,
        traverse_pictures: true,
        ..Default::default()
    };
    match &legacy {
        Err(e) => v
            .streaming_with_options
            .push(format!("{rel}: convert error: {e}")),
        Ok(legacy) => {
            let buffered = legacy.export_to_markdown_with_options(&options).0;
            let streamed = SourceDocument::from_file(&src.path)
                .map_err(|e| e.to_string())
                .and_then(|source| {
                    converter_for(&src.path)
                        .convert_streaming_with_options(source, &options)
                        .map_err(|e| e.to_string())
                })
                .and_then(|stream| {
                    stream
                        .collect::<Result<String, _>>()
                        .map_err(|e| e.to_string())
                });
            match streamed {
                Ok(streamed) if streamed == buffered => {}
                Ok(_) => v
                    .streaming_with_options
                    .push(format!("{rel}: streamed Markdown != buffered export")),
                Err(e) => v
                    .streaming_with_options
                    .push(format!("{rel}: stream error: {e}")),
            }
        }
    }
    v
}

#[test]
fn streaming_matches_buffered_markdown() {
    let failures = &verdicts().streaming;
    assert!(
        failures.is_empty(),
        "{} streaming mismatch(es):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn streaming_with_options_matches_buffered_markdown() {
    let failures = &verdicts().streaming_with_options;
    assert!(
        failures.is_empty(),
        "{} streaming mismatch(es):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Sources whose pictures are metafiles (EMF/WMF) with text in them,
/// rasterized with the host's fonts (`backend/metafile.rs` loads the system
/// fonts): the PNG bytes differ where the font set does — a WSL host that
/// sees `C:\Windows\Fonts` draws the text in Arial, the fixture's host
/// drew it in a substitute. For these the embedded PNG payloads are left out
/// of the JSON / HTML comparison; everything else, the pictures' sizes
/// included, still has to match.
const HOST_FONT_PICTURES: &[&str] = &["doc/embedded_word_object.doc"];

/// `text` with every `data:image/png;base64,` payload replaced by `…`.
fn mask_png_payloads(text: &str) -> String {
    const PREFIX: &str = "data:image/png;base64,";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        out.push_str(&rest[..at + PREFIX.len()]);
        out.push('…');
        rest = rest[at + PREFIX.len()..].trim_start_matches(|c: char| {
            c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')
        });
    }
    out.push_str(rest);
    out
}

/// The masking drops the PNG payloads and nothing else.
#[test]
fn png_payload_masking_keeps_the_rest() {
    let a = r#"{"uri": "data:image/png;base64,iVBORw0+/=", "size": {"width": 2048.0}}"#;
    let b = r#"{"uri": "data:image/png;base64,QUJD", "size": {"width": 2048.0}}"#;
    let c = r#"{"uri": "data:image/png;base64,QUJD", "size": {"width": 1024.0}}"#;
    assert_eq!(mask_png_payloads(a), mask_png_payloads(b));
    assert_ne!(mask_png_payloads(b), mask_png_payloads(c));
    assert_eq!(
        mask_png_payloads(r#"<img src="data:image/png;base64,QUJD" width="9">"#),
        r#"<img src="data:image/png;base64,…" width="9">"#
    );
}

#[test]
fn outputs_match_fixtures() {
    let v = verdicts();
    if v.regen {
        eprintln!("regenerated fixtures for {} sources", v.sources);
        return;
    }
    assert!(
        v.fixtures.is_empty(),
        "{} regression failure(s):\n{}",
        v.fixtures.len(),
        v.fixtures.join("\n")
    );
}
