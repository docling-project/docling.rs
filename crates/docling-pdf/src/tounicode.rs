use std::collections::HashMap;

/// Parse a ToUnicode CMap's `bfchar` / `bfrange` sections into code→string.
pub(crate) fn parse_tounicode(data: &[u8]) -> HashMap<u32, String> {
    let text = String::from_utf8_lossy(data);
    let mut map = HashMap::new();
    let hex = |s: &str| -> Option<Vec<u16>> {
        let s = s.trim();
        if !s.starts_with('<') || !s.ends_with('>') {
            return None;
        }
        let h = &s[1..s.len() - 1];
        let bytes: Vec<u8> = (0..h.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
            .collect();
        Some(
            bytes
                .chunks(2)
                .map(|c| {
                    if c.len() == 2 {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        c[0] as u16
                    }
                })
                .collect(),
        )
    };
    // None for a lone surrogate, which leaves the code unmapped.
    let u16s_to_string = |u: &[u16]| {
        char::decode_utf16(u.iter().copied())
            .collect::<Result<String, _>>()
            .ok()
    };
    let code_of = |u: &[u16]| u.iter().fold(0u32, |acc, &x| (acc << 16) | x as u32);

    // Tokenize by structure, not whitespace: CMap hex groups are often written
    // back-to-back with no separators (`<21><21><0054>`), so scan for `<…>`
    // groups, `[`/`]` brackets, and bareword keywords.
    let tokens: Vec<String> = {
        let bytes = text.as_bytes();
        let mut toks = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_whitespace() {
                i += 1;
            } else if c == b'<' {
                let start = i;
                while i < bytes.len() && bytes[i] != b'>' {
                    i += 1;
                }
                i += 1; // include '>'
                toks.push(String::from_utf8_lossy(&bytes[start..i.min(bytes.len())]).into_owned());
            } else if c == b'[' || c == b']' {
                toks.push((c as char).to_string());
                i += 1;
            } else {
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && bytes[i] != b'<'
                    && bytes[i] != b'['
                    && bytes[i] != b']'
                {
                    i += 1;
                }
                toks.push(String::from_utf8_lossy(&bytes[start..i]).into_owned());
            }
        }
        toks
    };
    let tokens: Vec<&str> = tokens.iter().map(|s| s.as_str()).collect();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "beginbfchar" => {
                i += 1;
                while i + 1 < tokens.len() && tokens[i] != "endbfchar" {
                    if let (Some(src), Some(s)) = (
                        hex(tokens[i]),
                        hex(tokens[i + 1]).and_then(|d| u16s_to_string(&d)),
                    ) {
                        map.insert(code_of(&src), s);
                    }
                    i += 2;
                }
            }
            "beginbfrange" => {
                i += 1;
                while i + 2 < tokens.len() && tokens[i] != "endbfrange" {
                    let (Some(lo), Some(hi)) = (hex(tokens[i]), hex(tokens[i + 1])) else {
                        i += 1;
                        continue;
                    };
                    let lo = code_of(&lo);
                    let hi = code_of(&hi);
                    if tokens[i + 2] == "[" {
                        // `<lo> <hi> [ <d0> <d1> ... ]`: one dst per code in the range.
                        let mut j = i + 3;
                        let mut code = lo;
                        while j < tokens.len() && tokens[j] != "]" {
                            if let Some(s) = hex(tokens[j]).and_then(|d| u16s_to_string(&d)) {
                                map.insert(code, s);
                            }
                            code += 1;
                            j += 1;
                        }
                        i = j + 1;
                    } else if let Some(mut dst) = hex(tokens[i + 2]) {
                        // Increment the last unit for at most 65,536 codes; the
                        // range ends where it overflows or stops being UTF-16.
                        for code in (lo..=hi).take(0x10000) {
                            let Some(s) = u16s_to_string(&dst) else { break };
                            map.insert(code, s);
                            match dst.last_mut() {
                                Some(unit) if *unit < u16::MAX => *unit += 1,
                                Some(_) => break,
                                None => {}
                            }
                        }
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
            }
            _ => i += 1,
        }
    }
    map
}
