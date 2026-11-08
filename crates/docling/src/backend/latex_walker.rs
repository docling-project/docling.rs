//! A port of pylatexenc 2.11's `LatexWalker` in its tolerant mode — the
//! parser docling's `LatexDocumentBackend` reads a `.tex` source with (#466).
//!
//! docling's output is a function of pylatexenc's node stream far more than
//! of LaTeX semantics: which macros take arguments (the walker's default
//! specs, so `\paragraph{…}` — unknown to pylatexenc — has its title as a
//! sibling group), where a chars node ends, what a comment swallows, how a
//! mismatched `\end` or brace is skipped. Every rule here mirrors the Python
//! source (`latexwalker/__init__.py`, `macrospec/_argparsers.py`,
//! `latexwalker/_defaultspecs.py`), quirks included, so the handler port in
//! [`super::latex`] sees the same nodes upstream sees. Offsets are byte
//! offsets into the source; a node's `latex_verbatim()` is its `pos..pos + len`
//! slice of it.

use std::cell::Cell;
use std::collections::HashSet;

/// A parsed node: pylatexenc's `LatexNode` subclasses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Node {
    pub kind: Kind,
    pub pos: usize,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    /// `LatexCharsNode`.
    Chars(String),
    /// `LatexGroupNode`: `{…}` (or `[…]` as an optional argument).
    Group {
        nodelist: Vec<Node>,
        delims: (char, char),
    },
    /// `LatexCommentNode`: the text after `%` and the whitespace that follows
    /// the line break.
    Comment { text: String, post_space: String },
    /// `LatexMacroNode`. `args` is `None` where pylatexenc leaves `nodeargd`
    /// unset (a macro read as a bare expression, or whose arguments failed to
    /// parse).
    Macro {
        name: String,
        args: Option<Args>,
        post_space: String,
    },
    /// `LatexEnvironmentNode`.
    Env {
        name: String,
        nodelist: Vec<Node>,
        args: Option<Args>,
    },
    /// `LatexSpecialsNode`: `&`, `~`, ``` `` ```, `''`, `--`, `---`, `` !` ``, `` ?` ``.
    Specials(String),
    /// `LatexMathNode`.
    Math {
        display: bool,
        nodelist: Vec<Node>,
        delims: (String, String),
    },
}

/// pylatexenc's `ParsedMacroArgs`: one slot per `argspec` character, `None`
/// where an optional argument or star was absent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Args {
    pub argspec: String,
    pub argnlist: Vec<Option<Node>>,
}

// ---------------------------------------------------------------------------
// Specs (`_defaultspecs.py`)

/// The macros pylatexenc knows, with their argument specification (`*` a
/// possible star, `[` an optional argument, `{` a mandatory one). Anything
/// else takes no arguments.
const MACRO_SPECS: &[(&str, &str)] = &[
    ("documentclass", "[{"),
    ("usepackage", "[{"),
    ("RequirePackage", "[{"),
    ("selectlanguage", "[{"),
    ("setlength", "[{{"),
    ("addlength", "[{{"),
    ("setcounter", "[{{"),
    ("addcounter", "[{{"),
    ("newcommand", "*{[[{"),
    ("renewcommand", "*{[[{"),
    ("providecommand", "*{[[{"),
    ("newenvironment", "*{[[{{"),
    ("renewenvironment", "*{[[{{"),
    ("provideenvironment", "*{[[{{"),
    ("DeclareMathOperator", "*{{"),
    ("hspace", "*{"),
    ("vspace", "*{"),
    ("mbox", "{"),
    ("title", "{"),
    ("author", "{"),
    ("date", "{"),
    ("\\", "*["),
    ("item", "["),
    ("input", "{"),
    ("include", "{"),
    ("includegraphics", "[{"),
    ("chapter", "*[{"),
    ("section", "*[{"),
    ("subsection", "*[{"),
    ("subsubsection", "*[{"),
    // pylatexenc's own typo: `\paragraph` has no spec, `\pagagraph` does.
    ("pagagraph", "*[{"),
    ("subparagraph", "*[{"),
    ("bibliography", "{"),
    ("emph", "{"),
    ("textrm", "{"),
    ("textit", "{"),
    ("textbf", "{"),
    ("textmd", "{"),
    ("textsc", "{"),
    ("textsf", "{"),
    ("textsl", "{"),
    ("texttt", "{"),
    ("textup", "{"),
    ("text", "{"),
    ("mathrm", "{"),
    ("mathbb", "{"),
    ("mathbf", "{"),
    ("mathit", "{"),
    ("mathsf", "{"),
    ("mathtt", "{"),
    ("mathcal", "{"),
    ("mathscr", "{"),
    ("mathfrak", "{"),
    ("label", "{"),
    ("ref", "{"),
    ("autoref", "{"),
    ("cref", "{"),
    ("Cref", "{"),
    ("eqref", "{"),
    ("url", "{"),
    // hyperref's `\href[options]{url}{text}` (docling#4600): parsed with its
    // arguments so they do not stay in the body as glued text.
    ("href", "[{{"),
    ("hypersetup", "{"),
    ("footnote", "[{"),
    ("keywords", "{"),
    ("hphantom", "[{"),
    ("vphantom", "[{"),
    ("'", "{"),
    ("`", "{"),
    ("\"", "{"),
    ("c", "{"),
    ("^", "{"),
    ("~", "{"),
    ("H", "{"),
    ("k", "{"),
    ("=", "{"),
    ("b", "{"),
    (".", "{"),
    ("d", "{"),
    ("r", "{"),
    ("u", "{"),
    ("v", "{"),
    ("ensuremath", "{"),
    ("not", "{"),
    ("vec", "{"),
    ("dot", "{"),
    ("hat", "{"),
    ("check", "{"),
    ("breve", "{"),
    ("acute", "{"),
    ("grave", "{"),
    ("tilde", "{"),
    ("bar", "{"),
    ("ddot", "{"),
    ("frac", "{{"),
    ("nicefrac", "{{"),
    ("sqrt", "[{"),
    ("overline", "{"),
    ("underline", "{"),
    ("widehat", "{"),
    ("widetilde", "{"),
    ("wideparen", "{"),
    ("overleftarrow", "{"),
    ("overrightarrow", "{"),
    ("overleftrightarrow", "{"),
    ("underleftarrow", "{"),
    ("underrightarrow", "{"),
    ("underleftrightarrow", "{"),
    ("overbrace", "{"),
    ("underbrace", "{"),
    ("overgroup", "{"),
    ("undergroup", "{"),
    ("overbracket", "{"),
    ("underbracket", "{"),
    ("overlinesegment", "{"),
    ("underlinesegment", "{"),
    ("overleftharpoon", "{"),
    ("overrightharpoon", "{"),
    ("xleftarrow", "[{"),
    ("xrightarrow", "[{"),
    ("ket", "{"),
    ("bra", "{"),
    ("braket", "{{"),
    ("ketbra", "{{"),
    ("texorpdfstring", "{{"),
    ("definecolor", "[{{{"),
    ("providecolor", "[{{{"),
    ("colorlet", "[{[{"),
    ("color", "[{"),
    ("textcolor", "[{{"),
    ("pagecolor", "[{"),
    ("nopagecolor", ""),
    ("colorbox", "[{{"),
    ("fcolorbox", "[{[{{"),
    ("boxframe", "{{{"),
    ("rowcolors", "*[{{{"),
    ("verb", "verb"),
    // natbib
    ("cite", "*[[{"),
    ("citet", "*[[{"),
    ("citep", "*[[{"),
    ("citealt", "*[[{"),
    ("citealp", "*[[{"),
    ("citeauthor", "*[[{"),
    ("citefullauthor", "[[{"),
    ("citeyear", "[[{"),
    ("citeyearpar", "[[{"),
    ("Citet", "*[[{"),
    ("Citep", "*[[{"),
    ("Citealt", "*[[{"),
    ("Citealp", "*[[{"),
    ("Citeauthor", "*[[{"),
    ("citetext", "{"),
    ("citenum", "{"),
    ("defcitealias", "{{"),
    ("citetalias", "[[{"),
    ("citepalias", "[[{"),
    // latex-ethuebung
    ("UebungLoesungFont", "{"),
    ("UebungHinweisFont", "{"),
    ("UebungExTitleFont", "{"),
    ("UebungSubExTitleFont", "{"),
    ("UebungTipsFont", "{"),
    ("UebungLabel", "{"),
    ("UebungSubLabel", "{"),
    ("UebungLabelEnum", "{"),
    ("UebungLabelEnumSub", "{"),
    ("UebungSolLabel", "{"),
    ("UebungHinweisLabel", "{"),
    ("UebungHinweiseLabel", "{"),
    ("UebungSolEquationLabel", "{"),
    ("UebungTipsLabel", "{"),
    ("UebungTipsEquationLabel", "{"),
    ("UebungsblattTitleSeries", "{"),
    ("UebungsblattTitleSolutions", "{"),
    ("UebungsblattTitleTips", "{"),
    ("UebungsblattNumber", "{"),
    ("UebungsblattTitleFont", "{"),
    ("UebungTitleCenterVSpacing", "{"),
    ("UebungAttachedSolutionTitleTop", "{"),
    ("UebungAttachedSolutionTitleFont", "{"),
    ("UebungAttachedSolutionTitle", "{"),
    ("UebungTextAttachedSolution", "{"),
    ("UebungDueByLabel", "{"),
    ("UebungDueBy", "{"),
    ("UebungLecture", "{"),
    ("UebungProf", "{"),
    ("UebungLecturer", "{"),
    ("UebungSemester", "{"),
    ("UebungLogoFile", "{"),
    ("UebungLanguage", "{"),
    ("UebungStyle", "{"),
    ("uebung", "{["),
    ("exercise", "{["),
    ("subuebung", "{"),
    ("subexercise", "{"),
    ("pdfloesung", "[{"),
    ("pdfsolution", "[{"),
    ("exenumfulllabel", "{"),
    ("hint", "{"),
    ("hints", "{"),
    ("hinweis", "{"),
    ("hinweise", "{"),
];

/// Macros whose one argument is parsed in text mode even inside math
/// (`args_math_mode=[False]`), `\ensuremath` the reverse.
const TEXT_MODE_ARG: &[&str] = &[
    "mbox", "textrm", "textit", "textbf", "textmd", "textsc", "textsf", "textsl", "texttt",
    "textup", "text",
];

/// (name, argspec, is_math_mode) for the environments pylatexenc knows.
const ENV_SPECS: &[(&str, &str, bool)] = &[
    ("figure", "[", false),
    ("figure*", "[", false),
    ("table", "[", false),
    ("table*", "[", false),
    ("abstract", "", false),
    ("tabular", "{", false),
    ("tabular*", "{{", false),
    ("tabularx", "{[{", false),
    // docling's own addition to pylatexenc's context (`latex_context.py`,
    // docling#4325): without it a longtable's column specification would be
    // read as its first cell.
    ("longtable", "[{", false),
    ("array", "[{", false),
    ("equation", "", true),
    ("equation*", "", true),
    ("eqnarray", "", true),
    ("eqnarray*", "", true),
    ("align", "", true),
    ("align*", "", true),
    ("gather", "", true),
    ("gather*", "", true),
    ("flalign", "", true),
    ("flalign*", "", true),
    ("multline", "", true),
    ("multline*", "", true),
    ("alignat", "{", true),
    ("alignat*", "{", true),
    ("split", "", true),
    ("verbatim", "verbatim", false),
    ("theorem", "[", false),
    ("proposition", "[", false),
    ("lemma", "[", false),
    ("corollary", "[", false),
    ("definition", "[", false),
    ("conjecture", "[", false),
    ("remark", "[", false),
    ("proof", "[", false),
    ("thm", "[", false),
    ("prop", "[", false),
    ("lem", "[", false),
    ("cor", "[", false),
    ("conj", "[", false),
    ("rem", "[", false),
    ("defn", "[", false),
    ("enumerate", "[", false),
    ("itemize", "[", false),
    ("description", "[", false),
];

/// The "latex specials", longest match first.
const SPECIALS: &[&str] = &["---", "``", "''", "--", "!`", "?`", "&", "~"];

/// docling's `docling-metadata-and-bibliography` context category
/// (`utils/latex_context.py`, docling#4817), prepended to pylatexenc's
/// defaults: front-matter commands, `\caption` / `\captionof`, `\bibitem`
/// and `\parbox` take their arguments, so their text reaches the handlers as
/// the macro's argument instead of a sibling group.
const DOCLING_MACRO_SPECS: &[(&str, &str)] = &[
    ("title", "[{"),
    ("affiliation", "{"),
    ("affil", "{"),
    ("address", "{"),
    ("institute", "{"),
    ("email", "{"),
    ("altaffiliation", "[{"),
    ("thanks", "{"),
    ("inst", "{"),
    ("preprint", "{"),
    ("parbox", "[{{"),
    ("caption", "*[{"),
    ("captionof", "{{"),
    ("bibitem", "[{"),
    ("bibliographystyle", "{"),
    ("newtheorem", "*{[{["),
];

fn macro_argspec(name: &str) -> &'static str {
    DOCLING_MACRO_SPECS
        .iter()
        .chain(MACRO_SPECS)
        .find(|(n, _)| *n == name)
        .map_or("", |(_, spec)| spec)
}

fn env_spec(name: &str) -> (&'static str, bool) {
    if name == "thebibliography" {
        // docling's context: the widest-label argument, `{99}`.
        return ("{", false);
    }
    ENV_SPECS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map_or(("", false), |(_, spec, math)| (spec, *math))
}

// ---------------------------------------------------------------------------
// Tokens

#[derive(Debug, Clone, PartialEq)]
enum Tok<'s> {
    Char(&'s str),
    Macro(&'s str),
    Comment(&'s str),
    BraceOpen(char),
    BraceClose(char),
    /// `$`, `\(`, `\)`.
    MathInline(&'s str),
    /// `$$`, `\[`, `\]`.
    MathDisplay(&'s str),
    BeginEnv(&'s str),
    EndEnv(&'s str),
    Specials(&'static str),
}

#[derive(Debug, Clone)]
struct Token<'s> {
    tok: Tok<'s>,
    pos: usize,
    len: usize,
    pre_space: &'s str,
    post_space: &'s str,
}

/// pylatexenc's `LatexWalkerEndOfStream` (with the trailing whitespace it
/// carries) and `LatexWalkerParseError`, which tolerant parsing reports and
/// ignores wherever the Python does.
#[derive(Debug)]
enum Fail<'s> {
    EndOfStream(&'s str),
    Parse,
}

#[derive(Debug, Clone, Copy)]
struct State<'s> {
    in_math_mode: bool,
    math_mode_delimiter: Option<&'s str>,
}

/// What a nested `get_latex_nodes` stops at.
#[derive(Debug, Clone, Copy)]
enum Stop<'s> {
    None,
    Brace(char, char),
    EndEnv(&'s str),
    MathMode(&'s str),
}

/// Nesting past which the walker stops descending — pylatexenc recurses
/// without limit; a crafted source must not overflow the stack here.
const MAX_DEPTH: usize = 400;

pub(crate) struct Walker<'s> {
    pub s: &'s str,
    depth: Cell<usize>,
    /// Macros the document defines without arguments, registered ahead of
    /// every other spec (docling's `docling-source-macros` category) so a
    /// source definition of a built-in name — `\def\b{…}` — does not take an
    /// argument the main parse would otherwise read.
    zero_arg: Option<&'s HashSet<String>>,
}

impl<'s> Walker<'s> {
    pub fn new(s: &'s str) -> Self {
        Walker {
            s,
            depth: Cell::new(0),
            zero_arg: None,
        }
    }

    /// [`Walker::new`] with the document's argument-less macros prepended
    /// to the context.
    pub fn with_zero_arg_macros(s: &'s str, names: &'s HashSet<String>) -> Self {
        Walker {
            s,
            depth: Cell::new(0),
            zero_arg: (!names.is_empty()).then_some(names),
        }
    }

    /// `LatexWalker(s, tolerant_parsing=True).get_latex_nodes()`.
    pub fn parse(&self) -> Vec<Node> {
        let state = State {
            in_math_mode: false,
            math_mode_delimiter: None,
        };
        self.get_latex_nodes(0, Stop::None, state).0
    }

    fn ch(&self, pos: usize) -> Option<char> {
        self.s.get(pos..)?.chars().next()
    }

    fn is_space_at(&self, pos: usize) -> bool {
        self.ch(pos).is_some_and(char::is_whitespace)
    }

    // -- get_token ------------------------------------------------------------

    fn get_token(
        &self,
        mut pos: usize,
        include_brace: Option<(char, char)>,
        environments: bool,
        state: State<'s>,
    ) -> Result<Token<'s>, Fail<'s>> {
        let s = self.s;
        let space_start = pos;
        while let Some(c) = self.ch(pos).filter(|c| c.is_whitespace()) {
            pos += c.len_utf8();
            if s[space_start..pos].ends_with("\n\n") {
                return Ok(Token {
                    tok: Tok::Char("\n\n"),
                    pos: pos - 2,
                    len: 2,
                    pre_space: &s[space_start..pos - 2],
                    post_space: "",
                });
            }
        }
        let space = &s[space_start..pos];
        let Some(c) = self.ch(pos) else {
            return Err(Fail::EndOfStream(space));
        };
        if c == '\\' {
            let Some(next) = self.ch(pos + 1) else {
                return Err(Fail::EndOfStream(""));
            };
            let mut i = 1 + next.len_utf8();
            let isalpha = next.is_alphabetic();
            if isalpha {
                while let Some(c) = self.ch(pos + i).filter(|c| c.is_alphabetic()) {
                    i += c.len_utf8();
                }
            }
            let macro_name = &s[pos + 1..pos + i];
            if macro_name == "[" || macro_name == "]" {
                return Ok(Token {
                    tok: Tok::MathDisplay(&s[pos..pos + i]),
                    pos,
                    len: i,
                    pre_space: space,
                    post_space: "",
                });
            }
            if macro_name == "(" || macro_name == ")" {
                return Ok(Token {
                    tok: Tok::MathInline(&s[pos..pos + i]),
                    pos,
                    len: i,
                    pre_space: space,
                    post_space: "",
                });
            }
            if environments && (macro_name == "begin" || macro_name == "end") {
                // `^\s*\{([\w* ._-]+)\}` after the macro name.
                match self.env_name_at(pos + i) {
                    Some((name, end)) => {
                        return Ok(Token {
                            tok: if macro_name == "begin" {
                                Tok::BeginEnv(name)
                            } else {
                                Tok::EndEnv(name)
                            },
                            pos,
                            len: end - pos,
                            pre_space: space,
                            post_space: "",
                        });
                    }
                    None => {
                        // Tolerant: a placeholder chars token holding `\begin`.
                        return Ok(Token {
                            tok: Tok::Char(&s[pos..pos + i]),
                            pos,
                            len: i,
                            pre_space: space,
                            post_space: "",
                        });
                    }
                }
            }
            let mut post_end = pos + i;
            if isalpha {
                // LaTeX swallows the space after an alphabetic macro — up to
                // a paragraph break, which stays in the stream.
                while let Some(c) = self.ch(post_end).filter(|c| c.is_whitespace()) {
                    post_end += c.len_utf8();
                    if s[pos + i..post_end].ends_with("\n\n") {
                        post_end -= 2;
                        break;
                    }
                }
            }
            return Ok(Token {
                tok: Tok::Macro(macro_name),
                pos,
                len: post_end - pos,
                pre_space: space,
                post_space: &s[pos + i..post_end],
            });
        }
        if c == '%' {
            // `(\n|\r|\n\r)(?P<extraspace>\s*)` searched from `pos`.
            let rest = &s[pos..];
            return Ok(match rest.find(['\n', '\r']) {
                Some(nl) => {
                    let m_start = pos + nl;
                    let mut m_end = m_start + 1;
                    while let Some(c) = self.ch(m_end).filter(|c| c.is_whitespace()) {
                        m_end += c.len_utf8();
                    }
                    let extraspace = &s[m_start + 1..m_end];
                    if extraspace.starts_with(['\n', '\r']) {
                        // A blank line follows: the paragraph break stays.
                        Token {
                            tok: Tok::Comment(&s[pos + 1..m_start]),
                            pos,
                            len: m_start - pos,
                            pre_space: space,
                            post_space: "",
                        }
                    } else {
                        Token {
                            tok: Tok::Comment(&s[pos + 1..m_start]),
                            pos,
                            len: m_end - pos,
                            pre_space: space,
                            post_space: &s[m_start..m_end],
                        }
                    }
                }
                None => Token {
                    tok: Tok::Comment(&s[pos + 1..]),
                    pos,
                    len: s.len() - pos,
                    pre_space: space,
                    post_space: "",
                },
            });
        }
        let mut braces = vec![('{', '}')];
        if let Some(pair) = include_brace {
            braces.push(pair);
        }
        if braces.iter().any(|(o, _)| *o == c) {
            return Ok(Token {
                tok: Tok::BraceOpen(c),
                pos,
                len: 1,
                pre_space: space,
                post_space: "",
            });
        }
        if braces.iter().any(|(_, cl)| *cl == c) {
            return Ok(Token {
                tok: Tok::BraceClose(c),
                pos,
                len: 1,
                pre_space: space,
                post_space: "",
            });
        }
        if s[pos..].starts_with("$$")
            && !(state.in_math_mode && state.math_mode_delimiter == Some("$"))
        {
            return Ok(Token {
                tok: Tok::MathDisplay("$$"),
                pos,
                len: 2,
                pre_space: space,
                post_space: "",
            });
        }
        if c == '$' {
            return Ok(Token {
                tok: Tok::MathInline("$"),
                pos,
                len: 1,
                pre_space: space,
                post_space: "",
            });
        }
        if let Some(sp) = SPECIALS.iter().find(|sp| s[pos..].starts_with(**sp)) {
            return Ok(Token {
                tok: Tok::Specials(sp),
                pos,
                len: sp.len(),
                pre_space: space,
                post_space: "",
            });
        }
        Ok(Token {
            tok: Tok::Char(&s[pos..pos + c.len_utf8()]),
            pos,
            len: c.len_utf8(),
            pre_space: space,
            post_space: "",
        })
    }

    /// `^\s*\{([\w* ._-]+)\}` at `pos`: the environment name and the offset
    /// past the closing brace.
    fn env_name_at(&self, mut pos: usize) -> Option<(&'s str, usize)> {
        while let Some(c) = self.ch(pos).filter(|c| c.is_whitespace()) {
            pos += c.len_utf8();
        }
        if self.ch(pos)? != '{' {
            return None;
        }
        let start = pos + 1;
        let mut end = start;
        while let Some(c) = self
            .ch(end)
            .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '*' | ' ' | '.' | '-'))
        {
            end += c.len_utf8();
        }
        if end == start || self.ch(end)? != '}' {
            return None;
        }
        Some((&self.s[start..end], end + 1))
    }

    // -- expressions and arguments -------------------------------------------

    /// `get_latex_expression`: one TeX "token" — a single char, a bare macro
    /// or specials, or a braced group.
    fn get_latex_expression(
        &self,
        pos: usize,
        state: State<'s>,
    ) -> Result<(Node, usize, usize), Fail<'s>> {
        let tok = self.get_token(pos, None, false, state)?;
        Ok(match tok.tok {
            Tok::Macro("end") => (mk(Kind::Chars(String::new()), tok.pos, 0), tok.pos, 0),
            Tok::Macro(name) => (
                mk(
                    Kind::Macro {
                        name: name.to_string(),
                        args: None,
                        post_space: tok.post_space.to_string(),
                    },
                    tok.pos,
                    tok.len,
                ),
                tok.pos,
                tok.len,
            ),
            Tok::Specials(sp) => (
                mk(Kind::Specials(sp.to_string()), tok.pos, tok.len),
                tok.pos,
                tok.len,
            ),
            Tok::Comment(_) => return self.get_latex_expression(tok.pos + tok.len, state),
            Tok::BraceOpen(c) => return self.get_latex_braced_group(tok.pos, c, state),
            Tok::BraceClose(_) => (mk(Kind::Chars(String::new()), tok.pos, 0), tok.pos, 0),
            Tok::Char(c) => (
                mk(Kind::Chars(c.to_string()), tok.pos, tok.len),
                tok.pos,
                tok.len,
            ),
            Tok::MathInline(arg) | Tok::MathDisplay(arg) => {
                let kind = if arg.starts_with('\\') {
                    Kind::Macro {
                        name: arg.to_string(),
                        args: None,
                        post_space: String::new(),
                    }
                } else {
                    Kind::Chars(arg.to_string())
                };
                (mk(kind, tok.pos, tok.len), tok.pos, tok.len)
            }
            // Unreachable with `environments=False`.
            Tok::BeginEnv(_) | Tok::EndEnv(_) => return Err(Fail::Parse),
        })
    }

    fn get_latex_maybe_optional_arg(
        &self,
        pos: usize,
        state: State<'s>,
    ) -> Result<Option<(Node, usize, usize)>, Fail<'s>> {
        let tok = match self.get_token(pos, Some(('[', ']')), false, state) {
            Ok(t) => t,
            Err(Fail::EndOfStream(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if tok.tok == Tok::BraceOpen('[') {
            return self.get_latex_braced_group(pos, '[', state).map(Some);
        }
        Ok(None)
    }

    fn get_latex_braced_group(
        &self,
        pos: usize,
        brace: char,
        state: State<'s>,
    ) -> Result<(Node, usize, usize), Fail<'s>> {
        let closing = match brace {
            '{' => '}',
            '[' => ']',
            '(' => ')',
            '<' => '>',
            _ => return Err(Fail::Parse),
        };
        let include = (brace != '{').then_some((brace, closing));
        let first = self.get_token(pos, include, true, state)?;
        if first.tok != Tok::BraceOpen(brace) {
            return Err(Fail::Parse);
        }
        let (nodelist, npos, nlen) =
            self.get_latex_nodes(first.pos + first.len, Stop::Brace(brace, closing), state);
        let len = npos + nlen - first.pos;
        Ok((
            mk(
                Kind::Group {
                    nodelist,
                    delims: (brace, closing),
                },
                first.pos,
                len,
            ),
            first.pos,
            len,
        ))
    }

    fn get_latex_environment(
        &self,
        pos: usize,
        state: State<'s>,
    ) -> Result<(Node, usize, usize), Fail<'s>> {
        let first = self.get_token(pos, None, true, state)?;
        let Tok::BeginEnv(name) = first.tok else {
            return Err(Fail::Parse);
        };
        let mut p = first.pos + first.len;
        let (argspec, is_math) = env_spec(name);
        // Arguments that fail to parse are dropped, the environment kept.
        let args = match self.parse_args(argspec, p, state, false, None) {
            Ok((args, apos, alen)) => {
                p = apos + alen;
                Some(args)
            }
            Err(_) => None,
        };
        let inner = if is_math {
            State {
                in_math_mode: true,
                // pylatexenc: `'{' + environmentname + '}'` — never `$`, which
                // is all the delimiter is ever compared against.
                math_mode_delimiter: Some("{env}"),
            }
        } else {
            state
        };
        let (nodelist, npos, nlen) = self.get_latex_nodes(p, Stop::EndEnv(name), inner);
        let len = npos + nlen - first.pos;
        Ok((
            mk(
                Kind::Env {
                    name: name.to_string(),
                    nodelist,
                    args,
                },
                first.pos,
                len,
            ),
            first.pos,
            len,
        ))
    }

    /// `MacroStandardArgsParser.parse_args` (and the two verbatim parsers).
    fn parse_args(
        &self,
        argspec: &str,
        pos: usize,
        state: State<'s>,
        optional_arg_no_space: bool,
        text_mode_args: Option<bool>,
    ) -> Result<(Args, usize, usize), Fail<'s>> {
        match argspec {
            "verb" => return self.parse_verb_args(pos),
            "verbatim" => return self.parse_verbatim_env_args(pos),
            _ => {}
        }
        let inner = match text_mode_args {
            Some(false) if state.in_math_mode => State {
                in_math_mode: false,
                math_mode_delimiter: None,
            },
            Some(true) if !state.in_math_mode => State {
                in_math_mode: true,
                math_mode_delimiter: None,
            },
            _ => state,
        };
        let mut argnlist = Vec::new();
        let mut p = pos;
        for c in argspec.chars() {
            match c {
                '{' => {
                    let (node, np, nl) = self.get_latex_expression(p, inner)?;
                    p = np + nl;
                    argnlist.push(Some(node));
                }
                '[' => {
                    if optional_arg_no_space && self.is_space_at(p) {
                        argnlist.push(None);
                        continue;
                    }
                    match self.get_latex_maybe_optional_arg(p, inner)? {
                        Some((node, np, nl)) => {
                            p = np + nl;
                            argnlist.push(Some(node));
                        }
                        None => argnlist.push(None),
                    }
                }
                '*' => {
                    let tok = self.get_token(p, None, true, state)?;
                    match tok.tok {
                        Tok::Char(c) if c.starts_with('*') => {
                            argnlist.push(Some(mk(Kind::Chars("*".into()), tok.pos, 1)));
                            p = tok.pos + 1;
                        }
                        _ => argnlist.push(None),
                    }
                }
                _ => return Err(Fail::Parse),
            }
        }
        Ok((
            Args {
                argspec: argspec.to_string(),
                argnlist,
            },
            pos,
            p - pos,
        ))
    }

    /// `VerbatimArgsParser('verb-macro')`: `\verb+…+`.
    fn parse_verb_args(&self, mut pos: usize) -> Result<(Args, usize, usize), Fail<'s>> {
        while self.is_space_at(pos) {
            pos += self.ch(pos).map_or(1, char::len_utf8);
            if pos >= self.s.len() {
                return Err(Fail::Parse);
            }
        }
        let delim = self.ch(pos).ok_or(Fail::Parse)?;
        let begin = pos + delim.len_utf8();
        let end = begin + self.s[begin..].find(delim).ok_or(Fail::Parse)?;
        let node = mk(
            Kind::Chars(self.s[begin..end].to_string()),
            begin,
            end - begin,
        );
        Ok((
            Args {
                argspec: "{".into(),
                argnlist: vec![Some(node)],
            },
            pos,
            end + delim.len_utf8() - pos,
        ))
    }

    /// `VerbatimArgsParser('verbatim-environment')`: everything up to
    /// `\end{verbatim}` is the one argument, and the body parse then starts
    /// right at that `\end`.
    fn parse_verbatim_env_args(&self, pos: usize) -> Result<(Args, usize, usize), Fail<'s>> {
        let end = pos + self.s[pos..].find("\\end{verbatim}").ok_or(Fail::Parse)?;
        let node = mk(Kind::Chars(self.s[pos..end].to_string()), pos, end - pos);
        Ok((
            Args {
                argspec: "{".into(),
                argnlist: vec![Some(node)],
            },
            pos,
            end - pos,
        ))
    }

    // -- the node reader ------------------------------------------------------

    /// `get_latex_nodes`: read nodes from `pos` until `stop` (or the end).
    /// Tolerant parsing never fails: a stray closing brace or mismatched
    /// `\end` is consumed and dropped, a missing terminator ends the list at
    /// the end of the input.
    fn get_latex_nodes(
        &self,
        pos: usize,
        stop: Stop<'s>,
        state: State<'s>,
    ) -> (Vec<Node>, usize, usize) {
        let depth = self.depth.get();
        if depth > MAX_DEPTH {
            return (Vec::new(), pos, 0);
        }
        self.depth.set(depth + 1);
        let result = self.read_nodes(pos, stop, state);
        self.depth.set(depth);
        result
    }

    fn read_nodes(
        &self,
        origpos: usize,
        stop: Stop<'s>,
        state: State<'s>,
    ) -> (Vec<Node>, usize, usize) {
        let include_brace = match stop {
            Stop::Brace(o, c) if c != '}' => Some((o, c)),
            _ => None,
        };
        let mut nodelist: Vec<Node> = Vec::new();
        let mut p = origpos;
        // Accumulated plain characters and where they started.
        let mut lastchars = String::new();
        let mut lastchars_pos: Option<usize> = None;

        loop {
            let tok = match self.get_token(p, include_brace, true, state) {
                Ok(t) => t,
                Err(Fail::EndOfStream(final_space)) => {
                    // The trailing whitespace joins the last chars — inside
                    // an unclosed group or environment too: tolerant
                    // `do_read` returns the end-of-stream error itself, and
                    // the loop pushes its `final_space` whatever it stops at.
                    if lastchars_pos.is_none() {
                        lastchars_pos = Some(p);
                    }
                    lastchars.push_str(final_space);
                    p += final_space.len();
                    break;
                }
                Err(Fail::Parse) => break,
            };
            p = tok.pos + tok.len;
            if let Tok::Char(c) = tok.tok {
                if lastchars_pos.is_none() {
                    lastchars_pos = Some(tok.pos - tok.pre_space.len());
                }
                lastchars.push_str(tok.pre_space);
                lastchars.push_str(c);
                continue;
            }
            if let Some(charspos) = lastchars_pos.take() {
                let mut chars = std::mem::take(&mut lastchars);
                chars.push_str(tok.pre_space);
                nodelist.push(mk(Kind::Chars(chars), charspos, tok.pos - charspos));
            } else if !tok.pre_space.is_empty() {
                nodelist.push(mk(
                    Kind::Chars(tok.pre_space.to_string()),
                    tok.pos - tok.pre_space.len(),
                    tok.pre_space.len(),
                ));
            }
            match tok.tok {
                Tok::Char(_) => unreachable!("handled above"),
                Tok::BraceClose(c) => {
                    if matches!(stop, Stop::Brace(_, want) if want == c) {
                        break;
                    }
                    // Mismatched: reported, ignored, consumed.
                }
                Tok::EndEnv(name) => {
                    if matches!(stop, Stop::EndEnv(want) if want == name) {
                        break;
                    }
                }
                Tok::MathInline(arg) | Tok::MathDisplay(arg) => {
                    if let Stop::MathMode(want) = stop {
                        if arg == want {
                            break;
                        }
                        if arg == "\\)" || arg == "\\]" {
                            continue;
                        }
                    } else if arg == "\\)" || arg == "\\]" {
                        continue;
                    }
                    let closing = match arg {
                        "\\(" => "\\)",
                        "\\[" => "\\]",
                        other => other,
                    };
                    let display = !(arg == "\\(" || arg == "$");
                    let inner = State {
                        in_math_mode: true,
                        math_mode_delimiter: Some(arg),
                    };
                    let (mnodes, mpos, mlen) =
                        self.get_latex_nodes(p, Stop::MathMode(closing), inner);
                    p = mpos + mlen;
                    nodelist.push(mk(
                        Kind::Math {
                            display,
                            nodelist: mnodes,
                            delims: (arg.to_string(), closing.to_string()),
                        },
                        tok.pos,
                        p - tok.pos,
                    ));
                }
                Tok::Comment(text) => nodelist.push(mk(
                    Kind::Comment {
                        text: text.to_string(),
                        post_space: tok.post_space.to_string(),
                    },
                    tok.pos,
                    tok.len,
                )),
                Tok::BraceOpen(c) => {
                    if let Ok((group, bpos, blen)) = self.get_latex_braced_group(tok.pos, c, state)
                    {
                        p = bpos + blen;
                        nodelist.push(group);
                    }
                }
                Tok::BeginEnv(_) => {
                    if let Ok((env, epos, elen)) = self.get_latex_environment(tok.pos, state) {
                        p = epos + elen;
                        nodelist.push(env);
                    }
                }
                Tok::Macro(name) => {
                    let argspec = if self.zero_arg.is_some_and(|z| z.contains(name)) {
                        ""
                    } else {
                        macro_argspec(name)
                    };
                    let text_mode = if TEXT_MODE_ARG.contains(&name) {
                        Some(false)
                    } else if name == "ensuremath" {
                        Some(true)
                    } else {
                        None
                    };
                    let args = match self.parse_args(
                        argspec,
                        tok.pos + tok.len,
                        state,
                        name == "\\",
                        text_mode,
                    ) {
                        Ok((args, apos, alen)) => {
                            p = apos + alen;
                            Some(args)
                        }
                        // Arguments that fail to parse are dropped and reading
                        // resumes right after the macro name.
                        Err(_) => {
                            p = tok.pos + tok.len;
                            None
                        }
                    };
                    nodelist.push(mk(
                        Kind::Macro {
                            name: name.to_string(),
                            args,
                            post_space: tok.post_space.to_string(),
                        },
                        tok.pos,
                        p - tok.pos,
                    ));
                }
                Tok::Specials(sp) => {
                    nodelist.push(mk(Kind::Specials(sp.to_string()), tok.pos, tok.len))
                }
            }
        }
        if let Some(charspos) = lastchars_pos.filter(|_| !lastchars.is_empty()) {
            let len = lastchars.len();
            nodelist.push(mk(Kind::Chars(lastchars), charspos, len));
        }
        (nodelist, origpos, p - origpos)
    }
}

fn mk(kind: Kind, pos: usize, len: usize) -> Node {
    Node { kind, pos, len }
}

/// A one-line-per-node dump of a node tree, the format the port is checked
/// against pylatexenc's own output with (see `scripts/` and the tests).
#[cfg(test)]
pub(crate) fn dump(nodes: &[Node], indent: usize, out: &mut String) {
    use std::fmt::Write;
    let pad = "  ".repeat(indent);
    for n in nodes {
        match &n.kind {
            Kind::Chars(c) => {
                let _ = writeln!(out, "{pad}Chars {:?} @{}+{}", c, n.pos, n.len);
            }
            Kind::Group { nodelist, delims } => {
                let _ = writeln!(
                    out,
                    "{pad}Group {}{} @{}+{}",
                    delims.0, delims.1, n.pos, n.len
                );
                dump(nodelist, indent + 1, out);
            }
            Kind::Comment { text, post_space } => {
                let _ = writeln!(
                    out,
                    "{pad}Comment {:?} post={:?} @{}+{}",
                    text, post_space, n.pos, n.len
                );
            }
            Kind::Macro {
                name,
                args,
                post_space,
            } => {
                let _ = writeln!(
                    out,
                    "{pad}Macro {:?} post={:?} @{}+{}",
                    name, post_space, n.pos, n.len
                );
                dump_args(args.as_ref(), indent + 1, out);
            }
            Kind::Env {
                name,
                nodelist,
                args,
            } => {
                let _ = writeln!(out, "{pad}Env {:?} @{}+{}", name, n.pos, n.len);
                dump_args(args.as_ref(), indent + 1, out);
                dump(nodelist, indent + 1, out);
            }
            Kind::Specials(c) => {
                let _ = writeln!(out, "{pad}Specials {:?} @{}+{}", c, n.pos, n.len);
            }
            Kind::Math {
                display,
                nodelist,
                delims,
            } => {
                let _ = writeln!(
                    out,
                    "{pad}Math {} {:?} {:?} @{}+{}",
                    if *display { "display" } else { "inline" },
                    delims.0,
                    delims.1,
                    n.pos,
                    n.len
                );
                dump(nodelist, indent + 1, out);
            }
        }
    }
}

#[cfg(test)]
fn dump_args(args: Option<&Args>, indent: usize, out: &mut String) {
    use std::fmt::Write;
    let pad = "  ".repeat(indent);
    match args {
        None => {
            let _ = writeln!(out, "{pad}args None");
        }
        Some(a) => {
            let _ = writeln!(out, "{pad}args {:?}", a.argspec);
            for arg in &a.argnlist {
                match arg {
                    None => {
                        let _ = writeln!(out, "{pad}  -");
                    }
                    Some(n) => dump(std::slice::from_ref(n), indent + 1, out),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_str(s: &str) -> String {
        let mut out = String::new();
        dump(&Walker::new(s).parse(), 0, &mut out);
        out
    }

    /// Oracle check: `DOCLING_LATEX_DUMP=<file.tex> cargo test -p docling
    /// --lib latex_walker::tests::dump_file -- --ignored --nocapture` writes
    /// `<file.tex>.rs.txt`, to diff against `scripts/dev/pylatexenc_dump.py`.
    #[test]
    #[ignore]
    fn dump_file() {
        let Ok(path) = std::env::var("DOCLING_LATEX_DUMP") else {
            return;
        };
        for path in path.split(':') {
            let s = std::fs::read_to_string(path).unwrap();
            std::fs::write(format!("{path}.rs.txt"), dump_str(&s)).unwrap();
        }
    }

    #[test]
    fn chars_macros_and_paragraph_breaks_tokenize_like_pylatexenc() {
        let out = dump_str("A \\textbf{b} c\n\nd \\x e");
        // The macro's argument is a group; the space after `\x` is its
        // post_space, not chars; `\n\n` stays in the chars stream.
        assert_eq!(
            out,
            "Chars \"A \" @0+2\nMacro \"textbf\" post=\"\" @2+10\n  args \"{\"\n    Group {} @9+3\n      Chars \"b\" @10+1\nChars \" c\\n\\nd \" @12+6\nMacro \"x\" post=\" \" @18+3\n  args \"\"\nChars \"e\" @21+1\n"
        );
    }

    #[test]
    fn comments_environments_math_and_specials() {
        let out = dump_str("x % note\n  y $a$ \\begin{tabular}{cc}1 & 2 \\\\\\end{tabular} a~b");
        assert!(out.contains("Comment \" note\" post=\"\\n  \""), "{out}");
        assert!(out.contains("Math inline \"$\" \"$\""), "{out}");
        assert!(out.contains("Env \"tabular\""), "{out}");
        assert!(out.contains("Specials \"&\""), "{out}");
        assert!(out.contains("Macro \"\\\\\" post=\"\""), "{out}");
        assert!(out.contains("Specials \"~\""), "{out}");
    }

    #[test]
    fn tolerant_parsing_skips_stray_closers_and_ends_at_eof() {
        // A stray `}` is dropped; a group missing its `}` runs to the end.
        let out = dump_str("a } b {c");
        assert_eq!(
            out,
            "Chars \"a \" @0+2\nChars \" b \" @3+3\nGroup {} @6+2\n  Chars \"c\" @7+1\n"
        );
        // `\paragraph` is unknown to pylatexenc: its title is a sibling group.
        let out = dump_str("\\paragraph{T} x");
        assert!(
            out.contains("Macro \"paragraph\" post=\"\" @0+10\n  args \"\"\nGroup {} @10+3"),
            "{out}"
        );
    }
}
