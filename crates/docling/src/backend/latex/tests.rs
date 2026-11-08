//! Unit tests for the LaTeX backend — upstream's `tests/test_latex` suite
//! (docling 2.142) ported assertion for assertion where it checks the
//! document, plus the corners this port has to get right on its own.

use super::*;
use crate::format::InputFormat;
use serde_json::Value;

fn convert(tex: &str) -> DoclingDocument {
    let src = SourceDocument::from_bytes("d", InputFormat::Latex, tex.as_bytes().to_vec());
    LatexBackend.convert(&src).unwrap()
}

fn md(tex: &str) -> String {
    convert(tex).export_to_markdown().trim().to_string()
}

fn json(tex: &str) -> Value {
    serde_json::from_str(&convert(tex).export_to_json()).unwrap()
}

/// `(label, text)` of every body-layer text item, in creation order.
fn body_texts(json: &Value) -> Vec<(String, String)> {
    json["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["content_layer"] == "body")
        .map(|t| {
            (
                t["label"].as_str().unwrap().to_string(),
                t["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn formulas(json: &Value) -> Vec<String> {
    body_texts(json)
        .into_iter()
        .filter(|(l, _)| l == "formula")
        .map(|(_, t)| t)
        .collect()
}

fn furniture(json: &Value) -> Vec<String> {
    json["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["content_layer"] == "furniture")
        .map(|t| t["text"].as_str().unwrap().to_string())
        .collect()
}

/// The JSON item a `$ref` points at.
fn resolve<'a>(json: &'a Value, r: &Value) -> &'a Value {
    let path = r["$ref"].as_str().unwrap();
    let mut it = path.trim_start_matches("#/").split('/');
    let bucket = it.next().unwrap();
    let idx: usize = it.next().unwrap().parse().unwrap();
    &json[bucket][idx]
}

// -- text, paragraphs, spacing (test_basic) ----------------------------------

#[test]
fn paragraph_breaks_and_inline_macros_follow_docling() {
    let tex = "\\begin{document}\nA \\textit{it} text. B \\textbf{bf} more.\n\n\
        C \\emph{em}x and \\textit{it}.\n\\end{document}";
    // The space after a formatting macro's argument is ordinary text (only
    // the space after a macro *name* is swallowed); a sentence continuing
    // after a paragraph break stays one paragraph (docling#4340).
    assert_eq!(md(tex), "A it text. B bf more.\n\nC emx and it.");
}

#[test]
fn source_linebreaks_are_spaces_in_document_text() {
    // `_normalize_document_text`: a whitespace run holding a line break is
    // one space; runs of plain spaces stay.
    let j = json("\\begin{document}\n\\section{A\nheading}\nOne\nline  kept.\n\\end{document}");
    assert_eq!(
        body_texts(&j),
        [
            ("section_header".into(), "A heading".into()),
            ("text".into(), "One line  kept.".into()),
        ]
    );
}

#[test]
fn decodes_cp1252_and_translates_line_endings() {
    // docling#4475: not UTF-8 → cp1252 (0x93/0x94 are curly quotes there).
    let bytes =
        b"\\begin{document}\r\n\x93Quoted\x94 caf\xe9\r\n\r\nNext.\r\n\\end{document}".to_vec();
    let src = SourceDocument::from_bytes("d", InputFormat::Latex, bytes);
    let doc = LatexBackend.convert(&src).unwrap();
    assert_eq!(
        doc.export_to_markdown().trim(),
        "\u{201c}Quoted\u{201d} caf\u{e9}\n\nNext."
    );
}

// -- macros (test_macros, test_macros_more) ----------------------------------

#[test]
fn accent_and_letter_macros_become_unicode() {
    // docling#4517, latex2text's combining marks NFC-composed.
    assert_eq!(
        md("\\begin{document}Caf\\'e na\\\"ive \\c{c}a \\v{s} \\H{o} \\ss{} \\o{} \\AA{} \\'{\\i}.\\end{document}"),
        "Café naïve ça š ő ß ø Å í."
    );
    // `\~{}` and `\^{}` typeset the literal characters.
    assert_eq!(md("\\begin{document}a\\~{}b\\^{}c\\end{document}"), "a~b^c");
}

#[test]
fn a_redefined_accent_name_is_a_custom_macro() {
    // docling#4817: `\def\b{…}` is registered argument-less before the
    // parse, so `$` after it is not read as the accent's argument.
    let j = json("\\def\\b{\\mathbf{b}}\n\\begin{document}\nSee $\\b$ here.\n\\end{document}");
    assert_eq!(formulas(&j), ["\\mathbf{b}"]);
}

#[test]
fn figures_captions_theorems_and_custom_macros() {
    let tex = "\\newcommand{\\ours}{OTSL}\\newcommand{\\emp}[1]{<#1>}\n\\begin{document}\n\
        \\begin{figure}\\centering\\includegraphics[width=1cm]{fig/a_b.png}\\caption{A fig.}\\label{f}\\end{figure}\n\
        \\begin{theorem}Text with \\ours{} and \\emp{x}.\\end{theorem}\n\
        Cite~\\cite{a,b}; dashes -- gone.\n\\end{document}";
    let doc = convert(tex);
    // Inside a figure the `\caption` is the picture's caption and no
    // `Image: <path>` caption is added (docling#4817).
    assert_eq!(
        doc.export_to_markdown().trim(),
        "A fig.\n\n<!-- image -->\n\n**Theorem.**\n\nText with OTSL and &lt;x&gt;.\n\nCite[a,b]; dashes  gone."
    );
    let j: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    // The definitions are furniture, ahead of everything else.
    assert_eq!(
        furniture(&j),
        ["\\newcommand{\\ours}{OTSL}", "\\newcommand{\\emp}[1]{<#1>}"]
    );
    let picture = &j["pictures"][0];
    assert_eq!(picture["parent"]["$ref"], "#/groups/0");
    assert_eq!(j["groups"][0]["name"], "figure");
    let caption = resolve(&j, &picture["captions"][0]);
    assert_eq!(caption["label"], "caption");
    assert_eq!(caption["text"], "A fig.");
    assert_eq!(caption["parent"]["$ref"], "#/groups/0");
}

#[test]
fn image_path_caption_outside_a_figure() {
    let j = json("\\begin{document}\n\\includegraphics{x.png}\n\\end{document}");
    let caption = resolve(&j, &j["pictures"][0]["captions"][0]);
    assert_eq!(caption["text"], "Image: x.png");
    assert_eq!(caption["parent"]["$ref"], "#/body");
}

#[test]
fn layout_assignments_and_bibliography_style_are_not_body_text() {
    let tex =
        "\\documentclass{article}\n\\hfuzz=2pt\n\\bibliographystyle{plain}\n\\begin{document}\n\
        Body \\vfuzz = 1.5pt text.\n\\end{document}";
    // The assignment goes, the spaces around it stay (upstream's output).
    assert_eq!(md(tex), "Body  text.");
    assert_eq!(
        furniture(&json(tex)),
        ["\\documentclass{article}", "\\bibliographystyle{plain}"]
    );
}

#[test]
fn footnotes_keep_source_order() {
    assert_eq!(
        md("\\documentclass{article}\n\\begin{document}\n\
            First\\footnote{note one} second\\footnote{note two} third.\n\\end{document}\n"),
        "First\n\nnote one\n\nsecond\n\nnote two\n\nthird."
    );
}

#[test]
fn comments_inside_definitions() {
    // A commented-out definition hides nothing.
    assert_eq!(
        md("\\documentclass{article}\n% \\newcommand{\\unused}{\n\\begin{document}\nImportant content.\n\\end{document}\n"),
        "Important content."
    );
    // A comment inside a definition is dropped with its line break.
    assert_eq!(
        md("\\documentclass{article}\n\\newcommand{\\term}{First% {\nSecond}\n\\begin{document}\nImportant content. \\term\n\\end{document}\n"),
        "Important content. FirstSecond"
    );
    for comment in ["% comment\n", "% comment\n% more\n"] {
        let tex = format!(
            "\\documentclass{{article}}\n\\newcommand{{\\brand}}{{Docling}}\n\
             \\newcommand{{\\term}}[1]{{\\brand{comment}rocks #1}}\n\\begin{{document}}\n\
             Important content. \\term{{now}}\n\\end{{document}}\n"
        );
        assert_eq!(md(&tex), "Important content. Doclingrocks now");
    }
    // …and a control word it ends keeps its boundary.
    let j = json("\\documentclass{article}\n\\newcommand{\\expression}{\\alpha% comment\nbeta}\n\\begin{document}\n$\\expression$\n\\end{document}\n");
    assert_eq!(formulas(&j), ["\\alpha beta"]);
    let j = json("\\documentclass{article}\n\\newcommand{\\expression}{\\sqrt% comment\nx}\n\\begin{document}\n$\\expression$\n\\end{document}\n");
    assert_eq!(formulas(&j), ["\\sqrt x"]);
}

#[test]
fn body_definitions_are_supplementary() {
    let declarations = [
        "\\newcommand{\\term}{Defined}",
        "\\newenvironment{custom}{Start}{End}",
        "\\newtheorem{lemma}{Lemma}",
        "\\newlength{\\customlength}",
        "\\newcounter{customcounter}",
        "\\newif\\ifcustom",
        "\\newtheoremstyle{customstyle}{1pt}{2pt}{}{}{}{.}{1em}{}",
    ];
    let tex = format!(
        "\\documentclass{{article}}\\begin{{document}}\n{}\nVisible body.\\end{{document}}",
        declarations.join("\n")
    );
    let j = json(&tex);
    let source = furniture(&j);
    for d in declarations {
        assert!(source.iter().any(|s| s == d), "{d} not kept: {source:?}");
    }
    let markdown = md(&tex);
    assert_eq!(markdown, "Visible body.");
}

#[test]
fn verbatim_keeps_definitions_and_first_line_indentation() {
    let tex = "\\documentclass{article}\n\\begin{document}\n\\begin{verbatim}\n\n  \\newcommand{\\foo}{First}\nnext\n\\end{verbatim}\n\\end{document}\n";
    // docling#4502: only leading blank lines go; the first line's indent stays.
    assert_eq!(md(tex), "```\n  \\newcommand{\\foo}{First}\nnext\n```");
    assert_eq!(furniture(&json(tex)), ["\\documentclass{article}"]);
}

#[test]
fn custom_macro_arguments_with_inline_math() {
    let markdown = md("\\documentclass{article}\n\\newcommand{\\pair}[2]{#1 and #2}\n\\begin{document}\n\\textbf{\\pair{A}{B} $x$}\n\\end{document}\n");
    assert_eq!(markdown, "A and B $x$");
    let markdown =
        md("\\documentclass{article}\n\\begin{document}\n\\textbf{Before \\begin{math}x\\end{math} after.}\n\\end{document}\n");
    assert_eq!(markdown, "Before $x$ after.");
}

#[test]
fn bibliography_items_carry_their_keys() {
    let j = json("\\begin{document}\n\\begin{thebibliography}{99}\n\\bibitem{knuth} D. Knuth, \\emph{TAOCP}.\n\
        \\bibitem[L]{lamport} L. Lamport.\n\\end{thebibliography}\n\\end{document}");
    assert_eq!(
        body_texts(&j),
        [
            ("section_header".into(), "References".into()),
            ("list_item".into(), "[knuth] D. Knuth, TAOCP.".into()),
            ("list_item".into(), "[lamport] L. Lamport.".into()),
        ]
    );
    assert_eq!(j["groups"][0]["name"], "bibliography");
}

// -- lists (test_lists) ------------------------------------------------------

#[test]
fn every_item_flushes_and_description_terms_lead() {
    // docling#4644 / #4579.
    let tex = "\\begin{document}\n\\begin{description}\n\\item[Term] described\n\\item[] bare\n\\item plain\n\
        \\end{description}\n\\end{document}";
    assert_eq!(md(tex), "- Term: described\n- bare\n- plain");
}

#[test]
fn nested_lists_indent_under_their_item() {
    let tex = "\\begin{document}\n\\begin{itemize}\n\\item One\n\\begin{itemize}\n\\item Inner\n\\end{itemize}\n\
        \\item Two\n\\end{itemize}\n\\end{document}";
    assert_eq!(md(tex), "- One\n    - Inner\n- Two");
}

// -- math (test_math) ----------------------------------------------------------

#[test]
fn inline_math_is_an_inline_group_of_text_and_formulas() {
    let tex = "\\title{T}\\author{A}\n\\begin{document}\n\\maketitle\n\n\\section{Math}\n\nInline math: $x$\n\n\
        Display:\n$$y = 1$$\n\n\\subsection{List}\nAfter a heading \\textbf{bold} text.\n\n\
        \\begin{enumerate}\n\\item one\n\\item two\n\\end{enumerate}\n\\end{document}";
    let j = json(tex);
    assert_eq!(
        body_texts(&j),
        [
            ("title".into(), "T".into()),
            ("text".into(), "A".into()),
            ("section_header".into(), "Math".into()),
            ("text".into(), "".into()),
            ("text".into(), "Inline math: ".into()),
            ("formula".into(), "x".into()),
            ("text".into(), "Display:".into()),
            ("formula".into(), "y = 1".into()),
            ("section_header".into(), "List".into()),
            ("text".into(), "After a heading bold text.".into()),
            ("list_item".into(), "one".into()),
            ("list_item".into(), "two".into()),
        ]
    );
    let texts = j["texts"].as_array().unwrap();
    let inline_host = &texts[3];
    let group = resolve(&j, &inline_host["children"][0]);
    assert_eq!(group["label"], "inline");
    assert_eq!(group["children"].as_array().unwrap().len(), 2);
    // Front matter sits in `metadata` / `authors` groups.
    assert_eq!(texts[0]["parent"]["$ref"], "#/groups/0");
    assert_eq!(j["groups"][0]["name"], "metadata");
    assert_eq!(j["groups"][1]["name"], "authors");
    // The text run keeps its own trailing space and docling-core 2.106
    // concatenates the parts (#997): one space, as written.
    let markdown = md(tex);
    assert!(
        markdown.contains("## Math\n\nInline math: $x$\n\nDisplay:\n\n$$y = 1$$"),
        "{markdown}"
    );
    assert!(
        markdown.contains("1. one\n2. two") || markdown.contains("- one\n- two"),
        "{markdown}"
    );
}

#[test]
fn inline_math_in_headings_captions_and_list_items() {
    let tex = "\\begin{document}\n\\section{Intro with $\\alpha$}\n\\begin{itemize}\n\\item First $z$ item\n\\end{itemize}\n\
        \\begin{figure}\\includegraphics{x.png}\\caption{Cap with $q$}\\end{figure}\n\\end{document}";
    assert_eq!(
        md(tex),
        "## Intro with $\\alpha$\n\n- First $z$ item\n\nCap with $q$\n\n<!-- image -->"
    );
}

#[test]
fn numbered_and_aligned_environments_keep_their_delimiters() {
    // docling#4817: only displaymath / math / dmath lose theirs.
    let j = json("\\begin{document}\n\\begin{equation}\nE = mc^2 \\label{eq:1}\n\\end{equation}\n\
        \\begin{align}\na &= b\n\\end{align}\n\\begin{displaymath}z\\end{displaymath}\n\\[w\\]\n\\end{document}");
    assert_eq!(
        formulas(&j),
        [
            "\\begin{equation}\nE = mc^2 \n\\end{equation}",
            "\\begin{align}\na &= b\n\\end{align}",
            "z",
            "w",
        ]
    );
}

#[test]
fn inline_formula_source_linebreak_is_a_space() {
    let j = json("\\begin{document}\nBefore $a +\n b$ and \\begin{math}c+\n d\\end{math} and \\ensuremath{e+\n f} after.\n\\end{document}");
    assert_eq!(formulas(&j), ["a + b", "c+ d", "e+ f"]);
}

#[test]
fn shortcuts_expand_without_changing_definitions() {
    let j = json("\\documentclass{article}\\newcommand{\\be}{\\beta}\\begin{document}Before $\\be+1$ after.\\end{document}");
    assert_eq!(formulas(&j), ["\\beta+1"]);
    for (opening, name, closing, expected) in [
        (
            "\\begin{eqnarray}",
            "eeq",
            "\\end{eqnarray}",
            "\\begin{eqnarray} x&=&y \\end{eqnarray}",
        ),
        ("$$", "eeq", "$$", "x&=&y"),
    ] {
        let tex = format!(
            "\\documentclass{{article}}\\newcommand{{\\beq}}{{{opening}}}\\newcommand{{\\{name}}}{{{closing}}}\
             \\begin{{document}}\\beq x&=&y \\{name}\\end{{document}}"
        );
        assert_eq!(formulas(&json(&tex)), [expected]);
    }
    // Nested argument-less macros expand all the way down.
    let j = json("\\newcommand{\\inner}{x}\\newcommand{\\outer}{\\inner+1}\\begin{document}$\\outer$\\end{document}");
    assert_eq!(formulas(&j), ["x+1"]);
}

// -- front matter (test_metadata) ----------------------------------------------

#[test]
fn front_matter_keeps_roles_and_inline_math() {
    let tex = "\\documentclass{article}\n\\begin{document}\n\\author{C. Bal\\'{a}zs$^{1}$ \\and E.~L.~Berger$^{2}$}\n\
        \\thanks{Current address: Institute $A_2$}\n\\email{berger@example.org}\n\
        \\affiliation{$^{1}$Argonne Laboratory \\\\ $^{2}$Michigan State University}\n\
        \\altaffiliation[Present address:]{Faculty $B_3$}\n\\author{\\parbox{11cm}{Third Author $^3$}}\n\\end{document}";
    let j = json(tex);
    let group_name = |t: &Value| -> Option<String> {
        let parent = t["parent"]["$ref"].as_str()?;
        let g = parent.strip_prefix("#/groups/")?.parse::<usize>().ok()?;
        let name = j["groups"][g]["name"].as_str()?;
        matches!(name, "metadata" | "authors" | "affiliations").then(|| name.to_string())
    };
    let tagged: Vec<&Value> = j["texts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| group_name(t).is_some())
        .collect();
    let names: Vec<String> = tagged.iter().filter_map(|t| group_name(t)).collect();
    assert_eq!(
        names,
        [
            "authors",
            "authors",
            "metadata",
            "metadata",
            "affiliations",
            "affiliations",
            "authors"
        ]
    );
    assert_eq!(tagged[2]["label"], "footnote");
    assert_eq!(tagged[5]["label"], "footnote");
    let markdown = md(tex);
    assert!(!markdown.contains("11cm"), "{markdown}");
    assert!(
        markdown.contains("Present address: Faculty $B_3$"),
        "{markdown}"
    );
    assert!(markdown.contains("C. Balázs$^{1}$"), "{markdown}");
}

#[test]
fn source_roles_do_not_appear_in_default_markdown() {
    let tex = "\\documentclass{article}\n\\usepackage{amsmath}\n\\title{Paper}\n\\begin{document}\nBody.\n\\end{document}";
    assert_eq!(md(tex), "# Paper\n\nBody.");
    assert_eq!(
        furniture(&json(tex)),
        ["\\documentclass{article}", "\\usepackage{amsmath}"]
    );
}

#[test]
fn author_inst_markers_are_inline_formulas() {
    let j = json("\\author{Ann\\inst{1} \\and Ben\\inst{2,3}}\n\\institute{\\inst{1} Uni A \\and \\inst{2} Uni B}\n\
        \\begin{document}\\maketitle\\end{document}");
    assert_eq!(formulas(&j), ["^{1}", "^{2,3}", "^{1}", "^{2}"]);
    let affiliations = j["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "affiliations")
        .unwrap();
    assert_eq!(affiliations["children"].as_array().unwrap().len(), 2);
}

// -- tables (test_tables) ------------------------------------------------------

#[test]
fn tabular_keeps_the_trailing_blank_row() {
    let tex = "\\begin{document}\n\\begin{tabular}{|c|c|}\n\\hline\nH1 & H2 \\\\\n\\hline\n\
        a & b \\\\\n\\hline\n\\end{tabular}\n\\end{document}";
    let markdown = md(tex);
    assert!(
        markdown.contains("| H1   | H2   |\n|------|------|\n| a    | b    |\n|      |      |"),
        "got:\n{markdown}"
    );
}

/// docling#4325: `tabular*`, `tabularx` and `longtable` are tables too;
/// longtable's column specification is an argument (docling's own spec),
/// its `\\endhead` marker no cell content.
#[test]
fn wide_and_long_tables_are_tables() {
    let tex = "\\begin{document}\n\\begin{longtable}{cc}\nH1 & H2 \\\\ \\endhead\na & b \\\\\n\\end{longtable}\n\
        \\begin{tabularx}{\\textwidth}{XX}\nc & d \\\\\n\\end{tabularx}\n\\end{document}";
    let j = json(tex);
    let tables = j["tables"].as_array().unwrap();
    assert_eq!(tables.len(), 2, "{j}");
    let cells = |t: usize| -> Vec<String> {
        tables[t]["data"]["table_cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["text"].as_str().unwrap().to_string())
            .collect()
    };
    // Each ends with docling's trailing blank row (the newline after
    // the last `\\` is a chars node that finishes one more row).
    assert_eq!(cells(0), ["H1", "H2", "a", "b", "", ""]);
    assert_eq!(cells(1), ["c", "d", "", ""]);
}

#[test]
fn table_cells_with_math_are_rich_cells() {
    let tex =
        "\\begin{document}\n\\begin{tabular}{cc}\na & $b_1$ \\\\\n\\end{tabular}\n\\end{document}";
    let j = json(tex);
    let cell = &j["tables"][0]["data"]["table_cells"][1];
    assert_eq!(cell["text"], "");
    let group = resolve(&j, &cell["ref"]);
    assert_eq!(group["parent"]["$ref"], "#/tables/0");
    assert_eq!(formulas(&j), ["b_1"]);
    assert!(md(tex).contains("| a   | $b_1$   |"), "{}", md(tex));
}

#[test]
fn table_captions_print_before_the_table() {
    let tex = "\\begin{document}\n\\begin{table}\n\\begin{tabular}{c}\nx \\\\\n\\end{tabular}\n\
        \\caption{Tab $t$ cap}\n\\end{table}\n\\end{document}";
    let j = json(tex);
    let caption = resolve(&j, &j["tables"][0]["captions"][0]);
    assert_eq!(caption["label"], "caption");
    assert!(md(tex).starts_with("Tab $t$ cap\n\n| x   |"), "{}", md(tex));
}

#[test]
fn multicolumn_span_is_clamped() {
    // docling#4974: a declared span past the table's own columns does not
    // widen it, and none past the HTML caps is honoured.
    let j = json("\\begin{tabular}{cc}\\multicolumn{1000000}{c}{x} & y \\\\ a & b\\end{tabular}");
    let data = &j["tables"][0]["data"];
    // The capped 1000 columns of the span, then `y` and the empty second
    // row's cell: upstream's 1002.
    assert_eq!(data["num_cols"], 1002);
    assert_eq!(data["table_cells"][0]["col_span"], 1000);
    assert_eq!(clamp_span("1000000", MAX_COLSPAN), 1000);
    assert_eq!(clamp_span("-3", MAX_COLSPAN), 1);
    assert_eq!(clamp_span("x", MAX_ROWSPAN), 1);
    assert_eq!(clamp_span(" 2 ", MAX_ROWSPAN), 2);
    assert_eq!(clamp_span("99999999999999999999999", MAX_ROWSPAN), 65534);
}

#[test]
fn table_width_follows_rowspans() {
    // A rowspan shifts the next row's cells right; a span wider than the
    // table does not widen it.
    assert_eq!(
        table_width(&[vec![(1, 2), (1, 1)], vec![(1, 1), (1, 1)]]),
        3
    );
    assert_eq!(table_width(&[vec![(5, 1)], vec![(1, 1), (1, 1)]]), 2);
}

// -- environments, pictures ----------------------------------------------------

#[test]
fn tikzpicture_keeps_its_source_as_code_meta() {
    let tex = "\\begin{document}\nBefore.\n\\begin{tikzpicture}\n\\draw (0,0) -- (1,1);\n\\end{tikzpicture}\nAfter.\n\\end{document}";
    let j = json(tex);
    let code = &j["pictures"][0]["meta"]["code"];
    assert_eq!(code["language"], "Tikz");
    assert_eq!(
        code["text"],
        "\\begin{tikzpicture}\n\\draw (0,0) -- (1,1);\n\\end{tikzpicture}"
    );
    // docling-core prints the meta's Python repr into the Markdown; here the
    // picture is its placeholder.
    assert_eq!(md(tex), "Before.\n\n<!-- image -->\n\nAfter.");
}

#[test]
fn collapse_and_preprocess_helpers() {
    assert_eq!(collapse_layout_whitespace("a  b\n\t c\r\nd"), "a  b c d");
    let spans = macro_definition_spans(
        &"x \\def\\a{b{c}} \\newcommand*{\\z}[1][d]{e} y"
            .chars()
            .collect::<Vec<_>>(),
    );
    assert_eq!(spans, [(2, 14), (15, 40)]);
    assert_eq!(
        strip_latex_comments("\\foo%c\nbar \\% kept"),
        "\\foo bar \\% kept"
    );
    assert_eq!(
        replace_shortcuts(
            "\\be x \\bea \\beq",
            &[
                ("be".into(), "\\begin{equation}".into()),
                ("beq".into(), "$$".into())
            ]
        ),
        "\\begin{equation} x \\bea $$"
    );
}
