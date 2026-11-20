//! The standard (non-image) stream filters an image XObject may wrap its
//! samples in — `FlateDecode`, `LZWDecode`, `RunLengthDecode`,
//! `ASCII85Decode`, `ASCIIHexDecode` — with the PNG and TIFF predictors of
//! `/DecodeParms`. The image codecs (`DCTDecode` → [`super::jpeg`],
//! `CCITTFaxDecode` → [`super::fax`], `JBIG2Decode` → the renderer's
//! `jbig2` (#811), `JPXDecode` → its `jpx`) are reported to the caller as the
//! remaining filter rather than applied here.
//!
//! Written out rather than borrowed from lopdf's `Stream::decompressed_content`
//! because that one sizes the PNG predictor's rows for 8-bit samples only
//! (`max(8, BitsPerComponent)`), which is wrong for the 1-bit scans this
//! module exists for, and has no `RunLengthDecode`/`ASCIIHexDecode`/TIFF
//! predictor.

use lopdf::{Dictionary, Document, Object};

/// A filter left for an image codec, with its `/DecodeParms`.
#[derive(Debug, Clone)]
pub struct ImageCodec {
    pub name: String,
    pub parms: Option<Dictionary>,
}

/// Why a stream could not be decoded here.
#[derive(Debug)]
pub enum Unsupported {
    Filter(String),
    Predictor(i64),
    Corrupt(&'static str),
}

fn deref<'a>(doc: &'a Document, obj: &'a Object) -> &'a Object {
    match obj {
        Object::Reference(id) => doc.get_object(*id).unwrap_or(obj),
        o => o,
    }
}

/// The `/Filter` names and their aligned `/DecodeParms` dictionaries.
pub fn filters(doc: &Document, dict: &Dictionary) -> Vec<(String, Option<Dictionary>)> {
    let names: Vec<String> = match dict
        .get(b"Filter")
        .ok()
        .or_else(|| {
            dict.get(b"F")
                .ok()
                .filter(|o| !matches!(o, Object::String(..)))
        })
        .map(|o| deref(doc, o))
    {
        Some(Object::Name(n)) => vec![String::from_utf8_lossy(n).into_owned()],
        Some(Object::Array(a)) => a
            .iter()
            .filter_map(|o| match deref(doc, o) {
                Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let parms: Vec<Option<Dictionary>> = match dict
        .get(b"DecodeParms")
        .ok()
        .or_else(|| dict.get(b"DP").ok())
        .map(|o| deref(doc, o))
    {
        Some(Object::Dictionary(d)) => vec![Some(d.clone())],
        Some(Object::Array(a)) => a
            .iter()
            .map(|o| match deref(doc, o) {
                Object::Dictionary(d) => Some(d.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    names
        .into_iter()
        .enumerate()
        .map(|(i, n)| (n, parms.get(i).cloned().flatten()))
        .collect()
}

/// Apply every standard filter of `chain` in order to `data`; stop at an
/// image codec and hand it back. `/DecodeParms` predictors are undone after
/// Flate/LZW as the spec has it.
pub fn apply(
    doc: &Document,
    data: &[u8],
    chain: &[(String, Option<Dictionary>)],
) -> Result<(Vec<u8>, Option<ImageCodec>), Unsupported> {
    let mut cur: Vec<u8> = data.to_vec();
    for (name, parms) in chain {
        cur = match name.as_str() {
            "FlateDecode" | "Fl" => predictor(doc, inflate(&cur), parms.as_ref())?,
            "LZWDecode" | "LZW" => {
                let early = parms
                    .as_ref()
                    .and_then(|p| p.get(b"EarlyChange").ok())
                    .and_then(|o| deref(doc, o).as_i64().ok())
                    .map(|v| v != 0)
                    .unwrap_or(true);
                predictor(doc, lzw(&cur, early), parms.as_ref())?
            }
            "RunLengthDecode" | "RL" => run_length(&cur),
            "ASCII85Decode" | "A85" => ascii85(&cur),
            "ASCIIHexDecode" | "AHx" => ascii_hex(&cur),
            "DCTDecode" | "DCT" | "JPXDecode" | "JBIG2Decode" | "CCITTFaxDecode" | "CCF" => {
                return Ok((
                    cur,
                    Some(ImageCodec {
                        name: match name.as_str() {
                            "DCT" => "DCTDecode".into(),
                            "CCF" => "CCITTFaxDecode".into(),
                            n => n.to_string(),
                        },
                        parms: parms.clone(),
                    }),
                ));
            }
            other => return Err(Unsupported::Filter(other.to_string())),
        };
    }
    Ok((cur, None))
}

/// zlib inflate, keeping what a damaged stream yielded before the error — as
/// pdfium's `FlateModule` does.
fn inflate(data: &[u8]) -> Vec<u8> {
    crate::textparse::inflate_prefix(data)
}

/// PDF LZW: MSB-first codes starting at 9 bits, `EarlyChange` 1 by default.
fn lzw(data: &[u8], early_change: bool) -> Vec<u8> {
    use weezl::{decode::Decoder, BitOrder};
    let mut dec = if early_change {
        Decoder::with_tiff_size_switch(BitOrder::Msb, 8)
    } else {
        Decoder::new(BitOrder::Msb, 8)
    };
    let mut out = Vec::new();
    let mut input = data;
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let r = dec.decode_bytes(input, &mut buf);
        out.extend_from_slice(&buf[..r.consumed_out]);
        input = &input[r.consumed_in..];
        match r.status {
            Ok(weezl::LzwStatus::Done) | Err(_) => break,
            Ok(_) if r.consumed_in == 0 && r.consumed_out == 0 => break,
            Ok(_) => {}
        }
    }
    out
}

fn run_length(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let l = data[i] as usize;
        i += 1;
        match l {
            128 => break,
            0..=127 => {
                let end = (i + l + 1).min(data.len());
                out.extend_from_slice(&data[i..end]);
                i = end;
            }
            _ => {
                if let Some(&b) = data.get(i) {
                    out.extend(std::iter::repeat_n(b, 257 - l));
                }
                i += 1;
            }
        }
    }
    out
}

fn ascii_hex(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut hi: Option<u8> = None;
    for &c in data {
        if c == b'>' {
            break;
        }
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => continue,
        };
        match hi.take() {
            None => hi = Some(v),
            Some(h) => out.push(h << 4 | v),
        }
    }
    if let Some(h) = hi {
        out.push(h << 4);
    }
    out
}

fn ascii85(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut group = [0u8; 5];
    let mut n = 0;
    let mut i = 0;
    if data.starts_with(b"<~") {
        i = 2;
    }
    while i < data.len() {
        let c = data[i];
        i += 1;
        match c {
            b'~' => break,
            b'z' if n == 0 => out.extend_from_slice(&[0, 0, 0, 0]),
            b'!'..=b'u' => {
                group[n] = c - b'!';
                n += 1;
                if n == 5 {
                    let v = group.iter().fold(0u32, |acc, &d| {
                        acc.wrapping_mul(85).wrapping_add(u32::from(d))
                    });
                    out.extend_from_slice(&v.to_be_bytes());
                    n = 0;
                }
            }
            _ => {}
        }
    }
    if n > 0 {
        for g in group.iter_mut().skip(n) {
            *g = 84;
        }
        let v = group.iter().fold(0u32, |acc, &d| {
            acc.wrapping_mul(85).wrapping_add(u32::from(d))
        });
        out.extend_from_slice(&v.to_be_bytes()[..n - 1]);
    }
    out
}

/// Undo a `/Predictor` (PNG 10–15 per-row filters, TIFF 2) from `/DecodeParms`.
fn predictor(
    doc: &Document,
    data: Vec<u8>,
    parms: Option<&Dictionary>,
) -> Result<Vec<u8>, Unsupported> {
    let Some(p) = parms else { return Ok(data) };
    let int = |k: &[u8], d: i64| {
        p.get(k)
            .ok()
            .and_then(|o| deref(doc, o).as_i64().ok())
            .unwrap_or(d)
    };
    let pred = int(b"Predictor", 1);
    if pred <= 1 {
        return Ok(data);
    }
    let colors = int(b"Colors", 1).max(1) as usize;
    let bpc = int(b"BitsPerComponent", 8).max(1) as usize;
    let columns = int(b"Columns", 1).max(1) as usize;
    let bpp = (colors * bpc).div_ceil(8).max(1);
    let row_len = (colors * bpc * columns).div_ceil(8);
    match pred {
        2 => {
            if bpc != 8 {
                return Err(Unsupported::Predictor(pred));
            }
            let mut data = data;
            for row in data.chunks_exact_mut(row_len) {
                for i in bpp..row.len() {
                    row[i] = row[i].wrapping_add(row[i - bpp]);
                }
            }
            Ok(data)
        }
        10..=15 => {
            let mut out = Vec::with_capacity(data.len());
            let mut prev = vec![0u8; row_len];
            for chunk in data.chunks(row_len + 1) {
                let Some((&ft, row)) = chunk.split_first() else {
                    break;
                };
                let mut cur = row.to_vec();
                cur.resize(row_len, 0);
                for i in 0..row_len {
                    let a = if i >= bpp { cur[i - bpp] } else { 0 };
                    let b = prev[i];
                    let c = if i >= bpp { prev[i - bpp] } else { 0 };
                    let x = cur[i];
                    cur[i] = match ft {
                        0 => x,
                        1 => x.wrapping_add(a),
                        2 => x.wrapping_add(b),
                        3 => x.wrapping_add(((u16::from(a) + u16::from(b)) / 2) as u8),
                        4 => {
                            let (ia, ib, ic) = (i16::from(a), i16::from(b), i16::from(c));
                            let pa = (ib - ic).abs();
                            let pb = (ia - ic).abs();
                            let pc = (ia + ib - 2 * ic).abs();
                            let pr = if pa <= pb && pa <= pc {
                                a
                            } else if pb <= pc {
                                b
                            } else {
                                c
                            };
                            x.wrapping_add(pr)
                        }
                        _ => return Err(Unsupported::Corrupt("PNG predictor row filter")),
                    };
                }
                out.extend_from_slice(&cur);
                prev = cur;
            }
            Ok(out)
        }
        other => Err(Unsupported::Predictor(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_length_and_ascii_filters() {
        assert_eq!(
            run_length(&[2, b'a', b'b', b'c', 254, b'z', 128]),
            b"abczzz"
        );
        assert_eq!(ascii_hex(b"48 65 6C6c 6F>"), b"Hello");
        assert_eq!(ascii85(b"<~87cURD]i,\"Ebo80~>"), b"Hello World!");
        assert_eq!(ascii85(b"z~>"), [0, 0, 0, 0]);
    }

    #[test]
    fn png_up_predictor_on_one_bit_rows() {
        // Two 16-pixel 1-bit rows (2 bytes each), the second `Up`-filtered.
        let doc = Document::new();
        let mut parms = Dictionary::new();
        parms.set("Predictor", 12);
        parms.set("Colors", 1);
        parms.set("BitsPerComponent", 1);
        parms.set("Columns", 16);
        let data = vec![0u8, 0b1010_1010, 0b0000_1111, 2, 0b0101_0101, 0b1111_0000];
        let out = predictor(&doc, data, Some(&parms)).unwrap();
        assert_eq!(
            out,
            vec![0b1010_1010, 0b0000_1111, 0b1111_1111, 0b1111_1111]
        );
    }

    #[test]
    fn flate_round_trip_with_trailing_garbage() {
        use std::io::Write;
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"scan line bytes").unwrap();
        let mut z = enc.finish().unwrap();
        z.extend_from_slice(b"junk");
        assert_eq!(inflate(&z), b"scan line bytes");
    }
}
