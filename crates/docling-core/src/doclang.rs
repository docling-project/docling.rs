//! DocLang XML serialization (`export_to_doclang`) — the markup inside a
//! `.dclx` archive, mirroring docling-core's `DocLangDocSerializer` with
//! `DocLangParams()` defaults (version 0.7, 2-space pretty indent, AUTO
//! CDATA/content wrapping, placeholder image mode → no `<src>` data).
//!
//! The Python reference builds a minified string, round-trips it through
//! `xml.dom.minidom.toprettyxml`, filters empty lines and re-expands
//! self-closing forms of non-self-closing tags. For the subset our `Node`
//! model produces, that pipeline's output is reproduced *directly*: an
//! element whose content is a single text/CDATA run renders inline
//! (`<text>abc</text>`), anything with element children renders as an
//! indented block. See docs in the .dclx conformance PR for the full spec.
//!
//! Inline formatting: our model bakes bold/italic/code/links into the text as
//! docling-legacy Markdown markers; [`inline_runs`] re-parses those into the
//! structural `<bold>`/`<italic>`/`<code>` elements DocLang expects.

use crate::document::{ContentLayer, FieldItem, InlineRun, Node, Script, Table};
use std::borrow::Cow;

const INDENT: &str = "  ";

/// Rendered fragments: (indent depth, content, newline-after). minidom writes
/// a CDATA child with no indent and no trailing newline, so the next fragment
/// (usually the parent's closing tag, at its own indent) lands on the same
/// line — `newline` false reproduces that glue.
struct Out {
    lines: Vec<(i32, String, bool)>,
    /// Running index for exported image assets (`assets/image_{NNNNNN}_…`),
    /// incremented per image-bearing picture in document order.
    pic_index: usize,
}

impl Out {
    fn push(&mut self, depth: i32, s: impl Into<String>) {
        self.lines.push((depth, s.into(), true));
    }

    /// A fragment with no indent and no trailing newline (CDATA glue).
    fn push_glue(&mut self, s: impl Into<String>) {
        self.lines.push((0, s.into(), false));
    }

    fn finish(self) -> String {
        let mut s = String::new();
        for (d, line, nl) in self.lines {
            // minidom writes every node's indentation prefix; only a glued
            // fragment (CDATA/plain text child) suppresses the *newline*, so
            // the following node's indent lands on the same line. Emitting the
            // indent unconditionally reproduces that (glue fragments carry
            // depth 0, contributing none).
            for _ in 0..d {
                s.push_str(INDENT);
            }
            s.push_str(&line);
            if nl {
                s.push('\n');
            }
        }
        // The reference's empty-line filter drops the trailing blank, so the
        // serialized text carries no final newline; the archive writer adds
        // exactly one back.
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }
}

/// Reverse the Markdown-oriented escaping backends bake into node text
/// (`&amp;`/`&lt;`/`&gt;` and `\_`), recovering the raw text DocLang serializes:
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
fn escape_text(text: &str) -> String {
    let raw = unescape_stored(text);
    let text = raw.as_ref();
    let needs_cdata = text.contains(['"', '\'', '&', '<', '>']);
    let needs_content = text != text.trim() || text.contains('\n');
    let mut t = if needs_cdata {
        format!("<![CDATA[{text}]]>")
    } else {
        text.to_string()
    };
    if needs_content {
        t = format!("<content>{t}</content>");
    }
    t
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
    let bytes: Vec<char> = text.chars().collect();
    let n = bytes.len();
    let mut i = 0;
    let find = |open: usize, pat: &str| -> Option<usize> {
        let hay: String = bytes[open..].iter().collect();
        hay.find(pat).map(|p| open + hay[..p].chars().count())
    };
    while i < n {
        let rest: String = bytes[i..].iter().collect();
        let take = |runs: &mut Vec<Run>, plain: &mut String, r: Run| {
            if !plain.is_empty() {
                runs.push(Run::Plain(std::mem::take(plain)));
            }
            runs.push(r);
        };
        if rest.starts_with("***") {
            if let Some(end) = find(i + 3, "***") {
                let inner: String = bytes[i + 3..end].iter().collect();
                take(&mut runs, &mut plain, Run::BoldItalic(inner));
                i = end + 3;
                continue;
            }
        }
        if rest.starts_with("**") {
            if let Some(end) = find(i + 2, "**") {
                let inner: String = bytes[i + 2..end].iter().collect();
                take(&mut runs, &mut plain, Run::Bold(inner));
                i = end + 2;
                continue;
            }
        }
        if rest.starts_with('*') && !rest.starts_with("**") {
            if let Some(end) = find(i + 1, "*") {
                let inner: String = bytes[i + 1..end].iter().collect();
                if !inner.is_empty() {
                    take(&mut runs, &mut plain, Run::Italic(inner));
                    i = end + 1;
                    continue;
                }
            }
        }
        if rest.starts_with('`') {
            if let Some(end) = find(i + 1, "`") {
                let inner: String = bytes[i + 1..end].iter().collect();
                take(&mut runs, &mut plain, Run::Code(inner));
                i = end + 1;
                continue;
            }
        }
        if rest.starts_with('[') {
            if let (Some(close), true) = (find(i + 1, "]("), true) {
                if let Some(endp) = find(close + 2, ")") {
                    let anchor: String = bytes[i + 1..close].iter().collect();
                    let uri: String = bytes[close + 2..endp].iter().collect();
                    take(&mut runs, &mut plain, Run::Link { anchor, uri });
                    i = endp + 1;
                    continue;
                }
            }
        }
        plain.push(bytes[i]);
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
    let find = |open: usize, pat: &str| -> Option<usize> {
        let hay: String = chars[open..].iter().collect();
        hay.find(pat).map(|p| open + hay[..p].chars().count())
    };
    let sub = |a: usize, b: usize| -> Vec<char> { chars[a..b].to_vec() };
    while i < n {
        let rest: String = chars[i..].iter().collect();
        // Longest markers first so `**`/`***` aren't mis-split.
        if rest.starts_with("***") {
            if let Some(end) = find(i + 3, "***") {
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
        if rest.starts_with("**") {
            if let Some(end) = find(i + 2, "**") {
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
        if rest.starts_with('*') {
            if let Some(end) = find(i + 1, "*") {
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
        if rest.starts_with("~~") {
            if let Some(end) = find(i + 2, "~~") {
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
        if rest.starts_with('`') {
            if let Some(end) = find(i + 1, "`") {
                flush_md_plain(&mut plain, &style, out);
                let inner: String = sub(i + 1, end).iter().collect();
                let inner = inner.trim();
                if !inner.is_empty() {
                    out.push(InlineRun {
                        text: inner.to_string(),
                        code: true,
                        ..style.clone()
                    });
                }
                i = end + 1;
                continue;
            }
        }
        if rest.starts_with('[') {
            if let Some(close) = find(i + 1, "](") {
                if let Some(endp) = find(close + 2, ")") {
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
        push_location(out, depth + 1, loc);
        if !text.is_empty() {
            emit_runs(out, depth + 1, inline_runs(text));
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
        let body = escape_text(text);
        // A `<content>` wrapper is an *element* child, so minidom renders the
        // wrapper in block form; bare text / CDATA is a single text child and
        // stays inline.
        if body.starts_with("<content>") {
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

fn emit_runs(out: &mut Out, depth: i32, runs: Vec<Run>) {
    for run in runs {
        match run {
            Run::Plain(t) => {
                let t = t.trim_matches('\n');
                if !t.is_empty() {
                    emit_text_node(out, depth, t);
                }
            }
            Run::Bold(t) => out.push(depth, format!("<bold>{}</bold>", escape_text(&t))),
            Run::Italic(t) => out.push(depth, format!("<italic>{}</italic>", escape_text(&t))),
            Run::BoldItalic(t) => {
                out.push(depth, "<italic>".to_string());
                out.push(depth + 1, format!("<bold>{}</bold>", escape_text(&t)));
                out.push(depth, "</italic>".to_string());
            }
            Run::Code(t) => out.push(depth, format!("<code>{}</code>", escape_text(&t))),
            Run::Link { anchor, .. } => {
                // Inline scope: DocLang drops the target, keeps the anchor.
                if !anchor.is_empty() {
                    emit_text_node(out, depth, &anchor);
                }
            }
        }
    }
}

/// A text node in block (element-children) context: plain data indents like
/// any child; a `<content>` wrapper is a normal element; bare CDATA glues to
/// the next fragment with no indent/newline (minidom's CDATA rule).
fn emit_text_node(out: &mut Out, depth: i32, text: &str) {
    let e = escape_text(text);
    if e.starts_with("<![CDATA[") {
        out.push_glue(e);
    } else {
        out.push(depth, e);
    }
}

/// Map a docling `CodeLanguageLabel` value (as stored in [`Node::Code::language`]
/// and the JSON export) to the DocLang recommended (Linguist) label. Returns
/// `None` for unknown/absent languages — matching docling's AUTO `label_mode`,
/// which omits the `<label>` when the resolved label would be `undefined`.
fn code_lang_label(lang: &str) -> Option<&'static str> {
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

/// Emit a `<code>` element. With a resolved language, a `<label value=…/>` head
/// forces the block form (matching docling); the code text follows as a text
/// child (CDATA/plain glued to the closing tag, `<content>`-wrapped text on its
/// own line). Without a language, single-fragment text renders inline.
fn emit_code(
    out: &mut Out,
    depth: i32,
    language: Option<&str>,
    text: &str,
    location: Option<&[u16; 4]>,
) {
    let label = language.and_then(code_lang_label);
    let escaped = escape_text(text);
    let is_content_element = escaped.starts_with("<content>");
    // Layout provenance forces the block form: `<location>` tokens follow the
    // opening `<code>`, before the (optional) label and the code body.
    if let Some(loc) = location {
        out.push(depth, "<code>".to_string());
        push_location(out, depth + 1, loc);
        if let Some(l) = label {
            out.push(depth + 1, format!("<label value=\"{}\"/>", attr_escape(l)));
        }
        if is_content_element {
            out.push(depth + 1, escaped);
        } else {
            out.push_glue(escaped);
        }
        out.push(depth, "</code>".to_string());
        return;
    }
    match (label, is_content_element) {
        (None, false) => out.push(depth, format!("<code>{escaped}</code>")),
        (None, true) => {
            out.push(depth, "<code>".to_string());
            out.push(depth + 1, escaped);
            out.push(depth, "</code>".to_string());
        }
        (Some(l), false) => {
            out.push(depth, "<code>".to_string());
            out.push(depth + 1, format!("<label value=\"{}\"/>", attr_escape(l)));
            // Text child glues at column 0; the closing tag keeps its indent.
            out.push_glue(escaped);
            out.push(depth, "</code>".to_string());
        }
        (Some(l), true) => {
            out.push(depth, "<code>".to_string());
            out.push(depth + 1, format!("<label value=\"{}\"/>", attr_escape(l)));
            out.push(depth + 1, escaped);
            out.push(depth, "</code>".to_string());
        }
    }
}

/// Emit the four `<location>` provenance tokens (`x0,y0,x1,y1`) as element
/// children — docling's element head for backends with real geometry.
fn push_location(out: &mut Out, depth: i32, loc: &[u16; 4]) {
    for v in loc {
        out.push(depth, format!("<location value=\"{v}\"/>"));
    }
}

fn emit_table(out: &mut Out, depth: i32, table: &Table) {
    out.push(depth, "<table>".to_string());
    emit_table_rows(out, depth, table);
    out.push(depth, "</table>".to_string());
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
/// the location head, then each row's OTSL cell tokens at `depth + 1`.
fn emit_table_rows(out: &mut Out, depth: i32, table: &Table) {
    // Layout provenance (spreadsheet/slide backends): four `<location>` tokens
    // (x0,y0,x1,y1) precede the cells, matching docling's element head.
    if let Some(loc) = &table.location {
        push_location(out, depth + 1, loc);
    }
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

/// Serialize the node stream to DocLang XML (no trailing newline).
pub fn export_to_doclang(nodes: &[Node]) -> String {
    let mut out = Out {
        lines: Vec::new(),
        pic_index: 0,
    };
    out.push(0, "<doclang version=\"0.7\">".to_string());
    let mut i = 0usize;
    emit_nodes(&mut out, 1, nodes, &mut i, 0);
    out.push(0, "</doclang>".to_string());
    out.finish()
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
                    format!("heading level=\"{level}\"")
                };
                emit_text_element(out, depth, &open, "heading", text, None);
                *i += 1;
            }
            Node::Paragraph { text } => {
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
            Node::CheckboxItem { checked, text } => {
                // A `<text>` with a `<checkbox class="selected|unselected"/>` head
                // element and the label text child (block form).
                let class = if *checked { "selected" } else { "unselected" };
                out.push(depth, "<text>".to_string());
                out.push(depth + 1, format!("<checkbox class=\"{class}\"/>"));
                if !text.is_empty() {
                    out.push(depth + 1, escape_text(text));
                }
                out.push(depth, "</text>".to_string());
                *i += 1;
            }
            Node::Code {
                language,
                text,
                orig: _,
            } => {
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
                    out.push(depth, format!("<formula>{}</formula>", escape_text(latex)));
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
            Node::Table(t) => {
                emit_table(out, depth, t);
                *i += 1;
            }
            // Classifier predictions are JSON-only; DocLang keeps the plain
            // `<picture>` shape.
            Node::Picture { caption, image, .. } => {
                emit_picture(out, depth, caption.as_deref(), image.as_ref(), None);
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
            Node::Group { children, .. } => {
                let mut j = 0usize;
                emit_nodes(out, depth, children, &mut j, 0);
                *i += 1;
            }
            Node::FieldRegion { items } => {
                emit_field_region(out, depth, items);
                *i += 1;
            }
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
            Node::Located { location, inner } => {
                emit_located(out, depth, location, inner);
                *i += 1;
            }
            Node::PageBreak => {
                out.push(depth, "<page_break/>".to_string());
                *i += 1;
            }
            Node::TextDump(text) => {
                emit_text_dump(out, depth, text);
                *i += 1;
            }
        }
    }
}

/// One minidom child of the dump's `<text>`: a plain text node, a `<![CDATA[…]]>`
/// section, or a formatted element (`<italic>…</italic>`).
enum DumpNode {
    Text(String),
    Cdata(String),
    Elem(String),
}

/// Render docling's plain-text backend dump: the whole file as one `<text>` item,
/// serialized the way `xml.dom.minidom.toprettyxml` renders a `<text>` element.
///
/// docling applies inline Markdown to the text item, then builds a minified
/// `<text>…</text>` string — each source line a record, `*`-emphasis converted to
/// `<italic>`, XML-significant lines (`" ' & < >`) wrapped in `<![CDATA[…]]>` — and
/// pretty-prints it, dropping blank lines. This reproduces that pipeline: parse the
/// emphasis ([`dump_records`]), assemble the minidom child nodes, then simulate
/// `toprettyxml`, which writes a text node as `indent + data`, a CDATA section as a
/// bare `<![CDATA[…]]>` (no indent, no newline — so the next child's indent glues
/// onto its line), and an element as `indent + <tag>…</tag>`.
fn emit_text_dump(out: &mut Out, depth: i32, text: &str) {
    let records = dump_records(text);
    if records.is_empty() {
        out.push(depth, "<text></text>".to_string());
        return;
    }
    // Assemble the `<text>` element's minidom children. Consecutive plain records
    // (and the `\n` record separators around them) collapse into one text node; a
    // CDATA or formatted record breaks the run into its own node.
    let mut nodes: Vec<DumpNode> = Vec::new();
    let mut buf = String::new();
    for (r, (line, italic)) in records.iter().enumerate() {
        if r > 0 {
            buf.push('\n'); // the record separator
        }
        let raw = unescape_stored(line);
        let s = raw.as_ref();
        let is_cdata = s.contains(['"', '\'', '&', '<', '>']);
        if *italic || is_cdata {
            if !buf.is_empty() {
                nodes.push(DumpNode::Text(std::mem::take(&mut buf)));
            }
            let inner = if is_cdata {
                format!("<![CDATA[{s}]]>")
            } else {
                s.to_string()
            };
            if *italic {
                nodes.push(DumpNode::Elem(format!("<italic>{inner}</italic>")));
            } else {
                nodes.push(DumpNode::Cdata(inner));
            }
        } else {
            buf.push_str(s);
        }
    }
    if !buf.is_empty() {
        nodes.push(DumpNode::Text(buf));
    }

    // A lone text node is a single text child — minidom renders it inline.
    if let [DumpNode::Text(d)] = nodes.as_slice() {
        out.push(depth, format!("<text>{d}\n</text>"));
        return;
    }

    // Simulate `toprettyxml`: element/text children indent at depth+1, CDATA sits
    // bare; then drop the blank lines docling's empty-line filter removes.
    let ind_child = INDENT.repeat((depth + 1).max(0) as usize);
    let ind_self = INDENT.repeat(depth.max(0) as usize);
    let mut raw = String::new();
    for node in &nodes {
        match node {
            DumpNode::Text(d) => {
                raw.push_str(&ind_child);
                raw.push_str(d);
                raw.push('\n');
            }
            DumpNode::Cdata(b) => raw.push_str(b),
            DumpNode::Elem(b) => {
                raw.push_str(&ind_child);
                raw.push_str(b);
                raw.push('\n');
            }
        }
    }
    let full = format!("{ind_self}<text>\n{raw}{ind_self}</text>");
    for line in full.split('\n') {
        if !line.trim().is_empty() {
            out.push(0, line.to_string());
        }
    }
}

/// Parse a plain-text dump into one record per line, applying docling's inline
/// Markdown: CommonMark `*`/`**` emphasis (flanking rules + the delimiter-stack
/// match) is stripped and its span flagged `italic`; a Markdown thematic break (a
/// line of only underscores) collapses to ten underscores; blank lines drop out.
fn dump_records(text: &str) -> Vec<(String, bool)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();

    struct Delim {
        pos: usize,
        length: usize,
        rem: usize,
        can_open: bool,
        can_close: bool,
    }
    let is_ws = |c: Option<char>| c.map_or(true, |c| c.is_whitespace());
    let is_punct =
        |c: Option<char>| c.is_some_and(|c| "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~".contains(c));

    // Delimiter runs of `*`, each tagged left-/right-flanking (CommonMark 6.2).
    let mut delims: Vec<Delim> = Vec::new();
    let mut i = 0;
    while i < n {
        if chars[i] == '*' {
            let mut j = i;
            while j < n && chars[j] == '*' {
                j += 1;
            }
            let prev = (i > 0).then(|| chars[i - 1]);
            let next = (j < n).then(|| chars[j]);
            let left = !is_ws(next) && (!is_punct(next) || is_ws(prev) || is_punct(prev));
            let right = !is_ws(prev) && (!is_punct(prev) || is_ws(next) || is_punct(next));
            delims.push(Delim {
                pos: i,
                length: j - i,
                rem: j - i,
                can_open: left,
                can_close: right,
            });
            i = j;
        } else {
            i += 1;
        }
    }

    // Match closers to the nearest eligible opener (CommonMark "process emphasis"),
    // marking the delimiter characters consumed and the spanned text emphasized.
    let mut emph = vec![false; n];
    let mut consumed = vec![false; n];
    let mut ci = 0;
    while ci < delims.len() {
        if !(delims[ci].can_close && delims[ci].rem > 0) {
            ci += 1;
            continue;
        }
        let mut found: Option<usize> = None;
        let mut oi = ci as i64 - 1;
        while oi >= 0 {
            let o = &delims[oi as usize];
            let c = &delims[ci];
            if o.can_open && o.rem > 0 {
                // "Rule of three": a run may not close its own kind when the
                // combined length is a multiple of three (unless both are).
                let odd = (o.can_close || c.can_open)
                    && (o.length + c.length) % 3 == 0
                    && !(o.length % 3 == 0 && c.length % 3 == 0);
                if !odd {
                    found = Some(oi as usize);
                    break;
                }
            }
            oi -= 1;
        }
        let Some(fi) = found else {
            ci += 1;
            continue;
        };
        let use_ = if delims[fi].rem >= 2 && delims[ci].rem >= 2 {
            2
        } else {
            1
        };
        let oend = delims[fi].pos + delims[fi].rem;
        for c in consumed.iter_mut().take(oend).skip(oend - use_) {
            *c = true;
        }
        let cstart = delims[ci].pos + (delims[ci].length - delims[ci].rem);
        for c in consumed.iter_mut().take(cstart + use_).skip(cstart) {
            *c = true;
        }
        for e in emph.iter_mut().take(cstart).skip(oend) {
            *e = true;
        }
        delims[fi].rem -= use_;
        delims[ci].rem -= use_;
        delims.drain((fi + 1)..ci);
        ci = if delims[fi].rem == 0 { fi + 1 } else { fi };
    }

    // Drop the consumed markers, then split into lines carrying their emphasis.
    let mut records: Vec<(String, bool)> = Vec::new();
    let mut line = String::new();
    let mut line_italic = false;
    let push_line = |line: &mut String, italic: &mut bool, out: &mut Vec<(String, bool)>| {
        let text = std::mem::take(line);
        let ital = std::mem::replace(italic, false);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        // A Markdown thematic break (underscores only) normalizes to ten.
        let norm = if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '_') {
            "_".repeat(10)
        } else {
            text
        };
        out.push((norm, ital));
    };
    for k in 0..n {
        if consumed[k] {
            continue;
        }
        if chars[k] == '\n' {
            push_line(&mut line, &mut line_italic, &mut records);
        } else {
            line.push(chars[k]);
            if emph[k] {
                line_italic = true;
            }
        }
    }
    push_line(&mut line, &mut line_italic, &mut records);
    records
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
    let has_styled = runs.iter().any(|r| !r.is_plain());

    if unwrapped {
        for run in runs {
            if run.is_plain() {
                out.push(0, escape_text(&run.text));
            } else if run.formula {
                out.push(
                    depth,
                    format!("<formula>{}</formula>", escape_text(&run.text)),
                );
            } else {
                emit_styled(out, depth, &style_tags(run), &escape_text(&run.text));
            }
        }
        return;
    }

    // Wrapped: an all-plain group is a single text node — inline form, runs
    // joined by "\n" with the serializer's trailing "\n" before `</text>`.
    if !has_styled {
        let joined = runs
            .iter()
            .map(|r| escape_text(&r.text))
            .collect::<Vec<_>>()
            .join("\n");
        out.push(depth, format!("<text>{joined}\n</text>"));
        return;
    }

    out.push(depth, "<text>".to_string());
    emit_inline_runs_body(out, depth + 1, runs);
    out.push(depth, "</text>".to_string());
}

/// Emit the child runs of an inline group at `depth` (the body shared by a
/// wrapped `<text>` group and a list item's bare content). A `<content>`-wrapped
/// run is an *element* child → indented; a bare text/CDATA node sits at column 0
/// (its record delimiter's leading newline), except the first child, which has
/// no leading newline and stays indented. Styled/formula runs are elements.
fn emit_inline_runs_body(out: &mut Out, depth: i32, runs: &[InlineRun]) {
    for (i, run) in runs.iter().enumerate() {
        if run.is_plain() {
            let e = escape_text(&run.text);
            let d = if e.starts_with("<content>") || i == 0 {
                depth
            } else {
                0
            };
            if e.starts_with("<![CDATA[") && i + 1 == runs.len() && d == 0 {
                // minidom writes a trailing CDATA bare (no newline); the "\n"
                // record delimiter that follows still writes its indentation
                // before its newline, leaving trailing spaces on the CDATA line
                // (the blank line it opens is dropped by the empty-line filter).
                out.push_glue(e);
                out.push(depth, "");
            } else {
                out.push(d, e);
            }
        } else if run.formula {
            out.push(
                depth,
                format!("<formula>{}</formula>", escape_text(&run.text)),
            );
        } else {
            emit_styled(out, depth, &style_tags(run), &escape_text(&run.text));
        }
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
fn emit_styled(out: &mut Out, depth: i32, tags: &[&str], inner: &str) {
    match tags {
        [] => emit_text_node(out, depth, inner),
        [tag] => out.push(depth, format!("<{tag}>{inner}</{tag}>")),
        [tag, rest @ ..] => {
            out.push(depth, format!("<{tag}>"));
            emit_styled(out, depth + 1, rest, inner);
            out.push(depth, format!("</{tag}>"));
        }
    }
}

/// Render a [`Node::Furniture`] wrapper: the inner element with a
/// `<layer value="{layer}"/>` head (which forces the block form). Headings (the
/// HTML `<title>`, section chrome) and body text (docx comments, nav items) are
/// emitted with the layer token; other nodes fall back to their body rendering.
fn emit_furniture(out: &mut Out, depth: i32, layer: ContentLayer, inner: &Node) {
    let token = format!("<layer value=\"{}\"/>", layer.value());
    match inner {
        Node::Heading { level, text } => {
            let open = if *level <= 1 {
                "heading".to_string()
            } else {
                format!("heading level=\"{level}\"")
            };
            out.push(depth, format!("<{open}>"));
            out.push(depth + 1, token);
            out.push(depth + 1, escape_text(text));
            out.push(depth, "</heading>".to_string());
        }
        Node::Paragraph { text } => {
            out.push(depth, "<text>".to_string());
            out.push(depth + 1, token);
            out.push(depth + 1, escape_text(text));
            out.push(depth, "</text>".to_string());
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
            } else {
                let mut i = 0usize;
                emit_nodes(out, depth, std::slice::from_ref(inner.as_ref()), &mut i, 0);
            }
        }
        // A furniture inline group (a mixed-formatting header/footer paragraph):
        // wrapped in `<text>`, with each child run carrying its own layer token
        // (docling stamps the layer on every text item of the group).
        Node::InlineGroup { runs, .. } => {
            out.push(depth, "<text>".to_string());
            for run in runs {
                out.push(depth + 1, token.clone());
                if run.is_plain() {
                    out.push(depth + 1, escape_text(&run.text));
                } else if run.formula {
                    out.push(
                        depth + 1,
                        format!("<formula>{}</formula>", escape_text(&run.text)),
                    );
                } else {
                    emit_styled(out, depth + 1, &style_tags(run), &escape_text(&run.text));
                }
            }
            out.push(depth, "</text>".to_string());
        }
        // A furniture picture (site-chrome logo/banner, header/footer image):
        // the layer token, an embedded-image `<src>` when the picture carries
        // pixels (docling's referenced-asset conversion skips furniture, so the
        // image stays a base64 data URI), then a caption that carries its own
        // `<href>`/`<layer>` head when the caption is a link.
        Node::Picture { caption, image, .. } => {
            let caption = caption.as_deref().filter(|c| !c.trim().is_empty());
            out.push(depth, "<picture>".to_string());
            out.push(depth + 1, token.clone());
            if let Some(img) = image {
                out.push(
                    depth + 1,
                    format!(
                        "<src uri=\"data:image/png;base64,{}\"/>",
                        crate::base64::encode(&img.data)
                    ),
                );
            }
            if let Some(c) = caption {
                out.push(depth + 1, "<caption>".to_string());
                match inline_runs(c).into_iter().next() {
                    Some(Run::Link { anchor, uri }) => {
                        out.push(depth + 2, format!("<href uri=\"{}\"/>", attr_escape(&uri)));
                        out.push(depth + 2, token.clone());
                        out.push(depth + 2, escape_text(&anchor));
                    }
                    _ => {
                        out.push(depth + 2, token.clone());
                        out.push(depth + 2, escape_text(c));
                    }
                }
                out.push(depth + 1, "</caption>".to_string());
            }
            out.push(depth, "</picture>".to_string());
        }
        // An invisible-layer table (a hidden spreadsheet sheet): the layer
        // token precedes the location/cells inside the `<table>`.
        Node::Table(table) => {
            out.push(depth, "<table>".to_string());
            out.push(depth + 1, token);
            emit_table_rows(out, depth, table);
            out.push(depth, "</table>".to_string());
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
    image: Option<&crate::document::PictureImage>,
    location: Option<&[u16; 4]>,
) {
    let caption = caption.filter(|c| !c.trim().is_empty());
    // An image-bearing picture carries a referenced-image `<src>` naming the
    // exported asset (`assets/image_{index:06}_{sha256}.png`), matching docling's
    // referenced-image mode. docling re-encodes every image to PNG through PIL, so
    // the extension is always `.png` and the content hash is over those re-encoded
    // bytes — not reproducible here, so we hash the source bytes and the
    // conformance harness canonicalizes the digest before comparing.
    let src = image.map(|img| {
        let idx = out.pic_index;
        out.pic_index += 1;
        format!("assets/image_{idx:06}_{}.png", sha256_hex(&img.data))
    });
    if location.is_none() && caption.is_none() && src.is_none() {
        out.push(depth, "<picture></picture>".to_string());
        return;
    }
    out.push(depth, "<picture>".to_string());
    if let Some(loc) = location {
        push_location(out, depth + 1, loc);
    }
    if let Some(s) = src {
        out.push(depth + 1, format!("<src uri=\"{}\"/>", attr_escape(&s)));
    }
    if let Some(c) = caption {
        emit_caption(out, depth + 1, c);
    }
    out.push(depth, "</picture>".to_string());
}

/// A `<caption>` — inline when plain text, or block form with an `<href uri=…/>`
/// head + anchor text when the caption is a single Markdown link (docling's
/// linked image captions).
fn emit_caption(out: &mut Out, depth: i32, text: &str) {
    if let Some(Run::Link { anchor, uri }) = inline_runs(text).into_iter().next() {
        if inline_runs(text).len() == 1 {
            out.push(depth, "<caption>".to_string());
            out.push(depth + 1, format!("<href uri=\"{}\"/>", attr_escape(&uri)));
            out.push(depth + 1, escape_text(&anchor));
            out.push(depth, "</caption>".to_string());
            return;
        }
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

/// Lowercase hex SHA-256 of `bytes` (image asset content hash).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Render a [`Node::Located`] wrapper: the inner element with its `<location>`
/// tokens as the first children.
fn emit_located(out: &mut Out, depth: i32, location: &[u16; 4], inner: &Node) {
    match inner {
        Node::Heading { level, text } => {
            let open = if *level <= 1 {
                "heading".to_string()
            } else {
                format!("heading level=\"{level}\"")
            };
            emit_text_element(out, depth, &open, "heading", text, Some(location));
        }
        Node::Paragraph { text } => {
            emit_text_element(out, depth, "text", "text", text, Some(location));
        }
        Node::Picture { caption, image, .. } => {
            emit_picture(
                out,
                depth,
                caption.as_deref(),
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
    let mut prev_number: Option<u64> = None;
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
                let eff_ordered = dclx.as_ref().map_or(*o, |d| d.ordered);
                let eff_marker = dclx.as_ref().map_or(marker.as_ref(), |d| d.marker.as_ref());
                // A new sibling list at this depth closes this one (the caller
                // re-opens): the backend flagged a fresh list, the kind flips, or
                // an ordered run breaks — matching the Markdown serializer.
                if *i != start
                    && (*first_in_list
                        || eff_ordered != ordered
                        || (ordered && Some(*number) != prev_number.map(|n| n + 1)))
                {
                    break;
                }
                prev_number = Some(*number);
                // docling wraps a list item's content in `<text>` when a nested
                // list follows it *anywhere* inside the same `<list>` — its
                // `_list_item_has_segment_siblings` scans the parent group's
                // children after the item; a plain item with no later nested
                // list stays bare.
                let has_nested = {
                    let mut found = false;
                    let mut pn = Some(*number);
                    let mut j = *i + 1;
                    while let Some(Node::ListItem {
                        level: nl,
                        ordered: no,
                        number: nn,
                        first_in_list: nf,
                        dclx: nd,
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
                        // The same run-break rules as the main loop: a sibling
                        // list at this depth ends this `<list>` element.
                        let n_ordered = nd.as_ref().map_or(*no, |d| d.ordered);
                        if *nf
                            || n_ordered != ordered
                            || (ordered && Some(*nn) != pn.map(|n| n + 1))
                        {
                            break;
                        }
                        pn = Some(*nn);
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
                // Layout provenance (PPTX shapes): the four `<location>` tokens
                // follow the `<ldiv>` and precede the item's content, matching
                // docling's element head inside the list.
                if let Some(loc) = location {
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
                        // docling emits an `<href>` head only when the item's whole
                        // content is a lone link (`[anchor](uri)`); a mixed item
                        // (`text [anchor](uri) …`) keeps the anchor inline with no
                        // head. A non-body layer always rides in the head.
                        let stripped = strip_lone_link(text);
                        let eff_href = href
                            .as_deref()
                            .filter(|_| matches!(stripped, Cow::Owned(_)));
                        if eff_href.is_some() || layer.is_some() {
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
            // starts a *new* list (fresh-list flag, kind flip, or an ordered
            // sequence break), no reuse happens and the empty text survives.
            Node::Paragraph { text }
                if text.is_empty()
                    && matches!(
                        nodes.get(*i + 1),
                        Some(Node::ListItem { level: nl, ordered: no, number: nn,
                                              first_in_list: nf, dclx: nd, .. })
                            if *nl > level
                                || (*nl == level
                                    && !*nf
                                    && nd.as_ref().map_or(*no, |d| d.ordered) == ordered
                                    && (!ordered
                                        || Some(*nn) == prev_number.map(|n| n + 1)))
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
) {
    let head = |out: &mut Out, d: i32| {
        if let Some(uri) = href {
            out.push(d, format!("<href uri=\"{}\"/>", attr_escape(uri)));
        }
        if let Some(l) = layer {
            out.push(d, format!("<layer value=\"{}\"/>", l.value()));
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
    let single_plain = runs.len() <= 1 && runs.first().map_or(true, |r| r.is_plain());
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
        }])
    }

    #[test]
    fn code_with_language_emits_linguist_label_block_form() {
        // Fence language folds through the docling value onto the Linguist key,
        // forcing the block form; CDATA text glues the closing tag.
        assert_eq!(
            code(Some("python"), "print(\"Hello world!\")"),
            "<doclang version=\"0.7\">\n  <code>\n    <label value=\"Python\"/>\n\
             <![CDATA[print(\"Hello world!\")]]>  </code>\n</doclang>"
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
        body.trim_start_matches("<doclang version=\"0.7\">\n")
            .trim_end_matches("\n</doclang>")
            .to_string()
    }

    #[test]
    fn inline_group_matches_reference_layout() {
        // wrapped, mixed: first text indented, post-element text at col 0.
        assert_eq!(
            ig(
                false,
                vec![plain("This is a"), bold("bold"), plain("example")]
            ),
            "  <text>\n    This is a\n    <bold>bold</bold>\nexample\n  </text>"
        );
        // unwrapped, mixed: text at col 0, elements at depth 1.
        assert_eq!(
            ig(
                true,
                vec![
                    plain("aa"),
                    bold("bb"),
                    plain("cc"),
                    bold("dd"),
                    plain("ee")
                ]
            ),
            "aa\n  <bold>bb</bold>\ncc\n  <bold>dd</bold>\nee"
        );
        // wrapped, all-plain: single text node with trailing newline.
        assert_eq!(
            ig(false, vec![plain("aa"), plain("bb")]),
            "  <text>aa\nbb\n</text>"
        );
        assert_eq!(ig(false, vec![plain("aa")]), "  <text>aa\n</text>");
        // wrapped, single element.
        assert_eq!(
            ig(false, vec![bold("bb")]),
            "  <text>\n    <bold>bb</bold>\n  </text>"
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
        // italic (applied after bold) is outermost; block form.
        assert_eq!(
            ig(true, vec![bi]),
            "  <italic>\n    <bold>bi</bold>\n  </italic>"
        );
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
            "<doclang version=\"0.7\">\n  <heading>\n    <layer value=\"furniture\"/>\n    Anchor Links Test\n  </heading>\n</doclang>"
        );
    }

    #[test]
    fn text_dump_reproduces_minidom_per_line_layout() {
        // A plain-text dump: the first record indents, later plain records sit at
        // column 0, a line with `"`/`&` becomes CDATA (with the next child's indent
        // glued on as trailing whitespace), a `*`…`*` span becomes per-line
        // `<italic>`, and an underscore rule collapses to ten underscores.
        let text = "PATN\nWKU 1\nPAL K. \"Determination\"\nfollow-up\n*Note A\n_______________\nNote B*\nEND";
        let out = export_to_doclang(&[Node::TextDump(text.into())]);
        let expected = "<doclang version=\"0.7\">\n  \
             <text>\n    \
             PATN\nWKU 1\n\
             <![CDATA[PAL K. \"Determination\"]]>    \n\
             follow-up\n    \
             <italic>Note A</italic>\n    \
             <italic>__________</italic>\n    \
             <italic>Note B</italic>\n\
             END\n  \
             </text>\n</doclang>";
        assert_eq!(out, expected, "got:\n{out}");
    }

    #[test]
    fn code_without_language_stays_inline_and_unlabeled() {
        assert_eq!(
            code(None, "print(\"Hi!\")"),
            "<doclang version=\"0.7\">\n  <code><![CDATA[print(\"Hi!\")]]></code>\n</doclang>"
        );
        // Unknown fence language: no label, still inline.
        assert!(!code(Some("brainfuck"), "+++.").contains("<label"));
    }
}
