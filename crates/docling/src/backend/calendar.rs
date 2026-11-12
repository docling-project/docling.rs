//! iCalendar (RFC 5545, `.ics` / `.ical` / `.icalendar` / `.ifb`,
//! `text/calendar`) — a port of docling's `CalendarDocumentBackend`
//! (docling#4880): events, tasks, journal entries and free/busy blocks.
//!
//! Each `VEVENT` / `VTODO` / `VJOURNAL` / `VFREEBUSY` becomes a heading (its
//! `SUMMARY`), its fields as `Field: value` paragraphs (start/end with the
//! TZID as written, recurrence in words, location, organizer, status,
//! priority, conference link, …), its `DESCRIPTION` paragraphs, then the
//! `Attendees` / `Reminders` / `Attachments` lists under their own headings.
//! A calendar's `X-WR-CALNAME` is the title and a non-`PUBLISH` `METHOD` a
//! `Message type:` line. Inline attachment payloads are never decoded.
//!
//! Folded lines are joined on the *bytes* before decoding — RFC 5545 lets a
//! fold fall inside a multi-byte UTF-8 character — unless the input is
//! UTF-16/32 (a wide BOM, or a requested encoding that is not ASCII
//! compatible), which is decoded first and unfolded as text. The converter's
//! `encoding` option applies, as docling's `CalendarBackendOptions.encoding`.
//!
//! The JSON export carries docling's item tree: `title`, `section_header`
//! items (entries at level 1, their lists' headings at level 2), `text`
//! fields, and each list a `list` group named after its heading in lower
//! case (`attendees`, `reminders`, `free/busy periods`).

use std::borrow::Cow;

use docling_core::tree::{ItemTree, TreeKind};
use docling_core::{DoclingDocument, Node};

use crate::backend::markdown::escape_underscores;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

#[derive(Debug, Default, Clone, Copy)]
pub struct CalendarBackend;

impl DeclarativeBackend for CalendarBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = read_calendar_text(&source.bytes, source.encoding.as_deref())?;
        let components = parse_components(&text);
        if !components.iter().any(|c| {
            matches!(
                c.name.as_str(),
                "VCALENDAR" | "VEVENT" | "VTODO" | "VJOURNAL" | "VFREEBUSY"
            )
        }) {
            return Err(ConversionError::Parse(
                "The document holds no calendar component (BEGIN:VCALENDAR or \
                 BEGIN:VEVENT, VTODO, VJOURNAL, VFREEBUSY)."
                    .into(),
            ));
        }
        let mut writer = Writer {
            doc: DoclingDocument::new(&source.name),
            tree: ItemTree::default(),
            level: 1,
        };
        for component in &components {
            if component.name == "VCALENDAR" {
                writer.add_calendar(component);
            } else if RENDERED_ENTRIES.contains(&component.name.as_str()) {
                writer.add_entry(component);
            }
        }
        let Writer { mut doc, tree, .. } = writer;
        doc.tree = Some(tree);
        Ok(doc)
    }
}

/// Whether `bytes` open with `BEGIN:VCALENDAR` (case-insensitive, after a
/// byte-order mark and blank space) — docling's `_detect_calendar`, which
/// reads UTF-16 behind its BOM too. The sniff for a nameless input.
pub(crate) fn looks_like_calendar(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let text: Cow<'_, str> = if head.starts_with(&[0xff, 0xfe]) || head.starts_with(&[0xfe, 0xff]) {
        encoding_rs::UTF_16LE.decode(head).0
    } else {
        String::from_utf8_lossy(head)
    };
    let text = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let Some(rest) = text
        .get(..15)
        .filter(|h| h.eq_ignore_ascii_case("BEGIN:VCALENDAR"))
        .map(|_| &text[15..])
    else {
        return false;
    };
    // `\b`: no word character may follow.
    !rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------- reading

/// Decode the file with its folded lines joined (see the module docs).
fn read_calendar_text(raw: &[u8], encoding: Option<&str>) -> Result<String, ConversionError> {
    const WIDE_BOMS: [&[u8]; 4] = [
        &[0xff, 0xfe, 0x00, 0x00],
        &[0x00, 0x00, 0xfe, 0xff],
        &[0xff, 0xfe],
        &[0xfe, 0xff],
    ];
    let mut ascii_compatible = !WIDE_BOMS.iter().any(|b| raw.starts_with(b));
    if let Some(label) = encoding {
        // An unknown label is reported by the decoder below.
        if let Some(enc) = crate::source::lookup_encoding(label) {
            ascii_compatible = enc.is_ascii_compatible();
        }
    }
    let decode = |bytes: &[u8]| -> Result<String, ConversionError> {
        Ok(match encoding {
            Some(label) => crate::source::decode_text_as(bytes, label)?.into_owned(),
            None => crate::source::decode_text(bytes)?.into_owned(),
        })
    };
    if ascii_compatible {
        decode(&unfold_bytes(raw))
    } else {
        Ok(unfold_text(&decode(raw)?))
    }
}

/// Remove every line break that starts a fold: `\r\n`, `\n` or `\r`
/// followed by a space or a tab.
fn unfold_bytes(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let fold = match raw[i] {
            b'\r'
                if raw.get(i + 1) == Some(&b'\n')
                    && matches!(raw.get(i + 2), Some(b' ' | b'\t')) =>
            {
                3
            }
            b'\r' | b'\n' if matches!(raw.get(i + 1), Some(b' ' | b'\t')) => 2,
            _ => 0,
        };
        if fold > 0 {
            i += fold;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    out
}

/// [`unfold_bytes`] on decoded text.
fn unfold_text(text: &str) -> String {
    String::from_utf8(unfold_bytes(text.as_bytes())).expect("only ASCII bytes are removed")
}

// ---------------------------------------------------------------- parsing

/// A content line: its upper-case name, its parameters (several values
/// joined by commas) and its values — TEXT already unescaped, list
/// properties one entry per item.
#[derive(Debug, Clone, Default)]
struct Property {
    name: String,
    params: Vec<(String, String)>,
    values: Vec<String>,
}

impl Property {
    fn value(&self) -> &str {
        self.values.first().map_or("", String::as_str)
    }

    fn param(&self, name: &str) -> Option<&str> {
        // A repeated parameter keeps its last value, as a dict does.
        self.params
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn param_or_empty(&self, name: &str) -> &str {
        self.param(name).unwrap_or("")
    }
}

/// A BEGIN/END block with its properties and nested components.
#[derive(Debug, Clone, Default)]
struct Component {
    name: String,
    properties: Vec<Property>,
    components: Vec<Component>,
}

impl Component {
    fn get(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == name)
    }

    fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.properties.iter().filter(move |p| p.name == name)
    }

    fn text(&self, name: &str) -> &str {
        self.get(name).map_or("", Property::value)
    }
}

/// Properties whose value type is TEXT, so backslash escapes apply.
const TEXT_PROPERTIES: &[&str] = &[
    "ACTION",
    "CATEGORIES",
    "CLASS",
    "COMMENT",
    "CONTACT",
    "DESCRIPTION",
    "LOCATION",
    "METHOD",
    "NAME",
    "RELATED-TO",
    "RESOURCES",
    "STATUS",
    "SUMMARY",
    "TRANSP",
    "TZID",
    "TZNAME",
    "UID",
    "X-WR-CALDESC",
    "X-WR-CALNAME",
    "X-WR-TIMEZONE",
];
/// Properties that hold a comma-separated list of values.
const LIST_PROPERTIES: &[&str] = &["CATEGORIES", "EXDATE", "FREEBUSY", "RDATE", "RESOURCES"];
/// Components that hold calendar entries; one never nests in another.
const ENTRY_COMPONENTS: &[&str] = &[
    "VAVAILABILITY",
    "VEVENT",
    "VFREEBUSY",
    "VJOURNAL",
    "VTIMEZONE",
    "VTODO",
];
const RFC9073_COMPONENTS: &[&str] = &["PARTICIPANT", "VLOCATION", "VRESOURCE"];

/// The components `parent` may hold; `None` for a component the table does
/// not know.
fn children_of(parent: &str) -> Option<&'static [&'static str]> {
    Some(match parent {
        "VCALENDAR" => ENTRY_COMPONENTS,
        "VEVENT" | "VTODO" => &["VALARM", "PARTICIPANT", "VLOCATION", "VRESOURCE"],
        "VJOURNAL" | "VFREEBUSY" => RFC9073_COMPONENTS,
        "VTIMEZONE" => &["DAYLIGHT", "STANDARD"],
        "VAVAILABILITY" => &["AVAILABLE"],
        "PARTICIPANT" => &["VLOCATION", "VRESOURCE"],
        "VALARM" | "VLOCATION" | "VRESOURCE" | "STANDARD" | "DAYLIGHT" | "AVAILABLE" => &[],
        _ => return None,
    })
}

fn can_contain(parent: &str, child: &str) -> bool {
    if child == "VCALENDAR" {
        return false;
    }
    match children_of(parent) {
        None => !ENTRY_COMPONENTS.contains(&child),
        Some(allowed) => allowed.contains(&child) || children_of(child).is_none(),
    }
}

/// Build the component tree from unfolded text. A missing END is recovered
/// from the containment rules: a BEGIN the open component cannot hold closes
/// it first, so one bad block does not swallow the entries after it. A stray
/// END is ignored.
fn parse_components(text: &str) -> Vec<Component> {
    // The open components, innermost last; each closed one is attached to
    // its parent (or the roots) as it is popped.
    let mut roots: Vec<Component> = Vec::new();
    let mut stack: Vec<Component> = Vec::new();
    fn close(stack: &mut Vec<Component>, roots: &mut Vec<Component>) {
        let done = stack.pop().expect("non-empty stack");
        match stack.last_mut() {
            Some(parent) => parent.components.push(done),
            None => roots.push(done),
        }
    }
    // Lines break on CRLF, CR or LF only — not on U+2028 inside a value.
    for line in text.split("\r\n").flat_map(|l| l.split(['\r', '\n'])) {
        let Some(prop) = parse_line(line) else {
            continue;
        };
        if prop.name == "BEGIN" || prop.name == "END" {
            let component = prop.value().trim().to_uppercase();
            if component.is_empty() {
                continue;
            }
            if prop.name == "BEGIN" {
                while stack
                    .last()
                    .is_some_and(|open| !can_contain(&open.name, &component))
                {
                    close(&mut stack, &mut roots);
                }
                stack.push(Component {
                    name: component,
                    ..Default::default()
                });
            } else if stack.iter().any(|c| c.name == component) {
                loop {
                    let name = stack.last().map(|c| c.name.clone());
                    close(&mut stack, &mut roots);
                    if name.as_deref() == Some(component.as_str()) {
                        break;
                    }
                }
            }
            continue;
        }
        if let Some(open) = stack.last_mut() {
            open.properties.push(prop);
        }
    }
    while !stack.is_empty() {
        close(&mut stack, &mut roots);
    }
    roots
}

/// The length of the `[A-Za-z0-9_-]+` name at the start of `s`.
fn name_len(s: &str) -> usize {
    s.bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        .count()
}

/// RFC 6868 caret escapes in a parameter value: `^n`, `^^`, `^'`.
fn uncaret(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '^' {
            if let Some(&next) = chars.peek() {
                let mapped = match next {
                    'n' => Some('\n'),
                    '^' => Some('^'),
                    '\'' => Some('"'),
                    _ => None,
                };
                if let Some(m) = mapped {
                    out.push(m);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

fn parse_line(line: &str) -> Option<Property> {
    let n = name_len(line);
    if n == 0 {
        return None;
    }
    let name = line[..n].to_ascii_uppercase();
    let mut pos = n;
    let b = line.as_bytes();
    let mut params: Vec<(String, String)> = Vec::new();
    while pos < b.len() && b[pos] == b';' {
        let pn = name_len(&line[pos + 1..]);
        if pn == 0 {
            return None;
        }
        let param_name = line[pos + 1..pos + 1 + pn].to_ascii_uppercase();
        pos += 1 + pn;
        if pos >= b.len() || b[pos] != b'=' {
            // A vCalendar 1.0 parameter without a value (`;QUOTED-PRINTABLE`).
            params.push((param_name, String::new()));
            continue;
        }
        let mut values: Vec<&str> = Vec::new();
        pos += 1;
        loop {
            if pos < b.len() && b[pos] == b'"' {
                let end = line[pos + 1..].find('"')? + pos + 1;
                values.push(&line[pos + 1..end]);
                pos = end + 1;
            } else {
                let end = line[pos..]
                    .find(['"', ';', ':', ','])
                    .map_or(line.len(), |e| e + pos);
                values.push(&line[pos..end]);
                pos = end;
            }
            if pos < b.len() && b[pos] == b',' {
                pos += 1;
                continue;
            }
            break;
        }
        let joined = values
            .iter()
            .map(|v| uncaret(v))
            .collect::<Vec<_>>()
            .join(",");
        params.push((param_name, joined));
    }
    if pos >= b.len() || b[pos] != b':' {
        return None;
    }
    let raw = &line[pos + 1..];
    let value_type = params
        .iter()
        .rev()
        .find(|(n, _)| n == "VALUE")
        .map(|(_, v)| v.to_uppercase());
    let is_text = TEXT_PROPERTIES.contains(&name.as_str()) || value_type.as_deref() == Some("TEXT");
    let values = if LIST_PROPERTIES.contains(&name.as_str()) {
        split_list(raw, is_text, true)
    } else if is_text {
        split_list(raw, true, false)
    } else {
        vec![raw.to_string()]
    };
    Some(Property {
        name,
        params,
        values,
    })
}

/// Split on commas that are not escaped, unescaping TEXT on the way. An
/// escape the grammar does not define (`\:`) is kept as it is.
fn split_list(value: &str, unescape: bool, split: bool) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if unescape && c == '\\' {
            if let Some(&escaped) = chars.peek() {
                chars.next();
                match escaped {
                    'n' | 'N' => current.push('\n'),
                    '\\' | ';' | ',' => current.push(escaped),
                    _ => {
                        current.push(c);
                        current.push(escaped);
                    }
                }
                continue;
            }
        }
        if split && c == ',' {
            items.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    items.push(current);
    if split {
        items
            .into_iter()
            .map(|s| py_strip(&s).to_string())
            .collect()
    } else {
        items
    }
}

/// Python's `str.strip()`: Unicode whitespace on both ends.
fn py_strip(s: &str) -> &str {
    s.trim()
}

// ---------------------------------------------------------------- values

/// A civil date (proleptic Gregorian), as Python's `date`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Date {
    year: i32,
    month: u32,
    day: u32,
}

impl Date {
    fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || day == 0 {
            return None;
        }
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let days_in_month = match month {
            2 if leap => 29,
            2 => 28,
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        (day <= days_in_month).then_some(Self { year, month, day })
    }

    /// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    fn to_days(self) -> i64 {
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn from_days(z: i64) -> Option<Self> {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self::new(i32::try_from(year).ok()?, month, day)
    }

    /// `date + timedelta(days=n)`; `None` past Python's `date` range.
    fn plus_days(self, n: i64) -> Option<Self> {
        Self::from_days(self.to_days().checked_add(n)?)
    }

    fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// A DATE or DATE-TIME value with the time-zone label it was written with:
/// `"UTC"`, the TZID as written, or none (a floating time or a date).
#[derive(Debug, Clone)]
struct CalendarTime {
    date: Date,
    /// `(hour, minute, second)` for a DATE-TIME; `None` for a DATE.
    time: Option<(u32, u32, u32)>,
    tz: Option<String>,
}

impl CalendarTime {
    fn all_day(&self) -> bool {
        self.time.is_none()
    }
}

/// Read a DATE or DATE-TIME value. Its shape decides between the two, as
/// producers often leave out `VALUE=DATE`; a trailing `Z` wins over a TZID.
fn parse_time(raw: &str, tzid: Option<&str>) -> Option<CalendarTime> {
    let re = cached_regex!(
        r"(?i)^([0-9]{4})-?([0-9]{2})-?([0-9]{2})(?:T([0-9]{2}):?([0-9]{2})(?::?([0-9]{2}))?(?:\.[0-9]+)?(Z)?)?$"
    );
    let caps = re.captures(py_strip(raw))?;
    let num = |i: usize| caps.get(i).map(|m| m.as_str().parse::<u32>().unwrap_or(0));
    let date = Date::new(num(1)? as i32, num(2)?, num(3)?)?;
    let Some(hour) = num(4) else {
        return Some(CalendarTime {
            date,
            time: None,
            tz: None,
        });
    };
    let minute = num(5)?;
    let second = num(6).unwrap_or(0);
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let tz = if caps.get(7).is_some() {
        Some("UTC".to_string())
    } else {
        tzid.filter(|t| !t.is_empty()).map(str::to_string)
    };
    Some(CalendarTime {
        date,
        time: Some((hour, minute, second)),
        tz,
    })
}

fn property_time(prop: &Property) -> Option<CalendarTime> {
    parse_time(prop.value(), prop.param("TZID"))
}

/// `2026-10-20 09:00 UTC`: seconds only when not zero, the zone label when
/// there is one.
fn format_time(time: &CalendarTime) -> String {
    let mut text = time.date.iso();
    if let Some((h, m, s)) = time.time {
        text.push_str(&format!(" {h:02}:{m:02}"));
        if s != 0 {
            text.push_str(&format!(":{s:02}"));
        }
    }
    match &time.tz {
        Some(tz) => format!("{text} {tz}"),
        None => text,
    }
}

fn format_property_time(prop: &Property) -> String {
    property_time(prop).map_or_else(|| prop.value().to_string(), |t| format_time(&t))
}

/// A DURATION (`PT1H30M`, `P1W`, `-PT15M`) in seconds, negative for a
/// leading `-`; `None` when the text is not one.
fn parse_duration(raw: &str) -> Option<i64> {
    let re = cached_regex!(
        r"(?i)^([+-])?P(?:([0-9]+)W)?(?:([0-9]+)D)?(?:T(?:([0-9]+)H)?(?:([0-9]+)M)?(?:([0-9]+)S)?)?$"
    );
    let caps = re.captures(py_strip(raw))?;
    if (2..=6).all(|i| caps.get(i).is_none()) {
        return None;
    }
    let n = |i: usize| -> Option<i64> {
        caps.get(i)
            .map_or(Some(0), |m| m.as_str().parse::<i64>().ok())
    };
    let total = n(2)?
        .checked_mul(7 * 86_400)?
        .checked_add(n(3)?.checked_mul(86_400)?)?
        .checked_add(n(4)?.checked_mul(3_600)?)?
        .checked_add(n(5)?.checked_mul(60)?)?
        .checked_add(n(6)?)?;
    Some(if caps.get(1).is_some_and(|m| m.as_str() == "-") {
        -total
    } else {
        total
    })
}

fn plural(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("{count} {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

/// `1 hour 30 minutes`; the sign is ignored, zero is empty.
fn format_duration(seconds: i64) -> String {
    let total = seconds.unsigned_abs() as i64;
    let (days, rest) = (total / 86_400, total % 86_400);
    let (hours, rest2) = (rest / 3_600, rest % 3_600);
    let (minutes, secs) = (rest2 / 60, rest2 % 60);
    let mut parts: Vec<String> = Vec::new();
    if days != 0 && days % 7 == 0 && rest == 0 {
        parts.push(plural(days / 7, "week"));
    } else if days != 0 {
        parts.push(plural(days, "day"));
    }
    for (count, unit) in [(hours, "hour"), (minutes, "minute"), (secs, "second")] {
        if count != 0 {
            parts.push(plural(count, unit));
        }
    }
    parts.join(" ")
}

/// `A, B and C`.
fn join_words(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn ordinal(number: i64) -> String {
    let suffix = if matches!(number % 100, 11..=13) {
        "th"
    } else {
        match number % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("{number}{suffix}")
}

fn ordinal_word(number: i64) -> String {
    match number {
        -1 => "last".into(),
        n if n < 0 => format!("{}-to-last", ordinal_word(-n)),
        1 => "first".into(),
        2 => "second".into(),
        3 => "third".into(),
        4 => "fourth".into(),
        5 => "fifth".into(),
        n => ordinal(n),
    }
}

fn weekday_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "MO" => "Monday",
        "TU" => "Tuesday",
        "WE" => "Wednesday",
        "TH" => "Thursday",
        "FR" => "Friday",
        "SA" => "Saturday",
        "SU" => "Sunday",
        _ => return None,
    })
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Python's `int(token)`: surrounding whitespace and a sign allowed.
fn py_int(token: &str) -> Option<i64> {
    let t = py_strip(token).replace('_', "");
    if t.is_empty() || py_strip(token).starts_with('_') || py_strip(token).ends_with('_') {
        return None;
    }
    t.parse::<i64>().ok()
}

fn describe_byday(value: &str) -> Option<String> {
    let re = cached_regex!(r"(?i)^([+-]?[0-9]{1,2})?(MO|TU|WE|TH|FR|SA|SU)$");
    let mut days = Vec::new();
    for token in value.split(',') {
        let caps = re.captures(py_strip(token))?;
        let name = weekday_name(&caps[2].to_ascii_uppercase())?;
        match caps.get(1) {
            None => days.push(name.to_string()),
            Some(pos) => {
                let n: i64 = pos.as_str().parse().ok()?;
                if n == 0 {
                    return None;
                }
                days.push(format!("the {} {name}", ordinal_word(n)));
            }
        }
    }
    Some(join_words(&days))
}

fn describe_bymonthday(value: &str) -> Option<String> {
    let mut days = Vec::new();
    for token in value.split(',') {
        let n = py_int(token)?;
        if n == 0 || !(-31..=31).contains(&n) {
            return None;
        }
        days.push(match n {
            n if n > 0 => ordinal(n),
            -1 => "last day".into(),
            n => format!("{}-to-last day", ordinal(-n)),
        });
    }
    Some(format!("the {}", join_words(&days)))
}

fn describe_bymonth(value: &str) -> Option<String> {
    let mut months = Vec::new();
    for token in value.split(',') {
        let n = py_int(token)?;
        if !(1..=12).contains(&n) {
            return None;
        }
        months.push(MONTHS[(n - 1) as usize].to_string());
    }
    Some(join_words(&months))
}

/// A recurrence rule in words (`Weekly on Monday and Wednesday`), or `None`
/// to keep the raw rule — also when it has a part this does not describe,
/// since dropping BYSETPOS or BYHOUR would state a different schedule.
fn describe_rrule(raw: &str) -> Option<String> {
    const PARTS: &[&str] = &[
        "FREQ",
        "INTERVAL",
        "COUNT",
        "UNTIL",
        "BYDAY",
        "BYMONTHDAY",
        "BYMONTH",
        "WKST",
    ];
    let mut parts: Vec<(String, String)> = Vec::new();
    for item in raw.split(';') {
        let (key, value) = item.split_once('=')?;
        if value.is_empty() {
            return None;
        }
        let key = py_strip(key).to_uppercase();
        let value = py_strip(value).to_string();
        // A repeated key keeps its last value, as a dict does.
        match parts.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => parts.push((key, value)),
        }
    }
    if !parts.iter().all(|(k, _)| PARTS.contains(&k.as_str())) {
        return None;
    }
    let get = |k: &str| parts.iter().find(|(p, _)| p == k).map(|(_, v)| v.as_str());
    let freq = get("FREQ").unwrap_or("").to_uppercase();
    let (adverb, unit) = match freq.as_str() {
        "SECONDLY" => ("Every second", "seconds"),
        "MINUTELY" => ("Every minute", "minutes"),
        "HOURLY" => ("Hourly", "hours"),
        "DAILY" => ("Daily", "days"),
        "WEEKLY" => ("Weekly", "weeks"),
        "MONTHLY" => ("Monthly", "months"),
        "YEARLY" => ("Yearly", "years"),
        _ => return None,
    };
    let interval = py_int(get("INTERVAL").unwrap_or("1"))?;
    if interval < 1 {
        return None;
    }
    // WKST only moves the week boundary a weekly rule with a longer interval
    // counts from.
    if get("WKST").is_some() && freq == "WEEKLY" && interval > 1 {
        return None;
    }
    let mut text = if interval == 1 {
        adverb.to_string()
    } else {
        format!("Every {interval} {unit}")
    };
    if let Some(v) = get("BYMONTH") {
        text.push_str(&format!(" in {}", describe_bymonth(v)?));
    }
    if let Some(v) = get("BYDAY") {
        text.push_str(&format!(" on {}", describe_byday(v)?));
    }
    if let Some(v) = get("BYMONTHDAY") {
        text.push_str(&format!(" on {}", describe_bymonthday(v)?));
    }
    if let Some(v) = get("COUNT") {
        let count = py_int(v)?;
        if count == 1 {
            text.push_str(", once");
        } else {
            text.push_str(&format!(", {count} times"));
        }
    }
    if let Some(v) = get("UNTIL") {
        text.push_str(&format!(", until {}", format_time(&parse_time(v, None)?)));
    }
    Some(text)
}

// ---------------------------------------------------------------- rendering

const RENDERED_ENTRIES: &[&str] = &["VEVENT", "VTODO", "VJOURNAL", "VFREEBUSY"];
/// Apple Calendar writes this trigger for a default alarm that never fires.
const APPLE_PLACEHOLDER_TRIGGER: &str = "19760401T005545Z";
const CONFERENCE_PROPERTIES: &[&str] = &[
    "CONFERENCE",
    "X-GOOGLE-CONFERENCE",
    "X-MICROSOFT-SKYPETEAMSMEETINGURL",
];
const FILENAME_PARAMS: &[&str] = &[
    "FILENAME",
    "X-FILENAME",
    "X-APPLE-FILENAME",
    "X-ORACLE-FILENAME",
];

struct Writer {
    doc: DoclingDocument,
    tree: ItemTree,
    /// The heading level of each entry (docling's `base_level`).
    level: u8,
}

fn text_kind(label: &str, text: &str, level: Option<u8>) -> TreeKind {
    TreeKind::Text {
        label: label.into(),
        text: text.into(),
        orig: None,
        formatting: None,
        hyperlink: None,
        level,
        list: (label == "list_item").then(Default::default),
    }
}

/// Markdown for a text item: underscores escaped, as docling-core prints a
/// text item's `_`. Unlike [`escape_text`](crate::backend::markdown::escape_text)
/// `& < >` stay as they are — upstream's groundtruth prints
/// `"Martin, Carla" <carla.martin@example.com>` verbatim — and a `\n` needs
/// nothing: the serializer turns it into a hard break itself.
fn md(text: &str) -> String {
    escape_underscores(text)
}

impl Writer {
    fn add_title(&mut self, text: &str) {
        self.doc.push(Node::Heading {
            level: 1,
            text: md(text),
        });
        self.tree.add(None, None, text_kind("title", text, None));
    }

    fn add_heading(&mut self, text: &str, level: u8) {
        self.doc.push(Node::Heading {
            level: level + 1,
            text: md(text),
        });
        self.tree
            .add(None, None, text_kind("section_header", text, Some(level)));
    }

    fn add_text(&mut self, text: &str) {
        self.doc.push(Node::Paragraph { text: md(text) });
        self.tree.add(None, None, text_kind("text", text, None));
    }

    fn add_list(&mut self, heading: &str, items: &[String]) {
        if items.is_empty() {
            return;
        }
        self.add_heading(heading, self.level + 1);
        let group = self.tree.add(
            None,
            None,
            TreeKind::Group {
                label: "list".into(),
                name: heading.to_lowercase(),
            },
        );
        for (i, item) in items.iter().enumerate() {
            self.doc.push(Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: i == 0,
                text: md(item),
                level: 0,
                marker: None,
                location: None,
                split: None,
                dclx: None,
                href: None,
                layer: None,
            });
            self.tree
                .add(Some(group), None, text_kind("list_item", item, None));
        }
    }

    fn add_calendar(&mut self, calendar: &Component) {
        let name = {
            let n = calendar.text("X-WR-CALNAME");
            one_line(if n.is_empty() {
                calendar.text("NAME")
            } else {
                n
            })
        };
        if !name.is_empty() {
            self.add_title(&name);
        }
        let description = {
            let d = calendar.text("X-WR-CALDESC");
            if d.is_empty() {
                calendar.text("DESCRIPTION")
            } else {
                d
            }
        };
        for paragraph in paragraphs(description) {
            self.add_text(&paragraph);
        }
        let method = py_strip(calendar.text("METHOD")).to_uppercase();
        if !method.is_empty() && method != "PUBLISH" {
            let label = match method.as_str() {
                "REQUEST" => "Invitation".to_string(),
                "CANCEL" => "Cancellation".into(),
                "REPLY" => "Reply".into(),
                "ADD" => "Added occurrences".into(),
                "REFRESH" => "Refresh request".into(),
                "COUNTER" => "Counter-proposal".into(),
                "DECLINECOUNTER" => "Declined counter-proposal".into(),
                other => words(other),
            };
            self.add_text(&format!("Message type: {label}"));
        }
        for component in &calendar.components {
            if RENDERED_ENTRIES.contains(&component.name.as_str()) {
                self.add_entry(component);
            }
        }
    }

    fn add_entry(&mut self, entry: &Component) {
        let mut title = one_line(entry.text("SUMMARY"));
        if title.is_empty() {
            title = match entry.name.as_str() {
                "VEVENT" => "Untitled event",
                "VTODO" => "Untitled task",
                "VJOURNAL" => "Untitled journal entry",
                _ => "Free/busy",
            }
            .into();
        }
        self.add_heading(&title, self.level);
        for field in entry_fields(entry) {
            self.add_text(&field);
        }
        // A journal entry may hold several DESCRIPTION properties.
        let descriptions: Vec<String> = entry
            .get_all("DESCRIPTION")
            .flat_map(|d| paragraphs(d.value()))
            .collect();
        for paragraph in descriptions {
            self.add_text(&paragraph);
        }
        if entry.name == "VFREEBUSY" {
            self.add_list("Free/busy periods", &free_busy_periods(entry));
        }
        let attendees: Vec<String> = entry.get_all("ATTENDEE").map(attendee).collect();
        self.add_list("Attendees", &attendees);
        let reminders: Vec<String> = entry
            .components
            .iter()
            .filter(|c| c.name == "VALARM")
            .filter_map(|a| alarm(a, &entry.name))
            .filter(|r| !r.is_empty())
            .collect();
        self.add_list("Reminders", &reminders);
        let attachments: Vec<String> = entry.get_all("ATTACH").map(attachment).collect();
        self.add_list("Attachments", &attachments);
    }
}

/// `" ".join(text.split())`.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Paragraphs split on blank lines — `re.split(r"\n\s*\n+", text.strip())`,
/// each stripped, empty ones dropped.
fn paragraphs(text: &str) -> Vec<String> {
    let re = cached_regex!(r"\n\s*\n+");
    re.split(py_strip(text))
        .map(py_strip)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// `value.replace("-", " ").capitalize()`.
fn words(value: &str) -> String {
    let lower = value.replace('-', " ").to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn entry_fields(entry: &Component) -> Vec<String> {
    let mut fields = time_fields(entry);
    for prop in entry.get_all("RRULE") {
        let described = describe_rrule(prop.value()).unwrap_or_else(|| prop.value().to_string());
        fields.push(format!("Repeats: {described}"));
    }
    let list_times = |name: &str| -> Vec<String> {
        entry
            .get_all(name)
            .flat_map(|p| p.values.iter().map(move |v| list_time(v, p)))
            .collect()
    };
    let exceptions = list_times("EXDATE");
    if !exceptions.is_empty() {
        fields.push(format!("Except: {}", exceptions.join(", ")));
    }
    let additions = list_times("RDATE");
    if !additions.is_empty() {
        fields.push(format!("Also on: {}", additions.join(", ")));
    }
    if let Some(rid) = entry.get("RECURRENCE-ID") {
        let label = if rid.param_or_empty("RANGE").to_uppercase() == "THISANDFUTURE" {
            "Changes occurrences from"
        } else {
            "Changes occurrence"
        };
        fields.push(format!("{label}: {}", format_property_time(rid)));
    }
    if let Some(completed) = entry.get("COMPLETED") {
        fields.push(format!("Completed: {}", format_property_time(completed)));
    }
    let percent = py_strip(entry.text("PERCENT-COMPLETE"));
    if !percent.is_empty() {
        fields.push(format!("Progress: {percent}%"));
    }

    let location = entry.text("LOCATION");
    if !location.is_empty() {
        let lines: Vec<&str> = py_splitlines(location)
            .into_iter()
            .map(py_strip)
            .filter(|l| !l.is_empty())
            .collect();
        fields.push(format!("Location: {}", lines.join(", ")));
    }
    if let Some(organizer) = entry.get("ORGANIZER") {
        fields.push(format!("Organizer: {}", address(organizer)));
    }
    let status = py_strip(entry.text("STATUS"));
    if !status.is_empty() {
        fields.push(format!("Status: {}", words(status)));
    }
    let visibility = py_strip(entry.text("CLASS"));
    if !visibility.is_empty() && visibility.to_uppercase() != "PUBLIC" {
        fields.push(format!("Visibility: {}", words(visibility)));
    }
    if let Some(priority) = priority(entry.text("PRIORITY")) {
        fields.push(format!("Priority: {priority}"));
    }
    for (name, label) in [("CATEGORIES", "Categories"), ("RESOURCES", "Resources")] {
        let values: Vec<&str> = entry
            .get_all(name)
            .flat_map(|p| p.values.iter())
            .map(String::as_str)
            .filter(|v| !v.is_empty())
            .collect();
        if !values.is_empty() {
            fields.push(format!("{label}: {}", values.join(", ")));
        }
    }
    let url = py_strip(entry.text("URL"));
    if !url.is_empty() {
        fields.push(format!("URL: {url}"));
    }
    fields.extend(
        conferences(entry)
            .into_iter()
            .map(|c| format!("Conference: {c}")),
    );
    for prop in entry.get_all("CONTACT") {
        fields.push(format!("Contact: {}", one_line(prop.value())));
    }
    for prop in entry.get_all("COMMENT") {
        fields.push(format!("Comment: {}", one_line(prop.value())));
    }
    fields
}

/// Python's `str.splitlines()` for the separators a TEXT value can hold.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find([
            '\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
            '\u{2029}',
        ]) {
            Some(i) => {
                out.push(&rest[..i]);
                let c = rest[i..].chars().next().expect("found a char");
                let mut skip = c.len_utf8();
                if c == '\r' && rest[i + 1..].starts_with('\n') {
                    skip += 1;
                }
                rest = &rest[i + skip..];
            }
            None => {
                out.push(rest);
                break;
            }
        }
    }
    out
}

fn time_fields(entry: &Component) -> Vec<String> {
    let mut fields = Vec::new();
    let start_prop = entry.get("DTSTART");
    let end_prop = entry.get("DTEND");
    let duration = parse_duration(entry.text("DURATION"));
    let start = start_prop.and_then(property_time);
    let start_label = if entry.name == "VJOURNAL" {
        "Date"
    } else {
        "Start"
    };

    match (start_prop, start) {
        (None, _) => {}
        (Some(prop), None) => fields.push(format!("{start_label}: {}", prop.value())),
        (Some(_), Some(start)) if start.all_day() && entry.name == "VEVENT" => {
            // DTEND of an all-day entry is exclusive: a one-day event ends the
            // next day.
            let end = end_prop.and_then(property_time);
            let last_day = match &end {
                Some(end) if end.all_day() => end.date.plus_days(-1),
                None => duration
                    .filter(|d| *d >= 86_400)
                    // `date + timedelta` adds the whole days only.
                    .and_then(|d| start.date.plus_days(d / 86_400 - 1)),
                Some(_) => None,
            };
            match (&end, last_day) {
                (Some(end), _) if !end.all_day() => {
                    fields.push(format!("Start: {}", format_time(&start)));
                    fields.push(format!("End: {}", format_time(end)));
                }
                (_, Some(last)) if last > start.date => {
                    fields.push(format!("Start: {} (all day)", format_time(&start)));
                    fields.push(format!("End: {} (all day)", last.iso()));
                }
                _ => fields.push(format!("Date: {} (all day)", format_time(&start))),
            }
        }
        (Some(_), Some(start)) => {
            fields.push(format!("{start_label}: {}", format_time(&start)));
            if let Some(end) = end_prop {
                fields.push(format!("End: {}", format_property_time(end)));
            } else if let Some(d) = duration.filter(|d| *d != 0) {
                fields.push(format!("Duration: {}", format_duration(d)));
            }
        }
    }

    if let Some(due) = entry.get("DUE") {
        fields.push(format!("Due: {}", format_property_time(due)));
    }
    fields
}

fn list_time(value: &str, prop: &Property) -> String {
    if prop.param_or_empty("VALUE").to_uppercase() == "PERIOD" || value.contains('/') {
        return period(value, prop.param("TZID"));
    }
    parse_time(value, prop.param("TZID")).map_or_else(|| value.to_string(), |t| format_time(&t))
}

fn period(value: &str, tzid: Option<&str>) -> String {
    let Some((start_raw, end_raw)) = value.split_once('/') else {
        return value.to_string();
    };
    let Some(start) = parse_time(start_raw, tzid) else {
        return value.to_string();
    };
    if let Some(end) = parse_time(end_raw, tzid) {
        return format!("{} to {}", format_time(&start), format_time(&end));
    }
    match parse_duration(end_raw) {
        Some(d) if d != 0 => format!("{} for {}", format_time(&start), format_duration(d)),
        _ => value.to_string(),
    }
}

fn priority(value: &str) -> Option<&'static str> {
    match py_int(value)? {
        1..=4 => Some("High"),
        5 => Some("Medium"),
        6..=9 => Some("Low"),
        _ => None,
    }
}

fn conferences(entry: &Component) -> Vec<String> {
    for name in CONFERENCE_PROPERTIES {
        let mut found: Vec<String> = Vec::new();
        for prop in entry.get_all(name) {
            let uri = py_strip(prop.value());
            let label = one_line(prop.param_or_empty("LABEL"));
            let text = if !label.is_empty() && !uri.is_empty() {
                format!("{uri} ({label})")
            } else if !uri.is_empty() {
                uri.to_string()
            } else {
                label
            };
            if !text.is_empty() && !found.contains(&text) {
                found.push(text);
            }
        }
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// Quote a display name as the email backend does, so it parses back as
/// itself.
fn quote_display_name(name: &str) -> String {
    if name.contains(['[', ']', '(', ')', '<', '>', '@', ',', ':', ';', '"', '\\']) {
        let mut out = String::from("\"");
        for c in name.chars() {
            if c == '\\' || c == '"' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    } else {
        name.to_string()
    }
}

fn address(prop: &Property) -> String {
    let value = py_strip(prop.value());
    let address = match value.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("mailto:") => &value[7..],
        _ => value,
    };
    let name = one_line(prop.param_or_empty("CN"));
    if !name.is_empty() && !address.is_empty() && name.to_lowercase() != address.to_lowercase() {
        return format!("{} <{address}>", quote_display_name(&name));
    }
    if name.is_empty() {
        address.to_string()
    } else {
        name
    }
}

fn attendee(prop: &Property) -> String {
    let mut details: Vec<String> = Vec::new();
    match prop.param_or_empty("CUTYPE").to_uppercase().as_str() {
        "GROUP" => details.push("group".into()),
        "RESOURCE" => details.push("resource".into()),
        "ROOM" => details.push("room".into()),
        _ => {}
    }
    let role = prop.param_or_empty("ROLE").to_uppercase();
    if !role.is_empty() {
        details.push(match role.as_str() {
            "CHAIR" => "chair".into(),
            "REQ-PARTICIPANT" => "required".into(),
            "OPT-PARTICIPANT" => "optional".into(),
            "NON-PARTICIPANT" => "for information".into(),
            other => words(other).to_lowercase(),
        });
    }
    let participation = prop.param_or_empty("PARTSTAT").to_uppercase();
    if !participation.is_empty() {
        details.push(match participation.as_str() {
            "NEEDS-ACTION" => "awaiting response".into(),
            other => words(other).to_lowercase(),
        });
    }
    let text = address(prop);
    if details.is_empty() {
        text
    } else {
        format!("{text} ({})", details.join(", "))
    }
}

/// A reminder in words; `None` for one that never fires (`ACTION:NONE`,
/// Apple's placeholder trigger).
fn alarm(alarm: &Component, owner: &str) -> Option<String> {
    let action = py_strip(alarm.text("ACTION")).to_uppercase();
    if action == "NONE" {
        return None;
    }
    let trigger = alarm.get("TRIGGER");
    if trigger.is_some_and(|t| py_strip(t.value()) == APPLE_PLACEHOLDER_TRIGGER) {
        return None;
    }
    let mut text = match action.as_str() {
        "AUDIO" => "Sound".to_string(),
        "DISPLAY" => "Display".into(),
        "EMAIL" => "Email".into(),
        "PROCEDURE" => "Procedure".into(),
        "" => "Reminder".into(),
        other => words(other),
    };
    if let Some(trigger) = trigger {
        text.push_str(&format!(", {}", trigger_text(trigger, owner)));
    }
    let repeat = py_int(alarm.text("REPEAT")).unwrap_or(0);
    if let Some(interval) = parse_duration(alarm.text("DURATION")).filter(|d| *d != 0) {
        if repeat > 0 {
            let times = if repeat == 1 {
                "once".to_string()
            } else {
                format!("{repeat} times")
            };
            text.push_str(&format!(
                ", repeated {times} every {}",
                format_duration(interval)
            ));
        }
    }
    Some(text)
}

fn trigger_text(trigger: &Property, owner: &str) -> String {
    let Some(offset) = parse_duration(trigger.value()) else {
        return format!("at {}", format_property_time(trigger));
    };
    let related_to_end = trigger.param_or_empty("RELATED").to_uppercase() == "END";
    let anchor = match (related_to_end, owner) {
        (true, "VTODO") => "due",
        (true, _) => "end",
        (false, _) => "start",
    };
    if offset == 0 {
        return format!("at {anchor}");
    }
    let direction = if offset < 0 { "before" } else { "after" };
    format!("{} {direction} {anchor}", format_duration(offset))
}

/// Describe an attachment; inline binary content is never decoded or copied.
fn attachment(prop: &Property) -> String {
    let filename = FILENAME_PARAMS
        .iter()
        .find_map(|p| prop.param(p).filter(|v| !v.is_empty()))
        .map(one_line)
        .unwrap_or_default();
    let media_type = py_strip(prop.param_or_empty("FMTTYPE"));
    let inline = prop.param_or_empty("VALUE").to_uppercase() == "BINARY"
        || prop.param_or_empty("ENCODING").to_uppercase() == "BASE64";
    let name = if !filename.is_empty() {
        filename
    } else if inline {
        "Embedded attachment".to_string()
    } else {
        String::new()
    };
    let label = if !name.is_empty() && !media_type.is_empty() {
        format!("{name} ({media_type})")
    } else {
        name.clone()
    };
    if inline {
        return label;
    }
    let uri = py_strip(prop.value());
    if name.is_empty() {
        return if media_type.is_empty() {
            uri.to_string()
        } else {
            format!("{uri} ({media_type})")
        };
    }
    format!("{label}: {uri}")
}

fn free_busy_periods(entry: &Component) -> Vec<String> {
    let mut periods = Vec::new();
    for prop in entry.get_all("FREEBUSY") {
        let kind = prop
            .param("FBTYPE")
            .filter(|t| !t.is_empty())
            .unwrap_or("BUSY")
            .to_uppercase();
        let label = match kind.as_str() {
            "FREE" => "Free".to_string(),
            "BUSY" => "Busy".into(),
            "BUSY-UNAVAILABLE" => "Unavailable".into(),
            "BUSY-TENTATIVE" => "Tentative".into(),
            other => words(other),
        };
        periods.extend(
            prop.values
                .iter()
                .map(|v| format!("{label}: {}", period(v, None))),
        );
    }
    periods
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docling's `test_describe_rrule`: a rule with a part the description
    /// cannot carry stays raw (`None`).
    #[test]
    fn describes_recurrence_rules() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU",
                Some("Every 2 weeks on Tuesday"),
            ),
            (
                "FREQ=MONTHLY;BYDAY=-1FR",
                Some("Monthly on the last Friday"),
            ),
            (
                "FREQ=MONTHLY;BYDAY=-2MO",
                Some("Monthly on the second-to-last Monday"),
            ),
            (
                "FREQ=YEARLY;BYMONTH=3;BYMONTHDAY=15",
                Some("Yearly in March on the 15th"),
            ),
            (
                "FREQ=MONTHLY;BYMONTHDAY=1,-1",
                Some("Monthly on the 1st and last day"),
            ),
            ("FREQ=DAILY;COUNT=1", Some("Daily, once")),
            ("FREQ=DAILY;UNTIL=20261231", Some("Daily, until 2026-12-31")),
            ("FREQ=WEEKLY;WKST=MO;BYDAY=MO", Some("Weekly on Monday")),
            ("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1", None),
            ("FREQ=DAILY;BYHOUR=9,17", None),
            ("FREQ=WEEKLY;INTERVAL=2;WKST=SU;BYDAY=MO,SU", None),
            ("FREQ=FORTNIGHTLY", None),
        ];
        for (rule, expected) in cases {
            assert_eq!(describe_rrule(rule).as_deref(), *expected, "{rule}");
        }
    }

    #[test]
    fn civil_date_arithmetic_round_trips() {
        let d = Date::new(2024, 2, 28).unwrap();
        assert_eq!(d.plus_days(1), Date::new(2024, 2, 29));
        assert_eq!(d.plus_days(2), Date::new(2024, 3, 1));
        assert_eq!(
            Date::new(2026, 1, 1).unwrap().plus_days(-1),
            Date::new(2025, 12, 31)
        );
        assert_eq!(Date::new(1, 1, 1).unwrap().plus_days(-1), None);
        assert_eq!(Date::new(2025, 2, 29), None);
    }

    #[test]
    fn durations_spell_out() {
        assert_eq!(parse_duration("-PT15M"), Some(-900));
        assert_eq!(parse_duration("P1W"), Some(7 * 86_400));
        assert_eq!(parse_duration("PT"), None);
        assert_eq!(format_duration(5_400), "1 hour 30 minutes");
        assert_eq!(format_duration(14 * 86_400), "2 weeks");
        assert_eq!(format_duration(8 * 86_400), "8 days");
    }
}
