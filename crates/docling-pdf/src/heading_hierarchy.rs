//! Section-header level inference for the PDF/image pipeline (#302 — the
//! port of docling's `HeadingHierarchyModel`).
//!
//! The layout model classifies regions as `section_header` without a level,
//! so every heading the PDF path emits lands at the same depth and the
//! document hierarchy is flattened (Roman-numeral parts and Arabic-numeral
//! subsections collapse together). When enabled, this stage runs on the
//! assembled document — right after reading-order assembly, like docling's —
//! and assigns each heading a level from, in precedence order:
//!
//! 1. **bookmarks** — the PDF outline ([`crate::outline`]), the document's
//!    own declared hierarchy. Bookmarks are fuzzily matched (title + page) to
//!    detected headings; a confidently matched heading takes the bookmark's
//!    depth, and a confidently matched *list item* is promoted to a heading
//!    (layout models often mis-classify a heading as a list item).
//! 2. **numbering** — legal/outline numbering such as `PART I → 1. → 1.1 →
//!    (a) → (i)`. The primary signal for headings without a bookmark match.
//! 3. **style** — the heading's visual style, read from the PDF text layer's
//!    glyphs ([`GlyphStyle`], gathered by the pdfium backend): font size
//!    first — with near-equal sizes merged, since the measured height of the
//!    same font varies with descenders — then weight, slant and letter case.
//!
//! Apart from promoting a confidently bookmark-matched list item, the stage
//! only rewrites heading levels — it never adds, removes or reorders items,
//! and headings with no applicable signal keep their level. Docling's
//! semantic level `N` corresponds to our [`Node::Heading`] `level: N + 1`
//! (docling's Markdown serializer renders `section_header` level 1 as `##`,
//! which is exactly what our assembler already emits for every heading).
//!
//! Divergence from docling noted for the record: style is aggregated over
//! *glyphs* rather than parsed text-line cells (same signal, finer
//! granularity), and OCR-only headings carry no style at all (docling reads
//! OCR cell heights; our glyph pass reads the digital text layer only).

use std::collections::HashMap;

use docling_core::Node;

use crate::outline::OutlineItem;

/// Options for the heading-hierarchy stage (docling's
/// `HeadingHierarchyOptions`, defaults included).
#[derive(Clone, Debug)]
pub struct HeadingHierarchyOptions {
    /// Master switch. Off by default (docling parity): all detected headings
    /// keep the assembler's level and the output is byte-for-byte unchanged.
    pub enabled: bool,
    /// Use the PDF outline (bookmarks/ToC) as the authoritative signal.
    pub use_bookmarks: bool,
    /// Use legal/outline numbering for headings without a bookmark match.
    pub use_numbering: bool,
    /// Use visual style (font size, and below) as the last-resort signal.
    pub use_style: bool,
    /// Refine the style fallback with font weight/slant (from the embedded
    /// font names) and all-caps detection.
    pub use_font_style: bool,
    /// Relative difference below which two heading font sizes count as one
    /// size (absorbs descender-driven measurement noise).
    pub style_size_tolerance: f32,
    /// Maximum semantic heading level to assign; deeper levels clamp.
    pub max_level: u8,
    /// Minimum fuzzy title similarity (0..1) for a bookmark to match a
    /// heading/list item.
    pub bookmark_match_threshold: f32,
    /// Override of the numbering-scheme precedence (highest level first);
    /// known schemes: `part`, `chapter`, `division`, `subdivision`,
    /// `article`, `roman_u`, `arabic`,
    /// `alpha_u`, `alpha_l`, `roman_l`. `None` = the default legal ordering.
    pub numbering_schemes: Option<Vec<String>>,
}

impl Default for HeadingHierarchyOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            use_bookmarks: true,
            use_numbering: true,
            use_style: true,
            use_font_style: true,
            style_size_tolerance: 0.05,
            max_level: 6,
            bookmark_match_threshold: 0.8,
            numbering_schemes: None,
        }
    }
}

impl HeadingHierarchyOptions {
    /// The plumbed-everywhere form: default options with the master switch.
    pub fn enabled(on: bool) -> Self {
        Self {
            enabled: on,
            ..Self::default()
        }
    }
}

/// One text-layer glyph's box and style, in top-left page points — the
/// stage's input for the style signal, gathered per page by the pdfium
/// backend (each glyph counts as one character in the style vote).
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphStyle {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
    /// Line height (font ascent + descent at the glyph's size) — the
    /// font-size proxy, matching docling's text-line cell height.
    pub height: f32,
    /// 0 light/regular, 1 medium/semibold, 2 bold+ (from the font name).
    pub weight_cls: u8,
    pub italic: bool,
    /// Whether the font name carried any recognizable style at all.
    pub styled: bool,
}

/// Default precedence of numbering schemes, highest hierarchy level first.
/// `dotted` shares the `arabic` rank and orders below it by segment depth.
const DEFAULT_FAMILY_ORDER: [&str; 10] = [
    "part",        // PART I / TITLE I / BOOK I
    "chapter",     // CHAPTER 1
    "division",    // DIVISION I / Division 1 / Division A (docling#4473)
    "subdivision", // SUBDIVISION A / Subdivision a
    "article",     // ARTICLE 1 / SECTION 1 / Clause / § 1
    "roman_u",     // I. II. III.
    "arabic",      // 1. 2. 3.  (and dotted 1.1, 1.1.1 by depth)
    "alpha_u",     // A. B. C.
    "alpha_l",     // (a) (b) (c)
    "roman_l",     // (i) (ii) (iii)
];

// ------------------------------------------------------------------ markers

/// A parsed leading numbering marker.
#[derive(Clone, Debug, PartialEq)]
struct Marker {
    family: &'static str,
    /// Dotted-decimal segment count; 1 for everything else.
    depth: usize,
    /// Raw alpha/Roman token, kept for ambiguity resolution.
    token: Option<String>,
    /// Single-letter Roman/alpha that needs document context to resolve.
    ambiguous: bool,
}

impl Marker {
    fn family(family: &'static str) -> Self {
        Marker {
            family,
            depth: 1,
            token: None,
            ambiguous: false,
        }
    }
}

/// Canonical Roman-numeral validator (1..3999), case-insensitive — the
/// hand-rolled equivalent of `M{0,4}(CM|CD|D?C{0,3})(XC|XL|L?X{0,3})
/// (IX|IV|V?I{0,3})` on a non-empty token.
fn is_roman(token: &str) -> bool {
    if token.is_empty() || !token.is_ascii() {
        return false;
    }
    let s: Vec<u8> = token.bytes().map(|b| b.to_ascii_uppercase()).collect();
    let mut i = 0;
    // M{0,4}
    let mut m = 0;
    while i < s.len() && s[i] == b'M' && m < 4 {
        i += 1;
        m += 1;
    }
    // (CM|CD|D?C{0,3})
    if s[i..].starts_with(b"CM") || s[i..].starts_with(b"CD") {
        i += 2;
    } else {
        if i < s.len() && s[i] == b'D' {
            i += 1;
        }
        let mut c = 0;
        while i < s.len() && s[i] == b'C' && c < 3 {
            i += 1;
            c += 1;
        }
    }
    // (XC|XL|L?X{0,3})
    if s[i..].starts_with(b"XC") || s[i..].starts_with(b"XL") {
        i += 2;
    } else {
        if i < s.len() && s[i] == b'L' {
            i += 1;
        }
        let mut x = 0;
        while i < s.len() && s[i] == b'X' && x < 3 {
            i += 1;
            x += 1;
        }
    }
    // (IX|IV|V?I{0,3})
    if s[i..].starts_with(b"IX") || s[i..].starts_with(b"IV") {
        i += 2;
    } else {
        if i < s.len() && s[i] == b'V' {
            i += 1;
        }
        let mut n = 0;
        while i < s.len() && s[i] == b'I' && n < 3 {
            i += 1;
            n += 1;
        }
    }
    i == s.len()
}

/// Whether `text` starts with `word` as a whole word (ASCII case-insensitive).
fn starts_with_word(text: &str, word: &str) -> bool {
    // `word.len()` is a byte count taken from a *different* string, so it can
    // land inside a multi-byte char of `text` — `Note 1\u{a0}Overview` against
    // the 7-byte `chapter` splits the no-break space at bytes 6..8 (#377), and
    // a direct slice panics there. `get` returns `None` instead, which is also
    // the right answer: `text` cannot start with an ASCII word of that length
    // if byte `word.len()` is mid-character.
    let Some(head) = text.get(..word.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(word) {
        return false;
    }
    text[word.len()..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_alphanumeric())
}

/// Classify a bare alpha/Roman token (`A`, `iv`, `i` …) into a marker.
fn classify_letter(token: &str) -> Option<Marker> {
    let upper = token.chars().all(|c| c.is_uppercase());
    if token.chars().count() == 1 {
        let is_roman_single = token
            .chars()
            .next()
            .is_some_and(|c| "IVXLCDMivxlcdm".contains(c));
        let family = match (is_roman_single, upper) {
            (true, true) => "roman_u",
            (true, false) => "roman_l",
            (false, true) => "alpha_u",
            (false, false) => "alpha_l",
        };
        return Some(Marker {
            family,
            depth: 1,
            token: Some(token.to_string()),
            ambiguous: is_roman_single,
        });
    }
    // Multi-letter tokens only count as numbering if they are valid Roman
    // numerals; otherwise they are plain words ("Summary."), not numbering.
    if is_roman(token) {
        return Some(Marker {
            family: if upper { "roman_u" } else { "roman_l" },
            depth: 1,
            token: Some(token.to_string()),
            ambiguous: false,
        });
    }
    None
}

/// What docling's `_KW_TAIL` (docling#5154) matches after a part/chapter/
/// article keyword: `Some(None)` when the keyword stands alone — the end, or
/// `.` `:` `–` `—` or `- ` after optional whitespace (`APPENDIX`, `Annex:`,
/// `Appendix – Data`) — `Some(Some(token))` for the whitespace-separated
/// token that follows (`[^\s.,:;)\]–—-]+`), `None` when the keyword is not
/// a whole word there (`Part-Year`, `Parts`, `Chapter, …`).
fn keyword_tail<'a>(s: &'a str, kw: &str) -> Option<Option<&'a str>> {
    // `get`: the keyword's byte length may fall inside a multi-byte char (#377).
    let head = s.get(..kw.len())?;
    if !head.eq_ignore_ascii_case(kw) {
        return None;
    }
    let rest = &s[kw.len()..];
    let after = rest.trim_start();
    if after.len() < rest.len() {
        let tok_len = after.find(keyword_token_end).unwrap_or(after.len());
        if tok_len > 0 {
            return Some(Some(&after[..tok_len]));
        }
    }
    let mut chars = after.chars();
    match chars.next() {
        None | Some('.' | ':' | '\u{2013}' | '\u{2014}') => Some(None),
        Some('-') if chars.next().is_some_and(char::is_whitespace) => Some(None),
        _ => None,
    }
}

/// The characters that end a keyword's enumerator token: whitespace and
/// docling's `.,:;)]–—-`.
fn keyword_token_end(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '.' | ',' | ':' | ';' | ')' | ']' | '\u{2013}' | '\u{2014}' | '-'
        )
}

/// docling's `_NUMBER_WORDS` (docling#5154): `CHAPTER ONE`, `ARTICLE FIRST`.
const NUMBER_WORDS: [&str; 47] = [
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
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "first",
    "second",
    "third",
    "fourth",
    "fifth",
    "sixth",
    "seventh",
    "eighth",
    "ninth",
    "tenth",
    "eleventh",
    "twelfth",
    "thirteenth",
    "fourteenth",
    "fifteenth",
    "sixteenth",
    "seventeenth",
    "eighteenth",
    "nineteenth",
    "twentieth",
];

/// docling's `_keyword_marker` (docling#5154): part/chapter/article keywords
/// are ordinary words too (`Rule Definition`, `Chapter Summary`, `Schedule of
/// Fees`), so one is numbering only when it stands alone or the next token
/// holds a digit (`Chapter 2D`, `Rule 12(b)`), is one letter, a valid Roman
/// numeral or a number word — otherwise the plain word would outrank the
/// heading's real `1.` / `2.1` numbering.
fn keyword_marker(tail: Option<Option<&str>>) -> bool {
    match tail {
        None => false,
        Some(None) => true,
        Some(Some(tok)) => {
            tok.chars().any(char::is_numeric)
                || tok.chars().count() == 1
                || is_roman(tok)
                || NUMBER_WORDS.contains(&tok.to_lowercase().as_str())
        }
    }
}

/// docling's `_KW_ENUM` + `_keyword_enumerator` (docling#4473): `Division`
/// / `Subdivision` are stricter still — an Arabic index, a single letter or a
/// valid Roman numeral must follow, terminated by whitespace, the end or
/// separator punctuation (so the French elision `Division d'appel` and
/// `Division Civil Remedies` are not markers).
fn keyword_enumerator(s: &str, kw: &str) -> bool {
    let Some(head) = s.get(..kw.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(kw) {
        return false;
    }
    let rest = &s[kw.len()..];
    let after = rest.trim_start();
    if after.len() == rest.len() {
        return false;
    }
    let tok = &after[..after.find(keyword_token_end).unwrap_or(after.len())];
    !tok.is_empty()
        && (tok.bytes().all(|b| b.is_ascii_digit())
            || (tok.len() == 1 && tok.as_bytes()[0].is_ascii_alphabetic())
            || is_roman(tok))
}

/// docling's `_KW_CHAPTER` lookahead: a statute citation such as `Chapter
/// A-15` (letter-dash-digit) is not a chapter heading.
fn chapter_citation(s: &str) -> bool {
    let Some(rest) = s.get("chapter".len()..) else {
        return false;
    };
    let after = rest.trim_start();
    let b = after.as_bytes();
    after.len() < rest.len()
        && b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b'-'
        && after[2..].starts_with(|c: char| c.is_numeric())
}

/// Extract the leading numbering marker from a heading, or `None`.
fn parse_marker(text: &str) -> Option<Marker> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }

    // docling's order: part, division, subdivision, chapter, article.
    if ["part", "title", "book"]
        .iter()
        .any(|kw| keyword_marker(keyword_tail(s, kw)))
    {
        return Some(Marker::family("part"));
    }
    if keyword_enumerator(s, "division") {
        return Some(Marker::family("division"));
    }
    if keyword_enumerator(s, "subdivision") {
        return Some(Marker::family("subdivision"));
    }
    if keyword_marker(keyword_tail(s, "chapter")) && !chapter_citation(s) {
        return Some(Marker::family("chapter"));
    }
    if [
        "article", "section", "clause", "schedule", "annex", "appendix", "rule",
    ]
    .iter()
    .any(|kw| keyword_marker(keyword_tail(s, kw)))
    {
        return Some(Marker::family("article"));
    }
    // § 1 / §§ 1.2
    if s.starts_with('§') {
        let after = s.trim_start_matches('§').trim_start();
        if after.starts_with(|c: char| c.is_ascii_digit()) {
            return Some(Marker::family("article"));
        }
    }

    // docling 2.129 (docling#4204): every numeric/letter form may open with
    // `(`, and besides `.`/`)`/`]` a colon or a dash (`-`, `–`, `—`) that is
    // followed by whitespace or the end closes the marker (`1: Intro`,
    // `A - Scope`, `1.2 – Detail`).
    let body = s.strip_prefix('(').map(str::trim_start).unwrap_or(s);

    // Dotted decimal outline (1.1, 1.1.1, …) terminated by punct/space/end.
    if let Some((segments, rest)) = take_dotted(body) {
        if segments >= 2
            && (rest
                .chars()
                .next()
                .is_none_or(|c| matches!(c, '.' | ')' | ']') || c.is_whitespace())
                || separator_at(rest))
        {
            return Some(Marker {
                family: "dotted",
                depth: segments,
                token: None,
                ambiguous: false,
            });
        }
    }
    // Single Arabic index (1. / 1) / (1) / 1: / 1 -): `\d+\s*(?:\.(?!\d)|SEP)`.
    let digits = body.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = body[digits..].trim_start();
        let dot_not_decimal = rest
            .strip_prefix('.')
            .is_some_and(|r| !r.starts_with(|c: char| c.is_ascii_digit()));
        if dot_not_decimal || separator_at(rest) {
            return Some(Marker::family("arabic"));
        }
    }

    // Single/multi letter marker, optionally parenthesized: (a) / A. / (iv) /
    // A: / A -.
    let letters: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if !letters.is_empty() {
        let rest = body[letters.len()..].trim_start();
        if rest.starts_with('.') || separator_at(rest) {
            return classify_letter(&letters);
        }
    }
    None
}

/// docling's `_SEP`: `)` or `]`, or a colon / hyphen / en dash / em dash
/// followed by whitespace or the end of the text.
fn separator_at(rest: &str) -> bool {
    let mut chars = rest.chars();
    match chars.next() {
        Some(')' | ']') => true,
        Some(':' | '-' | '\u{2013}' | '\u{2014}') => chars.next().is_none_or(char::is_whitespace),
        _ => false,
    }
}

/// docling's `_BARE_ARABIC` (`^(\d+)\s+\S`): the number a heading opens with
/// when nothing but whitespace separates it from the title.
fn bare_arabic(text: &str) -> Option<u64> {
    let s = text.trim();
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &s[digits..];
    let ws = rest.len() - rest.trim_start().len();
    (ws > 0 && !rest[ws..].is_empty())
        .then(|| s[..digits].parse().ok())
        .flatten()
}

/// docling 2.129's `_resolve_bare_arabic` (docling#4179): a heading whose
/// only marker is a bare number (`1 Introduction`) is numbered when the
/// document carries a consecutive run starting at 1 with at least two
/// entries — explicit Arabic markers (`2.`) count as evidence too, dotted
/// sections and unnumbered headings may intervene, and a gap (`1`, `2`, `4`)
/// ends the run, leaving the `4` unrecognized. A leading number alone may be
/// a year or a quantity, hence the sequence requirement; a 1-based run of
/// incidental headings (`1 January`, `2 February`) can still be misread.
fn resolve_bare_arabic(heading_texts: &[&str], markers: &mut [Option<Marker>]) {
    let mut sequence: Vec<usize> = Vec::new();
    for (i, text) in heading_texts.iter().enumerate() {
        let explicit = match &markers[i] {
            Some(m) if m.family != "arabic" => continue,
            Some(_) => true,
            None => false,
        };
        let number = if explicit {
            let body = text
                .trim()
                .strip_prefix('(')
                .map(str::trim_start)
                .unwrap_or(text.trim());
            let digits = body.chars().take_while(|c| c.is_ascii_digit()).count();
            body[..digits].parse::<u64>().ok()
        } else {
            bare_arabic(text)
        };
        let Some(number) = number else {
            continue;
        };
        if number == 1 {
            sequence = vec![i];
        } else if !sequence.is_empty() && number == sequence.len() as u64 + 1 {
            sequence.push(i);
            // Confirm the first entry when 2 arrives, then only the newly
            // extended pair.
            let n = sequence.len();
            for &index in &sequence[n.saturating_sub(2)..] {
                if markers[index].is_none() {
                    markers[index] = Some(Marker::family("arabic"));
                }
            }
        } else {
            sequence.clear();
        }
    }
}

/// Parse a leading `N(.N)+` run: `(segment count, rest)`. `None` when the
/// text does not start with at least `N.N`.
fn take_dotted(s: &str) -> Option<(usize, &str)> {
    let mut rest = s;
    let mut segments = 0;
    loop {
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            break;
        }
        segments += 1;
        rest = &rest[digits..];
        match rest.strip_prefix('.') {
            // Only continue when a digit follows the dot — `1.` is arabic,
            // not dotted.
            Some(r) if r.starts_with(|c: char| c.is_ascii_digit()) => rest = r,
            _ => break,
        }
    }
    (segments >= 2).then_some((segments, rest))
}

/// Resolve single-letter Roman/alpha markers in place using document-wide
/// evidence: a lone `I.` is Roman when the document also contains unambiguous
/// Roman markers and alpha when it contains unambiguous alpha markers. When
/// evidence is absent or conflicting, `I`/`i` default to Roman (the common
/// legal case) and other letters to alpha.
fn resolve_ambiguous(markers: &mut [Option<Marker>]) {
    let has = |family: &str, ms: &[Option<Marker>]| {
        ms.iter()
            .flatten()
            .any(|m| !m.ambiguous && m.family == family)
    };
    let upper_roman = has("roman_u", markers);
    let upper_alpha = has("alpha_u", markers);
    let lower_roman = has("roman_l", markers);
    let lower_alpha = has("alpha_l", markers);

    for m in markers.iter_mut().flatten() {
        if !m.ambiguous {
            continue;
        }
        let Some(token) = m.token.as_deref() else {
            continue;
        };
        let upper = token.chars().all(|c| c.is_uppercase());
        let (has_roman, has_alpha) = if upper {
            (upper_roman, upper_alpha)
        } else {
            (lower_roman, lower_alpha)
        };
        let roman = if has_roman && !has_alpha {
            true
        } else if has_alpha && !has_roman {
            false
        } else {
            token == "I" || token == "i"
        };
        m.family = match (roman, upper) {
            (true, true) => "roman_u",
            (true, false) => "roman_l",
            (false, true) => "alpha_u",
            (false, false) => "alpha_l",
        };
        m.ambiguous = false;
    }
}

fn family_rank(family: &str, order: &[String]) -> usize {
    let key = if family == "dotted" { "arabic" } else { family };
    order.iter().position(|f| f == key).unwrap_or(order.len()) // unknown scheme → lowest priority
}

/// Map heading index → level from numbering markers (relative, compressed).
fn infer_from_numbering(
    heading_texts: &[&str],
    options: &HeadingHierarchyOptions,
) -> HashMap<usize, usize> {
    let order: Vec<String> = options
        .numbering_schemes
        .clone()
        .unwrap_or_else(|| DEFAULT_FAMILY_ORDER.iter().map(|s| s.to_string()).collect());
    let mut markers: Vec<Option<Marker>> = heading_texts.iter().map(|t| parse_marker(t)).collect();
    resolve_ambiguous(&mut markers);
    resolve_bare_arabic(heading_texts, &mut markers);

    let mut keys: HashMap<usize, (usize, usize)> = HashMap::new();
    for (i, m) in markers.iter().enumerate() {
        if let Some(m) = m {
            keys.insert(i, (family_rank(m.family, &order), m.depth));
        }
    }
    compress_keys(keys)
}

/// Compress the distinct sort keys actually present into contiguous 1-based
/// levels, so a document that starts at "1." is not forced to start deep.
fn compress_keys<K: Ord + Clone + std::hash::Hash>(
    keys: HashMap<usize, K>,
) -> HashMap<usize, usize> {
    let mut distinct: Vec<K> = keys.values().cloned().collect();
    distinct.sort();
    distinct.dedup();
    let level_of: HashMap<K, usize> = distinct
        .into_iter()
        .enumerate()
        .map(|(i, k)| (k, i + 1))
        .collect();
    keys.into_iter().map(|(i, k)| (i, level_of[&k])).collect()
}

// -------------------------------------------------------------------- style

/// Share of a heading's styled characters that must be italic to count.
const ITALIC_RATIO: f32 = 0.6;

/// Whether the text is written in capitals (ignoring digits and punctuation).
fn is_all_caps(text: &str) -> bool {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    letters.len() >= 4 && letters.iter().all(|c| c.is_uppercase())
}

/// The visual style of one heading — the ranking key of the style fallback.
#[derive(Clone, Copy, Debug)]
struct HeadingStyle {
    size: f32,
    weight_cls: u8,
    italic: bool,
    caps: bool,
}

/// Derive a heading's style from the glyphs overlapping its box. Weight and
/// slant are a character-weighted vote (a heading can mix a regular "1.1 "
/// with a bold title); glyphs without font style contribute size only, so the
/// ranking degrades to font size.
fn heading_style(
    bbox: [f32; 4],
    text: &str,
    glyphs: &[GlyphStyle],
    options: &HeadingHierarchyOptions,
) -> Option<HeadingStyle> {
    let [hl, ht, hr, hb] = bbox;
    let mut heights: Vec<f32> = Vec::new();
    let mut weights = [0usize; 3];
    let mut styled_chars = 0usize;
    let mut italic_chars = 0usize;
    for g in glyphs {
        if g.l < hr && g.r > hl && g.t < hb && g.b > ht {
            heights.push(g.height);
            if options.use_font_style && g.styled {
                weights[g.weight_cls.min(2) as usize] += 1;
                styled_chars += 1;
                if g.italic {
                    italic_chars += 1;
                }
            }
        }
    }
    if heights.is_empty() {
        return None;
    }
    heights.sort_by(f32::total_cmp);
    let size = if heights.len() % 2 == 1 {
        heights[heights.len() / 2]
    } else {
        (heights[heights.len() / 2 - 1] + heights[heights.len() / 2]) / 2.0
    };
    if !options.use_font_style {
        return Some(HeadingStyle {
            size,
            weight_cls: 0,
            italic: false,
            caps: false,
        });
    }
    // On a tie, the heavier class wins: emphasis makes a heading stand out.
    let weight_cls = (0u8..3)
        .max_by_key(|&cls| (weights[cls as usize], cls))
        .unwrap_or(0);
    Some(HeadingStyle {
        size,
        weight_cls,
        italic: styled_chars > 0 && italic_chars as f32 / styled_chars as f32 >= ITALIC_RATIO,
        caps: is_all_caps(text),
    })
}

/// Group font sizes into clusters (largest first) and map each size to its
/// cluster index; consecutive sizes within `tolerance` (relative) merge, to
/// absorb descender-driven measurement noise.
fn cluster_sizes(mut sizes: Vec<f32>, tolerance: f32) -> Vec<(f32, usize)> {
    sizes.sort_by(|a, b| b.total_cmp(a));
    sizes.dedup();
    let mut clusters = Vec::with_capacity(sizes.len());
    let mut index = 0usize;
    let mut previous: Option<f32> = None;
    for size in sizes {
        if let Some(prev) = previous {
            if (prev - size) > tolerance * prev {
                index += 1;
            }
        }
        clusters.push((size, index));
        previous = Some(size);
    }
    clusters
}

/// Map heading index → level from heading styles (most prominent = level 1).
fn infer_from_style(
    headings: &[HeadingRef],
    glyph_styles: &HashMap<usize, Vec<GlyphStyle>>,
    options: &HeadingHierarchyOptions,
) -> HashMap<usize, usize> {
    if glyph_styles.is_empty() {
        return HashMap::new();
    }
    let mut styles: HashMap<usize, HeadingStyle> = HashMap::new();
    for (i, h) in headings.iter().enumerate() {
        let Some(glyphs) = glyph_styles.get(&h.page_no) else {
            continue;
        };
        let Some(bbox) = h.bbox_points else { continue };
        if let Some(style) = heading_style(bbox, &h.text, glyphs, options) {
            styles.insert(i, style);
        }
    }
    if styles.is_empty() {
        return HashMap::new();
    }
    let clusters = cluster_sizes(
        styles.values().map(|s| s.size).collect(),
        options.style_size_tolerance,
    );
    let cluster_of = |size: f32| -> usize {
        clusters
            .iter()
            .find(|(s, _)| *s == size)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    };
    // Order by size cluster, then by how much the heading stands out within
    // its size: heavier before lighter, upright before italic, capitals
    // before mixed case.
    let keys: HashMap<usize, (usize, i8, bool, bool)> = styles
        .into_iter()
        .map(|(i, s)| {
            (
                i,
                (cluster_of(s.size), -(s.weight_cls as i8), s.italic, !s.caps),
            )
        })
        .collect();
    compress_keys(keys)
}

// ---------------------------------------------------------------- bookmarks

/// Lower-case, collapse whitespace, trim outer punctuation for matching.
fn norm(text: &str) -> String {
    let collapsed = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    collapsed
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_string()
}

/// Strip one leading numbering marker before fuzzy-matching a title, so a
/// bookmark "Definitions" matches an on-page heading "1.1 Definitions" (the
/// port of docling's `_LEADING_MARKER`).
fn strip_marker(text: &str) -> String {
    let s = text.trim_start();
    let matched_len = leading_marker_len(s);
    match matched_len {
        Some(n) => {
            let rest = &s[n..];
            let trimmed = rest.trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '.' | ':' | ')' | '-' | '\u{2013}' | '\u{2014}')
            });
            trimmed.to_string()
        }
        None => text.to_string(),
    }
}

/// Byte length of the leading marker in `s`, `None` when there is none.
fn leading_marker_len(s: &str) -> Option<usize> {
    // Keyword + optional `[\s.:]*[0-9ivxlcdm]*` tail.
    for kw in [
        "chapter",
        "article",
        "section",
        "clause",
        "schedule",
        "annex",
        "appendix",
        "rule",
        "part",
        "title",
        "book",
        "division",
        "subdivision",
    ] {
        if starts_with_word(s, kw) {
            let mut i = kw.len();
            let bytes = s.as_bytes();
            while i < bytes.len()
                && (bytes[i].is_ascii_whitespace() || bytes[i] == b'.' || bytes[i] == b':')
            {
                i += 1;
            }
            while i < bytes.len()
                && (bytes[i].is_ascii_digit() || b"ivxlcdmIVXLCDM".contains(&bytes[i]))
            {
                i += 1;
            }
            return Some(i);
        }
    }
    // §+ \s* [0-9.]+
    if s.starts_with('§') {
        let rest = s.trim_start_matches('§');
        let ws = rest.len() - rest.trim_start().len();
        let rest2 = rest.trim_start();
        let num = rest2
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .count();
        if num > 0 {
            return Some(s.len() - rest.len() + ws + num);
        }
    }
    // \(? \d+(\.\d+)* [.)\]]?
    let (paren, body) = match s.strip_prefix('(') {
        Some(r) => (1, r),
        None => (0, s),
    };
    let digits = body.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 {
        let mut i = digits;
        let b = body.as_bytes();
        while i < b.len() && b[i] == b'.' {
            let d = body[i + 1..]
                .bytes()
                .take_while(|x| x.is_ascii_digit())
                .count();
            if d == 0 {
                break;
            }
            i += 1 + d;
        }
        if i < b.len() && matches!(b[i], b')' | b'.' | b']') {
            i += 1;
        }
        return Some(paren + i);
    }
    // \(? [A-Za-z]{1,2} (?:[.)\]]|(?=\s*[:\-–—])) — docling 2.129 also
    // strips `A: Title` / `A - Title`, leaving the separator to the trailing
    // trim.
    let letters = body.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
    if (1..=2).contains(&letters) {
        let b = body.as_bytes();
        if letters < b.len() && matches!(b[letters], b')' | b'.' | b']') {
            return Some(paren + letters + 1);
        }
        let after = body[letters..].trim_start();
        if after.starts_with([':', '-', '\u{2013}', '\u{2014}']) {
            return Some(paren + letters);
        }
    }
    None
}

/// `difflib.SequenceMatcher.ratio()` for two short strings: 2·M / T over the
/// Ratcliff–Obershelp matching blocks (no junk heuristic — titles are short).
fn similarity(a: &str, b: &str) -> f32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (j, &c) in b.iter().enumerate() {
        b2j.entry(c).or_default().push(j);
    }
    let mut matches = 0usize;
    let mut queue = vec![(0usize, a.len(), 0usize, b.len())];
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        // find_longest_match(alo, ahi, blo, bhi)
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for (i, ch) in a.iter().enumerate().take(ahi).skip(alo) {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(js) = b2j.get(ch) {
                for &j in js {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|p| j2len.get(&p))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }
        if bestsize == 0 {
            continue;
        }
        matches += bestsize;
        if besti > alo && bestj > blo {
            queue.push((alo, besti, blo, bestj));
        }
        if besti + bestsize < ahi && bestj + bestsize < bhi {
            queue.push((besti + bestsize, ahi, bestj + bestsize, bhi));
        }
    }
    (2.0 * matches as f32) / (a.len() + b.len()) as f32
}

/// Fuzzy similarity in 0..1 between a detected heading and a bookmark title.
/// Both are compared with and without their leading numbering marker, and
/// containment of one normalized title in the other boosts the score
/// (bookmarks are frequently truncated).
fn match_score(cand_text: &str, bm_title: &str) -> f32 {
    let mut variants_a = vec![norm(cand_text), norm(&strip_marker(cand_text))];
    let mut variants_b = vec![norm(bm_title), norm(&strip_marker(bm_title))];
    variants_a.retain(|v| !v.is_empty());
    variants_b.retain(|v| !v.is_empty());
    variants_a.dedup();
    variants_b.dedup();
    let mut best: f32 = 0.0;
    for a in &variants_a {
        for b in &variants_b {
            best = best.max(similarity(a, b));
            if a.chars().count() >= 4
                && b.chars().count() >= 4
                && (a.contains(b.as_str()) || b.contains(a.as_str()))
            {
                best = best.max(0.92);
            }
        }
    }
    best
}

// ------------------------------------------------------------------- stage

/// A heading (or promotable list item) found in the node stream.
struct HeadingRef {
    /// Index into the node vec.
    node_idx: usize,
    /// Plain text (markers reconstructed for ordered list items).
    text: String,
    /// 1-based page.
    page_no: usize,
    /// `[l, t, r, b]` in top-left page points, when the node carries one.
    bbox_points: Option<[f32; 4]>,
    /// Whether this is a list item (a bookmark match promotes it).
    is_list_item: bool,
}

/// Collect heading/list-item candidates with their page geometry.
fn collect(nodes: &[Node], with_list_items: bool) -> Vec<HeadingRef> {
    let mut out = Vec::new();
    let mut page_no = 0usize;
    let mut page_w = 0f32;
    let mut page_h = 0f32;
    let denorm = |loc: [u16; 4], w: f32, h: f32| -> Option<[f32; 4]> {
        (w > 0.0 && h > 0.0).then(|| {
            [
                loc[0] as f32 / 512.0 * w,
                loc[1] as f32 / 512.0 * h,
                loc[2] as f32 / 512.0 * w,
                loc[3] as f32 / 512.0 * h,
            ]
        })
    };
    for (idx, node) in nodes.iter().enumerate() {
        match node {
            Node::PageInfo {
                page_no: p,
                width,
                height,
            } => {
                page_no = *p;
                page_w = *width;
                page_h = *height;
            }
            Node::Located { location, inner } => {
                if let Node::Heading { text, .. } = inner.as_ref() {
                    out.push(HeadingRef {
                        node_idx: idx,
                        text: text.clone(),
                        page_no,
                        bbox_points: denorm(*location, page_w, page_h),
                        is_list_item: false,
                    });
                }
            }
            Node::Heading { text, .. } => out.push(HeadingRef {
                node_idx: idx,
                text: text.clone(),
                page_no,
                bbox_points: None,
                is_list_item: false,
            }),
            Node::ListItem {
                ordered,
                number,
                text,
                location,
                ..
            } if with_list_items => {
                // Reconstruct the enumeration marker the assembler folded into
                // `number`, so bookmark titles that carry it still match.
                let text = if *ordered {
                    format!("{number}. {text}")
                } else {
                    text.clone()
                };
                out.push(HeadingRef {
                    node_idx: idx,
                    text,
                    page_no,
                    bbox_points: location.and_then(|loc| denorm(loc, page_w, page_h)),
                    is_list_item: true,
                });
            }
            _ => {}
        }
    }
    out
}

/// The 1-based pages that contain headings — what the style glyph pass needs
/// to read. Empty when the stage would have nothing to do.
pub(crate) fn heading_pages(nodes: &[Node]) -> Vec<usize> {
    let mut pages: Vec<usize> = collect(nodes, false).iter().map(|h| h.page_no).collect();
    pages.sort_unstable();
    pages.dedup();
    pages.retain(|&p| p > 0);
    pages
}

/// Match the PDF outline to candidates; returns `candidate index → level`
/// (compressed, 1-based). Mirrors docling's `_infer_from_bookmarks`.
fn infer_from_bookmarks(
    candidates: &[HeadingRef],
    outline: &[OutlineItem],
    options: &HeadingHierarchyOptions,
) -> HashMap<usize, usize> {
    let mut claimed: Vec<bool> = vec![false; candidates.len()];
    let mut matches: Vec<(usize, usize)> = Vec::new(); // (candidate, raw level)

    for bm in outline {
        let title = bm.title.trim();
        if title.is_empty() {
            continue;
        }
        // A cross-page (page-less) match must be stronger.
        let threshold = if bm.page_no.is_none() {
            (options.bookmark_match_threshold + 0.1).min(1.0)
        } else {
            options.bookmark_match_threshold
        };
        let mut best: Option<(usize, f32, f32)> = None; // (idx, score, dist)
        for (idx, cand) in candidates.iter().enumerate() {
            if claimed[idx] {
                continue;
            }
            if let (Some(bp), cp) = (bm.page_no, cand.page_no) {
                if cp != 0 && cp != bp {
                    continue;
                }
            }
            let score = match_score(&cand.text, title);
            if score < threshold {
                continue;
            }
            let dist = match (cand.bbox_points.map(|b| b[1]), bm.y_top) {
                (Some(top), Some(y)) => (top - y).abs(),
                _ => f32::INFINITY,
            };
            let better = match best {
                None => true,
                Some((_, bs, bd)) => score > bs + 1e-6 || ((score - bs).abs() <= 1e-6 && dist < bd),
            };
            if better {
                best = Some((idx, score, dist));
            }
        }
        if let Some((idx, _, _)) = best {
            claimed[idx] = true;
            matches.push((idx, bm.level));
        }
    }
    if matches.is_empty() {
        return HashMap::new();
    }
    // Compress the raw bookmark depths actually used into contiguous levels.
    compress_keys(matches.into_iter().collect())
}

/// Run the stage: assign heading levels in place on the assembled node
/// stream. Precedence: bookmarks > numbering > style; headings with no
/// applicable signal keep their level; a confidently bookmark-matched list
/// item is promoted to a heading. `outline` and `glyph_styles` may be empty
/// (signals degrade individually, docling parity).
pub(crate) fn apply(
    nodes: &mut [Node],
    outline: &[OutlineItem],
    glyph_styles: &HashMap<usize, Vec<GlyphStyle>>,
    options: &HeadingHierarchyOptions,
) {
    if !options.enabled {
        return;
    }

    // Bookmark pass first: it may promote list items, changing the set of
    // headings, so it has to run before the heading list is (re)collected.
    let mut bookmark_levels: HashMap<usize, usize> = HashMap::new(); // node_idx → level
    if options.use_bookmarks && !outline.is_empty() {
        let candidates = collect(nodes, true);
        let matched = infer_from_bookmarks(&candidates, outline, options);
        for (cand_idx, level) in matched {
            let cand = &candidates[cand_idx];
            if cand.is_list_item {
                promote_list_item(nodes, cand.node_idx, &cand.text);
            }
            bookmark_levels.insert(cand.node_idx, level);
        }
    }

    let headings = collect(nodes, false);
    if headings.is_empty() {
        return;
    }

    let mut levels: HashMap<usize, usize> = HashMap::new(); // heading index → level
                                                            // Bookmarks are authoritative: seed first so nothing overrides them.
    for (i, h) in headings.iter().enumerate() {
        if let Some(level) = bookmark_levels.get(&h.node_idx) {
            levels.insert(i, *level);
        }
    }
    if options.use_numbering {
        let texts: Vec<&str> = headings.iter().map(|h| h.text.as_str()).collect();
        for (i, level) in infer_from_numbering(&texts, options) {
            levels.entry(i).or_insert(level);
        }
    }
    if options.use_style && !glyph_styles.is_empty() {
        for (i, level) in infer_from_style(&headings, glyph_styles, options) {
            levels.entry(i).or_insert(level);
        }
    }

    for (i, h) in headings.iter().enumerate() {
        let Some(&level) = levels.get(&i) else {
            continue;
        };
        let semantic = level.clamp(1, options.max_level.max(1) as usize);
        // Our `Node::Heading` level is the rendered Markdown depth: docling's
        // semantic level N serializes as N+1 hashes (`##` for level 1).
        let rendered = (semantic + 1).min(u8::MAX as usize) as u8;
        set_heading_level(&mut nodes[h.node_idx], rendered);
    }
}

fn set_heading_level(node: &mut Node, new_level: u8) {
    match node {
        Node::Heading { level, .. } => *level = new_level,
        Node::Located { inner, .. } => {
            if let Node::Heading { level, .. } = inner.as_mut() {
                *level = new_level;
            }
        }
        _ => {}
    }
}

/// Promote a bookmark-matched list item to a heading in place: same text and
/// position, now a `Heading` (the level is assigned by the caller). The
/// following sibling inherits `first_in_list` when the promoted item opened
/// its list, so adjacent lists keep their separation.
fn promote_list_item(nodes: &mut [Node], idx: usize, text: &str) {
    let Node::ListItem {
        first_in_list,
        location,
        ..
    } = &nodes[idx]
    else {
        return;
    };
    let was_first = *first_in_list;
    let loc = *location;
    let heading = Node::Heading {
        level: 2,
        text: text.to_string(),
    };
    nodes[idx] = match loc {
        Some(location) => Node::Located {
            location,
            inner: Box::new(heading),
        },
        None => heading,
    };
    if was_first {
        if let Some(Node::ListItem { first_in_list, .. }) = nodes.get_mut(idx + 1) {
            *first_in_list = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #377: a keyword probe whose byte length falls inside a multi-byte
    /// character of the heading must answer `false`, not panic — the
    /// reporter's `Note 1\u{a0}Overview` (no-break space at bytes 6..8 vs the
    /// 7-byte `chapter`), and every offset of a few non-ASCII characters.
    #[test]
    fn keyword_probe_never_slices_mid_char() {
        assert!(!starts_with_word("Note 1\u{a0}Overview", "chapter"));
        for word in ["chapter", "section", "part", "article", "appendix", "annex"] {
            for k in 0..12 {
                for ch in ['\u{a0}', '\u{e9}', '\u{3a9}', '\u{1f600}'] {
                    let text = format!("{}{ch}Overview", "x".repeat(k));
                    assert!(!starts_with_word(&text, word), "{text:?} vs {word}");
                }
            }
        }
        // The positive cases and the whole-word rule are unchanged.
        assert!(starts_with_word("Chapter\u{a0}1", "chapter"));
        assert!(starts_with_word("CHAPTER 2 Scope", "chapter"));
        assert!(starts_with_word("Section", "section"));
        assert!(!starts_with_word("Chapters", "chapter"));
        assert!(!starts_with_word("Chapt", "chapter"));
    }

    fn heading(loc: [u16; 4], text: &str) -> Node {
        Node::Located {
            location: loc,
            inner: Box::new(Node::Heading {
                level: 2,
                text: text.to_string(),
            }),
        }
    }

    fn page(no: usize) -> Node {
        Node::PageInfo {
            page_no: no,
            width: 512.0,
            height: 512.0,
        }
    }

    fn levels(nodes: &[Node]) -> Vec<u8> {
        nodes
            .iter()
            .filter_map(|n| match n {
                Node::Located { inner, .. } => match inner.as_ref() {
                    Node::Heading { level, .. } => Some(*level),
                    _ => None,
                },
                Node::Heading { level, .. } => Some(*level),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn roman_validator_matches_difflib_regex() {
        for ok in ["I", "iv", "XIV", "MCMXCIX", "iii", "C"] {
            assert!(is_roman(ok), "{ok}");
        }
        for bad in ["", "IIII", "VX", "ABC", "Summary", "IC"] {
            assert!(!is_roman(bad), "{bad}");
        }
    }

    #[test]
    fn markers_parse_the_docling_families() {
        let fam = |t: &str| parse_marker(t).map(|m| (m.family, m.depth));
        assert_eq!(fam("PART I — General"), Some(("part", 1)));
        assert_eq!(fam("Chapter 2: Scope"), Some(("chapter", 1)));
        assert_eq!(fam("Article 5"), Some(("article", 1)));
        assert_eq!(fam("§ 12 Something"), Some(("article", 1)));
        assert_eq!(fam("1. Introduction"), Some(("arabic", 1)));
        assert_eq!(fam("2) Also arabic"), Some(("arabic", 1)));
        assert_eq!(fam("1.1 Scope"), Some(("dotted", 2)));
        assert_eq!(fam("2.3.1 Deep"), Some(("dotted", 3)));
        assert_eq!(fam("A. Annex-ish"), Some(("alpha_u", 1)));
        assert_eq!(fam("(a) item"), Some(("alpha_l", 1)));
        assert_eq!(fam("(iv) sub"), Some(("roman_l", 1)));
        assert_eq!(fam("IV. Chapter"), Some(("roman_u", 1)));
        // Plain words are not numbering.
        assert_eq!(fam("Summary."), None);
        assert_eq!(fam("Overview"), None);
        // docling#4204 (2.129): colon, dash, bracket and parenthesized forms.
        assert_eq!(fam("1: Introduction"), Some(("arabic", 1)));
        assert_eq!(fam("(2) Scope"), Some(("arabic", 1)));
        assert_eq!(fam("3] Notes"), Some(("arabic", 1)));
        assert_eq!(fam("4 - Methods"), Some(("arabic", 1)));
        assert_eq!(fam("5 — Results"), Some(("arabic", 1)));
        assert_eq!(fam("1.2: Detail"), Some(("dotted", 2)));
        assert_eq!(fam("(1.2.3) Deep"), Some(("dotted", 3)));
        assert_eq!(fam("A: Annex"), Some(("alpha_u", 1)));
        assert_eq!(fam("B - Bee"), Some(("alpha_u", 1)));
        assert_eq!(fam("(e) see"), Some(("alpha_l", 1)));
        // A dash glued to the title is a hyphenated word, not a separator; a
        // bare number needs sequence evidence (see `resolve_bare_arabic`).
        assert_eq!(fam("1-A Something"), None);
        assert_eq!(fam("2 Methods"), None);
        assert_eq!(fam("1.5 kg"), Some(("dotted", 2)));
    }

    /// docling#5154's `test_keyword_part_and_article` additions: a keyword
    /// standing alone or before an enumerator (digit-holding token, letter,
    /// Roman numeral, number word) is a marker.
    #[test]
    fn keyword_part_and_article() {
        let fam = |t: &str| parse_marker(t).map(|m| m.family);
        assert_eq!(fam("Part I"), Some("part"));
        assert_eq!(fam("TITLE II General"), Some("part"));
        assert_eq!(fam("Article 1 - Scope"), Some("article"));
        assert_eq!(fam("Section 2"), Some("article"));
        assert_eq!(fam("§ 1.2 Liability"), Some("article"));
        assert_eq!(fam("Part One."), Some("part"));
        assert_eq!(fam("CHAPTER ONE"), Some("chapter"));
        assert_eq!(fam("ARTICLE FIRST"), Some("article"));
        assert_eq!(fam("Rule 12(b) Motions"), Some("article"));
        assert_eq!(fam("Appendix A1"), Some("article"));
        assert_eq!(fam("APPENDIX"), Some("article"));
        assert_eq!(fam("Annex: Glossary"), Some("article"));
        assert_eq!(fam("Appendix \u{2013} Data Tables"), Some("article"));
        assert_eq!(fam("Chapter 2D"), Some("chapter"));
        // `Chapter A-15` is a statute citation, not a chapter.
        assert_eq!(fam("Chapter A-15 Licensing"), None);
        // A multi-byte char right after the keyword neither panics nor matches.
        assert_eq!(fam("Part\u{e9}e"), None);
    }

    /// docling#5154's `test_keyword_followed_by_a_word_is_not_a_marker`: a
    /// keyword used as an ordinary word does not outrank the real numbering.
    #[test]
    fn keyword_followed_by_a_word_is_not_a_marker() {
        let fam = |t: &str| parse_marker(t).map(|m| m.family);
        for t in [
            "Rule Definition",
            "Part-Year Method",
            "Title Insurance",
            "Book Review",
            "Chapter Summary",
            "Section Overview",
            "Schedule of Fees",
        ] {
            assert_eq!(fam(t), None, "{t}");
        }
        let opts = HeadingHierarchyOptions::default();
        let texts = ["1. Overview", "Rule Definition", "2. Rules", "2.1 Syntax"];
        let map = infer_from_numbering(&texts.map(|t| t), &opts);
        let mut got: Vec<_> = map.iter().map(|(k, v)| (*k, *v)).collect();
        got.sort();
        assert_eq!(got, vec![(0, 1), (2, 1), (3, 2)]);
    }

    /// docling#4473's `Division` / `Subdivision`: markers only before an
    /// explicit enumerator, ranked between chapter and article.
    #[test]
    fn keyword_division_and_subdivision() {
        let fam = |t: &str| parse_marker(t).map(|m| m.family);
        assert_eq!(fam("DIVISION I"), Some("division"));
        assert_eq!(fam("Division 1 - General"), Some("division"));
        assert_eq!(fam("Division A"), Some("division"));
        assert_eq!(fam("SUBDIVISION A"), Some("subdivision"));
        assert_eq!(fam("Subdivision a: Scope"), Some("subdivision"));
        assert_eq!(fam("Division of Powers"), None);
        assert_eq!(fam("Division Civil Remedies"), None);
        assert_eq!(fam("Division d'appel"), None);
        assert_eq!(fam("Division"), None);
        let opts = HeadingHierarchyOptions::default();
        let texts = [
            "CHAPTER 1",
            "DIVISION I",
            "SUBDIVISION A",
            "ARTICLE 1",
            "1. Scope",
        ];
        let map = infer_from_numbering(&texts.map(|t| t), &opts);
        let levels: Vec<_> = (0..texts.len()).map(|i| map[&i]).collect();
        assert_eq!(levels, vec![1, 2, 3, 4, 5]);
        assert_eq!(strip_marker("Division 2 General"), "General");
    }

    /// docling#4179 (2.129): bare Arabic chapter numbers count once the
    /// document shows a consecutive run from 1 (dotted sections may
    /// intervene, explicit `2.` markers are evidence too); a gap ends the run.
    #[test]
    fn bare_arabic_chapter_numbers_need_a_sequence() {
        let opts = HeadingHierarchyOptions::default();
        let texts = ["1 Intro", "1.1 Scope", "2. Methods", "3 Results", "Plain"];
        let map = infer_from_numbering(&texts.map(|t| t), &opts);
        assert_eq!(map[&0], 1, "confirmed by the explicit `2.`");
        assert_eq!(map[&1], 2);
        assert_eq!(map[&2], 1);
        assert_eq!(map[&3], 1, "3 follows the run 1, 2");
        assert!(!map.contains_key(&4));
        let texts = ["1 Intro", "2 Methods", "4 Results"];
        let map = infer_from_numbering(&texts.map(|t| t), &opts);
        assert_eq!(map.get(&0), Some(&1));
        assert_eq!(map.get(&1), Some(&1));
        assert!(!map.contains_key(&2), "the gap leaves 4 unrecognized");
        let texts = ["2024 Report", "3 kg", "Summary"];
        assert!(infer_from_numbering(&texts.map(|t| t), &opts).is_empty());
    }

    #[test]
    fn ambiguous_single_letters_resolve_from_document_context() {
        // With unambiguous Roman evidence, a lone "V." reads as Roman.
        let texts = ["I. One", "II. Two", "V. Five"];
        let map = infer_from_numbering(&texts.map(|t| t), &HeadingHierarchyOptions::default());
        // All three land on the same (roman_u) level.
        assert_eq!(map[&0], map[&2]);
        // With alpha evidence instead, "C." reads as alpha.
        let texts = ["B. Bee", "C. Sea", "D. Dee"];
        let map = infer_from_numbering(&texts.map(|t| t), &HeadingHierarchyOptions::default());
        assert_eq!(map[&0], map[&1]);
        assert_eq!(map[&1], map[&2]);
    }

    #[test]
    fn numbering_levels_compress_to_contiguous() {
        // part > dotted-2 > dotted-3: distinct keys → levels 1, 2, 3 even
        // though the arabic family rank sits far from `part`.
        let texts = ["PART I", "1.1 Scope", "1.1.1 Detail", "No marker"];
        let map = infer_from_numbering(&texts.map(|t| t), &HeadingHierarchyOptions::default());
        assert_eq!(map[&0], 1);
        assert_eq!(map[&1], 2);
        assert_eq!(map[&2], 3);
        assert!(!map.contains_key(&3));
    }

    #[test]
    fn similarity_behaves_like_difflib_ratio() {
        assert_eq!(similarity("abc", "abc"), 1.0);
        assert_eq!(similarity("", ""), 1.0);
        assert_eq!(similarity("abc", "xyz"), 0.0);
        // difflib: SequenceMatcher(None, "abcd", "bcde").ratio() == 0.75
        assert!((similarity("abcd", "bcde") - 0.75).abs() < 1e-6);
    }

    #[test]
    fn bookmark_titles_match_with_and_without_markers() {
        assert!(match_score("1.1 Definitions", "Definitions") >= 0.9);
        assert!(match_score("ARTICLE 5 Payment Terms", "Payment Terms") >= 0.9);
        assert!(match_score("Introduction", "Conclusion") < 0.8);
    }

    #[test]
    fn apply_assigns_numbering_levels_end_to_end() {
        let mut nodes = vec![
            page(1),
            heading([10, 10, 200, 20], "1. Introduction"),
            heading([10, 40, 200, 50], "1.1 Scope"),
            heading([10, 70, 200, 80], "Unnumbered"),
        ];
        apply(
            &mut nodes,
            &[],
            &HashMap::new(),
            &HeadingHierarchyOptions::enabled(true),
        );
        // Semantic 1/2 render as Markdown levels 2/3; the unnumbered heading
        // keeps the assembler's level.
        assert_eq!(levels(&nodes), vec![2, 3, 2]);
    }

    /// #377 end to end: headings and bookmark titles with no-break spaces
    /// and non-ASCII letters go through the numbering *and* bookmark passes
    /// without a panic; the numbered ones still get their levels.
    #[test]
    fn apply_survives_multibyte_headings_and_bookmarks() {
        let mut nodes = vec![
            page(1),
            heading([10, 10, 200, 20], "Note 1\u{a0}Overview"),
            heading([10, 40, 200, 50], "1.\u{a0}Einf\u{fc}hrung"),
            heading(
                [10, 70, 200, 80],
                "1.1\u{a0}\u{dc}berblick \u{2014} Teil\u{a0}A",
            ),
            heading([10, 100, 200, 110], "Chapter\u{a0}2\u{a0}\u{3a9}mega"),
            heading([10, 130, 200, 140], "\u{1f600} Anhang"),
        ];
        let outline = vec![
            OutlineItem {
                title: "Note\u{a0}1 Overview".into(),
                level: 0,
                page_no: Some(1),
                y_top: None,
            },
            OutlineItem {
                title: "Einf\u{fc}hrung".into(),
                level: 1,
                page_no: Some(1),
                y_top: None,
            },
        ];
        apply(
            &mut nodes,
            &outline,
            &HashMap::new(),
            &HeadingHierarchyOptions::enabled(true),
        );
        assert_eq!(levels(&nodes).len(), 5);
    }

    #[test]
    fn apply_is_inert_when_disabled() {
        let mut nodes = vec![page(1), heading([10, 10, 200, 20], "1.1.1 Deep")];
        apply(
            &mut nodes,
            &[],
            &HashMap::new(),
            &HeadingHierarchyOptions::default(),
        );
        assert_eq!(levels(&nodes), vec![2]);
    }

    #[test]
    fn bookmarks_win_over_numbering_and_promote_list_items() {
        let outline = vec![
            OutlineItem {
                title: "1. Introduction".into(),
                level: 0,
                page_no: Some(1),
                y_top: None,
            },
            OutlineItem {
                title: "Hidden Heading".into(),
                level: 1,
                page_no: Some(1),
                y_top: None,
            },
        ];
        let mut nodes = vec![
            page(1),
            // Numbering alone would put this on level 1 too, but the bookmark
            // is authoritative and the depths compress from the outline.
            heading([10, 10, 200, 20], "1. Introduction"),
            Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: true,
                text: "Hidden Heading".into(),
                level: 0,
                marker: None,
                location: Some([10, 40, 200, 50]),
                dclx: None,
                href: None,
                layer: None,
            },
            Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: false,
                text: "a real item".into(),
                level: 0,
                marker: None,
                location: Some([10, 70, 200, 80]),
                dclx: None,
                href: None,
                layer: None,
            },
        ];
        apply(
            &mut nodes,
            &outline,
            &HashMap::new(),
            &HeadingHierarchyOptions::enabled(true),
        );
        // The matched list item became a level-2 (semantic 2 → rendered 3)
        // heading; the trailing sibling re-opens its list.
        assert_eq!(levels(&nodes), vec![2, 3]);
        match &nodes[3] {
            Node::ListItem {
                first_in_list,
                text,
                ..
            } => {
                assert!(*first_in_list, "sibling re-opens the list");
                assert_eq!(text, "a real item");
            }
            other => panic!("expected the sibling list item, got {other:?}"),
        }
    }

    #[test]
    fn style_ranks_by_size_then_prominence() {
        // Two size clusters (18pt vs 12pt); within 12pt, bold beats regular.
        let glyphs = vec![
            GlyphStyle {
                l: 10.0,
                t: 10.0,
                r: 100.0,
                b: 28.0,
                height: 18.0,
                weight_cls: 2,
                italic: false,
                styled: true,
            },
            GlyphStyle {
                l: 10.0,
                t: 60.0,
                r: 100.0,
                b: 72.0,
                height: 12.0,
                weight_cls: 2,
                italic: false,
                styled: true,
            },
            GlyphStyle {
                l: 10.0,
                t: 110.0,
                r: 100.0,
                b: 122.0,
                height: 12.0,
                weight_cls: 0,
                italic: false,
                styled: true,
            },
        ];
        let mut styles = HashMap::new();
        styles.insert(1usize, glyphs);
        let mut nodes = vec![
            page(1),
            heading([10, 10, 200, 28], "Big Title Words"),
            heading([10, 60, 200, 72], "Bold Twelve"),
            heading([10, 110, 200, 122], "Plain Twelve"),
        ];
        // Page is 512x512 points in these tests, so locations ≈ points.
        apply(
            &mut nodes,
            &[],
            &styles,
            &HeadingHierarchyOptions::enabled(true),
        );
        assert_eq!(levels(&nodes), vec![2, 3, 4]);
    }

    #[test]
    fn strip_marker_removes_leading_numbering() {
        assert_eq!(strip_marker("1.1 Definitions"), "Definitions");
        assert_eq!(strip_marker("ARTICLE 5 - Payment"), "Payment");
        assert_eq!(strip_marker("(a) item"), "item");
        assert_eq!(strip_marker("No marker here"), "No marker here");
    }
}
