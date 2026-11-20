//! Keyword markers of the heading numbering signal (`PART I`, `Chapter 2`,
//! `Art. 5`) — a port of docling's `KeywordLexicon`
//! (`models/stages/heading_hierarchy/numbering_lexicon.py`, docling#5474,
//! #1145).
//!
//! The words are data, not code: the built-in English words merge with
//! `HeadingHierarchyOptions::numbering_keywords`, so a document in another
//! language reads with the same rules as an English one. Matching is on
//! NFC-normalized text, case-insensitive per character (`Capítulo II` is
//! `capítulo`), longest word first; a keyword ends at whitespace, the end or
//! `.:–—-`, and one that ends with a dot of its own (`art.`) may be followed
//! by its enumerator directly (`Art.5`). A multi-word keyword matches with
//! any whitespace run between its parts.

use std::collections::{BTreeMap, HashMap, HashSet};

use unicode_normalization::UnicodeNormalization;

/// docling's `_DEFAULT_KEYWORDS`.
const DEFAULT_KEYWORDS: [(&str, &[&str]); 5] = [
    ("part", &["part", "title", "book"]),
    ("chapter", &["chapter"]),
    ("division", &["division"]),
    ("subdivision", &["subdivision"]),
    (
        "article",
        &[
            "article", "section", "clause", "schedule", "annex", "appendix", "rule",
        ],
    ),
];

/// docling's `_DEFAULT_NUMBER_WORDS`: `CHAPTER ONE`, `ARTICLE FIRST`.
const DEFAULT_NUMBER_WORDS: &str = "one two three four five six seven eight nine ten eleven \
    twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty thirty forty \
    fifty sixty seventy eighty ninety first second third fourth fifth sixth seventh eighth \
    ninth tenth eleventh twelfth thirteenth fourteenth fifteenth sixteenth seventeenth \
    eighteenth nineteenth twentieth";

/// docling's `_ENUMERATOR_ONLY`: `Division` / `Subdivision` count only when
/// an explicit enumerator follows (`Oddział Kardiologii` is no marker).
const ENUMERATOR_ONLY: [&str; 2] = ["division", "subdivision"];

/// The built-in keyword families, in rank order — docling's
/// `NUMBERING_KEYWORD_FAMILIES`.
pub(crate) const KEYWORD_FAMILIES: [&str; 5] =
    ["part", "chapter", "division", "subdivision", "article"];

/// The numeral schemes — docling's `NUMBERING_NUMERAL_SCHEMES`; no keyword
/// family may take one of these names.
pub(crate) const NUMERAL_SCHEMES: [&str; 6] = [
    "roman_u", "arabic", "alpha_u", "alpha_l", "roman_l", "dotted",
];

/// One keyword: its parts (whitespace-split, NFC) and its family.
struct Entry {
    parts: Vec<String>,
    /// Ends with a dot of its own: no boundary is needed after it.
    dotted: bool,
    family: String,
    /// Character length of the spelling, parts joined by one space — the
    /// sort key (longest first).
    len: usize,
}

/// The keyword markers and number words of one configuration, built once.
pub(crate) struct KeywordLexicon {
    entries: Vec<Entry>,
    number_words: HashSet<String>,
}

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// Python's `str.casefold()` closely enough for keyword identity: NFC,
/// then lowercase.
fn fold(s: &str) -> String {
    nfc(s).to_lowercase()
}

/// Whether two characters match under Python's `re.IGNORECASE`.
fn same_char(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// docling's `is_roman_numeral` (1..3999, case-insensitive).
pub(crate) fn is_roman(token: &str) -> bool {
    crate::heading_hierarchy::is_roman_token(token)
}

impl Default for KeywordLexicon {
    fn default() -> Self {
        KeywordLexicon::new(&BTreeMap::new(), &[])
    }
}

impl KeywordLexicon {
    /// The built-in words merged with `keywords` (family → words) and the
    /// extra `number_words`. A word in `keywords` replaces the built-in
    /// family of that word (`{"section": ["section"]}` moves Section out of
    /// `article`).
    pub(crate) fn new(keywords: &BTreeMap<String, Vec<String>>, number_words: &[String]) -> Self {
        let mut by_word: HashMap<String, (Vec<String>, String)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let defaults = DEFAULT_KEYWORDS.iter().flat_map(|(family, words)| {
            words
                .iter()
                .map(move |w| (family.to_string(), w.to_string()))
        });
        let added = keywords
            .iter()
            .flat_map(|(family, words)| words.iter().map(move |w| (family.clone(), w.clone())));
        for (family, word) in defaults.chain(added) {
            let word = nfc(&word);
            let parts: Vec<String> = word.split_whitespace().map(str::to_string).collect();
            if parts.is_empty() {
                continue;
            }
            let key = fold(&parts.join(" "));
            if !by_word.contains_key(&key) {
                order.push(key.clone());
            }
            by_word.insert(key, (parts, family));
        }
        let mut entries: Vec<Entry> = order
            .into_iter()
            .map(|key| {
                let (parts, family) = by_word.remove(&key).expect("collected");
                let spelling = parts.join(" ");
                Entry {
                    dotted: spelling.ends_with('.'),
                    len: spelling.chars().count(),
                    parts,
                    family,
                }
            })
            .collect();
        // Longest first, so `article` is tried before `art` (stable: equal
        // lengths keep their order).
        entries.sort_by_key(|e| std::cmp::Reverse(e.len));
        let mut words: HashSet<String> = DEFAULT_NUMBER_WORDS
            .split_whitespace()
            .map(str::to_string)
            .collect();
        words.extend(number_words.iter().map(|w| nfc(w).to_lowercase()));
        KeywordLexicon {
            entries,
            number_words: words,
        }
    }

    /// The byte end of `entry` at the start of `s`, or `None`.
    fn match_entry(entry: &Entry, s: &str) -> Option<usize> {
        let mut pos = 0usize;
        for (i, part) in entry.parts.iter().enumerate() {
            if i > 0 {
                let ws = s[pos..]
                    .char_indices()
                    .find(|(_, c)| !c.is_whitespace())
                    .map_or(s.len() - pos, |(k, _)| k);
                if ws == 0 {
                    return None;
                }
                pos += ws;
            }
            let mut text = s[pos..].char_indices();
            for want in part.chars() {
                let (_, got) = text.next()?;
                if !same_char(want, got) {
                    return None;
                }
            }
            pos = text.next().map_or(s.len(), |(k, _)| pos + k);
        }
        Some(pos)
    }

    /// docling's `_keyword` regex: the first entry (longest first) at the
    /// start of `s` that ends at a boundary — whitespace, the end or
    /// `.:–—-` — unless it ends with its own dot. Its index and byte end.
    fn keyword_at(&self, s: &str) -> Option<(usize, usize)> {
        self.entries.iter().enumerate().find_map(|(i, e)| {
            let end = Self::match_entry(e, s)?;
            let boundary = e.dotted
                || s[end..].chars().next().is_none_or(|c| {
                    c.is_whitespace() || matches!(c, '.' | ':' | '\u{2013}' | '\u{2014}' | '-')
                });
            boundary.then_some((i, end))
        })
    }

    /// The scheme family of the keyword marker that starts `text`, or
    /// `None` — docling's `KeywordLexicon.family`.
    pub(crate) fn family(&self, text: &str) -> Option<&'static str> {
        self.family_str(text).map(intern)
    }

    fn family_str(&self, text: &str) -> Option<&str> {
        let text = nfc(text);
        let (i, end) = self.keyword_at(&text)?;
        let family = self.entries[i].family.as_str();
        let rest = &text[end..];
        // `_GAP`: whitespace, or nothing after a keyword's own dot.
        let gapped = rest.trim_start();
        let has_gap = gapped.len() < rest.len() || text[..end].ends_with('.');
        if ENUMERATOR_ONLY.contains(&family) {
            if !has_gap {
                return None;
            }
            return self.enumerator(gapped).then_some(family);
        }
        if family == "chapter" && citation(rest) {
            return None;
        }
        // `_TAIL`: a token after the gap, else the keyword standing alone.
        if has_gap {
            let tok_len = gapped.find(token_end).unwrap_or(gapped.len());
            if tok_len > 0 {
                return self.word_enumerator(&gapped[..tok_len]).then_some(family);
            }
        }
        let after = rest.trim_start();
        let mut chars = after.chars();
        match chars.next() {
            None | Some('.' | ':' | '\u{2013}' | '\u{2014}') => Some(family),
            Some('-') if chars.next().is_some_and(char::is_whitespace) => Some(family),
            _ => None,
        }
    }

    /// `_ENUMERATOR` + `_is_enumerator` for the enumerator-only families:
    /// an Arabic index, one ASCII letter or a Roman numeral, ended by
    /// whitespace, the end or `.,:;)]–—-`.
    fn enumerator(&self, s: &str) -> bool {
        let tok = &s[..s.find(token_end).unwrap_or(s.len())];
        !tok.is_empty()
            && (tok.chars().all(|c| c.is_numeric())
                || (tok.chars().count() == 1 && tok.chars().all(|c| c.is_ascii_alphabetic()))
                || (tok.chars().all(|c| "ivxlcdmIVXLCDM".contains(c)) && is_roman(tok)))
    }

    /// `_is_enumerator(token, with_words=True)`: a token holding a digit
    /// (`2D`, `12(b)`), a number word, one character or a Roman numeral.
    fn word_enumerator(&self, tok: &str) -> bool {
        tok.chars().any(char::is_numeric)
            || self.number_words.contains(&tok.to_lowercase())
            || tok.chars().count() == 1
            || is_roman(tok)
    }

    /// The byte length of a leading keyword marker in `s` for bookmark
    /// matching (`_leading_marker`'s keyword branch): the keyword — a whole
    /// word when it ends with a letter or digit — then `[\s.:]*` and
    /// `[0-9ivxlcdm]*`.
    pub(crate) fn leading_keyword_len(&self, s: &str) -> Option<usize> {
        self.entries.iter().find_map(|e| {
            let end = Self::match_entry(e, s)?;
            let last = e.parts.last()?.chars().last()?;
            if last.is_alphanumeric()
                && s[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                return None;
            }
            let mut i = end;
            let b = s.as_bytes();
            while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'.' || b[i] == b':') {
                i += 1;
            }
            while i < b.len() && (b[i].is_ascii_digit() || b"ivxlcdmIVXLCDM".contains(&b[i])) {
                i += 1;
            }
            Some(i)
        })
    }
}

/// A family name as `&'static str`: the built-in names as they are, an
/// added one leaked once and reused (the names a process sees are few).
fn intern(name: &str) -> &'static str {
    if let Some(f) = KEYWORD_FAMILIES.iter().find(|f| **f == name) {
        return f;
    }
    static NAMES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = names.iter().find(|f| **f == name) {
        return f;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    names.push(leaked);
    leaked
}

/// docling's `_CITATION` (`\s+[a-z]-\d`): `Chapter A-15` is a statute
/// citation, not a chapter.
fn citation(rest: &str) -> bool {
    let after = rest.trim_start();
    let mut c = after.chars();
    after.len() < rest.len()
        && c.next().is_some_and(|c| c.is_ascii_alphabetic())
        && c.next() == Some('-')
        && c.next().is_some_and(|c| c.is_ascii_digit())
}

/// The characters that end a keyword's enumerator token: whitespace and
/// `.,:;)]–—-`.
fn token_end(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '.' | ',' | ':' | ';' | ')' | ']' | '\u{2013}' | '\u{2014}' | '-'
        )
}

/// docling's `HeadingHierarchyOptions` validation of the two keyword
/// options: no empty family or word, no numeral scheme as a family, no word
/// in two families (after NFC + case folding), and every new family listed
/// in `numbering_schemes` (which gives it its rank).
pub(crate) fn validate(
    keywords: &BTreeMap<String, Vec<String>>,
    number_words: &[String],
    numbering_schemes: Option<&[String]>,
) -> Result<(), String> {
    let mut seen: HashMap<String, &str> = HashMap::new();
    for (family, words) in keywords {
        if family.trim().is_empty() {
            return Err("numbering_keywords: a family name is empty".into());
        }
        if NUMERAL_SCHEMES.contains(&family.as_str()) {
            return Err(format!(
                "numbering_keywords: `{family}` is a numeral scheme, not a keyword family"
            ));
        }
        if words.is_empty() {
            return Err(format!(
                "numbering_keywords: family `{family}` has no words"
            ));
        }
        for word in words {
            if word.trim().is_empty() {
                return Err(format!(
                    "numbering_keywords: family `{family}` has an empty word"
                ));
            }
            let key = fold(&word.split_whitespace().collect::<Vec<_>>().join(" "));
            if let Some(other) = seen.insert(key, family) {
                if other != family {
                    return Err(format!(
                        "numbering_keywords: `{word}` is in the families `{other}` and `{family}`"
                    ));
                }
            }
        }
        if !KEYWORD_FAMILIES.contains(&family.as_str())
            && !numbering_schemes.is_some_and(|s| s.iter().any(|f| f == family))
        {
            return Err(format!(
                "numbering_keywords: the new family `{family}` must be listed in numbering_schemes"
            ));
        }
    }
    if number_words.iter().any(|w| w.trim().is_empty()) {
        return Err("numbering_number_words: an empty word".into());
    }
    Ok(())
}
