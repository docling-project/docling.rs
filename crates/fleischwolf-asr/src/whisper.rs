//! Whisper inference: ONNX encoder + autoregressive decoder with greedy
//! sampling and OpenAI's timestamp rules — a port of the `temperature=0` path
//! of `whisper/transcribe.py` + `whisper/decoding.py` (docling's ASR defaults:
//! greedy, timestamps on).
//!
//! The decoder is the plain (cache-less) export: each step re-runs the whole
//! prefix. For the tiny model and conversation-length audio that is a few
//! seconds of compute — simplicity over KV-cache plumbing (the same trade the
//! TableFormer port started with).

use ort::session::Session;
use ort::value::{Tensor, TensorRef};

use crate::mel::{log_mel_spectrogram, N_FRAMES, N_SAMPLES};
use crate::tokenizer::Tokenizer;

// Multilingual Whisper special tokens (vocab 51865).
const EOT: u32 = 50_257;
const SOT: u32 = 50_258;
const LANG_EN: u32 = 50_259;
const TRANSCRIBE: u32 = 50_359;
const NO_SPEECH: u32 = 50_362;
const TS_BEGIN: u32 = 50_364;
/// Space token (`Ġ`), suppressed as the first sampled token (`suppress_blank`).
const SPACE: u32 = 220;
/// Max new tokens per window (docling's `max_new_tokens=256` capped by
/// Whisper's own `n_text_ctx / 2 = 224`).
const SAMPLE_LEN: usize = 224;
/// `max_initial_timestamp` = 1.0 s → 50 timestamp steps of 0.02 s.
const MAX_INITIAL_TS: u32 = 50;
/// Seconds per timestamp token step.
const TIME_PRECISION: f64 = 0.02;
/// Mel frames per second (hop 160 at 16 kHz).
const FRAMES_PER_SECOND: f64 = 100.0;

/// One transcribed segment, in seconds from the start of the audio.
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

pub struct Transcriber {
    encoder: Session,
    decoder: Session,
    tokenizer: Tokenizer,
    lang_token: u32,
    /// OpenAI's default `suppress_tokens="-1"` non-speech symbol set.
    suppress: Vec<u32>,
}

fn model_path(var: &str, default: &str) -> std::path::PathBuf {
    if let Ok(p) = std::env::var(var) {
        return p.into();
    }
    // Default is CWD-relative; fall back to the executable's directory and one
    // level above it (the `scripts/install.sh` layout, reached through the
    // /usr/local/bin symlink), mirroring fleischwolf-pdf's asset resolution.
    if !std::path::Path::new(default).exists() {
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        {
            for base in [Some(dir.as_path()), dir.parent()].into_iter().flatten() {
                let p = base.join(default);
                if p.exists() {
                    return p;
                }
            }
        }
    }
    default.to_string().into()
}

/// Whether the Whisper model files are present (so callers can fail with a
/// clear "models missing" message instead of an opaque load error).
pub fn models_available() -> bool {
    model_path("DOCLING_ASR_ENCODER", "models/asr/encoder_model.onnx").exists()
        && model_path("DOCLING_ASR_DECODER", "models/asr/decoder_model.onnx").exists()
        && model_path("DOCLING_ASR_VOCAB", "models/asr/vocab.json").exists()
}

impl Transcriber {
    /// Load the encoder/decoder ONNX graphs and the vocabulary. Paths come from
    /// `DOCLING_ASR_{ENCODER,DECODER,VOCAB}`, defaulting to `models/asr/…`
    /// relative to the working directory (mirroring the PDF models).
    pub fn load() -> Result<Self, String> {
        let session = |path: std::path::PathBuf| -> Result<Session, String> {
            Session::builder()
                .map_err(|e| format!("asr: builder: {e}"))?
                .with_intra_threads(
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(1),
                )
                .map_err(|e| format!("asr: threads: {e}"))?
                .commit_from_file(&path)
                .map_err(|e| format!("asr: loading {}: {e}", path.display()))
        };
        let encoder = session(model_path(
            "DOCLING_ASR_ENCODER",
            "models/asr/encoder_model.onnx",
        ))?;
        let decoder = session(model_path(
            "DOCLING_ASR_DECODER",
            "models/asr/decoder_model.onnx",
        ))?;
        let tokenizer = Tokenizer::load(&model_path("DOCLING_ASR_VOCAB", "models/asr/vocab.json"))?;
        let suppress = tokenizer.non_speech_tokens();
        Ok(Self {
            encoder,
            decoder,
            tokenizer,
            lang_token: language_token(),
            suppress,
        })
    }

    /// Transcribe 16 kHz mono samples into timed segments, windowing the audio
    /// in 30-second chunks the way `whisper/transcribe.py` does (seek advances
    /// to the last emitted timestamp).
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Vec<Segment>, String> {
        // Pad a full extra window so the tail still sees 30 s of context.
        let (mel, total_frames) = log_mel_spectrogram(samples, N_SAMPLES);
        let content_frames = total_frames.saturating_sub(N_FRAMES);
        let content_duration = samples.len() as f64 / crate::audio::SAMPLE_RATE as f64;

        let mut segments = Vec::new();
        let mut seek = 0usize;
        while seek < content_frames.max(1) {
            let window = window_mel(&mel, total_frames, seek);
            let hidden = self.encode(&window)?;
            let out = self.decode_window(&hidden)?;

            let time_offset = seek as f64 / FRAMES_PER_SECOND;
            let segment_duration =
                (content_duration - time_offset).min(N_FRAMES as f64 / FRAMES_PER_SECOND);

            // transcribe.py's silence gate: skip the window when the model is
            // confident it is non-speech AND the transcription scores poorly.
            if out.no_speech_prob > 0.6 && out.avg_logprob < -1.0 {
                seek += N_FRAMES;
                continue;
            }

            let advance =
                self.emit_segments(&out.tokens, time_offset, segment_duration, &mut segments);
            seek += advance;
            if content_frames == 0 {
                break; // sub-window clip: single pass
            }
        }
        Ok(segments)
    }

    /// Split a window's sampled tokens into `[ts … text … ts]` segments
    /// (`whisper/transcribe.py`), append them, and return how many mel frames
    /// to advance.
    fn emit_segments(
        &self,
        tokens: &[u32],
        time_offset: f64,
        segment_duration: f64,
        segments: &mut Vec<Segment>,
    ) -> usize {
        let is_ts = |t: u32| t >= TS_BEGIN;
        let ts_sec = |t: u32| (t - TS_BEGIN) as f64 * TIME_PRECISION;
        let push = |segments: &mut Vec<Segment>, start: f64, end: f64, ids: &[u32]| {
            let text = self
                .tokenizer
                .decode(&ids.iter().cloned().filter(|&t| t < EOT).collect::<Vec<_>>());
            let text = text.trim();
            if !text.is_empty() {
                segments.push(Segment {
                    start: round2(start),
                    end: round2(end),
                    text: text.to_string(),
                });
            }
        };

        // Boundaries: positions where two timestamp tokens are adjacent.
        let mut slices = Vec::new();
        let mut last = 0usize;
        for i in 1..tokens.len() {
            if is_ts(tokens[i]) && is_ts(tokens[i - 1]) {
                slices.push(&tokens[last..i]);
                last = i;
            }
        }

        if !slices.is_empty() {
            let mut last_end = 0f64;
            for slice in &slices {
                let (Some(&first), Some(&end)) = (slice.first(), slice.last()) else {
                    continue;
                };
                if !is_ts(first) || !is_ts(end) {
                    continue;
                }
                last_end = ts_sec(end);
                push(
                    segments,
                    time_offset + ts_sec(first),
                    time_offset + last_end,
                    &slice[1..slice.len() - 1],
                );
            }
            // Seek to the end of the last complete segment.
            ((last_end * FRAMES_PER_SECOND) as usize).max(1)
        } else {
            // No complete pair: one segment covering the window; a trailing
            // lone timestamp still bounds its duration.
            let ts_tokens: Vec<u32> = tokens.iter().cloned().filter(|&t| is_ts(t)).collect();
            let duration = match ts_tokens.last() {
                Some(&t) if t > TS_BEGIN => ts_sec(t),
                _ => segment_duration,
            };
            push(segments, time_offset, time_offset + duration, tokens);
            N_FRAMES
        }
    }

    fn encode(&mut self, window: &[f32]) -> Result<(Vec<f32>, Vec<usize>), String> {
        let input = Tensor::from_array(([1usize, crate::mel::N_MELS, N_FRAMES], window.to_vec()))
            .map_err(|e| format!("asr: mel tensor: {e}"))?;
        let outputs = self
            .encoder
            .run(ort::inputs!["input_features" => input])
            .map_err(|e| format!("asr: encoder: {e}"))?;
        let (shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("asr: encoder output: {e}"))?;
        Ok((data.to_vec(), shape.iter().map(|&d| d as usize).collect()))
    }

    /// Greedy decode of one 30-second window with OpenAI's timestamp rules.
    fn decode_window(&mut self, hidden: &(Vec<f32>, Vec<usize>)) -> Result<WindowOutput, String> {
        let mut tokens: Vec<u32> = vec![SOT, self.lang_token, TRANSCRIBE];
        let prompt_len = tokens.len();
        let mut sampled: Vec<u32> = Vec::new();
        let mut sum_logprob = 0f64;
        let mut no_speech_prob = 0f64;

        for step in 0..SAMPLE_LEN {
            let logits = self.decoder_logits(&tokens, hidden)?;
            let vocab = logits.len() / tokens.len();
            if step == 0 {
                // p(no-speech) is read at the <|startoftranscript|> position.
                let sot_row = &logits[..vocab];
                no_speech_prob = softmax_prob(sot_row, NO_SPEECH as usize);
            }
            let mut row: Vec<f32> = logits[(tokens.len() - 1) * vocab..].to_vec();
            apply_rules(&mut row, &sampled, step, &self.suppress);

            // Greedy sample from the masked distribution; accumulate log-prob.
            let (next, logprob) = argmax_logprob(&row);
            sum_logprob += logprob;
            if next == EOT {
                break;
            }
            sampled.push(next);
            tokens.push(next);
            let _ = prompt_len;
        }

        let avg_logprob = sum_logprob / (sampled.len() as f64 + 1.0);
        Ok(WindowOutput {
            tokens: sampled,
            avg_logprob,
            no_speech_prob,
        })
    }

    fn decoder_logits(
        &mut self,
        tokens: &[u32],
        hidden: &(Vec<f32>, Vec<usize>),
    ) -> Result<Vec<f32>, String> {
        let ids: Vec<i64> = tokens.iter().map(|&t| t as i64).collect();
        let ids_t = Tensor::from_array(([1usize, ids.len()], ids))
            .map_err(|e| format!("asr: ids tensor: {e}"))?;
        let hid_t = TensorRef::from_array_view((hidden.1.as_slice(), hidden.0.as_slice()))
            .map_err(|e| format!("asr: hidden tensor: {e}"))?;
        let outputs = self
            .decoder
            .run(ort::inputs!["input_ids" => ids_t, "encoder_hidden_states" => hid_t])
            .map_err(|e| format!("asr: decoder: {e}"))?;
        let (_, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("asr: decoder output: {e}"))?;
        Ok(data.to_vec())
    }
}

struct WindowOutput {
    tokens: Vec<u32>,
    avg_logprob: f64,
    no_speech_prob: f64,
}

/// Slice one 30-second mel window (row-major `N_MELS × total_frames` → flat
/// `N_MELS × N_FRAMES`), zero-padding past the end.
fn window_mel(mel: &[f32], total_frames: usize, seek: usize) -> Vec<f32> {
    let mut out = vec![0f32; crate::mel::N_MELS * N_FRAMES];
    for m in 0..crate::mel::N_MELS {
        let row = &mel[m * total_frames..(m + 1) * total_frames];
        let take = N_FRAMES.min(row.len().saturating_sub(seek));
        out[m * N_FRAMES..m * N_FRAMES + take].copy_from_slice(&row[seek..seek + take]);
    }
    out
}

/// OpenAI's greedy-decode filters (`whisper/decoding.py`), in application
/// order: special-token + non-speech-symbol suppression, `suppress_blank`, the
/// timestamp pairing/monotonicity rules, and the timestamp-probability-mass
/// rule.
fn apply_rules(row: &mut [f32], sampled: &[u32], step: usize, suppress: &[u32]) {
    let neg_inf = f32::NEG_INFINITY;
    // Suppress every special token between EOT and the timestamps (SOT, task,
    // language, no-speech, no-timestamps, …) — none is ever valid output here.
    for v in row
        .iter_mut()
        .take(TS_BEGIN as usize)
        .skip(EOT as usize + 1)
    {
        *v = neg_inf;
    }
    // The default `suppress_tokens="-1"` list: bare annotation symbols (`[`,
    // `(`, `♪`, …) that produce non-speech captions like `[BLANK_AUDIO]`.
    for &t in suppress {
        row[t as usize] = neg_inf;
    }

    if step == 0 {
        // suppress_blank: never start with a space or immediate end.
        row[SPACE as usize] = neg_inf;
        row[EOT as usize] = neg_inf;
        // The first token must be a timestamp, capped at max_initial_timestamp.
        for v in row.iter_mut().take(TS_BEGIN as usize) {
            *v = neg_inf;
        }
        for v in row
            .iter_mut()
            .skip((TS_BEGIN + MAX_INITIAL_TS) as usize + 1)
        {
            *v = neg_inf;
        }
        return;
    }

    let is_ts = |t: u32| t >= TS_BEGIN;
    let last_was_ts = sampled.last().is_some_and(|&t| is_ts(t));
    let penultimate_was_ts = match sampled.len() {
        0 | 1 => true,
        n => is_ts(sampled[n - 2]),
    };
    if last_was_ts {
        if penultimate_was_ts {
            // A completed pair: the next token has to be text (or EOT).
            for v in row.iter_mut().skip(TS_BEGIN as usize) {
                *v = neg_inf;
            }
        } else {
            // An opening timestamp: only its closing timestamp (or EOT) may follow.
            for v in row.iter_mut().take(EOT as usize) {
                *v = neg_inf;
            }
        }
    }
    // Timestamps must be monotonic across the window.
    if let Some(&last_ts) = sampled.iter().rev().find(|&&t| is_ts(t)) {
        let floor = if last_was_ts && !penultimate_was_ts {
            last_ts // the closing timestamp may repeat the opening one
        } else {
            last_ts + 1
        };
        for v in row.iter_mut().take(floor as usize).skip(TS_BEGIN as usize) {
            *v = neg_inf;
        }
    }

    // If the total probability mass on timestamps exceeds any single text
    // token, a timestamp must be sampled.
    let logprobs = log_softmax(row);
    let ts_mass = logsumexp(&logprobs[TS_BEGIN as usize..]);
    let max_text = logprobs[..TS_BEGIN as usize]
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    if ts_mass > max_text {
        for v in row.iter_mut().take(TS_BEGIN as usize) {
            *v = neg_inf;
        }
    }
}

fn argmax_logprob(row: &[f32]) -> (u32, f64) {
    let mut best = 0usize;
    for (i, &v) in row.iter().enumerate() {
        if v > row[best] {
            best = i;
        }
    }
    let logprobs = log_softmax(row);
    (best as u32, logprobs[best])
}

fn log_softmax(row: &[f32]) -> Vec<f64> {
    let max = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max) as f64;
    let mut sum = 0f64;
    for &v in row {
        let e = (v as f64 - max).exp();
        if e.is_finite() {
            sum += e;
        }
    }
    let log_z = max + sum.ln();
    row.iter().map(|&v| v as f64 - log_z).collect()
}

fn logsumexp(logprobs: &[f64]) -> f64 {
    let max = logprobs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if !max.is_finite() {
        return max;
    }
    max + logprobs.iter().map(|&v| (v - max).exp()).sum::<f64>().ln()
}

fn softmax_prob(row: &[f32], index: usize) -> f64 {
    let max = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max) as f64;
    let mut sum = 0f64;
    for &v in row {
        sum += (v as f64 - max).exp();
    }
    ((row[index] as f64 - max).exp()) / sum
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The `<|lang|>` prompt token. `FLEISCHWOLF_ASR_LANG` selects the language
/// (default `en`); codes are resolved through `added_tokens.json` next to the
/// vocabulary when present, so any Whisper language works without a table here.
fn language_token() -> u32 {
    let lang = std::env::var("FLEISCHWOLF_ASR_LANG").unwrap_or_else(|_| "en".into());
    if lang == "en" {
        return LANG_EN;
    }
    let path = model_path("DOCLING_ASR_ADDED_TOKENS", "models/asr/added_tokens.json");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(map) = serde_json::from_str::<std::collections::HashMap<String, u32>>(&raw) {
            if let Some(&id) = map.get(&format!("<|{lang}|>")) {
                return id;
            }
        }
    }
    LANG_EN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_rules_force_initial_timestamp_and_pairing() {
        let v = 51_865usize;
        // Step 0: only timestamps within the first second survive.
        let mut row = vec![0f32; v];
        apply_rules(&mut row, &[], 0, &[]);
        assert!(row[..TS_BEGIN as usize]
            .iter()
            .all(|&x| x == f32::NEG_INFINITY));
        assert!(row[TS_BEGIN as usize].is_finite());
        assert!(row[(TS_BEGIN + MAX_INITIAL_TS) as usize].is_finite());
        assert_eq!(
            row[(TS_BEGIN + MAX_INITIAL_TS) as usize + 1],
            f32::NEG_INFINITY
        );

        // After an opening timestamp + text + closing timestamp pair, the next
        // token must be text again (timestamps masked).
        let mut row = vec![0f32; v];
        apply_rules(
            &mut row,
            &[TS_BEGIN, 1000, TS_BEGIN + 10, TS_BEGIN + 10],
            4,
            &[],
        );
        assert!(row[TS_BEGIN as usize..]
            .iter()
            .all(|&x| x == f32::NEG_INFINITY));
        assert!(row[1000].is_finite());
    }

    #[test]
    fn lone_opening_timestamp_only_allows_closing_timestamp() {
        let v = 51_865usize;
        let mut row = vec![0f32; v];
        // "<|5.00|> hello" then an opening ts <|7.00|>: text is masked, earlier
        // timestamps are masked (monotonicity), the closing timestamp — allowed
        // to repeat the opening one — survives. (On this uniform row the
        // timestamp-probability-mass rule also masks EOT, exactly as OpenAI's
        // `logits[:timestamp_begin] = -inf` does.)
        apply_rules(&mut row, &[TS_BEGIN + 250, 1000, TS_BEGIN + 350], 3, &[]);
        assert!(row[..EOT as usize].iter().all(|&x| x == f32::NEG_INFINITY));
        assert_eq!(row[(TS_BEGIN + 349) as usize], f32::NEG_INFINITY);
        assert!(row[(TS_BEGIN + 350) as usize].is_finite());
        assert!(row[(TS_BEGIN + 351) as usize].is_finite());
    }
}
