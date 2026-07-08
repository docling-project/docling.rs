//! Audio decoding: any supported container/codec → 16 kHz mono `f32` samples.
//!
//! symphonia (pure Rust) demuxes and decodes every format docling's ASR accepts
//! — wav/pcm, mp3, flac, ogg/vorbis, aac (adts), m4a/mp4 — replacing the ffmpeg
//! dependency Python docling shells out to. Channels are averaged to mono and
//! linearly resampled to Whisper's fixed 16 kHz input rate.

use symphonia::core::audio::AudioBufferRef;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Whisper's fixed input sample rate.
pub const SAMPLE_RATE: u32 = 16_000;

/// Decode `bytes` (using the file extension in `name` as a format hint) into
/// mono `f32` samples at 16 kHz.
pub fn decode_to_mono_16k(bytes: &[u8], name: &str) -> Result<Vec<f32>, String> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = name.rsplit('.').next() {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("asr: unrecognized audio container: {e}"))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "asr: no decodable audio track".to_string())?;
    let track_id = track.id;
    let src_rate = track.codec_params.sample_rate.unwrap_or(SAMPLE_RATE);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("asr: unsupported audio codec: {e}"))?;

    // Decode every packet, averaging channels to mono at the source rate. The
    // loop ends on any read error — end of stream, or a truncated tail (keep
    // what we decoded).
    let mut mono: Vec<f32> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A corrupt frame is skipped, not fatal (matches ffmpeg's behavior).
            Err(_) => continue,
        };
        append_mono(&decoded, &mut mono);
    }

    if mono.is_empty() {
        return Err("asr: audio stream decoded to zero samples".to_string());
    }
    Ok(resample_linear(&mono, src_rate, SAMPLE_RATE))
}

/// Average all channels of a decoded buffer into `out` as `f32`.
fn append_mono(buf: &AudioBufferRef, out: &mut Vec<f32>) {
    macro_rules! mix {
        ($b:expr, $to_f32:expr) => {{
            let planes = $b.planes();
            let chans = planes.planes();
            if chans.is_empty() {
                return;
            }
            let frames = chans[0].len();
            let n = chans.len() as f32;
            for i in 0..frames {
                let mut acc = 0f32;
                for ch in chans {
                    acc += $to_f32(ch[i]);
                }
                out.push(acc / n);
            }
        }};
    }
    match buf {
        AudioBufferRef::F32(b) => mix!(b, |s: f32| s),
        AudioBufferRef::F64(b) => mix!(b, |s: f64| s as f32),
        AudioBufferRef::S32(b) => mix!(b, |s: i32| s as f32 / i32::MAX as f32),
        AudioBufferRef::S16(b) => mix!(b, |s: i16| s as f32 / i16::MAX as f32),
        AudioBufferRef::S8(b) => mix!(b, |s: i8| s as f32 / i8::MAX as f32),
        AudioBufferRef::U32(b) => mix!(b, |s: u32| (s as f32 / u32::MAX as f32) * 2.0 - 1.0),
        AudioBufferRef::U16(b) => mix!(b, |s: u16| (s as f32 / u16::MAX as f32) * 2.0 - 1.0),
        AudioBufferRef::U8(b) => mix!(b, |s: u8| (s as f32 / u8::MAX as f32) * 2.0 - 1.0),
        AudioBufferRef::S24(b) => mix!(b, |s: symphonia::core::sample::i24| s.inner() as f32
            / 8_388_607.0),
        AudioBufferRef::U24(b) => mix!(b, |s: symphonia::core::sample::u24| (s.inner() as f32
            / 16_777_215.0)
            * 2.0
            - 1.0),
    }
}

/// Linear-interpolation resampler. Whisper's mel front-end is robust to the
/// difference vs. a windowed-sinc resampler on speech, and this keeps the
/// pipeline dependency-free.
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let a = input[i0];
        let b = *input.get(i0 + 1).unwrap_or(&a);
        out.push(a + (b - a) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_halves_and_keeps_length_ratio() {
        let input: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = resample_linear(&input, 32_000, 16_000);
        assert_eq!(out.len(), 500);
        // Downsampling by 2 keeps every other sample (linear interp at exact points).
        assert!((out[10] - input[20]).abs() < 1e-6);
    }

    #[test]
    fn decodes_wav_bytes() {
        // Minimal 16-bit PCM wav: 100 samples of silence at 16 kHz.
        let mut wav = Vec::new();
        let data_len = 200u32;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend(std::iter::repeat_n(0u8, data_len as usize));
        let samples = decode_to_mono_16k(&wav, "t.wav").expect("wav decodes");
        assert_eq!(samples.len(), 100);
        assert!(samples.iter().all(|&s| s == 0.0));
    }
}
