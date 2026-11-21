//! docling-core's `FormattingFallbackMode` (docling-core#661, #1205): how the
//! Markdown export renders the formatting Markdown has no syntax for —
//! underline, subscript and superscript. `none` (the default) drops it, as
//! every docling Markdown has; `html` writes `<u>…</u>`, `<sub>…</sub>` and
//! `<sup>…</sup>` (`MarkdownParams.formatting_fallback_mode=HTML`), in
//! docling-core's decoration order — bold, italic, underline, strikethrough,
//! sub/superscript, then the link.
//!
//! The flat nodes carry Markdown the backends rendered at conversion time,
//! by when a run's underline or script is gone. So the fallback is decided
//! there too: while a conversion runs [`with_fallback`] (the converter's
//! `markdown_formatting_fallback(Html)`), a backend's run renderer wraps
//! such a run in invisible marks — Unicode noncharacters, which no
//! interchanged text holds — through [`underline`] and [`script`]. The
//! Markdown export turns the marks into the tags ([`render`]); every other
//! export strips them ([`strip`]): DocLang, LaTeX, the chunks and the plain
//! text read the same node text, and keep their own rendering of the runs
//! (or none). Without the fallback no mark is ever written, so the default
//! output is untouched.

use std::borrow::Cow;
use std::cell::Cell;

use crate::Script;

/// docling-core's `FormattingFallbackMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FormattingFallback {
    /// Underline and sub/superscript print as plain text (docling's default).
    #[default]
    None,
    /// `<u>`, `<sub>`, `<sup>` tags.
    Html,
}

impl FormattingFallback {
    /// The wire value: `none` | `html`.
    pub fn as_str(self) -> &'static str {
        match self {
            FormattingFallback::None => "none",
            FormattingFallback::Html => "html",
        }
    }
}

impl std::fmt::Display for FormattingFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FormattingFallback {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(FormattingFallback::None),
            "html" => Ok(FormattingFallback::Html),
            other => Err(format!(
                "unknown formatting fallback {other:?} (expected none | html)"
            )),
        }
    }
}

/// The marks, open and close per tag: noncharacters U+FDD0–U+FDD5.
const MARKS: [(char, &str); 6] = [
    ('\u{FDD0}', "<u>"),
    ('\u{FDD1}', "</u>"),
    ('\u{FDD2}', "<sub>"),
    ('\u{FDD3}', "</sub>"),
    ('\u{FDD4}', "<sup>"),
    ('\u{FDD5}', "</sup>"),
];

fn is_mark(c: char) -> bool {
    ('\u{FDD0}'..='\u{FDD5}').contains(&c)
}

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// A mark was ever written in this process ([`any_marks`]).
static EVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Run `f` — a backend's conversion — with the HTML fallback on (`on`) or
/// off; the previous state is restored afterwards, also on a panic.
pub fn with_fallback<R>(on: bool, f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|a| a.set(self.0));
        }
    }
    let _restore = Restore(ACTIVE.with(|a| a.replace(on)));
    f()
}

/// Whether the current conversion writes the marks.
pub fn active() -> bool {
    ACTIVE.with(Cell::get)
}

fn wrap(text: String, open: char, close: char) -> String {
    if text.is_empty() {
        return text;
    }
    EVER.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut s = String::with_capacity(text.len() + 6);
    s.push(open);
    s.push_str(&text);
    s.push(close);
    s
}

/// An underlined run's Markdown `text` — wrapped for `<u>` under the
/// fallback, unchanged otherwise. Call it where docling-core's
/// `serialize_underline` sits: after italic, before strikethrough.
pub fn underline(text: String, on: bool) -> String {
    if on && active() {
        wrap(text, '\u{FDD0}', '\u{FDD1}')
    } else {
        text
    }
}

/// A run's Markdown `text` with its script — wrapped for `<sub>` / `<sup>`
/// under the fallback, unchanged otherwise. Call it after strikethrough,
/// before the link (docling-core's `serialize_subscript` / `_superscript`).
pub fn script(text: String, script: Script) -> String {
    if !active() {
        return text;
    }
    match script {
        Script::Sub => wrap(text, '\u{FDD2}', '\u{FDD3}'),
        Script::Super => wrap(text, '\u{FDD4}', '\u{FDD5}'),
        Script::Baseline => text,
    }
}

/// The Markdown export's view of node text: the marks as tags under
/// [`FormattingFallback::Html`], dropped otherwise.
pub fn render(text: &str, mode: FormattingFallback) -> Cow<'_, str> {
    if !text.contains(is_mark) {
        return Cow::Borrowed(text);
    }
    match mode {
        FormattingFallback::None => strip(text),
        FormattingFallback::Html => {
            let mut s = String::with_capacity(text.len() + 16);
            for c in text.chars() {
                match MARKS.iter().find(|(m, _)| *m == c) {
                    Some((_, tag)) => s.push_str(tag),
                    None => s.push(c),
                }
            }
            Cow::Owned(s)
        }
    }
}

/// `text` without the marks — every export but Markdown.
pub fn strip(text: &str) -> Cow<'_, str> {
    if text.contains(is_mark) {
        Cow::Owned(text.chars().filter(|&c| !is_mark(c)).collect())
    } else {
        Cow::Borrowed(text)
    }
}

/// [`strip`] on an owned string, without a copy when there is nothing to do.
pub fn strip_owned(text: String) -> String {
    match strip(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(s) => s,
    }
}

/// Whether any mark was ever written in this process: until then no node
/// holds one, and the exports skip the strip entirely.
pub(crate) fn any_marks() -> bool {
    EVER.load(std::sync::atomic::Ordering::Relaxed)
}

/// A writer passing everything on but the marks — the JSON writers' strip,
/// streaming: a mark is the UTF-8 sequence `EF B7 90`–`95`, held back
/// across a write boundary until it is complete.
pub(crate) struct MarkFilter<W> {
    inner: W,
    pending: Vec<u8>,
}

impl<W: std::io::Write> MarkFilter<W> {
    pub(crate) fn new(inner: W) -> Self {
        MarkFilter {
            inner,
            pending: Vec::new(),
        }
    }
}

impl<W: std::io::Write> std::io::Write for MarkFilter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(buf);
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i] == 0xEF {
                // An incomplete sequence at the end waits for the next write.
                if data.len() - i < 3 {
                    self.pending = data[i..].to_vec();
                    break;
                }
                if data[i + 1] == 0xB7 && (0x90..=0x95).contains(&data[i + 2]) {
                    i += 3;
                    continue;
                }
            }
            out.push(data[i]);
            i += 1;
        }
        self.inner.write_all(&out)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            let rest = std::mem::take(&mut self.pending);
            self.inner.write_all(&rest)?;
        }
        self.inner.flush()
    }
}

/// [`strip`] over every string of a JSON value.
pub fn strip_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            if s.contains(is_mark) {
                *s = strip(s).into_owned();
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(strip_value),
        serde_json::Value::Object(o) => o.values_mut().for_each(strip_value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_only_under_the_fallback() {
        assert_eq!(underline("x".into(), true), "x");
        assert_eq!(script("2".into(), Script::Sub), "2");
        with_fallback(true, || {
            let u = underline("&lt;tag&gt;".into(), true);
            let sub = script("2".into(), Script::Sub);
            let sup = script("2".into(), Script::Super);
            let text = format!("H{sub}O, mc{sup}, {u}");
            assert_eq!(
                render(&text, FormattingFallback::Html),
                "H<sub>2</sub>O, mc<sup>2</sup>, <u>&lt;tag&gt;</u>"
            );
            assert_eq!(
                render(&text, FormattingFallback::None),
                "H2O, mc2, &lt;tag&gt;"
            );
            assert_eq!(strip(&text), "H2O, mc2, &lt;tag&gt;");
            // Not underlined: unchanged even under the fallback.
            assert_eq!(underline("x".into(), false), "x");
        });
        assert!(!active());
    }

    #[test]
    fn parses_the_wire_values() {
        assert_eq!("HTML".parse(), Ok(FormattingFallback::Html));
        assert_eq!("none".parse(), Ok(FormattingFallback::None));
        assert!("tags".parse::<FormattingFallback>().is_err());
    }
}
