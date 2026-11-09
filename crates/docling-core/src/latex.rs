//! LaTeX serializer — the Rust counterpart of docling-core's
//! `LaTeXDocSerializer` with default `LaTeXParams` (docling 2.124's
//! `--to latex`, #317), scored byte-for-byte against upstream's own output
//! (`docling --to latex`) on the shared declarative corpus.
//!
//! The Python serializer walks the *JSON* document model, so this one mirrors
//! what [`crate::json`] emits for each [`Node`] rather than the node itself:
//! a level-1 heading is a `title` item, deeper headings are `section_header`s
//! one level down, a `$$…$$` paragraph is a formula item, runs of list items
//! fold into (sibling / nested) list groups, form regions are `field_region`
//! / `field_item` items the upstream fallback renders as `% missing-text`,
//! and everything docling keeps out of the JSON body (furniture, page breaks,
//! DocLang-only nodes) is absent here too. Text goes through the same
//! `unescape_text` the JSON export applies, then LaTeX escaping — exactly the
//! `post_process` order upstream uses.
//!
//! Upstream quirks reproduced on purpose: the first `\title{…}` without nested
//! braces is hoisted into the preamble (a `\maketitle` takes its place), the
//! table grid repeats a spanning cell's text over every covered position, and
//! the placeholder picture is `% image` whether or not an image is attached.
//!
//! Inline markup: the [`Node`] model bakes docling's Markdown rendering of
//! inline formatting into its text (`**bold**`, `*italic*`, `~~strike~~`,
//! `` `code` ``, `[text](url)`, `$formula$`), whereas upstream keeps them as
//! separate items with a `Formatting` / `hyperlink` and its LaTeX serializer
//! renders `\textbf{}` / `\textit{}` / `\sout{}` / `\texttt{}` / `\href{}{}` /
//! `$…$`. [`inline_md`] parses the markers back into those commands (nesting
//! order `\textit{\textbf{…}}` as upstream's `post_process` applies bold first),
//! so the output matches what Python docling produces natively from the same
//! source — not merely what it would produce from the exported JSON.
//! Underline and sub/superscript have no Markdown form and stay plain text.
//! Rich table cells (`Table::cell_blocks`) render their block content inline
//! the way upstream's `RichTableCell` does (a nested `itemize`, a nested
//! `table`), newlines flattened to spaces.
//!
//! Deliberate deviation: upstream *raises* on a section header deeper than
//! `\subsubsection`; a conversion should not fail on an `<h5>`, so those
//! degrade to `\paragraph` / `\subparagraph`. (Until docling-core 2.95 the
//! upstream serializer also emitted the text of a formatted list item or
//! heading *twice* — inside `\item` / `\section{}` and again as the inline
//! group's own paragraph, docling-core#740; that was never reproduced here
//! and docling-core#743 fixed it upstream, so the outputs agree again.)

use crate::document::{DoclingDocument, FieldItem, Node, Table};

const DOCUMENT_CLASS: &str = r"\documentclass[11pt,a4paper]{article}";
const PACKAGES: [&str; 11] = [
    r"\usepackage[utf8]{inputenc} % allow utf-8 input",
    r"\usepackage[T1]{fontenc}    % use 8-bit T1 fonts",
    r"\usepackage{hyperref}       % hyperlinks",
    r"\usepackage{url}            % simple URL typesetting",
    r"\usepackage{booktabs}       % professional-quality tables",
    r"\usepackage{amsfonts}       % blackboard math symbols",
    r"\usepackage{nicefrac}       % compact symbols for 1/2, etc.",
    r"\usepackage{microtype}      % microtypography",
    r"\usepackage{xcolor}         % colors",
    r"\usepackage{graphicx}       % graphics",
    r"\usepackage[normalem]{ulem} % strikethrough",
];
const IMAGE_PLACEHOLDER: &str = "% image";
const MISSING_TEXT: &str = "% missing-text";
/// Spaces per nesting level on a nested list's `\begin`/`\end` lines.
const INDENT: usize = 2;

/// Serialize `doc` to a complete LaTeX document. No trailing newline — the
/// upstream CLI writes the serializer's text verbatim into `<stem>.tex`.
pub fn to_latex(doc: &DoclingDocument) -> String {
    let parts = render_nodes(&doc.nodes, 1, false);
    let mut body = parts.join("\n\n");

    // Hoist the first `\title{…}` into the preamble, drop every such
    // occurrence from the body, collapse the blank runs that leaves behind.
    // The group is closed by brace depth, escaped braces skipped
    // (docling-core#983), so a formatted title (`\title{\textbf{Bold}}`) or
    // one with `\{` is hoisted too; an unclosed one stays in the body.
    let mut title_cmd: Option<String> = None;
    let mut out = String::with_capacity(body.len());
    let mut pos = 0;
    let mut search = 0;
    while let Some((start, content_start, close)) = find_title(&body, search) {
        if title_cmd.is_none() {
            title_cmd = Some(format!("\\title{{{}}}", &body[content_start..close]));
        }
        out.push_str(&body[pos..start]);
        pos = close + 1;
        search = pos;
    }
    if title_cmd.is_some() {
        out.push_str(&body[pos..]);
        body = collapse_blank_runs(&out).trim().to_string();
    }

    let mut preamble: Vec<&str> = vec![DOCUMENT_CLASS, ""];
    preamble.extend(PACKAGES);
    let mut header = preamble.join("\n");
    if let Some(t) = &title_cmd {
        header.push('\n');
        header.push_str(t);
    }
    header.push_str("\n\n\\begin{document}");

    let mut blocks: Vec<&str> = Vec::new();
    if title_cmd.is_some() {
        blocks.push("\\maketitle");
    }
    if !body.is_empty() {
        blocks.push(&body);
    }
    if blocks.is_empty() {
        format!("{header}\n\n\\end{{document}}")
    } else {
        format!("{header}\n\n{}\n\n\\end{{document}}", blocks.join("\n\n"))
    }
}

/// Upstream's `_post_process_title` scan (docling-core#983): a
/// `\\title\s*\{` match, closed where the brace depth returns to zero over
/// `\\.|[{}]` tokens (so `\{` / `\}` do not count). Returns (match start,
/// content start, closing-brace index) of the first closed one at or after
/// `from`; an unclosed match is skipped, as upstream leaves it in the body.
fn find_title(text: &str, from: usize) -> Option<(usize, usize, usize)> {
    let bytes = text.as_bytes();
    let mut search = from;
    while let Some(rel) = text[search..].find("\\title") {
        let start = search + rel;
        let mut i = start + "\\title".len();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) == Some(&b'{') {
            let content_start = i + 1;
            let mut depth = 1usize;
            let mut j = content_start;
            while j < bytes.len() {
                match bytes[j] {
                    // `\\.` with DOTALL: the escaped character, whatever it
                    // is (a multi-byte one is never a brace, so skipping
                    // one byte and resuming on a continuation byte is safe).
                    b'\\' => j += 1,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((start, content_start, j));
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        search = start + 1;
    }
    None
}

/// `re.sub(r"\n{3,}", "\n\n", …)`.
fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    for ch in text.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(ch);
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out
}

/// Upstream's `_escape_latex`: single pass, character by character.
pub fn escape_latex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str(r"\textbackslash{}"),
            '{' => out.push_str(r"\{"),
            '}' => out.push_str(r"\}"),
            '#' => out.push_str(r"\#"),
            '$' => out.push_str(r"\$"),
            '%' => out.push_str(r"\%"),
            '&' => out.push_str(r"\&"),
            '_' => out.push_str(r"\_"),
            '~' => out.push_str(r"\textasciitilde{}"),
            '^' => out.push_str(r"\textasciicircum{}"),
            c => out.push(c),
        }
    }
    out
}

/// The JSON export's `unescape_text`: the model text is the Markdown-free
/// raw string, which is what upstream then LaTeX-escapes.
fn unescape_text(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("\\_", "_")
}

fn text_item(s: &str) -> String {
    escape_latex(&unescape_text(s))
}

/// Upstream escapes only `#` in inline code — and does so with a doubled
/// backslash (its `r"\\#"`), reproduced verbatim. `raw` is the unescaped text.
fn code_inline(raw: &str) -> String {
    format!("\\texttt{{{}}}", raw.replace('#', "\\\\#"))
}

// ---------------------------------------------------------------------------
// Inline markup — Markdown markers back into LaTeX commands (module docs).
// ---------------------------------------------------------------------------

/// Render a text carrying docling's Markdown inline markers as LaTeX inline
/// content: plain runs are escaped, `**` / `*` / `***` / `~~` spans become
/// `\textbf` / `\textit` / `\textit{\textbf{}}` / `\sout`, a `` `code` `` span
/// `\texttt` (only `#` escaped, as upstream), `[text](url)` an `\href`, and a
/// `$…$` span an inline formula copied verbatim. A literal `$$` in running
/// text is not a formula (the Markdown backend keeps it as text), and a
/// blank-only span (`* *`) is not a formatting run.
fn inline_md(md: &str) -> String {
    let chars: Vec<char> = md.chars().collect();
    render_inline(&chars, true)
}

/// [`inline_md`] for a plain table cell: upstream escapes a `TableCell`'s
/// text wholesale, so a `$…$` there stays literal text (a docx cell holding
/// an equation), while formatted runs still arrive as rich cells and render.
fn inline_cell(md: &str) -> String {
    let chars: Vec<char> = md.chars().collect();
    render_inline(&chars, false)
}

fn starts_with(chars: &[char], at: usize, pat: &str) -> bool {
    let mut it = chars[at..].iter();
    pat.chars().all(|p| it.next() == Some(&p))
}

fn find(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    let plen = pat.chars().count();
    (from..chars.len().saturating_sub(plen - 1)).find(|&i| starts_with(chars, i, pat))
}

fn render_inline(chars: &[char], formulas: bool) -> String {
    let n = chars.len();
    let mut out = String::new();
    let mut plain = String::new();
    fn flush(out: &mut String, plain: &mut String) {
        if !plain.is_empty() {
            out.push_str(&text_item(plain));
            plain.clear();
        }
    }
    let mut i = 0;
    while i < n {
        // A hyperlink `[label](url)` (not an image `![…](…)`, not the empty
        // `[](…)`); the label may hold balanced brackets, the URL balanced
        // parentheses.
        if chars[i] == '[' && !starts_with(chars, i, "[](") && !(i > 0 && chars[i - 1] == '!') {
            if let Some((label_end, url_end)) = link_bounds(chars, i) {
                flush(&mut out, &mut plain);
                let url: String = chars[label_end + 2..url_end].iter().collect();
                out.push_str(&hyperlink(&chars[i + 1..label_end], &url));
                i = url_end + 1;
                continue;
            }
        }
        // A fenced code block flattened into the text (a rich cell's block
        // code): upstream's `CodeItem` renders `verbatim`.
        if starts_with(chars, i, "```") {
            // The closing fence is the opening one's length (#859).
            if let Some((s, e, after)) = crate::markdown::code_span_bounds(chars, i) {
                flush(&mut out, &mut plain);
                let raw: String = chars[s..e].iter().collect();
                let raw = unescape_text(raw.trim());
                out.push_str(&format!("\\begin{{verbatim}}\n{raw}\n\\end{{verbatim}}"));
                i = after;
                continue;
            }
        }
        // A code span, its backticks possibly more than one (#859).
        if chars[i] == '`' {
            if let Some((s, e, after)) = crate::markdown::code_span_bounds(chars, i) {
                let inner = &chars[s..e];
                if !inner.is_empty() && !inner.iter().all(|c| c.is_whitespace()) {
                    flush(&mut out, &mut plain);
                    out.push_str(&span("`", inner));
                    i = after;
                    continue;
                }
            }
        }
        // A formatted span; longest markers first.
        let mut matched = false;
        for marker in ["***", "**", "*", "~~"] {
            if starts_with(chars, i, marker) {
                let mlen = marker.chars().count();
                if let Some(end) = find(chars, i + mlen, marker) {
                    let inner = &chars[i + mlen..end];
                    if !inner.is_empty() && !inner.iter().all(|c| c.is_whitespace()) {
                        flush(&mut out, &mut plain);
                        out.push_str(&span(marker, inner));
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
            plain.push_str("$$");
            i += 2;
            continue;
        }
        // An inline formula: LaTeX source, never escaped. Pandoc's
        // `tex_math_dollars` rule tells it from currency: a non-space right
        // after the opening `$`, a non-space right before the closing one, and
        // no digit right after it (`$9 million ($158 million today)` is text).
        if formulas && chars[i] == '$' {
            if let Some(end) = find(chars, i + 1, "$") {
                let non_space = |k: usize| chars.get(k).is_some_and(|c| !c.is_whitespace());
                let digit_after = chars.get(end + 1).is_some_and(|c| c.is_ascii_digit());
                if end > i + 1 && non_space(i + 1) && non_space(end - 1) && !digit_after {
                    flush(&mut out, &mut plain);
                    out.push('$');
                    out.extend(&chars[i + 1..end]);
                    out.push('$');
                    i = end + 1;
                    continue;
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush(&mut out, &mut plain);
    out
}

/// For a `[` at `open`: the index of the `](` closing the label (with balanced
/// brackets inside) and of the `)` closing the URL (balanced parentheses).
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

/// A span whose content is exactly one code span or inline formula: upstream
/// models it as a `CodeItem` / `FormulaItem`, which take neither their
/// ancestors' formatting nor a hyperlink.
fn is_bare_item(inner: &[char]) -> bool {
    let n = inner.len();
    if n < 3 {
        return false;
    }
    if inner[0] == '`' {
        return crate::markdown::code_span_bounds(inner, 0).is_some_and(|(_, _, after)| after == n);
    }
    let body = &inner[1..n - 1];
    inner[0] == '$' && inner[n - 1] == '$' && inner[1] != '$' && !body.contains(&'$')
}

fn span(marker: &str, inner: &[char]) -> String {
    if marker == "`" {
        let raw: String = inner.iter().collect();
        return code_inline(&unescape_text(&raw));
    }
    let content = render_inline(inner, true);
    if is_bare_item(inner) {
        return content;
    }
    match marker {
        "***" => format!("\\textit{{\\textbf{{{content}}}}}"),
        "**" => format!("\\textbf{{{content}}}"),
        "*" => format!("\\textit{{{content}}}"),
        _ => format!("\\sout{{{content}}}"),
    }
}

/// `\href{url}{label}` — the URL LaTeX-escaped like upstream (`\_`, `\#`, …)
/// and normalized the way pydantic's `AnyUrl` does (a bare `scheme://host`
/// gains its `/` path).
fn hyperlink(label: &[char], url: &str) -> String {
    let content = render_inline(label, true);
    if is_bare_item(label) {
        return content;
    }
    let url = unescape_text(url);
    // An unsafe destination (`javascript:`, #860) keeps the text alone.
    if !crate::hyperlink::is_safe_hyperlink(&url) {
        return content;
    }
    let url = match url.find("://") {
        Some(p) => {
            let (scheme, rest) = url.split_at(p + 3);
            let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            if rest[host_end..].starts_with('/') || rest.is_empty() {
                url.clone()
            } else {
                format!("{scheme}{}/{}", &rest[..host_end], &rest[host_end..])
            }
        }
        None => url,
    };
    format!("\\href{{{}}}{{{content}}}", escape_latex(&url))
}

/// Serialize a sibling run of nodes into the non-empty parts upstream's
/// `get_parts` would collect at this level. `list_level` is the nesting depth
/// a list group *started here* renders at (1 at the document body: the outer
/// `\begin{itemize}` is unindented, its nested groups indent by one step).
fn render_nodes(nodes: &[Node], list_level: usize, inline: bool) -> Vec<String> {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        if matches!(nodes[i], Node::ListItem { .. }) {
            // Fold the run exactly like the JSON export's `walk_into`: list
            // items plus an empty paragraph sitting between two of them.
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
                let text = render_list(group, list_level - 1);
                if !text.is_empty() {
                    parts.push(text);
                }
            }
        } else {
            render_one(&nodes[i], list_level, inline, &mut parts);
            i += 1;
        }
    }
    parts
}

fn render_one(node: &Node, list_level: usize, inline: bool, parts: &mut Vec<String>) {
    let push = |parts: &mut Vec<String>, s: String| {
        if !s.is_empty() {
            parts.push(s);
        }
    };
    match node {
        // JSON: level 1 is the `title` item; deeper levels are section headers
        // one level down (1 → \section … 3 → \subsubsection).
        Node::Heading { level: 1, text } => push(parts, format!("\\title{{{}}}", inline_md(text))),
        Node::Heading { level, text } => {
            let cmd = match level {
                2 => "section",
                3 => "subsection",
                4 => "subsubsection",
                // Upstream raises here; degrade instead (module docs).
                5 => "paragraph",
                _ => "subparagraph",
            };
            push(parts, format!("\\{cmd}{{{}}}", inline_md(text)));
        }
        // A standalone caption item is a text item to the LaTeX serializer.
        Node::Caption { text, .. } => push(parts, inline_md(text)),
        Node::LabeledText { .. } => {
            render_one(&node.labeled_as_paragraph(), list_level, inline, parts)
        }
        Node::Paragraph { text } | Node::ListText { text, .. } => {
            // A whole-paragraph `$$…$$` is a formula item in the JSON — never
            // escaped, re-wrapped by the serializer.
            let t = text.trim();
            match t.strip_prefix("$$").and_then(|s| s.strip_suffix("$$")) {
                Some(inner) if !inner.is_empty() => push(parts, formula(inner, inline)),
                _ => push(parts, inline_md(text)),
            }
        }
        // Upstream has no LaTeX rendering for a checkbox item beyond its text:
        // the `- [x]` marker is Markdown-only.
        Node::CheckboxItem { text, .. } => push(parts, inline_md(text)),
        Node::Code { text, .. } => {
            let raw = unescape_text(text);
            if inline {
                push(parts, code_inline(&raw));
            } else {
                push(
                    parts,
                    format!("\\begin{{verbatim}}\n{raw}\n\\end{{verbatim}}"),
                );
            }
        }
        Node::Formula { latex, orig, .. } => {
            if !latex.is_empty() {
                push(parts, formula(latex, inline));
            } else if !orig.is_empty() {
                push(parts, "% formula-not-decoded".to_string());
            }
        }
        Node::Table(t) => push(parts, render_table(t)),
        // The picture's meta as comments in the figure, docling-core's
        // `LaTeXMetaSerializer` order (docling-core#871): the main
        // classification, then the description — an alt-text description
        // (#676, which docling does not have) is left out, as the default
        // Markdown and HTML leave it out.
        Node::Picture {
            caption,
            classification,
            description,
            ..
        } => {
            let mut comments = Vec::new();
            if let Some(class) = classification.as_deref().and_then(|c| c.first()) {
                comments.extend(annotation_comment(
                    "classification",
                    &class.class_name.replace('_', " "),
                ));
            }
            if let Some(d) = description.as_ref().filter(|d| !d.is_alt_text()) {
                comments.extend(annotation_comment("description", &d.text));
            }
            push(parts, render_figure(caption.as_deref(), &comments));
        }
        // A chart is a picture item classified as its chart kind and carrying
        // the chart's data: the figure with that classification, then the
        // data grid as a `table` after it (`enable_chart_tables`,
        // docling-core#871).
        Node::Chart {
            kind,
            table,
            caption,
            ..
        } => {
            let comments: Vec<String> =
                annotation_comment("classification", &kind.replace('_', " "))
                    .into_iter()
                    .collect();
            push(parts, render_figure(caption.as_deref(), &comments));
            push(parts, render_table(table));
        }
        // Upstream's fallback on a group joins its children with blank lines;
        // an `inline` group is docling's InlineGroup, joined with spaces in
        // inline scope.
        // A group on a non-body content layer (a hidden spreadsheet sheet) is
        // not rendered, like every other non-body item.
        Node::Group { layer: Some(_), .. } => {}
        Node::Group {
            label, children, ..
        } => {
            if label == "inline" {
                push(parts, render_nodes(children, list_level, true).join(" "));
            } else {
                push(
                    parts,
                    render_nodes(children, list_level, inline).join("\n\n"),
                );
            }
        }
        Node::FieldRegion { items } => render_field_region(items, parts),
        // A key-value graph has no LaTeX rendering (docling's serializer skips it).
        Node::KeyValueGraph { .. } => {}
        Node::InlineGroup { md_text, .. } => push(parts, inline_md(md_text)),
        Node::TextDump(text) => push(parts, text_item(text)),
        // Notes-layer comments are omitted; an annotated item renders itself.
        Node::CommentSection { .. } => {}
        Node::Commented { inner, .. } | Node::ScriptText { inner, .. } => {
            render_one(inner, list_level, inline, parts)
        }
        Node::Located { inner, .. } | Node::Prov { inner, .. } | Node::Track { inner, .. } => {
            render_one(inner, list_level, inline, parts)
        }
        // A lone list item outside a run (defensive; `render_nodes` folds
        // every run it sees).
        Node::ListItem { .. } => {
            let text = render_list(std::slice::from_ref(node), list_level - 1);
            push(parts, text);
        }
        // Not in the JSON body, so not in upstream's output.
        Node::DoclangOnly(_)
        | Node::Furniture { .. }
        | Node::PageFurniture { .. }
        | Node::FurnitureText { .. }
        | Node::PictureChildren(_)
        | Node::PageBreak
        | Node::PageInfo { .. }
        | Node::Anchor { .. } => {}
    }
}

fn formula(latex: &str, inline: bool) -> String {
    if inline {
        format!("${latex}$")
    } else {
        format!("$${latex}$$")
    }
}

/// `field_region` / `field_item` items are neither text nor groups to the
/// upstream serializer: each falls back to `% missing-text`, and the item's
/// marker / key / value children follow as plain text items (the outer walk
/// still descends into them).
fn render_field_region(items: &[FieldItem], parts: &mut Vec<String>) {
    parts.push(MISSING_TEXT.to_string());
    for item in items {
        parts.push(MISSING_TEXT.to_string());
        for text in [&item.marker, &item.key, &item.value].into_iter().flatten() {
            let t = text_item(text);
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
}

/// A picture's `figure`: the image placeholder, the caption, then its meta
/// comments.
fn render_figure(caption: Option<&str>, comments: &[String]) -> String {
    let mut lines = vec![
        "\\begin{figure}[h]".to_string(),
        IMAGE_PLACEHOLDER.to_string(),
    ];
    if let Some(cap) = caption.filter(|c| !c.is_empty()) {
        lines.push(format!("\\caption{{{}}}", text_item(cap)));
    }
    lines.extend(comments.iter().cloned());
    lines.push("\\end{figure}".to_string());
    lines.join("\n")
}

/// docling-core's `_to_latex_comment`: `% annotation[kind]: first line`, each
/// further line as `% line`; nothing for blank text.
fn annotation_comment(kind: &str, text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let mut out = format!("% annotation[{kind}]: {first}");
    for line in lines {
        out.push_str("\n% ");
        out.push_str(line);
    }
    Some(out)
}

/// The `table` environment over a `|l|…|l|` tabular of the JSON grid: with
/// first-class cells a spanning cell's text repeats over every position it
/// covers (holes are empty); otherwise the padded row grid.
pub(crate) fn render_table(t: &Table) -> String {
    let num_rows = t.rows.len();
    let num_cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut grid: Vec<Vec<String>> = Vec::with_capacity(num_rows);
    // A rich cell renders its block content through the document serializer
    // (upstream's `RichTableCell` path), otherwise the flat text. Its blocks
    // live on its anchor; every position it covers repeats them, rendered
    // once per table (docling-core#857, #777).
    let anchors = t.span_anchors();
    let mut rich: std::collections::HashMap<(usize, usize), String> =
        std::collections::HashMap::new();
    let mut cell = |r: usize, c: usize, flat: &str| -> String {
        let anchor = anchors
            .get(r)
            .and_then(|row| row.get(c))
            .copied()
            .unwrap_or((r, c));
        match t
            .cell_blocks
            .as_ref()
            .and_then(|b| b.get(anchor.0))
            .and_then(|row| row.get(anchor.1))
            .filter(|b| !b.is_empty())
        {
            Some(blocks) => rich
                .entry(anchor)
                .or_insert_with(|| {
                    render_nodes(blocks, 1, false)
                        .join("\n\n")
                        .replace('\n', " ")
                })
                .clone(),
            None => cell_text(flat),
        }
    };
    match t.cells.as_ref().filter(|c| !c.is_empty()) {
        Some(cells) => {
            let mut by_pos: std::collections::HashMap<(usize, usize), &str> =
                std::collections::HashMap::new();
            // A merged cell repeats in every position it covers, plain or
            // rich (a rich one used to be left empty beyond its first, #527).
            for c in cells {
                for r in c.start_row..(c.start_row + c.row_span).min(num_rows) {
                    for k in c.start_col..(c.start_col + c.col_span).min(num_cols) {
                        by_pos.insert((r, k), c.text.as_str());
                    }
                }
            }
            for r in 0..num_rows {
                grid.push(
                    (0..num_cols)
                        .map(|c| cell(r, c, by_pos.get(&(r, c)).copied().unwrap_or("")))
                        .collect(),
                );
            }
        }
        None => {
            for (r, row) in t.rows.iter().enumerate() {
                grid.push(
                    (0..num_cols)
                        .map(|c| cell(r, c, row.get(c).map(String::as_str).unwrap_or("")))
                        .collect(),
                );
            }
        }
    }
    let mut tabular = String::new();
    if !grid.is_empty() {
        let colspec = format!("|{}|", vec!["l"; num_cols].join("|"));
        let mut lines = vec![
            format!("\\begin{{tabular}}{{{colspec}}}"),
            "\\hline".to_string(),
        ];
        for row in &grid {
            lines.push(format!("{} \\\\ \\hline", row.join(" & ")));
        }
        lines.push("\\end{tabular}".to_string());
        tabular = lines.join("\n");
    }
    let caption = t
        .caption
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(text_item)
        .unwrap_or_default();
    if tabular.is_empty() && caption.is_empty() {
        return String::new();
    }
    let mut content = vec!["\\begin{table}[h]".to_string()];
    if !caption.is_empty() {
        content.push(format!("\\caption{{{caption}}}"));
    }
    if !tabular.is_empty() {
        content.push(tabular);
    }
    content.push("\\end{table}".to_string());
    content.join("\n")
}

fn cell_text(s: &str) -> String {
    // A rich cell's flat text is its Markdown serialization; drop the GFM
    // hard-line-break marker (docling-core#721) before flattening.
    inline_cell(&crate::markdown::strip_hard_breaks(s)).replace('\n', " ")
}

// ---------------------------------------------------------------------------
// Lists — the grouping mirrors `json.rs` (`add_sibling_lists` / `add_list`).
// ---------------------------------------------------------------------------

fn level_of(node: &Node) -> u8 {
    match node {
        Node::ListItem { level, .. } => *level,
        _ => 0,
    }
}

/// Split a run of list items into sibling list groups at the base level: a
/// new group starts where the backend flagged one (`first_in_list`), the same
/// boundary the Markdown and JSON serializers use (#385).
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

/// One list group: `\begin{env}` … `\end{env}`, both indented by `depth`
/// steps; items are never indented. An item's deeper successors nest under
/// it as further groups (each their own environment, one step deeper).
fn render_list(items: &[Node], depth: usize) -> String {
    let base = level_of(&items[0]);
    // The list kind and item text follow the DocLang override when present:
    // a docx multilevel item is `- 1.1. Sub one` in Markdown but an ordered
    // item with text `Sub one` in docling's model (and so upstream's LaTeX).
    let enumerated = items.iter().find_map(|n| match n {
        Node::ListItem {
            level,
            ordered,
            dclx,
            layer: None,
            ..
        } if *level == base => Some(dclx.as_ref().map_or(*ordered, |d| d.ordered)),
        _ => None,
    });
    let env = if enumerated.unwrap_or(false) {
        "enumerate"
    } else {
        "itemize"
    };
    let mut lines: Vec<String> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let Node::ListItem {
            text, dclx, layer, ..
        } = &items[i]
        else {
            i += 1;
            continue;
        };
        if level_of(&items[i]) > base {
            i += 1;
            continue;
        }
        // Furniture / notes items are outside the body layer upstream exports.
        if layer.is_some() {
            i += 1;
            continue;
        }
        let text = dclx
            .as_ref()
            .filter(|d| !d.text.is_empty())
            .map_or(text.as_str(), |d| d.text.as_str());
        lines.push(format!("\\item {}", inline_md(text)));
        let mut j = i + 1;
        while j < items.len() && level_of(&items[j]) > base {
            j += 1;
        }
        if j > i + 1 {
            let nested: Vec<&Node> = items[i + 1..j]
                .iter()
                .filter(|n| matches!(n, Node::ListItem { .. }))
                .collect();
            if !nested.is_empty() {
                let owned: Vec<Node> = nested.into_iter().cloned().collect();
                for group in sibling_lists(&owned) {
                    let text = render_list(group, depth + 1);
                    if !text.is_empty() {
                        lines.push(text);
                    }
                }
            }
        }
        i = j;
    }
    if lines.is_empty() {
        return String::new();
    }
    let indent = " ".repeat(depth * INDENT);
    format!(
        "{indent}\\begin{{{env}}}\n{}\n{indent}\\end{{{env}}}",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_markers_become_latex_commands() {
        assert_eq!(
            inline_md("Foo *emphasis* **strong emphasis** ***both*** ~~gone~~ ."),
            "Foo \\textit{emphasis} \\textbf{strong emphasis} \\textit{\\textbf{both}} \\sout{gone} ."
        );
        // Escaping applies to the plain runs and inside the commands.
        assert_eq!(inline_md("**50% & more**"), "\\textbf{50\\% \\& more}");
        // A lone or blank-only marker is plain text.
        assert_eq!(inline_md("Brutto in *"), "Brutto in *");
        assert_eq!(inline_md("x * * y"), "x * * y");
    }

    #[test]
    fn inline_code_and_formulas() {
        // Only `#` is escaped in code, with upstream's doubled backslash.
        assert_eq!(inline_md("Run `a_b #1`"), "Run \\texttt{a_b \\\\#1}");
        // Formatting around a code span is dropped (a `CodeItem` upstream).
        assert_eq!(
            inline_md("Some *`formatted\\_code`*"),
            "Some \\texttt{formatted_code}"
        );
        assert_eq!(
            inline_md("area $A= \\pi r^{2}$ ."),
            "area $A= \\pi r^{2}$ ."
        );
        // Currency is not a formula; a literal `$$` is text.
        assert_eq!(
            inline_md("reached $9 million ($158 million today)"),
            "reached \\$9 million (\\$158 million today)"
        );
        assert_eq!(
            inline_md("[$$E=mc^2$$](https://x.org/a)"),
            "\\href{https://x.org/a}{\\$\\$E=mc\\textasciicircum{}2\\$\\$}"
        );
        // A flattened fenced block is `verbatim`.
        assert_eq!(
            inline_cell("``` do_thing() ```"),
            "\\begin{verbatim}\ndo_thing()\n\\end{verbatim}"
        );
        // A plain cell keeps `$…$` literal.
        assert_eq!(inline_cell("$A=1$"), "\\$A=1\\$");
    }

    #[test]
    fn located_pdf_footnote_link_renders_href() {
        // The PDF pipeline bakes a covered footnote's link as `[text](url)`
        // inside a located paragraph.
        let mut doc = DoclingDocument::new("t");
        doc.nodes.push(Node::Located {
            location: [0, 0, 10, 10],
            inner: Box::new(Node::Paragraph {
                text: "[www.magenta.at/faq](https://www.magenta.at/faq)".into(),
            }),
        });
        assert!(
            to_latex(&doc).contains("\\href{https://www.magenta.at/faq}{www.magenta.at/faq}"),
            "{}",
            to_latex(&doc)
        );
    }

    #[test]
    fn hyperlinks() {
        assert_eq!(
            inline_md("Pull the [**repository**](https://github.com/x/y_z) ."),
            "Pull the \\href{https://github.com/x/y\\_z}{\\textbf{repository}} ."
        );
        assert_eq!(inline_md("[top](#)"), "\\href{\\#}{top}");
        // pydantic's AnyUrl gives a bare host its `/`; relative paths stay.
        assert_eq!(
            inline_md("[E](https://example.com)"),
            "\\href{https://example.com/}{E}"
        );
        assert_eq!(
            inline_md("[E](https://example.com#s)"),
            "\\href{https://example.com/\\#s}{E}"
        );
        assert_eq!(inline_md("[H](/home.html)"), "\\href{/home.html}{H}");
        // Balanced parentheses in the URL, brackets in the label.
        assert_eq!(
            inline_md("[[ 1 ]](https://w.org/Duck_(film))"),
            "\\href{https://w.org/Duck\\_(film)}{[ 1 ]}"
        );
        // An image placeholder is not a link; a code label drops the link.
        assert_eq!(inline_md("![alt](img.png)"), "![alt](img.png)");
        assert_eq!(inline_md("[`code`](u)"), "\\texttt{code}");
    }

    #[test]
    fn escapes_every_special_character_once() {
        assert_eq!(
            escape_latex(r"a\b{c}#$%&_~^"),
            r"a\textbackslash{}b\{c\}\#\$\%\&\_\textasciitilde{}\textasciicircum{}"
        );
    }

    #[test]
    fn title_is_hoisted_and_maketitle_added() {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::Heading {
            level: 1,
            text: "Hello 100%".into(),
        });
        doc.push(Node::Paragraph {
            text: "Body.".into(),
        });
        let tex = to_latex(&doc);
        assert!(tex.contains("\\title{Hello 100\\%}\n\n\\begin{document}\n\n\\maketitle\n\nBody."));
        assert!(!tex.ends_with('\n'));
    }

    /// docling-core#983's `test_latex_title_with_braces`: a title with
    /// formatting or escaped braces is hoisted into the preamble, once.
    #[test]
    fn title_with_braces_is_hoisted() {
        for (text, expected) in [
            ("Plain", r"\title{Plain}"),
            ("**Bold**", r"\title{\textbf{Bold}}"),
            ("**Bold** title", r"\title{\textbf{Bold} title}"),
            ("Set {a}", r"\title{Set \{a\}}"),
            ("Unmatched {", r"\title{Unmatched \{}"),
        ] {
            let mut doc = DoclingDocument::new("t");
            doc.push(Node::Heading {
                level: 1,
                text: text.into(),
            });
            doc.push(Node::Paragraph {
                text: "Body remains.".into(),
            });
            let tex = to_latex(&doc);
            let at = tex
                .find(expected)
                .unwrap_or_else(|| panic!("{text}: {tex}"));
            assert!(at < tex.find(r"\begin{document}").unwrap(), "{tex}");
            assert_eq!(tex.matches(r"\maketitle").count(), 1, "{tex}");
            assert!(tex.find(r"\maketitle").unwrap() < tex.find("Body remains.").unwrap());
        }
    }

    /// `test_latex_keeps_first_title_when_later_title_is_formatted`.
    #[test]
    fn first_title_wins_when_a_later_one_is_formatted() {
        let mut doc = DoclingDocument::new("t");
        for (title, body) in [
            ("**First**", "Between titles."),
            ("*Second*", "After titles."),
        ] {
            doc.push(Node::Heading {
                level: 1,
                text: title.into(),
            });
            doc.push(Node::Paragraph { text: body.into() });
        }
        let tex = to_latex(&doc);
        assert_eq!(tex.matches(r"\title{").count(), 1, "{tex}");
        assert!(tex.contains(r"\title{\textbf{First}}"), "{tex}");
        assert!(!tex.contains("Second"), "{tex}");
        assert_eq!(tex.matches(r"\maketitle").count(), 1);
        assert!(tex.contains("Between titles.") && tex.contains("After titles."));
    }

    #[test]
    fn nested_lists_indent_their_environments_only() {
        let item = |level: u8, text: &str, first: bool| Node::ListItem {
            ordered: false,
            number: 1,
            first_in_list: first,
            text: text.into(),
            level,
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        };
        let mut doc = DoclingDocument::new("t");
        doc.push(item(0, "a", true));
        doc.push(item(1, "a1", true));
        doc.push(item(0, "b", false));
        let tex = to_latex(&doc);
        assert!(
            tex.contains(
                "\\begin{itemize}\n\\item a\n  \\begin{itemize}\n\\item a1\n  \\end{itemize}\n\\item b\n\\end{itemize}"
            ),
            "{tex}"
        );
    }

    #[test]
    fn picture_meta_becomes_figure_comments() {
        // docling-core#871: classification, then description, after the
        // caption; continuation lines get a bare `% `.
        let picture = |description| Node::Picture {
            caption: Some("Fig".into()),
            caption_href: None,
            image: None,
            classification: Some(vec![crate::document::PictureClass {
                class_name: "bar_chart".into(),
                confidence: 0.9,
            }]),
            description,
            caption_parent: Default::default(),
            caption_location: None,
        };
        let mut doc = DoclingDocument::new("t");
        doc.push(picture(Some(crate::document::PictureDescription {
            text: "two\nlines".into(),
            provenance: "picture_ocr".into(),
        })));
        assert!(to_latex(&doc).contains(
            "\\caption{Fig}\n% annotation[classification]: bar chart\n\
             % annotation[description]: two\n% lines\n\\end{figure}"
        ));
        // Alt text is a docling.rs addition, off like in Markdown and HTML.
        let mut doc = DoclingDocument::new("t");
        doc.push(picture(Some(
            crate::document::PictureDescription::alt_text("alt"),
        )));
        assert!(!to_latex(&doc).contains("annotation[description]"));
    }

    #[test]
    fn empty_document_is_just_the_skeleton() {
        let tex = to_latex(&DoclingDocument::new("t"));
        assert!(tex.ends_with("\\begin{document}\n\n\\end{document}"));
    }
}
