//! Whisper token **decoding** (ids → text).
//!
//! Whisper uses a GPT-2-style byte-level BPE. Turning ids back into text only
//! needs the vocabulary (`vocab.json`: token string → id) and the fixed GPT-2
//! byte↔unicode table — no merges, no encoder. Special tokens (ids at or above
//! the base vocabulary) decode to nothing.

use std::collections::HashMap;

pub struct Tokenizer {
    /// id → token string (in GPT-2 byte-unicode space).
    tokens: Vec<String>,
    /// GPT-2 printable-unicode char → original byte.
    byte_of_char: HashMap<char, u8>,
}

impl Tokenizer {
    /// Load from a HuggingFace `vocab.json` (token → id map).
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("asr: reading {}: {e}", path.display()))?;
        let map: HashMap<String, u32> = serde_json::from_str(&raw)
            .map_err(|e| format!("asr: parsing {}: {e}", path.display()))?;
        let size = map.values().max().map(|&m| m as usize + 1).unwrap_or(0);
        let mut tokens = vec![String::new(); size];
        for (tok, id) in map {
            tokens[id as usize] = tok;
        }
        Ok(Self {
            tokens,
            byte_of_char: byte_decoder(),
        })
    }

    /// Decode a sequence of token ids to text. Ids outside the vocabulary
    /// (special/timestamp tokens) are skipped.
    pub fn decode(&self, ids: &[u32]) -> String {
        let mut bytes = Vec::new();
        for &id in ids {
            let Some(tok) = self.tokens.get(id as usize) else {
                continue;
            };
            for ch in tok.chars() {
                if let Some(&b) = self.byte_of_char.get(&ch) {
                    bytes.push(b);
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// OpenAI's default non-speech suppression set (`tokenizer.non_speech_tokens`,
    /// the `suppress_tokens="-1"` list): tokens that render as bare bracketing /
    /// annotation symbols — `[`, `(`, `♪`, `--`, … — optionally preceded by a
    /// space. Suppressing them is what keeps Whisper from emitting
    /// `[BLANK_AUDIO]`-style annotations on silence. Derived here by decoding
    /// every vocabulary token and matching against the symbol list (equivalent
    /// to OpenAI's encode-based single-token check for these short strings).
    pub fn non_speech_tokens(&self) -> Vec<u32> {
        const SYMBOLS: &[&str] = &[
            "\"",
            "#",
            "(",
            ")",
            "*",
            "+",
            "/",
            ":",
            ";",
            "<",
            "=",
            ">",
            "@",
            "[",
            "\\",
            "]",
            "^",
            "_",
            "`",
            "{",
            "|",
            "}",
            "~",
            "「",
            "」",
            "『",
            "』",
            "<<",
            ">>",
            "<<<",
            ">>>",
            "--",
            "---",
            "-(",
            "-[",
            "('",
            "(\"",
            "((",
            "))",
            "(((",
            ")))",
            "[[",
            "]]",
            "{{",
            "}}",
            "♪♪",
            "♪♪♪",
            "♩",
            "♪",
            "♫",
            "♬",
            "♭",
            "♮",
            "♯",
            " -",
            " '",
        ];
        let mut out = Vec::new();
        for (id, _) in self.tokens.iter().enumerate() {
            let text = self.decode(&[id as u32]);
            if text.is_empty() {
                continue;
            }
            let bare = text.strip_prefix(' ').unwrap_or(&text);
            if SYMBOLS.contains(&text.as_str())
                || (!bare.is_empty() && SYMBOLS.contains(&bare))
                || bare.chars().all(|c| "♩♪♫♬♭♮♯".contains(c)) && !bare.is_empty()
            {
                out.push(id as u32);
            }
        }
        out
    }
}

/// GPT-2's `bytes_to_unicode`, inverted: the 256 bytes map to printable unicode
/// chars (printable ASCII and Latin-1 keep themselves; the rest shift to
/// `256 + n`).
fn byte_decoder() -> HashMap<char, u8> {
    let mut bs: Vec<u16> = (b'!'..=b'~').map(u16::from).collect();
    bs.extend(0xA1..=0xAC_u16);
    bs.extend(0xAE..=0xFF_u16);
    let mut cs: Vec<u32> = bs.iter().map(|&b| b as u32).collect();
    let mut n = 0u32;
    for b in 0..=255u16 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    bs.iter()
        .zip(cs)
        .map(|(&b, c)| (char::from_u32(c).unwrap(), b as u8))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_decoder_roundtrips_ascii_and_space() {
        let dec = byte_decoder();
        // 'A' maps to itself; space (0x20) maps to 'Ġ' (U+0120 = 256 + 32nd gap).
        assert_eq!(dec[&'A'], b'A');
        assert_eq!(dec[&'\u{120}'], b' ');
        assert_eq!(dec.len(), 256);
    }
}
