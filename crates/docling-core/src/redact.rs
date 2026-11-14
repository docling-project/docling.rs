//! Opt-in PII redaction of a [`DoclingDocument`] (#621).
//!
//! A converted document feeds search and RAG pipelines — Markdown, chunks,
//! embeddings, vector stores, LLM prompts — and personal data in the source
//! flows into every one of them. Redacting each export format after the
//! fact misses the text that lives elsewhere in the model: the item tree
//! behind the JSON export, the link table, a code block's `orig`, the
//! key-value graph's cells, a caption's hyperlink, the pixels of an embedded
//! image. This pass runs **once, on the model, before any serializer,
//! chunker or stream reads it**, replaces every detected span in place with
//! a placeholder and returns a [`RedactionReport`] of counts — never the
//! original values (unless the caller asks for the mapping explicitly).
//!
//! Detection is pluggable through [`PiiDetector`]. The built-in
//! [`PatternDetector`] is pure Rust and compiles for wasm: e-mail, phone,
//! Luhn-validated card numbers, mod-97-validated IBANs, IPv4/IPv6, URL
//! credentials and a few national IDs with their own check digits (US SSN,
//! UK NINO, Indian Aadhaar — Verhoeff), plus the caller's own patterns and
//! deny terms. A name/organization/location detector (NER) lives in the
//! `docling` crate behind its `ner` feature and plugs into the same trait;
//! several detectors compose through [`CompositeDetector`], overlapping
//! spans resolved longest-first.
//!
//! Python docling has no redaction stage, so this is a docling.rs extension
//! (recorded in `docs/MIGRATION.md`); it is off by default and, unused,
//! changes nothing.
//!
//! What is walked: every user-visible string of the flat nodes, recursively
//! (groups, furniture wrappers, picture children, rich table cells), the
//! item tree (`tree`), the link table, code text and `orig`, a formula's
//! `orig` (the LaTeX is left alone — it is a rendering, not text, and the
//! pipeline's own `orig` is what a reader could recover PII from), tables
//! (grid, first-class cells, rich-cell blocks), field regions, key-value
//! cells, comments, hrefs, VTT cue voices. An inline group is matched on
//! the concatenation of its runs and the spans mapped back onto the runs.
//! The document's `name` (the file name) is not touched. Image handling is
//! [`ImageRedaction`]: `Drop` removes every embedded image and page render
//! here; `BoxOut` needs the OCR models and is the `docling` crate's job
//! (this pass then leaves the pixels alone, as `Keep` does).
//!
//! Non-goals: no compliance guarantee (GDPR/HIPAA/PCI), no rewriting of the
//! source file, no cross-node entity detection, no pseudonym persistence
//! across documents.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use regex::Regex;

use crate::document::{InlineRun, Node, Table};
use crate::tree::{ItemTree, TreeKind};
use crate::DoclingDocument;

/// The kinds of personal data a detector can report. The order is the
/// precedence when two detectors' spans overlap at equal length — the
/// check-digit formats (card, IBAN, national ID) over a phone number, whose
/// digit groups they also look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PiiKind {
    Email,
    CreditCard,
    Iban,
    NationalId,
    IpAddress,
    UrlCredentials,
    Phone,
    Person,
    Organization,
    Location,
    Address,
    /// A caller-supplied pattern ([`CustomPattern`]) or deny term.
    Custom,
}

impl PiiKind {
    /// Every kind, in precedence order.
    pub const ALL: [PiiKind; 12] = [
        PiiKind::Email,
        PiiKind::CreditCard,
        PiiKind::Iban,
        PiiKind::NationalId,
        PiiKind::IpAddress,
        PiiKind::UrlCredentials,
        PiiKind::Phone,
        PiiKind::Person,
        PiiKind::Organization,
        PiiKind::Location,
        PiiKind::Address,
        PiiKind::Custom,
    ];

    /// The kinds the pattern detector handles itself (no model needed).
    pub const PATTERN: [PiiKind; 7] = [
        PiiKind::Email,
        PiiKind::CreditCard,
        PiiKind::Iban,
        PiiKind::NationalId,
        PiiKind::IpAddress,
        PiiKind::UrlCredentials,
        PiiKind::Phone,
    ];

    /// The kinds only a named-entity model finds.
    pub const NER: [PiiKind; 4] = [
        PiiKind::Person,
        PiiKind::Organization,
        PiiKind::Location,
        PiiKind::Address,
    ];

    /// The placeholder label: `[EMAIL]`, `[PERSON_2]`.
    pub fn label(self) -> &'static str {
        match self {
            PiiKind::Email => "EMAIL",
            PiiKind::Phone => "PHONE",
            PiiKind::CreditCard => "CREDIT_CARD",
            PiiKind::Iban => "IBAN",
            PiiKind::IpAddress => "IP",
            PiiKind::UrlCredentials => "CREDENTIALS",
            PiiKind::NationalId => "NATIONAL_ID",
            PiiKind::Person => "PERSON",
            PiiKind::Organization => "ORG",
            PiiKind::Location => "LOCATION",
            PiiKind::Address => "ADDRESS",
            PiiKind::Custom => "REDACTED",
        }
    }

    /// The wire / CLI spelling (`credit_card`, `ip_address`, …).
    pub fn name(self) -> &'static str {
        match self {
            PiiKind::Email => "email",
            PiiKind::Phone => "phone",
            PiiKind::CreditCard => "credit_card",
            PiiKind::Iban => "iban",
            PiiKind::IpAddress => "ip_address",
            PiiKind::UrlCredentials => "url_credentials",
            PiiKind::NationalId => "national_id",
            PiiKind::Person => "person",
            PiiKind::Organization => "organization",
            PiiKind::Location => "location",
            PiiKind::Address => "address",
            PiiKind::Custom => "custom",
        }
    }

    /// Parse a wire spelling (case-insensitive; `-` as `_`; `ip` and `org`
    /// accepted as shorthands).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace('-', "_");
        match s.as_str() {
            "ip" => return Some(PiiKind::IpAddress),
            "org" => return Some(PiiKind::Organization),
            "credentials" | "url_credential" => return Some(PiiKind::UrlCredentials),
            _ => {}
        }
        PiiKind::ALL.into_iter().find(|k| k.name() == s)
    }

    /// Every spelling [`parse`](Self::parse) accepts, for error messages.
    pub const ACCEPTED: &'static str = "email, phone, credit_card, iban, ip_address, \
        url_credentials, national_id, person, organization, location, address, custom";
}

/// What a detected span becomes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Replacement {
    /// `[EMAIL]`, `[PERSON]` — the kind's label (the default).
    #[default]
    Label,
    /// `[EMAIL_1]`, `[EMAIL_2]`: a number per distinct value within the
    /// document, so the same e-mail reads as the same placeholder wherever it
    /// recurs and two different ones stay apart — what a downstream reader
    /// (or an LLM) needs to follow "who did what" with the names gone.
    Pseudonym,
    /// One fixed string for everything, e.g. `█████` or `***`.
    Fixed(String),
}

impl Replacement {
    /// Parse the wire spelling: `label` | `pseudonym` | `fixed:<text>`.
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("label") {
            return Some(Replacement::Label);
        }
        if t.eq_ignore_ascii_case("pseudonym") {
            return Some(Replacement::Pseudonym);
        }
        let lower = t.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("fixed:") {
            // Keep the caller's own casing of the text.
            let text = &t[t.len() - rest.len()..];
            return Some(Replacement::Fixed(text.to_string()));
        }
        None
    }

    pub const ACCEPTED: &'static str = "label, pseudonym, fixed:<text>";
}

/// What happens to embedded images (pictures, page renders) when the pass
/// runs. An image can carry anything the text does — a scanned letter, a
/// screenshot of a profile — and the pass cannot read it without OCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImageRedaction {
    /// Remove every embedded image and page render (the default): nothing
    /// unread leaves the document.
    #[default]
    Drop,
    /// OCR each image and paint solid boxes over the lines that carry a
    /// detected span; keep the rest of the pixels. Needs the OCR models (the
    /// `docling` crate does this; without the models it degrades to `Drop`
    /// with a warning).
    BoxOut,
    /// Leave the images as they are.
    Keep,
}

impl ImageRedaction {
    /// Parse the wire spelling: `drop` | `box_out` | `keep`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "drop" => Some(ImageRedaction::Drop),
            "box_out" | "boxout" | "box" => Some(ImageRedaction::BoxOut),
            "keep" => Some(ImageRedaction::Keep),
            _ => None,
        }
    }

    pub const ACCEPTED: &'static str = "drop, box_out, keep";
}

/// A caller-supplied pattern: `name` is the placeholder label (`[CASE_ID]`
/// for `case_id`), `regex` the expression (the `regex` crate's syntax —
/// no look-around); the whole match is the span, or capture group 1 when
/// the expression has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomPattern {
    pub name: String,
    pub regex: String,
}

impl CustomPattern {
    /// Parse the wire spelling `NAME=REGEX` (a bare `REGEX` gets the label
    /// `CUSTOM`).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (name, regex) = match s.split_once('=') {
            // `NAME=…` only when the name looks like an identifier — a regex
            // itself may start with `(?i)x=`.
            Some((n, r))
                if !n.is_empty()
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') =>
            {
                (n.to_string(), r.to_string())
            }
            _ => ("custom".to_string(), s.to_string()),
        };
        Some(CustomPattern { name, regex })
    }
}

/// The pass's settings. `Default` = every kind, labels, no custom patterns,
/// images dropped, no mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct RedactionOptions {
    /// Which kinds to redact; empty = every kind the available detectors
    /// find (the pattern kinds, and the NER kinds when a model runs).
    pub kinds: Vec<PiiKind>,
    pub replacement: Replacement,
    /// Extra patterns, redacted as [`PiiKind::Custom`] under their own label.
    pub custom_patterns: Vec<CustomPattern>,
    /// Literal terms (case-insensitive, whole words) always redacted, as
    /// `[REDACTED]` — a project code name, a client's name the NER misses.
    pub deny_terms: Vec<String>,
    /// Literal terms (case-insensitive) never redacted even when a detector
    /// flags them — `support@example.com`, the company's own name.
    pub allow_terms: Vec<String>,
    /// Regexes a flagged span must not match in full to be redacted (#694)
    /// — the escape hatch for a document's own number shapes: part numbers
    /// (`[A-Z]{2}\d{6}`), record IDs, a ticket format. Applied to every
    /// detector's spans, the NER's included.
    pub allow_patterns: Vec<String>,
    /// The NER detector's minimum confidence for a span (0–1; 0.85 by
    /// default — a lone common word in a table header reads as a location
    /// at ~0.8, a real name at 0.99+).
    pub ner_min_score: f32,
    pub images: ImageRedaction,
    /// Keep the `original → placeholder` pairs in the report. Off by default
    /// — the report then carries counts only and can be logged or returned
    /// over HTTP without leaking what was removed.
    pub return_mapping: bool,
}

impl Default for RedactionOptions {
    fn default() -> Self {
        Self {
            kinds: Vec::new(),
            replacement: Replacement::Label,
            custom_patterns: Vec::new(),
            deny_terms: Vec::new(),
            allow_terms: Vec::new(),
            allow_patterns: Vec::new(),
            ner_min_score: 0.85,
            images: ImageRedaction::Drop,
            return_mapping: false,
        }
    }
}

impl RedactionOptions {
    /// Whether `kind` is in scope (an empty `kinds` means all).
    pub fn wants(&self, kind: PiiKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }

    /// Whether any NER-only kind is in scope.
    pub fn wants_ner(&self) -> bool {
        PiiKind::NER.iter().any(|&k| self.wants(k))
    }
}

/// One detected span: byte offsets into the text it was detected on, the
/// kind, the detector's confidence (1.0 for a pattern) and, for a custom
/// pattern, its name (the label).
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: PiiKind,
    pub score: f32,
    pub name: Option<String>,
}

impl Span {
    pub fn new(start: usize, end: usize, kind: PiiKind) -> Self {
        Span {
            start,
            end,
            kind,
            score: 1.0,
            name: None,
        }
    }
}

/// A span detector. `detect` returns the spans of one text; the pass calls
/// it once per string of the document, so a detector keeps no per-document
/// state (pseudonym numbering is the pass's own).
pub trait PiiDetector {
    fn detect(&self, text: &str) -> Vec<Span>;
}

/// Several detectors as one; their spans are merged by the pass's overlap
/// rule (longest first, then kind precedence).
pub struct CompositeDetector(pub Vec<Box<dyn PiiDetector + Send + Sync>>);

impl PiiDetector for CompositeDetector {
    fn detect(&self, text: &str) -> Vec<Span> {
        self.0.iter().flat_map(|d| d.detect(text)).collect()
    }
}

/// What was redacted: one count per label (`EMAIL`, `PHONE`, a custom
/// pattern's name, `REDACTED` for deny terms), the total, and — only with
/// [`RedactionOptions::return_mapping`] — the `original → placeholder`
/// pairs. A backend that also builds docling's item tree (DOCX, HTML) has
/// its text redacted in both representations but counted once.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RedactionReport {
    pub counts: BTreeMap<String, usize>,
    pub total: usize,
    pub mapping: Option<Vec<(String, String)>>,
}

impl RedactionReport {
    /// The counts as a JSON object (`{"EMAIL": 2, "PHONE": 1}`), what the
    /// HTTP surfaces answer with — never the mapping.
    pub fn counts_json(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.counts
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                .collect(),
        )
    }
}

/// A rejected option (an invalid custom regex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionError(pub String);

impl fmt::Display for RedactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RedactionError {}

// ---------------------------------------------------------------------------
// The pattern detector
// ---------------------------------------------------------------------------

macro_rules! re {
    ($pat:expr) => {{
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new($pat).expect("static regex"))
    }};
}

/// What is never redacted: the allow terms (case-insensitive, the whole
/// span) and the allow patterns (a regex the whole span matches). Built
/// once per pass from the options and asked about every span, whichever
/// detector produced it.
#[derive(Debug, Default)]
pub struct AllowList {
    terms: Vec<String>,
    patterns: Vec<Regex>,
}

impl AllowList {
    /// Compile the options' allow terms and patterns; an invalid pattern is
    /// the error.
    pub fn new(opts: &RedactionOptions) -> Result<Self, RedactionError> {
        let terms = opts
            .allow_terms
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let mut patterns = Vec::new();
        for p in opts
            .allow_patterns
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
        {
            let re = Regex::new(&format!("^(?:{p})$"))
                .map_err(|e| RedactionError(format!("redact allow pattern {p:?}: {e}")))?;
            patterns.push(re);
        }
        Ok(Self { terms, patterns })
    }

    /// Whether a span's text is allowed through.
    pub fn allows(&self, text: &str) -> bool {
        (!self.terms.is_empty() && self.terms.iter().any(|a| a == &text.to_lowercase()))
            || self.patterns.iter().any(|re| re.is_match(text))
    }
}

/// The built-in detector: patterns with check-digit validation where the
/// format has one, so a random 16-digit order number is not a card and
/// `2024-01-15` is not a phone.
pub struct PatternDetector {
    kinds: Vec<PiiKind>,
    custom: Vec<(String, Regex)>,
    deny: Vec<Regex>,
    allow: AllowList,
}

impl PatternDetector {
    /// Build from the options: the pattern kinds in scope, the custom
    /// patterns compiled (an invalid one is the error), the deny terms as
    /// whole-word case-insensitive matches.
    pub fn new(opts: &RedactionOptions) -> Result<Self, RedactionError> {
        let kinds = PiiKind::PATTERN
            .into_iter()
            .filter(|&k| opts.wants(k))
            .collect();
        let mut custom = Vec::new();
        for p in &opts.custom_patterns {
            let re = Regex::new(&p.regex)
                .map_err(|e| RedactionError(format!("redact pattern {:?}: {e}", p.name)))?;
            custom.push((p.name.trim().to_ascii_uppercase().replace('-', "_"), re));
        }
        let deny = opts
            .deny_terms
            .iter()
            .filter(|t| !t.trim().is_empty())
            .map(|t| {
                Regex::new(&format!(r"(?i)\b{}\b", regex::escape(t.trim()))).expect("escaped term")
            })
            .collect();
        let allow = AllowList::new(opts)?;
        Ok(Self {
            kinds,
            custom,
            deny,
            allow,
        })
    }

    fn wants(&self, kind: PiiKind) -> bool {
        self.kinds.contains(&kind)
    }

    /// Whether the span's text is an allowed term or matches an allow
    /// pattern.
    pub fn allowed(&self, text: &str) -> bool {
        self.allow.allows(text)
    }
}

impl PiiDetector for PatternDetector {
    fn detect(&self, text: &str) -> Vec<Span> {
        let mut out = Vec::new();
        if text.is_empty() {
            return out;
        }
        // `scheme://user:password@host` — the `user:password` part. Found
        // first: `password@host.tld` also looks like an e-mail address, and
        // the credentials span is the one to keep.
        let mut credentials: Vec<(usize, usize)> = Vec::new();
        if self.wants(PiiKind::UrlCredentials) {
            for c in re!(r"(?i)[a-z][a-z0-9+.\-]*://([^\s/@:]+:[^\s/@]+)@").captures_iter(text) {
                let m = c.get(1).expect("group 1");
                credentials.push((m.start(), m.end()));
                out.push(Span::new(m.start(), m.end(), PiiKind::UrlCredentials));
            }
        }
        if self.wants(PiiKind::Email) {
            for m in re!(r"(?i)[a-z0-9._%+\-\\]+@[a-z0-9](?:[a-z0-9\-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9\-]*[a-z0-9])?)*\.[a-z]{2,}").find_iter(text) {
                // A trailing `.` belongs to the sentence; a leading `\` to
                // a Markdown escape of the character after it.
                let (mut s, mut e) = (m.start(), m.end());
                while e > s && text.as_bytes()[e - 1] == b'.' {
                    e -= 1;
                }
                while s < e && text.as_bytes()[s] == b'\\' {
                    s += 1;
                }
                if s < e && !credentials.iter().any(|&(cs, ce)| s < ce && cs < e) {
                    out.push(Span::new(s, e, PiiKind::Email));
                }
            }
        }
        if self.wants(PiiKind::CreditCard) {
            for m in re!(r"\b\d(?:[ \-]?\d){12,18}\b").find_iter(text) {
                let digits: String = m.as_str().chars().filter(|c| c.is_ascii_digit()).collect();
                // The whole number run, not a Luhn-valid suffix of a longer
                // one (`4111 1111 1111 1112` is not a card).
                if !maximal_digit_run(text, m.start(), m.end())
                    || !token_bounded(text, m.start(), m.end())
                {
                    continue;
                }
                // Luhn alone passes a tenth of random digit pairs (#694):
                // the number must also open with a brand's IIN at that
                // brand's length, and be grouped the way cards are printed.
                if card_brand(&digits).is_some()
                    && card_grouping_ok(m.as_str())
                    && luhn_valid(&digits)
                {
                    out.push(Span::new(m.start(), m.end(), PiiKind::CreditCard));
                }
            }
        }
        if self.wants(PiiKind::Iban) {
            for m in
                re!(r"\b[A-Z]{2}\d{2}(?: ?[A-Z0-9]{4}){2,7}(?: ?[A-Z0-9]{1,4})?\b").find_iter(text)
            {
                let compact: String = m.as_str().chars().filter(|c| *c != ' ').collect();
                if iban_valid(&compact) {
                    out.push(Span::new(m.start(), m.end(), PiiKind::Iban));
                }
            }
        }
        if self.wants(PiiKind::IpAddress) {
            for m in re!(r"\b(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(?:\.(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}\b").find_iter(text) {
                // Not the middle of a longer dotted run (`1.2.3.4.5`).
                let before = text[..m.start()].chars().next_back();
                let after = text[m.end()..].chars().next();
                if before == Some('.') || after == Some('.') {
                    continue;
                }
                // `v2.0.0.1` is a version, not an address.
                if matches!(before, Some('v') | Some('V')) {
                    continue;
                }
                out.push(Span::new(m.start(), m.end(), PiiKind::IpAddress));
            }
            for m in re!(r"[0-9A-Fa-f:]{3,45}").find_iter(text) {
                let s = m.as_str();
                if s.matches(':').count() < 2 || !s.chars().any(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                if s.parse::<std::net::Ipv6Addr>().is_ok() {
                    out.push(Span::new(m.start(), m.end(), PiiKind::IpAddress));
                }
            }
        }
        if self.wants(PiiKind::NationalId) {
            // US SSN: AAA-GG-SSSS with the SSA's never-issued ranges excluded
            // — and not a legal citation (`rule 412-18-0150`, `§ 310-20-1000`,
            // #694): a citation word right before it says what it is.
            for m in re!(r"\b\d{3}-\d{2}-\d{4}\b").find_iter(text) {
                let s = m.as_str();
                let area = &s[0..3];
                if area == "000" || area == "666" || area.starts_with('9') {
                    continue;
                }
                if &s[4..6] == "00" || &s[7..11] == "0000" {
                    continue;
                }
                if !token_bounded(text, m.start(), m.end()) || citation_before(text, m.start()) {
                    continue;
                }
                out.push(Span::new(m.start(), m.end(), PiiKind::NationalId));
            }
            // UK National Insurance number, written compact (`AB123456C`)
            // or fully spaced (`AB 12 34 56 C`) — never half and half, which
            // is how a part number and the next word line up (`TR482913 B`).
            for m in re!(
                r"\b[A-CEGHJ-PR-TW-Z][A-CEGHJ-NPR-TW-Z](?:\d{6}[A-D]| \d{2} \d{2} \d{2} [A-D])\b"
            )
            .find_iter(text)
            {
                let prefix = &m.as_str()[0..2];
                if ["BG", "GB", "NK", "KN", "TN", "NT", "ZZ"].contains(&prefix) {
                    continue;
                }
                if !token_bounded(text, m.start(), m.end()) {
                    continue;
                }
                out.push(Span::new(m.start(), m.end(), PiiKind::NationalId));
            }
            // Indian Aadhaar: 12 digits, first 2–9, grouped 4-4-4 with one
            // separator throughout (or none), the first group not a year
            // (`2019 95753514` is a date and a record number), Verhoeff
            // check digit.
            for m in re!(r"\b[2-9]\d{3}([ \-]?)\d{4}([ \-]?)\d{4}\b").captures_iter(text) {
                let whole = m.get(0).expect("match");
                let (sep1, sep2) = (
                    m.get(1).map_or("", |g| g.as_str()),
                    m.get(2).map_or("", |g| g.as_str()),
                );
                if sep1 != sep2 || !token_bounded(text, whole.start(), whole.end()) {
                    continue;
                }
                let s = whole.as_str();
                if !sep1.is_empty() && (s.starts_with("19") || s.starts_with("20")) {
                    continue;
                }
                let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
                if verhoeff_valid(&digits) {
                    out.push(Span::new(whole.start(), whole.end(), PiiKind::NationalId));
                }
            }
        }
        if self.wants(PiiKind::Phone) {
            let ssn_shape = re!(r"^\d{3}-\d{2}-\d{4}$");
            for m in re!(r"(?:\+\d{1,3}[ .\-]?)?(?:\(\d{1,4}\)[ .\-]?)?\d{2,4}(?:[ .\-]\d{2,4}){1,4}\b|\+\d{7,15}\b").find_iter(text) {
                let s = m.as_str();
                let digits = s.chars().filter(|c| c.is_ascii_digit()).count();
                if !(7..=15).contains(&digits) || looks_like_date_or_version(s) {
                    continue;
                }
                // `AAA-GG-SSSS` is an SSN shape, valid or not — never a phone.
                if ssn_shape.is_match(s) {
                    continue;
                }
                // Whole tokens only (#694): `IL 62704-1234` must not yield
                // `2704-1234`, nor `60601 0001 12500` a run that starts
                // inside a number.
                if !token_bounded(text, m.start(), m.end()) {
                    continue;
                }
                // Digit groups alone are not a phone — a ZIP+4, a rule
                // number, two record IDs side by side all look like one.
                // It takes a shape phones are written in: a `+` country
                // code, a parenthesized area code, NANP's 3-3-4, a national
                // number of ten or more digits in three or more groups, or
                // a word nearby that says it is a phone.
                if !phone_shape(s, digits) && !phone_keyword_before(text, m.start()) {
                    continue;
                }
                out.push(Span::new(m.start(), m.end(), PiiKind::Phone));
            }
        }
        for (name, re) in &self.custom {
            for c in re.captures_iter(text) {
                let m = c.get(1).or_else(|| c.get(0)).expect("match");
                if m.start() == m.end() {
                    continue;
                }
                out.push(Span {
                    start: m.start(),
                    end: m.end(),
                    kind: PiiKind::Custom,
                    score: 1.0,
                    name: Some(name.clone()),
                });
            }
        }
        for re in &self.deny {
            for m in re.find_iter(text) {
                out.push(Span::new(m.start(), m.end(), PiiKind::Custom));
            }
        }
        out.retain(|s| !self.allowed(&text[s.start..s.end]));
        out
    }
}

/// Whether `text[start..end]` is a whole run of digit groups: not preceded
/// or followed by another digit group through a space or dash.
fn maximal_digit_run(text: &str, start: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    let before =
        start >= 2 && matches!(bytes[start - 1], b' ' | b'-') && bytes[start - 2].is_ascii_digit();
    let after = end + 1 < bytes.len()
        && matches!(bytes[end], b' ' | b'-')
        && bytes[end + 1].is_ascii_digit();
    !before && !after
}

/// Whether `text[start..end]` begins and ends at a token boundary: the
/// character before and the one after are neither letters nor digits
/// (#694 — no match may start or end inside a longer alphanumeric token).
fn token_bounded(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    // A match that opens with `+` or `(` is its own left edge: the runs of
    // an inline paragraph are joined without the whitespace between them
    // (`or` + `+1 (555) …`), and a letter before a `+` is no digit run.
    let opens_itself = text[start..].starts_with(['+', '(']);
    let left_ok = match before {
        Some(c) if opens_itself => !c.is_ascii_digit(),
        Some(c) => !c.is_alphanumeric(),
        None => true,
    };
    left_ok && !after.is_some_and(|c| c.is_alphanumeric())
}

/// The shapes a phone number is written in (the digit groups alone are
/// not one): `+` country code; `(area)`; NANP `3-3-4` or `1-3-3-4`; a
/// national number of ten or eleven digits in three or more groups, or
/// one with a trunk `0` of up to thirteen (`020 7946 0958`, `0151 496
/// 0000`) — a `4-4-4` of twelve digits is a record number.
fn phone_shape(s: &str, digits: usize) -> bool {
    if s.starts_with('+') || s.contains('(') {
        return true;
    }
    let groups: Vec<usize> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|g| !g.is_empty())
        .map(str::len)
        .collect();
    groups == [3, 3, 4]
        || groups == [1, 3, 3, 4]
        || (groups.len() >= 3
            && ((10..=11).contains(&digits) || (s.starts_with('0') && digits <= 13)))
}

/// Whether a word that announces a phone number (`tel`, `phone`, `fax`,
/// `mobile`, `call`, …) stands within a few words before `pos`.
fn phone_keyword_before(text: &str, pos: usize) -> bool {
    let head = &text[..pos];
    let from = head.char_indices().rev().nth(30).map_or(0, |(i, _)| i);
    re!(r"(?i)\b(?:tel|tel\.|telephone|phone|fax|mobile|cell|call|contact|whatsapp|hotline|dial|sms|text)\b[^\d]*$")
        .is_match(&head[from..])
}

/// Whether a legal-citation word stands right before `pos`: `rule`,
/// `section`, `sec.`, `§`, `code`, `article`, `art.`, `para.`, `clause`,
/// `chapter`, `title`, `regulation`, `reg.`, `order`, `statute`, `part`.
fn citation_before(text: &str, pos: usize) -> bool {
    let head = text[..pos].trim_end();
    let from = head.char_indices().rev().nth(16).map_or(0, |(i, _)| i);
    re!(r"(?i)(?:\b(?:rule|rules|section|sections|sec|code|article|art|para|paragraph|clause|chapter|title|regulation|reg|order|statute|part|no|number)\.?|§+)\s*(?:no\.?\s*)?$")
        .is_match(&head[from..])
}

/// The card brand a digit string opens with, at a length that brand issues
/// (ISO/IEC 7812 IIN ranges): `None` for a number no brand would print.
fn card_brand(digits: &str) -> Option<&'static str> {
    let n = digits.len();
    let p = |k: usize| digits[..k.min(n)].parse::<u32>().unwrap_or(0);
    let (two, three, four) = (p(2), p(3), p(4));
    if digits.starts_with('4') && matches!(n, 13 | 16 | 19) {
        return Some("visa");
    }
    if ((51..=55).contains(&two) || (2221..=2720).contains(&four)) && n == 16 {
        return Some("mastercard");
    }
    if (two == 34 || two == 37) && n == 15 {
        return Some("amex");
    }
    if (four == 6011 || two == 65 || (644..=649).contains(&three)) && (16..=19).contains(&n) {
        return Some("discover");
    }
    if ((300..=305).contains(&three) || two == 36 || two == 38 || two == 39)
        && (14..=19).contains(&n)
    {
        return Some("diners");
    }
    if (3528..=3589).contains(&four) && (16..=19).contains(&n) {
        return Some("jcb");
    }
    if two == 62 && (16..=19).contains(&n) {
        return Some("unionpay");
    }
    if matches!(
        four,
        5018 | 5020 | 5038 | 5893 | 6304 | 6759 | 6761 | 6762 | 6763
    ) && (12..=19).contains(&n)
    {
        return Some("maestro");
    }
    None
}

/// Whether a card number's separators fall where cards print them: none,
/// groups of four (a last group of one to four), or Amex's 4-6-5 / Diners'
/// 4-6-4. `88061052 76665755`, two record IDs, is 8-8.
fn card_grouping_ok(s: &str) -> bool {
    let groups: Vec<usize> = s
        .split(|c: char| !c.is_ascii_digit())
        .filter(|g| !g.is_empty())
        .map(str::len)
        .collect();
    if groups.len() == 1 {
        return true;
    }
    let (last, head) = groups.split_last().expect("non-empty");
    (head.iter().all(|&g| g == 4) && (1..=4).contains(last))
        || groups == [4, 6, 5]
        || groups == [4, 6, 4]
}

/// Luhn (ISO/IEC 7812-1) check over a digit string.
pub fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for c in digits.chars().rev() {
        let Some(d) = c.to_digit(10) else {
            return false;
        };
        let d = if double {
            let x = d * 2;
            if x > 9 {
                x - 9
            } else {
                x
            }
        } else {
            d
        };
        sum += d;
        double = !double;
    }
    digits.len() >= 2 && sum % 10 == 0
}

/// IBAN length per country (the SWIFT registry); an unknown country is not
/// an IBAN for this detector, which keeps random uppercase-plus-digit tokens
/// (`XY12…`) from matching on mod-97 luck alone.
fn iban_length(country: &str) -> Option<usize> {
    Some(match country {
        "AL" | "AZ" | "CY" | "DO" | "GT" | "HU" | "LB" | "PL" | "BY" | "SV" | "NI" => 28,
        "AD" | "CZ" | "MD" | "PK" | "RO" | "SA" | "SK" | "ES" | "SE" | "TN" | "VG" => 24,
        "AT" | "BA" | "EE" | "KZ" | "LT" | "LU" | "XK" | "MN" => 20,
        "BH" | "BG" | "CR" | "GE" | "DE" | "IE" | "ME" | "RS" | "GB" | "VA" => 22,
        "BE" | "BI" => 16,
        "BR" | "PS" | "QA" | "UA" | "EG" => 29,
        "HR" | "LI" | "CH" => 21,
        "DK" | "FO" | "FI" | "GL" | "NL" | "SD" | "FK" => 18,
        "FR" | "GR" | "IT" | "MR" | "MC" | "SM" | "DJ" => 27,
        "GI" | "IL" | "AE" | "IQ" | "TL" | "SO" | "OM" => 23,
        "IS" => 26,
        "JO" | "KW" | "MU" | "YE" => 30,
        "LV" => 21,
        "MK" | "SI" => 19,
        "MT" => 31,
        "NO" => 15,
        "PT" | "ST" | "LY" => 25,
        "TR" => 26,
        "LC" => 32,
        "RU" => 33,
        _ => return None,
    })
}

/// ISO 13616 mod-97 check over a compact (no spaces) IBAN.
pub fn iban_valid(iban: &str) -> bool {
    if iban.len() < 15 || iban.len() > 34 || !iban.is_ascii() {
        return false;
    }
    let country = &iban[0..2];
    if iban_length(country) != Some(iban.len()) {
        return false;
    }
    let rearranged = format!("{}{}", &iban[4..], &iban[..4]);
    let mut rem = 0u32;
    for c in rearranged.chars() {
        let v = match c {
            '0'..='9' => c as u32 - '0' as u32,
            'A'..='Z' => c as u32 - 'A' as u32 + 10,
            _ => return false,
        };
        rem = if v >= 10 {
            (rem * 100 + v) % 97
        } else {
            (rem * 10 + v) % 97
        };
    }
    rem == 1
}

/// Verhoeff check (the Aadhaar check digit).
pub fn verhoeff_valid(digits: &str) -> bool {
    const D: [[u8; 10]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
        [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
        [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
        [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
        [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
        [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
        [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
        [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
        [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
    ];
    const P: [[u8; 10]; 8] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
        [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
        [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
        [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
        [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
        [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
        [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
    ];
    let mut c = 0u8;
    for (i, ch) in digits.chars().rev().enumerate() {
        let Some(d) = ch.to_digit(10) else {
            return false;
        };
        c = D[c as usize][P[i % 8][d as usize] as usize];
    }
    !digits.is_empty() && c == 0
}

/// `2024-01-15`, `15.01.2024`, `01/15/24`, `1.2.3`, `10.0.1` — digit groups
/// that are a date or a version number, not a phone.
fn looks_like_date_or_version(s: &str) -> bool {
    let s = s.trim();
    re!(r"^\d{4}[\-./]\d{1,2}[\-./]\d{1,2}$").is_match(s)
        || re!(r"^\d{1,2}[\-./]\d{1,2}[\-./]\d{2,4}$").is_match(s)
        // Dotted groups with no other separator: a version string.
        || (s.contains('.') && !s.contains(' ') && !s.contains('-') && !s.starts_with('+'))
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// The pass's state across one document: the pseudonym numbering and the
/// report. [`DoclingDocument::redact`] drives it over a whole document; the
/// streaming converter drives it over each page batch so the Markdown it
/// emits matches the buffered export byte for byte.
pub struct Redactor<'a> {
    opts: &'a RedactionOptions,
    detector: &'a dyn PiiDetector,
    /// The allow terms and patterns, asked about every span whichever
    /// detector produced it (an invalid pattern was rejected by the
    /// options; here it is simply absent).
    allow: AllowList,
    /// `(label, original) → number`, first-seen order per label.
    numbers: HashMap<(String, String), usize>,
    next: HashMap<String, usize>,
    report: RedactionReport,
    /// Off while the item tree is walked: a DOCX / HTML document holds its
    /// text twice (the flat nodes and the tree the JSON export reads), and
    /// the report counts a value once, not per representation.
    counting: bool,
}

impl<'a> Redactor<'a> {
    pub fn new(opts: &'a RedactionOptions, detector: &'a dyn PiiDetector) -> Self {
        Redactor {
            opts,
            detector,
            allow: AllowList::new(opts).unwrap_or_default(),
            numbers: HashMap::new(),
            next: HashMap::new(),
            report: RedactionReport {
                mapping: opts.return_mapping.then(Vec::new),
                ..Default::default()
            },
            counting: true,
        }
    }

    /// The report so far (the final one after the last batch).
    pub fn report(&self) -> &RedactionReport {
        &self.report
    }

    pub fn finish(self) -> RedactionReport {
        self.report
    }

    /// The spans of `text` the options keep, non-overlapping, in order:
    /// a longer span wins over a shorter one it overlaps, equal lengths by
    /// kind precedence; NER spans under the score floor and out-of-scope
    /// kinds are dropped.
    fn spans(&self, text: &str) -> Vec<Span> {
        let mut spans: Vec<Span> = self
            .detector
            .detect(text)
            .into_iter()
            .filter(|s| s.start < s.end && s.end <= text.len())
            .filter(|s| text.is_char_boundary(s.start) && text.is_char_boundary(s.end))
            .filter(|s| self.opts.wants(s.kind))
            .filter(|s| !PiiKind::NER.contains(&s.kind) || s.score >= self.opts.ner_min_score)
            .filter(|s| !self.allow.allows(&text[s.start..s.end]))
            .collect();
        spans.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then((b.end - b.start).cmp(&(a.end - a.start)))
                .then(a.kind.cmp(&b.kind))
        });
        let mut kept: Vec<Span> = Vec::with_capacity(spans.len());
        for s in spans {
            match kept.last() {
                Some(last) if s.start < last.end => {
                    // Overlap: keep the longer (or, equal, the earlier kind).
                    if s.end - s.start > last.end - last.start {
                        kept.pop();
                        kept.push(s);
                    }
                }
                _ => kept.push(s),
            }
        }
        kept
    }

    /// Whether `text` carries any span the options keep — what the image
    /// box-out asks per OCR line.
    pub fn has_pii(&self, text: &str) -> bool {
        !self.spans(text).is_empty()
    }

    fn label_of(span: &Span) -> String {
        span.name
            .clone()
            .unwrap_or_else(|| span.kind.label().to_string())
    }

    /// The placeholder for one span of `original`.
    fn placeholder(&mut self, span: &Span, original: &str) -> String {
        let label = Self::label_of(span);
        if self.counting {
            *self.report.counts.entry(label.clone()).or_insert(0) += 1;
            self.report.total += 1;
        }
        let out = match &self.opts.replacement {
            Replacement::Label => format!("[{label}]"),
            Replacement::Fixed(s) => s.clone(),
            Replacement::Pseudonym => {
                // The same value — case-folded for e-mails and names,
                // whitespace-folded for numbers — gets the same number.
                let key = normalize(original);
                let n = match self.numbers.get(&(label.clone(), key.clone())) {
                    Some(n) => *n,
                    None => {
                        let next = self.next.entry(label.clone()).or_insert(0);
                        *next += 1;
                        self.numbers.insert((label.clone(), key), *next);
                        *next
                    }
                };
                format!("[{label}_{n}]")
            }
        };
        if let Some(map) = self.report.mapping.as_mut() {
            if !map.iter().any(|(o, _)| o == original) {
                map.push((original.to_string(), out.clone()));
            }
        }
        out
    }

    /// Redact one string in place.
    pub fn redact_text(&mut self, text: &mut String) {
        let spans = self.spans(text);
        if spans.is_empty() {
            return;
        }
        let mut out = String::with_capacity(text.len());
        let mut pos = 0;
        for span in &spans {
            out.push_str(&text[pos..span.start]);
            let original = text[span.start..span.end].to_string();
            out.push_str(&self.placeholder(span, &original));
            pos = span.end;
        }
        out.push_str(&text[pos..]);
        *text = out;
    }

    fn redact_opt(&mut self, text: &mut Option<String>) {
        if let Some(t) = text.as_mut() {
            self.redact_text(t);
        }
    }

    /// Redact a run sequence as one text: spans are found on the joined
    /// runs and written back — the run the span starts in gets the
    /// placeholder, the runs it crosses lose the covered characters.
    pub fn redact_runs(&mut self, runs: &mut [InlineRun]) {
        let joined: String = runs.iter().map(|r| r.text.as_str()).collect();
        let spans = self.spans(&joined);
        if spans.is_empty() {
            return;
        }
        // Run byte ranges in the joined text.
        let mut starts = Vec::with_capacity(runs.len());
        let mut acc = 0;
        for r in runs.iter() {
            starts.push(acc);
            acc += r.text.len();
        }
        // Rebuild every run's text from the joined text with the spans
        // applied: for each run, walk its range and copy the pieces outside
        // spans; a span starting inside the run contributes its placeholder.
        let mut placeholders: Vec<String> = Vec::with_capacity(spans.len());
        for span in &spans {
            let original = joined[span.start..span.end].to_string();
            placeholders.push(self.placeholder(span, &original));
        }
        for (i, run) in runs.iter_mut().enumerate() {
            let (rs, re) = (starts[i], starts[i] + run.text.len());
            let mut out = String::new();
            let mut pos = rs;
            for (span, ph) in spans.iter().zip(&placeholders) {
                if span.end <= rs || span.start >= re {
                    continue;
                }
                let s = span.start.max(rs);
                out.push_str(&joined[pos..s]);
                if span.start >= rs {
                    out.push_str(ph);
                }
                pos = span.end.min(re);
            }
            out.push_str(&joined[pos..re]);
            run.text = out;
        }
    }

    fn redact_table(&mut self, table: &mut Table) {
        for row in table.rows.iter_mut() {
            for cell in row.iter_mut() {
                self.redact_text(cell);
            }
        }
        if let Some(cells) = table.cells.as_mut() {
            for c in cells.iter_mut() {
                self.redact_text(&mut c.text);
            }
        }
        if let Some(blocks) = table.cell_blocks.as_mut() {
            for row in blocks.iter_mut() {
                for cell in row.iter_mut() {
                    self.redact_nodes(cell);
                }
            }
        }
        self.redact_opt(&mut table.caption);
    }

    fn redact_fields(&mut self, items: &mut [crate::FieldItem]) {
        for f in items.iter_mut() {
            self.redact_opt(&mut f.marker);
            self.redact_opt(&mut f.key);
            self.redact_opt(&mut f.value);
        }
    }

    fn redact_graph(&mut self, cells: &mut [crate::GraphCell]) {
        for c in cells.iter_mut() {
            self.redact_text(&mut c.text);
            self.redact_text(&mut c.orig);
        }
    }

    fn redact_image(&mut self, image: &mut Option<crate::PictureImage>) {
        if self.opts.images == ImageRedaction::Drop {
            *image = None;
        }
    }

    /// Redact every string of `nodes`, recursively.
    pub fn redact_nodes(&mut self, nodes: &mut [Node]) {
        for node in nodes.iter_mut() {
            self.redact_node(node);
        }
    }

    fn redact_node(&mut self, node: &mut Node) {
        // A marked PDF item (#1005) is redacted as its plain item: the runs
        // split a name across a script boundary (`Smith²`) where a pattern
        // would miss it, and each value is counted once.
        if let Node::ScriptText { inner, .. } = node {
            let plain = std::mem::replace(inner.as_mut(), Node::PageBreak);
            *node = plain;
        }
        match node {
            Node::Heading { text, .. }
            | Node::Paragraph { text }
            | Node::ListText { text, .. }
            | Node::CheckboxItem { text, .. }
            | Node::PageFurniture { text, .. }
            | Node::FurnitureText { text, .. }
            | Node::TextDump(text) => self.redact_text(text),
            Node::ListItem {
                text,
                marker,
                dclx,
                split,
                href,
                ..
            } => {
                self.redact_text(text);
                self.redact_opt(marker);
                self.redact_opt(href);
                if let Some(s) = split.as_mut() {
                    self.redact_text(&mut s.orig);
                    self.redact_text(&mut s.text);
                    self.redact_text(&mut s.marker);
                }
                if let Some(d) = dclx.as_mut() {
                    self.redact_text(&mut d.text);
                    self.redact_opt(&mut d.marker);
                    self.redact_runs(&mut d.runs);
                }
            }
            Node::Code {
                text, orig, pretty, ..
            } => {
                self.redact_text(text);
                self.redact_opt(orig);
                self.redact_opt(pretty);
            }
            Node::Table(t) => self.redact_table(t),
            Node::Picture {
                caption,
                caption_href,
                image,
                description,
                ..
            } => {
                self.redact_opt(caption);
                self.redact_opt(caption_href);
                // The picture-OCR text (#645) is read off the image: redact
                // it like any other text.
                if let Some(d) = description.as_mut() {
                    self.redact_text(&mut d.text);
                }
                self.redact_image(image);
            }
            // The LaTeX is a rendering of the formula's glyphs; `orig` is
            // the extracted text a reader could recover a value from.
            Node::Formula { orig, .. } => self.redact_text(orig),
            Node::Caption { text, href } | Node::LabeledText { text, href, .. } => {
                self.redact_text(text);
                self.redact_opt(href);
            }
            Node::Chart { table, caption, .. } => {
                self.redact_table(table);
                self.redact_opt(caption);
            }
            Node::Group { name, children, .. } => {
                self.redact_opt(name);
                self.redact_nodes(children);
            }
            Node::FieldRegion { items } => self.redact_fields(items),
            Node::KeyValueGraph { cells, .. } => self.redact_graph(cells),
            Node::InlineGroup { runs, md_text, .. } => {
                self.redact_runs(runs);
                self.redact_text(md_text);
            }
            // Replaced by its plain item above.
            Node::ScriptText { .. } => {}
            Node::Furniture { inner, .. }
            | Node::Commented { inner, .. }
            | Node::Located { inner, .. }
            | Node::Prov { inner, .. }
            | Node::DoclangOnly(inner) => self.redact_node(inner),
            // A transcript cue (#694): the words are redacted once, in
            // `cue`, and spliced into the paragraph behind its `[time: …]`
            // prefix — the timing is never a phone number, and the value is
            // counted once, not per representation.
            Node::Track { track, cue, inner } => {
                self.redact_opt(&mut track.voice);
                let original = cue.clone();
                self.redact_text(cue);
                match inner.as_mut() {
                    Node::Paragraph { text, .. } if text.ends_with(&original) => {
                        let prefix_len = text.len() - original.len();
                        text.truncate(prefix_len);
                        text.push_str(cue);
                    }
                    other => {
                        let counting = std::mem::replace(&mut self.counting, false);
                        self.redact_node(other);
                        self.counting = counting;
                    }
                }
            }
            Node::CommentSection { name, text, .. } => {
                self.redact_text(name);
                self.redact_text(text);
            }
            Node::PictureChildren(children) => self.redact_nodes(children),
            Node::PageBreak
            | Node::PageInfo { .. }
            | Node::ProvContinuation { .. }
            | Node::Anchor { .. } => {}
        }
    }

    /// Redact the item tree (#621: the JSON export reads it, not the nodes,
    /// for the backends that build one). The same pseudonym numbering as
    /// the nodes got; the counts are not incremented again — the tree is
    /// the same text in docling's shape, not more of it.
    pub fn redact_tree(&mut self, tree: &mut ItemTree) {
        let counting = std::mem::replace(&mut self.counting, false);
        self.redact_tree_items(tree);
        self.counting = counting;
    }

    fn redact_tree_items(&mut self, tree: &mut ItemTree) {
        for item in tree.items.iter_mut() {
            match &mut item.kind {
                TreeKind::Text {
                    text,
                    orig,
                    hyperlink,
                    list,
                    ..
                } => {
                    self.redact_text(text);
                    self.redact_opt(orig);
                    self.redact_opt(hyperlink);
                    if let Some(l) = list.as_mut() {
                        self.redact_text(&mut l.marker);
                    }
                }
                TreeKind::Code {
                    text,
                    orig,
                    hyperlink,
                    ..
                } => {
                    self.redact_text(text);
                    self.redact_opt(orig);
                    self.redact_opt(hyperlink);
                }
                TreeKind::Group { name, .. } => self.redact_text(name),
                TreeKind::Table { table, .. } => self.redact_table(table),
                TreeKind::Picture {
                    image,
                    chart,
                    description,
                    ..
                } => {
                    self.redact_image(image);
                    if let Some(d) = description.as_mut() {
                        self.redact_text(&mut d.text);
                    }
                    if let Some(t) = chart.as_mut() {
                        self.redact_table(t);
                    }
                }
                TreeKind::FieldRegion { items } => self.redact_fields(items),
                TreeKind::KeyValueGraph { cells, .. } => self.redact_graph(cells),
            }
            if let Some(track) = item.source.as_mut() {
                self.redact_opt(&mut track.voice);
            }
            for note in item.notes.iter_mut() {
                self.redact_text(&mut note.text);
            }
        }
    }

    /// Redact the link table (`(text, href)` pairs).
    pub fn redact_links(&mut self, links: &mut [(String, String)]) {
        for (text, href) in links.iter_mut() {
            self.redact_text(text);
            self.redact_text(href);
        }
    }

    /// The whole document: nodes, tree, links, page renders.
    pub fn redact_document(&mut self, doc: &mut DoclingDocument) {
        self.redact_nodes(&mut doc.nodes);
        if let Some(tree) = doc.tree.as_mut() {
            self.redact_tree(tree);
        }
        self.redact_links(&mut doc.links);
        self.redact_metadata(&mut doc.metadata);
        if self.opts.images == ImageRedaction::Drop {
            doc.page_images.clear();
        }
    }

    /// The document's metadata (#679). The author fields — `authors`,
    /// `last_modified_by`, EXIF's `Artist` / `XPAuthor`, an audio file's
    /// `artist` / `album_artist` / `composer` / `performer` — are names by
    /// definition, so each is masked whole as a `PERSON` without asking a
    /// detector (an allow term still exempts it); the GPS position and the
    /// `GPS*` EXIF tags are dropped, counted as `LOCATION`; every other
    /// string goes through the detectors like a paragraph (an e-mail in a
    /// description, a phone number in a comment). The kinds filter applies:
    /// `kinds=email` leaves the names and the position alone.
    pub fn redact_metadata(&mut self, m: &mut crate::metadata::DocumentMetadata) {
        if self.opts.wants(PiiKind::Person) {
            for a in m.authors.iter_mut() {
                self.redact_whole(a, PiiKind::Person);
            }
            if let Some(by) = m.last_modified_by.as_mut() {
                self.redact_whole(by, PiiKind::Person);
            }
            for key in ["Artist", "XPAuthor"] {
                if let Some(v) = m.exif.get_mut(key) {
                    self.redact_whole(v, PiiKind::Person);
                }
            }
            for key in ["artist", "album_artist", "composer", "performer"] {
                if let Some(v) = m.audio.get_mut(key) {
                    self.redact_whole(v, PiiKind::Person);
                }
            }
        }
        if self.opts.wants(PiiKind::Location) {
            let had_gps = m.gps.take().is_some();
            let before = m.exif.len();
            m.exif.retain(|k, _| !k.starts_with("GPS"));
            if had_gps || m.exif.len() != before {
                self.count(PiiKind::Location.label());
            }
        }
        self.redact_opt(&mut m.title);
        self.redact_opt(&mut m.subject);
        self.redact_opt(&mut m.description);
        for k in m.keywords.iter_mut() {
            self.redact_text(k);
        }
        self.redact_opt(&mut m.publisher);
        self.redact_opt(&mut m.rights);
        self.redact_opt(&mut m.identifier);
        self.redact_opt(&mut m.creator_tool);
        for v in m.properties.values_mut() {
            self.redact_text(v);
        }
        for (k, v) in m.exif.iter_mut() {
            if k != "Artist" && k != "XPAuthor" {
                self.redact_text(v);
            }
        }
        for (k, v) in m.audio.iter_mut() {
            if !matches!(
                k.as_str(),
                "artist" | "album_artist" | "composer" | "performer"
            ) {
                self.redact_text(v);
            }
        }
    }

    /// Replace a whole value with the placeholder of `kind` — a field that
    /// is a name by definition. An empty value and an allowed term stay.
    fn redact_whole(&mut self, text: &mut String, kind: PiiKind) {
        if text.trim().is_empty() || self.allow.allows(text) {
            return;
        }
        let span = Span::new(0, text.len(), kind);
        let original = text.clone();
        *text = self.placeholder(&span, &original);
    }

    /// Count a removal that leaves no placeholder behind.
    fn count(&mut self, label: &str) {
        if self.counting {
            *self.report.counts.entry(label.to_string()).or_insert(0) += 1;
            self.report.total += 1;
        }
    }
}

/// The pseudonym key of a value: case-folded, inner whitespace collapsed,
/// so `John Smith` / `john smith` / `JOHN  SMITH` share one number and
/// `+1 555 123 4567` / `+1-555-123-4567` do too.
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '.' && *c != '(' && *c != ')')
        .collect()
}

impl DoclingDocument {
    /// Redact the document in place with `detector` (#621) and return what
    /// was redacted. See the [module docs](self) for what is walked. The
    /// built-in [`PatternDetector`] is `PatternDetector::new(&opts)?`.
    pub fn redact(
        &mut self,
        opts: &RedactionOptions,
        detector: &dyn PiiDetector,
    ) -> RedactionReport {
        let mut r = Redactor::new(opts, detector);
        r.redact_document(self);
        r.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector(opts: &RedactionOptions) -> PatternDetector {
        PatternDetector::new(opts).unwrap()
    }

    fn redact(text: &str) -> String {
        let opts = RedactionOptions::default();
        let d = detector(&opts);
        let mut s = text.to_string();
        Redactor::new(&opts, &d).redact_text(&mut s);
        s
    }

    fn kinds(text: &str) -> Vec<PiiKind> {
        let opts = RedactionOptions::default();
        let d = detector(&opts);
        Redactor::new(&opts, &d)
            .spans(text)
            .into_iter()
            .map(|s| s.kind)
            .collect()
    }

    #[test]
    fn emails_including_markdown_links_and_escapes() {
        assert_eq!(
            redact("Write to [john.doe@example.com](mailto:john.doe@example.com)."),
            "Write to [[EMAIL]](mailto:[EMAIL])."
        );
        // docling escapes `_` in text; the escape is part of the address.
        assert_eq!(
            redact(r"jane\_doe@example.org, please."),
            "[EMAIL], please."
        );
        assert_eq!(redact("a@b"), "a@b");
    }

    #[test]
    fn cards_need_luhn_and_ibans_need_mod97() {
        assert_eq!(kinds("card 4111 1111 1111 1111"), vec![PiiKind::CreditCard]);
        assert_eq!(kinds("card 4111-1111-1111-1111"), vec![PiiKind::CreditCard]);
        assert_eq!(kinds("order 4111 1111 1111 1112"), vec![]);
        assert_eq!(
            kinds("IBAN DE89 3704 0044 0532 0130 00"),
            vec![PiiKind::Iban]
        );
        assert_eq!(kinds("IBAN GB82WEST12345698765432"), vec![PiiKind::Iban]);
        assert_eq!(kinds("IBAN DE89 3704 0044 0532 0130 01"), vec![]);
        assert_eq!(kinds("code XY12 ABCD 1234 EFGH 5678"), vec![]);
    }

    #[test]
    fn phones_but_not_dates_or_versions() {
        assert_eq!(kinds("call +1 (555) 123-4567"), vec![PiiKind::Phone]);
        assert_eq!(kinds("call 555-123-4567 today"), vec![PiiKind::Phone]);
        assert_eq!(kinds("tel +44 20 7946 0958"), vec![PiiKind::Phone]);
        assert_eq!(kinds("on 2024-01-15 and 15.01.2024 and 01/15/24"), vec![]);
        assert_eq!(kinds("version 1.2.3 and v10.0.1 and v2.0.0.1"), vec![]);
        assert_eq!(kinds("ISBN 978-3-16-148410-0"), vec![]);
    }

    #[test]
    fn ip_addresses_and_url_credentials() {
        assert_eq!(kinds("host 192.168.1.10 up"), vec![PiiKind::IpAddress]);
        assert_eq!(kinds("v 1.2.3.4.5"), vec![]);
        assert_eq!(
            kinds("addr 2001:db8::1 and ::1"),
            vec![PiiKind::IpAddress, PiiKind::IpAddress]
        );
        assert_eq!(kinds("at 12:30:45 today"), vec![]);
        assert_eq!(kinds("mac aa:bb:cc:dd:ee:ff"), vec![]);
        assert_eq!(
            redact("see https://alice:s3cret@db.example.com/x"),
            "see https://[CREDENTIALS]@db.example.com/x"
        );
    }

    #[test]
    fn national_ids_with_their_rules() {
        assert_eq!(kinds("SSN 123-45-6789"), vec![PiiKind::NationalId]);
        assert_eq!(
            kinds("not 000-45-6789 nor 123-00-6789 nor 900-45-6789"),
            vec![]
        );
        assert_eq!(kinds("NINO AB 12 34 56 C"), vec![PiiKind::NationalId]);
        assert_eq!(kinds("NINO QQ 12 34 56 C"), vec![]);
        assert_eq!(kinds("NINO BG 12 34 56 C"), vec![]);
        // Verhoeff: 2234 5678 9012 fails, the digit that passes is found by
        // the check itself.
        let base = "22345678901";
        let ok = (0..10)
            .map(|d| format!("{base}{d}"))
            .find(|s| verhoeff_valid(s))
            .unwrap();
        assert_eq!(kinds(&format!("Aadhaar {ok}")), vec![PiiKind::NationalId]);
        let bad = (0..10)
            .map(|d| format!("{base}{d}"))
            .find(|s| !verhoeff_valid(s))
            .unwrap();
        assert_eq!(kinds(&format!("Aadhaar {bad}")), vec![]);
    }

    #[test]
    fn replacement_modes_and_pseudonym_consistency() {
        let opts = RedactionOptions {
            replacement: Replacement::Pseudonym,
            return_mapping: true,
            ..Default::default()
        };
        let d = detector(&opts);
        let mut r = Redactor::new(&opts, &d);
        let mut a = "a@x.com wrote to b@x.com; A@X.COM again".to_string();
        r.redact_text(&mut a);
        assert_eq!(a, "[EMAIL_1] wrote to [EMAIL_2]; [EMAIL_1] again");
        let mut b = "call 555-123-4567 or 555 123 4567".to_string();
        r.redact_text(&mut b);
        assert_eq!(b, "call [PHONE_1] or [PHONE_1]");
        let report = r.finish();
        assert_eq!(report.counts["EMAIL"], 3);
        assert_eq!(report.counts["PHONE"], 2);
        assert_eq!(report.total, 5);
        let map = report.mapping.unwrap();
        assert!(map.contains(&("a@x.com".into(), "[EMAIL_1]".into())));
        assert_eq!(map.len(), 5);

        let opts = RedactionOptions {
            replacement: Replacement::Fixed("***".into()),
            ..Default::default()
        };
        let d = detector(&opts);
        let mut s = "a@x.com".to_string();
        Redactor::new(&opts, &d).redact_text(&mut s);
        assert_eq!(s, "***");
    }

    #[test]
    fn kinds_filter_custom_patterns_deny_and_allow_terms() {
        let opts = RedactionOptions {
            kinds: vec![PiiKind::Phone],
            ..Default::default()
        };
        let d = detector(&opts);
        let mut s = "a@x.com 555-123-4567".to_string();
        Redactor::new(&opts, &d).redact_text(&mut s);
        assert_eq!(s, "a@x.com [PHONE]");

        let opts = RedactionOptions {
            custom_patterns: vec![CustomPattern::parse("case_id=CASE-\\d{4}").unwrap()],
            deny_terms: vec!["Project Falcon".into()],
            allow_terms: vec!["support@example.com".into()],
            ..Default::default()
        };
        let d = detector(&opts);
        let mut s = "CASE-0042 by project falcon: support@example.com / x@example.com".to_string();
        Redactor::new(&opts, &d).redact_text(&mut s);
        assert_eq!(s, "[CASE_ID] by [REDACTED]: support@example.com / [EMAIL]");
        assert!(PatternDetector::new(&RedactionOptions {
            custom_patterns: vec![CustomPattern {
                name: "bad".into(),
                regex: "(".into()
            }],
            ..Default::default()
        })
        .is_err());
    }

    /// #694: the nine misfires reported on real documents stay as they are
    /// with the five pattern kinds selected; the control phone goes.
    #[test]
    fn postal_codes_citations_record_ids_and_part_numbers_are_not_pii() {
        let opts = RedactionOptions {
            kinds: vec![
                PiiKind::Phone,
                PiiKind::CreditCard,
                PiiKind::Iban,
                PiiKind::NationalId,
                PiiKind::IpAddress,
            ],
            ..Default::default()
        };
        let d = detector(&opts);
        let untouched = [
            "Ship to: Example Corp, 100 Main Street, Springfield, IL 62704-1234.",
            "Send comments to Department 55A, Mail Station 1234 5600 Elm Road.",
            "Leave accrual is defined in rule 412-18-205 of the state code.",
            "Leave accrual is defined in rule 412-18-0150 of the state code.",
            "The input file contains rows such as 60601 0001 12500 00350 and 60602 0002 9800 00120.",
            "Replace the bracket with part number TR482913 B before assembly.",
            "/XY123456 A configuration file is required for every job.",
            "Position created in 2019 95753514 for the regional office.",
            "Employee 88061052 76665755 Clerk Grade 4",
        ];
        for text in untouched {
            let mut s = text.to_string();
            Redactor::new(&opts, &d).redact_text(&mut s);
            assert_eq!(s, text);
        }
        let mut s = "Call the help desk at +1 555 010 0199 for assistance.".to_string();
        Redactor::new(&opts, &d).redact_text(&mut s);
        assert_eq!(s, "Call the help desk at [PHONE] for assistance.");
    }

    /// … while the real values keep being found, in every shape they are
    /// written in.
    #[test]
    fn real_values_are_still_redacted() {
        assert_eq!(kinds("+1 555 010 0199"), vec![PiiKind::Phone]);
        assert_eq!(kinds("(555) 010-0199"), vec![PiiKind::Phone]);
        assert_eq!(kinds("555-010-0199"), vec![PiiKind::Phone]);
        assert_eq!(kinds("Tel: 020 7946 0958"), vec![PiiKind::Phone]);
        assert_eq!(kinds("020 7946 0958"), vec![PiiKind::Phone]);
        assert_eq!(kinds("mobile 0151 496 0000"), vec![PiiKind::Phone]);
        assert_eq!(kinds("phone: 1234 5600"), vec![PiiKind::Phone]);
        assert_eq!(kinds("Mail Station 1234 5600"), vec![]);
        assert_eq!(kinds("card 4111 1111 1111 1111"), vec![PiiKind::CreditCard]);
        assert_eq!(kinds("amex 378282246310005"), vec![PiiKind::CreditCard]);
        assert_eq!(kinds("amex 3782 822463 10005"), vec![PiiKind::CreditCard]);
        assert_eq!(kinds("mc 5555 5555 5555 4444"), vec![PiiKind::CreditCard]);
        assert_eq!(kinds("mc 2223 0031 2200 3222"), vec![PiiKind::CreditCard]);
        // Luhn-valid, no brand: an order number.
        assert_eq!(kinds("order 8806105276665755"), vec![]);
        // A card number split as a phone is not a card.
        assert_eq!(
            kinds(
                "4111 1111 1111 1111"
                    .replace(' ', "-")
                    .replace("1111-1111-1111", "11111111-1111")
                    .as_str()
            ),
            vec![]
        );
        assert_eq!(kinds("NINO AB123456C"), vec![PiiKind::NationalId]);
        assert_eq!(kinds("NINO AB 12 34 56 C"), vec![PiiKind::NationalId]);
        assert_eq!(kinds("NINO AB123456 C"), vec![]);
        assert_eq!(kinds("NINO AB 123456C"), vec![]);
        assert_eq!(kinds("SSN 123-45-6789"), vec![PiiKind::NationalId]);
        assert_eq!(kinds("ID 412-18-0150 on file"), vec![PiiKind::NationalId]);
        assert_eq!(kinds("see § 412-18-0150 and section 412-18-0151"), vec![]);
        assert_eq!(
            kinds("IBAN GB82 WEST 1234 5698 7654 32"),
            vec![PiiKind::Iban]
        );
        // Aadhaar: consistent separators; a year first group is a date.
        let base = "22345678901";
        let ok = (0..10)
            .map(|d| format!("{base}{d}"))
            .find(|s| verhoeff_valid(s))
            .unwrap();
        let spaced = format!("{} {} {}", &ok[..4], &ok[4..8], &ok[8..]);
        assert_eq!(
            kinds(&format!("Aadhaar {spaced}")),
            vec![PiiKind::NationalId]
        );
        assert_eq!(kinds(&format!("Aadhaar {} {}", &ok[..4], &ok[4..])), vec![]);
        let year_base = "20199575351";
        let year_ok = (0..10)
            .map(|d| format!("{year_base}{d}"))
            .find(|s| verhoeff_valid(s))
            .unwrap();
        assert_eq!(
            kinds(&format!(
                "in {} {} {}",
                &year_ok[..4],
                &year_ok[4..8],
                &year_ok[8..]
            )),
            vec![]
        );
        assert_eq!(
            kinds(&format!("Aadhaar {year_ok}")),
            vec![PiiKind::NationalId]
        );
    }

    /// No match starts or ends inside a longer alphanumeric token.
    #[test]
    fn matches_are_whole_tokens() {
        assert_eq!(redact("IL 62704-1234 stays"), "IL 62704-1234 stays");
        assert_eq!(
            redact("row 60602 0002 9800 00120 stays"),
            "row 60602 0002 9800 00120 stays"
        );
        assert_eq!(redact("id x555-010-0199"), "id x555-010-0199");
        assert_eq!(redact("id 555-010-0199x"), "id 555-010-0199x");
        assert_eq!(redact("ref 4111111111111111a"), "ref 4111111111111111a");
        assert_eq!(redact("ref AB123456CD"), "ref AB123456CD");
        assert_eq!(redact("call 555-010-0199."), "call [PHONE].");
    }

    /// Allow patterns: a document's own number shapes pass every detector.
    #[test]
    fn allow_patterns_exempt_matching_spans() {
        let opts = RedactionOptions {
            allow_patterns: vec![r"\+1 555 010 \d{4}".into(), r"[a-z]+@example\.com".into()],
            ..Default::default()
        };
        let d = detector(&opts);
        let mut s =
            "+1 555 010 0199, +1 555 010 0200, +1 555 011 0200, bob@example.com, bob@example.org"
                .to_string();
        Redactor::new(&opts, &d).redact_text(&mut s);
        assert_eq!(
            s,
            "+1 555 010 0199, +1 555 010 0200, [PHONE], bob@example.com, [EMAIL]"
        );
        assert!(PatternDetector::new(&RedactionOptions {
            allow_patterns: vec!["(".into()],
            ..Default::default()
        })
        .is_err());
    }

    /// A transcript (#694): the `[time: …]` prefix is never a phone number
    /// and the cue's words are counted once; a spoken phone number is
    /// redacted in both `cue` and the paragraph, with one pseudonym.
    #[test]
    fn transcript_timings_are_not_phones_and_cues_count_once() {
        fn track(start: f64, end: f64, cue: &str) -> Node {
            Node::Track {
                track: crate::tree::TreeTrack {
                    start_time: start,
                    end_time: end,
                    identifier: None,
                    voice: None,
                },
                cue: cue.to_string(),
                inner: Box::new(Node::Paragraph {
                    text: format!("[time: {start}-{end}] {cue}"),
                }),
            }
        }
        let opts = RedactionOptions {
            replacement: Replacement::Pseudonym,
            ..Default::default()
        };
        let d = detector(&opts);
        let mut doc = DoclingDocument::new("t");
        doc.nodes = vec![
            track(1234.5, 1240.8, "Shakespeare on scenery by Oscar Wilde."),
            track(
                1240.8,
                1251.25,
                "Call me on 555-010-0199 or 555-010-0199 again.",
            ),
        ];
        let report = doc.redact(&opts, &d);
        assert_eq!(report.counts.get("PHONE"), Some(&2));
        assert_eq!(report.total, 2);
        let texts: Vec<(String, String)> = doc
            .nodes
            .iter()
            .map(|n| match n {
                Node::Track { cue, inner, .. } => match inner.as_ref() {
                    Node::Paragraph { text, .. } => (cue.clone(), text.clone()),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            texts[0],
            (
                "Shakespeare on scenery by Oscar Wilde.".to_string(),
                "[time: 1234.5-1240.8] Shakespeare on scenery by Oscar Wilde.".to_string()
            )
        );
        assert_eq!(
            texts[1],
            (
                "Call me on [PHONE_1] or [PHONE_1] again.".to_string(),
                "[time: 1240.8-1251.25] Call me on [PHONE_1] or [PHONE_1] again.".to_string()
            )
        );
        // The timing alone, as the ASR writes long transcripts, is no phone.
        assert_eq!(kinds("[time: 1234.5-1240.8]"), vec![]);
    }

    #[test]
    fn overlapping_spans_keep_the_longest() {
        // A Luhn-valid 16-digit number is a card, not three phone groups.
        assert_eq!(kinds("4111 1111 1111 1111"), vec![PiiKind::CreditCard]);
        // Custom pattern covering an e-mail wins by length.
        let opts = RedactionOptions {
            custom_patterns: vec![CustomPattern::parse("contact=Contact: \\S+").unwrap()],
            ..Default::default()
        };
        let d = detector(&opts);
        let mut s = "Contact: a@x.com now".to_string();
        Redactor::new(&opts, &d).redact_text(&mut s);
        assert_eq!(s, "[CONTACT] now");
    }

    #[test]
    fn inline_runs_are_matched_on_the_joined_text() {
        let opts = RedactionOptions::default();
        let d = detector(&opts);
        let mut runs = vec![
            InlineRun {
                text: "Mail john".into(),
                bold: true,
                ..Default::default()
            },
            InlineRun {
                text: "@example".into(),
                ..Default::default()
            },
            InlineRun {
                text: ".com today, or 555-".into(),
                italic: true,
                ..Default::default()
            },
            InlineRun {
                text: "123-4567.".into(),
                ..Default::default()
            },
        ];
        Redactor::new(&opts, &d).redact_runs(&mut runs);
        let texts: Vec<&str> = runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["Mail [EMAIL]", "", " today, or [PHONE]", "."]);
        assert!(runs[0].bold && runs[2].italic);
    }

    #[test]
    fn the_document_walk_reaches_every_string() {
        use crate::tree::TreeKind;
        let mut doc = DoclingDocument::new("file.docx");
        doc.push(Node::Heading {
            level: 1,
            text: "Re: a@x.com".into(),
        });
        doc.push(Node::Group {
            label: "section".into(),
            name: Some("b@x.com".into()),
            layer: None,
            children: vec![Node::Furniture {
                layer: crate::ContentLayer::Furniture,
                inner: Box::new(Node::Paragraph {
                    text: "c@x.com".into(),
                }),
            }],
        });
        let mut table = Table {
            rows: vec![vec!["d@x.com".into()]],
            ..Default::default()
        };
        table.cell_blocks = Some(vec![vec![vec![Node::Paragraph {
            text: "e@x.com".into(),
        }]]]);
        doc.push(Node::Table(table));
        doc.push(Node::Picture {
            caption: Some("f@x.com".into()),
            caption_href: Some("mailto:f@x.com".into()),
            image: Some(crate::PictureImage {
                mimetype: "image/png".into(),
                width: 1,
                height: 1,
                data: vec![1],
                dpi: 72,
                display_size: None,
            }),
            classification: None,
            description: Some(crate::PictureDescription {
                text: "ocr j@x.com".into(),
                provenance: "ppocr".into(),
            }),
            caption_parent: Default::default(),
            caption_location: None,
        });
        doc.push(Node::Formula {
            latex: "x@y.com".into(),
            orig: "g@x.com".into(),
            location: None,
        });
        doc.links.push(("h@x.com".into(), "mailto:h@x.com".into()));
        let mut tree = ItemTree::default();
        tree.add(
            None,
            None,
            TreeKind::Text {
                label: "text".into(),
                text: "i@x.com".into(),
                orig: Some("i@x.com".into()),
                formatting: None,
                hyperlink: Some("mailto:i@x.com".into()),
                level: None,
                list: None,
            },
        );
        doc.tree = Some(tree);
        doc.page_images.insert(
            1,
            crate::PictureImage {
                mimetype: "image/png".into(),
                width: 1,
                height: 1,
                data: vec![1],
                dpi: 72,
                display_size: None,
            },
        );

        let opts = RedactionOptions::default();
        let d = detector(&opts);
        let report = doc.redact(&opts, &d);
        // The tree's three copies are redacted but not counted again.
        assert_eq!(report.counts["EMAIL"], 11);
        let json = doc.export_to_json();
        let md = doc.export_to_markdown();
        let dl = doc.export_to_doclang();
        for out in [&json, &md, &dl] {
            assert!(!out.contains("@x.com"), "{out}");
        }
        assert_eq!(doc.name, "file.docx", "the file name is not touched");
        assert!(doc.page_images.is_empty());
        assert!(matches!(&doc.nodes[3], Node::Picture { image: None, .. }));
        // The LaTeX stays: it is a rendering, not extracted text.
        assert!(matches!(&doc.nodes[4], Node::Formula { latex, .. } if latex == "x@y.com"));

        // `Keep` leaves the images.
        let mut doc2 = DoclingDocument::new("t");
        doc2.page_images.insert(
            1,
            crate::PictureImage {
                mimetype: "image/png".into(),
                width: 1,
                height: 1,
                data: vec![1],
                dpi: 72,
                display_size: None,
            },
        );
        let opts = RedactionOptions {
            images: ImageRedaction::Keep,
            ..Default::default()
        };
        let d = detector(&opts);
        doc2.redact(&opts, &d);
        assert_eq!(doc2.page_images.len(), 1);
    }

    #[test]
    fn wire_spellings_parse() {
        assert_eq!(PiiKind::parse("Credit-Card"), Some(PiiKind::CreditCard));
        assert_eq!(PiiKind::parse("ip"), Some(PiiKind::IpAddress));
        assert_eq!(PiiKind::parse("nope"), None);
        assert_eq!(
            Replacement::parse("fixed:XXX"),
            Some(Replacement::Fixed("XXX".into()))
        );
        assert_eq!(
            Replacement::parse("Pseudonym"),
            Some(Replacement::Pseudonym)
        );
        assert_eq!(
            ImageRedaction::parse("box-out"),
            Some(ImageRedaction::BoxOut)
        );
        let p = CustomPattern::parse("case_id=CASE-\\d+").unwrap();
        assert_eq!(
            (p.name.as_str(), p.regex.as_str()),
            ("case_id", "CASE-\\d+")
        );
        assert_eq!(CustomPattern::parse("(?i)x=y").unwrap().name, "custom");
    }
}
