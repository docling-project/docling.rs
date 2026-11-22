//! Typst serializer (#1226) — `--to typst`: the document body as Typst
//! markup (`.typ`), for a pure-Rust / WASM `document → Typst → PDF` chain
//! without Pandoc. docling has no Typst serializer, so there is no upstream
//! output to match; the walk follows the LaTeX serializer's ([`crate::latex`])
//! reading of the model — body layer only, a level-1 heading is the title,
//! runs of list items fold into (nested) lists, rich table cells render their
//! blocks — and the Markdown inline markers baked into the node text
//! (`**bold**`, `*italic*`, `~~strike~~`, `` `code` ``, `[text](url)`,
//! `$formula$`) become `#strong[…]` / `#emph[…]` / `#strike[…]` / `#raw(…)` /
//! `#link(…)[…]` / math.
//!
//! The output is body markup only: no `#set` rules, no template, no
//! preamble, so a caller wraps it in its own (`#import "template.typ": *`
//! then `#include "doc.typ"`). The crate gains no dependency — the Typst
//! compiler is the caller's.
//!
//! Mapping:
//! - headings → `=` × level (docling's title is the level-1 heading);
//! - paragraphs → escaped markup (every character Typst markup would read
//!   as syntax is backslash-escaped, line-start markers only at a line
//!   start), an in-text line break → Typst's `\` line break;
//! - lists → `-` / `+` items, nesting by two-space indentation; a checkbox
//!   item is a bullet with `☐` / `☒`;
//! - code → a fenced raw block with its language (a fence longer than any
//!   backtick run inside); inline code → `#raw("…")`;
//! - tables → `#table(columns: N, …)`, header rows as `table.header(…)`,
//!   spans as `table.cell(colspan: c, rowspan: r)[…]`, wrapped in
//!   `#figure(…, caption: […])` when captioned, footnotes after it;
//! - pictures → `#image("…")` under [`ImageMode::Referenced`] (the files
//!   are returned like the Markdown export's `artifacts/image_NNNNNN.<ext>`),
//!   in a `#figure` with its caption; otherwise — and for an encoding Typst
//!   cannot decode (EMF, TIFF, BMP) — a `/* image */` placeholder, a caption
//!   still making it a figure. Typst has no way to decode a `data:` URI, so
//!   [`ImageMode::Embedded`] falls back to the placeholder too;
//! - formulas → Typst math when the LaTeX source only uses the constructs
//!   [`latex_to_typst_math`] translates (scripts, `\frac`, `\sqrt`, Greek,
//!   operators, relations, arrows, `\mathbf` & co., `\left…\right`), else
//!   the LaTeX as a `latex` raw block / inline raw — never a guess that
//!   might not compile.

use crate::document::{DoclingDocument, FieldItem, Node, Table};
use crate::markdown::ImageMode;

/// Options for [`DoclingDocument::export_to_typst_with`].
#[derive(Debug, Clone)]
pub struct TypstExportOptions {
    /// How pictures carry their image: [`ImageMode::Referenced`] writes
    /// `#image("<artifacts_dir>/image_NNNNNN.<ext>")` and returns the
    /// files; any other mode is the placeholder (see the module docs).
    pub image_mode: ImageMode,
    /// The directory referenced images are named under.
    pub artifacts_dir: String,
}

impl Default for TypstExportOptions {
    fn default() -> Self {
        Self {
            image_mode: ImageMode::Placeholder,
            artifacts_dir: "artifacts".to_string(),
        }
    }
}

/// The Typst markup and, for [`ImageMode::Referenced`], the
/// `(path, bytes)` image files it links to.
pub type TypstOutput = (String, Vec<(String, Vec<u8>)>);

const IMAGE_PLACEHOLDER: &str = "/* image */";

struct Ctx {
    referenced: bool,
    artifacts_dir: String,
    pic_index: usize,
    artifacts: Vec<(String, Vec<u8>)>,
}

/// Serialize `doc` as Typst body markup, ending in one newline (an empty
/// document is the empty string).
pub fn to_typst(doc: &DoclingDocument, options: &TypstExportOptions) -> TypstOutput {
    let mut ctx = Ctx {
        referenced: options.image_mode == ImageMode::Referenced,
        artifacts_dir: options.artifacts_dir.trim_end_matches('/').to_string(),
        pic_index: 0,
        artifacts: Vec::new(),
    };
    let body = render_nodes(&mut ctx, &doc.nodes, 0).join("\n\n");
    let body = if body.is_empty() {
        body
    } else {
        format!("{body}\n")
    };
    (body, ctx.artifacts)
}

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

/// Escape `text` as Typst markup text. `line_start`: the text begins a
/// markup line, where `=`, `-`, `+`, `/` and `1.` open a heading / list /
/// term / enumeration. A newline becomes a Typst line break (`\` at the end
/// of the line) and the next line is escaped as a line start; a run of
/// newlines is one break (a blank line would end the paragraph).
pub fn escape_typst(text: &str, line_start: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut at_start = line_start;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\r' {
            i += 1;
            continue;
        }
        if c == '\n' {
            while i < chars.len() && (chars[i] == '\n' || chars[i] == '\r') {
                i += 1;
            }
            // A trailing break carries nothing.
            if i < chars.len() {
                // Drop the spaces before the break: `\` must end the line.
                while out.ends_with(' ') || out.ends_with('\t') {
                    out.pop();
                }
                out.push_str(" \\\n");
                at_start = true;
                while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                    i += 1;
                }
            }
            continue;
        }
        if at_start {
            if c == ' ' || c == '\t' {
                i += 1;
                continue;
            }
            at_start = false;
            if matches!(c, '=' | '-' | '+' | '/') {
                out.push('\\');
                out.push(c);
                i += 1;
                continue;
            }
            if c.is_ascii_digit() {
                // `12.` opens an enumeration item: escape its dot.
                let mut j = i;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                out.extend(&chars[i..j]);
                if chars.get(j) == Some(&'.') {
                    out.push_str("\\.");
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        match c {
            '\\' | '#' | '$' | '*' | '_' | '`' | '<' | '>' | '@' | '[' | ']' | '~' => {
                out.push('\\');
                out.push(c);
            }
            // `//` and `/*` open comments.
            '/' if matches!(chars.get(i + 1), Some('/' | '*')) => out.push_str("\\/"),
            // `--` / `---` are dash shorthands, `-?` a soft hyphen.
            '-' if matches!(chars.get(i + 1), Some('-' | '?')) => out.push_str("\\-"),
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// A Typst string literal (`"…"`, with `\\`, `\"` and control escapes).
fn string_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The JSON export's `unescape_text`: the model text minus the Markdown
/// escapes docling's serializer added.
fn unescape_text(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("\\_", "_")
}

// ---------------------------------------------------------------------------
// Inline markup — Markdown markers into Typst (module docs).
// ---------------------------------------------------------------------------

/// Render a text carrying docling's Markdown inline markers as Typst inline
/// markup, starting at a line start.
fn inline_md(md: &str) -> String {
    let chars: Vec<char> = crate::markdown::strip_hard_breaks(md).chars().collect();
    let mut w = Inline::new(true);
    render_inline(&chars, true, &mut w);
    w.finish()
}

/// [`inline_md`] for a table cell: a `$…$` in a plain cell stays text (as
/// in the LaTeX export), and newlines flatten to spaces.
fn inline_cell(md: &str) -> String {
    let flat = crate::markdown::strip_hard_breaks(md).replace('\n', " ");
    let chars: Vec<char> = flat.chars().collect();
    let mut w = Inline::new(true);
    render_inline(&chars, false, &mut w);
    w.finish()
}

/// The inline writer: plain runs are buffered and escaped on flush; after an
/// embedded `#call(…)` a following `.` or `(` would continue the expression
/// (a method call, another call), so a `;` ends it first.
struct Inline {
    out: String,
    plain: String,
    line_start: bool,
    after_code: bool,
}

impl Inline {
    fn new(line_start: bool) -> Self {
        Self {
            out: String::new(),
            plain: String::new(),
            line_start,
            after_code: false,
        }
    }

    fn flush(&mut self) {
        if self.plain.is_empty() {
            return;
        }
        let raw = unescape_text(&std::mem::take(&mut self.plain));
        let start = self.line_start && self.out.is_empty();
        let escaped = escape_typst(&raw, start);
        if self.after_code && continues_expr(&escaped) {
            self.out.push(';');
        }
        self.after_code = false;
        self.out.push_str(&escaped);
    }

    fn code(&mut self, s: &str) {
        self.flush();
        if self.after_code && continues_expr(s) {
            self.out.push(';');
        }
        self.out.push_str(s);
        self.after_code = s.starts_with('#');
    }

    fn finish(mut self) -> String {
        self.flush();
        self.out
    }
}

/// Whether `next`, written right after an embedded expression, would extend
/// it: a call (`(`, `[`) or a field access (`.` and an identifier).
fn continues_expr(next: &str) -> bool {
    let mut it = next.chars();
    match it.next() {
        Some('(' | '[') => true,
        Some('.') => it.next().is_some_and(|c| c.is_alphabetic() || c == '_'),
        _ => false,
    }
}

fn starts_with(chars: &[char], at: usize, pat: &str) -> bool {
    let mut it = chars[at..].iter();
    pat.chars().all(|p| it.next() == Some(&p))
}

fn find(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    let plen = pat.chars().count();
    (from..chars.len().saturating_sub(plen - 1)).find(|&i| starts_with(chars, i, pat))
}

fn render_inline(chars: &[char], formulas: bool, w: &mut Inline) {
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if chars[i] == '[' && !starts_with(chars, i, "[](") && !(i > 0 && chars[i - 1] == '!') {
            if let Some((label_end, url_end)) = link_bounds(chars, i) {
                let url: String = chars[label_end + 2..url_end].iter().collect();
                let link = hyperlink(&chars[i + 1..label_end], &url, formulas);
                w.code(&link);
                i = url_end + 1;
                continue;
            }
        }
        if chars[i] == '`' {
            if let Some((s, e, after)) = crate::markdown::code_span_bounds(chars, i) {
                let inner = &chars[s..e];
                if !inner.is_empty() && !inner.iter().all(|c| c.is_whitespace()) {
                    let raw: String = inner.iter().collect();
                    let raw = unescape_text(&raw);
                    // A fence flattened into the text (a rich cell's block
                    // code) is block raw; a span inline raw.
                    let fenced = after - e >= 3;
                    let raw = if fenced { raw.trim().to_string() } else { raw };
                    let call = if fenced {
                        format!("#raw({}, block: true)", string_lit(&raw))
                    } else {
                        format!("#raw({})", string_lit(&raw))
                    };
                    w.code(&call);
                    i = after;
                    continue;
                }
            }
        }
        let mut matched = false;
        for marker in ["***", "**", "*", "~~"] {
            if starts_with(chars, i, marker) {
                let mlen = marker.chars().count();
                if let Some(end) = find(chars, i + mlen, marker) {
                    let inner = &chars[i + mlen..end];
                    if !inner.is_empty() && !inner.iter().all(|c| c.is_whitespace()) {
                        let mut sub = Inline::new(true);
                        render_inline(inner, formulas, &mut sub);
                        let content = sub.finish();
                        let call = match marker {
                            "***" => format!("#strong[#emph[{content}]]"),
                            "**" => format!("#strong[{content}]"),
                            "*" => format!("#emph[{content}]"),
                            _ => format!("#strike[{content}]"),
                        };
                        w.code(&call);
                        i = end + mlen;
                        matched = true;
                    }
                }
                break;
            }
        }
        if matched {
            continue;
        }
        if formulas && starts_with(chars, i, "$$") {
            w.plain.push_str("$$");
            i += 2;
            continue;
        }
        // An inline formula, told from currency by Pandoc's
        // `tex_math_dollars` rule (as the LaTeX export does).
        if formulas && chars[i] == '$' {
            if let Some(end) = find(chars, i + 1, "$") {
                let non_space = |k: usize| chars.get(k).is_some_and(|c| !c.is_whitespace());
                let digit_after = chars.get(end + 1).is_some_and(|c| c.is_ascii_digit());
                if end > i + 1 && non_space(i + 1) && non_space(end - 1) && !digit_after {
                    let latex: String = chars[i + 1..end].iter().collect();
                    w.code(&formula(&latex, false));
                    i = end + 1;
                    continue;
                }
            }
        }
        w.plain.push(chars[i]);
        i += 1;
    }
}

/// For a `[` at `open`: the index of the `](` closing the label and of the
/// `)` closing the URL (balanced brackets / parentheses inside).
fn link_bounds(chars: &[char], open: usize) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    let mut label_end = None;
    for k in open + 1..chars.len() {
        match chars[k] {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            ']' if starts_with(chars, k, "](") => {
                label_end = Some(k);
                break;
            }
            ']' => return None,
            _ => {}
        }
    }
    let label_end = label_end?;
    let mut depth = 0usize;
    for (k, &c) in chars.iter().enumerate().skip(label_end + 2) {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some((label_end, k)),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// `#link("url")[label]`; an unsafe destination (`javascript:`, #860)
/// keeps the label alone.
fn hyperlink(label: &[char], url: &str, formulas: bool) -> String {
    let mut sub = Inline::new(true);
    render_inline(label, formulas, &mut sub);
    let content = sub.finish();
    let url = unescape_text(url);
    if !crate::hyperlink::is_safe_hyperlink(&url) {
        return content;
    }
    if content.is_empty() {
        format!("#link({})", string_lit(&url))
    } else {
        format!("#link({})[{content}]", string_lit(&url))
    }
}

/// A formula: Typst math when [`latex_to_typst_math`] can translate the
/// LaTeX, else the LaTeX source as raw text tagged `latex`.
fn formula(latex: &str, block: bool) -> String {
    let latex = latex.trim();
    match latex_to_typst_math(latex) {
        Some(math) if block => format!("$ {math} $"),
        Some(math) => format!("${math}$"),
        None if block => crate::markdown::code_fence("latex", latex),
        None => format!("#raw({}, lang: \"latex\")", string_lit(latex)),
    }
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/// The non-empty Typst blocks of a sibling run, list runs folded into
/// lists. `indent`: the list nesting the run sits in (0 at the body).
fn render_nodes(ctx: &mut Ctx, nodes: &[Node], indent: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        if matches!(nodes[i], Node::ListItem { .. }) {
            let start = i;
            i += 1;
            loop {
                match nodes.get(i) {
                    Some(Node::ListItem { .. }) => i += 1,
                    Some(Node::Paragraph { text })
                        if text.is_empty()
                            && matches!(nodes.get(i + 1), Some(Node::ListItem { .. })) =>
                    {
                        i += 1
                    }
                    _ => break,
                }
            }
            for group in sibling_lists(&nodes[start..i]) {
                let text = render_list(group, indent);
                if !text.is_empty() {
                    parts.push(text);
                }
            }
        } else {
            render_one(ctx, &nodes[i], indent, &mut parts);
            i += 1;
        }
    }
    parts
}

fn render_one(ctx: &mut Ctx, node: &Node, indent: usize, parts: &mut Vec<String>) {
    let push = |parts: &mut Vec<String>, s: String| {
        if !s.trim().is_empty() {
            parts.push(s);
        }
    };
    match node {
        Node::Heading { level, text } => {
            let body = inline_md(&text.replace('\n', " "));
            if !body.trim().is_empty() {
                let marks = "=".repeat(usize::from((*level).max(1)));
                parts.push(format!("{marks} {body}"));
            }
        }
        Node::Caption { text, .. } => push(parts, inline_md(text)),
        Node::LabeledText { .. } => render_one(ctx, &node.labeled_as_paragraph(), indent, parts),
        Node::Paragraph { text } | Node::ListText { text, .. } => {
            let t = text.trim();
            match t.strip_prefix("$$").and_then(|s| s.strip_suffix("$$")) {
                Some(inner) if !inner.trim().is_empty() => push(parts, formula(inner, true)),
                _ => push(parts, inline_md(text)),
            }
        }
        Node::CheckboxItem { checked, text } => {
            let mark = if *checked { "☒" } else { "☐" };
            push(parts, format!("- {mark} {}", inline_md(text)));
        }
        Node::Code { language, text, .. } => {
            let raw = unescape_text(text);
            let lang = language.as_deref().map(raw_lang).unwrap_or_default();
            push(parts, crate::markdown::code_fence(&lang, &raw));
        }
        Node::Formula { latex, orig, .. } => {
            if !latex.trim().is_empty() {
                push(parts, formula(latex, true));
            } else if !orig.trim().is_empty() {
                push(parts, "/* formula-not-decoded */".to_string());
            }
        }
        Node::Table(t) => push(parts, render_table(ctx, t)),
        Node::Picture {
            caption,
            image,
            description,
            ..
        } => {
            let alt = description
                .as_ref()
                .filter(|d| d.is_alt_text())
                .map(|d| d.text.as_str());
            let img = picture_body(ctx, image.as_ref(), alt);
            push(parts, figure(img, caption.as_deref()));
        }
        Node::Chart { table, caption, .. } => {
            push(parts, figure(None, caption.as_deref()));
            push(parts, render_table(ctx, table));
        }
        Node::Group { layer: Some(_), .. } => {}
        Node::Group {
            label, children, ..
        } => {
            if label == "inline" {
                push(parts, render_nodes(ctx, children, indent).join(" "));
            } else {
                push(parts, render_nodes(ctx, children, indent).join("\n\n"));
            }
        }
        Node::FieldRegion { items } => render_field_region(items, parts),
        Node::KeyValueGraph { .. } => {}
        Node::InlineGroup { md_text, .. } => push(parts, inline_md(md_text)),
        Node::TextDump(text) => push(parts, escape_typst(&unescape_text(text), true)),
        Node::CommentSection { .. } => {}
        Node::Commented { inner, .. } | Node::ScriptText { inner, .. } => {
            render_one(ctx, inner, indent, parts)
        }
        Node::Located { inner, .. } | Node::Prov { inner, .. } | Node::Track { inner, .. } => {
            render_one(ctx, inner, indent, parts)
        }
        Node::ListItem { .. } => {
            let text = render_list(std::slice::from_ref(node), indent);
            push(parts, text);
        }
        Node::DoclangOnly(_)
        | Node::Furniture { .. }
        | Node::PageFurniture { .. }
        | Node::FurnitureText { .. }
        | Node::PictureChildren(_)
        | Node::PageBreak
        | Node::PageInfo { .. }
        | Node::ProvContinuation { .. }
        | Node::Anchor { .. } => {}
    }
}

/// A raw block's language tag: Typst reads it up to the first whitespace or
/// backtick, so keep a tag of `[A-Za-z0-9_+#.-]` only (docling's `C++`,
/// `C#`, `objective-c`), lower-cased; anything else is no tag.
fn raw_lang(lang: &str) -> String {
    let lang = lang.trim();
    if !lang.is_empty()
        && lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '#' | '.' | '-'))
    {
        lang.to_ascii_lowercase()
    } else {
        String::new()
    }
}

/// Field regions: the marker / key / value texts as paragraphs.
fn render_field_region(items: &[FieldItem], parts: &mut Vec<String>) {
    for item in items {
        for text in [&item.marker, &item.key, &item.value].into_iter().flatten() {
            let t = escape_typst(&unescape_text(text), true);
            if !t.trim().is_empty() {
                parts.push(t);
            }
        }
    }
}

/// The encodings Typst's `image` decodes.
fn typst_decodes(mimetype: &str) -> bool {
    matches!(
        mimetype,
        "image/png" | "image/jpeg" | "image/gif" | "image/svg+xml" | "image/webp"
    )
}

/// A picture's `image(…)` call (code form, no `#`) under referenced
/// images, registering its file; `None` for the placeholder.
fn picture_body(
    ctx: &mut Ctx,
    image: Option<&crate::document::PictureImage>,
    alt: Option<&str>,
) -> Option<String> {
    let image = image.filter(|i| ctx.referenced && !i.data.is_empty())?;
    if !typst_decodes(&image.mimetype) {
        return None;
    }
    let path = format!(
        "{}/image_{:06}.{}",
        ctx.artifacts_dir,
        ctx.pic_index,
        crate::markdown::ext_for(&image.mimetype)
    );
    ctx.pic_index += 1;
    ctx.artifacts.push((path.clone(), image.data.clone()));
    let mut call = format!("image({}", string_lit(&path));
    // The displayed size (inches, #834), when the format records one.
    if let Some([w, _]) = image.display_size.filter(|[w, h]| *w > 0.0 && *h > 0.0) {
        call.push_str(&format!(", width: {:.2}in", w));
    }
    if let Some(alt) = alt.map(str::trim).filter(|a| !a.is_empty()) {
        call.push_str(&format!(", alt: {}", string_lit(alt)));
    }
    call.push(')');
    Some(call)
}

/// `#figure(body, caption: […])`, or the bare `#image(…)` / placeholder
/// without a caption.
fn figure(body: Option<String>, caption: Option<&str>) -> String {
    let caption = caption.map(inline_cell).filter(|c| !c.trim().is_empty());
    match (body, caption) {
        (Some(b), Some(c)) => format!("#figure(\n  {b},\n  caption: [{c}],\n)"),
        (Some(b), None) => format!("#{b}"),
        (None, Some(c)) => format!("#figure(\n  [{IMAGE_PLACEHOLDER}],\n  caption: [{c}],\n)"),
        (None, None) => IMAGE_PLACEHOLDER.to_string(),
    }
}

/// One cell's content: a rich cell's blocks rendered as markup, else the
/// flat text.
fn cell_content(ctx: &mut Ctx, t: &Table, anchor: (usize, usize), flat: &str) -> String {
    let blocks = t
        .cell_blocks
        .as_ref()
        .and_then(|b| b.get(anchor.0))
        .and_then(|row| row.get(anchor.1))
        .filter(|b| !b.is_empty());
    match blocks {
        Some(blocks) => render_nodes(ctx, blocks, 0).join("\n\n"),
        None => inline_cell(flat),
    }
}

/// `#table(columns: N, …)`: first-class cells at their anchors (spans as
/// `table.cell(colspan:, rowspan:)`, positions no cell covers as `[]`),
/// else the padded row grid; leading all-header rows go in `table.header`.
fn render_table(ctx: &mut Ctx, t: &Table) -> String {
    let num_rows = t.rows.len();
    let num_cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut rows: Vec<Vec<String>> = vec![Vec::new(); num_rows];
    let mut header_rows = 0usize;
    match t.cells.as_ref().filter(|c| !c.is_empty()) {
        Some(cells) => {
            // Each cell placed at its anchor with its span clamped to the
            // grid; a cell overlapping one placed before it keeps only its
            // anchor (or is dropped when that is taken too) — Typst rejects
            // overlapping spans.
            struct Placed {
                k: usize,
                rs: usize,
                cs: usize,
            }
            let mut owner = vec![vec![None::<usize>; num_cols]; num_rows];
            let mut placed: Vec<Placed> = Vec::new();
            for (k, c) in cells.iter().enumerate() {
                let (r0, c0) = (c.start_row, c.start_col);
                if r0 >= num_rows || c0 >= num_cols || owner[r0][c0].is_some() {
                    continue;
                }
                let mut rs = c.row_span.max(1).min(num_rows - r0);
                let mut cs = c.col_span.max(1).min(num_cols - c0);
                let free = (r0..r0 + rs).all(|r| (c0..c0 + cs).all(|q| owner[r][q].is_none()));
                if !free {
                    (rs, cs) = (1, 1);
                }
                for row in owner.iter_mut().skip(r0).take(rs) {
                    for slot in row.iter_mut().skip(c0).take(cs) {
                        *slot = Some(placed.len());
                    }
                }
                placed.push(Placed { k, rs, cs });
            }
            // The header: the longest run of leading rows wholly covered by
            // column-header cells that do not span past it (and leaving a
            // body under it).
            for h in 1..num_rows {
                let rows_ok = (0..h).all(|r| {
                    (0..num_cols).all(|c| {
                        owner[r][c].is_some_and(|p| {
                            let cell = &cells[placed[p].k];
                            cell.column_header && cell.start_row + placed[p].rs <= h
                        })
                    })
                });
                if !rows_ok {
                    break;
                }
                header_rows = h;
            }
            // Row-major at the anchors: Typst's auto-placement skips the
            // positions a span covers, and a position no cell covers is an
            // empty cell.
            for (r, owned) in owner.iter().enumerate() {
                for (c, slot) in owned.iter().enumerate() {
                    match *slot {
                        Some(p) => {
                            let Placed { k, rs, cs } = placed[p];
                            let cell = &cells[k];
                            if cell.start_row != r || cell.start_col != c {
                                continue;
                            }
                            let content = cell_content(ctx, t, (r, c), &cell.text);
                            rows[r].push(if cs > 1 || rs > 1 {
                                let mut args = Vec::new();
                                if cs > 1 {
                                    args.push(format!("colspan: {cs}"));
                                }
                                if rs > 1 {
                                    args.push(format!("rowspan: {rs}"));
                                }
                                format!("table.cell({})[{content}]", args.join(", "))
                            } else {
                                format!("[{content}]")
                            });
                        }
                        None => rows[r].push("[]".to_string()),
                    }
                }
            }
        }
        None => {
            for (r, row) in t.rows.iter().enumerate() {
                for c in 0..num_cols {
                    let flat = row.get(c).map(String::as_str).unwrap_or("");
                    let content = cell_content(ctx, t, (r, c), flat);
                    rows[r].push(format!("[{content}]"));
                }
            }
            // The default structure: row 0 is the header (Markdown's
            // reading), when there is a body under it.
            let declared = t
                .structure
                .as_ref()
                .map(|s| s.header_row.iter().take_while(|h| **h).count());
            header_rows = match declared {
                Some(h) => h.min(num_rows),
                None => usize::from(num_rows > 1),
            };
            if header_rows == num_rows {
                header_rows = 0;
            }
        }
    }
    let caption = t
        .caption
        .as_deref()
        .map(inline_cell)
        .filter(|c| !c.trim().is_empty());
    let notes: Vec<String> = t
        .footnotes
        .iter()
        .map(|n| inline_md(n))
        .filter(|n| !n.trim().is_empty())
        .collect();
    if num_cols == 0 {
        let mut out: Vec<String> = caption.into_iter().collect();
        out.extend(notes);
        return out.join("\n\n");
    }
    let mut lines = vec![format!("table(\n  columns: {num_cols},")];
    if header_rows > 0 {
        lines.push("  table.header(".to_string());
        for row in &rows[..header_rows] {
            if !row.is_empty() {
                lines.push(format!("    {},", row.join(", ")));
            }
        }
        lines.push("  ),".to_string());
    }
    for row in &rows[header_rows..] {
        if !row.is_empty() {
            lines.push(format!("  {},", row.join(", ")));
        }
    }
    lines.push(")".to_string());
    let table = lines.join("\n");
    let block = match caption {
        Some(c) => {
            let indented = table.replace('\n', "\n  ");
            format!("#figure(\n  {indented},\n  caption: [{c}],\n)")
        }
        None => format!("#{table}"),
    };
    let mut out = vec![block];
    out.extend(notes);
    out.join("\n\n")
}

// ---------------------------------------------------------------------------
// Lists
// ---------------------------------------------------------------------------

fn level_of(node: &Node) -> u8 {
    match node {
        Node::ListItem { level, .. } => *level,
        _ => 0,
    }
}

/// Split a run of list items into sibling lists at the base level, at the
/// backend's `first_in_list` boundaries (as the Markdown / LaTeX exports).
fn sibling_lists(run: &[Node]) -> Vec<&[Node]> {
    let base = level_of(&run[0]);
    let mut groups = Vec::new();
    let mut seg = 0;
    for k in 0..run.len() {
        let Node::ListItem {
            first_in_list,
            level,
            ..
        } = &run[k]
        else {
            continue;
        };
        if *level != base {
            continue;
        }
        if k > seg && *first_in_list {
            groups.push(&run[seg..k]);
            seg = k;
        }
    }
    groups.push(&run[seg..]);
    groups
}

/// One list: `- item` / `+ item` lines indented by `depth` steps of two
/// spaces; deeper successors nest one step further. An item's continuation
/// lines are indented under its text. Two sibling lists in a row are
/// separated by a blank line, which Typst keeps as two lists.
fn render_list(items: &[Node], depth: usize) -> String {
    let base = level_of(&items[0]);
    let pad = "  ".repeat(depth);
    let mut lines: Vec<String> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let Node::ListItem {
            text,
            dclx,
            layer,
            ordered,
            ..
        } = &items[i]
        else {
            i += 1;
            continue;
        };
        if level_of(&items[i]) > base {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < items.len() && level_of(&items[j]) > base {
            j += 1;
        }
        if layer.is_none() {
            let ordered = dclx.as_ref().map_or(*ordered, |d| d.ordered);
            let text = dclx
                .as_ref()
                .filter(|d| !d.text.is_empty())
                .map_or(text.as_str(), |d| d.text.as_str());
            let marker = if ordered { "+" } else { "-" };
            let body = inline_md(text).replace('\n', &format!("\n{pad}  "));
            lines.push(format!("{pad}{marker} {body}").trim_end().to_string());
            if j > i + 1 {
                let nested: Vec<Node> = items[i + 1..j]
                    .iter()
                    .filter(|n| matches!(n, Node::ListItem { .. }))
                    .cloned()
                    .collect();
                if !nested.is_empty() {
                    for group in sibling_lists(&nested) {
                        let text = render_list(group, depth + 1);
                        if !text.is_empty() {
                            lines.push(text);
                        }
                    }
                }
            }
        }
        i = j;
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// LaTeX math → Typst math
// ---------------------------------------------------------------------------

/// Translate a LaTeX math source into Typst math, or `None` when it uses a
/// construct outside the supported subset (environments, alignment `&`,
/// `\\` rows, unknown macros) — the caller then keeps the LaTeX verbatim.
/// Literal delimiters are backslash-escaped so they never group, letters
/// are spaced apart (Typst reads `ab` as one identifier), symbols are
/// written as their Unicode characters (stable across Typst versions,
/// unlike the symbol names).
pub fn latex_to_typst_math(latex: &str) -> Option<String> {
    let chars: Vec<char> = latex.chars().collect();
    let mut p = MathParser { chars, pos: 0 };
    let out = p.seq(false)?;
    if p.pos != p.chars.len() {
        return None;
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

struct MathParser {
    chars: Vec<char>,
    pos: usize,
}

const MATH_FUNCS: &[&str] = &[
    "sin", "cos", "tan", "cot", "sec", "csc", "sinh", "cosh", "tanh", "coth", "arcsin", "arccos",
    "arctan", "log", "ln", "lg", "exp", "lim", "liminf", "limsup", "max", "min", "sup", "inf",
    "det", "dim", "ker", "deg", "gcd", "arg", "Pr",
];

fn math_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "cdot" => "⋅",
        "times" => "×",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "∙",
        "leq" | "le" => "≤",
        "geq" | "ge" => "≥",
        "neq" | "ne" => "≠",
        "ll" => "≪",
        "gg" => "≫",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "propto" => "∝",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "setminus" => "∖",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "to" | "rightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" | "implies" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" | "iff" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "prime" => "′",
        "perp" => "⊥",
        "parallel" => "∥",
        "angle" => "∠",
        "mid" | "vert" => "|",
        "Vert" => "‖",
        _ => return None,
    })
}

impl MathParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    /// A command name after `\`: a letter run, or one non-letter.
    fn command(&mut self) -> Option<String> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        if self.pos == start {
            let c = self.peek()?;
            self.pos += 1;
            return Some(c.to_string());
        }
        Some(self.chars[start..self.pos].iter().collect())
    }

    /// Tokens up to the end, or to the `}` closing a group when `group`.
    fn seq(&mut self, group: bool) -> Option<String> {
        let mut out = String::new();
        loop {
            self.skip_ws();
            match self.peek() {
                None if group => return None,
                None => return Some(out),
                Some('}') if group => {
                    self.pos += 1;
                    return Some(out);
                }
                Some('}') => return None,
                Some('^' | '_') => {
                    let op = self.peek()?;
                    self.pos += 1;
                    let arg = self.argument()?;
                    if out.trim_end().is_empty() {
                        out.push_str("\"\"");
                    }
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push(op);
                    out.push('(');
                    out.push_str(&arg);
                    out.push_str(") ");
                }
                Some('\'') => {
                    self.pos += 1;
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push_str("' ");
                }
                Some(_) => {
                    let atom = self.atom()?;
                    if !atom.is_empty() {
                        out.push_str(&atom);
                        out.push(' ');
                    }
                }
            }
        }
    }

    /// A script's / macro's argument: a `{…}` group or one atom.
    fn argument(&mut self) -> Option<String> {
        self.skip_ws();
        match self.peek()? {
            '{' => {
                self.pos += 1;
                Some(self.seq(true)?.trim().to_string())
            }
            '}' | '^' | '_' => None,
            _ => Some(self.atom()?.trim().to_string()),
        }
    }

    /// A function call's argument: [`Self::argument`], refused when it
    /// holds a top-level `,` / `;` (they would split the call's arguments).
    fn call_argument(&mut self) -> Option<String> {
        let arg = self.argument()?;
        let mut depth = 0i32;
        let mut prev = ' ';
        for c in arg.chars() {
            match c {
                '(' if prev != '\\' => depth += 1,
                ')' if prev != '\\' => depth -= 1,
                ',' | ';' if depth == 0 && prev != '\\' => return None,
                _ => {}
            }
            prev = c;
        }
        (!arg.is_empty()).then_some(arg)
    }

    /// The text of a `{…}` argument read verbatim (`\text{…}`).
    fn text_argument(&mut self) -> Option<String> {
        self.skip_ws();
        if self.peek()? != '{' {
            return None;
        }
        self.pos += 1;
        let start = self.pos;
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => {
                    let text: String = self.chars[start..self.pos].iter().collect();
                    self.pos += 1;
                    return Some(text);
                }
                '}' => depth -= 1,
                '\\' => return None,
                _ => {}
            }
            self.pos += 1;
        }
        None
    }

    fn atom(&mut self) -> Option<String> {
        let c = self.peek()?;
        self.pos += 1;
        Some(match c {
            '{' => self.seq(true)?.trim().to_string(),
            '\\' => return self.macro_atom(),
            c if c.is_ascii_digit() => {
                let start = self.pos - 1;
                while self.peek().is_some_and(|d| d.is_ascii_digit() || d == '.') {
                    self.pos += 1;
                }
                let mut num: String = self.chars[start..self.pos].iter().collect();
                // A trailing `.` is punctuation, not a decimal point.
                if num.ends_with('.') {
                    num.pop();
                    self.pos -= 1;
                }
                num
            }
            c if c.is_alphabetic() => c.to_string(),
            '+' | '-' | '=' | '<' | '>' | ',' | ';' | ':' | '!' | '|' | '?' | '.' => c.to_string(),
            '*' => "∗".to_string(),
            '(' | ')' | '[' | ']' | '/' => format!("\\{c}"),
            '~' => String::new(),
            c if !c.is_ascii() && !c.is_control() => c.to_string(),
            _ => return None,
        })
    }

    fn macro_atom(&mut self) -> Option<String> {
        let name = self.command()?;
        if let Some(sym) = math_symbol(&name) {
            return Some(sym.to_string());
        }
        if MATH_FUNCS.contains(&name.as_str()) {
            return Some(name);
        }
        Some(match name.as_str() {
            "frac" | "dfrac" | "tfrac" => {
                let num = self.call_argument()?;
                let den = self.call_argument()?;
                format!("frac({num}, {den})")
            }
            "sqrt" => {
                self.skip_ws();
                if self.peek() == Some('[') {
                    self.pos += 1;
                    let start = self.pos;
                    while self.peek().is_some_and(|c| c != ']') {
                        self.pos += 1;
                    }
                    let index: String = self.chars[start..self.pos].iter().collect();
                    self.peek()?;
                    self.pos += 1;
                    let index = latex_to_typst_math(&index)?;
                    if index.contains([',', ';']) {
                        return None;
                    }
                    let radicand = self.call_argument()?;
                    format!("root({index}, {radicand})")
                } else {
                    format!("sqrt({})", self.call_argument()?)
                }
            }
            "mathbf" | "boldsymbol" | "bm" => format!("bold({})", self.call_argument()?),
            "mathit" => format!("italic({})", self.call_argument()?),
            "mathcal" => format!("cal({})", self.call_argument()?),
            "mathbb" => format!("bb({})", self.call_argument()?),
            "mathfrak" => format!("frak({})", self.call_argument()?),
            "mathsf" => format!("sans({})", self.call_argument()?),
            "mathtt" => format!("mono({})", self.call_argument()?),
            "mathrm" | "text" | "textrm" | "textit" | "textbf" | "mbox" => {
                string_lit(&self.text_argument()?)
            }
            "operatorname" => format!("op({})", string_lit(&self.text_argument()?)),
            "hat" | "widehat" => format!("hat({})", self.call_argument()?),
            "bar" | "overline" => format!("overline({})", self.call_argument()?),
            "underline" => format!("underline({})", self.call_argument()?),
            "tilde" | "widetilde" => format!("tilde({})", self.call_argument()?),
            "vec" => format!("arrow({})", self.call_argument()?),
            "dot" => format!("dot({})", self.call_argument()?),
            "ddot" => format!("dot.double({})", self.call_argument()?),
            // Sizing hints: the delimiter that follows is kept as a literal.
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "Bigl"
            | "Bigr" => {
                self.skip_ws();
                if self.peek() == Some('.') {
                    self.pos += 1;
                }
                String::new()
            }
            "displaystyle" | "textstyle" | "limits" | "nolimits" => String::new(),
            "," | "thinspace" => "thin".to_string(),
            ":" | ";" | ">" => "med".to_string(),
            "!" => String::new(),
            " " => "space".to_string(),
            "quad" => "quad".to_string(),
            "qquad" => "wide".to_string(),
            "{" | "}" | "%" | "&" | "#" | "$" | "_" => format!("\\{name}"),
            "|" => "‖".to_string(),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{DoclingDocument, Node, TableCell};

    fn doc(nodes: Vec<Node>) -> DoclingDocument {
        let mut d = DoclingDocument::new("t");
        d.nodes = nodes;
        d
    }

    fn typ(nodes: Vec<Node>) -> String {
        to_typst(&doc(nodes), &TypstExportOptions::default()).0
    }

    fn para(text: &str) -> Node {
        Node::Paragraph { text: text.into() }
    }

    #[test]
    fn empty_document_is_empty() {
        assert_eq!(typ(Vec::new()), "");
    }

    #[test]
    fn headings_by_level() {
        let out = typ(vec![
            Node::Heading {
                level: 1,
                text: "Title".into(),
            },
            Node::Heading {
                level: 3,
                text: "Sub *sec*".into(),
            },
            para("Body."),
        ]);
        assert_eq!(out, "= Title\n\n=== Sub #emph[sec]\n\nBody.\n");
    }

    #[test]
    fn escapes_markup_characters() {
        assert_eq!(
            escape_typst(r"a#b $5 *x* _y_ `c` <l> @r [s] ~ \ end", false),
            r"a\#b \$5 \*x\* \_y\_ \`c\` \<l\> \@r \[s\] \~ \\ end"
        );
        // Line-start markers only at a line start.
        assert_eq!(escape_typst("= no heading", true), r"\= no heading");
        assert_eq!(escape_typst("- no list", true), r"\- no list");
        assert_eq!(escape_typst("+ no enum", true), r"\+ no enum");
        assert_eq!(escape_typst("/ no term: x", true), r"\/ no term: x");
        assert_eq!(escape_typst("12. no enum", true), r"12\. no enum");
        assert_eq!(escape_typst("a - b = c", true), "a - b = c");
        // Comments and dash shorthands.
        assert_eq!(escape_typst("a // b /* c", false), r"a \// b \/\* c");
        assert_eq!(escape_typst("a -- b -? c", false), r"a \-- b \-? c");
        // A newline is a line break; the next line is a line start again.
        assert_eq!(escape_typst("one\n\n- two", true), "one \\\n\\- two");
    }

    #[test]
    fn inline_formatting_and_links() {
        let out = typ(vec![para(
            "A **bold** and *it* and ~~gone~~, `x\"y` and [site](https://e.org/a_b) here.",
        )]);
        assert_eq!(
            out,
            "A #strong[bold] and #emph[it] and #strike[gone], #raw(\"x\\\"y\") and \
             #link(\"https://e.org/a_b\")[site] here.\n"
        );
        // A `.` or `(` right after an embedded call would continue it.
        assert_eq!(typ(vec![para("**a**. (b)")]), "#strong[a]. (b)\n");
        assert_eq!(typ(vec![para("**a**.b")]), "#strong[a];.b\n");
        assert_eq!(typ(vec![para("**a**(b)")]), "#strong[a];(b)\n");
        // An unsafe link keeps its text.
        assert_eq!(typ(vec![para("[x](javascript:alert(1))")]), "x\n");
    }

    #[test]
    fn lists_nest_and_order() {
        let item = |level: u8, ordered: bool, first: bool, text: &str| Node::ListItem {
            ordered,
            number: 1,
            first_in_list: first,
            text: text.into(),
            level,
            marker: None,
            location: None,
            dclx: None,
            split: None,
            href: None,
            layer: None,
        };
        let out = typ(vec![
            item(0, false, true, "one"),
            item(1, true, true, "one.a"),
            item(1, true, false, "one.b"),
            item(0, false, false, "two"),
            item(0, true, true, "fresh list"),
        ]);
        assert_eq!(out, "- one\n  + one.a\n  + one.b\n- two\n\n+ fresh list\n");
    }

    #[test]
    fn code_blocks_fence_and_language() {
        let out = typ(vec![Node::Code {
            language: Some("Python".into()),
            text: "print(\"```\")".into(),
            orig: None,
            pretty: None,
        }]);
        assert_eq!(out, "````python\nprint(\"```\")\n````\n");
    }

    #[test]
    fn table_with_header_and_spans() {
        let cell = |r, c, rs, cs, text: &str, header| TableCell {
            text: text.into(),
            start_row: r,
            start_col: c,
            row_span: rs,
            col_span: cs,
            column_header: header,
            bbox: None,
            row_header: false,
            row_section: false,
        };
        let mut t = Table {
            rows: vec![
                vec!["H".into(), "H".into()],
                vec!["a".into(), "b".into()],
                vec!["a".into(), "c".into()],
            ],
            ..Default::default()
        };
        t.cells = Some(vec![
            cell(0, 0, 1, 2, "H", true),
            cell(1, 0, 2, 1, "a", false),
            cell(1, 1, 1, 1, "b", false),
            cell(2, 1, 1, 1, "c_d", false),
        ]);
        t.caption = Some("Cap".into());
        t.footnotes = vec!["Note.".into()];
        let out = typ(vec![Node::Table(t)]);
        assert_eq!(
            out,
            "#figure(\n  table(\n    columns: 2,\n    table.header(\n      \
             table.cell(colspan: 2)[H],\n    ),\n    table.cell(rowspan: 2)[a], [b],\n    \
             [c\\_d],\n  ),\n  caption: [Cap],\n)\n\nNote.\n"
        );
    }

    #[test]
    fn plain_grid_table() {
        let t = Table {
            rows: vec![vec!["x".into(), "y".into()], vec!["1".into()]],
            ..Default::default()
        };
        assert_eq!(
            typ(vec![Node::Table(t)]),
            "#table(\n  columns: 2,\n  table.header(\n    [x], [y],\n  ),\n  [1], [],\n)\n"
        );
    }

    #[test]
    fn pictures_placeholder_and_referenced() {
        let png = crate::document::PictureImage {
            mimetype: "image/png".into(),
            width: 1,
            height: 1,
            data: vec![1, 2, 3],
            dpi: 72,
            display_size: None,
        };
        let pic =
            |image: Option<crate::document::PictureImage>, caption: Option<&str>| Node::Picture {
                caption: caption.map(Into::into),
                caption_href: None,
                image,
                classification: None,
                description: None,
                caption_parent: Default::default(),
                caption_location: None,
            };
        let d = doc(vec![
            pic(Some(png.clone()), Some("A *cap*")),
            pic(Some(png.clone()), None),
            pic(None, None),
        ]);
        let (placeholder, files) = to_typst(&d, &TypstExportOptions::default());
        assert!(files.is_empty());
        assert_eq!(
            placeholder,
            "#figure(\n  [/* image */],\n  caption: [A #emph[cap]],\n)\n\n/* image */\n\n/* image */\n"
        );
        let (refd, files) = to_typst(
            &d,
            &TypstExportOptions {
                image_mode: ImageMode::Referenced,
                artifacts_dir: "doc_artifacts".into(),
            },
        );
        assert_eq!(
            refd,
            "#figure(\n  image(\"doc_artifacts/image_000000.png\"),\n  caption: [A #emph[cap]],\n)\
             \n\n#image(\"doc_artifacts/image_000001.png\")\n\n/* image */\n"
        );
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].0, "doc_artifacts/image_000001.png");
        assert_eq!(files[1].1, vec![1, 2, 3]);
    }

    #[test]
    fn formulas_translate_or_stay_latex() {
        assert_eq!(
            latex_to_typst_math(r"\frac{a+b}{2} \leq \sqrt{x^{2}} + \alpha_i").as_deref(),
            Some("frac(a + b, 2) ≤ sqrt(x^(2)) + α_(i)")
        );
        assert_eq!(
            latex_to_typst_math(r"\sum_{i=1}^{n} \mathbf{x}_i").as_deref(),
            Some("∑_(i = 1)^(n) bold(x)_(i)")
        );
        assert_eq!(
            latex_to_typst_math(r"\left( ab \right] / 3.5.").as_deref(),
            Some(r"\( a b \] \/ 3.5 .")
        );
        assert_eq!(
            latex_to_typst_math(r"\mathrm{d}x \text{ if } f'(x)").as_deref(),
            Some("\"d\" x \" if \" f' \\( x \\)")
        );
        // Outside the subset → verbatim LaTeX.
        assert_eq!(
            latex_to_typst_math(r"\begin{pmatrix}a&b\end{pmatrix}"),
            None
        );
        assert_eq!(latex_to_typst_math(r"\frac{a,b}{c}"), None);
        assert_eq!(latex_to_typst_math(r"\unknownmacro x"), None);
        assert_eq!(latex_to_typst_math("{a"), None);
        let out = typ(vec![
            para("$$E = mc^2$$"),
            para(r"Inline $\alpha$ and $\weird$ and $5 apples."),
            Node::Formula {
                latex: r"\begin{cases}1\end{cases}".into(),
                orig: String::new(),
                location: None,
            },
        ]);
        assert_eq!(
            out,
            "$ E = m c^(2) $\n\nInline $α$ and #raw(\"\\\\weird\", lang: \"latex\") and \\$5 apples.\
             \n\n```latex\n\\begin{cases}1\\end{cases}\n```\n"
        );
    }

    #[test]
    fn rich_cells_render_their_blocks() {
        let t = Table {
            rows: vec![vec!["x".into()]],
            cell_blocks: Some(vec![vec![vec![para("p1"), para("*p2*")]]]),
            ..Default::default()
        };
        assert_eq!(
            typ(vec![Node::Table(t)]),
            "#table(\n  columns: 1,\n  [p1\n\n#emph[p2]],\n)\n"
        );
    }
}
