//! CMaps for composite fonts (ISO 32000-1, 9.7.5): the byte-length codespace
//! ranges that split a string into codes, and the `cidchar` / `cidrange`
//! entries that map codes to CIDs. Embedded CMap streams are read in full;
//! of the predefined ones only the `Identity` and Unicode (`Uni*-UCS2`,
//! `Uni*-UTF16`) families are known — the others (no `cmap-resources` in a
//! Rust checkout) fall back to two-byte codes.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CMap {
    /// `(byte length, low, high)` codespace ranges.
    codespace: Vec<(usize, u32, u32)>,
    single: HashMap<u32, u32>,
    /// `(lo, hi, cid_of_lo)`.
    ranges: Vec<(u32, u32, u32)>,
    pub identity: bool,
    /// The codes are Unicode (UCS-2 / UTF-16 BE) — a predefined `Uni*` CMap.
    pub unicode_codes: bool,
    pub vertical: bool,
}

impl CMap {
    pub fn identity_h() -> CMap {
        CMap {
            codespace: vec![(2, 0, 0xFFFF)],
            single: HashMap::new(),
            ranges: Vec::new(),
            identity: true,
            unicode_codes: false,
            vertical: false,
        }
    }

    /// A predefined CMap by name.
    pub fn predefined(name: &[u8]) -> CMap {
        let s = String::from_utf8_lossy(name);
        let vertical = s.ends_with("-V");
        let mut c = CMap::identity_h();
        c.vertical = vertical;
        if s.starts_with("Identity") {
            return c;
        }
        // CIDs remain unknown for non-Identity predefined CMaps, but the
        // Unicode families tell us the codes and their byte lengths.
        c.identity = false;
        c.unicode_codes = s.contains("UCS2") || s.contains("UTF16");
        if s.contains("UTF16") {
            c.codespace = vec![
                (2, 0, 0xd7ff),
                (4, 0xd800dc00, 0xdbffdfff),
                (2, 0xe000, 0xffff),
            ];
        }
        c
    }

    /// Parse an embedded CMap stream.
    pub fn parse(data: &[u8]) -> CMap {
        let mut c = CMap {
            codespace: Vec::new(),
            single: HashMap::new(),
            ranges: Vec::new(),
            identity: false,
            unicode_codes: false,
            vertical: false,
        };
        let toks = tokenize(data);
        let mut i = 0;
        while i < toks.len() {
            match &toks[i] {
                Tok::Kw(k) if k == "begincodespacerange" => {
                    i += 1;
                    while i + 1 < toks.len() {
                        match (&toks[i], &toks[i + 1]) {
                            (Tok::Hex(lo), Tok::Hex(hi)) => {
                                let n = lo.len().clamp(1, 4);
                                c.codespace.push((n, be(lo), be(hi)));
                                i += 2;
                            }
                            _ => break,
                        }
                    }
                }
                Tok::Kw(k) if k == "begincidrange" => {
                    i += 1;
                    while i + 2 < toks.len() {
                        match (&toks[i], &toks[i + 1], &toks[i + 2]) {
                            (Tok::Hex(lo), Tok::Hex(hi), Tok::Num(cid)) => {
                                c.ranges.push((be(lo), be(hi), *cid as u32));
                                if c.codespace.is_empty() {
                                    c.codespace.push((lo.len().clamp(1, 4), 0, u32::MAX));
                                }
                                i += 3;
                            }
                            _ => break,
                        }
                    }
                }
                Tok::Kw(k) if k == "begincidchar" => {
                    i += 1;
                    while i + 1 < toks.len() {
                        match (&toks[i], &toks[i + 1]) {
                            (Tok::Hex(code), Tok::Num(cid)) => {
                                c.single.insert(be(code), *cid as u32);
                                if c.codespace.is_empty() {
                                    c.codespace.push((code.len().clamp(1, 4), 0, u32::MAX));
                                }
                                i += 2;
                            }
                            _ => break,
                        }
                    }
                }
                Tok::Kw(k) if k == "usecmap" => {
                    // `/Identity-H usecmap` and friends: the parent's codes.
                    if let Some(Tok::Name(n)) = toks.get(i.wrapping_sub(1)) {
                        if n.starts_with("Identity") {
                            c.identity = c.single.is_empty() && c.ranges.is_empty();
                            if c.codespace.is_empty() {
                                c.codespace.push((2, 0, 0xFFFF));
                            }
                        }
                    }
                    i += 1;
                }
                Tok::Kw(k) if k == "def" => {
                    if let (Some(Tok::Name(n)), Some(Tok::Num(v))) =
                        (toks.get(i.wrapping_sub(2)), toks.get(i.wrapping_sub(1)))
                    {
                        if n == "WMode" && *v == 1.0 {
                            c.vertical = true;
                        }
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        if c.codespace.is_empty() {
            c.codespace.push((2, 0, 0xFFFF));
        }
        // Shortest byte length first, so a one-byte range wins over a
        // two-byte one that happens to contain the same leading byte.
        c.codespace.sort_by_key(|r| r.0);
        c
    }

    /// Split `bytes` into `(code, byte length)` pairs.
    pub fn split(&self, bytes: &[u8]) -> Vec<(u32, usize)> {
        let mut out = Vec::with_capacity(bytes.len() / 2 + 1);
        let mut i = 0;
        while i < bytes.len() {
            let mut taken = None;
            for &(n, lo, hi) in &self.codespace {
                if i + n > bytes.len() {
                    continue;
                }
                let code = bytes[i..i + n]
                    .iter()
                    .fold(0u32, |a, &b| (a << 8) | u32::from(b));
                if code >= lo
                    && code <= hi
                    && !(self.unicode_codes
                        && n == 4
                        && crate::textparse::unicode_cmap_char(code).is_none())
                {
                    taken = Some((code, n));
                    break;
                }
            }
            let (code, n) = taken.unwrap_or_else(|| {
                // Not in any range: the shortest codespace length, per 9.7.6.3.
                let n = self
                    .codespace
                    .first()
                    .map(|r| r.0)
                    .unwrap_or(1)
                    .min(bytes.len() - i);
                let code = bytes[i..i + n]
                    .iter()
                    .fold(0u32, |a, &b| (a << 8) | u32::from(b));
                (code, n)
            });
            out.push((code, n));
            i += n;
        }
        out
    }

    pub fn cid(&self, code: u32) -> u32 {
        if self.identity {
            return code;
        }
        if let Some(c) = self.single.get(&code) {
            return *c;
        }
        for &(lo, hi, cid) in &self.ranges {
            if code >= lo && code <= hi {
                return cid + (code - lo);
            }
        }
        if self.single.is_empty() && self.ranges.is_empty() {
            code
        } else {
            0
        }
    }
}

fn be(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .take(4)
        .fold(0u32, |a, &b| (a << 8) | u32::from(b))
}

enum Tok {
    Hex(Vec<u8>),
    Num(f64),
    Name(String),
    Kw(String),
}

fn tokenize(data: &[u8]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        match b {
            b'%' => {
                while i < data.len() && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
            }
            b'<' => {
                if data.get(i + 1) == Some(&b'<') {
                    i += 2;
                    continue;
                }
                let end = data[i..]
                    .iter()
                    .position(|&c| c == b'>')
                    .map(|p| i + p)
                    .unwrap_or(data.len());
                let mut bytes = Vec::new();
                let mut hi: Option<u8> = None;
                for &c in &data[i + 1..end] {
                    let v = match c {
                        b'0'..=b'9' => c - b'0',
                        b'a'..=b'f' => c - b'a' + 10,
                        b'A'..=b'F' => c - b'A' + 10,
                        _ => continue,
                    };
                    match hi.take() {
                        None => hi = Some(v),
                        Some(h) => bytes.push((h << 4) | v),
                    }
                }
                if let Some(h) = hi {
                    bytes.push(h << 4);
                }
                out.push(Tok::Hex(bytes));
                i = end + 1;
            }
            b'/' => {
                let start = i + 1;
                let mut j = start;
                while j < data.len()
                    && !data[j].is_ascii_whitespace()
                    && !b"/<>[](){}%".contains(&data[j])
                {
                    j += 1;
                }
                out.push(Tok::Name(
                    String::from_utf8_lossy(&data[start..j]).into_owned(),
                ));
                i = j;
            }
            b'[' | b']' | b'{' | b'}' | b'>' | b'(' | b')' => i += 1,
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < data.len()
                    && !data[i].is_ascii_whitespace()
                    && !b"/<>[](){}%".contains(&data[i])
                {
                    i += 1;
                }
                let s = String::from_utf8_lossy(&data[start..i]).into_owned();
                match s.parse::<f64>() {
                    Ok(v) => out.push(Tok::Num(v)),
                    Err(_) => out.push(Tok::Kw(s)),
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_cmap_ranges_and_codespaces() {
        let src = b"%!PS\n/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n1 begincodespacerange <00> <80> <8140> <9ffc> endcodespacerange\n2 begincidrange <20> <7e> 1 <8140> <817e> 633 endcidrange\n1 begincidchar <80> 97 endcidchar\nendcmap";
        let c = CMap::parse(src);
        assert_eq!(
            c.split(b"\x41\x81\x41\x80"),
            vec![(0x41, 1), (0x8141, 2), (0x80, 1)]
        );
        assert_eq!(c.cid(0x41), 1 + 0x21);
        assert_eq!(c.cid(0x8141), 634);
        assert_eq!(c.cid(0x80), 97);
    }

    #[test]
    fn identity_is_two_byte() {
        let c = CMap::predefined(b"Identity-H");
        assert_eq!(c.split(b"\x00\x41\x12\x34"), vec![(0x41, 2), (0x1234, 2)]);
        assert_eq!(c.cid(0x1234), 0x1234);
        assert!(CMap::predefined(b"UniGB-UCS2-H").unicode_codes);
    }

    #[test]
    fn utf16_pairs_are_four_byte_codes_only_in_utf16_cmaps() {
        let bytes = b"\x65\xe5\xd8\x42\xdf\xb7\x8a\x9e";
        for name in [b"UniJIS-UTF16-H".as_slice(), b"UniJIS-UTF16-V"] {
            assert_eq!(
                CMap::predefined(name).split(bytes),
                vec![(0x65e5, 2), (0xd842dfb7, 4), (0x8a9e, 2)]
            );
        }
        for name in [b"Identity-H".as_slice(), b"UniJIS-UCS2-H"] {
            assert_eq!(
                CMap::predefined(name).split(bytes),
                vec![(0x65e5, 2), (0xd842, 2), (0xdfb7, 2), (0x8a9e, 2)]
            );
        }
        let c = CMap::predefined(b"UniJIS-UTF16-H");
        assert_eq!(c.split(b"\xd8\x42\x65\xe5"), vec![(0xd842, 2), (0x65e5, 2)]);
        assert_eq!(c.split(b"\xd8\x42"), vec![(0xd842, 2)]);
    }
}
