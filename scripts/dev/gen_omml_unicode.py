#!/usr/bin/env python3
"""Generate crates/docling/src/backend/omml_unicode.rs from docling's OMML converter.

docling's `oMath2Latex.process_unicode` (docling/backend/docx/latex/omml.py)
turns every character of an `<m:t>` run into LaTeX through pylatexenc's
`UnicodeToLatexEncoder`. The Rust port (`backend/omml.rs`) cannot call
pylatexenc, so the per-character results are dumped from the installed
package: run with docling and its pinned pylatexenc active (the conformance
venv — see scripts/conformance/setup-docling.sh)

    python scripts/dev/gen_omml_unicode.py > crates/docling/src/backend/omml_unicode.rs

Deliberate differences from docling's strings, all checked below:

- docling unwraps pylatexenc's `\\ensuremath{…}` with `replace("}", " ")`,
  which turns *every* closing brace into a space, so a macro with an argument
  comes out unbalanced (`ℝ` → `\\mathbb{R`, `𝐀` → `\\mathbf{A`). Here only the
  wrapper's own brace is dropped; every character docling already gets right
  keeps docling's string byte for byte.
- `{` and `}` are left out of the table: pylatexenc's `{\\{}` / `{\\}}` go
  through `do_r`'s brace un-escaping afterwards and come out as `\\{\\{\\}` or an
  unbalanced `{\\}\\}`, so the Rust port escapes a literal brace with
  `escape_latex` alone (`\\{`, `\\}`).
- `√` is `\\surd`, not pylatexenc's `\\sqrt{}`, which renders an empty root
  before its operand.
- Where docling's string is a text-mode command in `\\text{…}` that math
  renderers do not read (KaTeX, math-core: `→` → ` \\text{ \\textrightarrow } `,
  #1253), the table writes the math-mode command (`MATH_MODE`: `\\rightarrow`,
  padded as docling pads its math macros) or, for a text symbol without one,
  the character as typed (`AS_TYPED`), which they read. The text-mode commands
  KaTeX reads too (`KATEX_TEXT`) keep docling's string. Any new text-mode
  entry, or an override whose docling string is no longer text mode, stops
  the script: the overrides go once docling emits math-mode commands
  (docling-project/docling#5666).
- Only the blocks a formula draws from are covered (ASCII, the Latin-1
  symbols, Greek, punctuation, letterlike, arrows, the mathematical
  operators and technical symbols, the mathematical alphanumerics). Accented
  letters stay as typed: pylatexenc's text accents (`é` → `\\'e`) are errors
  in math mode.
"""

import importlib.metadata
import sys

import lxml.etree as ET
from docling.backend.docx.latex.omml import oMath2Latex

RANGES = [
    (0x0020, 0x007E),  # ASCII
    (0x00A0, 0x00BF),  # Latin-1 symbols
    (0x00D7, 0x00D7),  # ×
    (0x00F7, 0x00F7),  # ÷
    (0x0391, 0x03A9),  # Greek capitals
    (0x03B1, 0x03C9),  # Greek small letters
    (0x03D0, 0x03F6),  # Greek symbol variants (ϑ ϕ ϖ ϰ ϱ ϵ …)
    (0x2000, 0x206F),  # General Punctuation (dashes, primes, ‖, …)
    (0x2100, 0x214F),  # Letterlike Symbols (ℕ ℝ ℏ ℓ …)
    (0x2190, 0x21FF),  # Arrows
    (0x2200, 0x22FF),  # Mathematical Operators
    (0x2300, 0x23FF),  # Miscellaneous Technical
    (0x25A0, 0x25FF),  # Geometric Shapes
    (0x27C0, 0x27FF),  # Misc. Mathematical Symbols-A, Supplemental Arrows-A
    (0x2900, 0x2AFF),  # Supplemental Arrows-B, Misc. Math Symbols-B, Supplemental Operators
    (0x1D400, 0x1D7FF),  # Mathematical Alphanumeric Symbols
]

ENSUREMATH = "\\ensuremath{"

# Characters left out of the table: `do_r` escapes them itself.
SKIP = {"{", "}"}

# docling's text-mode entries with a math-mode command (#1253). KaTeX and
# pdflatex (amsmath; amssymb for `\mho` and `\yen`) read each one.
MATH_MODE = {
    "\u2190": "\\leftarrow",  # ←
    "\u2191": "\\uparrow",  # ↑
    "\u2192": "\\rightarrow",  # →
    "\u2193": "\\downarrow",  # ↓
    "\u00ac": "\\neg",  # ¬
    "\u00b7": "\\cdot",  # ·
    "\u00f7": "\\div",  # ÷
    "\u00b5": "\\mu",  # µ
    "\u00b9": "^{1}",  # ¹
    "\u00b2": "^{2}",  # ²
    "\u00b3": "^{3}",  # ³
    "\u00bc": "\\frac{1}{4}",  # ¼
    "\u00bd": "\\frac{1}{2}",  # ½
    "\u00be": "\\frac{3}{4}",  # ¾
    "\u2103": "{}^{\\circ}\\mathrm{C}",  # ℃
    "\u2044": "/",  # ⁄ (fraction slash)
    "\u2022": "\\bullet",  # •
    "\u25e6": "\\circ",  # ◦
    "\u25ef": "\\bigcirc",  # ◯
    "\u2127": "\\mho",  # ℧
    "\u00a7": "\\S",  # §
    "\u00b6": "\\P",  # ¶
    "\u00a5": "\\yen",  # ¥
}

# docling's text-mode entries without one: text symbols, kept as typed (#1253).
AS_TYPED = set("¡¢¤¦¨ª¯´º¿‰‱※‽⁎⁒№℗℞℠™℮\u200c")

# docling's text-mode entries KaTeX reads as they are: docling's string kept.
KATEX_TEXT = {
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
}

# Entries replaced outright. A macro gets the two-space padding docling gives
# its math macros (`⇒` → `  \Rightarrow  `); `/` is ASCII, as typed.
OVERRIDES = {
    "\u221a": "  \\surd  ",
    **{ch: cmd if cmd == "/" else f"  {cmd}  " for ch, cmd in MATH_MODE.items()},
    **{ch: ch for ch in AS_TYPED},
}


def balanced(s: str) -> bool:
    depth = 0
    escaped = False
    for ch in s:
        if escaped:
            escaped = False
        elif ch == "\\":
            escaped = True
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth < 0:
                return False
    return depth == 0


def unwrap_ensuremath(s: str) -> str:
    """`process_unicode`'s `\\ensuremath{…}` unwrapping, keeping inner braces."""
    while (i := s.find(ENSUREMATH)) >= 0:
        depth = 1
        j = i + len(ENSUREMATH)
        while depth:
            if s[j] == "\\":
                j += 2
                continue
            depth += {"{": 1, "}": -1}.get(s[j], 0)
            j += 1
        s = s[:i] + " " + s[i + len(ENSUREMATH) : j - 1] + " " + s[j:]
    return s


def process_unicode(conv: oMath2Latex, ch: str) -> str:
    """docling's `process_unicode`, with only the brace handling changed."""
    if ch in conv._MATH_CHAR_MAP:
        return conv._MATH_CHAR_MAP[ch]
    out = conv.u.unicode_to_latex(ch)
    if (
        not ch.startswith("{")
        and out.startswith("{")
        and not ch.endswith("}")
        and out.endswith("}")
    ):
        out = f" {out[1:-1]} "
    if ENSUREMATH in out:
        out = unwrap_ensuremath(out)
    if out.strip().startswith("\\text"):
        out = f" \\text{{{out}}} "
    return out


def text_mode(s: str) -> str | None:
    """The command of a ` \\text{ \\textxxx } ` entry, else None."""
    s = s.strip()
    if s.startswith("\\text{") and s.endswith("}"):
        return s[len("\\text{") : -1].strip()
    return None


def rust_str(s: str) -> str:
    out = []
    for ch in s:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch.isprintable():
            out.append(ch)
        else:
            out.append(f"\\u{{{ord(ch):x}}}")
    return '"' + "".join(out) + '"'


def rust_char(ch: str) -> str:
    return f"'\\u{{{ord(ch):x}}}'"


def main() -> None:
    ns = "http://schemas.openxmlformats.org/officeDocument/2006/math"
    conv = oMath2Latex(ET.Element(f"{{{ns}}}oMath"))
    rows = []
    for lo, hi in RANGES:
        for cp in range(lo, hi + 1):
            ch = chr(cp)
            if ch in SKIP:
                continue
            upstream = conv.process_unicode(ch)
            if (cmd := text_mode(upstream)) is not None:
                # Every text-mode entry is a known one (#1253).
                known = ch in MATH_MODE or ch in AS_TYPED or cmd in KATEX_TEXT
                assert known, (hex(cp), upstream)
            else:
                # docling no longer writes it in text mode: drop the override.
                assert ch not in MATH_MODE and ch not in AS_TYPED, (hex(cp), upstream)
            ours = OVERRIDES.get(ch) or process_unicode(conv, ch)
            if balanced(upstream) and ch not in OVERRIDES:
                # Where docling's string is well-formed it is kept as is.
                assert ours == upstream, (hex(cp), upstream, ours)
            assert balanced(ours), (hex(cp), ours)
            if ours != ch:
                rows.append((ch, ours))

    out = sys.stdout
    out.write(
        "//! docling's per-character OMML text → LaTeX — GENERATED by\n"
        "//! `scripts/dev/gen_omml_unicode.py`, do not edit by hand.\n"
        "//!\n"
        "//! Source: docling's `oMath2Latex.process_unicode` on pylatexenc "
        f"{importlib.metadata.version('pylatexenc')},\n"
        "//! with the `\\ensuremath{…}` wrapper dropped without the braces of the\n"
        "//! macro inside it, `{` and `}` left to `escape_latex`, `√` as `\\surd`\n"
        "//! and docling's text-mode commands math renderers reject as math-mode\n"
        "//! ones or the character (see the script). Characters that map to\n"
        "//! themselves are left out.\n"
        "\n"
        "/// `(character, LaTeX)`, sorted by character for binary search.\n"
        "pub(super) static UNICODE_LATEX: &[(char, &str)] = &[\n"
    )
    for ch, latex in rows:
        out.write(f"    ({rust_char(ch)}, {rust_str(latex)}),\n")
    out.write("];\n")


if __name__ == "__main__":
    main()
