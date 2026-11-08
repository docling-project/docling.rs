//! LaTeX → MathML for the HTML export — a port of
//! [`latex2mathml`](https://github.com/roniemartinez/latex2mathml) 3.81.1,
//! the library docling-core's `HTMLDocSerializer._process_formula` runs on
//! every formula item (`formula_to_mathml=True` is the default).
//!
//! The port is deliberately literal: the same four stages (`tokenizer` →
//! `walker` → `converter` → ElementTree serialisation), the same symbol
//! table ([`symbols`], generated from the package's `unimathsymbols.txt`),
//! the same attribute insertion order, and the same failure modes — every
//! Python exception the pipeline can raise (its twelve `latex2mathml`
//! exceptions plus the `StopIteration`/`IndexError`/`ValueError`s an
//! unbalanced input provokes) maps to an [`Error`], so upstream's
//! `except Exception` fallback (`<pre>{text}</pre>`) fires for exactly the
//! same inputs. Output text is written the way docling-core 2.106
//! (docling-core#894) leaves `tostring(...)`: `&<>` escaped in text and
//! attributes (plus `"` and CR/LF/TAB in attributes), with only the numeric
//! references latex2mathml stores as text (`&#x0003D;`) un-double-escaped.
//!
//! Only `convert_to_element` is reproduced (no `xmlns` override, no
//! `parent`); [`formula_to_mathml`] appends the escaped `<annotation
//! encoding="TeX">` source and the `<div>` block wrapper docling-core puts
//! around it.

mod symbols;

use std::collections::HashMap;

/// Every way the Python pipeline can fail; docling-core treats them all
/// alike (one `except Exception`), so callers only need `is_err()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Error {
    NumeratorNotFound,
    DenominatorNotFound,
    ExtraLeftOrMissingRight,
    MissingSuperScriptOrSubscript,
    DoubleSubscripts,
    DoubleSuperscripts,
    NoAvailableTokens,
    InvalidStyleForGenfrac,
    MissingEnd,
    InvalidAlignment,
    InvalidWidth,
    LimitsMustFollowMathOperator,
    /// A Python runtime error (`StopIteration` from `next()` on an exhausted
    /// token stream, `IndexError`, `ValueError`, `RecursionError`).
    Runtime(&'static str),
}

type Result<T> = std::result::Result<T, Error>;

/// docling-core's `_process_formula` success path (2.106, docling-core#894):
/// `latex2mathml`'s element serialised by `tostring` — text and attributes
/// escaped, with only the numeric character references latex2mathml stores
/// as literal text (`&#x003B1;`) reverted from their double escaping — and
/// an `<annotation encoding="TeX">` carrying the HTML-escaped source spliced
/// in before `</math>`; block formulas come wrapped in `<div>`. `Err` for
/// whatever `latex2mathml` raises on.
pub(crate) fn formula_to_mathml(latex: &str, inline: bool) -> Result<String> {
    let display = if inline { "inline" } else { "block" };
    let mut conv = Converter::new(display);
    let root = conv.convert_to_element(latex)?;
    let mathml = conv.tree.to_string(root);
    let mathml = mathml.strip_suffix("</math>").unwrap_or(&mathml);
    let mathml = format!(
        "{mathml}<annotation encoding=\"TeX\">{}</annotation></math>",
        escape_text(latex)
    );
    Ok(if inline {
        mathml
    } else {
        format!("<div>{mathml}</div>")
    })
}

/// `symbols_parser.convert_symbol`.
fn convert_symbol(name: &str) -> Option<&'static str> {
    symbols::SYMBOLS
        .binary_search_by(|(k, _)| (*k).cmp(name))
        .ok()
        .map(|i| symbols::SYMBOLS[i].1)
}

/// Python's `\d` on `str` (Unicode category `Nd`).
fn is_digit(c: char) -> bool {
    if c.is_ascii() {
        return c.is_ascii_digit();
    }
    let cp = c as u32;
    symbols::DECIMAL_DIGIT_RANGES
        .iter()
        .any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// `str.isdigit()` as the walker uses it (non-empty, all decimal digits).
fn py_isdigit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_digit)
}

/// `int(s)` for a string `py_isdigit` accepted (digits of any script).
fn py_int(s: &str) -> Result<i64> {
    let mut v: i64 = 0;
    for c in s.chars() {
        let d = if c.is_ascii_digit() {
            c as i64 - '0' as i64
        } else {
            // Every `Nd` block is a run of ten starting at the zero.
            let cp = c as u32;
            let lo = symbols::DECIMAL_DIGIT_RANGES
                .iter()
                .find(|&&(lo, hi)| cp >= lo && cp <= hi)
                .map(|&(lo, _)| lo)
                .ok_or(Error::Runtime("int()"))?;
            ((cp - lo) % 10) as i64
        };
        v = v
            .checked_mul(10)
            .and_then(|v| v.checked_add(d))
            .ok_or(Error::Runtime("int()"))?;
    }
    Ok(v)
}

/// `int(s)` where Python accepts a sign and surrounding whitespace too
/// (`_parse_optional_int`'s `[n]` argument).
fn py_int_lenient(s: &str) -> Result<i64> {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if !py_isdigit(digits) {
        return Err(Error::Runtime("int()"));
    }
    let v = py_int(digits)?;
    Ok(if neg { -v } else { v })
}

/// Python's `\s` / `str.isspace()`.
fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

// ---------------------------------------------------------------------------
// commands.py — the constants the walker and converter dispatch on.
// ---------------------------------------------------------------------------

const OPENING_BRACE: &str = "{";
const CLOSING_BRACE: &str = "}";
const BRACES: &str = "{}";
const OPENING_BRACKET: &str = "[";
const CLOSING_BRACKET: &str = "]";
const SUBSUP: &str = "_^";
const SUBSCRIPT: &str = "_";
const SUPERSCRIPT: &str = "^";
const APOSTROPHE: &str = "'";
const PRIME: &str = r"\prime";
const DPRIME: &str = r"\dprime";
const TRPRIME: &str = r"\trprime";
const QPRIME: &str = r"\qprime";
const LEFT: &str = r"\left";
const MIDDLE: &str = r"\middle";
const RIGHT: &str = r"\right";
const ABOVE: &str = r"\above";
const ABOVEWITHDELIMS: &str = r"\abovewithdelims";
const ATOP: &str = r"\atop";
const ATOPWITHDELIMS: &str = r"\atopwithdelims";
const BINOM: &str = r"\binom";
const BRACE: &str = r"\brace";
const BRACK: &str = r"\brack";
const CFRAC: &str = r"\cfrac";
const CHOOSE: &str = r"\choose";
const DBINOM: &str = r"\dbinom";
const DFRAC: &str = r"\dfrac";
const FRAC: &str = r"\frac";
const GENFRAC: &str = r"\genfrac";
const OVER: &str = r"\over";
const TBINOM: &str = r"\tbinom";
const TFRAC: &str = r"\tfrac";
const ROOT: &str = r"\root";
const SQRT: &str = r"\sqrt";
const OVERSET: &str = r"\overset";
const STACKREL: &str = r"\stackrel";
const UNDERSET: &str = r"\underset";
const MATHRING: &str = r"\mathring";
const OVERBRACE: &str = r"\overbrace";
const UNDERBRACE: &str = r"\underbrace";
const HBOX: &str = r"\hbox";
const MBOX: &str = r"\mbox";
const BEGIN: &str = r"\begin";
const END: &str = r"\end";
const LIMITS: &str = r"\limits";
const NOLIMITS: &str = r"\nolimits";
const SUMMATION: &str = r"\sum";
const PRODUCT: &str = r"\prod";
const LIMIT: [&str; 5] = [r"\lim", r"\sup", r"\inf", r"\max", r"\min"];
const NEWCOMMAND: &str = r"\newcommand";
const NEWENVIRONMENT: &str = r"\newenvironment";
const DEF: &str = r"\def";
const DECLAREMATHOPERATOR: &str = r"\DeclareMathOperator";
const OPERATORNAME: &str = r"\operatorname";
const OPERATORNAMESTAR: &str = r"\operatorname*";
const OPERATORNAMEWITHLIMITS: &str = r"\operatornamewithlimits";
const LBRACE: &str = r"\{";
const FUNCTIONS: [&str; 31] = [
    r"\arccos", r"\arcctg", r"\arcsin", r"\arctan", r"\arctg", r"\ch", r"\cos", r"\cosh",
    r"\cosec", r"\cot", r"\cotg", r"\coth", r"\csc", r"\ctg", r"\cth", r"\deg", r"\dim", r"\exp",
    r"\hom", r"\ker", r"\ln", r"\lg", r"\log", r"\sec", r"\sh", r"\sin", r"\sinh", r"\tan",
    r"\tanh", r"\tg", r"\th",
];
const GCD: &str = r"\gcd";
const MOD: &str = r"\mod";
const PMOD: &str = r"\pmod";
const POD: &str = r"\pod";
const BMOD: &str = r"\bmod";
const HDASHLINE: &str = r"\hdashline";
const HLINE: &str = r"\hline";
const HFIL: &str = r"\hfil";
const NONUMBER: &str = r"\nonumber";
const NOTAG: &str = r"\notag";
const CASES: &str = r"\cases";
const EQALIGN: &str = r"\eqalign";
const EQALIGNNO: &str = r"\eqalignno";
const DISPLAYLINES: &str = r"\displaylines";
const SMALLMATRIX: &str = r"\smallmatrix";
const SUBSTACK: &str = r"\substack";
const SPLIT: &str = r"\split";
const ALIGN: &str = r"\align";
const ALIGNSTAR: &str = r"\align*";
const MATRICES: [&str; 22] = [
    r"\matrix",
    r"\matrix*",
    r"\pmatrix",
    r"\pmatrix*",
    r"\bmatrix",
    r"\bmatrix*",
    r"\Bmatrix",
    r"\Bmatrix*",
    r"\vmatrix",
    r"\vmatrix*",
    r"\Vmatrix",
    r"\Vmatrix*",
    r"\array",
    SUBSTACK,
    CASES,
    DISPLAYLINES,
    EQALIGN,
    EQALIGNNO,
    SMALLMATRIX,
    SPLIT,
    ALIGN,
    ALIGNSTAR,
];
const CARRIAGERETURN: &str = r"\cr";
const DOUBLEBACKSLASH: &str = r"\\";
const HSKIP: &str = r"\hskip";
const HSPACE: &str = r"\hspace";
const KERN: &str = r"\kern";
const MKERN: &str = r"\mkern";
const MSKIP: &str = r"\mskip";
const MSPACE: &str = r"\mspace";
const NOBREAKSPACE: &str = r"\nobreakspace";
const SPACE: &str = r"\space";
const MATH: &str = r"\math";
const MATHCHOICE: &str = r"\mathchoice";
const BRA: &str = r"\bra";
const BRAKET: &str = r"\braket";
const CLASS: &str = r"\class";
const CLAP: &str = r"\clap";
const LLAP: &str = r"\llap";
const FBOX: &str = r"\fbox";
const KET: &str = r"\ket";
const RLAP: &str = r"\rlap";
const COLOR: &str = r"\color";
const COLORBOX: &str = r"\colorbox";
const FCOLORBOX: &str = r"\fcolorbox";
const TEXTCOLOR: &str = r"\textcolor";
const DISPLAYSTYLE: &str = r"\displaystyle";
const TEXTSTYLE: &str = r"\textstyle";
const SCRIPTSTYLE: &str = r"\scriptstyle";
const SCRIPTSCRIPTSTYLE: &str = r"\scriptscriptstyle";
const STYLE: &str = r"\style";
const HPHANTOM: &str = r"\hphantom";
const MATHSTRUT: &str = r"\mathstrut";
const STRUT: &str = r"\strut";
const VPHANTOM: &str = r"\vphantom";
const MATH_NON_FONT_COMMANDS: [&str; 11] = [
    MATHRING,
    r"\mathbin",
    MATHCHOICE,
    r"\mathclose",
    r"\mathinner",
    r"\mathop",
    r"\mathopen",
    r"\mathord",
    r"\mathpunct",
    r"\mathrel",
    MATHSTRUT,
];
const IDOTSINT: &str = r"\idotsint";
const LATEX: &str = r"\LaTeX";
const TEX: &str = r"\TeX";
const LEFTROOT: &str = r"\leftroot";
const LOWER: &str = r"\lower";
const MOVELEFT: &str = r"\moveleft";
const MOVERIGHT: &str = r"\moveright";
const RAISE: &str = r"\raise";
const RULE: &str = r"\rule";
const SMASH: &str = r"\smash";
const SIDESET: &str = r"\sideset";
const SKEW: &str = r"\skew";
const TAG: &str = r"\tag";
const TAGSTAR: &str = r"\tag*";
const UNICODE: &str = r"\unicode";
const UPROOT: &str = r"\uproot";
const VERB: &str = r"\verb";
const NOT: &str = r"\not";
const HREF: &str = r"\href";
const MULTIPRIMES: &str = "multiprimes";
const MAX_MACRO_DEPTH: usize = 100;
/// Python's default recursion limit is what stops a runaway `{{{{…` there;
/// the walker and the converter each recurse once per nesting level. The
/// frames are large in a debug build, so the HTML serializer calls in here
/// from its own 256 MB thread; a release build fits this depth in 2 MB.
const MAX_NESTING: usize = 400;

/// `commands.EXTENSIBLE_ARROWS`.
fn extensible_arrow(token: &str) -> Option<&'static str> {
    Some(match token {
        r"\xleftarrow" => "&#x2190;",
        r"\xleftharpoondown" => "&#x21BD;",
        r"\xleftharpoonup" => "&#x21BC;",
        r"\xleftrightarrow" => "&#x2194;",
        r"\xleftrightharpoons" => "&#x21CB;",
        r"\xlongequal" => "&#x003D;",
        r"\xmapsto" => "&#x21A6;",
        r"\xrightarrow" => "&#x2192;",
        r"\xrightharpoondown" => "&#x21C1;",
        r"\xrightharpoonup" => "&#x21C0;",
        r"\xrightleftharpoons" => "&#x21CC;",
        r"\xtofrom" => "&#x21C4;",
        r"\xtwoheadleftarrow" => "&#x219E;",
        r"\xtwoheadrightarrow" => "&#x21A0;",
        r"\xhookleftarrow" => "&#x21A9;",
        r"\xhookrightarrow" => "&#x21AA;",
        r"\xLeftarrow" => "&#x21D0;",
        r"\xRightarrow" => "&#x21D2;",
        r"\xLeftrightarrow" => "&#x21D4;",
        _ => return None,
    })
}

/// `commands.COMMANDS_WITH_ONE_PARAMETER`.
fn has_one_parameter(token: &str) -> bool {
    matches!(
        token,
        r"\acute"
            | r"\bar"
            | r"\bcancel"
            | r"\Bbb"
            | r"\bm"
            | r"\bold"
            | r"\bra"
            | r"\braket"
            | r"\boldsymbol"
            | r"\boxed"
            | r"\cancel"
            | r"\breve"
            | r"\check"
            | r"\dot"
            | r"\ddot"
            | r"\dddot"
            | r"\ddddot"
            | r"\grave"
            | r"\hat"
            | r"\hphantom"
            | r"\ket"
            | r"\mathbin"
            | r"\mathclap"
            | r"\mathclose"
            | r"\mathinner"
            | r"\mathllap"
            | r"\mathop"
            | r"\mathopen"
            | r"\mathord"
            | r"\mathpunct"
            | r"\mathrel"
            | r"\mathrlap"
            | r"\mathring"
            | r"\mit"
            | r"\mod"
            | r"\oldstyle"
            | r"\overbrace"
            | r"\overbracket"
            | r"\overleftarrow"
            | r"\overleftrightarrow"
            | r"\overline"
            | r"\overparen"
            | r"\overrightarrow"
            | r"\phantom"
            | r"\pmb"
            | r"\unicode"
            | r"\pmod"
            | r"\pod"
            | r"\scr"
            | r"\shoveleft"
            | r"\shoveright"
            | r"\sout"
            | r"\tilde"
            | r"\tt"
            | r"\underbar"
            | r"\underbrace"
            | r"\underbracket"
            | r"\underleftarrow"
            | r"\underline"
            | r"\underparen"
            | r"\underrightarrow"
            | r"\underleftrightarrow"
            | r"\vec"
            | r"\vcenter"
            | r"\vphantom"
            | r"\widecheck"
            | r"\widehat"
            | r"\widetilde"
            | r"\xcancel"
    )
}

/// `commands.COMMANDS_WITH_TWO_PARAMETERS`.
fn has_two_parameters(token: &str) -> bool {
    matches!(
        token,
        r"\binom"
            | r"\cfrac"
            | r"\dbinom"
            | r"\dfrac"
            | r"\frac"
            | r"\overset"
            | r"\stackrel"
            | r"\tbinom"
            | r"\tfrac"
            | r"\underset"
    )
}

/// `commands.BIG` — `(minsize/maxsize)`.
fn big_size(token: &str) -> Option<&'static str> {
    Some(match token {
        r"\Bigg" => "2.470em",
        r"\bigg" => "2.047em",
        r"\Big" => "1.623em",
        r"\big" => "1.2em",
        _ => return None,
    })
}

/// `commands.BIG_OPEN_CLOSE` — `\bigl`, `\bigm`, `\bigr` and friends.
fn big_open_close(token: &str) -> Option<&'static str> {
    let stem = token
        .strip_suffix('l')
        .or_else(|| token.strip_suffix('m'))
        .or_else(|| token.strip_suffix('r'))?;
    big_size(stem)
}

/// `commands.MSTYLE_SIZES` — the `mathsize`.
fn mstyle_size(token: &str) -> Option<&'static str> {
    Some(match token {
        r"\Huge" => "2.49em",
        r"\huge" => "2.07em",
        r"\LARGE" => "1.73em",
        r"\Large" => "1.44em",
        r"\large" => "1.2em",
        r"\footnotesize" => "0.85em",
        r"\normalsize" => "1em",
        r"\scriptsize" => "0.7em",
        r"\small" => "0.85em",
        r"\tiny" => "0.5em",
        r"\Tiny" => "0.6em",
        _ => return None,
    })
}

/// `commands.STYLES` — `(displaystyle, scriptlevel)`.
fn style_attrs(token: &str) -> Option<(&'static str, &'static str)> {
    Some(match token {
        DISPLAYSTYLE => ("true", "0"),
        TEXTSTYLE => ("false", "0"),
        SCRIPTSTYLE => ("false", "1"),
        SCRIPTSCRIPTSTYLE => ("false", "2"),
        _ => return None,
    })
}

type Attrs = Vec<(String, String)>;

fn attrs(pairs: &[(&str, &str)]) -> Attrs {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `dict.__setitem__` / `dict.update` on an insertion-ordered dict: an
/// existing key keeps its position.
fn attr_set(a: &mut Attrs, key: &str, value: &str) {
    if let Some(slot) = a.iter_mut().find(|(k, _)| k == key) {
        slot.1 = value.to_string();
    } else {
        a.push((key.to_string(), value.to_string()));
    }
}

fn attr_get<'a>(a: &'a Attrs, key: &str) -> Option<&'a str> {
    a.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `commands.CONVERSION_MAP`: `(mathml tag, attributes)`; `None` for a
/// token the map does not know.
fn conversion_map(token: &str) -> Option<(&'static str, Attrs)> {
    if let Some(size) = big_size(token) {
        return Some(("mo", attrs(&[("minsize", size), ("maxsize", size)])));
    }
    if let Some(size) = big_open_close(token) {
        return Some((
            "mo",
            attrs(&[
                ("stretchy", "true"),
                ("fence", "true"),
                ("minsize", size),
                ("maxsize", size),
            ]),
        ));
    }
    if let Some(size) = mstyle_size(token) {
        return Some(("mstyle", attrs(&[("mathsize", size)])));
    }
    if let Some((d, s)) = style_attrs(token) {
        return Some(("mstyle", attrs(&[("displaystyle", d), ("scriptlevel", s)])));
    }
    if extensible_arrow(token).is_some() {
        return Some(("mover", Vec::new()));
    }
    if LIMIT.contains(&token) {
        return Some(("mo", Vec::new()));
    }
    let empty = |tag| Some((tag, Vec::new()));
    match token {
        DISPLAYLINES => Some((
            "mtable",
            attrs(&[
                ("rowspacing", "0.5em"),
                ("columnspacing", "1em"),
                ("displaystyle", "true"),
            ]),
        )),
        EQALIGN | EQALIGNNO => Some((
            "mtable",
            attrs(&[("displaystyle", "true"), ("columnspacing", "0em")]),
        )),
        SMALLMATRIX => Some((
            "mtable",
            attrs(&[("rowspacing", "0.1em"), ("columnspacing", "0.2778em")]),
        )),
        SPLIT => Some((
            "mtable",
            attrs(&[
                ("displaystyle", "true"),
                ("columnspacing", "0em"),
                ("rowspacing", "3pt"),
            ]),
        )),
        ALIGN | ALIGNSTAR => Some((
            "mtable",
            attrs(&[("displaystyle", "true"), ("rowspacing", "3pt")]),
        )),
        t if MATRICES.contains(&t) => empty("mtable"),
        SUBSCRIPT => empty("msub"),
        SUPERSCRIPT => empty("msup"),
        SUBSUP => empty("msubsup"),
        BINOM | DBINOM | TBINOM => Some(("mfrac", attrs(&[("linethickness", "0")]))),
        CFRAC | DFRAC | FRAC | GENFRAC | TFRAC => empty("mfrac"),
        r"\acute"
        | r"\bar"
        | r"\breve"
        | r"\check"
        | r"\dot"
        | r"\ddot"
        | r"\dddot"
        | r"\ddddot"
        | r"\grave"
        | MATHRING
        | OVERBRACE
        | r"\overbracket"
        | r"\overleftarrow"
        | r"\overleftrightarrow"
        | r"\overline"
        | r"\overparen"
        | r"\overrightarrow"
        | r"\tilde"
        | OVERSET
        | STACKREL
        | r"\vec"
        | r"\widecheck"
        | r"\widehat"
        | r"\widetilde" => empty("mover"),
        r"\hat" => Some(("mover", attrs(&[("accent", "true")]))),
        LIMITS => empty("munderover"),
        r"\underbar"
        | UNDERBRACE
        | r"\underbracket"
        | r"\underleftarrow"
        | r"\underline"
        | r"\underparen"
        | r"\underrightarrow"
        | r"\underleftrightarrow"
        | UNDERSET => empty("munder"),
        r"\:" | r"\>" => Some(("mspace", attrs(&[("width", "0.222em")]))),
        r"\," => Some(("mspace", attrs(&[("width", "0.167em")]))),
        DOUBLEBACKSLASH => Some(("mspace", attrs(&[("linebreak", "newline")]))),
        r"\enspace" | r"\Space" => Some(("mspace", attrs(&[("width", "0.5em")]))),
        r"\!" | r"\negthinspace" => Some(("mspace", attrs(&[("width", "negativethinmathspace")]))),
        HSKIP | HSPACE | KERN | MKERN | MSKIP | MSPACE => empty("mspace"),
        r"\negmedspace" => Some(("mspace", attrs(&[("width", "negativemediummathspace")]))),
        r"\negthickspace" => Some(("mspace", attrs(&[("width", "negativethickmathspace")]))),
        r"\thickspace" => Some(("mspace", attrs(&[("width", "thickmathspace")]))),
        r"\thinspace" => Some(("mspace", attrs(&[("width", "thinmathspace")]))),
        r"\qquad" => Some(("mspace", attrs(&[("width", "2em")]))),
        r"\quad" => Some(("mspace", attrs(&[("width", "1em")]))),
        r"\;" => Some(("mspace", attrs(&[("width", "0.278em")]))),
        CLAP | r"\mathclap" => Some((
            "mpadded",
            attrs(&[("lspace", "-0.5width"), ("width", "0px")]),
        )),
        LLAP | r"\mathllap" => Some(("mpadded", attrs(&[("lspace", "-1width"), ("width", "0px")]))),
        r"\mathrlap" | RLAP => Some(("mpadded", attrs(&[("width", "0px")]))),
        r"\shoveleft" => Some(("mpadded", attrs(&[("lspace", "0")]))),
        r"\shoveright" => Some(("mpadded", attrs(&[("lspace", "0"), ("width", "0")]))),
        r"\bcancel" => Some(("menclose", attrs(&[("notation", "downdiagonalstrike")]))),
        r"\boxed" | FBOX => Some(("menclose", attrs(&[("notation", "box")]))),
        r"\cancel" => Some(("menclose", attrs(&[("notation", "updiagonalstrike")]))),
        r"\sout" => Some(("menclose", attrs(&[("notation", "horizontalstrike")]))),
        r"\xcancel" => Some((
            "menclose",
            attrs(&[("notation", "updiagonalstrike downdiagonalstrike")]),
        )),
        LEFT => Some((
            "mo",
            attrs(&[("stretchy", "true"), ("fence", "true"), ("form", "prefix")]),
        )),
        MIDDLE => Some((
            "mo",
            attrs(&[
                ("stretchy", "true"),
                ("fence", "true"),
                ("lspace", "0.05em"),
                ("rspace", "0.05em"),
            ]),
        )),
        RIGHT => Some((
            "mo",
            attrs(&[("stretchy", "true"), ("fence", "true"), ("form", "postfix")]),
        )),
        COLOR => empty("mstyle"),
        COLORBOX | FCOLORBOX | SMASH => empty("mpadded"),
        SQRT => empty("msqrt"),
        ROOT => empty("mroot"),
        r"\emph" | r"\textit" => Some(("mtext", attrs(&[("mathvariant", "italic")]))),
        HREF => empty("mrow"),
        r"\text" | r"\textmd" | r"\textnormal" | r"\textrm" | TAG | TAGSTAR | r"\textup" | HBOX
        | MBOX => empty("mtext"),
        r"\textbf" => Some(("mtext", attrs(&[("mathvariant", "bold")]))),
        r"\textsf" => Some(("mtext", attrs(&[("mathvariant", "sans-serif")]))),
        r"\texttt" | VERB => Some(("mtext", attrs(&[("mathvariant", "monospace")]))),
        HPHANTOM | r"\phantom" | VPHANTOM => empty("mphantom"),
        LOWER | r"\mathinner" | MOVELEFT | MOVERIGHT | RAISE | r"\vcenter" => empty("mpadded"),
        r"\mathbin" => Some(("mo", attrs(&[("lspace", "0.22em"), ("rspace", "0.22em")]))),
        r"\mathclose" | r"\mathopen" => Some((
            "mo",
            attrs(&[("stretchy", "false"), ("lspace", "0em"), ("rspace", "0em")]),
        )),
        r"\mathop" | r"\mathrel" | BMOD => empty("mo"),
        r"\mathord" | MOD | PMOD | POD => empty("mi"),
        r"\mathpunct" => Some((
            "mo",
            attrs(&[
                ("separator", "true"),
                ("lspace", "0em"),
                ("rspace", "0.17em"),
            ]),
        )),
        BRA | BRAKET | KET | SIDESET | SKEW => empty("mrow"),
        _ => None,
    }
}

/// `commands.DIACRITICS`: `(text, attributes)` of the `<mo>` appended
/// after the base.
fn diacritic(token: &str) -> Option<(&'static str, Attrs)> {
    let plain = |t| Some((t, Vec::new()));
    let stretchy = |t| Some((t, attrs(&[("stretchy", "true")])));
    match token {
        r"\acute" => plain("&#x000B4;"),
        r"\bar" => stretchy("&#x000AF;"),
        r"\breve" => plain("&#x002D8;"),
        r"\check" => plain("&#x002C7;"),
        r"\dot" => plain("&#x002D9;"),
        r"\ddot" => plain("&#x000A8;"),
        r"\dddot" => plain("&#x020DB;"),
        r"\ddddot" => plain("&#x020DC;"),
        r"\grave" => plain("&#x00060;"),
        r"\hat" => plain("&#x0005E;"),
        MATHRING => plain("&#x002DA;"),
        OVERBRACE => plain("&#x23DE;"),
        r"\overbracket" => stretchy("&#x23B4;"),
        r"\overleftarrow" => plain("&#x02190;"),
        r"\overleftrightarrow" => plain("&#x02194;"),
        r"\overline" => Some(("&#x02015;", attrs(&[("accent", "true")]))),
        r"\overparen" => plain("&#x023DC;"),
        r"\overrightarrow" => plain("&#x02192;"),
        r"\tilde" => Some(("&#x0007E;", attrs(&[("stretchy", "false")]))),
        r"\underbar" => Some((
            "&#x02015;",
            attrs(&[("stretchy", "true"), ("accent", "true")]),
        )),
        UNDERBRACE => plain("&#x23DF;"),
        r"\underbracket" => stretchy("&#x23B5;"),
        r"\underleftarrow" => plain("&#x02190;"),
        r"\underleftrightarrow" => plain("&#x02194;"),
        r"\underline" => Some(("&#x02015;", attrs(&[("accent", "true")]))),
        r"\underparen" => plain("&#x023DD;"),
        r"\underrightarrow" => plain("&#x02192;"),
        r"\vec" => stretchy("&#x02192;"),
        r"\widecheck" => stretchy("&#x002C7;"),
        r"\widehat" => plain("&#x0005E;"),
        r"\widetilde" => plain("&#x0007E;"),
        _ => None,
    }
}

/// One of `commands.LOCAL_FONTS` / `GLOBAL_FONTS`: the `mathvariant` per
/// element kind (`font_factory(default, replacements)` resolved).
#[derive(Clone, Copy, Debug)]
struct Font {
    mi: Option<&'static str>,
    mo: Option<&'static str>,
    mn: Option<&'static str>,
    mtext: Option<&'static str>,
    fence: Option<&'static str>,
}

impl Font {
    const fn all(v: Option<&'static str>) -> Font {
        Font {
            mi: v,
            mo: v,
            mn: v,
            mtext: v,
            fence: v,
        }
    }
    /// `font_factory(default, {"fence": None})`.
    const fn no_fence(v: &'static str) -> Font {
        Font {
            fence: None,
            ..Font::all(Some(v))
        }
    }
    /// `font_factory(None, {"mi": v})`.
    const fn mi_only(v: &'static str) -> Font {
        Font {
            mi: Some(v),
            ..Font::all(None)
        }
    }
    fn get(&self, key: &str) -> Option<&'static str> {
        match key {
            "mi" => self.mi,
            "mo" => self.mo,
            "mn" => self.mn,
            "mtext" => self.mtext,
            "fence" => self.fence,
            _ => None,
        }
    }
}

fn local_font(token: &str) -> Option<Font> {
    Some(match token {
        r"\Bbb" | r"\mathbb" => Font::no_fence("double-struck"),
        r"\bm" => Font::no_fence("bold-italic"),
        r"\bold" | r"\mathbf" | r"\pmb" => Font::no_fence("bold"),
        r"\boldsymbol" => Font {
            mi: Some("bold-italic"),
            mtext: None,
            ..Font::all(Some("bold"))
        },
        r"\mathcal" | r"\mathscr" | r"\scr" => Font::no_fence("script"),
        r"\mathfrak" => Font::no_fence("fraktur"),
        r"\mathit" => Font::no_fence("italic"),
        r"\mathrm" | r"\mathnormal" => Font::mi_only("normal"),
        r"\mathsf" => Font::mi_only("sans-serif"),
        r"\mathsfit" => Font::mi_only("sans-serif-italic"),
        r"\mathtt" | r"\tt" => Font::no_fence("monospace"),
        r"\mit" => Font {
            fence: None,
            mi: None,
            ..Font::all(Some("italic"))
        },
        r"\oldstyle" => Font::no_fence("normal"),
        _ => return None,
    })
}

fn global_font(token: &str) -> Option<Font> {
    Some(match token {
        r"\rm" => Font::mi_only("normal"),
        r"\bf" => Font::mi_only("bold"),
        r"\it" => Font::mi_only("italic"),
        r"\sf" => Font::mi_only("sans-serif"),
        r"\tt" => Font::mi_only("monospace"),
        r"\cal" => Font::no_fence("script"),
        r"\frak" => Font::no_fence("fraktur"),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// tokenizer.py
// ---------------------------------------------------------------------------

const UNITS: [&str; 12] = [
    "in", "mm", "cm", "pt", "em", "ex", "pc", "bp", "dd", "cc", "sp", "mu",
];

/// The text-taking commands of the tokenizer's `text_cmd` rule.
const TEXT_CMDS: [&str; 24] = [
    "clap",
    "class",
    "color",
    "emph",
    "fbox",
    "hbox",
    "href",
    "llap",
    "mbox",
    "rlap",
    "style",
    "tag",
    "tag*",
    "text",
    "textbf",
    "textcolor",
    "textit",
    "textmd",
    "textnormal",
    "textrm",
    "textsf",
    "texttt",
    "textup",
    "underbar",
];

/// The `\math…` prefixes the `math_font` rule's negative lookahead rejects.
const MATH_FONT_EXCLUDED: [&str; 10] = [
    "ring", "bin", "close", "inner", "op", "open", "ord", "punct", "rel", "strut",
];

/// One `PATTERN.finditer` match: the non-`None` groups, in order.
enum Match {
    /// `comment`, or a bare `%` the `char` rule caught — dropped either way.
    Comment,
    /// Every other rule: the captured groups (`subsup` yields two, the text
    /// commands two, `frac_cmd` two or three, `math_font` four).
    Groups(Vec<String>),
    /// `\verb<d>…<d>`: the content only.
    Verb(String),
}

fn s(cs: &[char]) -> String {
    cs.iter().collect()
}

fn starts_with(cs: &[char], i: usize, lit: &str) -> bool {
    (i..).zip(lit.chars()).all(|(j, c)| cs.get(j) == Some(&c))
}

fn skip(cs: &[char], mut i: usize, pred: impl Fn(char) -> bool) -> usize {
    while i < cs.len() && pred(cs[i]) {
        i += 1;
    }
    i
}

/// `\d+(?:\.\d+)?` at `i`; the end index, or `None`.
fn match_number(cs: &[char], i: usize) -> Option<usize> {
    let mut j = skip(cs, i, is_digit);
    if j == i {
        return None;
    }
    if cs.get(j) == Some(&'.') {
        let k = skip(cs, j + 1, is_digit);
        if k > j + 1 {
            j = k;
        }
    }
    Some(j)
}

/// `PATTERN` at `i` — the alternatives in the regex's order, the first
/// one that matches wins. `None` when nothing matches (whitespace).
fn match_at(cs: &[char], i: usize) -> Option<(Match, usize)> {
    let c = cs[i];
    // comment: %[^\n]+
    if c == '%' && cs.get(i + 1).is_some_and(|&n| n != '\n') {
        return Some((Match::Comment, skip(cs, i + 1, |c| c != '\n')));
    }
    // letter
    if c.is_ascii_alphabetic() {
        return Some((Match::Groups(vec![c.to_string()]), i + 1));
    }
    // subsup_operator + subsup_digit
    if (c == '_' || c == '^') && cs.get(i + 1).is_some_and(|&d| is_digit(d)) {
        return Some((
            Match::Groups(vec![c.to_string(), cs[i + 1].to_string()]),
            i + 2,
        ));
    }
    // dimension: -?\d+(?:\.\d+)?\s*(unit)
    if c == '-' || is_digit(c) {
        let start = if c == '-' { i + 1 } else { i };
        if let Some(end) = match_number(cs, start) {
            let j = skip(cs, end, is_space);
            if let Some(u) = UNITS.iter().find(|u| starts_with(cs, j, u)) {
                return Some((Match::Groups(vec![s(&cs[i..j + u.len()])]), j + u.len()));
            }
        }
    }
    // number
    if let Some(end) = match_number(cs, i) {
        return Some((Match::Groups(vec![s(&cs[i..end])]), end));
    }
    // dot_decimal: \.\d*
    if c == '.' {
        let end = skip(cs, i + 1, is_digit);
        return Some((Match::Groups(vec![s(&cs[i..end])]), end));
    }
    if c == '\\' {
        // escaped: \\[\\\[\]{}\s!,:>;|_%#$&]
        if let Some(&n) = cs.get(i + 1) {
            if "\\[]{}!,:>;|_%#$&".contains(n) || is_space(n) {
                return Some((Match::Groups(vec![s(&cs[i..i + 2])]), i + 2));
            }
        }
        // begin_end: \\(?:begin|end)\s*{[a-zA-Z]+\*?}
        for kw in ["begin", "end"] {
            if starts_with(cs, i + 1, kw) {
                let j = skip(cs, i + 1 + kw.len(), is_space);
                if cs.get(j) == Some(&'{') {
                    let mut k = skip(cs, j + 1, |c| c.is_ascii_alphabetic());
                    if k > j + 1 {
                        if cs.get(k) == Some(&'*') {
                            k += 1;
                        }
                        if cs.get(k) == Some(&'}') {
                            return Some((Match::Groups(vec![s(&cs[i..k + 1])]), k + 1));
                        }
                    }
                }
            }
        }
        // operatorname: \\operatorname(?:withlimits|\*)?\s*{[a-zA-Z\s*]+\*?\s*}
        if starts_with(cs, i + 1, "operatorname") {
            let mut j = i + 1 + "operatorname".len();
            if starts_with(cs, j, "withlimits") {
                j += "withlimits".len();
            } else if cs.get(j) == Some(&'*') {
                j += 1;
            }
            let j = skip(cs, j, is_space);
            if cs.get(j) == Some(&'{') {
                let k = skip(cs, j + 1, |c| {
                    c.is_ascii_alphabetic() || is_space(c) || c == '*'
                });
                if k > j + 1 && cs.get(k) == Some(&'}') {
                    return Some((Match::Groups(vec![s(&cs[i..k + 1])]), k + 1));
                }
            }
        }
        // text_cmd: \\(name)\s*{(?P<text_content>[^}]*)}
        {
            let mut j = skip(cs, i + 1, |c| c.is_ascii_alphabetic());
            if j > i + 1 {
                if cs.get(j) == Some(&'*') && s(&cs[i + 1..j]) == "tag" {
                    j += 1;
                }
                let name = s(&cs[i + 1..j]);
                if TEXT_CMDS.contains(&name.as_str()) {
                    let k = skip(cs, j, is_space);
                    if cs.get(k) == Some(&'{') {
                        let e = skip(cs, k + 1, |c| c != '}');
                        if cs.get(e) == Some(&'}') {
                            return Some((
                                Match::Groups(vec![format!("\\{name}"), s(&cs[k + 1..e])]),
                                e + 1,
                            ));
                        }
                    }
                }
            }
        }
        // frac_cmd: \\[cdt]?frac\s*([.\d])\s*([.\d])?
        {
            let mut j = i + 1;
            if cs.get(j).is_some_and(|c| matches!(c, 'c' | 'd' | 't')) {
                j += 1;
            }
            if starts_with(cs, j, "frac") {
                let cmd = s(&cs[i..j + 4]);
                let k = skip(cs, j + 4, is_space);
                if let Some(&a1) = cs.get(k).filter(|&&c| c == '.' || is_digit(c)) {
                    let mut groups = vec![cmd, a1.to_string()];
                    let mut end = skip(cs, k + 1, is_space);
                    if let Some(&a2) = cs.get(end).filter(|&&c| c == '.' || is_digit(c)) {
                        groups.push(a2.to_string());
                        end += 1;
                    }
                    return Some((Match::Groups(groups), end));
                }
            }
        }
        // math_font: \\math(?!ring|bin|…)[a-z]+{[a-zA-Z]}
        if starts_with(cs, i + 1, "math") {
            let j = i + 5;
            if !MATH_FONT_EXCLUDED.iter().any(|x| starts_with(cs, j, x)) {
                let k = skip(cs, j, |c| c.is_ascii_lowercase());
                if k > j
                    && cs.get(k) == Some(&'{')
                    && cs.get(k + 1).is_some_and(|c| c.is_ascii_alphabetic())
                    && cs.get(k + 2) == Some(&'}')
                {
                    return Some((
                        Match::Groups(vec![
                            s(&cs[i..k]),
                            "{".to_string(),
                            cs[k + 1].to_string(),
                            "}".to_string(),
                        ]),
                        k + 3,
                    ));
                }
            }
        }
        // verb: \\verb(?P<d>.)(?P<content>.*?)(?P=d)
        if starts_with(cs, i + 1, "verb") {
            if let Some(&d) = cs.get(i + 5).filter(|&&c| c != '\n') {
                let mut k = i + 6;
                while k < cs.len() && cs[k] != '\n' {
                    if cs[k] == d {
                        return Some((Match::Verb(s(&cs[i + 6..k])), k + 1));
                    }
                    k += 1;
                }
            }
        }
        // command: \\[a-zA-Z]+
        let j = skip(cs, i + 1, |c| c.is_ascii_alphabetic());
        if j > i + 1 {
            return Some((Match::Groups(vec![s(&cs[i..j])]), j));
        }
    }
    // char: \S
    if !is_space(c) {
        return Some((Match::Groups(vec![c.to_string()]), i + 1));
    }
    None
}

/// `tokenizer.tokenize` (with `skip_comments=True`), collected eagerly —
/// the walker always drains the stream, so laziness is unobservable.
fn tokenize(latex: &str) -> Result<Vec<String>> {
    let cs: Vec<char> = latex.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let Some((m, end)) = match_at(&cs, i) else {
            i += 1;
            continue;
        };
        i = end;
        let groups = match m {
            Match::Comment => continue,
            Match::Verb(content) => {
                out.push(VERB.to_string());
                out.push(content);
                continue;
            }
            Match::Groups(g) => g,
        };
        let first = &groups[0];
        if first.starts_with(VERB) {
            // `tokens[2]` of a bare `\verb…` command: IndexError upstream.
            return Err(Error::Runtime("IndexError"));
        }
        if first.starts_with(MATH) && !MATH_NON_FONT_COMMANDS.contains(&first.as_str()) {
            let full: String = groups.concat();
            if let Some(sym) = convert_symbol(&full) {
                out.push(format!("&#x{sym};"));
                continue;
            }
        }
        for captured in groups {
            if captured.starts_with('%') {
                break;
            }
            if UNITS.iter().any(|u| captured.ends_with(u))
                && captured.chars().next().is_some_and(is_digit)
            {
                out.push(captured.replace(' ', ""));
                continue;
            }
            if captured.starts_with(BEGIN)
                || captured.starts_with(END)
                || captured.starts_with(OPERATORNAME)
            {
                out.push(captured.replace(' ', ""));
                continue;
            }
            out.push(captured);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// walker.py
// ---------------------------------------------------------------------------

/// `walker.Node`.
#[derive(Clone, Debug, Default)]
struct Node {
    token: String,
    children: Option<Vec<Node>>,
    delimiter: Option<String>,
    alignment: Option<String>,
    text: Option<String>,
    attributes: Option<Attrs>,
    modifier: Option<String>,
}

impl Node {
    fn new(token: &str) -> Node {
        Node {
            token: token.to_string(),
            ..Node::default()
        }
    }
    fn with_children(token: &str, children: Vec<Node>) -> Node {
        Node {
            token: token.to_string(),
            children: Some(children),
            ..Node::default()
        }
    }
    fn with_text(token: &str, text: String) -> Node {
        Node {
            token: token.to_string(),
            text: Some(text),
            ..Node::default()
        }
    }
    fn kids(&self) -> &[Node] {
        self.children.as_deref().unwrap_or(&[])
    }
    /// `node.children[i]` — `IndexError` when out of range.
    fn kid(&self, i: usize) -> Result<&Node> {
        self.kids().get(i).ok_or(Error::Runtime("IndexError"))
    }
}

/// The shared token iterator the walker's recursive calls consume.
trait Tokens {
    fn next_token(&mut self) -> Option<String>;
}

struct Base {
    tokens: Vec<String>,
    pos: usize,
}

impl Tokens for Base {
    fn next_token(&mut self) -> Option<String> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
}

/// `itertools.chain(iter(expanded), tokens)`: a macro's expansion in front
/// of the live stream; whatever the recursive walk leaves of the prefix is
/// dropped with it, exactly as the Python chain object is.
struct Chain<'a> {
    prefix: Vec<String>,
    pos: usize,
    rest: &'a mut dyn Tokens,
}

impl Tokens for Chain<'_> {
    fn next_token(&mut self) -> Option<String> {
        if self.pos < self.prefix.len() {
            self.pos += 1;
            return Some(self.prefix[self.pos - 1].clone());
        }
        self.rest.next_token()
    }
}

/// `next(tokens)` — `StopIteration` propagates as an error.
fn next(tk: &mut dyn Tokens) -> Result<String> {
    tk.next_token().ok_or(Error::Runtime("StopIteration"))
}

type Macros = HashMap<String, (Vec<String>, i64)>;

/// `str.lstrip("\\")`.
fn lstrip_backslash(s: &str) -> &str {
    s.trim_start_matches('\\')
}

struct Walker {
    /// Recursion budget standing in for Python's recursion limit.
    nesting: usize,
}

impl Walker {
    /// `walker.walk`.
    fn walk(latex: &str, block: bool, macros: &mut Macros) -> Result<Vec<Node>> {
        let mut base = Base {
            tokens: tokenize(latex)?,
            pos: 0,
        };
        let mut w = Walker { nesting: 0 };
        w.walk_tokens(&mut base, None, 0, block, macros, 0)
    }

    /// `_walk(tokens, terminator, limit, block, macros, depth)`.
    fn walk_tokens(
        &mut self,
        tk: &mut dyn Tokens,
        terminator: Option<&str>,
        limit: usize,
        block: bool,
        macros: &mut Macros,
        depth: usize,
    ) -> Result<Vec<Node>> {
        self.nesting += 1;
        let r = self.walk_inner(tk, terminator, limit, block, macros, depth);
        self.nesting -= 1;
        r
    }

    /// `_walk(tokens, terminator, limit, macros=…)` — the recursive call
    /// shape the walker uses everywhere but for environments and macros
    /// (`block` and `depth` are *not* forwarded there).
    fn sub(
        &mut self,
        tk: &mut dyn Tokens,
        terminator: Option<&str>,
        limit: usize,
        macros: &mut Macros,
    ) -> Result<Vec<Node>> {
        self.walk_tokens(tk, terminator, limit, false, macros, 0)
    }

    /// `tuple(_walk(tokens, terminator=terminator, limit=1, …))[0]`.
    fn one(
        &mut self,
        tk: &mut dyn Tokens,
        terminator: Option<&str>,
        macros: &mut Macros,
    ) -> Result<Node> {
        let mut v = self.sub(tk, terminator, 1, macros)?;
        if v.is_empty() {
            return Err(Error::Runtime("IndexError"));
        }
        Ok(v.swap_remove(0))
    }

    #[allow(clippy::too_many_lines)]
    fn walk_inner(
        &mut self,
        tk: &mut dyn Tokens,
        terminator: Option<&str>,
        limit: usize,
        block: bool,
        macros: &mut Macros,
        depth: usize,
    ) -> Result<Vec<Node>> {
        if self.nesting > MAX_NESTING {
            return Err(Error::Runtime("RecursionError"));
        }
        let mut group: Vec<Node> = Vec::new();
        let mut has_available_tokens = false;
        while let Some(token) = tk.next_token() {
            has_available_tokens = true;
            let t = token.as_str();
            let node: Node;
            if Some(t) == terminator {
                let delimiter = if terminator == Some(RIGHT) {
                    Some(next(tk)?)
                } else {
                    None
                };
                group.push(Node {
                    delimiter,
                    ..Node::new(t)
                });
                break;
            } else if (t == RIGHT && terminator != Some(RIGHT))
                || (t == MIDDLE && terminator != Some(RIGHT))
            {
                return Err(Error::ExtraLeftOrMissingRight);
            } else if t == LEFT {
                let delimiter = next(tk)?;
                let children = self.sub(tk, Some(RIGHT), 0, macros)?;
                if children.last().is_none_or(|c| c.token != RIGHT) {
                    return Err(Error::ExtraLeftOrMissingRight);
                }
                node = Node {
                    delimiter: Some(delimiter),
                    ..Node::with_children(t, children)
                };
            } else if t == OPENING_BRACE {
                let mut children = self.sub(tk, Some(CLOSING_BRACE), 0, macros)?;
                if children.last().is_some_and(|c| c.token == CLOSING_BRACE) {
                    children.pop();
                }
                node = Node::with_children(BRACES, children);
            } else if t == SUBSCRIPT || t == SUPERSCRIPT {
                let previous = group.pop().unwrap_or_else(|| Node::new(""));
                if t == SUBSCRIPT && previous.token == SUBSCRIPT {
                    return Err(Error::DoubleSubscripts);
                }
                if t == SUPERSCRIPT
                    && previous.token == SUPERSCRIPT
                    && previous.children.is_some()
                    && previous.kids().len() >= 2
                    && previous.kids()[1].token != PRIME
                {
                    return Err(Error::DoubleSuperscripts);
                }
                let mut modifier: Option<String> = None;
                let previous = if previous.token == LIMITS || previous.token == NOLIMITS {
                    modifier = Some(previous.token.clone());
                    match group.pop() {
                        Some(p) if p.token.starts_with('\\') => p,
                        _ => return Err(Error::LimitsMustFollowMathOperator),
                    }
                } else {
                    if block && (previous.token == SUMMATION || previous.token == PRODUCT) {
                        modifier = Some(LIMITS.to_string());
                    }
                    previous
                };
                if t == SUBSCRIPT && previous.token == SUPERSCRIPT && previous.children.is_some() {
                    let children = self.sub(tk, terminator, 1, macros)?;
                    let pk = previous.kids();
                    let mut all = vec![pk.first().cloned().ok_or(Error::Runtime("IndexError"))?];
                    all.extend(children);
                    all.push(pk.get(1).cloned().ok_or(Error::Runtime("IndexError"))?);
                    node = Node {
                        modifier: previous.modifier.clone(),
                        ..Node::with_children(SUBSUP, all)
                    };
                } else if t == SUPERSCRIPT
                    && previous.token == SUBSCRIPT
                    && previous.children.is_some()
                {
                    let children = self.sub(tk, terminator, 1, macros)?;
                    let mut all = previous.kids().to_vec();
                    all.extend(children);
                    node = Node {
                        modifier: previous.modifier.clone(),
                        ..Node::with_children(SUBSUP, all)
                    };
                } else if t == SUPERSCRIPT
                    && previous.token == SUPERSCRIPT
                    && previous.children.is_some()
                    && previous.kid(1)?.token == PRIME
                {
                    let children = self.sub(tk, terminator, 1, macros)?;
                    let mut braces = vec![previous.kid(1)?.clone()];
                    braces.extend(children);
                    node = Node {
                        modifier: previous.modifier.clone(),
                        ..Node::with_children(
                            SUPERSCRIPT,
                            vec![
                                previous.kid(0)?.clone(),
                                Node::with_children(BRACES, braces),
                            ],
                        )
                    };
                } else {
                    let children = match self.sub(tk, terminator, 1, macros) {
                        Ok(c) => c,
                        Err(Error::NoAvailableTokens) => {
                            return Err(Error::MissingSuperScriptOrSubscript)
                        }
                        Err(e) => return Err(e),
                    };
                    if previous.token == OVERBRACE || previous.token == UNDERBRACE {
                        modifier = Some(previous.token.clone());
                    }
                    let mut all = vec![previous];
                    all.extend(children);
                    node = Node {
                        modifier,
                        ..Node::with_children(t, all)
                    };
                }
            } else if t == APOSTROPHE {
                let previous = group.pop().unwrap_or_else(|| Node::new(""));
                let prev_is_super_with_children = previous.token == SUPERSCRIPT
                    && previous.children.is_some()
                    && previous.kids().len() >= 2;
                let prev_prime = if prev_is_super_with_children {
                    Some(&previous.kids()[1])
                } else {
                    None
                };
                let prev_is_prime_token = prev_prime.is_some_and(|p| {
                    matches!(
                        p.token.as_str(),
                        PRIME | DPRIME | TRPRIME | QPRIME | MULTIPRIMES
                    )
                });
                if prev_is_super_with_children && !prev_is_prime_token {
                    return Err(Error::DoubleSuperscripts);
                }
                if let (true, Some(pp)) = (prev_is_prime_token, prev_prime) {
                    let new_prime = match pp.token.as_str() {
                        PRIME => Node::new(DPRIME),
                        DPRIME => Node::new(TRPRIME),
                        TRPRIME => Node::new(QPRIME),
                        QPRIME => Node::with_text(MULTIPRIMES, "5".to_string()),
                        _ => {
                            let n = py_int_lenient(pp.text.as_deref().unwrap_or("0"))?;
                            Node::with_text(MULTIPRIMES, (n + 1).to_string())
                        }
                    };
                    node =
                        Node::with_children(SUPERSCRIPT, vec![previous.kid(0)?.clone(), new_prime]);
                } else if previous.token == SUBSCRIPT && previous.children.is_some() {
                    let mut all = previous.kids().to_vec();
                    all.push(Node::new(PRIME));
                    node = Node {
                        modifier: previous.modifier.clone(),
                        ..Node::with_children(SUBSUP, all)
                    };
                } else {
                    node = Node::with_children(SUPERSCRIPT, vec![previous, Node::new(PRIME)]);
                }
            } else if has_two_parameters(t) {
                let mut children = self.sub(tk, terminator, 2, macros)?;
                if t == OVERSET || t == STACKREL || t == UNDERSET {
                    children.reverse();
                }
                node = Node::with_children(t, children);
            } else if has_one_parameter(t)
                || (t.starts_with(MATH) && !MATH_NON_FONT_COMMANDS.contains(&t))
            {
                let children = self.sub(tk, terminator, 1, macros)?;
                node = Node::with_children(t, children);
            } else if t == NOT {
                match self.one(tk, terminator, macros) {
                    Ok(next_node) => {
                        if next_node.token.starts_with('\\') {
                            let negated = format!("\\n{}", &next_node.token[1..]);
                            if convert_symbol(&negated).is_some() {
                                group.push(Node::new(&negated));
                                continue;
                            }
                        }
                        group.push(Node::new(t));
                        group.push(next_node);
                        continue;
                    }
                    Err(Error::NoAvailableTokens) => node = Node::new(t),
                    Err(e) => return Err(e),
                }
            } else if extensible_arrow(t).is_some() {
                let mut children = self.sub(tk, terminator, 1, macros)?;
                if children.first().ok_or(Error::Runtime("IndexError"))?.token == OPENING_BRACKET {
                    let mut opt = self.sub(tk, Some(CLOSING_BRACKET), 0, macros)?;
                    opt.pop();
                    let mut all = vec![Node::with_children(BRACES, opt)];
                    all.extend(self.sub(tk, terminator, 1, macros)?);
                    children = all;
                }
                node = Node::with_children(t, children);
            } else if matches!(t, HSKIP | HSPACE | KERN | MKERN | MSKIP | MSPACE) {
                let children = self.sub(tk, terminator, 1, macros)?;
                let width = unwrap_token(children.first().ok_or(Error::Runtime("IndexError"))?)?;
                node = Node {
                    attributes: Some(attrs(&[("width", &width)])),
                    ..Node::new(t)
                };
            } else if matches!(t, RAISE | LOWER | MOVELEFT | MOVERIGHT) {
                let dim_children = self.sub(tk, terminator, 1, macros)?;
                let dim = match dim_children.first() {
                    Some(d) => unwrap_token(d)?,
                    None => "0".to_string(),
                };
                let children = self.sub(tk, terminator, 1, macros)?;
                let attributes = match t {
                    RAISE => attrs(&[
                        ("voffset", &dim),
                        ("height", &format!("+{dim}")),
                        ("depth", &format!("-{dim}")),
                    ]),
                    LOWER => attrs(&[
                        ("voffset", &format!("-{dim}")),
                        ("height", &format!("-{dim}")),
                        ("depth", &format!("+{dim}")),
                    ]),
                    MOVELEFT => attrs(&[("lspace", &format!("-{dim}"))]),
                    _ => attrs(&[("lspace", &dim)]),
                };
                node = Node {
                    attributes: Some(attributes),
                    ..Node::with_children(t, children)
                };
            } else if t == RULE {
                let mut dims = Vec::new();
                for _ in 0..2 {
                    let arg = self.one(tk, terminator, macros)?;
                    dims.push(unwrap_token(&arg)?);
                }
                node = Node {
                    attributes: Some(attrs(&[("width", &dims[0]), ("height", &dims[1])])),
                    ..Node::new(t)
                };
            } else if t == SMASH {
                let mut children = self.sub(tk, terminator, 1, macros)?;
                let mut attributes = attrs(&[("height", "0px"), ("depth", "0px")]);
                if children.first().ok_or(Error::Runtime("IndexError"))?.token == OPENING_BRACKET {
                    let mut opt = self.sub(tk, Some(CLOSING_BRACKET), 0, macros)?;
                    opt.pop();
                    match opt.first().map(|n| n.token.as_str()) {
                        Some("b") => attributes = attrs(&[("depth", "0px")]),
                        Some("t") => attributes = attrs(&[("height", "0px")]),
                        _ => {}
                    }
                    children = self.sub(tk, terminator, 1, macros)?;
                }
                node = Node {
                    attributes: Some(attributes),
                    ..Node::with_children(t, children)
                };
            } else if t == TEXTCOLOR {
                let color = next(tk)?;
                let children = self.sub(tk, terminator, 1, macros)?;
                node = Node {
                    attributes: Some(attrs(&[("mathcolor", &color)])),
                    ..Node::with_children(COLOR, children)
                };
            } else if t == COLORBOX || t == FCOLORBOX {
                let arg_count = if t == FCOLORBOX { 3 } else { 2 };
                let mut args: Vec<String> = Vec::new();
                for _ in 0..arg_count {
                    let arg_node = self.one(tk, terminator, macros)?;
                    args.push(
                        arg_node
                            .children
                            .as_ref()
                            .map(|c| c.iter().map(|n| n.token.as_str()).collect::<String>())
                            .unwrap_or_default(),
                    );
                }
                let attributes = if t == FCOLORBOX {
                    attrs(&[("mathbackground", &args[1]), ("border-color", &args[0])])
                } else {
                    attrs(&[("mathbackground", &args[0])])
                };
                node = Node {
                    attributes: Some(attributes),
                    text: args.last().cloned(),
                    ..Node::new(t)
                };
            } else if t == COLOR {
                let attributes = attrs(&[("mathcolor", &next(tk)?)]);
                let mut children = self.sub(tk, terminator, 0, macros)?;
                let mut sibling = None;
                if children
                    .last()
                    .is_some_and(|c| Some(c.token.as_str()) == terminator)
                {
                    sibling = children.pop();
                }
                group.push(Node {
                    attributes: Some(attributes),
                    ..Node::with_children(t, children)
                });
                if let Some(sib) = sibling {
                    group.push(sib);
                }
                break;
            } else if t == LEFTROOT || t == UPROOT {
                self.sub(tk, terminator, 1, macros)?;
                continue;
            } else if t == MATHCHOICE {
                let mut choices = self.sub(tk, terminator, 4, macros)?;
                let idx = if block { 0 } else { 1 };
                if choices.len() <= idx {
                    return Err(Error::Runtime("IndexError"));
                }
                let choice = choices.swap_remove(idx);
                match choice.children {
                    Some(kids) if !kids.is_empty() => group.extend(kids),
                    _ => group.push(choice),
                }
                continue;
            } else if t == CLASS || t == STYLE {
                let attr_name = if t == CLASS { "class" } else { "style" };
                let attributes = attrs(&[(attr_name, &next(tk)?)]);
                let next_node = self.one(tk, terminator, macros)?;
                node = Node {
                    attributes: Some(attributes),
                    ..next_node
                };
            } else if big_size(t).is_some()
                || big_open_close(t).is_some()
                || matches!(
                    t,
                    CLAP | r"\emph"
                        | FBOX
                        | HBOX
                        | LLAP
                        | MBOX
                        | MIDDLE
                        | RLAP
                        | TAG
                        | TAGSTAR
                        | r"\text"
                        | r"\textbf"
                        | r"\textit"
                        | r"\textmd"
                        | r"\textnormal"
                        | r"\textrm"
                        | r"\textsf"
                        | r"\texttt"
                        | r"\textup"
                        | VERB
                )
            {
                node = Node::with_text(t, next(tk)?);
            } else if t == HREF {
                let attributes = attrs(&[("href", &next(tk)?)]);
                let children = self.sub(tk, terminator, 1, macros)?;
                node = Node {
                    attributes: Some(attributes),
                    ..Node::with_children(t, children)
                };
            } else if matches!(
                t,
                ABOVE | ATOP | ABOVEWITHDELIMS | ATOPWITHDELIMS | BRACE | BRACK | CHOOSE | OVER
            ) {
                let mut attributes: Option<Attrs> = None;
                let mut delimiter: Option<String> = None;
                if t == ABOVEWITHDELIMS {
                    let a = next(tk)?;
                    let b = next(tk)?;
                    delimiter = Some(format!("{}{}", lstrip_backslash(&a), lstrip_backslash(&b)));
                } else if t == ATOPWITHDELIMS {
                    attributes = Some(attrs(&[("linethickness", "0")]));
                    let a = next(tk)?;
                    let b = next(tk)?;
                    delimiter = Some(format!("{}{}", lstrip_backslash(&a), lstrip_backslash(&b)));
                } else if t == BRACE {
                    delimiter = Some("{}".to_string());
                } else if t == BRACK {
                    delimiter = Some("[]".to_string());
                } else if t == CHOOSE {
                    delimiter = Some("()".to_string());
                }
                if t == ABOVE || t == ABOVEWITHDELIMS {
                    let dimension_node = self.one(tk, terminator, macros)?;
                    let dimension = unwrap_token(&dimension_node)?;
                    attributes = Some(attrs(&[("linethickness", &dimension)]));
                } else if matches!(t, ATOP | BRACE | BRACK | CHOOSE) {
                    attributes = Some(attrs(&[("linethickness", "0")]));
                }
                let mut denominator = self.sub(tk, terminator, 0, macros)?;
                let mut sibling = None;
                if denominator
                    .last()
                    .is_some_and(|c| Some(c.token.as_str()) == terminator)
                {
                    sibling = denominator.pop();
                }
                if denominator.is_empty() {
                    if t == BRACE || t == BRACK {
                        denominator = vec![Node::with_children(BRACES, Vec::new())];
                    } else {
                        return Err(Error::DenominatorNotFound);
                    }
                }
                if group.is_empty() {
                    if t == BRACE || t == BRACK {
                        group = vec![Node::with_children(BRACES, Vec::new())];
                    } else {
                        return Err(Error::NumeratorNotFound);
                    }
                }
                if denominator.len() > 1 {
                    denominator = vec![Node::with_children(BRACES, denominator)];
                }
                let mut children = if group.len() == 1 {
                    std::mem::take(&mut group)
                } else {
                    vec![Node::with_children(BRACES, std::mem::take(&mut group))]
                };
                children.extend(denominator);
                group = vec![Node {
                    attributes,
                    delimiter,
                    ..Node::with_children(FRAC, children)
                }];
                if let Some(sib) = sibling {
                    group.push(sib);
                }
                break;
            } else if t == SQRT {
                let mut root_nodes: Option<Vec<Node>> = None;
                let mut next_node = self.one(tk, None, macros)?;
                if next_node.token == OPENING_BRACKET {
                    let mut rn = self.sub(tk, Some(CLOSING_BRACKET), 0, macros)?;
                    rn.pop();
                    next_node = self.one(tk, None, macros)?;
                    if rn.len() > 1 {
                        rn = vec![Node::with_children(BRACES, rn)];
                    }
                    root_nodes = Some(rn);
                }
                match root_nodes {
                    Some(rn) if !rn.is_empty() => {
                        let mut all = vec![next_node];
                        all.extend(rn);
                        node = Node::with_children(ROOT, all);
                    }
                    _ => node = Node::with_children(t, vec![next_node]),
                }
            } else if t == ROOT {
                let mut root_nodes = self.sub(tk, Some(r"\of"), 0, macros)?;
                root_nodes.pop();
                let next_node = self.one(tk, None, macros)?;
                if root_nodes.len() > 1 {
                    root_nodes = vec![Node::with_children(BRACES, root_nodes)];
                }
                let mut all = vec![next_node];
                if root_nodes.is_empty() {
                    all.push(Node::with_children(BRACES, Vec::new()));
                } else {
                    all.extend(root_nodes);
                }
                node = Node::with_children(t, all);
            } else if MATRICES.contains(&t) {
                let mut children = self.sub(tk, terminator, 0, macros)?;
                let mut sibling = None;
                if children
                    .last()
                    .is_some_and(|c| Some(c.token.as_str()) == terminator)
                {
                    sibling = children.pop();
                }
                if children.len() == 1
                    && children[0].token == BRACES
                    && children[0].children.as_ref().is_some_and(|c| !c.is_empty())
                {
                    children = children.swap_remove(0).children.unwrap_or_default();
                }
                let matrix = Node {
                    alignment: Some(String::new()),
                    ..Node::with_children(t, children)
                };
                if let Some(sib) = sibling {
                    group.push(matrix);
                    group.push(sib);
                    break;
                }
                node = matrix;
            } else if t == GENFRAC {
                let a = next(tk)?;
                let b = next(tk)?;
                let delimiter = format!("{}{}", lstrip_backslash(&a), lstrip_backslash(&b));
                let pair = self.sub(tk, terminator, 2, macros)?;
                if pair.len() != 2 {
                    return Err(Error::Runtime("ValueError"));
                }
                let dimension = unwrap_token(&pair[0])?;
                let style = get_style(&pair[1])?;
                let attributes = attrs(&[("linethickness", &dimension)]);
                let children = self.sub(tk, terminator, 2, macros)?;
                group.push(Node::new(style));
                group.push(Node {
                    delimiter: Some(delimiter),
                    attributes: Some(attributes),
                    ..Node::with_children(t, children)
                });
                break;
            } else if t == SIDESET {
                let mut three = self.sub(tk, terminator, 3, macros)?;
                if three.len() != 3 {
                    return Err(Error::Runtime("ValueError"));
                }
                let operator = three.pop().unwrap_or_default();
                let right = three.pop().unwrap_or_default();
                let left = three.pop().unwrap_or_default();
                let (left_token, left_children) = make_subsup(&left)?;
                let (right_token, right_children) = make_subsup(&right)?;
                let op = |attributes: Attrs| Node {
                    token: operator.token.clone(),
                    children: operator.children.clone(),
                    attributes: Some(attributes),
                    ..Node::default()
                };
                let movable = attrs(&[("movablelimits", "false")]);
                let mut lk = vec![Node::with_children(VPHANTOM, vec![op(movable.clone())])];
                lk.extend(left_children);
                let mut rk = vec![op(movable)];
                rk.extend(right_children);
                node = Node::with_children(
                    t,
                    vec![
                        Node::with_children(left_token, lk),
                        Node::with_children(right_token, rk),
                    ],
                );
            } else if t == SKEW {
                let mut pair = self.sub(tk, terminator, 2, macros)?;
                if pair.len() != 2 {
                    return Err(Error::Runtime("ValueError"));
                }
                let child = pair.pop().unwrap_or_default();
                let width_node = pair.pop().unwrap_or_default();
                let width = if width_node.token == BRACES {
                    match width_node.children.as_ref().and_then(|c| c.first()) {
                        Some(first) => first.token.clone(),
                        None => return Err(Error::InvalidWidth),
                    }
                } else {
                    width_node.token.clone()
                };
                if !py_isdigit(&width) {
                    return Err(Error::InvalidWidth);
                }
                let em = 0.0555 * py_int(&width)? as f64;
                node = Node {
                    attributes: Some(attrs(&[("width", &format!("{em:.3}em"))])),
                    ..Node::with_children(t, vec![child])
                };
            } else if t.starts_with(BEGIN) {
                node = self.environment_node(t, tk, macros, block)?;
            } else if t == NEWCOMMAND {
                parse_newcommand(tk, macros)?;
                continue;
            } else if t == DEF {
                parse_def(tk, macros)?;
                continue;
            } else if t == DECLAREMATHOPERATOR {
                parse_declare_math_operator(tk, macros)?;
                continue;
            } else if t == NEWENVIRONMENT {
                parse_newenvironment(tk, macros)?;
                continue;
            } else if macros.contains_key(t) {
                if depth >= MAX_MACRO_DEPTH {
                    return Err(Error::Runtime("RecursionError"));
                }
                let expanded = expand_macro(t, tk, macros)?;
                if expanded.is_empty() {
                    continue;
                }
                let mut chained = Chain {
                    prefix: expanded,
                    pos: 0,
                    rest: tk,
                };
                let remaining_limit = if limit != 0 {
                    limit.saturating_sub(group.len())
                } else {
                    0
                };
                let more = self.walk_tokens(
                    &mut chained,
                    terminator,
                    remaining_limit,
                    block,
                    macros,
                    depth + 1,
                )?;
                group.extend(more);
                break;
            } else {
                node = Node::new(t);
            }

            group.push(node);
            if limit != 0 && group.len() >= limit {
                break;
            }
        }
        if !has_available_tokens {
            return Err(Error::NoAvailableTokens);
        }
        Ok(group)
    }

    /// `_get_environment_node`.
    fn environment_node(
        &mut self,
        token: &str,
        tk: &mut dyn Tokens,
        macros: &mut Macros,
        block: bool,
    ) -> Result<Node> {
        let start = token.find('{').ok_or(Error::Runtime("ValueError"))? + 1;
        let environment = &token[start..token.len() - 1];
        let env_key = format!("\\begin{{{environment}}}");
        let terminator = format!("{END}{{{environment}}}");
        if let Some((begin_body, nargs)) = macros.get(&env_key).cloned() {
            let (end_body, _) = macros
                .get(&format!("\\end{{{environment}}}"))
                .cloned()
                .unwrap_or_default();
            let mut args: Vec<Vec<String>> = Vec::new();
            for _ in 0..nargs.max(0) {
                args.push(consume_brace_arg(tk)?);
            }
            let mut raw_tokens: Vec<String> = Vec::new();
            let mut found_end = false;
            while let Some(t) = tk.next_token() {
                if t == terminator {
                    found_end = true;
                    break;
                }
                raw_tokens.push(t);
            }
            if !found_end {
                return Err(Error::MissingEnd);
            }
            let mut expanded = substitute_params(&begin_body, &args);
            expanded.extend(raw_tokens);
            expanded.extend(substitute_params(&end_body, &args));
            let mut base = Base {
                tokens: expanded,
                pos: 0,
            };
            let mut result = self.walk_tokens(&mut base, None, 0, block, macros, 0)?;
            if result.len() == 1 {
                return Ok(result.swap_remove(0));
            }
            return Ok(Node::with_children(BRACES, result));
        }
        let mut children = self.walk_tokens(tk, Some(&terminator), 0, block, macros, 0)?;
        if children.last().is_some_and(|c| c.token != terminator) {
            return Err(Error::MissingEnd);
        }
        children.pop();
        let mut alignment = String::new();
        if children.first().is_some_and(|c| c.token == OPENING_BRACKET) {
            let mut it = children.into_iter();
            it.next();
            for c in it.by_ref() {
                if c.token == CLOSING_BRACKET {
                    break;
                } else if !"lcr|".contains(c.token.as_str()) {
                    return Err(Error::InvalidAlignment);
                }
                alignment.push_str(&c.token);
            }
            children = it.collect();
        } else if children.first().is_some_and(|c| {
            c.children.is_some()
                && c.token == BRACES
                && c.kids().iter().all(|k| "lcr|".contains(k.token.as_str()))
        }) {
            alignment = children[0]
                .kids()
                .iter()
                .map(|k| k.token.as_str())
                .collect();
            children.remove(0);
        }
        Ok(Node {
            alignment: Some(alignment),
            ..Node::with_children(&format!("\\{environment}"), children)
        })
    }
}

/// `_make_subsup`.
fn make_subsup(node: &Node) -> Result<(&str, Vec<Node>)> {
    if node.token != BRACES {
        return Err(Error::MissingSuperScriptOrSubscript);
    }
    if let Some(first) = node.kids().first() {
        if let Some(fk) = &first.children {
            if (2..=3).contains(&fk.len())
                && matches!(first.token.as_str(), SUBSUP | SUBSCRIPT | SUPERSCRIPT)
            {
                return Ok((first.token.as_str(), fk[1..].to_vec()));
            }
        }
    }
    Ok(("", Vec::new()))
}

/// `_unwrap_token`.
fn unwrap_token(node: &Node) -> Result<String> {
    if node.token == BRACES && node.children.is_some() {
        return Ok(node.kid(0)?.token.clone());
    }
    Ok(node.token.clone())
}

/// `_get_style`.
fn get_style(node: &Node) -> Result<&'static str> {
    match unwrap_token(node)?.as_str() {
        "0" => Ok(DISPLAYSTYLE),
        "1" => Ok(TEXTSTYLE),
        "2" => Ok(SCRIPTSTYLE),
        "3" => Ok(SCRIPTSCRIPTSTYLE),
        _ => Err(Error::InvalidStyleForGenfrac),
    }
}

/// `_consume_brace_arg`.
fn consume_brace_arg(tk: &mut dyn Tokens) -> Result<Vec<String>> {
    let token = next(tk)?;
    if token == "{" {
        return Ok(read_until_close_brace(tk));
    }
    Ok(vec![token])
}

/// `_read_until_close_brace`.
fn read_until_close_brace(tk: &mut dyn Tokens) -> Vec<String> {
    let mut depth = 1;
    let mut content = Vec::new();
    while let Some(t) = tk.next_token() {
        if t == "{" {
            depth += 1;
        } else if t == "}" {
            depth -= 1;
            if depth == 0 {
                return content;
            }
        }
        content.push(t);
    }
    content
}

/// `_parse_optional_int`: `(nargs, the token after)`.
fn parse_optional_int(tk: &mut dyn Tokens) -> Result<(i64, String)> {
    let peek = next(tk)?;
    if peek != "[" {
        return Ok((0, peek));
    }
    let mut nargs_str = String::new();
    while let Some(t) = tk.next_token() {
        if t == "]" {
            break;
        }
        nargs_str.push_str(&t);
    }
    Ok((py_int_lenient(&nargs_str)?, next(tk)?))
}

/// `_parse_newcommand`.
fn parse_newcommand(tk: &mut dyn Tokens, macros: &mut Macros) -> Result<()> {
    let name = consume_brace_arg(tk)?.concat();
    let (nargs, mut peek) = parse_optional_int(tk)?;
    if peek == "[" {
        while let Some(t) = tk.next_token() {
            if t == "]" {
                break;
            }
        }
        peek = next(tk)?;
    }
    let body = if peek == "{" {
        read_until_close_brace(tk)
    } else {
        vec![peek]
    };
    macros.insert(name, (body, nargs));
    Ok(())
}

/// `_parse_newenvironment`.
fn parse_newenvironment(tk: &mut dyn Tokens, macros: &mut Macros) -> Result<()> {
    let name = consume_brace_arg(tk)?.concat();
    let (nargs, peek) = parse_optional_int(tk)?;
    let begin_body = if peek == "{" {
        read_until_close_brace(tk)
    } else {
        vec![peek]
    };
    let end_body = consume_brace_arg(tk)?;
    macros.insert(format!("\\begin{{{name}}}"), (begin_body, nargs));
    macros.insert(format!("\\end{{{name}}}"), (end_body, 0));
    Ok(())
}

/// `_parse_def`.
fn parse_def(tk: &mut dyn Tokens, macros: &mut Macros) -> Result<()> {
    let name = next(tk)?;
    let mut nargs: i64 = 0;
    while let Some(t) = tk.next_token() {
        if t == "#" {
            let param = tk.next_token().unwrap_or_default();
            if py_isdigit(&param) {
                nargs = nargs.max(py_int(&param)?);
            }
        } else if t == "{" {
            break;
        }
    }
    let body = read_until_close_brace(tk);
    macros.insert(name, (body, nargs));
    Ok(())
}

/// `_parse_declare_math_operator`.
fn parse_declare_math_operator(tk: &mut dyn Tokens, macros: &mut Macros) -> Result<()> {
    let name = consume_brace_arg(tk)?.concat();
    let text = consume_brace_arg(tk)?.concat();
    macros.insert(name, (vec![format!("\\operatorname{{{text}}}")], 0));
    Ok(())
}

/// `_substitute_params`.
fn substitute_params(body: &[String], args: &[Vec<String>]) -> Vec<String> {
    if args.is_empty() {
        return body.to_vec();
    }
    let mut expanded = Vec::new();
    let mut it = body.iter();
    while let Some(tok) = it.next() {
        if tok == "#" {
            let param_num = it.next().cloned().unwrap_or_default();
            let n = if py_isdigit(&param_num) {
                py_int(&param_num).ok()
            } else {
                None
            };
            match n {
                Some(n) if n >= 1 && (n as usize) <= args.len() => {
                    expanded.extend(args[n as usize - 1].iter().cloned());
                }
                _ => {
                    expanded.push(tok.clone());
                    if !param_num.is_empty() {
                        expanded.push(param_num);
                    }
                }
            }
        } else {
            expanded.push(tok.clone());
        }
    }
    expanded
}

/// `_expand_macro`.
fn expand_macro(token: &str, tk: &mut dyn Tokens, macros: &Macros) -> Result<Vec<String>> {
    let (body, nargs) = macros.get(token).ok_or(Error::Runtime("KeyError"))?;
    if *nargs == 0 {
        return Ok(body.clone());
    }
    let mut args = Vec::new();
    for _ in 0..nargs.max(&0).to_owned() {
        args.push(consume_brace_arg(tk)?);
    }
    Ok(substitute_params(body, &args))
}

// ---------------------------------------------------------------------------
// xml.etree.ElementTree — the little of it the converter uses.
// ---------------------------------------------------------------------------

/// An element arena standing in for ElementTree's mutable `Element`s.
#[derive(Default)]
struct Tree {
    nodes: Vec<Element>,
}

struct Element {
    tag: &'static str,
    attrib: Attrs,
    text: Option<String>,
    children: Vec<usize>,
}

type El = usize;

impl Tree {
    /// `Element(tag, attrib)` — a root.
    fn root(&mut self, tag: &'static str, attrib: Attrs) -> El {
        self.nodes.push(Element {
            tag,
            attrib,
            text: None,
            children: Vec::new(),
        });
        self.nodes.len() - 1
    }
    /// `SubElement(parent, tag, attrib)`.
    fn sub_attrs(&mut self, parent: El, tag: &'static str, attrib: Attrs) -> El {
        let id = self.root(tag, attrib);
        self.nodes[parent].children.push(id);
        id
    }
    fn sub(&mut self, parent: El, tag: &'static str, pairs: &[(&str, &str)]) -> El {
        self.sub_attrs(parent, tag, attrs(pairs))
    }
    fn set_text(&mut self, el: El, text: &str) {
        self.nodes[el].text = Some(text.to_string());
    }
    /// `element.set(key, value)` / `element.attrib[key] = value`.
    fn set_attr(&mut self, el: El, key: &str, value: &str) {
        attr_set(&mut self.nodes[el].attrib, key, value);
    }
    fn get_attr(&self, el: El, key: &str) -> Option<&str> {
        attr_get(&self.nodes[el].attrib, key)
    }
    /// `len(element)`.
    fn len(&self, el: El) -> usize {
        self.nodes[el].children.len()
    }
    /// `parent.remove(child)`.
    fn remove(&mut self, parent: El, child: El) {
        self.nodes[parent].children.retain(|&c| c != child);
    }

    /// `_DOUBLE_ESCAPED_CHAR_REF.sub(r"&\1", tostring(el))`: ElementTree's
    /// serialisation (`_escape_cdata` on text, `_escape_attrib` on
    /// attributes), then every `&amp;#x…;` turned back into the numeric
    /// character reference latex2mathml meant.
    fn to_string(&self, el: El) -> String {
        let mut out = String::new();
        self.write(el, &mut out);
        out
    }

    fn write(&self, el: El, out: &mut String) {
        let e = &self.nodes[el];
        out.push('<');
        out.push_str(e.tag);
        for (k, v) in &e.attrib {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&escape_attrib(v));
            out.push('"');
        }
        let text = e.text.as_deref().filter(|t| !t.is_empty());
        if text.is_none() && e.children.is_empty() {
            out.push_str(" />");
            return;
        }
        out.push('>');
        if let Some(t) = text {
            out.push_str(&revert_char_refs(escape_text(t)));
        }
        for &c in &e.children {
            self.write(c, out);
        }
        out.push_str("</");
        out.push_str(e.tag);
        out.push('>');
    }
}

/// ElementTree's `_escape_cdata` (= `html.escape(quote=False)`): `&<>`.
fn escape_text(v: &str) -> String {
    if !v.contains(['&', '<', '>']) {
        return v.to_string();
    }
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// ElementTree's `_escape_attrib`, then the char-ref revert.
fn escape_attrib(v: &str) -> String {
    let v = escape_text(v);
    if !v.contains(['"', '\r', '\n', '\t', '&']) {
        return v;
    }
    revert_char_refs(
        v.replace('"', "&quot;")
            .replace('\r', "&#13;")
            .replace('\n', "&#10;")
            .replace('\t', "&#09;"),
    )
}

/// docling-core's `_DOUBLE_ESCAPED_CHAR_REF.sub(r"&\1", …)`:
/// `&amp;(#x[0-9A-Fa-f]+;)` → `&\1`. Only hexadecimal references, as
/// upstream: latex2mathml writes its symbols that way.
fn revert_char_refs(s: String) -> String {
    const PAT: &str = "&amp;#x";
    if !s.contains(PAT) {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find(PAT) {
        out.push_str(&rest[..i]);
        let after = &rest[i + PAT.len()..];
        let hex = after.bytes().take_while(|b| b.is_ascii_hexdigit()).count();
        if hex > 0 && after.as_bytes().get(hex) == Some(&b';') {
            out.push_str("&#x");
            out.push_str(&after[..=hex]);
            rest = &after[hex + 1..];
        } else {
            out.push_str(PAT);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// converter.py
// ---------------------------------------------------------------------------

/// `converter.OPERATORS`.
fn is_operator(token: &str) -> bool {
    matches!(
        token,
        "+" | "-"
            | "*"
            | "/"
            | "("
            | ")"
            | "="
            | ","
            | "?"
            | "["
            | "]"
            | "|"
            | r"\|"
            | "!"
            | r"\{"
            | r"\}"
            | ">"
            | "<"
            | "."
            | r"\ast"
            | r"\bigotimes"
            | r"\cdot"
            | r"\centerdot"
            | r"\div"
            | r"\dots"
            | r"\dotsc"
            | r"\dotso"
            | r"\gt"
            | r"\ldotp"
            | r"\lt"
            | r"\lvert"
            | r"\lVert"
            | r"\lvertneqq"
            | r"\ngeqq"
            | r"\omicron"
            | r"\rvert"
            | r"\rVert"
            | r"\S"
            | r"\smallfrown"
            | r"\smallint"
            | r"\smallsmile"
            | r"\surd"
            | r"\times"
            | r"\varsubsetneqq"
            | r"\varsupsetneqq"
    )
}

/// `converter.MOVABLE_LIMIT_TEXTS`.
fn movable_limit_text(token: &str) -> Option<&'static str> {
    Some(match token {
        r"\argmax" => "arg&#x02009;max",
        r"\argmin" => "arg&#x02009;min",
        r"\det" => "det",
        GCD => "gcd",
        r"\injlim" | r"\varinjlim" => "inj&#x02006;lim",
        r"\intop" => "&#x0222B;",
        r"\liminf" | r"\varliminf" => "lim&#x02006;inf",
        r"\limsup" | r"\varlimsup" => "lim&#x02006;sup",
        r"\plim" => "plim",
        r"\Pr" => "Pr",
        r"\projlim" | r"\varprojlim" => "proj&#x02006;lim",
        _ => return None,
    })
}

/// `COLUMN_ALIGNMENT_MAP.get(c)`.
fn column_alignment(c: char) -> Option<&'static str> {
    match c {
        'r' => Some("right"),
        'l' => Some("left"),
        'c' => Some("center"),
        _ => None,
    }
}

fn entity(code: Option<&str>) -> String {
    // `"&#x{};".format(None)` — what a missing symbol really produces.
    format!("&#x{};", code.unwrap_or("None"))
}

fn nbsp(text: &str) -> String {
    text.replace(' ', "&#x000A0;")
}

struct Converter {
    tree: Tree,
    display: &'static str,
    equation_counter: usize,
    macros: Macros,
    nesting: usize,
}

impl Converter {
    fn new(display: &'static str) -> Converter {
        Converter {
            tree: Tree::default(),
            display,
            equation_counter: 0,
            macros: Macros::new(),
            nesting: 0,
        }
    }

    /// `Converter.convert_to_element(latex)`.
    fn convert_to_element(&mut self, latex: &str) -> Result<El> {
        let math = self.tree.root(
            "math",
            attrs(&[
                ("xmlns", "http://www.w3.org/1998/Math/MathML"),
                ("display", self.display),
            ]),
        );
        let row = self.tree.sub(math, "mrow", &[]);
        let nodes = Walker::walk(latex, self.display == "block", &mut self.macros)?;
        self.convert_group(&nodes, row, None)?;
        Ok(math)
    }

    /// `_convert_matrix`.
    #[allow(clippy::too_many_lines)]
    fn convert_matrix(
        &mut self,
        nodes: &[Node],
        parent: El,
        command: &str,
        alignment: Option<&str>,
    ) -> Result<()> {
        let mut row: Option<El> = None;
        let mut cell: Option<El> = None;
        let mut col_index = 0usize;
        let mut col_alignment: Option<&'static str> = None;
        let mut max_col_size = 0usize;
        let mut row_index = 0usize;
        let mut row_lines: Vec<&str> = Vec::new();
        let mut hfil_indexes: Vec<bool> = Vec::new();
        let numbered = command == ALIGN;
        let mut skip_number = false;
        let is_split = matches!(command, SPLIT | ALIGN | ALIGNSTAR);

        for node in nodes {
            let r = match row {
                Some(r) => r,
                None => {
                    let r = self.tree.sub(parent, "mtr", &[]);
                    row = Some(r);
                    r
                }
            };
            let c = match cell {
                Some(c) => c,
                None => {
                    (col_alignment, col_index) =
                        get_column_alignment(alignment, col_alignment, col_index);
                    let c = self.make_matrix_cell(r, col_alignment);
                    cell = Some(c);
                    c
                }
            };
            let t = node.token.as_str();
            if t == BRACES {
                self.convert_group(std::slice::from_ref(node), c, None)?;
            } else if t == "&" {
                self.set_cell_alignment(c, &hfil_indexes);
                hfil_indexes.clear();
                (col_alignment, col_index) =
                    get_column_alignment(alignment, col_alignment, col_index);
                let nc = self.make_matrix_cell(r, col_alignment);
                cell = Some(nc);
                if is_split && col_index % 2 == 0 {
                    self.tree.sub(nc, "mi", &[]);
                }
            } else if t == DOUBLEBACKSLASH || t == CARRIAGERETURN {
                self.set_cell_alignment(c, &hfil_indexes);
                hfil_indexes.clear();
                if numbered && !skip_number {
                    self.equation_counter += 1;
                    let eqn_cell = self.tree.sub(r, "mtd", &[]);
                    let eqn_num = self.tree.sub(eqn_cell, "mtext", &[]);
                    let n = self.equation_counter;
                    self.tree.set_text(eqn_num, &format!("({n})"));
                }
                skip_number = false;
                row_index += 1;
                if col_index > max_col_size {
                    max_col_size = col_index;
                }
                col_index = 0;
                (col_alignment, col_index) =
                    get_column_alignment(alignment, col_alignment, col_index);
                let nr = self.tree.sub(parent, "mtr", &[]);
                row = Some(nr);
                cell = Some(self.make_matrix_cell(nr, col_alignment));
            } else if t == NONUMBER || t == NOTAG {
                skip_number = true;
            } else if t == HLINE {
                row_lines.push("solid");
            } else if t == HDASHLINE {
                row_lines.push("dashed");
            } else if t == HFIL {
                hfil_indexes.push(true);
            } else {
                if row_index > row_lines.len() {
                    row_lines.push("none");
                }
                hfil_indexes.push(false);
                self.convert_group(std::slice::from_ref(node), c, None)?;
            }
        }

        if col_index > max_col_size {
            max_col_size = col_index;
        }
        if row_lines.iter().any(|r| *r != "none") {
            self.tree.set_attr(parent, "rowlines", &row_lines.join(" "));
        }
        if let (Some(r), Some(c)) = (row, cell) {
            if self.tree.len(c) == 0 {
                self.tree.remove(parent, r);
                row = None;
            }
        }
        if let (true, Some(r), false) = (numbered, row, skip_number) {
            self.equation_counter += 1;
            let eqn_cell = self.tree.sub(r, "mtd", &[]);
            let eqn_num = self.tree.sub(eqn_cell, "mtext", &[]);
            let n = self.equation_counter;
            self.tree.set_text(eqn_num, &format!("({n})"));
        }
        if max_col_size > 0 && (command == ALIGN || command == ALIGNSTAR) {
            let spacing = ["0em", "2em"].repeat(max_col_size / 2).join(" ");
            self.tree.set_attr(parent, "columnspacing", &spacing);
        }
        Ok(())
    }

    /// `_convert_group`.
    fn convert_group(&mut self, nodes: &[Node], parent: El, font: Option<Font>) -> Result<()> {
        self.nesting += 1;
        if self.nesting > MAX_NESTING {
            self.nesting -= 1;
            return Err(Error::Runtime("RecursionError"));
        }
        let r = self.convert_group_inner(nodes, parent, font);
        self.nesting -= 1;
        r
    }

    fn convert_group_inner(
        &mut self,
        nodes: &[Node],
        parent: El,
        font: Option<Font>,
    ) -> Result<()> {
        let mut font = font;
        let mut i = 0;
        while i < nodes.len() {
            let node = &nodes[i];
            i += 1;
            let t = node.token.as_str();
            if mstyle_size(t).is_some() || style_attrs(t).is_some() {
                // The style swallows every following sibling.
                let rest = Node::with_children(t, nodes[i..].to_vec());
                self.convert_command(&rest, parent, font)?;
                break;
            } else if t == UNICODE {
                let code = match node.kids().first() {
                    Some(arg) if arg.children.as_ref().is_some_and(|c| !c.is_empty()) => arg
                        .kids()
                        .iter()
                        .map(|c| c.token.as_str())
                        .collect::<String>(),
                    Some(arg) => arg.token.clone(),
                    None => String::new(),
                };
                let element = self.tree.sub(parent, "mi", &[]);
                self.tree
                    .set_text(element, &format!("&#x{};", code.trim_start_matches('x')));
            } else if t == RULE {
                let a = node.attributes.clone().unwrap_or_default();
                let width = attr_get(&a, "width").ok_or(Error::Runtime("KeyError"))?;
                let height = attr_get(&a, "height").ok_or(Error::Runtime("KeyError"))?;
                self.tree.sub(
                    parent,
                    "mspace",
                    &[
                        ("mathbackground", "black"),
                        ("width", width),
                        ("height", height),
                    ],
                );
            } else if conversion_map(t).is_some() {
                self.convert_command(node, parent, font)?;
            } else if let (Some(lf), Some(kids)) = (local_font(t), &node.children) {
                self.convert_group(kids, parent, Some(lf))?;
            } else if let (true, Some(kids)) = (t.starts_with(MATH), &node.children) {
                self.convert_group(kids, parent, font)?;
            } else if let Some(gf) = global_font(t) {
                font = Some(gf);
            } else if let Some(kids) = &node.children {
                let row = self.tree.sub_attrs(
                    parent,
                    "mrow",
                    node.attributes.clone().unwrap_or_default(),
                );
                self.convert_group(kids, row, font)?;
            } else {
                self.convert_symbol(node, parent, font)?;
            }
        }
        Ok(())
    }

    /// `_convert_command`.
    #[allow(clippy::too_many_lines)]
    fn convert_command(&mut self, node: &Node, parent: El, font: Option<Font>) -> Result<()> {
        let command = node.token.as_str();
        let modifier = node.modifier.as_deref();
        let mut parent = parent;

        if command == SUBSTACK || command == SMALLMATRIX {
            parent = self.tree.sub(parent, "mstyle", &[("scriptlevel", "1")]);
        } else if command == CASES {
            parent = self.tree.sub(parent, "mrow", &[]);
            let lbrace = self.tree.sub(
                parent,
                "mo",
                &[("stretchy", "true"), ("fence", "true"), ("form", "prefix")],
            );
            self.tree.set_text(lbrace, &entity(convert_symbol(LBRACE)));
        } else if command == DBINOM || command == DFRAC {
            parent = self.tree.sub(
                parent,
                "mstyle",
                &[("displaystyle", "true"), ("scriptlevel", "0")],
            );
        } else if command == HPHANTOM {
            parent = self
                .tree
                .sub(parent, "mpadded", &[("height", "0"), ("depth", "0")]);
        } else if command == VPHANTOM {
            parent = self.tree.sub(parent, "mpadded", &[("width", "0")]);
        } else if matches!(command, TBINOM | HBOX | MBOX | TFRAC) {
            parent = self.tree.sub(
                parent,
                "mstyle",
                &[("displaystyle", "false"), ("scriptlevel", "0")],
            );
        } else if matches!(command, MOD | PMOD | POD) {
            self.tree.sub(parent, "mspace", &[("width", "1em")]);
        }

        let (mut tag, mut attributes) =
            conversion_map(command).ok_or(Error::Runtime("KeyError"))?;
        if let (Some(a), false) = (&node.attributes, command == SKEW) {
            for (k, v) in a {
                attr_set(&mut attributes, k, v);
            }
        }
        if command == LEFT {
            parent = self.tree.sub(parent, "mrow", &[]);
        }
        self.append_delimiter_element(node, parent, true);

        let (mut alignment, column_lines) =
            get_alignment_and_column_lines(node.alignment.as_deref());
        if let Some(cl) = column_lines.filter(|c| !c.is_empty()) {
            attr_set(&mut attributes, "columnlines", &cl);
        }

        let kids = node.children.as_deref();
        if command == SUBSUP && kids.is_some_and(|k| k.first().is_some_and(|c| c.token == GCD)) {
            tag = "munderover";
        } else if command == SUPERSCRIPT && matches!(modifier, Some(LIMITS | OVERBRACE)) {
            tag = "mover";
        } else if command == SUBSCRIPT && matches!(modifier, Some(LIMITS | UNDERBRACE)) {
            tag = "munder";
        } else if (command == SUBSUP && matches!(modifier, Some(LIMITS | OVERBRACE | UNDERBRACE)))
            || (extensible_arrow(command).is_some() && kids.is_some_and(|k| k.len() == 2))
        {
            tag = "munderover";
        }

        let element = self.tree.sub_attrs(parent, tag, attributes);

        if LIMIT.contains(&command) {
            self.tree.set_text(element, &command[1..]);
        } else if command == MOD || command == PMOD {
            self.tree.set_text(element, "mod");
            self.tree.sub(parent, "mspace", &[("width", "0.333em")]);
        } else if command == POD {
        } else if command == BMOD {
            self.tree.set_text(element, "mod");
        } else if let Some(arrow) = extensible_arrow(command) {
            let style = self.tree.sub(element, "mstyle", &[("scriptlevel", "0")]);
            let mo = self.tree.sub(style, "mo", &[]);
            self.tree.set_text(mo, arrow);
        } else if command == BRA || command == BRAKET {
            let mo = self.tree.sub(element, "mo", &[("stretchy", "false")]);
            self.tree.set_text(mo, "&#x27E8;");
        } else if command == KET {
            let mo = self.tree.sub(element, "mo", &[]);
            self.tree.set_text(mo, "&#x2223;");
        } else if let Some(text) = &node.text {
            if command == MIDDLE {
                self.tree.set_text(element, &entity(convert_symbol(text)));
            } else if command == HBOX {
                let mut mtext: Option<El> = Some(element);
                let template = self.tree.nodes[element].attrib.clone();
                for (piece, math_mode) in separate_by_mode(text) {
                    if !math_mode {
                        let el = match mtext {
                            Some(el) => el,
                            None => self.tree.sub_attrs(parent, tag, template.clone()),
                        };
                        self.tree.set_text(el, &nbsp(&piece));
                        self.set_font(el, "mtext", font);
                        mtext = None;
                    } else {
                        let row = self.tree.sub(parent, "mrow", &[]);
                        let nodes = Walker::walk(&piece, false, &mut self.macros)?;
                        self.convert_group(&nodes, row, None)?;
                    }
                }
            } else {
                let mut element = element;
                if matches!(command, FBOX | LLAP | RLAP | CLAP | COLORBOX | FCOLORBOX) {
                    element = self.tree.sub(element, "mtext", &[]);
                }
                if command == TAG {
                    self.tree.set_text(element, &format!("({text})"));
                } else if command == TAGSTAR {
                    self.tree.set_text(element, text);
                } else {
                    self.tree.set_text(element, &nbsp(text));
                }
                self.set_font(element, "mtext", font);
            }
        } else if let (Some(delim), false) =
            (&node.delimiter, command == FRAC || command == GENFRAC)
        {
            if delim != "." {
                let text = match convert_symbol(delim) {
                    None => delim.clone(),
                    Some(sym) => format!("&#x{sym};"),
                };
                self.tree.set_text(element, &text);
            }
        }

        if let Some(kids) = kids {
            let target = if matches!(command, LEFT | MOD | PMOD | POD) {
                parent
            } else {
                element
            };
            if MATRICES.contains(&command) {
                if command == CASES {
                    alignment = Some("l".to_string());
                } else if matches!(command, SPLIT | ALIGN | ALIGNSTAR) {
                    alignment = Some("rl".to_string());
                }
                self.convert_matrix(kids, target, command, alignment.as_deref())?;
            } else if command == CFRAC {
                for child in kids {
                    let p = self.tree.sub(
                        target,
                        "mstyle",
                        &[("displaystyle", "false"), ("scriptlevel", "0")],
                    );
                    self.convert_group(std::slice::from_ref(child), p, font)?;
                }
            } else if command == SIDESET {
                if kids.len() != 2 {
                    return Err(Error::Runtime("ValueError"));
                }
                self.convert_group(std::slice::from_ref(&kids[0]), target, font)?;
                let fill = self.tree.sub(target, "mstyle", &[("scriptlevel", "0")]);
                self.tree.sub(fill, "mspace", &[("width", "-0.167em")]);
                self.convert_group(std::slice::from_ref(&kids[1]), target, font)?;
            } else if command == SKEW {
                let child = kids.first().ok_or(Error::Runtime("IndexError"))?;
                let mut inner = child.children.clone().ok_or(Error::Runtime("TypeError"))?;
                inner.push(Node {
                    attributes: node.attributes.clone(),
                    ..Node::new(MKERN)
                });
                let new_node =
                    Node::with_children(&child.token, vec![Node::with_children(BRACES, inner)]);
                self.convert_group(std::slice::from_ref(&new_node), target, font)?;
            } else if extensible_arrow(command).is_some() {
                for child in kids {
                    let padded = self.tree.sub(
                        target,
                        "mpadded",
                        &[
                            ("width", "+0.833em"),
                            ("lspace", "0.556em"),
                            ("voffset", "-.2em"),
                            ("height", "-.2em"),
                        ],
                    );
                    self.convert_group(std::slice::from_ref(child), padded, font)?;
                    self.tree.sub(padded, "mspace", &[("depth", ".25em")]);
                }
            } else {
                self.convert_group(kids, target, font)?;
            }
        }

        if let Some((text, dattrs)) = diacritic(command) {
            let mo = self.tree.sub_attrs(element, "mo", dattrs);
            self.tree.set_text(mo, text);
        }

        if command == BRA {
            let mo = self.tree.sub(element, "mo", &[]);
            self.tree.set_text(mo, "&#x2223;");
        } else if command == KET || command == BRAKET {
            let mo = self.tree.sub(element, "mo", &[("stretchy", "false")]);
            self.tree.set_text(mo, "&#x27E9;");
        }

        self.append_delimiter_element(node, parent, false);
        Ok(())
    }

    /// `_append_delimiter_element`.
    fn append_delimiter_element(&mut self, node: &Node, parent: El, is_prefix: bool) {
        let t = node.token.as_str();
        let size = if self.tree.get_attr(parent, "displaystyle") == Some("false") || t == TBINOM {
            "1.2em"
        } else {
            "2.047em"
        };
        let paren = if is_prefix { r"\lparen" } else { r"\rparen" };
        if matches!(t, r"\pmatrix" | PMOD | POD) {
            self.convert_and_append_command(paren, parent, None);
        } else if matches!(t, BINOM | DBINOM | TBINOM) {
            self.convert_and_append_command(
                paren,
                parent,
                Some(attrs(&[("minsize", size), ("maxsize", size)])),
            );
        } else if t == r"\bmatrix" {
            self.convert_and_append_command(
                if is_prefix { r"\lbrack" } else { r"\rbrack" },
                parent,
                None,
            );
        } else if t == r"\Bmatrix" {
            self.convert_and_append_command(
                if is_prefix { r"\lbrace" } else { r"\rbrace" },
                parent,
                None,
            );
        } else if t == r"\vmatrix" {
            self.convert_and_append_command(r"\vert", parent, None);
        } else if t == r"\Vmatrix" {
            self.convert_and_append_command(r"\Vert", parent, None);
        } else if let (true, Some(delim)) = (t == FRAC || t == GENFRAC, &node.delimiter) {
            // `node.delimiter[index]` — a code point, not a byte.
            let d = delim.chars().nth(usize::from(!is_prefix));
            if let Some(d) = d.filter(|&d| d != '.') {
                self.convert_and_append_command(
                    &d.to_string(),
                    parent,
                    Some(attrs(&[("minsize", size), ("maxsize", size)])),
                );
            }
        } else if let (false, true, Some(a)) = (is_prefix, t == SKEW, &node.attributes) {
            let width = attr_get(a, "width").unwrap_or_default();
            self.tree
                .sub(parent, "mspace", &[("width", &format!("-{width}"))]);
        }
    }

    /// `_convert_symbol`.
    #[allow(clippy::too_many_lines)]
    fn convert_symbol(&mut self, node: &Node, parent: El, font: Option<Font>) -> Result<()> {
        let token = node.token.as_str();
        let attributes = node.attributes.clone().unwrap_or_default();
        let symbol = convert_symbol(token);
        if token == MULTIPRIMES {
            let count = py_int_lenient(node.text.as_deref().unwrap_or("0"))?.max(0) as usize;
            let element = self.tree.sub_attrs(parent, "mi", attributes);
            self.tree.set_text(element, &"&#x02032;".repeat(count));
            return Ok(());
        }
        let symbol_cp = symbol.and_then(|s| u32::from_str_radix(s, 16).ok());
        if token.chars().next().is_some_and(is_digit) {
            let element = self.tree.sub_attrs(parent, "mn", attributes);
            self.tree.set_text(element, token);
            self.set_font(element, "mn", font);
        } else if is_operator(token) {
            let element = self.tree.sub_attrs(parent, "mo", attributes);
            let text = match symbol {
                None => token.to_string(),
                Some(s) => format!("&#x{s};"),
            };
            self.tree.set_text(element, &text);
            if token == r"\|" {
                self.tree.set_attr(element, "fence", "false");
            }
            if token == r"\smallint" {
                self.tree.set_attr(element, "largeop", "false");
            }
            if matches!(
                token,
                "(" | ")" | "[" | "]" | "|" | r"\|" | r"\{" | r"\}" | r"\surd"
            ) {
                self.tree.set_attr(element, "stretchy", "false");
                self.set_font(element, "fence", font);
            } else {
                self.set_font(element, "mo", font);
            }
        } else if symbol_cp
            .is_some_and(|cp| (0x2200..=0x22FF).contains(&cp) || (0x2190..=0x21FF).contains(&cp))
        {
            let element = self.tree.sub_attrs(parent, "mo", attributes);
            self.tree.set_text(element, &entity(symbol));
            self.set_font(element, "mo", font);
        } else if matches!(token, r"\ " | "~" | NOBREAKSPACE | SPACE) {
            let element = self.tree.sub_attrs(parent, "mtext", attributes);
            self.tree.set_text(element, "&#x000A0;");
            self.set_font(element, "mtext", font);
        } else if token == NOT {
            let mpadded = self.tree.sub(parent, "mpadded", &[("width", "0")]);
            let element = self.tree.sub(mpadded, "mtext", &[]);
            self.tree.set_text(element, "&#x029F8;");
        } else if let Some(text) = movable_limit_text(token) {
            let mut a = attrs(&[("movablelimits", "true")]);
            for (k, v) in &attributes {
                attr_set(&mut a, k, v);
            }
            let element = self.tree.sub_attrs(parent, "mo", a);
            self.tree.set_text(element, text);
            self.set_font(element, "mo", font);
        } else if token == MATHSTRUT || token == STRUT {
            let mpadded = self.tree.sub(parent, "mpadded", &[("width", "0px")]);
            let mphantom = self.tree.sub(mpadded, "mphantom", &[]);
            let mo = self.tree.sub(mphantom, "mo", &[("stretchy", "false")]);
            self.tree.set_text(mo, "&#x00028;");
        } else if token == IDOTSINT {
            let row = self.tree.sub_attrs(parent, "mrow", attributes);
            for s in ["&#x0222B;", "&#x022EF;", "&#x0222B;"] {
                let mo = self.tree.sub(row, "mo", &[]);
                self.tree.set_text(mo, s);
            }
        } else if token == LATEX || token == TEX {
            let row = self.tree.sub_attrs(parent, "mrow", attributes);
            if token == LATEX {
                let mi_l = self.tree.sub(row, "mi", &[]);
                self.tree.set_text(mi_l, "L");
                self.tree.sub(row, "mspace", &[("width", "-.325em")]);
                let mpadded = self.tree.sub(
                    row,
                    "mpadded",
                    &[
                        ("height", "+.21ex"),
                        ("depth", "-.21ex"),
                        ("voffset", "+.21ex"),
                    ],
                );
                let mstyle = self.tree.sub(
                    mpadded,
                    "mstyle",
                    &[("displaystyle", "false"), ("scriptlevel", "1")],
                );
                let mrow = self.tree.sub(mstyle, "mrow", &[]);
                let mi_a = self.tree.sub(mrow, "mi", &[]);
                self.tree.set_text(mi_a, "A");
                self.tree.sub(row, "mspace", &[("width", "-.17em")]);
                self.set_font(mi_l, "mi", font);
                self.set_font(mi_a, "mi", font);
            }
            let mi_t = self.tree.sub(row, "mi", &[]);
            self.tree.set_text(mi_t, "T");
            self.tree.sub(row, "mspace", &[("width", "-.14em")]);
            let mpadded = self.tree.sub(
                row,
                "mpadded",
                &[
                    ("height", "-.5ex"),
                    ("depth", "+.5ex"),
                    ("voffset", "-.5ex"),
                ],
            );
            let mrow = self.tree.sub(mpadded, "mrow", &[]);
            let mi_e = self.tree.sub(mrow, "mi", &[]);
            self.tree.set_text(mi_e, "E");
            self.tree.sub(row, "mspace", &[("width", "-.115em")]);
            let mi_x = self.tree.sub(row, "mi", &[]);
            self.tree.set_text(mi_x, "X");
            self.set_font(mi_t, "mi", font);
            self.set_font(mi_e, "mi", font);
            self.set_font(mi_x, "mi", font);
        } else if token.starts_with(OPERATORNAME) {
            for prefix in [OPERATORNAMEWITHLIMITS, OPERATORNAMESTAR, OPERATORNAME] {
                if token.starts_with(prefix) {
                    let a = if prefix == OPERATORNAME {
                        attributes.clone()
                    } else {
                        let mut a = attrs(&[("movablelimits", "true")]);
                        for (k, v) in &attributes {
                            attr_set(&mut a, k, v);
                        }
                        a
                    };
                    let element = self.tree.sub_attrs(parent, "mo", a);
                    self.tree
                        .set_text(element, &py_slice(token, prefix.len() + 1, -1));
                    break;
                }
            }
        } else if let Some(name) = token.strip_prefix('\\') {
            let element = self.tree.sub_attrs(parent, "mi", attributes);
            let text = if let Some(s) = symbol {
                format!("&#x{s};")
            } else if FUNCTIONS.contains(&token) {
                name.to_string()
            } else {
                token.to_string()
            };
            self.tree.set_text(element, &text);
            self.set_font(element, "mi", font);
        } else {
            let element = self.tree.sub_attrs(parent, "mi", attributes);
            self.tree.set_text(element, token);
            self.set_font(element, "mi", font);
        }
        Ok(())
    }

    /// `_set_font`.
    fn set_font(&mut self, element: El, key: &str, font: Option<Font>) {
        if let Some(v) = font.and_then(|f| f.get(key)) {
            self.tree.set_attr(element, "mathvariant", v);
        }
    }

    /// `_set_cell_alignment`.
    fn set_cell_alignment(&mut self, cell: El, hfil: &[bool]) {
        if hfil.iter().any(|&h| h) && hfil.len() > 1 {
            let (first, last) = (hfil[0], hfil[hfil.len() - 1]);
            if first && !last {
                self.tree.set_attr(cell, "columnalign", "right");
            } else if !first && last {
                self.tree.set_attr(cell, "columnalign", "left");
            }
        }
    }

    /// `_make_matrix_cell`.
    fn make_matrix_cell(&mut self, row: El, column_alignment: Option<&str>) -> El {
        match column_alignment {
            Some(a) => self.tree.sub(row, "mtd", &[("columnalign", a)]),
            None => self.tree.sub(row, "mtd", &[]),
        }
    }

    /// `_convert_and_append_command`.
    fn convert_and_append_command(&mut self, command: &str, parent: El, attributes: Option<Attrs>) {
        let mo = self
            .tree
            .sub_attrs(parent, "mo", attributes.unwrap_or_default());
        let text = match convert_symbol(command) {
            Some(cp) => format!("&#x{cp};"),
            None => command.to_string(),
        };
        self.tree.set_text(mo, &text);
    }
}

/// `s[start:-1]` on code points, Python's clamping included.
fn py_slice(s: &str, start: usize, end: i64) -> String {
    let cs: Vec<char> = s.chars().collect();
    let n = cs.len() as i64;
    let e = if end < 0 {
        (n + end).max(0)
    } else {
        end.min(n)
    } as usize;
    let st = start.min(cs.len());
    if st >= e {
        return String::new();
    }
    cs[st..e].iter().collect()
}

/// `_get_column_alignment`.
fn get_column_alignment(
    alignment: Option<&str>,
    column_alignment: Option<&'static str>,
    column_index: usize,
) -> (Option<&'static str>, usize) {
    match alignment {
        Some(a) if !a.is_empty() => {
            let cs: Vec<char> = a.chars().collect();
            let c = cs[column_index % cs.len()];
            (column_alignment_of(c), column_index + 1)
        }
        _ => (column_alignment, column_index),
    }
}

fn column_alignment_of(c: char) -> Option<&'static str> {
    column_alignment(c)
}

/// `_get_alignment_and_column_lines`.
fn get_alignment_and_column_lines(alignment: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(alignment) = alignment else {
        return (None, None);
    };
    if !alignment.contains('|') {
        return (Some(alignment.to_string()), None);
    }
    let mut a = String::new();
    let mut a_len = 0usize;
    let mut column_lines: Vec<&str> = Vec::new();
    for c in alignment.chars() {
        if c == '|' {
            column_lines.push("solid");
        } else {
            a.push(c);
            a_len += 1;
        }
        if a_len as i64 - column_lines.len() as i64 == 2 {
            column_lines.push("none");
        }
    }
    (Some(a), Some(column_lines.join(" ")))
}

/// `_separate_by_mode`: `(piece, is_math_mode)` runs of an `\hbox` text,
/// split on unescaped `$`.
fn separate_by_mode(text: &str) -> Vec<(String, bool)> {
    let cs: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut string = String::new();
    let mut is_math = false;
    let mut i = 0;
    while i < cs.len() {
        // MATH_MODE_PATTERN = \\\$|\$|\\?[^\\$]+
        if cs[i] == '\\' && cs.get(i + 1) == Some(&'$') {
            string.push_str("\\$");
            i += 2;
        } else if cs[i] == '$' {
            out.push((std::mem::take(&mut string), is_math));
            is_math = !is_math;
            i += 1;
        } else {
            let start = i;
            if cs[i] == '\\' {
                i += 1;
            }
            let j = skip(&cs, i, |c| c != '\\' && c != '$');
            if j == i {
                // A backslash not followed by a plain char: no match, skip it.
                i = start + 1;
                continue;
            }
            string.extend(&cs[start..j]);
            i = j;
        }
    }
    if !string.is_empty() {
        out.push((string, is_math));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(latex: &str) -> String {
        formula_to_mathml(latex, true).unwrap()
    }

    #[test]
    fn simple_expression() {
        assert_eq!(
            conv("x = 1"),
            "<math xmlns=\"http://www.w3.org/1998/Math/MathML\" display=\"inline\"><mrow><mi>x</mi><mo>&#x0003D;</mo><mn>1</mn></mrow><annotation encoding=\"TeX\">x = 1</annotation></math>"
        );
    }

    #[test]
    fn block_wraps_in_div_and_fraction() {
        assert_eq!(
            formula_to_mathml(r"\frac{a}{b}", false).unwrap(),
            "<div><math xmlns=\"http://www.w3.org/1998/Math/MathML\" display=\"block\"><mrow><mfrac><mrow><mi>a</mi></mrow><mrow><mi>b</mi></mrow></mfrac></mrow><annotation encoding=\"TeX\">\\frac{a}{b}</annotation></math></div>"
        );
    }

    #[test]
    fn sum_limits_only_in_block_mode() {
        assert!(conv(r"\sum_{i=1}^n i").contains("<msubsup>"));
        assert!(formula_to_mathml(r"\sum_{i=1}^n i", false)
            .unwrap()
            .contains("<munderover>"));
    }

    #[test]
    fn errors_match_upstream() {
        assert_eq!(
            formula_to_mathml(r"\left( x", true),
            Err(Error::ExtraLeftOrMissingRight)
        );
        assert_eq!(
            formula_to_mathml("x_a_b", true),
            Err(Error::DoubleSubscripts)
        );
        assert_eq!(
            formula_to_mathml(r"\sqrt", true),
            Err(Error::NoAvailableTokens)
        );
        assert!(formula_to_mathml(r"\begin{matrix} a", true).is_err());
    }

    #[test]
    fn empty_operands_and_primes() {
        assert!(conv("^2").contains("<msup><mi /><mn>2</mn></msup>"));
        assert!(conv("f''").contains("<msup><mi>f</mi><mi>&#x02033;</mi></msup>"));
    }

    #[test]
    fn matrix_environment() {
        let out =
            formula_to_mathml(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}", false).unwrap();
        assert!(out.contains(
            "<mo>&#x00028;</mo><mtable><mtr><mtd><mi>a</mi></mtd><mtd><mi>b</mi></mtd></mtr>"
        ));
    }

    #[test]
    fn text_and_attribute_escaping() {
        assert!(conv(r#"\text{a "b"}"#).contains("<mtext>a&#x000A0;\"b\"</mtext>"));
        assert!(conv(r#"\class{x"y}{a}"#).contains("class=\"x&quot;y\""));
        // latex2mathml's own character references are not double-escaped…
        assert!(conv("a < b").contains("<mo>&#x0003C;</mo>"));
        // …but markup in text is (docling-core#894).
        let m = conv(r"\text{<svg/onload=alert(1)>}");
        assert!(
            m.contains("<mtext>&lt;svg/onload=alert(1)&gt;</mtext>"),
            "{m}"
        );
        assert!(
            m.contains(
                "<annotation encoding=\"TeX\">\\text{&lt;svg/onload=alert(1)&gt;}</annotation>"
            ),
            "{m}"
        );
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        // Debug frames of the walker are large; the HTML serializer runs
        // this on its 256 MB thread, so the test does likewise.
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let deep = "{".repeat(MAX_NESTING - 5) + "x" + &"}".repeat(MAX_NESTING - 5);
                assert!(formula_to_mathml(&deep, true).is_ok());
                let deeper = "{".repeat(100_000) + "x";
                assert_eq!(
                    formula_to_mathml(&deeper, true),
                    Err(Error::Runtime("RecursionError"))
                );
                // `^{` costs two walker levels (the script's limit-1 walk
                // and the brace group).
                let n = MAX_NESTING / 2 - 5;
                let sup = "x".to_string() + &"^{".repeat(n) + "y" + &"}".repeat(n);
                assert!(formula_to_mathml(&sup, false).is_ok());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn tokenizer_rules() {
        assert_eq!(tokenize(r"\frac12").unwrap(), vec![r"\frac", "1", "2"]);
        assert_eq!(tokenize(r"x_2^3").unwrap(), vec!["x", "_", "2", "^", "3"]);
        assert_eq!(tokenize("12.5 em a").unwrap(), vec!["12.5em", "a"]);
        assert_eq!(tokenize(r"\text{a b}").unwrap(), vec![r"\text", "a b"]);
        assert_eq!(tokenize("a % comment\nb").unwrap(), vec!["a", "b"]);
        assert_eq!(tokenize(r"\mathbb{R}").unwrap(), vec!["&#x0211D;"]);
        assert_eq!(
            tokenize(r"\mathbf{ab}").unwrap(),
            vec![r"\mathbf", "{", "a", "b", "}"]
        );
        assert_eq!(
            tokenize(r"\begin {matrix} \end{matrix}").unwrap(),
            vec![r"\begin{matrix}", r"\end{matrix}"]
        );
        assert_eq!(tokenize(r"\verb|x y|").unwrap(), vec![r"\verb", "x y"]);
        assert!(tokenize(r"\verbatim").is_err());
    }
}
