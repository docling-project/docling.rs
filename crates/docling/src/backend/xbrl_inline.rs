//! Inline XBRL (iXBRL) — docling#4689: an XHTML report whose facts are
//! tagged in place, read into the same [`Filing`] an XBRL instance gives, so
//! the title, the text blocks and the key-value graph come out alike (the
//! SEC's own instance of a filing is generated from its inline report).
//!
//! The reader follows the Inline XBRL 1.1 specification as arelle applies it:
//! the taxonomy entry points are the `link:schemaRef`s of `ix:references`, the
//! contexts and units those of `ix:resources`, and the facts every
//! `ix:nonFraction` and `ix:nonNumeric` in document order — the hidden ones
//! of `ix:header` included, a fact nested in another one counted on its own.
//! A numeric fact's value is its text through its `format` transformation,
//! times ten to its `scale`, negated by `sign="-"`; a non-numeric one's is
//! its text, or, with `escape="true"` (a text block), its markup as HTML,
//! each continued by the `ix:continuation` its `continuedAt` names, without
//! the `ix:exclude`d parts.

use std::collections::HashMap;

use roxmltree::Node as XmlNode;

use super::xbrl::{periods, unit_measures, Fact, Filing};
use super::xbrl_dts::{LINK_NS, XLINK_NS};

/// The Inline XBRL namespaces, 1.0 (2008) and 1.1 (2013).
const IX_NAMESPACES: [&str; 2] = [
    "http://www.xbrl.org/2013/inlineXBRL",
    "http://www.xbrl.org/2008/inlineXBRL",
];
const XSI_NS: &str = "http://www.w3.org/2001/XMLSchema-instance";

/// The bytes the format sniff reads (docling's `_CONTENT_SNIFF_BYTES`).
const SNIFF_BYTES: usize = 8192;

/// docling's `is_ixbrl` (docling#4689): whether a document is an inline XBRL
/// report, from its first 8 KiB — the **root** `html`/`xhtml` element (any
/// prefix) declares an inline XBRL namespace, 2008 or 2013, in either quote
/// style. Comments, processing instructions and the doctype before it are
/// skipped, so a page that only mentions the namespace, or carries it in a
/// comment or on a nested element, stays HTML.
pub(crate) fn is_ixbrl(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(SNIFF_BYTES)]);
    let prolog = cached_regex!(r"(?is)<!--.*?-->|<\?.*?\?>|<!DOCTYPE[^>]*>");
    let stripped = prolog.replace_all(&head, "");
    let Some(root) = cached_regex!(r"(?i)<(?:[\w.-]+:)?(?:html|xhtml)\b[^>]*>").find(&stripped)
    else {
        return false;
    };
    cached_regex!(r#"\bxmlns:[\w.-]+\s*=\s*["']http://www\.xbrl\.org/(?:2008|2013)/inlineXBRL["']"#)
        .is_match(root.as_str())
}

/// Whether a parsed root is an inline report: an `html` element declaring
/// an inline XBRL namespace.
pub(super) fn is_inline_root(root: XmlNode) -> bool {
    root.tag_name().name().eq_ignore_ascii_case("html")
        && root
            .namespaces()
            .any(|ns| IX_NAMESPACES.contains(&ns.uri()))
}

fn is_ix(node: XmlNode, name: &str) -> bool {
    node.is_element()
        && node.tag_name().name() == name
        && node
            .tag_name()
            .namespace()
            .is_some_and(|ns| IX_NAMESPACES.contains(&ns))
}

/// The [`Filing`] of an inline report.
pub(super) fn inline_filing(root: XmlNode) -> Filing {
    let schema_refs = root
        .descendants()
        .filter(|n| is_ix(*n, "references"))
        .flat_map(|r| r.descendants())
        .filter(|n| n.tag_name().namespace() == Some(LINK_NS) && n.tag_name().name() == "schemaRef")
        .filter_map(|n| n.attribute((XLINK_NS, "href")))
        .map(str::to_string)
        .collect();
    let mut contexts = HashMap::new();
    let mut units = HashMap::new();
    for resources in root.descendants().filter(|n| is_ix(*n, "resources")) {
        contexts.extend(periods(resources));
        units.extend(unit_measures(resources));
    }
    let continuations: HashMap<&str, XmlNode> = root
        .descendants()
        .filter(|n| is_ix(*n, "continuation"))
        .filter_map(|n| Some((n.attribute("id")?, n)))
        .collect();
    let facts = root
        .descendants()
        .filter(|n| is_ix(*n, "nonFraction") || is_ix(*n, "nonNumeric"))
        .filter_map(|n| inline_fact(n, &continuations))
        .collect();
    Filing {
        schema_refs,
        facts,
        contexts,
        units,
    }
}

fn inline_fact(node: XmlNode, continuations: &HashMap<&str, XmlNode>) -> Option<Fact> {
    let name = node.attribute("name")?.trim();
    let (prefix, local) = match name.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None => (None, name),
    };
    let namespace = node.lookup_namespace_uri(prefix).unwrap_or("");
    let nil = node
        .attribute((XSI_NS, "nil"))
        .is_some_and(|v| matches!(v.trim(), "true" | "1"));
    let numeric = node.tag_name().name() == "nonFraction";
    let format = node.attribute("format").map(str::trim);
    let value = if nil {
        String::new()
    } else if numeric {
        let text = plain_text(node, &HashMap::new());
        numeric_value(
            &text,
            format,
            node.attribute("scale"),
            node.attribute("sign"),
        )
    } else if node
        .attribute("escape")
        .is_some_and(|v| matches!(v.trim(), "true" | "1"))
    {
        let mut html = String::new();
        continued(node, continuations, |part| {
            serialize_children(part, &mut html)
        });
        html.trim().to_string()
    } else {
        let text = plain_text(node, continuations);
        match format {
            Some(format) => {
                transform_text(text.trim(), format).unwrap_or_else(|| TRANSFORM_ERROR.to_string())
            }
            None => text.trim().to_string(),
        }
    };
    Some(Fact {
        namespace: namespace.to_string(),
        local: local.to_string(),
        prefix: prefix.map(str::to_string),
        value,
        context_ref: node.attribute("contextRef").map(str::to_string),
        unit_ref: numeric
            .then(|| node.attribute("unitRef").map(str::to_string))
            .flatten(),
        decimals: node.attribute("decimals").map(str::to_string),
    })
}

/// arelle's value for a fact its transformation cannot read.
const TRANSFORM_ERROR: &str = "(ixTransformValueError)";

/// `node`, then each `ix:continuation` its `continuedAt` chain names (a
/// cycle stops at the first repeat).
fn continued<'a, 'i>(
    node: XmlNode<'a, 'i>,
    continuations: &HashMap<&str, XmlNode<'a, 'i>>,
    mut visit: impl FnMut(XmlNode<'a, 'i>),
) {
    let mut seen = Vec::new();
    let mut current = Some(node);
    while let Some(part) = current {
        visit(part);
        current = part
            .attribute("continuedAt")
            .map(str::trim)
            .filter(|id| !seen.contains(id))
            .and_then(|id| {
                seen.push(id);
                continuations.get(id).copied()
            });
    }
}

/// The text of a fact and its continuations, `ix:exclude`d parts left out.
fn plain_text(node: XmlNode, continuations: &HashMap<&str, XmlNode>) -> String {
    fn walk(node: XmlNode, out: &mut String) {
        for child in node.children() {
            if child.is_text() {
                out.push_str(child.text().unwrap_or(""));
            } else if child.is_element() && !is_ix(child, "exclude") {
                walk(child, out);
            }
        }
    }
    let mut out = String::new();
    continued(node, continuations, |part| walk(part, &mut out));
    out
}

/// HTML void elements, written without an end tag (`</br>` parses as a
/// second `<br>`).
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// The markup under `node` as HTML: XHTML elements by their local names with
/// their attributes, the `ix:` tags unwrapped (a nested fact keeps its text)
/// and `ix:exclude` dropped — what the text block of the filing's instance
/// holds, escaped.
fn serialize_children(node: XmlNode, out: &mut String) {
    for child in node.children() {
        if child.is_text() {
            escape_into(child.text().unwrap_or(""), false, out);
        } else if child.is_element() {
            if child
                .tag_name()
                .namespace()
                .is_some_and(|ns| IX_NAMESPACES.contains(&ns))
            {
                if child.tag_name().name() != "exclude" {
                    serialize_children(child, out);
                }
                continue;
            }
            let tag = child.tag_name().name();
            out.push('<');
            out.push_str(tag);
            for attr in child.attributes() {
                out.push(' ');
                out.push_str(attr.name());
                out.push_str("=\"");
                escape_into(attr.value(), true, out);
                out.push('"');
            }
            if VOID.contains(&tag) {
                out.push_str("/>");
                continue;
            }
            out.push('>');
            serialize_children(child, out);
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

fn escape_into(text: &str, attribute: bool, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// A numeric fact's value as arelle's `ModelInlineFact.value` writes it:
/// the transformed text in canonical form (`canonicalNumber`: `0.20` →
/// `0.2`, `10.00` → `10`) as a `Decimal`, times `10 ** Decimal(scale)` —
/// which keeps the exponent a negative scale adds (`80` at scale -2 is
/// `0.80`) — negated by `sign="-"`, an integral product written without its
/// fraction unless the text held `.0`, then `"{:f}"`. Unreadable text gives
/// arelle's `(ixTransformValueError)`.
fn numeric_value(
    text: &str,
    format: Option<&str>,
    scale: Option<&str>,
    sign: Option<&str>,
) -> String {
    let text = text.trim();
    let number = match format.map(|f| f.rsplit(':').next().unwrap_or(f)) {
        None => Some(text.to_string()).filter(|t| is_decimal(t)),
        Some(f) => transform_number(text, f),
    };
    let Some(number) = number.as_deref().map(canonical_number) else {
        return TRANSFORM_ERROR.to_string();
    };
    let scale = scale.and_then(|s| s.trim().parse::<i64>().ok());
    // The Decimal: coefficient digits and exponent.
    let (int, frac) = number.split_once('.').unwrap_or((&number, ""));
    let mut digits = format!("{int}{frac}");
    let mut exponent = -(frac.len() as i64);
    if let Some(scale) = scale {
        if scale >= 0 {
            digits.push_str(&"0".repeat(scale as usize));
        } else {
            exponent += scale;
        }
    }
    let point = digits.len() as i64 + exponent;
    let (mut int, mut frac) = if exponent >= 0 {
        (
            format!("{digits}{}", "0".repeat(exponent as usize)),
            String::new(),
        )
    } else if point <= 0 {
        (
            String::new(),
            format!("{}{digits}", "0".repeat((-point) as usize)),
        )
    } else {
        (
            digits[..point as usize].to_string(),
            digits[point as usize..].to_string(),
        )
    };
    let integral = frac.bytes().all(|b| b == b'0');
    if integral && !number.contains(".0") {
        frac.clear();
    }
    int = int.trim_start_matches('0').to_string();
    if int.is_empty() {
        int.push('0');
    }
    let mut value = if frac.is_empty() {
        int
    } else {
        format!("{int}.{frac}")
    };
    if sign.is_some_and(|s| s.trim() == "-") {
        value.insert(0, '-');
    }
    value
}

/// arelle's `canonicalNumber`: leading zeros and trailing fraction zeros
/// dropped (`0.20` → `0.2`, `010.00` → `10`, `.5` → `0.5`).
fn canonical_number(number: &str) -> String {
    let (int, frac) = number.split_once('.').unwrap_or((number, ""));
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    if frac.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac}")
    }
}

fn is_decimal(text: &str) -> bool {
    let mut parts = text.splitn(2, '.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next().unwrap_or("");
    !(int.is_empty() && frac.is_empty())
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
}

/// The numeric transformations of the `ixt` and `ixt-sec` registries the
/// SEC's filings use, by local name. `None` when the text does not read.
fn transform_number(text: &str, format: &str) -> Option<String> {
    let digits_only = |s: &str, decimal: char| -> Option<String> {
        let mut out = String::new();
        for c in s.chars() {
            if c.is_ascii_digit() {
                out.push(c);
            } else if c == decimal {
                out.push('.');
            } else if !matches!(c, ',' | '.' | ' ' | '\u{a0}' | '\'' | '\u{2009}') {
                return None;
            }
        }
        Some(out).filter(|o| is_decimal(o))
    };
    match format {
        "num-dot-decimal" | "numdotdecimal" | "numcommadot" | "numspacedot" => {
            digits_only(text, '.')
        }
        "num-comma-decimal" | "numcommadecimal" | "numdotcomma" | "numspacecomma" => {
            digits_only(text, ',')
        }
        "fixed-zero" | "zerodash" | "numdash" => Some("0".into()),
        "num-unit-decimal" | "numunitdecimal" => {
            let groups: Vec<&str> = cached_regex!(r"\d+")
                .find_iter(text)
                .map(|m| m.as_str())
                .collect();
            match groups.as_slice() {
                [int] => Some(int.to_string()),
                [int, frac] => Some(format!("{int}.{frac:0>2}")),
                _ => None,
            }
        }
        // ixt-sec's English number words (`one`, `twenty-three`, `no`):
        // arelle implements none of them and reports a transformation
        // error; the SEC's own instance of the filing holds the number, and
        // so do we (a deliberate divergence, docs/MIGRATION.md).
        "numwordsen" => number_words(text).map(|n| n.to_string()),
        _ => None,
    }
}

/// English number words to a number: `one`, `twenty-three`, `one hundred
/// and five`, `no` / `none` (zero).
fn number_words(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_lowercase();
    if matches!(text.as_str(), "no" | "none" | "zero") {
        return Some(0);
    }
    const SMALL: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = [
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    let (mut total, mut current) = (0u64, 0u64);
    let mut any = false;
    for word in text
        .split(|c: char| c.is_whitespace() || c == '-' || c == ',')
        .filter(|w| !w.is_empty() && *w != "and")
    {
        any = true;
        if let Some(n) = SMALL.iter().position(|w| *w == word) {
            current += n as u64;
        } else if let Some(n) = TENS.iter().position(|w| *w == word) {
            current += 20 + 10 * n as u64;
        } else {
            let factor = match word {
                "hundred" => 100,
                "thousand" => 1_000,
                "million" => 1_000_000,
                "billion" => 1_000_000_000,
                _ => return None,
            };
            if factor == 100 {
                current = current.max(1) * 100;
            } else {
                total += current.max(1) * factor;
                current = 0;
            }
        }
    }
    any.then_some(total + current)
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// A month by its English name or a prefix of at least three letters.
fn month_number(word: &str) -> Option<usize> {
    let word = word.trim_end_matches('.').to_ascii_lowercase();
    (word.len() >= 3)
        .then(|| MONTHS.iter().position(|m| m.starts_with(&word)))
        .flatten()
        .map(|i| i + 1)
}

/// The non-numeric transformations a filing's title and flags use, by
/// local name; `None` when the text does not read.
fn transform_text(text: &str, format: &str) -> Option<String> {
    let local = format.rsplit(':').next().unwrap_or(format);
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .collect();
    let day = |w: &str| {
        w.trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .parse::<u32>()
            .ok()
            .filter(|d| (1..=31).contains(d))
    };
    match local {
        "fixed-true" | "booleantrue" => Some("true".into()),
        "fixed-false" | "booleanfalse" => Some("false".into()),
        "fixed-empty" => Some(String::new()),
        "boolballotbox" => match text.trim() {
            "\u{2612}" | "\u{2611}" => Some("true".into()),
            "\u{2610}" => Some("false".into()),
            _ => None,
        },
        "date-monthname-day-year-en" | "datemonthdayyearen" => match words.as_slice() {
            [month, d, year] => {
                let (m, d, y) = (month_number(month)?, day(d)?, year.parse::<u32>().ok()?);
                Some(format!("{y:04}-{m:02}-{d:02}"))
            }
            _ => None,
        },
        "date-day-monthname-year-en" | "datedaymonthyearen" => match words.as_slice() {
            [d, month, year] => {
                let (m, d, y) = (month_number(month)?, day(d)?, year.parse::<u32>().ok()?);
                Some(format!("{y:04}-{m:02}-{d:02}"))
            }
            _ => None,
        },
        "date-monthname-day-en" | "datemonthdayen" => match words.as_slice() {
            [month, d] => Some(format!("--{:02}-{:02}", month_number(month)?, day(d)?)),
            _ => None,
        },
        // The SEC's other non-numeric transformations (durations, exchange
        // and country names, filer categories) feed no part of the document:
        // only numeric facts reach the graph, and the title takes the
        // document type, the registrant and the period end date. Their text
        // stays as written.
        _ => Some(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "http://www.xbrl.org/2013/inlineXBRL";

    /// docling#4689 (`test_guess_format_ixbrl_markers`).
    #[test]
    fn only_a_root_declaring_the_namespace_is_ixbrl() {
        let late: String = (0..30)
            .map(|i| format!(r#" xmlns:p{i}="http://example.org/ns/{i}""#))
            .collect();
        let yes = [
            format!(
                r#"<?xml version="1.0"?><!-- header -->
<html xmlns="http://www.w3.org/1999/xhtml"{late} xmlns:ix="{NS}"><body/></html>"#
            ),
            "<html xmlns:ixbrl='http://www.xbrl.org/2008/inlineXBRL'><body/></html>".into(),
        ];
        let no = [
            format!(r#"<html><body><p>Declare xmlns:ix="{NS}" on the root.</p></body></html>"#),
            format!(r#"<!-- <html xmlns:ix="{NS}"> --><html><body/></html>"#),
            format!(r#"<html><body><div xmlns:ix="{NS}"/></body></html>"#),
            "<!DOCTYPE html><html><body>text</body></html>".into(),
        ];
        for page in yes {
            assert!(is_ixbrl(page.as_bytes()), "{page}");
        }
        for page in no {
            assert!(!is_ixbrl(page.as_bytes()), "{page}");
        }
    }

    #[test]
    fn numeric_values_are_transformed_scaled_and_signed() {
        let v = |t, f, s, g| numeric_value(t, Some(f), s, g);
        assert_eq!(
            v("239,160,600", "ixt:num-dot-decimal", Some("0"), None),
            "239160600"
        );
        assert_eq!(v("1.2", "ixt:num-dot-decimal", Some("6"), None), "1200000");
        assert_eq!(v("5.5", "ixt:num-dot-decimal", Some("-2"), None), "0.055");
        // arelle's Decimal arithmetic: the canonical text, the exponent a
        // negative scale adds, an integral product without its fraction.
        assert_eq!(v("0.20", "ixt:num-dot-decimal", Some("0"), None), "0.2");
        assert_eq!(v("10.00", "ixt:num-dot-decimal", Some("0"), None), "10");
        assert_eq!(v("80", "ixt:num-dot-decimal", Some("-2"), None), "0.80");
        assert_eq!(v("100", "ixt:num-dot-decimal", Some("-2"), None), "1");
        assert_eq!(v("23.2", "ixt:num-dot-decimal", Some("-2"), None), "0.232");
        assert_eq!(v("1.05", "ixt:num-dot-decimal", Some("2"), None), "105.00");
        assert_eq!(v("1,000", "ixt:num-dot-decimal", None, Some("-")), "-1000");
        // `Decimal(0) * -1` is `-0`, which arelle writes as it is.
        assert_eq!(v("—", "ixt:fixed-zero", None, Some("-")), "-0");
        assert_eq!(v("—", "ixt:fixed-zero", None, None), "0");
        assert_eq!(v("one", "ixt-sec:numwordsen", None, None), "1");
        assert_eq!(v("twenty-three", "ixt-sec:numwordsen", None, None), "23");
        assert_eq!(v("n/a", "ixt:num-dot-decimal", None, None), TRANSFORM_ERROR);
        assert_eq!(
            numeric_value("1,5", Some("ixt:num-comma-decimal"), None, None),
            "1.5"
        );
    }

    #[test]
    fn dates_and_flags_are_transformed() {
        let t = |text, f| transform_text(text, f).unwrap();
        assert_eq!(
            t("December 31, 2025", "ixt:date-monthname-day-year-en"),
            "2025-12-31"
        );
        assert_eq!(t("Dec. 31", "ixt:date-monthname-day-en"), "--12-31");
        assert_eq!(t("\u{2612}", "ixt-sec:boolballotbox"), "true");
        assert_eq!(t("No", "ixt:fixed-false"), "false");
    }
}

/// The format detection and the filing parity of docling#4689.
#[cfg(test)]
mod filing_tests {
    use std::path::{Path, PathBuf};

    use crate::format::InputFormat;
    use crate::source::SourceDocument;
    use crate::DocumentConverter;

    fn data() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/xbrl")
    }

    const REPORT: &str = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:ix="http://www.xbrl.org/2013/inlineXBRL"><head><title>Report</title></head><body><p>Hello</p></body></html>"#;

    /// `test_guess_format_ixbrl_markers`, by name and as a stream: an
    /// `.htm`/`.html` whose root declares the namespace is XBRL.
    #[test]
    fn an_inline_report_is_typed_xbrl() {
        for (name, page, want) in [
            ("report.htm", REPORT, InputFormat::XmlXbrl),
            ("report.html", REPORT, InputFormat::XmlXbrl),
            (
                "plain.html",
                "<html><body>text</body></html>",
                InputFormat::Html,
            ),
        ] {
            let stream = SourceDocument::from_named_bytes(name, page.into(), None).unwrap();
            assert_eq!(stream.format, want, "{name}");
            let dir = std::env::temp_dir().join(format!("docling-ixbrl-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, page).unwrap();
            assert_eq!(
                SourceDocument::from_file(&path).unwrap().format,
                want,
                "{name}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
        // A nameless page is sniffed as HTML, then read by its root.
        let nameless = SourceDocument::from_named_bytes("report", REPORT.into(), None).unwrap();
        assert_eq!(nameless.format, InputFormat::XmlXbrl);
    }

    /// `test_html_only_converter_skips_ixbrl_report`: a converter allowed
    /// HTML alone does not read a filing as a web page.
    #[test]
    fn an_html_only_converter_skips_an_inline_report() {
        let converter = DocumentConverter::with_allowed_formats([InputFormat::Html]);
        let src = SourceDocument::from_named_bytes("report.htm", REPORT.into(), None).unwrap();
        assert_eq!(src.format, InputFormat::XmlXbrl);
        assert!(converter.convert(src).is_err());
    }

    /// The fact rows of a document's key-value graph: each key cell that
    /// heads value cells, with its concept and the four value texts.
    fn fact_rows(json: &serde_json::Value) -> Vec<(String, String, Vec<String>)> {
        let cells = json["key_value_items"][0]["graph"]["cells"]
            .as_array()
            .unwrap();
        cells
            .iter()
            .enumerate()
            .filter(|(i, c)| {
                c["label"] == "key"
                    && cells
                        .get(i + 1)
                        .and_then(|n| n["text"].as_str())
                        .is_some_and(|t| t.starts_with("value: "))
            })
            .map(|(i, c)| {
                (
                    c["text"].as_str().unwrap().to_string(),
                    c["orig"].as_str().unwrap().to_string(),
                    (1..5)
                        .map(|k| cells[i + k]["text"].as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect()
    }

    /// `test_ixbrl_report_matches_xbrl_instance_of_same_filing`: the inline
    /// report and the instance of one filing give the same Markdown, the
    /// same 23 tables and the same 319 facts. docling differs on three
    /// `ixt-sec:numwordsen` facts arelle cannot read
    /// (`(ixTransformValueError)`); we read them, so ours are equal
    /// throughout — the whole document but its name.
    #[test]
    fn an_inline_report_matches_the_instance_of_its_filing() {
        let taxonomy = data().join("sources/mlac-taxonomy");
        let convert = |path: PathBuf| {
            DocumentConverter::new()
                .xbrl_taxonomy(&taxonomy)
                .compact_tables(true)
                .convert(SourceDocument::from_file(path).unwrap())
                .unwrap()
                .document
        };
        let inline = convert(data().join("ixbrl/mlac-20251231.htm"));
        let instance = convert(data().join("sources/mlac-20251231.xml"));
        assert_eq!(inline.export_to_markdown(), instance.export_to_markdown());
        let (ij, xj) = (
            inline.export_to_json_value(),
            instance.export_to_json_value(),
        );
        assert_eq!(ij["tables"].as_array().unwrap().len(), 23);
        assert_eq!(ij["tables"], xj["tables"]);
        assert_eq!(ij["texts"], xj["texts"]);
        let (ir, xr) = (fact_rows(&ij), fact_rows(&xj));
        assert_eq!(ir.len(), 319);
        assert_eq!(ir, xr);
        let float = ir.iter().find(|r| r.1 == "dei:EntityPublicFloat").unwrap();
        assert_eq!(
            float.2,
            [
                "value: 239160600",
                "period: 2025-06-30",
                "currency: USD",
                "decimals: 0"
            ]
        );
        for concept in [
            "us-gaap:StockIssuedDuringPeriodSharesConversionOfUnits",
            "mlac:NumberOfOrdinaryShares",
            "us-gaap:NumberOfReportableSegments",
        ] {
            let row = ir.iter().find(|r| r.1 == concept).unwrap();
            assert_eq!(row.2[0], "value: 1", "{concept}");
        }
        assert_eq!(ij["key_value_items"], xj["key_value_items"]);
    }
}
