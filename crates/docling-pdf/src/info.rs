//! The PDF's Info dictionary (#679): `Title`, `Author`, `Subject`,
//! `Keywords`, `Creator`, `Producer`, `CreationDate`, `ModDate` and
//! whatever else the writer put there, as text — PDFDocEncoding or UTF-16
//! strings decoded, `D:YYYYMMDDHHmmSS±HH'mm'` dates turned into ISO 8601.
//! Pure lopdf, so it reads what the text-layer path reads, with the same
//! password handling: an encrypted file the password does not open yields
//! nothing rather than ciphertext.

use lopdf::{Dictionary, Object};

/// The Info dictionary's entries, `(key, text)` in the file's order; empty
/// when the file has none or cannot be read.
pub fn document_info(bytes: &[u8], password: Option<&str>) -> Vec<(String, String)> {
    let Ok(doc) = crate::textparse::open_document(bytes, password) else {
        return Vec::new();
    };
    let Some(info) = doc
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|o| match o {
            Object::Reference(id) => doc.get_object(*id).ok(),
            other => Some(other),
        })
        .and_then(|o| o.as_dict().ok())
    else {
        return Vec::new();
    };
    entries(info, &doc)
}

fn entries(info: &Dictionary, doc: &lopdf::Document) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (key, value) in info.iter() {
        let value = match value {
            Object::Reference(id) => match doc.get_object(*id) {
                Ok(v) => v,
                Err(_) => continue,
            },
            v => v,
        };
        let text = match value {
            Object::String(bytes, _) => decode_text(bytes),
            Object::Name(name) => String::from_utf8_lossy(name).into_owned(),
            Object::Integer(i) => i.to_string(),
            Object::Real(r) => r.to_string(),
            Object::Boolean(b) => b.to_string(),
            _ => continue,
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let key = String::from_utf8_lossy(key).into_owned();
        let text = if key.ends_with("Date") {
            pdf_date_to_iso(&text).unwrap_or(text)
        } else {
            text
        };
        out.push((key, text));
    }
    out
}

/// A PDF text string: UTF-16 with a byte-order mark, UTF-8 when it decodes
/// as such (writers do), else PDFDocEncoding read as Latin-1.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.len() >= 3 && bytes[..3] == [0xEF, 0xBB, 0xBF] {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// `D:YYYYMMDDHHmmSSOHH'mm'` (every part after the year optional, the
/// offset `Z`, `+HH'mm'` or `-HH'mm'`) → `YYYY-MM-DDTHH:mm:SS+HH:mm`, as
/// far as the date goes. `None` for anything else.
pub fn pdf_date_to_iso(s: &str) -> Option<String> {
    let s = s.trim().strip_prefix("D:").unwrap_or(s.trim());
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return None;
    }
    let rest = &s[digits.len()..];
    let part = |from: usize, len: usize| digits.get(from..from + len);
    let mut out = part(0, 4)?.to_string();
    if let Some(m) = part(4, 2) {
        out.push('-');
        out.push_str(m);
        if let Some(d) = part(6, 2) {
            out.push('-');
            out.push_str(d);
            if let Some(h) = part(8, 2) {
                out.push('T');
                out.push_str(h);
                out.push(':');
                out.push_str(part(10, 2).unwrap_or("00"));
                out.push(':');
                out.push_str(part(12, 2).unwrap_or("00"));
                let rest = rest.trim_end_matches('\'');
                if rest == "Z" {
                    out.push('Z');
                } else if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
                    let off: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).collect();
                    if off.len() >= 2 {
                        out.push(sign);
                        out.push_str(&off[..2]);
                        out.push(':');
                        out.push_str(off.get(2..4).unwrap_or("00"));
                    }
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_dates_become_iso_8601() {
        assert_eq!(
            pdf_date_to_iso("D:20220314012727Z").as_deref(),
            Some("2022-03-14T01:27:27Z")
        );
        assert_eq!(
            pdf_date_to_iso("D:20240102030405+01'00'").as_deref(),
            Some("2024-01-02T03:04:05+01:00")
        );
        assert_eq!(
            pdf_date_to_iso("D:20240102030405-0530").as_deref(),
            Some("2024-01-02T03:04:05-05:30")
        );
        assert_eq!(pdf_date_to_iso("D:2024").as_deref(), Some("2024"));
        assert_eq!(pdf_date_to_iso("D:202401").as_deref(), Some("2024-01"));
        assert_eq!(pdf_date_to_iso("March 2024"), None);
    }

    #[test]
    fn text_strings_decode_by_their_marker() {
        assert_eq!(decode_text(b"\xFE\xFF\x00T\x00\xE9"), "Té");
        assert_eq!(decode_text(b"T\xE9"), "Té");
        assert_eq!(decode_text("Té".as_bytes()), "Té");
    }

    /// A well-formed one-page file: the objects, an xref table with the
    /// real offsets, the trailer.
    fn minimal_pdf(objects: &[&str]) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Info 4 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn the_info_dictionary_of_a_minimal_pdf() {
        let pdf = minimal_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 5 0 R >>",
            "<< /Title (A title) /Author (Ada) /CreationDate (D:20240102030405Z) /Trapped /False >>",
            "<< /Length 5 >>\nstream\nBT ET\nendstream",
        ]);
        let info = document_info(&pdf, None);
        assert!(
            info.contains(&("Title".into(), "A title".into())),
            "{info:?}"
        );
        assert!(info.contains(&("Author".into(), "Ada".into())), "{info:?}");
        assert!(
            info.contains(&("CreationDate".into(), "2024-01-02T03:04:05Z".into())),
            "{info:?}"
        );
        assert!(
            info.contains(&("Trapped".into(), "False".into())),
            "{info:?}"
        );
        assert!(document_info(b"not a pdf", None).is_empty());
    }
}
