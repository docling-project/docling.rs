//! Office Math Markup Language (OMML) → LaTeX, a port of docling's
//! `docx/latex/omml.py` (itself adapted from dwml). Each `<m:oMath>` element is
//! converted to a LaTeX string; the DOCX backend then wraps it in `$…$` (inline)
//! or `$$…$$` (a standalone formula).
//!
//! `process_unicode` upstream defers arbitrary characters to `pylatexenc`; here
//! the (small, fixed) set of symbols the corpus uses is hard-coded with the exact
//! strings pylatexenc produces.

use roxmltree::Node as XmlNode;

/// Characters LaTeX-escaped by `escape_latex`.
const ESCAPE_CHARS: &[char] = &['{', '}', '_', '^', '#', '&', '$', '%', '~'];

/// Convert an `<m:oMath>` element to a LaTeX string. `oMath2Latex.__str__`
/// applies a **single** pass of `"  " -> " "` (not a full whitespace collapse):
/// symbols carry two-space padding, so a lone symbol collapses to one space
/// while a symbol adjacent to another (giving 3–4 spaces) keeps a double space.
/// docling then `.strip()`s the result.
pub fn to_latex(omath: XmlNode) -> String {
    process_children(omath)
        .replace("  ", " ")
        .trim()
        .to_string()
}

/// Characters that mark a limit/label as mathematical (so its spaces are *not*
/// escaped) — docling's `MATH_CHARS`.
const MATH_CHARS: &[char] = &['\\', '<', '>', '=', '+', '*', '/', '^', '_', '{', '}'];

fn local<'i>(n: XmlNode<'_, 'i>) -> &'i str {
    n.tag_name().name()
}

fn elems<'a, 'i>(n: XmlNode<'a, 'i>) -> impl Iterator<Item = XmlNode<'a, 'i>> {
    n.children().filter(XmlNode::is_element)
}

/// Concatenate the dispatched LaTeX of every OMML child element.
fn process_children(n: XmlNode) -> String {
    elems(n).map(dispatch).collect()
}

fn dispatch(e: XmlNode) -> String {
    match local(e) {
        "acc" => do_acc(e),
        "r" => do_r(e),
        "bar" => do_bar(e),
        "sSub" => do_ssub(e),
        "sSup" => do_ssup(e),
        "sSubSup" => do_ssubsup(e),
        "sub" => format!("_{{{}}}", process_children(e)),
        "sup" => format!("^{{{}}}", process_children(e)),
        "f" => do_f(e),
        "func" => do_func(e),
        "fName" => do_fname(e),
        "groupChr" => do_groupchr(e),
        "d" => do_d(e),
        "rad" => do_rad(e),
        "eqArr" => do_eqarr(e),
        "limLow" => do_limlow(e),
        "limUpp" => do_limupp(e),
        "lim" => do_lim(e),
        "m" => do_m(e),
        "mr" => do_mr(e),
        "nary" => do_nary(e),
        // "direct" tags whose children are inlined.
        "box" | "num" | "den" | "deg" | "e" => process_children(e),
        _ => String::new(),
    }
}

/// A child element by OMML local name, processed to a string.
fn child(n: XmlNode, tag: &str) -> Option<String> {
    elems(n).find(|c| local(*c) == tag).map(dispatch)
}

/// A `*Pr` properties element: its `m:val`-bearing flags plus inlined text.
#[derive(Default)]
struct Pr {
    chr: Option<String>,
    pos: Option<String>,
    beg: Option<String>,
    end: Option<String>,
    typ: Option<String>,
    text: String,
}

fn pr(n: XmlNode, tag: &str) -> Pr {
    let Some(node) = elems(n).find(|c| local(*c) == tag) else {
        return Pr::default();
    };
    let mut p = Pr::default();
    for c in elems(node) {
        let val = c
            .attributes()
            .find(|a| a.name() == "val")
            .map(|a| a.value().to_string());
        match local(c) {
            "chr" => p.chr = val,
            "pos" => p.pos = val,
            "begChr" => p.beg = val,
            "endChr" => p.end = val,
            "type" => p.typ = val,
            "brk" => p.text.push_str("\\\\"),
            _ => p.text.push_str(&dispatch(c)),
        }
    }
    p
}

fn do_f(e: XmlNode) -> String {
    let p = pr(e, "fPr");
    let num = child(e, "num").unwrap_or_default();
    let den = child(e, "den").unwrap_or_default();
    let body = match p.typ.as_deref() {
        Some("skw") => format!("^{{{num}}}/_{{{den}}}"),
        Some("noBar") => format!("\\genfrac{{}}{{}}{{0pt}}{{}}{{{num}}}{{{den}}}"),
        Some("lin") => format!("{{{num}}}/{{{den}}}"),
        _ => format!("\\frac{{{num}}}{{{den}}}"),
    };
    p.text + &body
}

fn do_d(e: XmlNode) -> String {
    let p = pr(e, "dPr");
    let inner = child(e, "e").unwrap_or_default();
    let left = greek_or(p.beg.as_deref(), "(");
    let right = greek_or(p.end.as_deref(), ")");
    let left = if left.is_empty() {
        ".".into()
    } else {
        escape_latex(&left)
    };
    let right = if right.is_empty() {
        ".".into()
    } else {
        escape_latex(&right)
    };
    format!("{}\\left{left}{inner}\\right{right}", p.text)
}

fn do_rad(e: XmlNode) -> String {
    let text = child(e, "e").unwrap_or_default();
    match child(e, "deg").filter(|d| !d.is_empty()) {
        Some(deg) => format!("\\sqrt[{deg}]{{{text}}}"),
        None => format!("\\sqrt{{{text}}}"),
    }
}

fn do_func(e: XmlNode) -> String {
    let fname = child(e, "fName").unwrap_or_default();
    let inner = child(e, "e").unwrap_or_default();
    fname.replace("{fe}", &inner)
}

fn do_fname(e: XmlNode) -> String {
    let mut out = String::new();
    for c in elems(e) {
        if local(c) == "r" {
            let t = do_r(c);
            out.push_str(func_latex(&t).unwrap_or(&t));
        } else {
            out.push_str(&dispatch(c));
        }
    }
    if out.contains("{fe}") {
        out
    } else {
        out + "{fe}"
    }
}

fn do_acc(e: XmlNode) -> String {
    let p = pr(e, "accPr");
    let inner = child(e, "e").unwrap_or_default();
    let tmpl = accent_latex(p.chr.as_deref()).unwrap_or("\\hat{%s}");
    apply_pct(tmpl, &inner)
}

fn do_bar(e: XmlNode) -> String {
    let p = pr(e, "barPr");
    let inner = child(e, "e").unwrap_or_default();
    let tmpl = match p.pos.as_deref() {
        Some("top") => "\\overline{%s}",
        Some("bot") => "\\underline{%s}",
        _ => "\\overline{%s}",
    };
    p.text.clone() + &apply_pct(tmpl, &inner)
}

fn do_groupchr(e: XmlNode) -> String {
    let p = pr(e, "groupChrPr");
    let inner = child(e, "e").unwrap_or_default();
    let tmpl = accent_latex(p.chr.as_deref()).unwrap_or("\\underbrace{%s}");
    p.text.clone() + &apply_pct(tmpl, &inner)
}

fn do_nary(e: XmlNode) -> String {
    let mut op = String::new();
    let mut rest = String::new();
    for c in elems(e) {
        if local(c) == "naryPr" {
            let p = pr(e, "naryPr");
            op = big_op(p.chr.as_deref()).unwrap_or("\\int").to_string();
        } else {
            rest.push_str(&dispatch(c));
        }
    }
    op + &rest
}

fn do_m(e: XmlNode) -> String {
    let rows: Vec<String> = elems(e).filter(|c| local(*c) == "mr").map(do_mr).collect();
    format!("\\begin{{matrix}}{}\\end{{matrix}}", rows.join("\\\\"))
}

fn do_mr(e: XmlNode) -> String {
    elems(e)
        .filter(|c| local(*c) == "e")
        .map(process_children)
        .collect::<Vec<_>>()
        .join("&")
}

fn do_eqarr(e: XmlNode) -> String {
    elems(e)
        .filter(|c| local(*c) == "e")
        .map(process_children)
        .collect::<Vec<_>>()
        .join("\\\\")
}

/// Grouping commands (underbrace/overbrace/…) that accept a subscript label.
const GROUPING_FUNCS: &[&str] = &[
    "\\underbrace",
    "\\overbrace",
    "\\underparen",
    "\\overparen",
    "\\underbracket",
    "\\overbracket",
];

fn do_limlow(e: XmlNode) -> String {
    let base = child(e, "e").unwrap_or_default();
    let lim = child(e, "lim").unwrap_or_default();
    // Known limit functions map to a dedicated template.
    if let Some(tmpl) = lim_func(&base) {
        return tmpl.replace("%(lim)s", &lim);
    }
    // Grouping commands (already-formatted LaTeX) just take a subscript.
    if GROUPING_FUNCS
        .iter()
        .any(|f| base.starts_with(&format!("{f}{{")))
    {
        return format!("{base}_{{{lim}}}");
    }
    format!("{base}_{{{lim}}}")
}

/// docling's `LIM_FUNC` table.
fn lim_func(base: &str) -> Option<&'static str> {
    Some(match base {
        "lim" => "\\lim_{%(lim)s}",
        "max" => "\\max_{%(lim)s}",
        "min" => "\\min_{%(lim)s}",
        "argmax" => "\\operatorname{argmax}_{%(lim)s}",
        "argmin" => "\\operatorname{argmin}_{%(lim)s}",
        _ => return None,
    })
}

fn do_limupp(e: XmlNode) -> String {
    let base = child(e, "e").unwrap_or_default();
    let lim = child(e, "lim").unwrap_or_default();
    format!("\\overset{{{lim}}}{{{base}}}")
}

/// A limit/label content (`<m:lim>`): `\rightarrow` → `\to`, trailing line-break
/// and whitespace stripped, and — for a plain-text label with no math chars —
/// spaces escaped as `\ ` (docling's `do_lim`).
fn do_lim(e: XmlNode) -> String {
    let mut result = process_children(e).replace("\\rightarrow", "\\to");
    result = result.trim_end().to_string();
    if let Some(stripped) = result.strip_suffix("\\\\") {
        result = stripped.trim_end().to_string();
    }
    if !result.is_empty() && !result.chars().any(|c| MATH_CHARS.contains(&c)) {
        result = result.replace(' ', "\\ ");
    }
    result
}

fn do_ssub(e: XmlNode) -> String {
    let base = child(e, "e").unwrap_or_default();
    let base = base.trim_end();
    let sub = unwrap_script(&child(e, "sub").unwrap_or_default(), '_');
    let base = group_if_needed(base);
    format!("{base}_{{{sub}}}")
}

fn do_ssup(e: XmlNode) -> String {
    let base = child(e, "e").unwrap_or_default();
    let base = base.trim_end();
    let sup = unwrap_script(&child(e, "sup").unwrap_or_default(), '^');
    let base = group_if_needed(base);
    format!("{base}^{{{sup}}}")
}

fn do_ssubsup(e: XmlNode) -> String {
    let base = child(e, "e").unwrap_or_default();
    let base = base.trim_end();
    let sub = unwrap_script(&child(e, "sub").unwrap_or_default(), '_');
    let sup = unwrap_script(&child(e, "sup").unwrap_or_default(), '^');
    let base = group_if_needed(base);
    format!("{base}_{{{sub}}}^{{{sup}}}")
}

fn group_if_needed(base: &str) -> String {
    if base.contains("\\frac") || base.contains("\\sqrt") {
        format!("{{{base}}}")
    } else {
        base.to_string()
    }
}

fn unwrap_script(script: &str, marker: char) -> String {
    let prefix = format!("{marker}{{");
    if let Some(inner) = script
        .strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix('}'))
    {
        inner.to_string()
    } else {
        script.to_string()
    }
}

fn apply_pct(tmpl: &str, arg: &str) -> String {
    if tmpl.contains("%s") {
        tmpl.replace("%s", arg)
    } else {
        tmpl.to_string()
    }
}

/// Process a `<m:r>` run: each character mapped via `process_unicode`, then the
/// whole string LaTeX-escaped, with the brace/caret un-escaping dance from the
/// upstream `do_r`.
fn do_r(e: XmlNode) -> String {
    let found: String = e
        .descendants()
        .filter(|n| local(*n) == "t")
        .filter_map(|t| t.text())
        .collect();
    let mapped: String = found.chars().map(process_unicode).collect();
    let mut proc = escape_latex(&mapped);

    if !found.contains('{') && proc.contains("\\{") {
        proc = proc.replace("\\{", "{");
    }
    if !found.contains('}') && proc.contains("\\}") {
        proc = proc.replace("\\}", "}");
    }
    // A caret in the source is a math superscript operator, not a literal.
    if found.contains('^') && proc.contains("\\^") {
        proc = proc.replace("\\^", "^");
    }
    proc
}

/// Per-character Unicode → LaTeX for the symbols the corpus uses — the exact
/// strings docling's `process_unicode` returns (`_MATH_CHAR_MAP` first, else
/// pylatexenc's `\ensuremath{…}`/`{…}` unwrapped to a **two-space**-padded macro;
/// the two-space padding is load-bearing, see [`to_latex`]).
fn process_unicode(c: char) -> String {
    match c {
        // _MATH_CHAR_MAP (returned verbatim, no padding).
        '\u{2013}' | '\u{2014}' | '\u{2212}' => "-".to_string(),
        '\u{005e}' => "^".to_string(),
        '\u{00d7}' => "\\times ".to_string(),
        // pylatexenc `\ensuremath{…}` → two-space-padded macro.
        '\u{00b1}' => "  \\pm  ".to_string(),
        '\u{03c0}' => "  \\pi  ".to_string(),
        '\u{03c4}' => "  \\tau  ".to_string(),
        '\u{03f5}' => "  \\epsilon  ".to_string(),
        '\u{221e}' => "  \\infty  ".to_string(),
        '\u{2229}' => "  \\cap  ".to_string(),
        '\u{2264}' => "  \\leq  ".to_string(),
        '\u{22c5}' => "  \\cdot  ".to_string(),
        '\u{003c}' => "  <  ".to_string(),
        // `{\textellipsis}` → one-space-padded `\text{ … }`.
        '\u{2026}' => " \\text{ \\textellipsis } ".to_string(),
        other => other.to_string(),
    }
}

fn escape_latex(s: &str) -> String {
    let s = s.replace("\\\\", "\\");
    let mut out = String::with_capacity(s.len());
    let mut last = '\0';
    for c in s.chars() {
        if ESCAPE_CHARS.contains(&c) && last != '\\' {
            out.push('\\');
        }
        out.push(c);
        last = c;
    }
    out
}

/// Function-name (`<m:r>` inside `<m:fName>`) → LaTeX template with `{fe}` slot.
fn func_latex(name: &str) -> Option<&'static str> {
    Some(match name {
        "sin" => "\\sin({fe})",
        "cos" => "\\cos({fe})",
        "tan" => "\\tan({fe})",
        "sinh" => "\\sinh({fe})",
        "cosh" => "\\cosh({fe})",
        "tanh" => "\\tanh({fe})",
        "sec" => "\\sec({fe})",
        "csc" => "\\csc({fe})",
        "log" => "\\log({fe})",
        "ln" => "\\ln({fe})",
        "exp" => "\\exp({fe})",
        "max" => "\\max({fe})",
        "min" => "\\min({fe})",
        "det" => "\\det({fe})",
        "lim" => "\\lim({fe})",
        _ => return None,
    })
}

fn accent_latex(chr: Option<&str>) -> Option<&'static str> {
    match chr? {
        "\u{0300}" => Some("\\grave{%s}"),
        "\u{0301}" => Some("\\acute{%s}"),
        "\u{0302}" => Some("\\hat{%s}"),
        "\u{0303}" => Some("\\tilde{%s}"),
        "\u{0304}" => Some("\\bar{%s}"),
        "\u{20d7}" => Some("\\vec{%s}"),
        "\u{23de}" => Some("\\overbrace{%s}"),
        "\u{23df}" => Some("\\underbrace{%s}"),
        _ => None,
    }
}

fn big_op(chr: Option<&str>) -> Option<&'static str> {
    match chr {
        None => None,
        Some(c) => Some(match c {
            "\u{220f}" => "\\prod",
            "\u{2210}" => "\\coprod",
            "\u{2211}" => "\\sum",
            "\u{222b}" => "\\int",
            "\u{222c}" => "\\iint",
            "\u{222d}" => "\\iiint",
            "\u{222e}" => "\\oint",
            "\u{222f}" => "\\oiint",
            "\u{2230}" => "\\oiiint",
            "\u{22c0}" => "\\bigwedge",
            "\u{22c1}" => "\\bigvee",
            "\u{22c2}" => "\\bigcap",
            "\u{22c3}" => "\\bigcup",
            _ => return None,
        }),
    }
}

/// Delimiter char via the Greek math-italic table, else the char itself.
fn greek_or(key: Option<&str>, default: &str) -> String {
    match key {
        None => default.to_string(),
        Some(k) => k.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roxmltree::Document;

    fn latex(xml: &str) -> String {
        let doc = Document::parse(xml).unwrap();
        let omath = doc.descendants().find(|n| n.has_tag_name("oMath")).unwrap();
        to_latex(omath)
    }

    #[test]
    fn argmax_limit_with_escaped_label() {
        // A `limLow` whose base is `argmax` maps to `\operatorname{argmax}`, and a
        // plain-text limit label (no math chars) has its spaces escaped as `\ `.
        let xml = r#"<r xmlns:m="m"><m:oMath><m:limLow>
            <m:e><m:r><m:t>argmax</m:t></m:r></m:e>
            <m:lim><m:r><m:t>x y</m:t></m:r></m:lim>
          </m:limLow></m:oMath></r>"#;
        assert_eq!(latex(xml), r"\operatorname{argmax}_{x\ y}");
    }

    #[test]
    fn symbol_padding_survives_single_collapse() {
        // `∞` and `<` each carry two-space padding; a single `"  " -> " "` pass
        // leaves a double space where two padded symbols meet.
        let xml = r#"<r xmlns:m="m"><m:oMath><m:r><m:t>&#8734;&#60;x</m:t></m:r></m:oMath></r>"#;
        assert_eq!(latex(xml), r"\infty  < x");
    }
}
