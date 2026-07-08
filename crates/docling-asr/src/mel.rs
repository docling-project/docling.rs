//! Whisper's log-mel spectrogram front-end, ported from
//! `whisper/audio.py::log_mel_spectrogram`:
//!
//! * STFT: `n_fft` 400, hop 160, periodic Hann window, centered with reflect
//!   padding, last frame dropped;
//! * 80 Slaney-normalized mel filters over 0–8000 Hz (the same filterbank
//!   librosa's `filters.mel(16000, 400, n_mels=80)` produces, which Whisper
//!   ships precomputed);
//! * `log10(clamp(x, 1e-10))`, floored at `max − 8`, then `(x + 4) / 4`.
//!
//! A naive `O(n_fft²)` real DFT with precomputed twiddles keeps this
//! dependency-free; at 201 bins × 400 points × ~3000 frames it is a few hundred
//! ms in release — irrelevant next to decoder inference.

pub const N_FFT: usize = 400;
pub const HOP_LENGTH: usize = 160;
pub const N_MELS: usize = 80;
/// Mel frames in one 30-second window (`3000`).
pub const N_FRAMES: usize = 3000;
/// Samples in one 30-second window (`480_000`).
pub const N_SAMPLES: usize = 30 * super::audio::SAMPLE_RATE as usize;
const N_BINS: usize = N_FFT / 2 + 1;

/// Compute the log-mel spectrogram of `samples` (16 kHz mono), returning
/// `(mel, frames)` where `mel` is `N_MELS × frames` in row-major order.
/// `padding` zero samples are appended first (Whisper pads a full extra window
/// so the last real content still gets a complete 30-second context).
pub fn log_mel_spectrogram(samples: &[f32], padding: usize) -> (Vec<f32>, usize) {
    let mut audio = Vec::with_capacity(samples.len() + padding);
    audio.extend_from_slice(samples);
    audio.resize(samples.len() + padding, 0.0);

    // Centered STFT: reflect-pad n_fft/2 on both sides.
    let half = N_FFT / 2;
    let padded = reflect_pad(&audio, half);
    // torch.stft yields 1 + len/hop frames; whisper drops the last one.
    let frames = audio.len() / HOP_LENGTH;

    let window = hann();
    let filters = mel_filters();
    let (cos_t, sin_t) = twiddles();

    let mut mel = vec![0f32; N_MELS * frames];
    let mut frame = [0f32; N_FFT];
    let mut power = [0f32; N_BINS];
    for t in 0..frames {
        let start = t * HOP_LENGTH;
        for (i, w) in window.iter().enumerate() {
            frame[i] = padded[start + i] * w;
        }
        // |DFT|² for the positive-frequency bins.
        for k in 0..N_BINS {
            let (mut re, mut im) = (0f64, 0f64);
            let (ct, st) = (
                &cos_t[k * N_FFT..(k + 1) * N_FFT],
                &sin_t[k * N_FFT..(k + 1) * N_FFT],
            );
            for i in 0..N_FFT {
                let s = frame[i] as f64;
                re += s * ct[i];
                im -= s * st[i];
            }
            power[k] = (re * re + im * im) as f32;
        }
        for m in 0..N_MELS {
            let row = &filters[m * N_BINS..(m + 1) * N_BINS];
            let mut acc = 0f32;
            for k in 0..N_BINS {
                acc += row[k] * power[k];
            }
            mel[m * frames + t] = acc;
        }
    }

    // log10 with clamp, dynamic-range floor, and Whisper's affine rescale.
    let mut max = f32::NEG_INFINITY;
    for v in mel.iter_mut() {
        *v = v.max(1e-10).log10();
        max = max.max(*v);
    }
    let floor = max - 8.0;
    for v in mel.iter_mut() {
        *v = (v.max(floor) + 4.0) / 4.0;
    }
    (mel, frames)
}

fn reflect_pad(audio: &[f32], half: usize) -> Vec<f32> {
    let n = audio.len();
    let mut out = Vec::with_capacity(n + 2 * half);
    for i in (1..=half).rev() {
        out.push(*audio.get(i).unwrap_or(&0.0));
    }
    out.extend_from_slice(audio);
    for i in 2..=half + 1 {
        out.push(if n >= i { audio[n - i] } else { 0.0 });
    }
    out
}

/// Periodic Hann window (`torch.hann_window(400)`).
fn hann() -> Vec<f32> {
    (0..N_FFT)
        .map(|i| {
            let x = (std::f64::consts::PI * i as f64 / N_FFT as f64).sin();
            (x * x) as f32
        })
        .collect()
}

fn twiddles() -> (Vec<f64>, Vec<f64>) {
    let mut cos_t = vec![0f64; N_BINS * N_FFT];
    let mut sin_t = vec![0f64; N_BINS * N_FFT];
    for k in 0..N_BINS {
        for i in 0..N_FFT {
            let angle = 2.0 * std::f64::consts::PI * (k * i) as f64 / N_FFT as f64;
            cos_t[k * N_FFT + i] = angle.cos();
            sin_t[k * N_FFT + i] = angle.sin();
        }
    }
    (cos_t, sin_t)
}

/// The Slaney-normalized mel filterbank, `N_MELS × N_BINS` row-major
/// (librosa `filters.mel(sr=16000, n_fft=400, n_mels=80)`, HTK off).
fn mel_filters() -> Vec<f32> {
    const SR: f64 = 16_000.0;
    let f_sp = 200.0 / 3.0;
    let min_log_hz = 1000.0;
    let min_log_mel = min_log_hz / f_sp;
    let logstep = (6.4f64).ln() / 27.0;

    let hz_to_mel = |hz: f64| {
        if hz >= min_log_hz {
            min_log_mel + (hz / min_log_hz).ln() / logstep
        } else {
            hz / f_sp
        }
    };
    let mel_to_hz = |mel: f64| {
        if mel >= min_log_mel {
            min_log_hz * (logstep * (mel - min_log_mel)).exp()
        } else {
            f_sp * mel
        }
    };

    let max_mel = hz_to_mel(SR / 2.0);
    // n_mels + 2 band edges, evenly spaced on the mel scale.
    let mel_f: Vec<f64> = (0..N_MELS + 2)
        .map(|i| mel_to_hz(max_mel * i as f64 / (N_MELS + 1) as f64))
        .collect();

    let mut filters = vec![0f32; N_MELS * N_BINS];
    for m in 0..N_MELS {
        let (f0, f1, f2) = (mel_f[m], mel_f[m + 1], mel_f[m + 2]);
        let enorm = 2.0 / (f2 - f0);
        for k in 0..N_BINS {
            let freq = k as f64 * SR / N_FFT as f64;
            let lower = (freq - f0) / (f1 - f0);
            let upper = (f2 - freq) / (f2 - f1);
            let w = lower.min(upper).max(0.0);
            filters[m * N_BINS + k] = (w * enorm) as f32;
        }
    }
    filters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filterbank_matches_librosa_reference_values() {
        let f = mel_filters();
        // Spot values from `librosa.filters.mel(sr=16000, n_fft=400, n_mels=80)`.
        // Filter 0 peaks at bin 1 (40 Hz); slaney norm makes the peak ≈ 0.02493.
        let peak0 = f[..N_BINS].iter().cloned().fold(0f32, f32::max);
        assert!((peak0 - 0.024930).abs() < 1e-4, "peak0 = {peak0}");
        // Every filter integrates to ~2/(f2-f0) triangles: all rows non-empty.
        for m in 0..N_MELS {
            assert!(
                f[m * N_BINS..(m + 1) * N_BINS].iter().any(|&v| v > 0.0),
                "filter {m} is empty"
            );
        }
    }

    #[test]
    fn silence_maps_to_constant_floor() {
        let samples = vec![0f32; N_SAMPLES];
        let (mel, frames) = log_mel_spectrogram(&samples, 0);
        assert_eq!(frames, N_FRAMES);
        // log10(1e-10) = -10 everywhere → max −8 floor → (−10.max(−18)+4)/4 = −1.5.
        assert!(mel.iter().all(|&v| (v - (-1.5)).abs() < 1e-6));
    }
}
