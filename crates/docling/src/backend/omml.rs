//! Office Math Markup Language (OMML) → LaTeX, a port of docling's
//! `docx/latex/omml.py` (itself adapted from dwml). Each `<m:oMath>` element is
//! converted to a LaTeX string; the DOCX backend then wraps it in `$…$` (inline)
//! or `$$…$$` (a standalone formula).
//!
//! `process_unicode` upstream defers every character to `pylatexenc`; here its
//! results are a generated table ([`UNICODE_LATEX`], `scripts/dev/gen_omml_unicode.py`)
//! over the blocks formulas draw from.

use roxmltree::Node as XmlNode;

use super::omml_unicode::UNICODE_LATEX;

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

/// Concatenate the dispatched LaTeX of every OMML child element. Adjacent
/// runs of one style ([`RunStyle`]) share a wrapper — `\text{ for all }`, not
/// `\text{ for}\text{ all }` — so a word split over runs stays one word.
fn process_children(n: XmlNode) -> String {
    let mut out = String::new();
    let mut pending: Option<(RunStyle, String)> = None;
    for c in elems(n) {
        if local(c) == "r" {
            let style = RunStyle::of(c);
            let body = run_body(c, &style);
            match pending.as_mut() {
                Some((s, acc)) if *s == style => acc.push_str(&body),
                _ => {
                    if let Some((s, acc)) = pending.take() {
                        out.push_str(&s.wrap(acc));
                    }
                    pending = Some((style, body));
                }
            }
        } else {
            if let Some((s, acc)) = pending.take() {
                out.push_str(&s.wrap(acc));
            }
            out.push_str(&dispatch(c));
        }
    }
    if let Some((s, acc)) = pending {
        out.push_str(&s.wrap(acc));
    }
    out
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
    sep: Option<String>,
    typ: Option<String>,
    /// `m:subHide` / `m:supHide` (an n-ary's hidden limits).
    sub_hide: bool,
    sup_hide: bool,
    text: String,
}

/// An `ST_OnOff` flag element: on when present without `m:val`, or with a
/// true value (`1`, `on`, `true`).
fn on_off(val: Option<&str>) -> bool {
    !matches!(val, Some("0" | "off" | "false"))
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
            "sepChr" => p.sep = val,
            "type" => p.typ = val,
            "subHide" => p.sub_hide = on_off(val.as_deref()),
            "supHide" => p.sup_hide = on_off(val.as_deref()),
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

/// A delimiter: its elements (one `<m:e>` each) joined by `m:sepChr` — `|`
/// when absent (ECMA-376 Part 1, `sepChr`), nothing when empty — as docling's `do_d`
/// does: `(a|b)`, `{x | x > 0}`, `⟨u|v⟩`. The three characters go through
/// docling's `T` table ([`greek_or`]) before `escape_latex`.
fn do_d(e: XmlNode) -> String {
    let p = pr(e, "dPr");
    let sep = escape_latex(&delim_or(p.sep.as_deref(), "|", true));
    let inner = elems(e)
        .filter(|c| local(*c) == "e")
        .map(process_children)
        .collect::<Vec<_>>()
        .join(&sep);
    let left = delim_or(p.beg.as_deref(), "(", false);
    let right = delim_or(p.end.as_deref(), ")", false);
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
            // A function name's own text: Word marks it `m:sty="p"`, and the
            // name is looked up, not wrapped.
            let t = math_run(c);
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

/// An accent: docling's `CHR` table, the circumflex when `m:chr` is absent
/// (ECMA-376's default). docling returns an unknown character alone, dropping
/// the base; here it stays above the base instead (`\overset`).
fn do_acc(e: XmlNode) -> String {
    let p = pr(e, "accPr");
    let inner = child(e, "e").unwrap_or_default();
    match p.chr.as_deref() {
        None => apply_pct("\\hat{%s}", &inner),
        Some(c) => match accent_latex(c) {
            Some(tmpl) => apply_pct(tmpl, &inner),
            None => format!("\\overset{{{}}}{{{inner}}}", escape_latex(c)),
        },
    }
}

fn do_bar(e: XmlNode) -> String {
    let p = pr(e, "barPr");
    let inner = child(e, "e").unwrap_or_default();
    let tmpl = match p.pos.as_deref() {
        Some("top") => "\\overline{%s}",
        Some("bot") => "\\underline{%s}",
        _ => "\\overline{%s}",
    };
    p.text + &apply_pct(tmpl, &inner)
}

/// A group character: docling's `CHR` table, the underbrace when `m:chr` is
/// absent. docling returns an unknown character alone, dropping the base
/// (Word's "arrow over text", `→` with `pos="top"`); here it goes above the
/// base when `m:pos` is `top`, below it otherwise (the schema's default
/// `bot`), as an unknown accent does.
fn do_groupchr(e: XmlNode) -> String {
    let p = pr(e, "groupChrPr");
    let inner = child(e, "e").unwrap_or_default();
    let body = match p.chr.as_deref() {
        None => apply_pct("\\underbrace{%s}", &inner),
        Some(c) => match accent_latex(c) {
            Some(tmpl) => apply_pct(tmpl, &inner),
            None => {
                let place = if p.pos.as_deref() == Some("top") {
                    "over"
                } else {
                    "under"
                };
                format!("\\{place}set{{{}}}{{{inner}}}", escape_latex(c))
            }
        },
    };
    p.text + &body
}

/// An n-ary operator. Without `m:chr` — or without `m:naryPr` at all, which the
/// schema allows (docling#5470) — it is an integral; an operator outside the
/// table is kept as written (docling's `get_val`), never made an integral.
///
/// A limit Word hides (`m:subHide` / `m:supHide`: the indefinite integral, a
/// `\bigwedge` over an implied range) is left out — docling writes the empty
/// placeholder, `\int_{}^{}`.
fn do_nary(e: XmlNode) -> String {
    let p = pr(e, "naryPr");
    let op = match p.chr {
        None => "\\int".to_string(),
        Some(c) => big_op(&c).map_or(c, str::to_string),
    };
    let rest: String = elems(e)
        .filter(|c| match local(*c) {
            "naryPr" => false,
            "sub" => !p.sub_hide,
            "sup" => !p.sup_hide,
            _ => true,
        })
        .map(dispatch)
        .collect();
    // A macro right before its operand needs a separator once a hidden limit
    // no longer stands between them (`\int x`, not `\intx`).
    if rest.starts_with(|c: char| c.is_ascii_alphabetic())
        && op.ends_with(|c: char| c.is_ascii_alphabetic())
    {
        format!("{op} {rest}")
    } else {
        op + &rest
    }
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

thread_local! {
    /// Inside an `m:eqArr` (a depth), where a run's `&` is an alignment point.
    static EQARR: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// An alignment point was written in the innermost `m:eqArr`.
    static ALIGNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// An equation array: its rows joined by `\\` as docling joins them. Word
/// marks an alignment point with an `&` typed in a row (or `m:aln` on the run
/// it starts), which docling escapes to a literal `\&`; here it stays an
/// alignment point, and the rows go in an `aligned` environment, where `&`
/// and `\\` are legal. Several rows without one are `gathered`; a single
/// row stays as docling writes it.
fn do_eqarr(e: XmlNode) -> String {
    let outer = ALIGNED.with(|a| a.replace(false));
    EQARR.with(|d| d.set(d.get() + 1));
    let rows: Vec<String> = elems(e)
        .filter(|c| local(*c) == "e")
        .map(process_children)
        .collect();
    EQARR.with(|d| d.set(d.get() - 1));
    let aligned = ALIGNED.with(|a| a.replace(outer));
    let body = rows.join("\\\\");
    if aligned {
        format!("\\begin{{aligned}}{body}\\end{{aligned}}")
    } else if rows.len() > 1 {
        format!("\\begin{{gathered}}{body}\\end{{gathered}}")
    } else {
        body
    }
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

/// A run's text: its `<m:t>`s.
fn run_text(e: XmlNode) -> String {
    e.descendants()
        .filter(|n| local(*n) == "t")
        .filter_map(|t| t.text())
        .collect()
}

/// A run's math formatting (`m:rPr`), which docling ignores: normal text
/// (`m:nor`), the script (`m:scr`) and the style (`m:sty`). The LaTeX it
/// wraps the run in is [`RunStyle::wrap`].
#[derive(Default, PartialEq, Eq)]
struct RunStyle {
    /// `m:nor`: the run is text, not math — `\text{…}`, its spaces kept.
    nor: bool,
    /// The font wrapper for `m:scr` / `m:sty`, if any.
    font: Option<&'static str>,
    /// `m:aln`: an alignment point of an equation array starts the run.
    aln: bool,
}

impl RunStyle {
    fn of(r: XmlNode) -> Self {
        let mut s = RunStyle::default();
        let Some(rpr) = elems(r).find(|c| local(*c) == "rPr") else {
            return s;
        };
        let (mut scr, mut sty) = (None, None);
        for c in elems(rpr) {
            let val = c
                .attributes()
                .find(|a| a.name() == "val")
                .map(|a| a.value());
            match local(c) {
                "nor" => s.nor = on_off(val),
                "aln" => s.aln = on_off(val),
                "scr" => scr = val,
                "sty" => sty = val,
                _ => {}
            }
        }
        // A script names its own alphabet; bold applies to the default one.
        // `p` (plain) is not wrapped: Word sets it on every function name,
        // `lim` and operator, which LaTeX already sets upright or where
        // `\mathrm` changes nothing but the noise.
        s.font = match scr {
            Some("double-struck") => Some("\\mathbb"),
            Some("script") => Some("\\mathcal"),
            Some("fraktur") => Some("\\mathfrak"),
            Some("sans-serif") => Some("\\mathsf"),
            Some("monospace") => Some("\\mathtt"),
            _ => match sty {
                Some("b") => Some("\\mathbf"),
                Some("bi") => Some("\\boldsymbol"),
                _ => None,
            },
        };
        s
    }

    /// The run text `body` in this style's LaTeX.
    fn wrap(&self, body: String) -> String {
        if body.is_empty() {
            return body;
        }
        let body = if self.nor {
            format!("\\text{{{body}}}")
        } else if let Some(font) = self.font {
            format!("{font}{{{body}}}")
        } else {
            body
        };
        if self.aln {
            ALIGNED.with(|a| a.set(true));
            format!("&{body}")
        } else {
            body
        }
    }
}

/// A run's text in LaTeX, before its style's wrapper: math ([`do_r`]'s
/// mapping) or, for `m:nor`, text-mode escaped.
fn run_body(e: XmlNode, style: &RunStyle) -> String {
    if style.nor {
        escape_text(&run_text(e))
    } else {
        math_run(e)
    }
}

/// Text-mode escaping for a `\text{…}` run: the LaTeX specials, the
/// characters text mode has no glyph for as their text macros.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\textbackslash{}"),
            '^' => out.push_str("\\textasciicircum{}"),
            '~' => out.push_str("\\textasciitilde{}"),
            '{' | '}' | '_' | '#' | '&' | '$' | '%' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// A `<m:r>` run in its style (`m:rPr`): [`RunStyle`].
fn do_r(e: XmlNode) -> String {
    let style = RunStyle::of(e);
    let body = run_body(e, &style);
    style.wrap(body)
}

/// Process a `<m:r>` run's text: each character mapped via `process_unicode`,
/// then the whole string LaTeX-escaped, with the brace/caret un-escaping dance
/// from the upstream `do_r`. In an equation array an `&` stays an alignment
/// point ([`do_eqarr`]).
fn math_run(e: XmlNode) -> String {
    let found = run_text(e);
    let in_eqarr = EQARR.with(|d| d.get() > 0);
    let mut mapped = String::with_capacity(found.len());
    // Whether `mapped` ends with a superscript entry (`²` → `  ^{2}  `).
    let mut in_exponent = false;
    for c in found.chars() {
        if c == '&' && in_eqarr {
            // An alignment point, not pylatexenc's padded `\&`.
            mapped.push('&');
            in_exponent = false;
            continue;
        }
        let latex = process_unicode(c);
        match exponent(&latex) {
            // Consecutive superscript digits (`10²³`) are one exponent,
            // `^{23}`: two would be a double superscript (#1253).
            Some(digits) if in_exponent => {
                mapped.truncate(mapped.len() - "}  ".len());
                mapped.push_str(digits);
                mapped.push_str("}  ");
            }
            digits => {
                mapped.push_str(&latex);
                in_exponent = digits.is_some();
            }
        }
    }
    let mut proc = escape_latex(&mapped);

    if !found.contains('{') && proc.contains("\\{") {
        proc = proc.replace("\\{", "{");
    }
    if !found.contains('}') && proc.contains("\\}") {
        proc = proc.replace("\\}", "}");
    }
    // A caret in the source is a math superscript operator, not a literal; so
    // is one the table writes (`²` → `^{2}`, `℉` → `^\circ F`), which
    // docling escapes to the text accent `\^` (#1253).
    if mapped.contains('^') && proc.contains("\\^") {
        proc = proc.replace("\\^", "^");
    }
    if found.contains('&') && in_eqarr {
        proc = proc.replace("\\&", "&");
        ALIGNED.with(|a| a.set(true));
    }
    proc
}

/// The exponent of a superscript entry of [`UNICODE_LATEX`] (`  ^{2}  ` → `2`).
fn exponent(latex: &str) -> Option<&str> {
    latex.strip_prefix("  ^{")?.strip_suffix("}  ")
}

/// Per-character Unicode → LaTeX: the strings docling's `process_unicode`
/// returns (`_MATH_CHAR_MAP` first, else pylatexenc's `\ensuremath{…}`/`{…}`
/// unwrapped to a **two-space**-padded macro; the two-space padding is
/// load-bearing, see [`to_latex`]), generated into [`UNICODE_LATEX`].
/// Deliberate differences: docling replaces *every* `}` with a space when it
/// unwraps `\ensuremath{…}`, which leaves a macro with an argument unbalanced
/// (`ℝ` → `\mathbb{R`); the table keeps that macro's braces (`\mathbb{R}`).
/// `√` is `\surd`, and `{` / `}` are not in the table: [`do_r`] escapes them
/// with `escape_latex` alone. Where docling writes a text-mode command math
/// renderers reject (`→` → `\text{ \textrightarrow }`, #1253), the table has
/// the math-mode command (`\rightarrow`) or, for a text symbol, the character.
/// A character outside the table is kept as typed.
fn process_unicode(c: char) -> String {
    UNICODE_LATEX
        .binary_search_by_key(&c, |&(k, _)| k)
        .map_or_else(|_| c.to_string(), |i| UNICODE_LATEX[i].1.to_string())
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

/// Function-name (`<m:r>` inside `<m:fName>`) → LaTeX template with `{fe}`
/// slot: docling's `FUNC`.
fn func_latex(name: &str) -> Option<&'static str> {
    Some(match name {
        "sin" => "\\sin({fe})",
        "cos" => "\\cos({fe})",
        "tan" => "\\tan({fe})",
        "arcsin" => "\\arcsin({fe})",
        "arccos" => "\\arccos({fe})",
        "arctan" => "\\arctan({fe})",
        "arccot" => "\\arccot({fe})",
        "sinh" => "\\sinh({fe})",
        "cosh" => "\\cosh({fe})",
        "tanh" => "\\tanh({fe})",
        "coth" => "\\coth({fe})",
        "sec" => "\\sec({fe})",
        "csc" => "\\csc({fe})",
        "mod" => "\\mod {fe}",
        "max" => "\\max({fe})",
        "min" => "\\min({fe})",
        "log" => "\\log({fe})",
        "ln" => "\\ln({fe})",
        "exp" => "\\exp({fe})",
        "det" => "\\det({fe})",
        "gcd" => "\\gcd({fe})",
        "deg" => "\\deg({fe})",
        "hom" => "\\hom({fe})",
        "ker" => "\\ker({fe})",
        "dim" => "\\dim({fe})",
        "arg" => "\\arg({fe})",
        "inf" => "\\inf({fe})",
        "sup" => "\\sup({fe})",
        "lim" => "\\lim({fe})",
        "Pr" => "\\Pr({fe})",
        _ => return None,
    })
}

/// Accent / group character → LaTeX template: docling's `CHR` (top accents,
/// bottom accents, over/under groups), then the spacing forms of the common
/// accents, which other writers than Word put in `m:chr` (pandoc writes
/// U+203E for `\bar`) and docling does not know.
fn accent_latex(chr: &str) -> Option<&'static str> {
    Some(match chr {
        "\u{0300}" => "\\grave{%s}",
        "\u{0301}" => "\\acute{%s}",
        "\u{0302}" => "\\hat{%s}",
        "\u{0303}" => "\\tilde{%s}",
        "\u{0304}" => "\\bar{%s}",
        "\u{0305}" => "\\overbar{%s}",
        "\u{0306}" => "\\breve{%s}",
        "\u{0307}" => "\\dot{%s}",
        "\u{0308}" => "\\ddot{%s}",
        "\u{0309}" => "\\ovhook{%s}",
        "\u{030a}" => "\\ocirc{%s}",
        "\u{030c}" => "\\check{%s}",
        "\u{0310}" => "\\candra{%s}",
        "\u{0312}" => "\\oturnedcomma{%s}",
        "\u{0315}" => "\\ocommatopright{%s}",
        "\u{031a}" => "\\droang{%s}",
        "\u{0338}" => "\\not{%s}",
        "\u{20d0}" => "\\leftharpoonaccent{%s}",
        "\u{20d1}" => "\\rightharpoonaccent{%s}",
        "\u{20d2}" => "\\vertoverlay{%s}",
        "\u{20d6}" => "\\overleftarrow{%s}",
        "\u{20d7}" => "\\vec{%s}",
        "\u{20db}" => "\\dddot{%s}",
        "\u{20dc}" => "\\ddddot{%s}",
        "\u{20e1}" => "\\overleftrightarrow{%s}",
        "\u{20e7}" => "\\annuity{%s}",
        "\u{20e9}" => "\\widebridgeabove{%s}",
        "\u{20f0}" => "\\asteraccent{%s}",
        "\u{0330}" => "\\wideutilde{%s}",
        "\u{0331}" => "\\underbar{%s}",
        "\u{20e8}" => "\\threeunderdot{%s}",
        "\u{20ec}" => "\\underrightharpoondown{%s}",
        "\u{20ed}" => "\\underleftharpoondown{%s}",
        // docling: `\underledtarrow`, a typo.
        "\u{20ee}" => "\\underleftarrow{%s}",
        "\u{20ef}" => "\\underrightarrow{%s}",
        "\u{23b4}" => "\\overbracket{%s}",
        "\u{23dc}" => "\\overparen{%s}",
        "\u{23de}" => "\\overbrace{%s}",
        "\u{23b5}" => "\\underbracket{%s}",
        "\u{23dd}" => "\\underparen{%s}",
        "\u{23df}" => "\\underbrace{%s}",
        // Spacing forms (not in docling's table).
        "\u{0060}" => "\\grave{%s}",
        "\u{00b4}" => "\\acute{%s}",
        "\u{02c6}" => "\\hat{%s}",
        "\u{02dc}" => "\\tilde{%s}",
        "\u{00af}" => "\\bar{%s}",
        "\u{203e}" => "\\bar{%s}",
        "\u{02d8}" => "\\breve{%s}",
        "\u{02d9}" => "\\dot{%s}",
        "\u{00a8}" => "\\ddot{%s}",
        "\u{02c7}" => "\\check{%s}",
        _ => return None,
    })
}

/// N-ary operator character → LaTeX: docling's `CHR_BO`, plus two operators
/// Word's equation gallery also offers that it lacks.
fn big_op(chr: &str) -> Option<&'static str> {
    Some(match chr {
        "\u{2140}" => "\\Bbbsum",
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
        "\u{2a00}" => "\\bigodot",
        "\u{2a01}" => "\\bigoplus",
        "\u{2a02}" => "\\bigotimes",
        // Not in docling's table.
        "\u{2a04}" => "\\biguplus",
        "\u{2a06}" => "\\bigsqcup",
        _ => return None,
    })
}

/// A delimiter character in LaTeX: docling's `T` table ([`greek_or`]), then
/// the bracket characters LaTeX names — `⟨u|v⟩`, `⌈x⌉`, `⌊x⌋`, `‖v‖`, `⟦x⟧`
/// (two nested `\left[`),
/// which docling writes as typed after `\left` / `\right`, where they are not
/// delimiters. Between the elements (`sep`) `∣` is the relation `\mid`
/// (`{x ∣ x > 0}`); as a bracket it is `|`.
fn delim_or(key: Option<&str>, default: &str, sep: bool) -> String {
    let c = greek_or(key, default);
    let named = match c.as_str() {
        "\u{2223}" if sep => "\\mid ",
        "\u{27e8}" | "\u{2329}" | "\u{3008}" => "\\langle ",
        "\u{27e9}" | "\u{232a}" | "\u{3009}" => "\\rangle ",
        "\u{2308}" => "\\lceil ",
        "\u{2309}" => "\\rceil ",
        "\u{230a}" => "\\lfloor ",
        "\u{230b}" => "\\rfloor ",
        "\u{2016}" | "\u{2225}" => "\\| ",
        "\u{2223}" => "| ",
        // `\llbracket` is no `\left` delimiter (stmaryrd, KaTeX): two nested
        // square brackets, the way plain LaTeX sets one.
        "\u{27e6}" if !sep => "[\\!\\left[",
        "\u{27e7}" if !sep => "]\\!\\right]",
        _ => return c,
    };
    named.to_string()
}

/// A delimiter character through docling's `T` table (`get_val(…, store=T)`),
/// else the character itself; `default` when the property is absent.
fn greek_or(key: Option<&str>, default: &str) -> String {
    match key {
        None => default.to_string(),
        Some(k) => t_table(k).unwrap_or(k).to_string(),
    }
}

/// docling's `T` (`latex_dict.py`): math-italic letters, arrows, relations.
/// Five entries are fixed where docling's is not the character's macro
/// (`docs/MIGRATION.md`).
fn t_table(k: &str) -> Option<&'static str> {
    Some(match k {
        // Greek letters
        "\u{1d6fc}" => "\\alpha ",
        "\u{1d6fd}" => "\\beta ",
        "\u{1d6fe}" => "\\gamma ",
        "\u{1d6ff}" => "\\delta ", // docling: `\theta `
        "\u{1d700}" => "\\epsilon ",
        "\u{1d701}" => "\\zeta ",
        "\u{1d702}" => "\\eta ",
        "\u{1d703}" => "\\theta ",
        "\u{1d704}" => "\\iota ",
        "\u{1d705}" => "\\kappa ",
        "\u{1d706}" => "\\lambda ",
        "\u{1d707}" => "\\mu ", // docling: `\m `
        "\u{1d708}" => "\\nu ", // docling: `\n `
        "\u{1d709}" => "\\xi ",
        "\u{1d70a}" => "\\omicron ",
        "\u{1d70b}" => "\\pi ",
        "\u{1d70c}" => "\\rho ",
        "\u{1d70d}" => "\\varsigma ",
        "\u{1d70e}" => "\\sigma ",
        "\u{1d70f}" => "\\tau ", // docling: `\ta `
        "\u{1d710}" => "\\upsilon ",
        "\u{1d711}" => "\\phi ",
        "\u{1d712}" => "\\chi ",
        "\u{1d713}" => "\\psi ",
        "\u{1d714}" => "\\omega ",
        "\u{1d715}" => "\\partial ",
        "\u{1d716}" => "\\varepsilon ",
        "\u{1d717}" => "\\vartheta ",
        "\u{1d718}" => "\\varkappa ",
        "\u{1d719}" => "\\varphi ",
        "\u{1d71a}" => "\\varrho ",
        "\u{1d71b}" => "\\varpi ",
        // Relation symbols
        "\u{2190}" => "\\leftarrow ",
        "\u{2191}" => "\\uparrow ",
        "\u{2192}" => "\\rightarrow ",
        "\u{2193}" => "\\downarrow ", // docling: `\downright `
        "\u{2194}" => "\\leftrightarrow ",
        "\u{2195}" => "\\updownarrow ",
        "\u{2196}" => "\\nwarrow ",
        "\u{2197}" => "\\nearrow ",
        "\u{2198}" => "\\searrow ",
        "\u{2199}" => "\\swarrow ",
        "\u{22ee}" => "\\vdots ",
        "\u{22ef}" => "\\cdots ",
        "\u{22f0}" => "\\adots ",
        "\u{22f1}" => "\\ddots ",
        "\u{2260}" => "\\ne ",
        "\u{2264}" => "\\leq ",
        "\u{2265}" => "\\geq ",
        "\u{2266}" => "\\leqq ",
        "\u{2267}" => "\\geqq ",
        "\u{2268}" => "\\lneqq ",
        "\u{2269}" => "\\gneqq ",
        "\u{226a}" => "\\ll ",
        "\u{226b}" => "\\gg ",
        "\u{2208}" => "\\in ",
        "\u{2209}" => "\\notin ",
        "\u{220b}" => "\\ni ",
        "\u{220c}" => "\\nni ",
        // Ordinary symbols
        "\u{221e}" => "\\infty ",
        // Binary relations
        "\u{b1}" => "\\pm ",
        "\u{2213}" => "\\mp ",
        // Italic, Latin, uppercase
        "\u{1d434}" => "A",
        "\u{1d435}" => "B",
        "\u{1d436}" => "C",
        "\u{1d437}" => "D",
        "\u{1d438}" => "E",
        "\u{1d439}" => "F",
        "\u{1d43a}" => "G",
        "\u{1d43b}" => "H",
        "\u{1d43c}" => "I",
        "\u{1d43d}" => "J",
        "\u{1d43e}" => "K",
        "\u{1d43f}" => "L",
        "\u{1d440}" => "M",
        "\u{1d441}" => "N",
        "\u{1d442}" => "O",
        "\u{1d443}" => "P",
        "\u{1d444}" => "Q",
        "\u{1d445}" => "R",
        "\u{1d446}" => "S",
        "\u{1d447}" => "T",
        "\u{1d448}" => "U",
        "\u{1d449}" => "V",
        "\u{1d44a}" => "W",
        "\u{1d44b}" => "X",
        "\u{1d44c}" => "Y",
        "\u{1d44d}" => "Z",
        // Italic, Latin, lowercase
        "\u{1d44e}" => "a",
        "\u{1d44f}" => "b",
        "\u{1d450}" => "c",
        "\u{1d451}" => "d",
        "\u{1d452}" => "e",
        "\u{1d453}" => "f",
        "\u{1d454}" => "g",
        "\u{1d456}" => "i",
        "\u{1d457}" => "j",
        "\u{1d458}" => "k",
        "\u{1d459}" => "l",
        "\u{1d45a}" => "m",
        "\u{1d45b}" => "n",
        "\u{1d45c}" => "o",
        "\u{1d45d}" => "p",
        "\u{1d45e}" => "q",
        "\u{1d45f}" => "r",
        "\u{1d460}" => "s",
        "\u{1d461}" => "t",
        "\u{1d462}" => "u",
        "\u{1d463}" => "v",
        "\u{1d464}" => "w",
        "\u{1d465}" => "x",
        "\u{1d466}" => "y",
        "\u{1d467}" => "z",
        _ => return None,
    })
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

    #[test]
    fn math_symbols_map_like_docling() {
        // docling maps every character of a run through pylatexenc; these are
        // its strings (relations, operators, arrows, Greek), not just the few
        // the corpus happens to use.
        let xml = r#"<r xmlns:m="m"><m:oMath><m:r><m:t>&#8704;x&#8712;A, x&#8805;0 &#8658; &#8706;f&#8800;0, A&#8746;B&#8838;C, &#945;&#8741;&#946;</m:t></m:r></m:oMath></r>"#;
        assert_eq!(
            latex(xml),
            r"\forall x \in A, x \geq 0  \Rightarrow   \partial f \neq 0, A \cup B \subseteq C,  \alpha  \parallel  \beta"
        );
    }

    #[test]
    fn delimiter_keeps_every_element_and_its_separator() {
        // `<m:d>` holds one `<m:e>` per element; `m:sepChr` (`|` when absent,
        // ECMA-376 Part 1) goes between them, as in docling's `do_d`.
        let d = |pr: &str, es: &[&str]| {
            let es: String = es
                .iter()
                .map(|t| format!("<m:e><m:r><m:t>{t}</m:t></m:r></m:e>"))
                .collect();
            latex(&format!(
                r#"<r xmlns:m="m"><m:oMath><m:d>{pr}{es}</m:d></m:oMath></r>"#
            ))
        };
        assert_eq!(d("", &["a", "b", "c"]), r"\left(a|b|c\right)");
        let comma = r#"<m:dPr><m:sepChr m:val=","/></m:dPr>"#;
        assert_eq!(d(comma, &["a", "b"]), r"\left(a,b\right)");
        let none = r#"<m:dPr><m:sepChr m:val=""/></m:dPr>"#;
        assert_eq!(d(none, &["a", "b"]), r"\left(ab\right)");
        let set =
            r#"<m:dPr><m:begChr m:val="{"/><m:sepChr m:val="&amp;"/><m:endChr m:val="}"/></m:dPr>"#;
        assert_eq!(d(set, &["x", "y"]), r"\left\{x\&y\right\}");
    }

    #[test]
    fn delimiter_characters_map_through_docling_t_table() {
        // docling looks `m:begChr`, `m:sepChr` and `m:endChr` up in its `T`
        // table (`get_val(…, store=T)`) before `escape_latex`.
        let d = |pr: &str| {
            latex(&format!(
                r#"<r xmlns:m="m"><m:oMath><m:d><m:dPr>{pr}</m:dPr><m:e><m:r><m:t>a</m:t></m:r></m:e><m:e><m:r><m:t>b</m:t></m:r></m:e></m:d></m:oMath></r>"#
            ))
        };
        let arrow = r#"<m:sepChr m:val="&#8594;"/>"#;
        assert_eq!(d(arrow), r"\left(a\rightarrow b\right)");
        let arrows =
            r#"<m:begChr m:val="&#8593;"/><m:sepChr m:val="&#8739;"/><m:endChr m:val="&#8593;"/>"#;
        assert_eq!(d(arrows), r"\left\uparrow a\mid b\right\uparrow");
    }

    #[test]
    fn t_table_typos_are_fixed() {
        // docling's `T` has `\downright` for `↓` and `\theta`, `\m`, `\n`,
        // `\ta` for math-italic δ, μ, ν, τ: none is the character's macro.
        let sep = |c: &str| {
            latex(&format!(
                r#"<r xmlns:m="m"><m:oMath><m:d><m:dPr><m:sepChr m:val="{c}"/></m:dPr><m:e><m:r><m:t>a</m:t></m:r></m:e><m:e><m:r><m:t>b</m:t></m:r></m:e></m:d></m:oMath></r>"#
            ))
        };
        assert_eq!(sep("&#8595;"), r"\left(a\downarrow b\right)");
        assert_eq!(sep("&#120575;"), r"\left(a\delta b\right)");
        assert_eq!(sep("&#120583;"), r"\left(a\mu b\right)");
        assert_eq!(sep("&#120584;"), r"\left(a\nu b\right)");
        assert_eq!(sep("&#120591;"), r"\left(a\tau b\right)");
    }

    fn acc(chr: &str, base: &str) -> String {
        latex(&format!(
            r#"<r xmlns:m="m"><m:oMath><m:acc><m:accPr><m:chr m:val="{chr}"/></m:accPr><m:e><m:r><m:t>{base}</m:t></m:r></m:e></m:acc></m:oMath></r>"#
        ))
    }

    fn nary(chr: &str) -> String {
        latex(&format!(
            r#"<r xmlns:m="m"><m:oMath><m:nary><m:naryPr><m:chr m:val="{chr}"/></m:naryPr><m:sub><m:r><m:t>i</m:t></m:r></m:sub><m:sup><m:r><m:t>n</m:t></m:r></m:sup><m:e><m:r><m:t>A</m:t></m:r></m:e></m:nary></m:oMath></r>"#
        ))
    }

    fn func(name: &str) -> String {
        latex(&format!(
            r#"<r xmlns:m="m"><m:oMath><m:func><m:fName><m:r><m:t>{name}</m:t></m:r></m:fName><m:e><m:r><m:t>x</m:t></m:r></m:e></m:func></m:oMath></r>"#
        ))
    }

    #[test]
    fn accents_follow_docling_table() {
        // docling's `CHR`: a dot accent is not a hat.
        assert_eq!(acc("&#x307;", "x"), r"\dot{x}");
        assert_eq!(acc("&#x308;", "x"), r"\ddot{x}");
        assert_eq!(acc("&#x306;", "x"), r"\breve{x}");
        assert_eq!(acc("&#x30C;", "x"), r"\check{x}");
        assert_eq!(acc("&#x20D6;", "AB"), r"\overleftarrow{AB}");
        assert_eq!(acc("&#x20E1;", "AB"), r"\overleftrightarrow{AB}");
        // No `m:chr`: the circumflex, ECMA-376's default accent.
        assert_eq!(
            latex(
                r#"<r xmlns:m="m"><m:oMath><m:acc><m:e><m:r><m:t>x</m:t></m:r></m:e></m:acc></m:oMath></r>"#
            ),
            r"\hat{x}"
        );
    }

    #[test]
    fn backslash_and_prime_are_not_raw() {
        // A literal `\` must not start a control sequence (`A\B` is `\B`), and
        // `′` is not a LaTeX character: both as docling writes them.
        let xml = r#"<r xmlns:m="m"><m:oMath><m:r><m:t>A\B</m:t></m:r></m:oMath></r>"#;
        assert_eq!(latex(xml), r"A \text{ \textbackslash } B");
        let xml = r#"<r xmlns:m="m"><m:oMath><m:sSup><m:e><m:r><m:t>f</m:t></m:r></m:e><m:sup><m:r><m:t>&#8242;</m:t></m:r></m:sup></m:sSup></m:oMath></r>"#;
        assert_eq!(latex(xml), r"f^{ ' }");
    }

    #[test]
    fn letterlike_and_alphanumeric_macros_keep_their_braces() {
        // docling replaces every `}` with a space when it unwraps
        // `\ensuremath{…}`, which leaves `\mathbb{R` unbalanced; the macro's own
        // braces are kept here.
        let xml = r#"<r xmlns:m="m"><m:oMath><m:r><m:t>x&#8712;&#8477;, &#119808;</m:t></m:r></m:oMath></r>"#;
        assert_eq!(latex(xml), r"x \in  \mathbb{R} ,  \mathbf{A}");
    }

    #[test]
    fn literal_braces_are_escaped_once() {
        // Set braces typed in a run are escaped by `escape_latex`, not mapped
        // to pylatexenc's `{\{}` (which `do_r`'s brace un-escaping turns into
        // `\{\{\}…` or an unbalanced `a{\}\}`).
        let xml = r#"<r xmlns:m="m"><m:oMath><m:r><m:t>{1,2}</m:t></m:r></m:oMath></r>"#;
        assert_eq!(latex(xml), r"\{1,2\}");
        let xml = r#"<r xmlns:m="m"><m:oMath><m:r><m:t>a}</m:t></m:r></m:oMath></r>"#;
        assert_eq!(latex(xml), r"a\}");
    }

    #[test]
    fn square_root_sign_is_surd() {
        // A bare `√` is the radical sign, not an empty `\sqrt{}` before its
        // operand.
        let xml = r#"<r xmlns:m="m"><m:oMath><m:r><m:t>&#8730;x</m:t></m:r></m:oMath></r>"#;
        assert_eq!(latex(xml), r"\surd x");
    }

    #[test]
    fn spacing_accent_characters_are_accents() {
        // Writers other than Word put the spacing form of an accent in
        // `m:chr` (pandoc's `\bar` is U+203E); docling drops the base for them.
        assert_eq!(acc("&#x203E;", "x"), r"\bar{x}");
        assert_eq!(acc("&#xAF;", "x"), r"\bar{x}");
        assert_eq!(acc("&#x2C6;", "x"), r"\hat{x}");
        assert_eq!(acc("&#x2DC;", "x"), r"\tilde{x}");
        assert_eq!(acc("&#x2D9;", "x"), r"\dot{x}");
        assert_eq!(acc("&#xA8;", "x"), r"\ddot{x}");
        // Any other character still sits above its base.
        assert_eq!(acc("&#x2605;", "x"), "\\overset{\u{2605}}{x}");
    }

    #[test]
    fn n_ary_operators_follow_docling_table() {
        assert_eq!(nary("&#x2A01;"), r"\bigoplus_{i}^{n}A");
        assert_eq!(nary("&#x2A02;"), r"\bigotimes_{i}^{n}A");
        assert_eq!(nary("&#x2140;"), r"\Bbbsum_{i}^{n}A");
        // Word's gallery also has these two.
        assert_eq!(nary("&#x2A04;"), r"\biguplus_{i}^{n}A");
        assert_eq!(nary("&#x2A06;"), r"\bigsqcup_{i}^{n}A");
        // An operator outside the table is kept, never turned into `\int`.
        assert_eq!(nary("&#x2AFF;"), "\u{2aff}_{i}^{n}A");
        // No `m:naryPr` (the schema allows it): an integral, not a bare `_{0}^{1}`.
        assert_eq!(
            latex(
                r#"<r xmlns:m="m"><m:oMath><m:nary><m:sub><m:r><m:t>0</m:t></m:r></m:sub><m:sup><m:r><m:t>1</m:t></m:r></m:sup><m:e><m:r><m:t>x</m:t></m:r></m:e></m:nary></m:oMath></r>"#
            ),
            r"\int_{0}^{1}x"
        );
    }

    #[test]
    fn function_names_follow_docling_table() {
        assert_eq!(func("arcsin"), r"\arcsin(x)");
        assert_eq!(func("coth"), r"\coth(x)");
        assert_eq!(func("gcd"), r"\gcd(x)");
        assert_eq!(func("ker"), r"\ker(x)");
        assert_eq!(func("Pr"), r"\Pr(x)");
        assert_eq!(func("mod"), r"\mod x");
    }

    #[test]
    fn combining_left_arrow_below_is_underleftarrow() {
        // docling's CHR has the typo `\underledtarrow` for U+20EE.
        let xml = r#"<r xmlns:m="m"><m:oMath><m:acc><m:accPr><m:chr m:val="&#8430;"/></m:accPr><m:e><m:r><m:t>x</m:t></m:r></m:e></m:acc></m:oMath></r>"#;
        assert_eq!(latex(xml), r"\underleftarrow{x}");
    }

    #[test]
    fn unknown_group_character_sits_over_or_under_its_base() {
        // Word's "arrow over text" is a `m:groupChr` with `→` and `pos="top"`:
        // the arrow goes above the base (docling keeps the arrow alone and
        // drops the base), below it when `m:pos` is `bot` or absent (the
        // schema's default); a character of the table keeps its macro.
        let g = |pr: &str| {
            latex(&format!(
                r#"<r xmlns:m="m"><m:oMath><m:groupChr><m:groupChrPr>{pr}</m:groupChrPr><m:e><m:r><m:t>x</m:t></m:r></m:e></m:groupChr></m:oMath></r>"#
            ))
        };
        assert_eq!(
            g(r#"<m:chr m:val="&#8594;"/><m:pos m:val="top"/>"#),
            r"\overset{→}{x}"
        );
        assert_eq!(
            g(r#"<m:chr m:val="&#8594;"/><m:pos m:val="bot"/>"#),
            r"\underset{→}{x}"
        );
        assert_eq!(g(r#"<m:chr m:val="&#8592;"/>"#), r"\underset{←}{x}");
        assert_eq!(
            g(r#"<m:chr m:val="&#9182;"/><m:pos m:val="top"/>"#),
            r"\overbrace{x}"
        );
        assert_eq!(g(""), r"\underbrace{x}");
    }

    fn math(body: &str) -> String {
        latex(&format!(r#"<r xmlns:m="m"><m:oMath>{body}</m:oMath></r>"#))
    }

    fn run(rpr: &str, t: &str) -> String {
        format!(r#"<m:r><m:rPr>{rpr}</m:rPr><m:t xml:space="preserve">{t}</m:t></m:r>"#)
    }

    #[test]
    fn hidden_n_ary_limits_are_left_out() {
        // #1239: Word hides an n-ary's limits with `m:subHide` / `m:supHide`
        // and keeps the empty `m:sub` / `m:sup`; docling writes `\int_{}^{}`.
        let nary = |pr: &str, e: &str| {
            math(&format!(
                r#"<m:nary><m:naryPr>{pr}</m:naryPr><m:sub/><m:sup/><m:e><m:r><m:t>{e}</m:t></m:r></m:e></m:nary>"#
            ))
        };
        let hidden = r#"<m:subHide m:val="1"/><m:supHide m:val="1"/>"#;
        assert_eq!(nary(hidden, "x"), r"\int x");
        assert_eq!(
            nary(&format!(r#"<m:chr m:val="&#8896;"/>{hidden}"#), "x"),
            r"\bigwedge x"
        );
        assert_eq!(nary(hidden, "(2x+1)dx"), r"\int(2x+1)dx");
        // Only the hidden one goes; `m:val="0"` hides nothing.
        let sup_only = r#"<m:supHide/><m:subHide m:val="0"/>"#;
        assert_eq!(nary(sup_only, "x"), r"\int_{}x");
        // Not hidden: docling's empty placeholders, unchanged.
        assert_eq!(nary("", "x"), r"\int_{}^{}x");
    }

    #[test]
    fn run_properties_set_the_alphabet() {
        // #1239: `m:nor` is text, `m:scr` an alphabet, `m:sty` bold; docling
        // ignores all three.
        let nor = r#"<m:nor/>"#;
        assert_eq!(
            math(&format!(
                "{}{}{}",
                run("", "x"),
                run(nor, " for all "),
                run("", "y")
            )),
            r"x\text{ for all }y"
        );
        // Adjacent runs of one style share the wrapper.
        assert_eq!(
            math(&format!("{}{}", run(nor, "if "), run(nor, "x &amp; y"))),
            r"\text{if x \& y}"
        );
        let scr = |v: &str| format!(r#"<m:scr m:val="{v}"/>"#);
        assert_eq!(math(&run(&scr("double-struck"), "R")), r"\mathbb{R}");
        assert_eq!(math(&run(&scr("script"), "L")), r"\mathcal{L}");
        assert_eq!(math(&run(&scr("fraktur"), "g")), r"\mathfrak{g}");
        assert_eq!(math(&run(&scr("sans-serif"), "A")), r"\mathsf{A}");
        assert_eq!(math(&run(&scr("monospace"), "f")), r"\mathtt{f}");
        assert_eq!(math(&run(&scr("roman"), "x")), "x");
        let sty = |v: &str| format!(r#"<m:sty m:val="{v}"/>"#);
        assert_eq!(math(&run(&sty("b"), "v")), r"\mathbf{v}");
        assert_eq!(math(&run(&sty("bi"), "v")), r"\boldsymbol{v}");
        assert_eq!(math(&run(&sty("i"), "v")), "v");
        // Plain is what Word sets on function names and operators: unwrapped.
        assert_eq!(math(&run(&sty("p"), "d")), "d");
        // A function name keeps its lookup under `m:sty="p"`.
        assert_eq!(
            math(&format!(
                r#"<m:func><m:fName>{}</m:fName><m:e><m:r><m:t>x</m:t></m:r></m:e></m:func>"#,
                run(&sty("p"), "sin")
            )),
            r"\sin(x)"
        );
        // Text mode escapes its own specials.
        assert_eq!(
            math(&run(nor, r"a\b ~c")),
            r"\text{a\textbackslash{}b \textasciitilde{}c}"
        );
    }

    #[test]
    fn bracket_characters_are_latex_delimiters() {
        // #1239: docling writes `⟨`, `⌈`, `‖`… as typed after `\left`, where
        // LaTeX does not take them.
        let d = |beg: &str, end: &str| {
            math(&format!(
                r#"<m:d><m:dPr><m:begChr m:val="{beg}"/><m:endChr m:val="{end}"/></m:dPr><m:e><m:r><m:t>u</m:t></m:r></m:e><m:e><m:r><m:t>v</m:t></m:r></m:e></m:d>"#
            ))
        };
        assert_eq!(d("&#10216;", "&#10217;"), r"\left\langle u|v\right\rangle");
        assert_eq!(d("&#8968;", "&#8969;"), r"\left\lceil u|v\right\rceil");
        assert_eq!(d("&#8970;", "&#8971;"), r"\left\lfloor u|v\right\rfloor");
        assert_eq!(d("&#8214;", "&#8214;"), r"\left\| u|v\right\|");
        assert_eq!(
            d("&#10214;", "&#10215;"),
            r"\left[\!\left[u|v\right]\!\right]"
        );
        // An empty character is no delimiter (`.`), as before.
        assert_eq!(d("", "&#10217;"), r"\left.u|v\right\rangle");
    }

    #[test]
    fn equation_array_alignment_points_align() {
        // #1239: an `&` typed in an equation array row is Word's alignment
        // point; docling escapes it to a literal `\&`.
        let row = |t: &str| format!("<m:e><m:r><m:t>{t}</m:t></m:r></m:e>");
        let arr = |rows: &[&str]| {
            let rows: String = rows.iter().map(|r| row(r)).collect();
            math(&format!("<m:eqArr>{rows}</m:eqArr>"))
        };
        assert_eq!(
            arr(&["x&amp;=1", "y&amp;=2"]),
            r"\begin{aligned}x&=1\\y&=2\end{aligned}"
        );
        // Rows without alignment points: centred, one per line.
        assert_eq!(
            arr(&["x=1", "y=2"]),
            r"\begin{gathered}x=1\\y=2\end{gathered}"
        );
        // A single row stays as docling writes it.
        assert_eq!(arr(&["a+b"]), "a+b");
        // `m:aln` on a run marks the point too.
        let aln = format!(
            "<m:eqArr><m:e><m:r><m:t>x</m:t></m:r>{}<m:r><m:t>1</m:t></m:r></m:e>{}</m:eqArr>",
            run("<m:aln/>", "="),
            row("y=2")
        );
        assert_eq!(math(&aln), r"\begin{aligned}x&=1\\y=2\end{aligned}");
        // Outside an equation array an `&` is a literal, as before.
        assert_eq!(math(&run("", "a&amp;b")), r"a \& b");
    }

    #[test]
    fn text_mode_symbols_are_math_mode_commands() {
        // #1253: for these characters docling's `process_unicode` gives
        // pylatexenc's text-mode command in `\text{…}` (` \text{ \textrightarrow } `),
        // which KaTeX and math-core reject; the math-mode command renders
        // there and compiles with pdflatex.
        let r = |t: &str| math(&format!("<m:r><m:t>{t}</m:t></m:r>"));
        assert_eq!(r("x→0"), r"x \rightarrow 0");
        assert_eq!(r("f:R→R"), r"f:R \rightarrow R");
        assert_eq!(r("¬p"), r"\neg p");
        assert_eq!(r("a·b"), r"a \cdot b");
        assert_eq!(r("a÷b"), r"a \div b");
        // A typed space next to the padding keeps a double space, as for `∞<`.
        assert_eq!(r("5 µm"), r"5  \mu m");
        assert_eq!(r("x²"), "x ^{2}");
        // Adjacent superscript digits are one exponent, not a double superscript.
        assert_eq!(r("6×10²³"), r"6\times 10 ^{23}");
        assert_eq!(r("½"), r"\frac{1}{2}");
        assert_eq!(r("37 ℃"), r"37  {}^{\circ}\mathrm{C}");
        assert_eq!(r("1⁄2"), "1/2");
        // docling's own `℉` entry writes a caret, which its escaping turns
        // into the text accent `\^`.
        assert_eq!(r("98 ℉"), r"98  ^\circ F");
        // Word's limit: `lim` with `x→0` below it. docling's `do_lim` writes
        // `\rightarrow` as `\to`, which the text-mode command never reached.
        let lim = math(
            "<m:limLow><m:e><m:r><m:t>lim</m:t></m:r></m:e>\
             <m:lim><m:r><m:t>x→0</m:t></m:r></m:lim></m:limLow>",
        );
        assert_eq!(lim, r"\lim_{x \to 0}");
    }

    #[test]
    fn other_text_symbols_are_kept_as_typed() {
        // #1253: text symbols without a math-mode command stay as typed, which
        // math renderers read, instead of docling's `\text{ \textperthousand }`.
        let r = |t: &str| math(&format!("<m:r><m:t>{t}</m:t></m:r>"));
        assert_eq!(r("5‰"), "5‰");
        assert_eq!(r("№ 7"), "№ 7");
    }

    #[test]
    fn text_mode_entries_are_ones_katex_reads() {
        // #1253: a `\text{…}` entry left in the table names a text-mode command
        // KaTeX reads too; every other text-mode entry is a math-mode command
        // or the character as typed.
        const KATEX_TEXT: &[&str] = &[
            "\\textbackslash",
            "\\textasciitilde",
            "\\textsterling",
            "\\textcopyright",
            "\\textregistered",
            "\\textdegree",
            "\\textemdash",
            "\\textquoteleft",
            "\\textquoteright",
            "\\textquotedblleft",
            "\\textquotedblright",
            "\\textdagger",
            "\\textdaggerdbl",
            "\\textellipsis",
        ];
        for &(c, latex) in UNICODE_LATEX {
            if let Some(inner) = latex.trim().strip_prefix("\\text{") {
                let cmd = inner.trim_end_matches('}').trim();
                assert!(KATEX_TEXT.contains(&cmd), "{c:?} → {latex:?}");
            }
        }
    }
}
