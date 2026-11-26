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
    sep: Option<String>,
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
            "sepChr" => p.sep = val,
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

/// A delimiter: its elements (one `<m:e>` each) joined by `m:sepChr` — `|`
/// when absent (ECMA-376 Part 1, `sepChr`), nothing when empty — as docling's `do_d`
/// does: `(a|b)`, `{x | x > 0}`, `⟨u|v⟩`. The three characters go through
/// docling's `T` table ([`greek_or`]) before `escape_latex`.
fn do_d(e: XmlNode) -> String {
    let p = pr(e, "dPr");
    let sep = escape_latex(&greek_or(p.sep.as_deref(), "|"));
    let inner = elems(e)
        .filter(|c| local(*c) == "e")
        .map(process_children)
        .collect::<Vec<_>>()
        .join(&sep);
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
fn do_nary(e: XmlNode) -> String {
    let op = match pr(e, "naryPr").chr {
        None => "\\int".to_string(),
        Some(c) => big_op(&c).map_or(c, str::to_string),
    };
    let rest: String = elems(e)
        .filter(|c| local(*c) != "naryPr")
        .map(dispatch)
        .collect();
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

/// Per-character Unicode → LaTeX: the strings docling's `process_unicode`
/// returns (`_MATH_CHAR_MAP` first, else pylatexenc's `\ensuremath{…}`/`{…}`
/// unwrapped to a **two-space**-padded macro; the two-space padding is
/// load-bearing, see [`to_latex`]), generated into [`UNICODE_LATEX`].
/// Deliberate differences: docling replaces *every* `}` with a space when it
/// unwraps `\ensuremath{…}`, which leaves a macro with an argument unbalanced
/// (`ℝ` → `\mathbb{R`); the table keeps that macro's braces (`\mathbb{R}`).
/// `√` is `\surd`, and `{` / `}` are not in the table: [`do_r`] escapes them
/// with `escape_latex` alone. A character outside the table is kept as typed.
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
        assert_eq!(d(arrows), r"\left\uparrow a∣b\right\uparrow");
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
}
