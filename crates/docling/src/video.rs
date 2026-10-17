//! Video frame sampling for [`InputFormat::Video`](crate::InputFormat::Video)
//! (#138 Phase 2).
//!
//! Symphonia demuxes only the *audio* track of a video container; decoding the
//! video track (h264/vp8/vp9/…) has no mature pure-Rust story, so frames come
//! from the `ffmpeg` **binary** — detected at runtime, no build-time
//! dependency. Without ffmpeg on `PATH` (override with `DOCLING_FFMPEG`) a
//! video still converts exactly as in Phase 1: transcript only.
//!
//! Sampling strategy, per file:
//! 1. **Scene changes** — a scan of the whole timeline (`select='gt(scene,
//!    0.27)'` with the scores printed, nothing written) lists every cut
//!    sharper than the threshold. This favors slides/scene boundaries in
//!    screen recordings and lectures. The first frame is always kept; with
//!    `max_frames − 1` or fewer cuts all of them follow. More cuts than that
//!    are spread over the duration (#648): the timeline is split into
//!    `max_frames` equal windows, the first window is the first frame, each
//!    other window keeps its sharpest cut (its midpoint when it has none) —
//!    the capped scan this replaces stopped at the `max_frames`-th cut, so a
//!    long lecture's eight frames all came from its first minutes.
//! 2. **Uniform fallback** — a static or single-scene video has no cut, in
//!    which case `fps=max_frames/duration` resamples the timeline evenly.
//!
//! The chosen frames are then decoded one at a time with an accurate seek
//! (`-ss` before `-i`), so a long file costs one scan plus `max_frames`
//! short decodes rather than a second full pass. Each decoded PNG is handed
//! to the caller's sink and its file deleted before the next seek, so one
//! frame is in flight at a time whatever `max_frames` is (#647) — the
//! converter's per-frame hook can OCR it and drop the bytes right there.
//!
//! Tunables ([`FrameOptions`], #647): the scene threshold (`0.27` — ffmpeg's
//! `scene` score of a frame against the previous one, 0–1), a cap on the
//! longer side applied inside ffmpeg's filter chain (`scale=…`) so a 1080p
//! recording's frames never reach memory at full size, and perceptual
//! de-duplication — a 64-bit difference hash of a 9×8 grey thumbnail; a
//! frame within the Hamming distance of any frame already kept is dropped
//! before it is embedded or OCR'd, which is what turns a slide re-lit by a
//! fade or a lecturer's camera cut back to the same slide into one picture.
//! `max_frames = ALL_FRAMES` keeps every distinct cut.
//!
//! Each extracted frame carries the source timestamp (`showinfo`'s
//! `pts_time`); the converter interleaves frames with the ASR transcript by
//! time and each becomes a picture node captioned `[time: <ts>]`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use docling_core::{DoclingDocument, Node, PictureImage};

/// How frames are sampled (#647). `Default` is the engine's behavior with
/// nothing set: eight frames, ffmpeg's scene score over 0.27, source
/// resolution, no de-duplication.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameOptions {
    /// At most this many frames; `0` = none (transcript only),
    /// [`ALL_FRAMES`] = every distinct cut.
    pub max_frames: usize,
    /// ffmpeg's `scene` score a frame must exceed to count as a cut, 0–1.
    pub scene_threshold: f32,
    /// Cap on a frame's longer side in pixels before it is encoded; `0` =
    /// the source resolution.
    pub max_side: u32,
    /// Drop a frame whose difference hash is within this Hamming distance
    /// (0–64) of a frame already kept; `None` keeps every sampled frame.
    pub dedupe: Option<u32>,
}

/// ffmpeg's default-equivalent scene threshold, the pre-#647 constant.
pub const DEFAULT_SCENE_THRESHOLD: f32 = 0.27;

/// `max_frames` meaning "every distinct keyframe" (#647): every cut over the
/// threshold is kept (de-duplication still applies); a video without a cut
/// yields its first frame alone rather than an even resample.
pub const ALL_FRAMES: usize = usize::MAX;

impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            max_frames: crate::DEFAULT_VIDEO_FRAMES,
            scene_threshold: DEFAULT_SCENE_THRESHOLD,
            max_side: 0,
            dedupe: None,
        }
    }
}

/// Convert a video: ASR transcript (Phase 1) interleaved by timestamp with up
/// to `opts.max_frames` sampled frames as timestamped pictures (Phase 2).
/// `on_frame` sees each frame's picture node as soon as it is decoded —
/// before the next frame is — so a hook that reads the image and drops its
/// bytes (picture OCR with `keep_picture_images(false)`, #645/#647) keeps
/// one frame in memory however many are sampled.
///
/// Degradation is deliberate and asymmetric: a frame-extraction failure (or no
/// ffmpeg binary) never blocks the transcript, while a video with *no audio
/// track* (screen capture, muted clip) still converts to its frames alone.
/// Only "both sides empty" propagates the ASR error.
pub fn convert_video(
    bytes: &[u8],
    name: &str,
    asr_model: Option<&str>,
    asr_lang: Option<&str>,
    opts: &FrameOptions,
    on_frame: &mut dyn FnMut(&mut Node),
) -> Result<DoclingDocument, String> {
    let mut frames: Vec<(f64, Node)> = Vec::new();
    if opts.max_frames > 0 && ffmpeg_available() {
        let outcome = extract_frames_with(bytes, name, opts, &mut |frame| {
            let ts = frame.ts;
            let mut node = picture_node(frame);
            on_frame(&mut node);
            frames.push((ts, node));
            Ok(())
        });
        if let Err(e) = outcome {
            eprintln!("warning: {e}; continuing with the transcript only");
            frames.clear();
        }
    }
    let segments = match docling_asr::transcribe_with_options(bytes, name, asr_model, asr_lang) {
        Ok(segments) => segments,
        Err(e) if !frames.is_empty() && e.0.contains("no decodable audio track") => Vec::new(),
        Err(e) => return Err(e.0),
    };

    let mut doc = DoclingDocument::new(name);
    let mut frames = frames.into_iter().peekable();
    for seg in &segments {
        // A frame sampled at (or before) the segment's start is what the
        // viewer sees as that speech begins — picture first.
        while frames.peek().is_some_and(|(ts, _)| *ts <= seg.start) {
            doc.nodes.push(frames.next().unwrap().1);
        }
        doc.nodes.push(docling_asr::segment_node(seg));
    }
    for (_, node) in frames {
        doc.nodes.push(node);
    }
    Ok(doc)
}

/// A sampled frame as a picture node: `[time: <ts>]` caption (matching the
/// transcript's paragraph convention) and the PNG embedded for JSON/DCLX
/// export (Markdown renders its usual placeholder).
fn picture_node(frame: VideoFrame) -> Node {
    let (width, height) = png_size(&frame.png).unwrap_or((0, 0));
    Node::Picture {
        caption: Some(format!("[time: {}]", docling_asr::fmt_seconds(frame.ts))),
        caption_href: None,
        image: Some(PictureImage {
            dpi: PictureImage::DEFAULT_DPI,
            mimetype: "image/png".to_string(),
            width,
            height,
            data: frame.png,
        }),
        classification: None,
        description: None,
        caption_parent: Default::default(),
        caption_location: None,
    }
}

/// Pixel size from a PNG IHDR (always the first chunk, at fixed offsets).
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Some((be(&png[16..20]), be(&png[20..24])))
}

/// One sampled frame: PNG bytes plus the source-timeline timestamp in seconds.
pub struct VideoFrame {
    pub ts: f64,
    pub png: Vec<u8>,
}

/// The ffmpeg invocation to use: `DOCLING_FFMPEG` if set, else `ffmpeg` from
/// `PATH`.
fn ffmpeg_bin() -> String {
    docling_core::env::nonempty("DOCLING_FFMPEG").unwrap_or_else(|| "ffmpeg".to_string())
}

/// Whether the ffmpeg binary is runnable. Checked once per call site — the
/// converter probes before extracting and silently skips frames when absent.
pub fn ffmpeg_available() -> bool {
    Command::new(ffmpeg_bin())
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Sample frames from a video (bytes + name, the extension hinting the
/// container), collected — the memory-bounded form is
/// [`extract_frames_with`]. `Ok(vec![])` means "no video track"; `Err`
/// carries the ffmpeg failure. Timestamps ascend.
pub fn extract_frames(
    bytes: &[u8],
    name: &str,
    opts: &FrameOptions,
) -> Result<Vec<VideoFrame>, String> {
    let mut frames = Vec::new();
    extract_frames_with(bytes, name, opts, &mut |f| {
        frames.push(f);
        Ok(())
    })?;
    Ok(frames)
}

/// Sample frames from a video, handing each to `sink` as it is decoded and
/// deleting its file before the next one is decoded: at most one frame's
/// bytes exist outside the sink at any time (#647). Returns how many frames
/// the sink saw. `Ok(0)` for no video track (or `max_frames == 0`); `Err`
/// is the ffmpeg failure, or the sink's.
pub fn extract_frames_with(
    bytes: &[u8],
    name: &str,
    opts: &FrameOptions,
    sink: &mut dyn FnMut(VideoFrame) -> Result<(), String>,
) -> Result<usize, String> {
    if opts.max_frames == 0 {
        return Ok(0);
    }
    let dir = TempDir::new()?;
    // ffmpeg needs a seekable input; piping via stdin breaks on mp4/mov files
    // whose moov atom trails the mdat. Keep the caller's extension as the
    // container hint (content sniffing still decides).
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    let input = dir.path.join(format!("input.{ext}"));
    std::fs::File::create(&input)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| format!("video: writing temp file: {e}"))?;
    extract_into(&dir, &input, opts, sink)
}

/// The sampling over an input file already in `dir` (the unit tests spy on
/// the directory from the sink).
fn extract_into(
    dir: &TempDir,
    input: &Path,
    opts: &FrameOptions,
    sink: &mut dyn FnMut(VideoFrame) -> Result<(), String>,
) -> Result<usize, String> {
    let probe = probe(input)?;
    if !probe.has_video {
        return Ok(0);
    }
    let duration = probe.duration.filter(|d| *d > 0.0);
    let mut dedupe = Deduper::new(opts.dedupe);
    let mut seen = 0usize;
    let mut emit = |frame: VideoFrame, sink: &mut dyn FnMut(VideoFrame) -> Result<(), String>| {
        if dedupe.is_duplicate(&frame.png) {
            return Ok(());
        }
        seen += 1;
        sink(frame)
    };

    // Pass 1: every cut of the whole timeline, with its score.
    let cuts = scene_cuts(input, opts.scene_threshold)?;
    // No cut at all: a static or single-scene video — resample evenly
    // (every-cut mode has nothing to resample to: the first frame alone).
    if cuts.is_empty() && opts.max_frames >= 2 && opts.max_frames != ALL_FRAMES {
        if let Some(duration) = duration {
            let filter = match scale_filter(opts.max_side) {
                Some(scale) => format!("fps={}/{duration},{scale},showinfo", opts.max_frames),
                None => format!("fps={}/{duration},showinfo", opts.max_frames),
            };
            run_filter(dir, input, &filter, opts.max_frames, "uniform", &mut |f| {
                emit(f, sink)
            })?;
            return Ok(seen);
        }
    }
    // Pass 2: decode the chosen frames, one accurate seek each.
    let chosen = choose_timestamps(&cuts, duration, opts.max_frames);
    for (i, ts) in chosen.into_iter().enumerate() {
        if let Some(png) = frame_at(dir, input, ts, i, opts.max_side)? {
            emit(VideoFrame { ts, png }, sink)?;
        }
    }
    Ok(seen)
}

/// The `scale` filter capping the longer side at `max_side` (#647): the
/// other side follows the aspect ratio (`-1`, so a frame already within the
/// cap keeps its exact size — `-2` would round an odd height). `None` for
/// `0` = source resolution, which keeps the default filter chain, and with
/// it the default output, untouched.
fn scale_filter(max_side: u32) -> Option<String> {
    (max_side > 0).then(|| {
        format!(
            "scale=w='if(gte(iw,ih),min(iw,{max_side}),-1)':h='if(gte(iw,ih),-1,min(ih,{max_side}))'"
        )
    })
}

/// Perceptual de-duplication (#647): the 64-bit difference hash of each
/// kept frame; a new frame whose hash is within `max_distance` of any of
/// them is a duplicate. `None` disables it. The hash sees structure, not
/// colour — a fade or a lighting change leaves it alone, which is the
/// point, and two featureless frames (a blank slide after a blank slide)
/// hash alike whatever their colour.
struct Deduper {
    max_distance: Option<u32>,
    kept: Vec<u64>,
}

impl Deduper {
    fn new(max_distance: Option<u32>) -> Self {
        Self {
            max_distance,
            kept: Vec::new(),
        }
    }

    fn is_duplicate(&mut self, png: &[u8]) -> bool {
        let Some(max) = self.max_distance else {
            return false;
        };
        // An undecodable frame is kept: dropping it would hide a decoder
        // problem behind "deduplicated".
        let Some(hash) = dhash(png) else {
            return false;
        };
        if self.kept.iter().any(|k| (k ^ hash).count_ones() <= max) {
            return true;
        }
        self.kept.push(hash);
        false
    }
}

/// The difference hash of an encoded image: a 9×8 greyscale thumbnail,
/// one bit per horizontally adjacent pair (left brighter than right). Robust
/// to a fade's uniform brightness change and to re-encoding, which is what
/// a re-shown slide looks like.
pub fn dhash(encoded: &[u8]) -> Option<u64> {
    let img = image::load_from_memory(encoded).ok()?.to_luma8();
    let thumb = image::imageops::resize(&img, 9, 8, image::imageops::FilterType::Triangle);
    let mut hash = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            hash <<= 1;
            if thumb.get_pixel(x, y).0[0] > thumb.get_pixel(x + 1, y).0[0] {
                hash |= 1;
            }
        }
    }
    Some(hash)
}

/// The frames to keep (#648): the first frame, then — with `max_frames − 1`
/// or fewer cuts — every cut; with more, the sharpest cut of each of the
/// `max_frames` equal windows the duration splits into (the first window is
/// the first frame; a window without a cut contributes its midpoint, so a
/// cluster of cuts never empties the rest of the timeline). Without a known
/// duration, the first `max_frames − 1` cuts — the capped scan's choice.
/// Timestamps ascend.
fn choose_timestamps(cuts: &[(f64, f64)], duration: Option<f64>, max_frames: usize) -> Vec<f64> {
    let mut out = vec![0.0];
    if max_frames <= 1 {
        return out;
    }
    if cuts.len() < max_frames {
        out.extend(cuts.iter().map(|&(ts, _)| ts));
        return out;
    }
    let Some(duration) = duration else {
        out.extend(cuts.iter().take(max_frames - 1).map(|&(ts, _)| ts));
        return out;
    };
    let window = duration / max_frames as f64;
    for k in 1..max_frames {
        let (lo, hi) = (window * k as f64, window * (k + 1) as f64);
        let best = cuts
            .iter()
            .filter(|(ts, _)| *ts >= lo && *ts < hi)
            // The sharpest cut; the earliest of equal scores.
            .fold(None::<(f64, f64)>, |acc, &(ts, score)| match acc {
                Some((_, s)) if s >= score => acc,
                _ => Some((ts, score)),
            });
        out.push(best.map_or((lo + hi) / 2.0, |(ts, _)| ts));
    }
    out
}

/// Scan the whole video for scene changes: `(pts_time, scene score)` of
/// every frame sharper than the threshold, in order, nothing written (`-f
/// null`). `select` tags each kept frame with `lavfi.scene_score`, which
/// `metadata=print` logs as a `pts_time:` line followed by the score line.
fn scene_cuts(input: &Path, threshold: f32) -> Result<Vec<(f64, f64)>, String> {
    let out = Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(input)
        .arg("-vf")
        .arg(format!(
            "select='gt(scene,{threshold})',metadata=print:key=lavfi.scene_score"
        ))
        .args(["-fps_mode", "vfr", "-f", "null", "-"])
        .output()
        .map_err(|e| format!("video: running ffmpeg: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        return Err(format!("video: ffmpeg scene scan failed: {last}"));
    }
    let mut cuts = Vec::new();
    let mut pending: Option<f64> = None;
    for line in stderr.lines() {
        let Some(rest) = line.split("Parsed_metadata").nth(1) else {
            continue;
        };
        if let Some(ts) = rest.split("pts_time:").nth(1) {
            pending = ts.split_whitespace().next().and_then(|v| v.parse().ok());
        } else if let Some(score) = rest.split("lavfi.scene_score=").nth(1) {
            if let (Some(ts), Some(score)) = (
                pending.take(),
                score
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok()),
            ) {
                cuts.push((ts, score));
            }
        }
    }
    Ok(cuts)
}

/// Decode the frame at `ts` seconds (the first frame at or after it — an
/// accurate seek, `-ss` ahead of `-i`) into one PNG, scaled to `max_side`
/// when set; `None` past the end. The file is deleted once read.
fn frame_at(
    dir: &TempDir,
    input: &Path,
    ts: f64,
    index: usize,
    max_side: u32,
) -> Result<Option<Vec<u8>>, String> {
    let path = dir.path.join(format!("frame_{index:04}.png"));
    let mut cmd = Command::new(ffmpeg_bin());
    cmd.args(["-hide_banner", "-nostats"]);
    if ts > 0.0 {
        cmd.arg("-ss").arg(format!("{ts:.6}"));
    }
    cmd.arg("-i").arg(input);
    if let Some(scale) = scale_filter(max_side) {
        cmd.arg("-vf").arg(scale);
    }
    let out = cmd
        .args(["-frames:v", "1", "-f", "image2", "-y"])
        .arg(&path)
        .output()
        .map_err(|e| format!("video: running ffmpeg: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        return Err(format!("video: ffmpeg frame extraction failed: {last}"));
    }
    let png = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    Ok(png)
}

struct Probe {
    has_video: bool,
    duration: Option<f64>,
}

/// `ffmpeg -i <file>` with no output exits non-zero but prints the stream
/// layout; parse `Duration:` and whether a `Video:` stream exists.
fn probe(input: &Path) -> Result<Probe, String> {
    let out = Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-i"])
        .arg(input)
        .output()
        .map_err(|e| format!("video: running ffmpeg: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let has_video = stderr
        .lines()
        .any(|l| l.trim_start().starts_with("Stream #") && l.contains("Video:"));
    let duration = stderr.lines().find_map(|l| {
        let rest = l.trim_start().strip_prefix("Duration: ")?;
        let hms = rest.split(',').next()?.trim();
        let mut parts = hms.split(':');
        let h: f64 = parts.next()?.parse().ok()?;
        let m: f64 = parts.next()?.parse().ok()?;
        let s: f64 = parts.next()?.parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    });
    Ok(Probe {
        has_video,
        duration,
    })
}

/// Run one extraction pass: `-vf <filter>` (which must end in `showinfo`),
/// writing at most `max_frames` PNGs, and hand each emitted file to `sink`
/// with the source timestamp `showinfo` logs (`pts_time:<seconds>`),
/// deleting it once read.
fn run_filter(
    dir: &TempDir,
    input: &Path,
    filter: &str,
    max_frames: usize,
    tag: &str,
    sink: &mut dyn FnMut(VideoFrame) -> Result<(), String>,
) -> Result<(), String> {
    let pattern = dir.path.join(format!("{tag}_%04d.png"));
    let out = Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-i"])
        .arg(input)
        .args(["-vf", filter, "-fps_mode", "vfr", "-frames:v"])
        .arg(max_frames.to_string())
        .args(["-f", "image2", "-y"])
        .arg(&pattern)
        .output()
        .map_err(|e| format!("video: running ffmpeg: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        return Err(format!("video: ffmpeg frame extraction failed: {last}"));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let timestamps: Vec<f64> = stderr
        .lines()
        .filter(|l| l.contains("Parsed_showinfo"))
        .filter_map(|l| {
            let rest = l.split("pts_time:").nth(1)?;
            rest.split_whitespace().next()?.parse().ok()
        })
        .collect();
    for i in 1..=max_frames {
        let path = dir.path.join(format!("{tag}_{i:04}.png"));
        let Ok(png) = std::fs::read(&path) else { break };
        let _ = std::fs::remove_file(&path);
        // showinfo logs one line per selected frame, in emit order; a missing
        // timestamp (log truncation) falls back to 0 rather than dropping the
        // frame.
        let ts = timestamps.get(i - 1).copied().unwrap_or(0.0);
        sink(VideoFrame { ts, png })?;
    }
    Ok(())
}

/// Minimal scoped temp dir (no `tempfile` dependency): unique per process ×
/// counter, best-effort removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Result<Self, String> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "docling-video-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).map_err(|e| format!("video: creating temp dir: {e}"))?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/data/audio/sources")
            .join(name);
        std::fs::read(path).expect("fixture exists")
    }

    /// The defaults with a frame cap — the pre-#647 call shape.
    fn cap(max_frames: usize) -> FrameOptions {
        FrameOptions {
            max_frames,
            ..FrameOptions::default()
        }
    }

    /// PNG magic on every frame, ascending timestamps, count within cap.
    fn assert_frames(frames: &[VideoFrame], max: usize) {
        assert!(!frames.is_empty(), "no frames extracted");
        assert!(frames.len() <= max);
        for w in frames.windows(2) {
            assert!(w[0].ts <= w[1].ts, "timestamps not ascending");
        }
        for f in frames {
            assert_eq!(&f.png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
        }
    }

    #[test]
    fn extracts_frames_from_mp4() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let frames =
            extract_frames(&fixture("sample_10s_video-mp4.mp4"), "s.mp4", &cap(8)).unwrap();
        assert_frames(&frames, 8);
        // The fixture is a single continuous scene → uniform sampling kicks in
        // and spreads frames across the ~10 s timeline.
        assert!(
            frames.len() >= 4,
            "expected uniform sampling, got {}",
            frames.len()
        );
        assert!(frames.last().unwrap().ts > 5.0);
    }

    #[test]
    fn extracts_frames_from_webm() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let frames =
            extract_frames(&fixture("sample_10s_video-webm.webm"), "s.webm", &cap(4)).unwrap();
        assert_frames(&frames, 4);
    }

    #[test]
    fn audio_only_yields_no_frames() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let frames =
            extract_frames(&fixture("sample_10s_audio-mp3.mp3"), "s.mp3", &cap(8)).unwrap();
        assert!(frames.is_empty(), "audio file must yield no frames");
    }

    #[test]
    fn zero_max_frames_disables_extraction() {
        let frames = extract_frames(b"not a video", "s.mp4", &cap(0)).unwrap();
        assert!(frames.is_empty());
    }

    /// A `seconds`-long 64×64 video that changes colour every `segment`
    /// seconds (a cut per boundary), synthesized by ffmpeg — no fixture.
    fn colour_cuts_video(seconds: u32, segment: u32) -> Vec<u8> {
        let dir = TempDir::new().unwrap();
        let out = dir.path.join("cuts.mp4");
        let status = Command::new(ffmpeg_bin())
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("color=c=black:s=64x64:r=10:d={seconds}"))
            .args(["-vf"])
            .arg(format!(
                "geq=r='255*mod(floor(T/{segment})\\,2)':g='128*mod(floor(T/{segment})\\,3)':b='255*mod(floor(T/{segment})+1\\,2)'"
            ))
            .args(["-c:v", "mpeg4", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(&out).unwrap()
    }

    /// #648: more cuts than the cap are spread over the whole duration —
    /// twenty 5-second scenes sampled as four frames land in four different
    /// quarters, where the capped scan took the first three cuts.
    #[test]
    fn capped_scene_sampling_spans_the_whole_video() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let frames = extract_frames(&colour_cuts_video(100, 5), "cuts.mp4", &cap(4)).unwrap();
        assert_frames(&frames, 4);
        let ts: Vec<f64> = frames.iter().map(|f| f.ts).collect();
        assert_eq!(ts.len(), 4, "{ts:?}");
        assert_eq!(ts[0], 0.0, "{ts:?}");
        let quarters: Vec<usize> = ts.iter().map(|t| (t / 25.0).floor() as usize).collect();
        assert_eq!(quarters, vec![0, 1, 2, 3], "{ts:?}");
        // Each later frame sits on a cut (a 5-second boundary).
        for t in &ts[1..] {
            assert!((t % 5.0).abs() < 0.2, "{ts:?}");
        }
    }

    /// A cap at or above the cut count keeps the first frame and every cut,
    /// as before.
    #[test]
    fn a_cap_above_the_cut_count_keeps_every_cut() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let frames = extract_frames(&colour_cuts_video(20, 5), "cuts.mp4", &cap(8)).unwrap();
        let ts: Vec<f64> = frames.iter().map(|f| f.ts).collect();
        assert_eq!(ts.len(), 4, "{ts:?}");
        assert_eq!(ts[0], 0.0);
        for (t, cut) in ts[1..].iter().zip([5.0, 10.0, 15.0]) {
            assert!((t - cut).abs() < 0.2, "{ts:?}");
        }
    }

    /// The selection rule on its own: all cuts under the cap, windows with
    /// the sharpest cut (or the midpoint) above it, the capped-scan choice
    /// without a duration.
    #[test]
    fn timestamp_selection_rules() {
        let cuts = [
            (5.0, 0.9),
            (7.0, 0.5),
            (12.0, 0.6),
            (30.0, 0.95),
            (31.0, 0.3),
        ];
        assert_eq!(
            choose_timestamps(&cuts, Some(40.0), 8),
            vec![0.0, 5.0, 7.0, 12.0, 30.0, 31.0]
        );
        // Four windows of 10 s: (10,20) → 12.0, (20,30) → midpoint 25, (30,40) → 30.0.
        assert_eq!(
            choose_timestamps(&cuts, Some(40.0), 4),
            vec![0.0, 12.0, 25.0, 30.0]
        );
        assert_eq!(choose_timestamps(&cuts, None, 4), vec![0.0, 5.0, 7.0, 12.0]);
        assert_eq!(choose_timestamps(&cuts, Some(40.0), 1), vec![0.0]);
        assert_eq!(choose_timestamps(&[], Some(40.0), 4), vec![0.0]);
    }

    /// A 64×64 clip whose grey level follows `levels` (one entry per
    /// `segment` seconds), synthesized by ffmpeg; `rect` additionally draws
    /// a black rectangle on every frame (the "slide content" the
    /// de-duplication must recognize under a different lighting).
    fn grey_steps_video(levels: &[u32], segment: u32, rect: bool) -> Vec<u8> {
        let dir = TempDir::new().unwrap();
        let out = dir.path.join("steps.mp4");
        let seconds = levels.len() as u32 * segment;
        // Nested `if(lt(T, k*segment), level_k, …)` — the last level past
        // the final boundary.
        let mut expr = levels.last().unwrap().to_string();
        for (k, level) in levels.iter().enumerate().rev().skip(1) {
            expr = format!(
                "if(lt(T\\,{})\\,{level}\\,{expr})",
                (k as u32 + 1) * segment
            );
        }
        if rect {
            expr = format!("if(between(X\\,16\\,48)*between(Y\\,24\\,40)\\,0\\,{expr})");
        }
        let status = Command::new(ffmpeg_bin())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg(format!("color=c=black:s=64x64:r=10:d={seconds}"))
            .args(["-vf"])
            .arg(format!("geq=r='{expr}':g='{expr}':b='{expr}'"))
            .args(["-c:v", "mpeg4", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(&out).unwrap()
    }

    /// #647: the scene threshold. White → light grey is a soft cut (ffmpeg
    /// scores it around 0.5), light grey → black a hard one (≈1.0): the
    /// default keeps both, 0.6 only the hard cut.
    #[test]
    fn scene_threshold_keeps_only_hard_cuts() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let clip = grey_steps_video(&[255, 192, 0], 3, false);
        let soft_and_hard = extract_frames(&clip, "steps.mp4", &cap(8)).unwrap();
        let ts: Vec<f64> = soft_and_hard.iter().map(|f| f.ts).collect();
        assert_eq!(ts.len(), 3, "default threshold: {ts:?}");
        let hard_only = extract_frames(
            &clip,
            "steps.mp4",
            &FrameOptions {
                scene_threshold: 0.6,
                ..cap(8)
            },
        )
        .unwrap();
        let ts: Vec<f64> = hard_only.iter().map(|f| f.ts).collect();
        assert_eq!(ts.len(), 2, "threshold 0.6: {ts:?}");
        assert!((ts[1] - 6.0).abs() < 0.2, "the hard cut at 6 s: {ts:?}");
    }

    /// #647: `max_side` scales inside ffmpeg — a 1920×1080 source at 640
    /// yields 640×360 PNGs, smaller than the full-size ones; `0` leaves the
    /// source size.
    #[test]
    fn max_side_downscales_frames() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        let out = dir.path.join("hd.mp4");
        let status = Command::new(ffmpeg_bin())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("testsrc=size=1920x1080:rate=10:duration=1")
            .args(["-c:v", "mpeg4", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        let clip = std::fs::read(&out).unwrap();
        let full = extract_frames(&clip, "hd.mp4", &cap(1)).unwrap();
        assert_eq!(png_size(&full[0].png), Some((1920, 1080)));
        let small = extract_frames(
            &clip,
            "hd.mp4",
            &FrameOptions {
                max_side: 640,
                ..cap(1)
            },
        )
        .unwrap();
        assert_eq!(png_size(&small[0].png), Some((640, 360)));
        assert!(small[0].png.len() < full[0].png.len());
        // A cap above the source size changes nothing.
        let same = extract_frames(
            &clip,
            "hd.mp4",
            &FrameOptions {
                max_side: 4000,
                ..cap(1)
            },
        )
        .unwrap();
        assert_eq!(png_size(&same[0].png), Some((1920, 1080)));
    }

    /// #647: de-duplication. The same slide (a black box) shown three times
    /// under white, grey and white lighting is three scene cuts; its
    /// difference hash is the same under each, so a distance of 5 keeps one
    /// frame and `None` keeps all three.
    #[test]
    fn dedupe_collapses_a_relit_slide() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let clip = grey_steps_video(&[255, 144, 255], 2, true);
        let all = extract_frames(&clip, "slide.mp4", &cap(8)).unwrap();
        assert_eq!(all.len(), 3, "three lighting changes = three cuts");
        let hashes: Vec<u64> = all.iter().map(|f| dhash(&f.png).unwrap()).collect();
        assert!(
            hashes.windows(2).all(|w| (w[0] ^ w[1]).count_ones() <= 5),
            "the relit slide hashes alike: {hashes:x?}"
        );
        let one = extract_frames(
            &clip,
            "slide.mp4",
            &FrameOptions {
                dedupe: Some(5),
                ..cap(8)
            },
        )
        .unwrap();
        assert_eq!(one.len(), 1, "de-duplicated to the first showing");
        assert_eq!(one[0].ts, 0.0);
        // A different slide (the box elsewhere) survives the same distance.
        let other = grey_steps_video(&[255], 2, false);
        let mut both = Vec::new();
        for clip in [&clip, &other] {
            both.push(dhash(&extract_frames(clip, "s.mp4", &cap(1)).unwrap()[0].png).unwrap());
        }
        assert!((both[0] ^ both[1]).count_ones() > 5, "{both:x?}");
    }

    /// #647: `max_frames = ALL_FRAMES` keeps every cut; a cut-less clip
    /// yields its first frame alone rather than an even resample.
    #[test]
    fn all_frames_keeps_every_cut() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        // Not every colour step scores over the default threshold in YUV,
        // so the reference is the uncapped count a generous cap yields.
        let clip = colour_cuts_video(60, 3);
        let generous = extract_frames(&clip, "cuts.mp4", &cap(1000)).unwrap();
        assert!(
            generous.len() > crate::DEFAULT_VIDEO_FRAMES,
            "{}",
            generous.len()
        );
        let frames = extract_frames(&clip, "cuts.mp4", &cap(ALL_FRAMES)).unwrap();
        assert_eq!(frames.len(), generous.len(), "every cut, no cap");
        assert!(frames.windows(2).all(|w| w[0].ts < w[1].ts));
        let one = extract_frames(
            &fixture("sample_10s_video-mp4.mp4"),
            "s.mp4",
            &cap(ALL_FRAMES),
        )
        .unwrap();
        assert_eq!(one.len(), 1, "no cut: the first frame only");
        assert_eq!(one[0].ts, 0.0);
    }

    /// #647: one frame in flight. The sink is called with each frame after
    /// its file is deleted and before the next is decoded, so a spy on the
    /// working directory never sees a decoded frame on disk, and the sink
    /// owns the only copy of the bytes.
    #[test]
    fn one_frame_in_flight() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not on PATH");
            return;
        }
        let dir = TempDir::new().unwrap();
        let input = dir.path.join("input.mp4");
        std::fs::write(&input, colour_cuts_video(40, 2)).unwrap();
        let mut seen = 0usize;
        let mut on_disk_max = 0usize;
        let n = extract_into(&dir, &input, &cap(ALL_FRAMES), &mut |frame| {
            seen += 1;
            assert!(!frame.png.is_empty());
            let pngs = std::fs::read_dir(&dir.path)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|x| x == "png")
                })
                .count();
            on_disk_max = on_disk_max.max(pngs);
            Ok(())
        })
        .unwrap();
        assert_eq!(n, seen);
        assert!(n > crate::DEFAULT_VIDEO_FRAMES, "{n} frames");
        assert_eq!(
            on_disk_max, 0,
            "a frame's file is gone before the sink sees it"
        );
    }

    /// Whether Whisper-tiny is reachable, pointing `DOCLING_ASR_*` at the
    /// workspace-root `.models/asr/` when running from the crate dir (model
    /// resolution is CWD-relative).
    fn asr_models_ready() -> bool {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.models/asr");
        if root.join("encoder_model.onnx").exists() {
            std::env::set_var("DOCLING_ASR_ENCODER", root.join("encoder_model.onnx"));
            std::env::set_var("DOCLING_ASR_DECODER", root.join("decoder_model.onnx"));
            std::env::set_var("DOCLING_ASR_VOCAB", root.join("vocab.json"));
            return true;
        }
        docling_asr::models_available()
    }

    #[test]
    fn video_document_interleaves_frames_with_transcript() {
        if !ffmpeg_available() || !asr_models_ready() {
            eprintln!("skipping: needs ffmpeg and the Whisper models");
            return;
        }
        let doc = convert_video(
            &fixture("sample_10s_video-mkv.mkv"),
            "s.mkv",
            None,
            None,
            &cap(4),
            &mut |_| {},
        )
        .unwrap();
        let mut pictures = 0;
        let mut paragraphs = 0;
        for node in &doc.nodes {
            match node {
                Node::Picture { caption, image, .. } => {
                    pictures += 1;
                    assert!(caption.as_deref().unwrap_or("").starts_with("[time: "));
                    let img = image.as_ref().expect("frame is embedded");
                    assert_eq!(img.mimetype, "image/png");
                    assert_eq!((img.width, img.height), (64, 64));
                }
                Node::Paragraph { text } => {
                    paragraphs += 1;
                    assert!(text.starts_with("[time: "));
                }
                // Transcript segments are timed items (#681): the cue's
                // span on the track, the `[time: …]` paragraph inside.
                Node::Track { track, inner, .. } => {
                    paragraphs += 1;
                    assert!(track.end_time > track.start_time, "{track:?}");
                    assert!(
                        matches!(inner.as_ref(), Node::Paragraph { text } if text.starts_with("[time: ")),
                        "{inner:?}"
                    );
                }
                other => panic!("unexpected node {other:?}"),
            }
        }
        assert!(pictures >= 2, "expected sampled frames, got {pictures}");
        assert!(paragraphs >= 1, "expected transcript paragraphs");
        // The frame at t=0 precedes the first transcript segment.
        assert!(matches!(doc.nodes.first(), Some(Node::Picture { .. })));
    }

    /// A video with no audio track still converts — to its frames alone.
    #[test]
    fn video_without_audio_track_converts_to_frames_only() {
        if !ffmpeg_available() || !asr_models_ready() {
            eprintln!("skipping: needs ffmpeg and the Whisper models");
            return;
        }
        // Strip the audio track off a fixture on the fly (`-an -c copy`)
        // rather than committing another binary fixture.
        let dir = TempDir::new().unwrap();
        let src = dir.path.join("in.mkv");
        let out = dir.path.join("noaudio.mkv");
        std::fs::write(&src, fixture("sample_10s_video-mkv.mkv")).unwrap();
        let status = Command::new(ffmpeg_bin())
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(&src)
            .args(["-an", "-c", "copy", "-y"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        let doc = convert_video(
            &std::fs::read(&out).unwrap(),
            "noaudio.mkv",
            None,
            None,
            &cap(4),
            &mut |_| {},
        )
        .unwrap();
        assert!(!doc.nodes.is_empty());
        assert!(doc.nodes.iter().all(|n| matches!(n, Node::Picture { .. })));
    }
}
