//! LaTeX backend (core) — a lightweight scanner for simple `.tex` documents.
//!
//! docling's backend drives the full `pylatexenc` parser plus macro/environment/
//! math handlers (and optionally a Tectonic engine); this core handles the
//! common structural subset: `\title`/`\author`/`\maketitle`, sectioning,
//! `itemize`/`enumerate`, `tabular`, display math, and paragraph text with the
//! usual font macros stripped. Multi-file projects (`\input`/`\include`),
//! custom macros and rich math/citations are out of scope.

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node, Table};

pub struct LatexBackend;

impl DeclarativeBackend for LatexBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let raw = source.text()?;
        let text = strip_comments(raw);
        let title = braced_arg(&text, "\\title");
        let author = braced_arg(&text, "\\author");
        let body = between(&text, "\\begin{document}", "\\end{document}").unwrap_or(&text);

        let mut doc = DoclingDocument::new(&source.name);
        let chars: Vec<char> = body.chars().collect();
        let mut p = Parser {
            chars: &chars,
            i: 0,
            title,
            author,
        };
        p.run(&mut doc);
        Ok(doc)
    }
}

struct Parser<'a> {
    chars: &'a [char],
    i: usize,
    title: Option<String>,
    author: Option<String>,
}

const HEADINGS: &[(&str, u8)] = &[
    ("\\subsubsection", 4),
    ("\\subsection", 3),
    ("\\section", 2),
    ("\\paragraph", 5),
    ("\\chapter", 1),
];

impl Parser<'_> {
    fn run(&mut self, doc: &mut DoclingDocument) {
        let mut para = String::new();
        while self.i < self.chars.len() {
            let rest: String = self.chars[self.i..].iter().collect();
            if rest.starts_with("\\maketitle") {
                self.i += "\\maketitle".len();
                if let Some(t) = self.title.take() {
                    doc.push(Node::Heading { level: 1, text: t });
                }
                if let Some(a) = self.author.take() {
                    doc.push(Node::Paragraph { text: a });
                }
            } else if let Some((cmd, level)) = HEADINGS.iter().find(|(c, _)| {
                rest.starts_with(*c) && !rest[c.len()..].starts_with(|ch: char| ch.is_alphabetic())
            }) {
                flush(&mut para, doc);
                self.i += cmd.len();
                self.skip_star();
                let text = self.read_group();
                doc.push(Node::Heading {
                    level: *level,
                    text: clean_inline(&text),
                });
            } else if rest.starts_with("\\begin{") {
                flush(&mut para, doc);
                self.read_environment(doc);
            } else if rest.starts_with("\\[") || rest.starts_with("$$") {
                flush(&mut para, doc);
                let close = if rest.starts_with("\\[") { "\\]" } else { "$$" };
                self.i += 2;
                let math = self.read_until(close);
                doc.push(Node::Paragraph {
                    text: format!(
                        "$${}$$",
                        math.split_whitespace().collect::<Vec<_>>().join(" ")
                    ),
                });
            } else if self.chars[self.i] == '$' {
                // Inline math becomes its own block (docling extracts formulas).
                flush(&mut para, doc);
                self.i += 1;
                let math = self.read_until("$");
                doc.push(Node::Paragraph {
                    text: format!(
                        "${}$",
                        math.split_whitespace().collect::<Vec<_>>().join(" ")
                    ),
                });
            } else if self.chars[self.i] == '\n' && self.peek_blank_line() {
                flush(&mut para, doc);
                self.consume_blank_line();
            } else {
                para.push(self.chars[self.i]);
                self.i += 1;
            }
        }
        flush(&mut para, doc);
    }

    /// `\begin{env} … \end{env}` — handle the structural environments, ignore others.
    fn read_environment(&mut self, doc: &mut DoclingDocument) {
        self.i += "\\begin".len();
        let env = self.read_group();
        self.skip_optional();
        let inner = self.read_until(&format!("\\end{{{env}}}"));
        match env.as_str() {
            "itemize" | "enumerate" => emit_list(&inner, doc),
            "tabular" => emit_table(&inner, doc),
            "equation" | "displaymath" | "align" | "equation*" | "align*" | "gather"
            | "gather*" => {
                doc.push(Node::Paragraph {
                    text: format!(
                        "$${}$$",
                        clean_inline(&inner)
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                });
            }
            // Containers: render inner content (nested tabular, caption, …) transparently.
            "table" | "figure" | "document" | "abstract" | "center" => {
                let chars: Vec<char> = inner.chars().collect();
                let mut sub = Parser {
                    chars: &chars,
                    i: 0,
                    title: None,
                    author: None,
                };
                sub.run(doc);
            }
            _ => {}
        }
    }

    /// Skip an optional `[…]` argument (e.g. `\begin{table}[h]`).
    fn skip_optional(&mut self) {
        let mut j = self.i;
        while j < self.chars.len() && self.chars[j].is_whitespace() {
            j += 1;
        }
        if j < self.chars.len() && self.chars[j] == '[' {
            self.i = j + 1;
            while self.i < self.chars.len() && self.chars[self.i] != ']' {
                self.i += 1;
            }
            if self.i < self.chars.len() {
                self.i += 1;
            }
        }
    }

    fn skip_star(&mut self) {
        if self.i < self.chars.len() && self.chars[self.i] == '*' {
            self.i += 1;
        }
    }

    /// Read a `{…}` group (skipping leading whitespace), returning its raw content.
    fn read_group(&mut self) -> String {
        while self.i < self.chars.len() && self.chars[self.i].is_whitespace() {
            self.i += 1;
        }
        if self.i >= self.chars.len() || self.chars[self.i] != '{' {
            return String::new();
        }
        self.i += 1;
        let mut depth = 1;
        let mut out = String::new();
        while self.i < self.chars.len() && depth > 0 {
            match self.chars[self.i] {
                '{' => {
                    depth += 1;
                    out.push('{');
                }
                '}' => {
                    depth -= 1;
                    if depth > 0 {
                        out.push('}');
                    }
                }
                c => out.push(c),
            }
            self.i += 1;
        }
        out
    }

    fn read_until(&mut self, marker: &str) -> String {
        let mut out = String::new();
        let mlen = marker.chars().count();
        while self.i < self.chars.len() {
            if self.chars[self.i..].len() >= mlen
                && self.chars[self.i..self.i + mlen].iter().collect::<String>() == marker
            {
                self.i += mlen;
                return out;
            }
            out.push(self.chars[self.i]);
            self.i += 1;
        }
        out
    }

    fn peek_blank_line(&self) -> bool {
        let mut j = self.i + 1;
        while j < self.chars.len() && (self.chars[j] == ' ' || self.chars[j] == '\t') {
            j += 1;
        }
        j < self.chars.len() && self.chars[j] == '\n'
    }

    fn consume_blank_line(&mut self) {
        while self.i < self.chars.len() && self.chars[self.i] != '\n' {
            self.i += 1;
        }
        while self.i < self.chars.len()
            && (self.chars[self.i] == '\n'
                || self.chars[self.i] == ' '
                || self.chars[self.i] == '\t')
        {
            self.i += 1;
        }
    }
}

fn flush(para: &mut String, doc: &mut DoclingDocument) {
    let text = clean_inline(para);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !text.is_empty() {
        doc.push(Node::Paragraph { text });
    }
    para.clear();
}

/// Emit `\item` entries of an itemize/enumerate body as (unordered) list items.
fn emit_list(inner: &str, doc: &mut DoclingDocument) {
    let mut first = true;
    for part in inner.split("\\item").skip(1) {
        let text = clean_inline(part);
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            doc.push(Node::ListItem {
                ordered: false,
                number: 0,
                first_in_list: first,
                text,
                level: 0,
                marker: None,
                location: None,
                dclx: None,
                href: None,
                layer: None,
            });
            first = false;
        }
    }
}

/// Build a table from a `tabular` environment's rows (`\\` separated, `&` cells).
fn emit_table(inner: &str, doc: &mut DoclingDocument) {
    // Drop the column spec that follows \begin{tabular}.
    let body = match inner.find('}') {
        Some(p) if inner.trim_start().starts_with('{') => &inner[p + 1..],
        _ => inner,
    };
    let mut rows = Vec::new();
    for line in body.split("\\\\") {
        let line = line.replace("\\hline", "");
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            line.split('&')
                .map(|c| {
                    let t = clean_inline(c);
                    t.split_whitespace().collect::<Vec<_>>().join(" ")
                })
                .collect::<Vec<_>>(),
        );
    }
    if !rows.is_empty() {
        doc.push(Node::Table(Table {
            rows,
            location: None,
            structure: None,
            cell_blocks: None,
        }));
    }
}

/// Strip line comments (`%` to end of line, unless escaped `\%`).
fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let bytes: Vec<char> = line.chars().collect();
        let mut k = 0;
        while k < bytes.len() {
            if bytes[k] == '%' && (k == 0 || bytes[k - 1] != '\\') {
                out.push('\n');
                break;
            }
            out.push(bytes[k]);
            k += 1;
        }
    }
    out
}

/// The braced argument of `\cmd{…}` anywhere in the text.
fn braced_arg(text: &str, cmd: &str) -> Option<String> {
    let start = text.find(cmd)? + cmd.len();
    let rest: Vec<char> = text[start..].chars().collect();
    let mut k = 0;
    while k < rest.len() && rest[k].is_whitespace() {
        k += 1;
    }
    if k >= rest.len() || rest[k] != '{' {
        return None;
    }
    k += 1;
    let mut depth = 1;
    let mut out = String::new();
    while k < rest.len() && depth > 0 {
        match rest[k] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        out.push(rest[k]);
        k += 1;
    }
    Some(clean_inline(&out))
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let s = text.find(open)? + open.len();
    let e = text[s..].find(close)? + s;
    Some(&text[s..e])
}

/// Strip the common inline font/reference macros, keeping their content
/// (`\textbf{x}`→`x`, `\cite{k}`→`[k]`, `\ref{k}`→`[k]`), and unescape `\&`,`\%`,…
fn clean_inline(s: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let rest: String = chars[i..].iter().collect();
            // \cite{k} / \ref{k} / \eqref{k} → [k]
            if let Some(cmd) = ["\\cite", "\\ref", "\\eqref", "\\citep", "\\citet"]
                .iter()
                .find(|c| rest.starts_with(**c))
            {
                i += cmd.len();
                let (arg, ni) = read_group_at(&chars, i);
                i = ni;
                out.push('[');
                out.push_str(&arg);
                out.push(']');
                continue;
            }
            // font/structure macros: keep the group content
            if let Some(cmd) = [
                "\\textbf",
                "\\textit",
                "\\emph",
                "\\texttt",
                "\\textrm",
                "\\textsc",
                "\\underline",
                "\\mbox",
                "\\caption",
                "\\text",
            ]
            .iter()
            .find(|c| rest.starts_with(**c))
            {
                i += cmd.len();
                let (arg, ni) = read_group_at(&chars, i);
                i = ni;
                out.push_str(&clean_inline(&arg));
                continue;
            }
            // escaped specials
            if matches!(chars[i + 1], '&' | '%' | '#' | '_' | '$' | '{' | '}') {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
        }
        if chars[i] == '~' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Read a `{…}` group starting at `i` (after the macro name); returns (content, next-index).
fn read_group_at(chars: &[char], mut i: usize) -> (String, usize) {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() || chars[i] != '{' {
        return (String::new(), i);
    }
    i += 1;
    let mut depth = 1;
    let mut out = String::new();
    while i < chars.len() && depth > 0 {
        match chars[i] {
            '{' => {
                depth += 1;
                out.push('{');
            }
            '}' => {
                depth -= 1;
                if depth > 0 {
                    out.push('}');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    (out, i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn title_sections_lists_and_font_macros() {
        let tex = "\\title{T}\\author{A}\n\\begin{document}\n\\maketitle\n\
            \\section{Intro}\nHello \\textbf{world}.\n\
            \\begin{itemize}\n\\item one\n\\item two\n\\end{itemize}\n\\end{document}";
        let src = SourceDocument::from_bytes("d", InputFormat::Latex, tex.as_bytes().to_vec());
        let md = LatexBackend.convert(&src).unwrap().export_to_markdown();
        assert!(
            md.starts_with("# T\n\nA\n\n## Intro\n\nHello world.\n\n- one\n- two"),
            "got:\n{md}"
        );
    }
}
