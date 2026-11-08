//! DocLang XML serialization of the flat nodes (`export_to_doclang`) — the
//! markup inside a `.dclx` archive, mirroring docling-core's
//! `DocLangDocSerializer` with `DocLangParams()` defaults (version 0.9,
//! 2-space pretty indent, AUTO CDATA/content wrapping, placeholder image
//! mode → no `<src>` data). A document with an item tree the tree port
//! covers is serialized from the tree instead (`doclang_tree.rs`); this is
//! the path of the located documents (PDF, the spreadsheet and slide
//! backends) and the ones without a tree.
//!
//! The elements are laid out as fragments, every text block body one
//! fragment — its runs concatenated as they are, as docling-core's inline
//! serializer does; DocLang 0.9 reads a line break inside a body as a space
//! — and docling-core's post-processing (`doclang_xml.rs`) then unwraps the
//! redundant `<content>` and pretty-prints the tree as upstream does.
//!
//! Inline formatting: our model bakes bold/italic/code/links into the text as
//! docling-legacy Markdown markers; [`inline_runs`] re-parses those into the
//! structural `<bold>`/`<italic>`/`<code>` elements DocLang expects.

use crate::document::{ContentLayer, FieldItem, InlineRun, Node, Script, Table};
use std::borrow::Cow;

const INDENT: &str = "  ";

/// Rendered fragments, one per line: (indent depth, content). A text block
/// body is always one fragment; the layout is redone by the post-processing.
struct Out {
    lines: Vec<(i32, String)>,
    /// Running index for exported image assets (`assets/image_{NNNNNN}_…`),
    /// incremented per body picture in document order.
    pic_index: usize,
    /// The `(path, encoded bytes)` of each referenced asset, in `<src>` order,
    /// when the caller packages them (`export_to_doclang_with_assets`).
    assets: Option<Vec<(String, Vec<u8>)>>,
    /// How a picture's image is written.
    pictures: Pictures,
    /// The referenced assets' digests and PNG bytes, decoded up front in
    /// parallel (#898); empty for the other modes, which decode as they go.
    digests: crate::pixel_digest::Assets,
}

/// docling's `image_mode` for DocLang: where a picture's `<src>` points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pictures {
    /// No `<src>` — docling-core's `export_to_doclang()` default.
    Placeholder,
    /// `assets/image_{index:06}_{sha256}.png` — the archive's `document.xml`
    /// (`save_as_doclang_archive`); `collect` gathers the asset bytes.
    Referenced { collect: bool },
    /// A `data:` URI of the PNG docling re-encodes.
    Embedded,
}

impl Out {
    fn push(&mut self, depth: i32, s: impl Into<String>) {
        self.lines.push((depth, s.into()));
    }

    fn finish(self) -> String {
        let mut s = String::new();
        for (d, line) in self.lines {
            for _ in 0..d {
                s.push_str(INDENT);
            }
            s.push_str(&line);
            s.push('\n');
        }
        // The fragments are laid out by element; every text block body is one
        // fragment, so the whitespace between them is only ever whitespace
        // between blocks (or at a block's edge), which DocLang 0.9 ignores.
        // docling-core's own post-processing then lays the tree out.
        crate::doclang_xml::finish(&s)
    }
}

/// Reverse the Markdown-oriented escaping backends bake into node text
/// (`&amp;`/`&lt;`/`&gt;` and `\_`), recovering the raw text DocLang serializes:
/// XML-1.0-illegal characters (C0 controls minus tab/LF/CR, U+FFFE/U+FFFF)
/// replaced with a visible `[U+XXXX]` marker, docling-core#687's rendering.
/// Surrogates cannot occur in a Rust `str`, so the Python pattern's surrogate
/// range has no counterpart here. Borrows when nothing needs replacing (the
/// overwhelmingly common case).
pub(crate) fn sanitize_xml_illegal(text: &str) -> std::borrow::Cow<'_, str> {
    let illegal = |c: char| matches!(c, '\u{00}'..='\u{08}' | '\u{0B}' | '\u{0C}' | '\u{0E}'..='\u{1F}' | '\u{FFFE}' | '\u{FFFF}');
    if !text.contains(illegal) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if illegal(c) {
            out.push_str(&format!("[U+{:04X}]", c as u32));
        } else {
            out.push(c);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// `<`/`>`/`&` go into a CDATA section verbatim, `_` stays literal.
fn unescape_stored(text: &str) -> Cow<'_, str> {
    if !text.contains('&') && !text.contains('\\') {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
            .replace("\\_", "_"),
    )
}

/// AUTO escape: any of `"' &<>` in the text → CDATA; leading/trailing
/// whitespace or a newline → additionally wrapped in `<content>`.
/// Robustness (docling-core 2.88/2.89 parity, #253): XML-1.0-illegal
/// characters are replaced with a visible `[U+XXXX]` marker before anything
/// else (docling-core#687 — a stray control byte must not make the archive
/// unparseable), and a literal `]]>` inside the text splits across adjacent
/// CDATA sections (docling-core#689 — a CDATA section cannot contain its own
/// closing delimiter).
fn escape_text(text: &str) -> String {
    let raw = unescape_stored(text);
    crate::doclang_xml::escape_text(raw.as_ref(), true)
}

/// An inline run of a text node after re-parsing our Markdown markers.
enum Run {
    Plain(String),
    Bold(String),
    Italic(String),
    BoldItalic(String),
    Code(String),
    /// `[anchor](uri)` — DocLang has no inline href element; the anchor text
    /// stays inline and, when the run is the only content, the uri becomes a
    /// `<href uri=…/>` in the element head.
    Link {
        anchor: String,
        uri: String,
    },
}

/// Split docling-legacy inline markers (`***x***`, `**x**`, `*x*`, `` `x` ``,
/// `[t](u)`) into runs. Unmatched markers stay literal.
fn inline_runs(text: &str) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    // All scanning stays on the char slice: materializing the tail as a
    // String per position (the previous shape) made this scanner O(n²) and
    // dominated whole-document DocLang serialization.
    let find = |open: usize, pat: &[char]| -> Option<usize> {
        chars
            .get(open..)?
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|p| open + p)
    };
    let starts = |at: usize, pat: &[char]| chars[at..].starts_with(pat);
    while i < n {
        let take = |runs: &mut Vec<Run>, plain: &mut String, r: Run| {
            if !plain.is_empty() {
                runs.push(Run::Plain(std::mem::take(plain)));
            }
            runs.push(r);
        };
        if starts(i, &['*', '*', '*']) {
            if let Some(end) = find(i + 3, &['*', '*', '*']) {
                let inner: String = chars[i + 3..end].iter().collect();
                take(&mut runs, &mut plain, Run::BoldItalic(inner));
                i = end + 3;
                continue;
            }
        }
        if starts(i, &['*', '*']) {
            if let Some(end) = find(i + 2, &['*', '*']) {
                let inner: String = chars[i + 2..end].iter().collect();
                take(&mut runs, &mut plain, Run::Bold(inner));
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '*' && !starts(i, &['*', '*']) {
            if let Some(end) = find(i + 1, &['*']) {
                let inner: String = chars[i + 1..end].iter().collect();
                if !inner.is_empty() {
                    take(&mut runs, &mut plain, Run::Italic(inner));
                    i = end + 1;
                    continue;
                }
            }
        }
        if chars[i] == '`' {
            if let Some((s, e, after)) = crate::markdown::code_span_bounds(&chars, i) {
                let inner: String = chars[s..e].iter().collect();
                take(&mut runs, &mut plain, Run::Code(inner));
                i = after;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some(close) = find(i + 1, &[']', '(']) {
                if let Some(endp) = link_dest_end(&chars, close + 2) {
                    let anchor: String = chars[i + 1..close].iter().collect();
                    let uri: String = chars[close + 2..endp].iter().collect();
                    take(&mut runs, &mut plain, Run::Link { anchor, uri });
                    i = endp + 1;
                    continue;
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    if !plain.is_empty() {
        runs.push(Run::Plain(plain));
    }
    runs
}

/// Parse a docling-legacy Markdown string into structured [`InlineRun`]s for a
/// [`Node::InlineGroup`]. Handles the marker set docling emits — `***`, `**`,
/// `*`, `~~`, `` ` ``, `[t](u)` — recursively so nested markers combine
/// formatting, and splits plain text on newlines (docling's `<br>` / text-node
/// boundaries become separate runs). Underline and sub/superscript have no
/// Markdown representation and therefore never appear via this path.
pub fn inline_runs_from_markdown(text: &str) -> Vec<InlineRun> {
    let mut out = Vec::new();
    parse_md_runs(
        &text.chars().collect::<Vec<_>>(),
        InlineRun::default(),
        &mut out,
    );
    out
}

/// Flush `acc`'s buffered text into `out` as one run, trimmed; a blank segment
/// yields nothing (docling has no empty text items). Internal newlines (soft
/// breaks) are kept — docling holds them in a single text item.
fn flush_md_plain(buf: &mut String, style: &InlineRun, out: &mut Vec<InlineRun>) {
    let text = std::mem::take(buf);
    let text = text.trim();
    if !text.is_empty() {
        out.push(InlineRun {
            text: text.to_string(),
            ..style.clone()
        });
    }
}

/// Recursive marker scanner: `style` carries the formatting active from enclosing
/// spans; plain text inherits it, and each marker recurses with the extra flag.
fn parse_md_runs(chars: &[char], style: InlineRun, out: &mut Vec<InlineRun>) {
    let n = chars.len();
    let mut i = 0;
    let mut plain = String::new();
    // Char-slice scanning throughout — the tail-String-per-position shape
    // this replaces was O(n²) over every serialized text node.
    let find = |open: usize, pat: &[char]| -> Option<usize> {
        chars
            .get(open..)?
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|p| open + p)
    };
    let starts = |at: usize, pat: &[char]| chars[at..].starts_with(pat);
    let sub = |a: usize, b: usize| -> Vec<char> { chars[a..b].to_vec() };
    while i < n {
        // Longest markers first so `**`/`***` aren't mis-split.
        if starts(i, &['*', '*', '*']) {
            if let Some(end) = find(i + 3, &['*', '*', '*']) {
                flush_md_plain(&mut plain, &style, out);
                parse_md_runs(
                    &sub(i + 3, end),
                    InlineRun {
                        bold: true,
                        italic: true,
                        ..style.clone()
                    },
                    out,
                );
                i = end + 3;
                continue;
            }
        }
        if starts(i, &['*', '*']) {
            if let Some(end) = find(i + 2, &['*', '*']) {
                flush_md_plain(&mut plain, &style, out);
                parse_md_runs(
                    &sub(i + 2, end),
                    InlineRun {
                        bold: true,
                        ..style.clone()
                    },
                    out,
                );
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '*' {
            if let Some(end) = find(i + 1, &['*']) {
                if end > i + 1 {
                    flush_md_plain(&mut plain, &style, out);
                    parse_md_runs(
                        &sub(i + 1, end),
                        InlineRun {
                            italic: true,
                            ..style.clone()
                        },
                        out,
                    );
                    i = end + 1;
                    continue;
                }
            }
        }
        if starts(i, &['~', '~']) {
            if let Some(end) = find(i + 2, &['~', '~']) {
                flush_md_plain(&mut plain, &style, out);
                parse_md_runs(
                    &sub(i + 2, end),
                    InlineRun {
                        strike: true,
                        ..style.clone()
                    },
                    out,
                );
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '`' {
            if let Some((s, e, after)) = crate::markdown::code_span_bounds(chars, i) {
                flush_md_plain(&mut plain, &style, out);
                let inner: String = sub(s, e).iter().collect();
                let inner = inner.trim();
                if !inner.is_empty() {
                    out.push(InlineRun {
                        text: inner.to_string(),
                        code: true,
                        ..style.clone()
                    });
                }
                i = after;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some(close) = find(i + 1, &[']', '(']) {
                if let Some(endp) = link_dest_end(chars, close + 2) {
                    flush_md_plain(&mut plain, &style, out);
                    // Inline scope drops the href; the anchor keeps its styling.
                    parse_md_runs(&sub(i + 1, close), style.clone(), out);
                    i = endp + 1;
                    continue;
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush_md_plain(&mut plain, &style, out);
}

/// The index of the `)` closing a `[anchor](destination)` link whose scan
/// starts at `start` (the first destination char), or `None` when it is never
/// closed. Parentheses inside the destination nest, per CommonMark: Wikipedia's
/// interwiki links (`/wiki/Houad_(evn)`) are exactly this case, and stopping at
/// the first `)` used to cut the URI in half and leak the tail into the text.
pub(crate) fn link_dest_end(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in chars.get(start..)?.iter().enumerate() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(start + offset),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Attribute-value escaping for generated URIs/labels.
fn attr_escape(v: &str) -> String {
    v.replace('&', "&amp;").replace('"', "&quot;")
}

/// Render a text body (with inline markers) into `out`.
///
/// A single plain run renders inline within its wrapper; mixed runs become
/// the reference's block form: plain fragments as bare indented lines,
/// formatted fragments as their own inline elements — matching minidom's
/// output for a `<text>` with element children.
fn emit_text_element(
    out: &mut Out,
    depth: i32,
    tag_open: &str,
    tag: &str,
    text: &str,
    location: Option<&[u16; 4]>,
) {
    // With layout provenance the element renders in block form: the `<location>`
    // tokens are element children, then the text runs.
    if let Some(loc) = location {
        out.push(depth, format!("<{tag_open}>"));
        let mut runs = if text.is_empty() {
            Vec::new()
        } else {
            inline_runs(text)
        };
        // A lone `[anchor](uri)` puts its `<href uri=…/>` in the head ahead of
        // the location, as docling-core does for an item's `hyperlink` (a
        // PPTX shape's link, #767).
        if let [Run::Link { anchor, uri }] = runs.as_slice() {
            out.push(depth + 1, format!("<href uri=\"{}\"/>", attr_escape(uri)));
            runs = if anchor.trim().is_empty() {
                Vec::new()
            } else {
                inline_runs(anchor)
            };
        }
        push_location(out, depth + 1, loc);
        if !runs.is_empty() {
            emit_runs(out, depth + 1, runs);
        }
        out.push(depth, format!("</{tag}>"));
        return;
    }
    // An empty text item renders as an empty element on one line (docling emits
    // one per blank body paragraph).
    if text.is_empty() {
        out.push(depth, format!("<{tag_open}></{tag}>"));
        return;
    }
    let runs = inline_runs(text);
    let only_plain = runs.len() == 1 && matches!(runs[0], Run::Plain(_));
    // A lone `[anchor](uri)` becomes `<href uri=…/>` in the head; the anchor's
    // own markers still render (`[***x***](u)` → href + `<italic><bold>…`).
    if runs.len() == 1 {
        if let Run::Link { anchor, uri } = &runs[0] {
            out.push(depth, format!("<{tag_open}>"));
            out.push(depth + 1, format!("<href uri=\"{}\"/>", attr_escape(uri)));
            if !anchor.trim().is_empty() {
                emit_runs(out, depth + 1, inline_runs(anchor));
            }
            out.push(depth, format!("</{tag}>"));
            return;
        }
    }
    if only_plain {
        let mut body = escape_text(text);
        // A right-to-left text carries docling-core's `<rtl>` marker.
        if is_rtl(text) {
            body = format!("<rtl>{body}</rtl>");
        }
        // A `<content>`/`<rtl>` wrapper is an *element* child, so minidom
        // renders the wrapper in block form; bare text / CDATA is a single text
        // child and stays inline.
        if body.starts_with("<content>") || body.starts_with("<rtl>") {
            out.push(depth, format!("<{tag_open}>"));
            out.push(depth + 1, body);
            out.push(depth, format!("</{tag}>"));
        } else {
            out.push(depth, format!("<{tag_open}>{body}</{tag}>"));
        }
        return;
    }
    out.push(depth, format!("<{tag_open}>"));
    emit_runs(out, depth + 1, runs);
    out.push(depth, format!("</{tag}>"));
}

/// Whether `text` (Markdown) holds a bold, italic or code run — a mixed
/// caption or furniture text, written with its runs; plain text keeps its
/// established form.
fn has_styled_runs(text: &str) -> bool {
    inline_runs(text).iter().any(|r| {
        matches!(
            r,
            Run::Bold(_) | Run::Italic(_) | Run::BoldItalic(_) | Run::Code(_)
        )
    })
}

fn emit_runs(out: &mut Out, depth: i32, runs: Vec<Run>) {
    let body = runs_markup(runs);
    if !body.is_empty() {
        out.push(depth, body);
    }
}

/// The markup of a text block body from its runs, concatenated as they are:
/// each run carries its own boundary whitespace (DocLang 0.9 — a line break
/// between runs would read as a space), escaped per run as docling-core
/// escapes each run's text item; the redundant `<content>` wrappers go in
/// the post-processing.
fn runs_markup(runs: Vec<Run>) -> String {
    let mut s = String::new();
    for run in runs {
        match run {
            Run::Plain(t) => {
                let t = t.trim_matches('\n');
                if !t.is_empty() {
                    s.push_str(&plain_markup(t));
                }
            }
            Run::Bold(t) => s.push_str(&format!("<bold>{}</bold>", escape_text(&t))),
            Run::Italic(t) => s.push_str(&format!("<italic>{}</italic>", escape_text(&t))),
            Run::BoldItalic(t) => s.push_str(&format!(
                "<italic><bold>{}</bold></italic>",
                escape_text(&t)
            )),
            Run::Code(t) => s.push_str(&format!("<code>{}</code>", escape_text(&t))),
            Run::Link { anchor, .. } => {
                // Inline scope: DocLang drops the target, keeps the anchor.
                if !anchor.is_empty() {
                    s.push_str(&plain_markup(&anchor));
                }
            }
        }
    }
    s
}

/// A text node in block (element-children) context: plain data indents like
/// any child; a `<content>` wrapper is a normal element; bare CDATA glues to
/// the next fragment with no indent/newline (minidom's CDATA rule).
fn emit_text_node(out: &mut Out, depth: i32, text: &str) {
    out.push(depth, plain_markup(text));
}

/// A plain run's markup: escaped, and — docling's DocLang post-processing,
/// docling-core's `serialize_rtl` — wrapped in `<rtl>` when right-to-left, so
/// a reader knows the run's direction without re-running the bidi algorithm.
fn plain_markup(text: &str) -> String {
    let e = escape_text(text);
    if is_rtl(text) {
        format!("<rtl>{e}</rtl>")
    } else {
        e
    }
}

/// docling-core's `get_text_direction` == "rtl": the first character is a
/// strong right-to-left one, or more than half of the characters are.
///
/// `is_strong_rtl` approximates Unicode's `R`/`AL` bidi classes by block rather
/// than by a full character-database table — docling-core reaches them through
/// Python's `unicodedata`, which has no equivalent in core (and pulling a
/// Unicode-table crate into a wasm-compiled, MSRV-1.85 crate for one predicate
/// is not worth it). Every assigned RTL script block is covered; the
/// approximation only shows up on unassigned code points inside them.
pub(crate) fn is_rtl(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if is_strong_rtl(first) {
        return true;
    }
    let total = 1 + chars.count();
    let rtl = text.chars().filter(|c| is_strong_rtl(*c)).count();
    rtl * 2 > total
}

/// True for the `R` and `AL` bidi classes (see [`is_rtl`]).
fn is_strong_rtl(c: char) -> bool {
    let c = c as u32;
    // Arabic digits and the Arabic number signs are `AN`/`EN`, not `AL`, and
    // sit inside the Arabic block — carve them out before the range test.
    if matches!(c, 0x0600..=0x0605 | 0x0660..=0x0669 | 0x066B..=0x066C | 0x06DD | 0x06F0..=0x06F9) {
        return false;
    }
    matches!(c,
        0x0590..=0x05FF        // Hebrew (R)
        | 0x0600..=0x07BF      // Arabic, Syriac, Arabic Supplement, Thaana (AL)
        | 0x07C0..=0x085F      // NKo, Samaritan, Mandaic (R)
        | 0x0860..=0x08FF      // Syriac Supplement, Arabic Extended-A/B (AL)
        | 0x200F               // RIGHT-TO-LEFT MARK
        | 0xFB1D..=0xFB4F      // Hebrew presentation forms (R)
        | 0xFB50..=0xFDFF      // Arabic presentation forms-A (AL)
        | 0xFE70..=0xFEFF      // Arabic presentation forms-B (AL)
        | 0x10800..=0x10FFF    // historic RTL scripts (R)
        | 0x1E800..=0x1EC6F    // Mende Kikakui, Adlam (R)
        | 0x1EE00..=0x1EEFF    // Arabic mathematical alphabetic symbols (AL)
    )
}

/// Map a docling `CodeLanguageLabel` value (as stored in [`Node::Code::language`]
/// and the JSON export) to the DocLang recommended (Linguist) label. Returns
/// `None` for unknown/absent languages — matching docling's AUTO `label_mode`,
/// which omits the `<label>` when the resolved label would be `undefined`.
pub(crate) fn code_lang_label(lang: &str) -> Option<&'static str> {
    // Fold the raw fence string (e.g. "python") onto the canonical docling
    // `CodeLanguageLabel` value ("Python") first — the same normalization the
    // JSON export uses — then map that to the DocLang (Linguist) label.
    let lang = crate::json::code_language(Some(lang));
    Some(match lang {
        // Docling values whose Linguist key differs.
        "Bash" => "Shell",
        "FORTRAN" => "Fortran",
        "Latex" => "TeX",
        "Lisp" => "Common Lisp",
        "Matlab" | "Octave" => "MATLAB",
        "ObjectiveC" => "Objective-C",
        "SML" => "Standard ML",
        "VisualBasic" => "Visual Basic .NET",
        "DocLang" => "XML",
        // Docling labels without a distinct Linguist key collapse to `other`.
        "bc" | "dc" | "Tikz" => "other",
        // Values whose Linguist key equals the docling value.
        "Ada" | "Awk" | "C" | "C#" | "C++" | "CMake" | "COBOL" | "CSS" | "Ceylon" | "Clojure"
        | "Crystal" | "Cuda" | "Cython" | "D" | "Dart" | "Dockerfile" | "Elixir" | "Erlang"
        | "Forth" | "Go" | "HTML" | "Haskell" | "Haxe" | "Java" | "JavaScript" | "JSON"
        | "Julia" | "Kotlin" | "Lua" | "MoonScript" | "Nim" | "OCaml" | "PHP" | "Pascal"
        | "Perl" | "Prolog" | "Python" | "Racket" | "Ruby" | "Rust" | "SQL" | "Scala"
        | "Scheme" | "Swift" | "TypeScript" | "XML" | "YAML" => {
            return Some(IDENTITY_LABELS[IDENTITY_LABELS.iter().position(|&x| x == lang).unwrap()])
        }
        _ => return None, // "unknown" and anything unrecognized → no <label>
    })
}

/// Language labels whose DocLang (Linguist) form is identical to the docling
/// `CodeLanguageLabel` value — used to hand back a `'static` reference.
static IDENTITY_LABELS: &[&str] = &[
    "Ada",
    "Awk",
    "C",
    "C#",
    "C++",
    "CMake",
    "COBOL",
    "CSS",
    "Ceylon",
    "Clojure",
    "Crystal",
    "Cuda",
    "Cython",
    "D",
    "Dart",
    "Dockerfile",
    "Elixir",
    "Erlang",
    "Forth",
    "Go",
    "HTML",
    "Haskell",
    "Haxe",
    "Java",
    "JavaScript",
    "JSON",
    "Julia",
    "Kotlin",
    "Lua",
    "MoonScript",
    "Nim",
    "OCaml",
    "PHP",
    "Pascal",
    "Perl",
    "Prolog",
    "Python",
    "Racket",
    "Ruby",
    "Rust",
    "SQL",
    "Scala",
    "Scheme",
    "Swift",
    "TypeScript",
    "XML",
    "YAML",
];

/// A `<code>`: the head (`_element_head_prefix`'s order: the Linguist label
/// of a known language, then the location), then the escaped body.
fn emit_code(
    out: &mut Out,
    depth: i32,
    language: Option<&str>,
    text: &str,
    location: Option<&[u16; 4]>,
) {
    let label = language.and_then(code_lang_label);
    let escaped = escape_text(text);
    if location.is_none() && label.is_none() {
        out.push(depth, format!("<code>{escaped}</code>"));
        return;
    }
    out.push(depth, "<code>".to_string());
    if let Some(l) = label {
        out.push(depth + 1, format!("<label value=\"{}\"/>", attr_escape(l)));
    }
    if let Some(loc) = location {
        push_location(out, depth + 1, loc);
    }
    out.push(depth + 1, escaped);
    out.push(depth, "</code>".to_string());
}

/// Emit the four `<location>` provenance tokens (`x0,y0,x1,y1`) as element
/// children — docling's element head for backends with real geometry.
fn push_location(out: &mut Out, depth: i32, loc: &[u16; 4]) {
    for v in loc {
        out.push(depth, format!("<location value=\"{v}\"/>"));
    }
}

/// A `<table>`: the element head — the location block, then the caption —
/// and the cells after it, the DocLang schema's `element_head` order
/// (docling-core writes it so too). The caption used to come first, ahead
/// of the location, which the schema rejects (#797).
fn emit_table(out: &mut Out, depth: i32, table: &Table) {
    out.push(depth, "<table>".to_string());
    // Layout provenance (PDF/spreadsheet/slide backends): the four
    // `<location>` tokens (x0,y0,x1,y1).
    if let Some(loc) = &table.location {
        push_location(out, depth + 1, loc);
    }
    if let Some(cap) = &table.caption {
        if has_styled_runs(cap) {
            // A mixed caption (docling-core#865) keeps its runs.
            out.push(depth + 1, "<caption>".to_string());
            emit_runs(out, depth + 2, inline_runs(cap));
            out.push(depth + 1, "</caption>".to_string());
        } else {
            // `caption` arrives already escaped (backend convention), so it
            // is emitted verbatim.
            out.push(depth + 1, format!("<caption>{cap}</caption>"));
        }
    }
    emit_table_rows(out, depth, table);
    out.push(depth, "</table>".to_string());
    // `_serialize_floating_satellites`: a table with footnotes is wrapped
    // with them in a `<group>`, the footnotes after it (the flat nodes do
    // not place them by layout). The post-processing lays the group out.
    let notes: Vec<&String> = table.footnotes.iter().filter(|n| !n.is_empty()).collect();
    if !notes.is_empty() {
        let at = out
            .lines
            .iter()
            .rposition(|(d, line)| *d == depth && line == "<table>")
            .unwrap_or(0);
        out.lines.insert(at, (depth, "<group>".to_string()));
        for note in notes {
            out.push(depth, format!("<footnote>{}</footnote>", escape_text(note)));
        }
        out.push(depth, "</group>".to_string());
    }
}

/// A chart — docling's `PictureItem` with a tabular chart-data annotation:
/// `<picture class="chart">` wrapping a `<label value="{kind}"/>` and the data
/// grid as a `<tabular>` (same cell tokens as a table).
fn emit_chart(
    out: &mut Out,
    depth: i32,
    kind: &str,
    table: &Table,
    caption: Option<&str>,
    location: Option<&[u16; 4]>,
) {
    out.pic_index += 1;
    out.push(depth, "<picture class=\"chart\">".to_string());
    out.push(
        depth + 1,
        format!("<label value=\"{}\"/>", attr_escape(kind)),
    );
    if let Some(loc) = location {
        push_location(out, depth + 1, loc);
    }
    if let Some(cap) = caption {
        out.push(
            depth + 1,
            format!("<caption>{}</caption>", escape_text(cap)),
        );
    }
    out.push(depth + 1, "<tabular>".to_string());
    emit_table_rows(out, depth + 1, table);
    out.push(depth + 1, "</tabular>".to_string());
    out.push(depth, "</picture>".to_string());
}

/// Emit a grid's cells (the shared body of `<table>` and a chart's `<tabular>`):
/// each row's OTSL cell tokens at `depth + 1`. The element head (location,
/// caption) is the caller's.
fn emit_table_rows(out: &mut Out, depth: i32, table: &Table) {
    for (ri, row) in table.rows.iter().enumerate() {
        for (ci, cell) in row.iter().enumerate() {
            // A span continuation is a token-only cell (no text child):
            // horizontal → `<lcel/>`, vertical → `<ucel/>`. Otherwise
            // empty→`<ecel/>`, header→`<ched/>`, else `<fcel/>`.
            let cont = |grid: &Vec<Vec<bool>>| {
                grid.get(ri)
                    .and_then(|r| r.get(ci))
                    .copied()
                    .unwrap_or(false)
            };
            let is_lcel = table
                .structure
                .as_ref()
                .map(|s| cont(&s.col_continuation))
                .unwrap_or(false);
            let is_ucel = table
                .structure
                .as_ref()
                .map(|s| cont(&s.row_continuation))
                .unwrap_or(false);
            let is_header = match &table.structure {
                Some(s) if !s.col_header.is_empty() => s
                    .col_header
                    .get(ri)
                    .and_then(|r| r.get(ci))
                    .copied()
                    .unwrap_or(false),
                Some(s) => s.header_row.get(ri).copied().unwrap_or(false),
                None => ri == 0,
            };
            let is_row_header = table
                .structure
                .as_ref()
                .map(|s| {
                    s.row_header
                        .get(ri)
                        .and_then(|r| r.get(ci))
                        .copied()
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let tok = if is_lcel && is_ucel {
                // Continues a span in both axes (a 2-D covered cell) → `<xcel/>`.
                "<xcel/>"
            } else if is_lcel {
                "<lcel/>"
            } else if is_ucel {
                "<ucel/>"
            } else if cell.trim().is_empty() {
                "<ecel/>"
            } else if is_header {
                "<ched/>"
            } else if is_row_header {
                "<rhed/>"
            } else {
                "<fcel/>"
            };
            out.push(depth + 1, tok.to_string());
            if !is_lcel && !is_ucel {
                // A rich cell (ODF lists / nested tables / multi-paragraph)
                // emits its structured blocks after the token; otherwise the
                // flat cell text renders inline.
                let blocks = table
                    .cell_blocks
                    .as_ref()
                    .and_then(|b| b.get(ri))
                    .and_then(|r| r.get(ci))
                    .filter(|b| !b.is_empty());
                if let Some(blocks) = blocks {
                    let mut bi = 0;
                    emit_nodes(out, depth + 1, blocks, &mut bi, 0);
                } else if !cell.trim().is_empty() {
                    emit_cell_text(out, depth + 1, cell);
                }
            }
        }
        out.push(depth + 1, "<nl/>".to_string());
    }
}

/// Table-cell content: virtual text (no wrapper), inline markers re-parsed.
fn emit_cell_text(out: &mut Out, depth: i32, text: &str) {
    let runs = inline_runs(text.trim());
    emit_runs(out, depth, runs);
}

/// Serialize the node stream to DocLang XML (no trailing newline) — the
/// archive's `document.xml`, as [`crate::DoclingDocument::export_to_doclang`]
/// writes it.
#[cfg(test)]
fn export_to_doclang(nodes: &[Node]) -> String {
    render(nodes, Pictures::Referenced { collect: false }).0
}

/// Serialize the node stream to DocLang XML (no trailing newline), pictures
/// written per `pictures`, plus the asset parts a `Referenced { collect }`
/// run's `<src>` references name — `(assets/image_….png, bytes)` in document
/// order, PNG for PNG and JPEG sources ([`crate::pixel_digest::asset_png`]),
/// the original encoding for the rest (the archive writer converts those).
pub(crate) fn render(nodes: &[Node], pictures: Pictures) -> (String, Vec<(String, Vec<u8>)>) {
    let digests = if matches!(pictures, Pictures::Referenced { .. }) {
        let mut images: Vec<&[u8]> = Vec::new();
        crate::document::for_each_picture_in(nodes, &mut |n| {
            if let Node::Picture {
                image: Some(img), ..
            } = n
            {
                images.push(img.data.as_slice());
            }
        });
        crate::pixel_digest::Assets::prepare(images)
    } else {
        crate::pixel_digest::Assets::empty()
    };
    let mut out = Out {
        lines: Vec::new(),
        pic_index: 0,
        assets: matches!(pictures, Pictures::Referenced { collect: true }).then(Vec::new),
        pictures,
        digests,
    };
    out.push(0, "<doclang version=\"0.9\">".to_string());
    let mut i = 0usize;
    emit_nodes(&mut out, 1, nodes, &mut i, 0);
    out.push(0, "</doclang>".to_string());
    let assets = out.assets.take().unwrap_or_default();
    (out.finish(), assets)
}

/// Emit nodes at list-nesting `level`; consumes consecutive ListItems into
/// `<list>` blocks (recursing for deeper levels).
fn emit_nodes(out: &mut Out, depth: i32, nodes: &[Node], i: &mut usize, level: u8) {
    while *i < nodes.len() {
        match &nodes[*i] {
            Node::Heading { level, text } => {
                let open = if *level <= 1 {
                    "heading".to_string()
                } else {
                    // Clamp to the heading vocabulary (docling-core#688): deep
                    // section headers serialize as level 6 instead of raising.
                    format!("heading level=\"{}\"", (*level).min(6))
                };
                emit_text_element(out, depth, &open, "heading", text, None);
                *i += 1;
            }
            Node::Paragraph { text } | Node::ListText { text, .. } => {
                // A standalone display equation (docling's block `FormulaItem`) is
                // stored as a `$$…$$` paragraph so Markdown/JSON render the fenced
                // math; DocLang emits it as a `<formula>` element.
                if let Some(latex) = text
                    .strip_prefix("$$")
                    .and_then(|t| t.strip_suffix("$$"))
                    .filter(|t| !t.is_empty())
                {
                    out.push(depth, format!("<formula>{}</formula>", escape_text(latex)));
                } else {
                    emit_text_element(out, depth, "text", "text", text, None);
                }
                *i += 1;
            }
            Node::Caption { text, href } => {
                emit_caption(out, depth, text, href.as_deref());
                *i += 1;
            }
            Node::LabeledText { .. } => {
                let para = nodes[*i].labeled_as_paragraph();
                emit_nodes(
                    out,
                    depth,
                    std::slice::from_ref(para.as_ref()),
                    &mut 0,
                    level,
                );
                *i += 1;
            }
            Node::CheckboxItem { checked, text } => {
                // A `<text>` opening with `<checkbox class="selected|unselected"/>`,
                // the label right after it — one body, so no space between.
                let class = if *checked { "selected" } else { "unselected" };
                out.push(
                    depth,
                    format!(
                        "<text><checkbox class=\"{class}\"/>{}</text>",
                        escape_text(text)
                    ),
                );
                *i += 1;
            }
            Node::Code { language, text, .. } => {
                emit_code(out, depth, language.as_deref(), text, None);
                *i += 1;
            }
            // A CodeFormula-decoded display formula: a `<formula>` element like
            // the inline-math one, with the layout location when present.
            Node::Formula {
                latex, location, ..
            } => {
                if let Some(loc) = location {
                    out.push(depth, "<formula>".to_string());
                    push_location(out, depth + 1, loc);
                    if !latex.is_empty() {
                        out.push(depth + 1, escape_text(latex));
                    }
                    out.push(depth, "</formula>".to_string());
                } else {
                    let body = escape_text(latex);
                    // A `<content>` wrapper (a multi-line formula: a LaTeX
                    // `equation` keeping its `\begin`/`\end`) is an element
                    // child, which minidom prints in block form.
                    if body.starts_with("<content>") {
                        out.push(depth, "<formula>".to_string());
                        out.push(depth + 1, body);
                        out.push(depth, "</formula>".to_string());
                    } else {
                        out.push(depth, format!("<formula>{body}</formula>"));
                    }
                }
                *i += 1;
            }
            Node::PageFurniture {
                footer,
                location,
                text,
            } => {
                let tag = if *footer {
                    "page_footer"
                } else {
                    "page_header"
                };
                out.push(depth, format!("<{tag}>"));
                out.push(depth + 1, "<layer value=\"furniture\"/>".to_string());
                push_location(out, depth + 1, location);
                if !text.is_empty() {
                    out.push(depth + 1, escape_text(text));
                }
                out.push(depth, format!("</{tag}>"));
                *i += 1;
            }
            Node::FurnitureText { label, text } => {
                out.push(depth, format!("<{label}>"));
                out.push(depth + 1, "<layer value=\"furniture\"/>".to_string());
                if !text.is_empty() {
                    out.push(depth + 1, escape_text(text));
                }
                out.push(depth, format!("</{label}>"));
                *i += 1;
            }
            Node::Table(t) => {
                emit_table(out, depth, t);
                *i += 1;
            }
            // Classifier predictions are JSON-only; DocLang keeps the plain
            // `<picture>` shape.
            Node::Picture {
                caption,
                caption_href,
                image,
                ..
            } => {
                emit_picture(
                    out,
                    depth,
                    caption.as_deref(),
                    caption_href.as_deref(),
                    image.as_ref(),
                    None,
                );
                *i += 1;
            }
            Node::Chart {
                kind,
                table,
                caption,
                location,
            } => {
                emit_chart(
                    out,
                    depth,
                    kind,
                    table,
                    caption.as_deref(),
                    location.as_ref(),
                );
                *i += 1;
            }
            Node::DoclangOnly(inner) => {
                let mut j = 0;
                emit_nodes(out, depth, std::slice::from_ref(inner), &mut j, level);
                *i += 1;
            }
            Node::ListItem { level: l, .. } => {
                if *l < level {
                    return; // caller's list continues / closes
                }
                emit_list(out, depth, nodes, i, *l);
            }
            // DocLang has no group element: a group's children render in place.
            // A layered group (a hidden sheet) stamps its layer on each child,
            // exactly as a `Node::Furniture` wrapper around each one would.
            Node::Group {
                children, layer, ..
            } => {
                match layer {
                    Some(l) => {
                        for child in children {
                            emit_furniture(out, depth, *l, child);
                        }
                    }
                    None => {
                        let mut j = 0usize;
                        emit_nodes(out, depth, children, &mut j, 0);
                    }
                }
                *i += 1;
            }
            Node::FieldRegion { items } => {
                emit_field_region(out, depth, items);
                *i += 1;
            }
            // DocLang has no element for a key-value graph.
            Node::KeyValueGraph { .. } => *i += 1,
            Node::InlineGroup {
                unwrapped, runs, ..
            } => {
                emit_inline_group(out, depth, *unwrapped, runs);
                *i += 1;
            }
            Node::Furniture { layer, inner } => {
                emit_furniture(out, depth, *layer, inner);
                *i += 1;
            }
            // A docx comment serializes exactly like any notes-layer text —
            // upstream's DocLang carries no group for it, only the flat item.
            Node::CommentSection { text, .. } => {
                emit_furniture(
                    out,
                    depth,
                    ContentLayer::Notes,
                    &Node::Paragraph { text: text.clone() },
                );
                *i += 1;
            }
            // The annotation is a JSON-only cross-reference; DocLang emits the
            // item itself.
            Node::Commented { inner, .. } => {
                emit_nodes(out, depth, std::slice::from_ref(inner), &mut 0, level);
                *i += 1;
            }
            Node::Located { location, inner } => {
                emit_located(out, depth, location, inner);
                *i += 1;
            }
            // Exact page provenance feeds the JSON export only; DocLang takes
            // its `<location>` tokens from the grid wrapper inside.
            Node::Prov { inner, .. } | Node::Track { inner, .. } => {
                emit_nodes(out, depth, std::slice::from_ref(inner), &mut 0, level);
                *i += 1;
            }
            Node::PageBreak => {
                out.push(depth, "<page_break/>".to_string());
                *i += 1;
            }
            // Page markers feed the JSON export only — DocLang stays unchanged
            // (its geometry travels in the <location> tokens).
            Node::PageInfo { .. } => {
                *i += 1;
            }
            // A link destination (#852) feeds the JSON export only.
            Node::Anchor { .. } => {
                *i += 1;
            }
            // A PDF picture's contained text feeds the JSON export only, so
            // the DocLang output is unchanged. (Upstream's DocLang picture
            // serializer does print a picture's children inside its body.)
            Node::PictureChildren(_) => {
                *i += 1;
            }
            Node::TextDump(text) => {
                emit_text_dump(out, depth, text);
                *i += 1;
            }
        }
    }
}

/// A plain-text dump: one text item holding the whole text — docling-core's
/// `_escape_text` of it (a CDATA section when it holds XML-significant
/// characters, a `<content>` wrapper for its line breaks, which DocLang 0.9
/// would otherwise read as spaces).
fn emit_text_dump(out: &mut Out, depth: i32, text: &str) {
    out.push(depth, format!("<text>{}</text>", escape_text(text)));
}

/// Render a [`Node::InlineGroup`] — docling's `InlineGroup`. Reproduces the
/// reference's `minidom.toprettyxml` layout, which is fully determined by how
/// `writexml` writes text nodes (`indent + data + newl`) once the runs are
/// joined by the `"\n"` record delimiter and the empty-line filter runs:
///
/// * A styled run becomes a nested element (`<italic><bold>…`) via
///   [`emit_styled`]; leaf elements inline, multi-layer ones in block form.
/// * A plain run is a bare text node. Its `"\n"`-delimited leading newline
///   pushes it to column 0 — except the *first* child of a `<text>` wrapper,
///   which has no leading newline and stays indented.
/// * `unwrapped` groups (docling parent is a heading/text) carry no `<text>`;
///   an all-plain wrapped group collapses to a single inline text node with a
///   trailing newline before `</text>`.
fn emit_inline_group(out: &mut Out, depth: i32, unwrapped: bool, runs: &[InlineRun]) {
    let body = inline_runs_markup(runs);
    if unwrapped {
        if !body.is_empty() {
            out.push(depth, body);
        }
    } else {
        out.push(depth, format!("<text>{body}</text>"));
    }
}

/// Emit the child runs of an inline group at `depth` (the body shared by a
/// wrapped `<text>` group and a list item's bare content). A `<content>`-wrapped
/// run is an *element* child → indented; a bare text/CDATA node sits at column 0
/// (its record delimiter's leading newline), except the first child, which has
/// no leading newline and stays indented. Styled/formula runs are elements.
fn emit_inline_runs_body(out: &mut Out, depth: i32, runs: &[InlineRun]) {
    let body = inline_runs_markup(runs);
    if !body.is_empty() {
        out.push(depth, body);
    }
}

/// An inline group's runs concatenated (`DocLangInlineSerializer`: no
/// delimiter — each run carries its own whitespace).
fn inline_runs_markup(runs: &[InlineRun]) -> String {
    runs.iter().map(inline_run_markup).collect()
}

/// One run of an inline group: a plain text node, a `<formula>`, or the
/// run's formatting tags around its escaped text.
fn inline_run_markup(run: &InlineRun) -> String {
    if run.is_plain() {
        escape_text(&run.text)
    } else if run.formula {
        format!("<formula>{}</formula>", escape_text(&run.text))
    } else {
        styled(&style_tags(run), &escape_text(&run.text))
    }
}

/// The DocLang wrapping tags for a run, outermost first. docling applies
/// formatting in the order bold → italic → underline → strikethrough → script,
/// each wrapping the previous result, so the *last* applied is the outermost.
fn style_tags(run: &InlineRun) -> Vec<&'static str> {
    let mut tags = Vec::new();
    match run.script {
        Script::Sub => tags.push("subscript"),
        Script::Super => tags.push("superscript"),
        Script::Baseline => {}
    }
    if run.strike {
        tags.push("strikethrough");
    }
    if run.underline {
        tags.push("underline");
    }
    if run.italic {
        tags.push("italic");
    }
    if run.bold {
        tags.push("bold");
    }
    if run.code {
        tags.push("code");
    }
    tags
}

/// Emit a linear chain of wrapping `tags` (outer→inner) around `inner` text. A
/// single tag renders inline (`<bold>x</bold>`); nested tags render block-form,
/// the innermost (a text child) inline — matching minidom's single-text-child
/// rule at each level.
fn styled(tags: &[&str], inner: &str) -> String {
    let mut s = inner.to_string();
    for tag in tags.iter().rev() {
        s = format!("<{tag}>{s}</{tag}>");
    }
    s
}

/// The `(anchor, uri)` of a text that is nothing but one `[anchor](uri)` link —
/// the shape docling serializes with an `<href uri=…/>` head element instead of
/// inline Markdown.
fn lone_link(text: &str) -> Option<(String, String)> {
    let mut runs = inline_runs(text);
    if runs.len() != 1 {
        return None;
    }
    match runs.pop() {
        Some(Run::Link { anchor, uri }) => Some((anchor, uri)),
        _ => None,
    }
}

/// Render a [`Node::Furniture`] wrapper: the inner element with a
/// `<layer value="{layer}"/>` head (which forces the block form). Headings (the
/// HTML `<title>`, section chrome) and body text (docx comments, nav items) are
/// emitted with the layer token; other nodes fall back to their body rendering.
fn emit_furniture(out: &mut Out, depth: i32, layer: ContentLayer, inner: &Node) {
    let token = format!(
        "<layer value=\"{}\"/>",
        crate::doclang_xml::layer_value(layer)
    );
    match inner {
        Node::Heading { level, text } => {
            let open = if *level <= 1 {
                "heading".to_string()
            } else {
                // Clamp to the heading vocabulary (docling-core#688): deep
                // section headers serialize as level 6 instead of raising.
                format!("heading level=\"{}\"", (*level).min(6))
            };
            out.push(depth, format!("<{open}>"));
            out.push(depth + 1, token);
            out.push(depth + 1, escape_text(text));
            out.push(depth, "</heading>".to_string());
        }
        Node::Paragraph { text } => {
            out.push(depth, "<text>".to_string());
            // A furniture item that is nothing but one link keeps the block
            // `<href>` head the body path gives it (docling's hyperlink
            // post-processing runs before the layer token is written, so the
            // href comes first) — nav chrome like "Jump to content" is exactly
            // this shape.
            match lone_link(text) {
                Some((anchor, uri)) => {
                    out.push(depth + 1, format!("<href uri=\"{}\"/>", attr_escape(&uri)));
                    out.push(depth + 1, token);
                    if !anchor.trim().is_empty() {
                        emit_runs(out, depth + 1, inline_runs(&anchor));
                    }
                }
                None => {
                    out.push(depth + 1, token);
                    if has_styled_runs(text) {
                        // A mixed furniture text (a page header with a bold
                        // run) keeps its runs, as a body text does.
                        emit_runs(out, depth + 1, inline_runs(text));
                    } else {
                        out.push(depth + 1, escape_text(text));
                    }
                }
            }
            out.push(depth, "</text>".to_string());
        }
        // A furniture checkbox (a collapsed nav menu's toggle): the layer token
        // precedes the `<checkbox>` head, like every other furniture element.
        Node::CheckboxItem { checked, text } => {
            let class = if *checked { "selected" } else { "unselected" };
            out.push(
                depth,
                format!(
                    "<text>{token}<checkbox class=\"{class}\"/>{}</text>",
                    escape_text(text)
                ),
            );
        }
        // A located notes text (PPTX speaker notes: docling gives them a zero
        // bbox provenance): layer token first, then the location tokens.
        Node::Located { location, inner } => {
            if let Node::Paragraph { text } = &**inner {
                out.push(depth, "<text>".to_string());
                out.push(depth + 1, token);
                push_location(out, depth + 1, location);
                out.push(depth + 1, escape_text(text));
                out.push(depth, "</text>".to_string());
            } else if let Node::Picture { .. } = &**inner {
                // A located picture on a layer (a hidden PPTX picture): the
                // layer's picture — its image a data URI, as docling's archive
                // keeps every picture off the body — with the location after
                // the layer token.
                let start = out.lines.len();
                emit_furniture(out, depth, layer, inner);
                let at = out.lines[start..]
                    .iter()
                    .position(|(d, line)| *d == depth + 1 && *line == token)
                    .map_or(start + 1, |p| start + p + 1);
                for (k, v) in location.iter().enumerate() {
                    out.lines
                        .insert(at + k, (depth + 1, format!("<location value=\"{v}\"/>")));
                }
            } else {
                // A located heading or table (a hidden PPTX slide's title, a
                // table hidden in the Selection Pane): rendered as the body
                // renders it, the layer token entering its head just ahead of
                // the location — the schema's `element_head` order, after an
                // `<href>`.
                let start = out.lines.len();
                emit_located(out, depth, location, inner);
                if out.lines.len() > start {
                    let at = out.lines[start..]
                        .iter()
                        .position(|(d, line)| *d == depth + 1 && line.starts_with("<location "))
                        .map_or(start + 1, |p| start + p);
                    out.lines.insert(at, (depth + 1, token));
                }
            }
        }
        // A furniture inline group (a mixed-formatting header/footer paragraph):
        // wrapped in `<text>`, with each child run carrying its own layer token
        // (docling stamps the layer on every text item of the group).
        Node::InlineGroup { runs, .. } => {
            // docling-core 2.106: the runs carry no layer of their own (an
            // inline run inherits its host's), the `<text>` wrapper carries
            // the group's.
            out.push(
                depth,
                format!("<text>{token}{}</text>", inline_runs_markup(runs)),
            );
        }
        // A furniture picture (site-chrome logo/banner, header/footer image):
        // the layer token, a caption that carries its own `<href>`/`<layer>`
        // head when the caption is a link, then an embedded-image `<src>` when
        // the picture carries pixels (docling's referenced-asset conversion
        // skips furniture, so the image stays a base64 data URI) — head before
        // body, as the schema orders them (#797).
        Node::Picture {
            caption,
            caption_href,
            image,
            ..
        } => {
            let caption = caption.as_deref().filter(|c| !c.trim().is_empty());
            out.push(depth, "<picture>".to_string());
            out.push(depth + 1, token.clone());
            if let Some(c) = caption {
                out.push(depth + 1, "<caption>".to_string());
                // The caption's link travels either as its own `caption_href`
                // (the backends that keep the target out of the text) or inline
                // in the caption Markdown; both render as an `<href>` head.
                let link = caption_href
                    .clone()
                    .map(|uri| (c.to_string(), uri))
                    .or_else(|| lone_link(c));
                match link {
                    Some((anchor, uri)) => {
                        out.push(depth + 2, format!("<href uri=\"{}\"/>", attr_escape(&uri)));
                        out.push(depth + 2, token);
                        out.push(depth + 2, escape_text(&anchor));
                    }
                    None => {
                        out.push(depth + 2, token);
                        out.push(depth + 2, escape_text(c));
                    }
                }
                out.push(depth + 1, "</caption>".to_string());
            }
            // The `<src>` is the picture's body: after the head's caption.
            if let Some(img) = image
                .as_ref()
                .filter(|_| out.pictures != Pictures::Placeholder)
            {
                // docling embeds the PIL-re-encoded PNG; a JPEG is re-encoded
                // here too, any other source keeps its own type in the URI.
                let (_, uri) = crate::pixel_digest::docling_data_uri(img);
                out.push(depth + 1, format!("<src uri=\"{uri}\"/>"));
            }
            out.push(depth, "</picture>".to_string());
        }
        // An invisible-layer table (a hidden spreadsheet sheet): the layer
        // token precedes the location/cells inside the `<table>`.
        Node::Table(table) => {
            out.push(depth, "<table>".to_string());
            out.push(depth + 1, token);
            if let Some(loc) = &table.location {
                push_location(out, depth + 1, loc);
            }
            emit_table_rows(out, depth, table);
            out.push(depth, "</table>".to_string());
        }
        // JSON-only wrappers: the layer reaches the node inside (a hidden
        // sheet's located table sits in its page provenance).
        Node::Prov { inner, .. } | Node::Track { inner, .. } | Node::Commented { inner, .. } => {
            emit_furniture(out, depth, layer, inner);
        }
        other => {
            let mut i = 0usize;
            emit_nodes(out, depth, std::slice::from_ref(other), &mut i, 0);
        }
    }
}

/// Render a `<picture>` — with optional layout provenance and caption. Empty
/// (no location, no caption) collapses to `<picture></picture>`.
fn emit_picture(
    out: &mut Out,
    depth: i32,
    caption: Option<&str>,
    caption_href: Option<&str>,
    image: Option<&crate::document::PictureImage>,
    location: Option<&[u16; 4]>,
) {
    let caption = caption.filter(|c| !c.trim().is_empty());
    // An image-bearing picture carries a referenced-image `<src>` naming the
    // exported asset (`assets/image_{index:06}_{sha256}.png`), matching docling's
    // referenced-image mode. docling re-encodes every image to PNG through PIL, so
    // the extension is always `.png`, and the digest is over the decoded pixels
    // (`PIL img.tobytes()`) — reproduced exactly for PNG and JPEG sources, the
    // encoded bytes otherwise (pixel_digest.rs).
    // The index counts every body picture, image-less ones too — docling's
    // `_with_pictures_refs` bumps `img_count` per `PictureItem`, not per image.
    let idx = out.pic_index;
    out.pic_index += 1;
    let src = image.and_then(|img| {
        match out.pictures {
            Pictures::Placeholder => return None,
            Pictures::Embedded => return Some(crate::pixel_digest::docling_data_uri(img).1),
            Pictures::Referenced { .. } => {}
        }
        let path = format!(
            "assets/image_{idx:06}_{}.png",
            out.digests.digest(&img.data)
        );
        if out.assets.is_some() {
            let bytes = out.digests.take_png(&img.data);
            if let Some(assets) = out.assets.as_mut() {
                assets.push((path.clone(), bytes));
            }
        }
        Some(path)
    });
    if location.is_none() && caption.is_none() && src.is_none() {
        out.push(depth, "<picture></picture>".to_string());
        return;
    }
    // The element head (location, caption), then the body's `<src>` — the
    // DocLang schema's order, and docling-core's; the caption used to
    // follow the `<src>` (#797).
    out.push(depth, "<picture>".to_string());
    if let Some(loc) = location {
        push_location(out, depth + 1, loc);
    }
    if let Some(c) = caption {
        emit_caption(out, depth + 1, c, caption_href);
    }
    if let Some(s) = src {
        out.push(depth + 1, format!("<src uri=\"{}\"/>", attr_escape(&s)));
    }
    out.push(depth, "</picture>".to_string());
}

/// A `<caption>` — inline when plain text, or block form with an `<href uri=…/>`
/// head + caption text when the caption carries a hyperlink annotation
/// (`href` — an `<a href>` wrapped the image, #328) or *is* a single Markdown
/// link (docling's linked image captions).
fn emit_caption(out: &mut Out, depth: i32, text: &str, href: Option<&str>) {
    if let Some(uri) = href {
        out.push(depth, "<caption>".to_string());
        out.push(depth + 1, format!("<href uri=\"{}\"/>", attr_escape(uri)));
        out.push(depth + 1, escape_text(text));
        out.push(depth, "</caption>".to_string());
        return;
    }
    if let Some(Run::Link { anchor, uri }) = inline_runs(text).into_iter().next() {
        if inline_runs(text).len() == 1 {
            out.push(depth, "<caption>".to_string());
            out.push(depth + 1, format!("<href uri=\"{}\"/>", attr_escape(&uri)));
            out.push(depth + 1, escape_text(&anchor));
            out.push(depth, "</caption>".to_string());
            return;
        }
    }
    if has_styled_runs(text) {
        // A mixed caption (docling-core#865) keeps its runs.
        out.push(depth, "<caption>".to_string());
        emit_runs(out, depth + 1, inline_runs(text));
        out.push(depth, "</caption>".to_string());
        return;
    }
    out.push(depth, format!("<caption>{}</caption>", escape_text(text)));
}

/// If `text` is a single `[anchor](uri)` Markdown link, return just `anchor`;
/// otherwise return `text` unchanged. Used when the link's uri rides in a list
/// item's `<href>` head, so the content keeps only the anchor text.
fn strip_lone_link(text: &str) -> Cow<'_, str> {
    if let Some(rest) = text.strip_prefix('[') {
        if let Some(close) = rest.find("](") {
            if rest.ends_with(')') {
                let anchor = &rest[..close];
                let uri = &rest[close + 2..rest.len() - 1];
                if !anchor.contains(['[', ']']) && !uri.contains(['(', ')']) {
                    return Cow::Owned(anchor.to_string());
                }
            }
        }
    }
    Cow::Borrowed(text)
}

/// Render a [`Node::Located`] wrapper: the inner element with its `<location>`
/// tokens as the first children.
fn emit_located(out: &mut Out, depth: i32, location: &[u16; 4], inner: &Node) {
    match inner {
        Node::Heading { level, text } => {
            let open = if *level <= 1 {
                "heading".to_string()
            } else {
                // Clamp to the heading vocabulary (docling-core#688): deep
                // section headers serialize as level 6 instead of raising.
                format!("heading level=\"{}\"", (*level).min(6))
            };
            emit_text_element(out, depth, &open, "heading", text, Some(location));
        }
        Node::Paragraph { text } => {
            emit_text_element(out, depth, "text", "text", text, Some(location));
        }
        Node::LabeledText { .. } => {
            emit_located(out, depth, location, &inner.labeled_as_paragraph())
        }
        Node::Picture {
            caption,
            caption_href,
            image,
            ..
        } => {
            emit_picture(
                out,
                depth,
                caption.as_deref(),
                caption_href.as_deref(),
                image.as_ref(),
                Some(location),
            );
        }
        Node::Table(t) => {
            // The wrapper's location takes precedence over any on the table.
            let mut t = t.clone();
            t.location = Some(*location);
            emit_table(out, depth, &t);
        }
        Node::Code { language, text, .. } => {
            emit_code(out, depth, language.as_deref(), text, Some(location));
        }
        // Other node kinds carry no location today — render them as-is.
        // (Located list items are routed to emit_list by emit_nodes so they
        // still group into one `<list>`.)
        other => {
            let mut i = 0usize;
            emit_nodes(out, depth, std::slice::from_ref(other), &mut i, 0);
        }
    }
}

fn emit_list(out: &mut Out, depth: i32, nodes: &[Node], i: &mut usize, level: u8) {
    // The list kind follows the first item's DocLang overlay when it has one
    // (a docx multilevel item is a Markdown bullet but a DocLang ordered item).
    let ordered = match &nodes[*i] {
        Node::ListItem { ordered, dclx, .. } => dclx.as_ref().map_or(*ordered, |d| d.ordered),
        _ => false,
    };
    let open = if ordered {
        "<list class=\"ordered\">"
    } else {
        "<list>"
    };
    out.push(depth, open.to_string());
    let start = *i;
    while *i < nodes.len() {
        match &nodes[*i] {
            Node::ListItem {
                level: l,
                text,
                marker,
                ordered: o,
                number,
                first_in_list,
                location,
                dclx,
                href,
                layer,
            } if *l == level => {
                // The DocLang overlay wins over the flat Markdown fields for the
                // list kind and marker (see `ListItemDclx`).
                let eff_marker = dclx.as_ref().map_or(marker.as_ref(), |d| d.marker.as_ref());
                // A new sibling list at this depth closes this one (the caller
                // re-opens): the backend flagged a fresh list — the one
                // boundary rule shared with the Markdown and JSON serializers
                // (#385; the kind-flip and number-gap guesses are gone).
                if *i != start && *first_in_list {
                    break;
                }
                // docling wraps a list item's content in `<text>` when a nested
                // list follows it *anywhere* inside the same `<list>` — its
                // `_list_item_has_segment_siblings` scans the parent group's
                // children after the item; a plain item with no later nested
                // list stays bare.
                let has_nested = {
                    let mut found = false;
                    let mut j = *i + 1;
                    while let Some(Node::ListItem {
                        level: nl,
                        first_in_list: nf,
                        ..
                    }) = nodes.get(j)
                    {
                        if *nl > level {
                            found = true;
                            break;
                        }
                        if *nl < level {
                            break;
                        }
                        // The same run-break rule as the main loop: a sibling
                        // list at this depth ends this `<list>` element.
                        if *nf {
                            break;
                        }
                        j += 1;
                    }
                    found
                };
                // An enumeration marker (HTML/DOCX ordered items) rides inside
                // the `<ldiv>`; without one the delimiter is self-closing.
                match eff_marker {
                    Some(m) => {
                        out.push(depth + 1, "<ldiv>".to_string());
                        out.push(depth + 2, format!("<marker>{}</marker>", escape_text(m)));
                        out.push(depth + 1, "</ldiv>".to_string());
                    }
                    None => out.push(depth + 1, "<ldiv/>".to_string()),
                }
                // docling emits an `<href>` head only when the item's whole
                // content is a lone link (`[anchor](uri)`); a mixed item
                // (`text [anchor](uri) …`) keeps the anchor inline with no
                // head. A non-body layer always rides in the head.
                let stripped = strip_lone_link(text);
                let eff_href = href
                    .as_deref()
                    .filter(|_| matches!(stripped, Cow::Owned(_)));
                let head = dclx.is_none() && (eff_href.is_some() || layer.is_some());
                // Layout provenance (PPTX shapes): the four `<location>` tokens
                // follow the `<ldiv>` and precede the item's content, matching
                // docling's element head inside the list — after an `<href>`
                // or `<layer>` token when the item has one.
                if let Some(loc) = location.as_ref().filter(|_| !head) {
                    push_location(out, depth + 1, loc);
                }
                match dclx {
                    // Structured DocLang content (equations/formatting): the runs
                    // render directly. The same `<text>` wrap rule as plain items
                    // applies — a nested list following the item wraps its
                    // content (docling's `_list_item_has_segment_siblings`).
                    Some(d) if !d.runs.is_empty() => {
                        if has_nested {
                            out.push(depth + 1, "<text>".to_string());
                            emit_inline_runs_body(out, depth + 2, &d.runs);
                            out.push(depth + 1, "</text>".to_string());
                        } else {
                            emit_inline_runs_body(out, depth + 1, &d.runs);
                        }
                    }
                    // A clean-text override (multilevel numbering) re-parses like
                    // a normal item but from the overlay's text.
                    Some(d) => emit_list_item_content(out, depth + 1, &d.text, has_nested),
                    None => {
                        if head {
                            let content: &str = if eff_href.is_some() {
                                stripped.as_ref()
                            } else {
                                text.as_str()
                            };
                            emit_list_item_with_head(
                                out,
                                depth + 1,
                                content,
                                has_nested,
                                eff_href,
                                *layer,
                                location.as_ref(),
                            );
                        } else {
                            emit_list_item_content(out, depth + 1, text, has_nested);
                        }
                    }
                }
                *i += 1;
            }
            Node::ListItem { level: l, .. } if *l > level => {
                emit_list(out, depth + 1, nodes, i, *l);
            }
            // An empty paragraph between two items of the *same* list run is
            // absorbed (docling deletes the empty text it added on close when
            // it reuses the ListGroup for the same numId). When the next item
            // starts a *new* list (the backend's fresh-list flag), no reuse
            // happens and the empty text survives.
            Node::Paragraph { text }
                if text.is_empty()
                    && matches!(
                        nodes.get(*i + 1),
                        Some(Node::ListItem { level: nl, first_in_list: nf, .. })
                            if *nl > level || (*nl == level && !*nf)
                    ) =>
            {
                *i += 1;
            }
            _ => break,
        }
    }
    out.push(depth, "</list>".to_string());
}

/// Render a list item's content after its `<ldiv/>`. docling wraps the content
/// in `<text>` when the item has a "segment sibling" — a nested list following
/// it — and otherwise emits it bare (a plain item as indented text, a formatted
/// one as its inline elements). (A uniformly-formatted item that docling stores
/// with direct formatting rather than an inline group is also wrapped, but that
/// backend-structural distinction isn't recoverable from the flat model.)
/// A list item whose head carries an `<href>` and/or `<layer>` (HTML links /
/// site chrome). Bare content puts the head right after the `<ldiv>` then the
/// anchor text; wrapped content (a `<text>` element, e.g. an item with a nested
/// sublist) puts the head *inside* the `<text>`.
fn emit_list_item_with_head(
    out: &mut Out,
    depth: i32,
    text: &str,
    has_nested: bool,
    href: Option<&str>,
    layer: Option<ContentLayer>,
    location: Option<&[u16; 4]>,
) {
    // docling's head order: the link, the layer, then the location.
    let head = |out: &mut Out, d: i32| {
        if let Some(uri) = href {
            out.push(d, format!("<href uri=\"{}\"/>", attr_escape(uri)));
        }
        if let Some(l) = layer {
            out.push(
                d,
                format!("<layer value=\"{}\"/>", crate::doclang_xml::layer_value(l)),
            );
        }
        if let Some(loc) = location {
            push_location(out, d, loc);
        }
    };
    if has_nested {
        out.push(depth, "<text>".to_string());
        head(out, depth + 1);
        emit_runs(out, depth + 1, inline_runs(text));
        out.push(depth, "</text>".to_string());
    } else {
        head(out, depth);
        emit_runs(out, depth, inline_runs(text));
    }
}

fn emit_list_item_content(out: &mut Out, depth: i32, text: &str, has_nested: bool) {
    // docling models an HTML list item's inline content as an InlineGroup: each
    // text node / inline element becomes a separate child, links flatten to
    // their anchor (the href is dropped in inline scope), and the children are
    // rendered on their own lines. Re-parse the Markdown markers into runs and
    // mirror that layout.
    let runs = inline_runs_from_markdown(text);
    let single_plain = runs.len() <= 1 && runs.first().is_none_or(|r| r.is_plain());
    if single_plain {
        if has_nested {
            emit_text_element(out, depth, "text", "text", text, None);
        } else if !text.trim().is_empty() {
            // The original text, not the re-parsed run: an unformatted item keeps
            // its raw boundary whitespace (docling stores the backend's run text
            // verbatim, and the serializer preserves it with `<content>`).
            emit_text_node(out, depth, text);
        }
    } else if has_nested {
        emit_inline_group(out, depth, false, &runs);
    } else {
        // Bare multi-segment item: the runs render at the item's own depth, the
        // first indented and the rest column-0 (minidom's text-child layout).
        emit_inline_runs_body(out, depth, &runs);
    }
}

fn emit_field_region(out: &mut Out, depth: i32, items: &[FieldItem]) {
    out.push(depth, "<field_region>".to_string());
    for item in items {
        out.push(depth + 1, "<field_item>".to_string());
        if let Some(m) = item.marker.as_ref().filter(|s| !s.is_empty()) {
            out.push(depth + 2, format!("<marker>{}</marker>", escape_text(m)));
        }
        if let Some(k) = item.key.as_ref().filter(|s| !s.is_empty()) {
            out.push(depth + 2, format!("<key>{}</key>", escape_text(k)));
        }
        if let Some(v) = item.value.as_ref().filter(|s| !s.is_empty()) {
            out.push(depth + 2, format!("<value>{}</value>", escape_text(v)));
        }
        out.push(depth + 1, "</field_item>".to_string());
    }
    out.push(depth, "</field_region>".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docling-core#687 (#253): XML-1.0-illegal characters render as visible
    /// `[U+XXXX]` markers so the archive stays parseable; tab/LF/CR are legal
    /// and pass through.
    #[test]
    fn xml_illegal_characters_become_visible_markers() {
        let doclang = export_to_doclang(&[Node::Paragraph {
            text: "Before break\u{0B}After\u{FFFF} tab\tok".into(),
        }]);
        assert!(
            doclang.contains("Before break[U+000B]After[U+FFFF] tab\tok"),
            "got:\n{doclang}"
        );
    }

    /// docling-core#689 (#253): a literal `]]>` inside CDATA-escaped text
    /// splits across adjacent CDATA sections, preserving the exact characters
    /// instead of producing unparseable XML.
    #[test]
    fn cdata_closing_delimiter_splits_sections() {
        let doclang = export_to_doclang(&[Node::Paragraph {
            text: "a]]>b & c".into(),
        }]);
        assert!(
            doclang.contains("<![CDATA[a]]]]><![CDATA[>b & c]]>"),
            "got:\n{doclang}"
        );
    }

    /// docling-core#688 (#253): heading levels past the vocabulary clamp to 6
    /// instead of emitting an out-of-range token.
    #[test]
    fn deep_heading_levels_clamp_to_six() {
        let doclang = export_to_doclang(&[
            Node::Heading {
                level: 6,
                text: "Deep".into(),
            },
            Node::Heading {
                level: 42,
                text: "Deeper".into(),
            },
        ]);
        assert!(doclang.contains("<heading level=\"6\">Deep</heading>"));
        assert!(
            doclang.contains("<heading level=\"6\">Deeper</heading>"),
            "got:\n{doclang}"
        );
    }

    fn red_pixel() -> crate::document::PictureImage {
        crate::document::PictureImage {
            mimetype: "image/png".into(),
            width: 1,
            height: 1,
            data: crate::base64::decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC",
            )
            .unwrap(),
            dpi: 72,
            display_size: None,
        }
    }

    /// The position of each needle in `hay`, in the order given.
    fn positions(hay: &str, needles: &[&str]) -> Vec<usize> {
        needles
            .iter()
            .map(|n| hay.find(n).unwrap_or_else(|| panic!("{n:?} in:\n{hay}")))
            .collect()
    }

    /// #797: the DocLang schema's `element_head` is the location block,
    /// then `caption`; the element's content (a table's cells, a picture's
    /// `<src>`) follows the head. docling-core writes it in that order too.
    #[test]
    fn captions_follow_the_location_and_precede_the_content() {
        let table = Table {
            rows: vec![vec!["a".into(), "b".into()]],
            caption: Some("Table 1. Caption".into()),
            ..Default::default()
        };
        let doclang = export_to_doclang(&[Node::Located {
            location: [54, 205, 247, 308],
            inner: Box::new(Node::Table(table)),
        }]);
        let p = positions(
            &doclang,
            &[
                "<table>",
                "<location value=\"308\"/>",
                "<caption>Table 1. Caption</caption>",
                "<ched/>",
            ],
        );
        assert!(p.windows(2).all(|w| w[0] < w[1]), "{doclang}");

        let picture = |caption: &str| Node::Picture {
            caption: Some(caption.into()),
            caption_href: None,
            image: Some(red_pixel()),
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        };
        let doclang = export_to_doclang(&[Node::Located {
            location: [61, 351, 267, 460],
            inner: Box::new(picture("Figure 7-26. Self-locking nuts.")),
        }]);
        let p = positions(
            &doclang,
            &[
                "<picture>",
                "<location value=\"460\"/>",
                "<caption>Figure 7-26.",
                "<src uri=",
            ],
        );
        assert!(p.windows(2).all(|w| w[0] < w[1]), "{doclang}");

        // A furniture picture: the layer token, the caption, then the `<src>`.
        let doclang = export_to_doclang(&[Node::Furniture {
            layer: ContentLayer::Furniture,
            inner: Box::new(picture("Logo")),
        }]);
        let p = positions(
            &doclang,
            &[
                "<picture>",
                "<layer value=\"furniture\"/>",
                "<caption>",
                "<src uri=",
            ],
        );
        assert!(p.windows(2).all(|w| w[0] < w[1]), "{doclang}");
    }

    #[test]
    fn located_heading_emits_location_tokens_in_block_form() {
        let doclang = export_to_doclang(&[Node::Located {
            location: [44, 170, 340, 386],
            inner: Box::new(Node::Heading {
                level: 1,
                text: "X-Library".into(),
            }),
        }]);
        assert!(
            doclang.contains(
                "<heading>\n    <location value=\"44\"/>\n    <location value=\"170\"/>\n    \
                 <location value=\"340\"/>\n    <location value=\"386\"/>\n    X-Library\n  </heading>"
            ),
            "got:\n{doclang}"
        );
    }

    fn code(language: Option<&str>, text: &str) -> String {
        export_to_doclang(&[Node::Code {
            language: language.map(String::from),
            text: text.into(),
            orig: None,
            pretty: None,
        }])
    }

    #[test]
    fn code_with_language_emits_linguist_label_block_form() {
        // Fence language folds through the docling value onto the Linguist
        // key: a head element, so the body goes on a line of its own.
        assert_eq!(
            code(Some("python"), "print(\"Hello world!\")"),
            "<doclang version=\"0.9\">\n  <code>\n    <label value=\"Python\"/>\n    \
             <![CDATA[print(\"Hello world!\")]]>\n  </code>\n</doclang>"
        );
        // Aliased label: bash -> Shell.
        assert!(code(Some("bash"), "ls -la").contains("<label value=\"Shell\"/>"));
    }

    fn plain(text: &str) -> InlineRun {
        InlineRun {
            text: text.into(),
            ..Default::default()
        }
    }
    fn bold(text: &str) -> InlineRun {
        InlineRun {
            text: text.into(),
            bold: true,
            ..Default::default()
        }
    }
    fn ig(unwrapped: bool, runs: Vec<InlineRun>) -> String {
        let body = export_to_doclang(&[Node::InlineGroup {
            unwrapped,
            runs,
            md_text: String::new(),
        }]);
        // strip the <doclang> envelope for readable assertions
        body.trim_start_matches("<doclang version=\"0.9\">\n")
            .trim_end_matches("\n</doclang>")
            .to_string()
    }

    /// docling-core 2.106's `DocLangInlineSerializer`: the runs concatenated
    /// as they are (each carries its own whitespace), the body on one line.
    #[test]
    fn inline_group_matches_reference_layout() {
        assert_eq!(
            ig(
                false,
                vec![plain("This is a "), bold("bold"), plain(" example")]
            ),
            "  <text>This is a <bold>bold</bold> example</text>"
        );
        // Unwrapped (its parent is a text item): the runs alone.
        assert_eq!(
            ig(
                true,
                vec![
                    plain("aa "),
                    bold("bb"),
                    plain(" cc "),
                    bold("dd"),
                    plain(" ee")
                ]
            ),
            "  aa <bold>bb</bold> cc <bold>dd</bold> ee"
        );
        assert_eq!(
            ig(false, vec![plain("aa"), plain("bb")]),
            "  <text>aabb</text>"
        );
        assert_eq!(ig(false, vec![plain("aa")]), "  <text>aa</text>");
        assert_eq!(
            ig(false, vec![bold("bb")]),
            "  <text><bold>bb</bold></text>"
        );
        // A run's edge space beside the block edge needs its `<content>`.
        assert_eq!(
            ig(false, vec![bold("bb"), plain(" tail ")]),
            "  <text><bold>bb</bold><content> tail </content></text>"
        );
    }

    #[test]
    fn nested_styles_wrap_outermost_last_applied() {
        let bi = InlineRun {
            text: "bi".into(),
            bold: true,
            italic: true,
            ..Default::default()
        };
        // italic (applied after bold) is outermost.
        assert_eq!(ig(true, vec![bi]), "  <italic><bold>bi</bold></italic>");
        let sub = InlineRun {
            text: "2".into(),
            script: Script::Sub,
            ..Default::default()
        };
        assert_eq!(ig(true, vec![sub]), "  <subscript>2</subscript>");
    }

    #[test]
    fn furniture_heading_gets_layer_head() {
        let out = export_to_doclang(&[Node::Furniture {
            layer: ContentLayer::Furniture,
            inner: Box::new(Node::Heading {
                level: 1,
                text: "Anchor Links Test".into(),
            }),
        }]);
        assert_eq!(
            out,
            "<doclang version=\"0.9\">\n  <heading>\n    <layer value=\"furniture\"/>\n    Anchor Links Test\n  </heading>\n</doclang>"
        );
    }

    /// A hidden PPTX slide's title, table and picture: the layer token heads
    /// the element ahead of its location, as docling-core writes them.
    #[test]
    fn located_furniture_keeps_layer_and_location() {
        let hidden = |inner: Node| Node::Furniture {
            layer: ContentLayer::Invisible,
            inner: Box::new(Node::Located {
                location: [1, 2, 3, 4],
                inner: Box::new(inner),
            }),
        };
        let out = export_to_doclang(&[
            hidden(Node::Heading {
                level: 1,
                text: "Hidden slide".into(),
            }),
            hidden(Node::Table(Table {
                rows: vec![vec!["a".into()]],
                ..Table::default()
            })),
            hidden(Node::Picture {
                caption: None,
                caption_href: None,
                image: None,
                classification: None,
                description: None,
                caption_parent: Default::default(),
                caption_location: None,
            }),
        ]);
        // DocLang has no invisible layer: docling-core maps it to background.
        let head = "    <layer value=\"background\"/>\n    <location value=\"1\"/>\n    <location value=\"2\"/>\n    <location value=\"3\"/>\n    <location value=\"4\"/>\n";
        assert_eq!(
            out,
            format!(
                "<doclang version=\"0.9\">\n  <heading>\n{head}    Hidden slide\n  </heading>\n  <table>\n{head}    <ched/>\n    a\n    <nl/>\n  </table>\n  <picture>\n{head}  </picture>\n</doclang>"
            )
        );
    }

    #[test]
    fn furniture_checkbox_keeps_its_layer() {
        // A collapsed nav menu's toggle: the layer token precedes `<checkbox>`.
        let out = export_to_doclang(&[Node::Furniture {
            layer: ContentLayer::Furniture,
            inner: Box::new(Node::CheckboxItem {
                checked: false,
                text: "Main menu".into(),
            }),
        }]);
        assert_eq!(
            out,
            "<doclang version=\"0.9\">\n  <text>\n    <layer value=\"furniture\"/>\n    <checkbox class=\"unselected\"/>Main menu\n  </text>\n</doclang>"
        );
    }

    #[test]
    fn furniture_text_that_is_one_link_gets_an_href_head() {
        // docling's hyperlink post-processing runs before the layer token, so
        // the `<href>` comes first — site chrome like "Jump to content".
        let out = export_to_doclang(&[Node::Furniture {
            layer: ContentLayer::Furniture,
            inner: Box::new(Node::Paragraph {
                text: "[Jump to content](#bodyContent)".into(),
            }),
        }]);
        assert_eq!(
            out,
            "<doclang version=\"0.9\">\n  <text>\n    <href uri=\"#bodyContent\"/>\n    <layer value=\"furniture\"/>\n    Jump to content\n  </text>\n</doclang>"
        );
        // Text around the link is not the lone-link shape: layer token first.
        let mixed = export_to_doclang(&[Node::Furniture {
            layer: ContentLayer::Furniture,
            inner: Box::new(Node::Paragraph {
                text: "see [here](#x) now".into(),
            }),
        }]);
        assert!(
            mixed.contains("<layer value=\"furniture\"/>\n    see [here](#x) now"),
            "{mixed}"
        );
    }

    #[test]
    fn link_destinations_balance_their_parentheses() {
        // CommonMark nests parentheses in a bare destination — Wikipedia's
        // interwiki links (`/wiki/Houad_(evn)`) are exactly this case, and
        // stopping at the first `)` used to leak the tail into the text.
        let out = export_to_doclang(&[Node::Paragraph {
            text: "[Brezhoneg](https://br.wikipedia.org/wiki/Houad_(evn))".into(),
        }]);
        assert_eq!(
            out,
            "<doclang version=\"0.9\">\n  <text>\n    <href uri=\"https://br.wikipedia.org/wiki/Houad_(evn)\"/>\n    Brezhoneg\n  </text>\n</doclang>"
        );
    }

    #[test]
    fn right_to_left_text_is_wrapped_in_rtl() {
        // docling-core's `serialize_rtl`: a text whose direction resolves to
        // right-to-left carries the `<rtl>` marker.
        let out = export_to_doclang(&[
            Node::Paragraph {
                text: "العربية".into(),
            },
            Node::Paragraph {
                text: "Aragonés".into(),
            },
        ]);
        assert_eq!(
            out,
            "<doclang version=\"0.9\">\n  <text><rtl>العربية</rtl></text>\n  <text>Aragonés</text>\n</doclang>"
        );
        // The rule is docling's: a strong-RTL first character, or an RTL
        // majority. A Latin lead with a minority of Hebrew stays left-to-right.
        assert!(is_rtl("עברית"));
        assert!(!is_rtl("Hebrew: עב"));
        assert!(!is_rtl(""));
        // Arabic-Indic digits are `AN`/`EN`, not strong RTL.
        assert!(!is_rtl("١٢٣"));
    }

    #[test]
    fn text_dump_is_one_literal_text() {
        let out = export_to_doclang(&[Node::TextDump("a \"b\"\n*c*".into())]);
        assert_eq!(
            out,
            "<doclang version=\"0.9\">\n  <text><content><![CDATA[a \"b\"\n*c*]]></content></text>\n</doclang>"
        );
    }

    #[test]
    fn code_without_language_stays_inline_and_unlabeled() {
        assert_eq!(
            code(None, "print(\"Hi!\")"),
            "<doclang version=\"0.9\">\n  <code><![CDATA[print(\"Hi!\")]]></code>\n</doclang>"
        );
        // Unknown fence language: no label, still inline.
        assert!(!code(Some("brainfuck"), "+++.").contains("<label"));
    }
}
