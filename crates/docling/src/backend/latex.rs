//! LaTeX backend — a port of docling's `LatexDocumentBackend` (#466), as of
//! docling 2.142 (docling#4817's arXiv rewrite, plus #4974, #4517, #4644,
//! #4579, #4325 and #4340).
//!
//! Upstream parses the source with pylatexenc and walks the node stream
//! with a handful of handler mixins (`handlers/macros.py`,
//! `handlers/environments.py`, `handlers/math.py`, `handlers/metadata.py`,
//! `utils/text.py`, `utils/table.py`); [`super::latex_walker`] is the parser
//! port and this module the handlers, function for function, including the
//! corners that shape upstream's groundtruth:
//!
//! - macro definitions are masked out of the source before the parse (their
//!   text kept as furniture-layer items), the zero-argument ones registered
//!   ahead of pylatexenc's specs, and shortcut aliases (`\def\be{\begin{equation}}`)
//!   expanded in the text;
//! - a text buffer of strings and inline formulas, flushed by structural
//!   macros, environments and paragraph breaks into one item — a buffer with
//!   inline math becomes an empty item holding an `inline` group of text and
//!   formula parts (headings, captions and list items alike);
//! - front matter in `metadata` / `authors` / `affiliations` groups,
//!   `\and`-split, `\inst` markers as superscript formulas;
//! - a `tabular` read cell by cell, a cell with math a rich cell, with
//!   `\multicolumn` and `\multirow` looked up at absolute source offsets
//!   (upstream slices the environment's own text with the node's document
//!   position, so the lookup usually finds nothing and the macro's groups
//!   become cell text);
//! - whitespace runs holding a line break collapsed in every item but
//!   formulas and code.
//!
//! The TikZ / Tectonic rendering is left out (no engine here: a
//! `tikzpicture` is a payload-less picture keeping its source as the
//! picture's `meta.code`, upstream's fallback without Tectonic). Picture payloads are
//! what upstream embeds: a raster figure decoded and re-encoded as PNG (as
//! `ImageRef.from_pil` does), a PDF figure's first page rendered at 144 dpi —
//! by docling-pdf's renderer where upstream calls pypdfium2, so the pixels
//! (not the size) differ, and only in a build with the `pdf` feature.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use super::latex_walker::{Kind, Node as LNode, Walker};
use super::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::tree::{ItemTree, ListMeta, TreeKind};
use docling_core::{
    ContentLayer, DoclingDocument, InlineRun, Node, PictureImage, Table, TableCell,
};

pub struct LatexBackend;

impl DeclarativeBackend for LatexBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = decode_latex(&source.bytes);
        let mut conv = Converter {
            base_dir: source
                .path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            custom_macros: Vec::new(),
            custom_macro_num_args: HashMap::new(),
            masked_macro_spans: Vec::new(),
            suppress_image_path_caption: false,
            input_stack: HashSet::new(),
            labels: HashSet::new(),
            metadata_groups: HashMap::new(),
            role_groups: HashMap::new(),
            figures: HashMap::new(),
            doc: Builder::default(),
        };
        conv.run(&text);
        let mut doc = DoclingDocument::new(&source.name);
        let (nodes, mut tree) = conv.doc.finish();
        // The tree is docling's own, so DocLang serializes it as docling-core
        // does (#884); the flat nodes lose footnote labels, inline formulas
        // in captions and cells, and raw text.
        tree.doclang = true;
        doc.nodes = nodes;
        doc.tree = Some(tree);
        Ok(doc)
    }
}

/// `decode_latex_content`: UTF-8, then cp1252, then latin-1 (which never
/// fails) — the main file and every `\input` alike (docling#4441, #4475).
/// Upstream reads a path in text mode, so line endings are translated
/// (`\r\n` and a lone `\r` become `\n`) before anything sees the text.
fn decode_latex(bytes: &[u8]) -> String {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        // Python's cp1252 leaves five bytes undefined and fails on them.
        Err(_)
            if bytes
                .iter()
                .any(|b| matches!(b, 0x81 | 0x8D | 0x8F | 0x90 | 0x9D)) =>
        {
            bytes.iter().map(|&b| b as char).collect()
        }
        Err(_) => encoding_rs::WINDOWS_1252
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
    };
    if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// Constants (`constants.py`)

const MACROS_NEWCOMMAND: &[&str] = &["newcommand", "renewcommand", "providecommand"];
const MACROS_METADATA: &[&str] = &[
    "title",
    "author",
    "date",
    "affiliation",
    "affil",
    "address",
    "institute",
    "email",
    "altaffiliation",
    "thanks",
    "footnote",
    "preprint",
];
const MACROS_INLINE_VERBATIM: &[&str] = &["%", "$", "&", "#", "_", "{", "}", "~"];
const MACROS_TEXT_FORMATTING: &[&str] = &["textbf", "textit", "emph", "texttt", "underline"];
const MACROS_CITATION: &[&str] = &["cite", "citep", "citet", "ref", "eqref"];
const MACROS_COLOR: &[&str] = &["color", "definecolor", "colorlet"];
const MACROS_COLOR_INLINE: &[&str] = &["textcolor", "colorbox"];
const MACROS_STRUCTURAL: &[&str] = &[
    "section",
    "subsection",
    "subsubsection",
    "chapter",
    "part",
    "paragraph",
    "subparagraph",
    "caption",
    "captionof",
    "label",
    "includegraphics",
    "bibliography",
    "title",
    "author",
    "date",
    "affiliation",
    "affil",
    "address",
    "institute",
    "email",
    "altaffiliation",
    "thanks",
    "preprint",
    "maketitle",
    "footnote",
    "marginpar",
    "textsc",
    "textsf",
    "textrm",
    "textnormal",
    "mbox",
    "href",
    "newline",
    "hfill",
    "break",
    "centering",
    "textcolor",
    "colorbox",
    "item",
    "input",
    "include",
];
const MACROS_HEADING: &[&str] = &[
    "part",
    "chapter",
    "section",
    "subsection",
    "subsubsection",
    "paragraph",
    "subparagraph",
];
const MACROS_TEXT_STYLE: &[&str] = &["textsc", "textsf", "textrm", "textnormal", "mbox"];
const MACROS_IGNORED: &[&str] = &[
    "documentclass",
    "usepackage",
    "geometry",
    "hypersetup",
    "lstset",
    "bibliographystyle",
    "newcommand",
    "renewcommand",
    "def",
    "let",
    "edef",
    "gdef",
    "xdef",
    "newenvironment",
    "renewenvironment",
    "DeclareMathOperator",
    "DeclareMathSymbol",
    "setlength",
    "setcounter",
    "addtolength",
    "color",
    "definecolor",
    "colorlet",
    "AtBeginDocument",
    "AtEndDocument",
    "newlength",
    "newcounter",
    "newif",
    "providecommand",
    "DeclareOption",
    "RequirePackage",
    "ProvidesPackage",
    "LoadClass",
    "makeatletter",
    "makeatother",
    "NeedsTeXFormat",
    "ProvidesClass",
    "DeclareRobustCommand",
    "newtheorem",
    "theoremstyle",
    "newtheoremstyle",
    "documentstyle",
    "pagestyle",
    "thispagestyle",
    "pagenumbering",
    "tableofcontents",
    "listoffigures",
    "listoftables",
    "appendix",
    "cleardoublepage",
    "clearpage",
    "newpage",
    "markboth",
    "markright",
    "lhead",
    "rhead",
    "cfoot",
    "hyphenation",
    "overfullrule",
    "protect",
];
/// Accent macros (docling#4517): converted with latex2text's combining marks.
const MACROS_ACCENTS: &[&str] = &[
    "'", "\"", "^", "`", "~", "=", ".", "c", "d", "b", "H", "k", "r", "t", "u", "v",
];
/// Text-mode letters typed as argument-less macros: `\o` (ø), `\ss` (ß).
const MACROS_LETTERS: &[&str] = &[
    "o", "O", "l", "L", "i", "j", "ss", "aa", "AA", "ae", "AE", "oe", "OE",
];
const MACROS_SPACING: &[&str] = &[
    "newline",
    "hfill",
    "break",
    "centering",
    "noindent",
    "par",
    "smallskip",
    "medskip",
    "bigskip",
    "vfill",
    "vskip",
    "hskip",
    "vspace",
    "hspace",
];
const MACROS_ESCAPED: &[&str] = &["&", "%", "$", "#", "_", "{", "}"];
const ENV_MATH_DISPLAY_PREFIXES: &[&str] = &[
    "$$",
    "\\[",
    "\\begin{equation}",
    "\\begin{align}",
    "\\begin{gather}",
    "\\begin{displaymath}",
];
/// The environments whose delimiters `_clean_math` strips — since
/// docling#4817 a numbered `equation` keeps its `\begin`/`\end` (an `align`
/// would not be valid without them).
const ENV_MATH_CLEAN: &[&str] = &["displaymath", "math", "dmath", "dmath*"];
const ENV_MATH: &[&str] = &[
    "equation",
    "align",
    "gather",
    "multline",
    "flalign",
    "alignat",
    "displaymath",
    "eqnarray",
    "dmath",
    "dgroup",
    "darray",
];
const ENV_THEOREM: &[&str] = &[
    "theorem",
    "lemma",
    "corollary",
    "proposition",
    "definition",
    "remark",
    "example",
    "conjecture",
];
const ENV_LIST: &[&str] = &["itemize", "enumerate", "description"];
const ENV_QUOTE: &[&str] = &["quote", "quotation", "verse"];
const TABLE_MACROS_RULE: &[&str] = &[
    "hline",
    "cline",
    "toprule",
    "midrule",
    "bottomrule",
    "cmidrule",
    "specialrule",
];
const TABLE_MACROS_IGNORE: &[&str] = &[
    "rule",
    "vspace",
    "hspace",
    "vskip",
    "hskip",
    "smallskip",
    "medskip",
    "bigskip",
    "strut",
    "phantom",
    "hphantom",
    "vphantom",
    "noalign",
    // longtable's repeating header/footer markers are not cell content
    // (docling#4325).
    "endhead",
    "endfirsthead",
    "endfoot",
    "endlastfoot",
];

/// The `\input` nesting upstream allows.
const MAX_INPUT_DEPTH: usize = 10;
/// `table_spans.py`'s caps, the HTML table model's (docling#4974).
const MAX_COLSPAN: i64 = 1000;
const MAX_ROWSPAN: i64 = 65534;

// ---------------------------------------------------------------------------
// Nodes: the walker's nodes, each carrying the source it was parsed from —
// cell content re-parsed from a `\multicolumn` argument, an `\input` file and
// a custom macro's expansion each have their own — plus the two kinds
// upstream splices into a node list itself.

#[derive(Debug, Clone)]
struct N {
    kind: K,
    pos: usize,
    len: usize,
    src: Rc<str>,
}

#[derive(Debug, Clone)]
enum K {
    Chars(String),
    Group(Vec<N>),
    Comment,
    Macro {
        name: String,
        args: Option<Vec<Option<N>>>,
    },
    Env {
        name: String,
        nodelist: Vec<N>,
        args: Option<Vec<Option<N>>>,
    },
    Specials(String),
    Math {
        display: bool,
        nodelist: Vec<N>,
    },
    /// handlers/math.py's `InlineFormula`, which the metadata handler puts
    /// in a node list for an `\inst` marker.
    Formula(String),
}

impl N {
    /// A `LatexCharsNode(chars=…)` built by a handler.
    fn chars(text: &str) -> N {
        N {
            kind: K::Chars(text.to_string()),
            pos: 0,
            len: text.len(),
            src: Rc::from(text),
        }
    }

    fn verbatim(&self) -> &str {
        self.src.get(self.pos..self.pos + self.len).unwrap_or("")
    }

    fn macro_name(&self) -> Option<&str> {
        match &self.kind {
            K::Macro { name, .. } => Some(name),
            _ => None,
        }
    }

    fn env_name(&self) -> Option<&str> {
        match &self.kind {
            K::Env { name, .. } => Some(name),
            _ => None,
        }
    }

    fn args(&self) -> Option<&[Option<N>]> {
        match &self.kind {
            K::Macro { args, .. } | K::Env { args, .. } => args.as_deref(),
            _ => None,
        }
    }

    /// `node.nodeargd and node.nodeargd.argnlist`.
    fn has_args(&self) -> bool {
        self.args().is_some_and(|a| !a.is_empty())
    }

    /// The last argument (`argnlist[-1]`), when it is present.
    fn last_arg(&self) -> Option<&N> {
        self.args().and_then(|a| a.last()).and_then(Option::as_ref)
    }

    /// The node list of a group node.
    fn group_list(&self) -> Option<&[N]> {
        match &self.kind {
            K::Group(list) => Some(list),
            _ => None,
        }
    }

    /// `iter_child_nodelists`: the node lists of a group, environment or
    /// math node and of a macro's or environment's group arguments.
    fn child_lists(&self) -> Vec<&[N]> {
        let mut out: Vec<&[N]> = Vec::new();
        match &self.kind {
            K::Group(list) | K::Env { nodelist: list, .. } | K::Math { nodelist: list, .. } => {
                out.push(list)
            }
            _ => {}
        }
        if let Some(args) = self.args() {
            for arg in args.iter().flatten() {
                if let K::Group(list) = &arg.kind {
                    out.push(list);
                }
            }
        }
        out
    }
}

/// The walker's nodes over `src`, owned and source-carrying.
fn own(nodes: Vec<LNode>, src: &Rc<str>) -> Vec<N> {
    nodes.into_iter().map(|n| own_one(n, src)).collect()
}

fn own_one(n: LNode, src: &Rc<str>) -> N {
    let own_args = |args: Option<super::latex_walker::Args>| {
        args.map(|a| {
            a.argnlist
                .into_iter()
                .map(|x| x.map(|x| own_one(x, src)))
                .collect()
        })
    };
    let kind = match n.kind {
        Kind::Chars(c) => K::Chars(c),
        Kind::Group { nodelist, .. } => K::Group(own(nodelist, src)),
        Kind::Comment { .. } => K::Comment,
        Kind::Macro { name, args, .. } => K::Macro {
            name,
            args: own_args(args),
        },
        Kind::Env {
            name,
            nodelist,
            args,
        } => K::Env {
            name,
            nodelist: own(nodelist, src),
            args: own_args(args),
        },
        Kind::Specials(s) => K::Specials(s),
        Kind::Math {
            display, nodelist, ..
        } => K::Math {
            display,
            nodelist: own(nodelist, src),
        },
    };
    N {
        kind,
        pos: n.pos,
        len: n.len,
        src: src.clone(),
    }
}

/// `LatexWalker(text, tolerant_parsing=True, latex_context=LATEX_CONTEXT_DB)`.
fn parse(text: &str) -> Vec<N> {
    let src: Rc<str> = Rc::from(text);
    let nodes = Walker::new(&src).parse();
    own(nodes, &src)
}

/// One piece of the text buffer: a string, or handlers/math.py's
/// `InlineFormula`.
#[derive(Debug, Clone)]
enum Part {
    Text(String),
    Formula(String),
}

/// Where a node list is processed: `_process_nodes`'s `parent`, `text_label`
/// and `heading_level`.
#[derive(Debug, Clone, Copy)]
struct Cx {
    parent: Option<usize>,
    label: Option<&'static str>,
    heading: Option<u8>,
}

impl Cx {
    fn new(parent: Option<usize>, label: Option<&'static str>) -> Self {
        Cx {
            parent,
            label,
            heading: None,
        }
    }
}

// ---------------------------------------------------------------------------
// The document builder: docling's `DoclingDocument` add_* calls recorded as
// the item tree (JSON, HTML, LaTeX); the flat nodes the Markdown, DocLang and
// chunk exports read are derived from it once it is complete.

#[derive(Default)]
struct Builder {
    tree: ItemTree,
}

impl Builder {
    fn add_text(&mut self, parent: Option<usize>, label: &str, text: &str) -> usize {
        let list = (label == "list_item").then(ListMeta::default);
        self.tree.add(
            parent,
            None,
            TreeKind::Text {
                label: label.into(),
                text: text.into(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list,
            },
        )
    }

    fn add_furniture_text(&mut self, text: &str) -> usize {
        self.tree.add(
            None,
            Some(ContentLayer::Furniture),
            TreeKind::Text {
                label: "text".into(),
                text: text.into(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: None,
                list: None,
            },
        )
    }

    fn add_code(&mut self, parent: Option<usize>, text: &str) {
        self.tree.add(
            parent,
            None,
            TreeKind::Code {
                text: text.into(),
                orig: None,
                language: None,
                formatting: None,
                hyperlink: None,
                captions: Vec::new(),
            },
        );
    }

    fn add_heading(&mut self, parent: Option<usize>, text: &str, level: u8) -> usize {
        self.tree.add(
            parent,
            None,
            TreeKind::Text {
                label: "section_header".into(),
                text: text.into(),
                orig: None,
                formatting: None,
                hyperlink: None,
                level: Some(level),
                list: None,
            },
        )
    }

    fn add_group(&mut self, parent: Option<usize>, name: &str, label: &str) -> usize {
        self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: label.into(),
                name: name.into(),
            },
        )
    }

    fn add_table(&mut self, parent: Option<usize>, table: Table) -> usize {
        self.tree.add(
            parent,
            None,
            TreeKind::Table {
                table,
                rich_cells: Vec::new(),
                captions: Vec::new(),
            },
        )
    }

    /// `add_picture(parent, caption, image)`: the caption item, created
    /// beforehand on the body, is referenced by the picture.
    fn add_picture(
        &mut self,
        parent: Option<usize>,
        caption: Option<usize>,
        image: Option<PictureImage>,
        dpi: Option<u32>,
    ) -> usize {
        self.tree.add(
            parent,
            None,
            TreeKind::Picture {
                captions: caption.into_iter().collect(),
                image,
                extra_classes: Vec::new(),
                classification: None,
                description: None,
                confidence: None,
                chart: None,
                dpi,
            },
        )
    }

    fn is_text(&self, id: usize) -> bool {
        matches!(
            self.tree.items[id].kind,
            TreeKind::Text { .. } | TreeKind::Code { .. }
        )
    }

    fn label(&self, id: usize) -> Option<&str> {
        match &self.tree.items[id].kind {
            TreeKind::Text { label, .. } => Some(label),
            _ => None,
        }
    }

    /// The caption-labelled text items created from item `start` on —
    /// `doc.texts[caption_start:]` filtered by label.
    fn captions_since(&self, start: usize) -> Vec<usize> {
        (start..self.tree.items.len())
            .filter(|&id| self.is_text(id) && self.label(id) == Some("caption"))
            .collect()
    }

    /// The first item of `want` kind created from item `start` on —
    /// `doc.pictures[picture_start]` / `doc.tables[table_start]`.
    fn first_since(&self, start: usize, want: fn(&TreeKind) -> bool) -> Option<usize> {
        (start..self.tree.items.len()).find(|&id| want(&self.tree.items[id].kind))
    }

    fn extend_captions(&mut self, id: usize, more: Vec<usize>) {
        match &mut self.tree.items[id].kind {
            TreeKind::Picture { captions, .. } | TreeKind::Table { captions, .. } => {
                captions.extend(more)
            }
            _ => {}
        }
    }

    /// `_normalize_document_text`, then the flat node list in tree order.
    fn finish(mut self) -> (Vec<Node>, ItemTree) {
        for item in &mut self.tree.items {
            if item.layer.is_some() {
                continue;
            }
            match &mut item.kind {
                TreeKind::Text { label, text, .. } if label != "formula" => {
                    *text = collapse_layout_whitespace(text);
                }
                TreeKind::Table { table, .. } => {
                    if let Some(cells) = &mut table.cells {
                        for c in cells.iter_mut() {
                            c.text = collapse_layout_whitespace(&c.text);
                        }
                    }
                    table.rows = table_grid(table.cells.as_deref().unwrap_or(&[]), &table.rows);
                }
                _ => {}
            }
        }
        let claimed: HashSet<usize> = self
            .tree
            .items
            .iter()
            .flat_map(|item| match &item.kind {
                TreeKind::Picture { captions, .. } | TreeKind::Table { captions, .. } => {
                    captions.clone()
                }
                _ => Vec::new(),
            })
            .collect();
        let flat = Flat {
            tree: &self.tree,
            claimed,
        };
        let nodes = self
            .tree
            .body
            .iter()
            .flat_map(|&id| flat.nodes(id, false, 0))
            .collect();
        (nodes, self.tree)
    }
}

/// The flat rendering of the finished tree, docling-core's Markdown
/// serializer's view of it: an empty item holding one inline group is that
/// group's parts joined by spaces (`$…$` around a formula), a picture or
/// table prints its captions, a caption it claims is skipped where it sits.
struct Flat<'t> {
    tree: &'t ItemTree,
    claimed: HashSet<usize>,
}

impl Flat<'_> {
    /// The item's nodes: one, or — for a list nested in a list, which
    /// docling-core's serializer indents under the item before it — its
    /// items one level deeper, spliced into the enclosing list's run.
    fn nodes(&self, id: usize, first_in_list: bool, list_level: u8) -> Vec<Node> {
        let item = &self.tree.items[id];
        if let TreeKind::Group { label, .. } = &item.kind {
            if label == "list" && list_level > 0 {
                return item
                    .children
                    .iter()
                    .flat_map(|&c| self.nodes(c, false, list_level + 1))
                    .collect();
            }
        }
        self.node(id, first_in_list, list_level.saturating_sub(1))
            .into_iter()
            .collect()
    }

    /// One item's node; `level` is a list item's nesting depth.
    fn node(&self, id: usize, first_in_list: bool, level: u8) -> Option<Node> {
        let item = &self.tree.items[id];
        if item.layer == Some(ContentLayer::Furniture) {
            let inner = match &item.kind {
                TreeKind::Text { text, .. } => Node::Paragraph {
                    text: escape_text(text),
                },
                _ => return None,
            };
            return Some(Node::Furniture {
                layer: ContentLayer::Furniture,
                inner: Box::new(inner),
            });
        }
        match &item.kind {
            TreeKind::Group { label, name } => {
                if label == "inline" {
                    return None;
                }
                let is_list = label == "list";
                let mut first = is_list;
                let mut children = Vec::new();
                for &c in &item.children {
                    let is_item = self.tree.items[c].kind_label() == Some("list_item");
                    children.extend(self.nodes(c, first && is_item, u8::from(is_list)));
                    if is_item {
                        first = false;
                    }
                }
                if children.is_empty() {
                    return None;
                }
                Some(Node::Group {
                    label: label.clone(),
                    name: Some(name.clone()),
                    layer: None,
                    children,
                })
            }
            TreeKind::Text {
                label,
                text,
                level: heading,
                ..
            } => {
                if self.claimed.contains(&id) {
                    return None;
                }
                if label == "formula" {
                    return Some(Node::Formula {
                        latex: text.clone(),
                        orig: text.clone(),
                        location: None,
                    });
                }
                let inline = self.inline_of(id);
                let md = match &inline {
                    Some((md, _)) => md.clone(),
                    None => escape_text(text),
                };
                Some(match label.as_str() {
                    "title" => Node::Heading { level: 1, text: md },
                    "section_header" => Node::Heading {
                        level: heading.unwrap_or(1).saturating_add(1),
                        text: md,
                    },
                    "list_item" => Node::ListItem {
                        ordered: false,
                        number: 0,
                        first_in_list,
                        text: md,
                        level,
                        marker: None,
                        location: None,
                        dclx: None,
                        href: None,
                        layer: None,
                    },
                    _ => match inline {
                        Some((md_text, runs)) => Node::InlineGroup {
                            unwrapped: false,
                            runs,
                            md_text,
                        },
                        None => Node::Paragraph { text: md },
                    },
                })
            }
            TreeKind::Code { text, .. } => Some(Node::Code {
                language: None,
                text: text.clone(),
                orig: None,
                pretty: None,
            }),
            TreeKind::Table {
                table,
                rich_cells,
                captions,
            } => {
                let mut flat = table.clone();
                if let Some(cells) = &mut flat.cells {
                    for &(row, col, group) in rich_cells {
                        let md = self.group_md(group);
                        for c in cells.iter_mut() {
                            if c.start_row == row && c.start_col == col {
                                c.text = md.clone();
                            }
                        }
                    }
                }
                flat.rows = table_grid(flat.cells.as_deref().unwrap_or(&[]), &flat.rows);
                flat.structure = Some(span_structure(
                    flat.cells.as_deref().unwrap_or(&[]),
                    &flat.rows,
                ));
                flat.caption = self.captions_md(captions);
                Some(Node::Table(flat))
            }
            TreeKind::Picture {
                captions, image, ..
            } => Some(Node::Picture {
                caption: self.captions_md(captions),
                caption_href: None,
                image: image.clone(),
                classification: None,
                description: None,
                caption_parent: Default::default(),
                caption_location: None,
            }),
            _ => None,
        }
    }

    /// An empty text item's one `inline` group child, rendered.
    fn inline_of(&self, id: usize) -> Option<(String, Vec<InlineRun>)> {
        let item = &self.tree.items[id];
        let TreeKind::Text { text, .. } = &item.kind else {
            return None;
        };
        if !text.is_empty() || item.children.len() != 1 {
            return None;
        }
        let group = item.children[0];
        match &self.tree.items[group].kind {
            TreeKind::Group { label, .. } if label == "inline" => Some(self.render_inline(group)),
            _ => None,
        }
    }

    fn render_inline(&self, group: usize) -> (String, Vec<InlineRun>) {
        let mut parts = Vec::new();
        let mut runs = Vec::new();
        for &c in &self.tree.items[group].children {
            if let TreeKind::Text { label, text, .. } = &self.tree.items[c].kind {
                if text.is_empty() {
                    continue;
                }
                let formula = label == "formula";
                parts.push(if formula {
                    format!("${text}$")
                } else {
                    escape_text(text)
                });
                runs.push(InlineRun {
                    text: text.clone(),
                    formula,
                    ..Default::default()
                });
            }
        }
        (parts.join(" "), runs)
    }

    /// An item's own Markdown text (an inline group's, or its escaped text).
    fn item_md(&self, id: usize) -> String {
        if let Some((md, _)) = self.inline_of(id) {
            return md;
        }
        match &self.tree.items[id].kind {
            TreeKind::Text { label, text, .. } if label == "formula" => format!("$${text}$$"),
            TreeKind::Text { text, .. } => escape_text(text),
            TreeKind::Group { .. } => self.group_md(id),
            _ => String::new(),
        }
    }

    /// A rich cell's group: its items' texts, space-joined.
    fn group_md(&self, group: usize) -> String {
        let parts: Vec<String> = self.tree.items[group]
            .children
            .iter()
            .map(|&c| self.item_md(c))
            .filter(|s| !s.is_empty())
            .collect();
        parts.join(" ")
    }

    /// `serialize_captions`: each caption's text and inline parts, the
    /// captions space-joined.
    fn captions_md(&self, captions: &[usize]) -> Option<String> {
        let parts: Vec<String> = captions
            .iter()
            .map(|&c| self.item_md(c))
            .filter(|s| !s.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

trait KindLabel {
    fn kind_label(&self) -> Option<&str>;
}

impl KindLabel for docling_core::tree::TreeItem {
    fn kind_label(&self) -> Option<&str> {
        match &self.kind {
            TreeKind::Text { label, .. } => Some(label),
            _ => None,
        }
    }
}

/// docling's `TableData.grid`: every slot a cell covers shows its text,
/// later cells overwriting earlier ones.
fn table_grid(cells: &[TableCell], rows: &[Vec<String>]) -> Vec<Vec<String>> {
    let num_rows = rows.len();
    let num_cols = rows.first().map_or(0, Vec::len);
    let mut grid = vec![vec![String::new(); num_cols]; num_rows];
    for c in cells {
        let rows = grid
            .iter_mut()
            .take((c.start_row + c.row_span).min(num_rows))
            .skip(c.start_row);
        for row in rows {
            let slots = row
                .iter_mut()
                .take((c.start_col + c.col_span).min(num_cols))
                .skip(c.start_col);
            for slot in slots {
                *slot = c.text.clone();
            }
        }
    }
    grid
}

/// The DocLang overlay of a LaTeX table: no header row (the backend flags no
/// `column_header` cell, so docling writes `<fcel/>` throughout) and the
/// slots a span covers as `<lcel/>` / `<ucel/>`.
fn span_structure(cells: &[TableCell], rows: &[Vec<String>]) -> docling_core::TableStructure {
    let num_rows = rows.len();
    let num_cols = rows.first().map_or(0, Vec::len);
    let mut col_continuation = vec![vec![false; num_cols]; num_rows];
    let mut row_continuation = vec![vec![false; num_cols]; num_rows];
    for c in cells {
        for r in c.start_row..(c.start_row + c.row_span).min(num_rows) {
            for col in c.start_col..(c.start_col + c.col_span).min(num_cols) {
                col_continuation[r][col] = col > c.start_col;
                row_continuation[r][col] = r > c.start_row;
            }
        }
    }
    docling_core::TableStructure {
        header_row: vec![false; num_rows],
        col_continuation,
        row_continuation,
        row_header: Vec::new(),
        col_header: Vec::new(),
    }
}

/// `collapse_source_layout_whitespace`: every whitespace run holding a tab
/// or a line break becomes one space; runs of plain spaces stay.
fn collapse_layout_whitespace(text: &str) -> String {
    if !text.contains(['\t', '\r', '\n']) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.contains(['\t', '\r', '\n']) {
            out.push(' ');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for ch in text.chars() {
        if matches!(ch, ' ' | '\t' | '\r' | '\n') {
            run.push(ch);
        } else {
            if !run.is_empty() {
                flush(&mut run, &mut out);
            }
            out.push(ch);
        }
    }
    if !run.is_empty() {
        flush(&mut run, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// The converter (`LatexDocumentBackend` + mixins)

struct Converter {
    base_dir: Option<PathBuf>,
    /// `\newcommand` / `\def` definitions in registration order (Python's
    /// dict order), name → body.
    custom_macros: Vec<(String, String)>,
    custom_macro_num_args: HashMap<String, usize>,
    /// The masked definitions, as char ranges of the source.
    masked_macro_spans: Vec<(usize, usize)>,
    /// Inside a `figure`: `\includegraphics` adds no `Image: <path>` caption.
    suppress_image_path_caption: bool,
    input_stack: HashSet<PathBuf>,
    labels: HashSet<String>,
    /// The metadata handler's `metadata` group per parent (`None` = body)
    /// and its `authors` / `affiliations` groups.
    metadata_groups: HashMap<Option<usize>, usize>,
    role_groups: HashMap<(Option<usize>, &'static str), usize>,
    /// `\includegraphics` payloads rendered ahead of the walk, on worker
    /// threads (`prerender_figures`), by the macro's argument text.
    figures: HashMap<String, Figure>,
    doc: Builder,
}

impl Converter {
    fn run(&mut self, latex_text: &str) {
        let preprocessed = self.preprocess_custom_macros(latex_text);
        let zero_arg: HashSet<String> = self
            .custom_macro_num_args
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(name, _)| name.clone())
            .collect();
        let src: Rc<str> = Rc::from(preprocessed.as_str());
        let nodes = own(Walker::with_zero_arg_macros(&src, &zero_arg).parse(), &src);
        self.preserve_source_commands(&nodes, latex_text, &src);
        self.extract_custom_macros(&nodes, 0);
        self.process_preamble(&nodes, 0);
        self.prerender_figures(&nodes);
        match find_document_env(&nodes, 0) {
            Some(K::Env { nodelist, .. }) => {
                let nodelist = nodelist.clone();
                self.process_nodes(&nodelist, Cx::new(None, None));
            }
            _ => self.process_nodes(&nodes, Cx::new(None, None)),
        }
    }

    // -- preprocessing (`_preprocess_custom_macros`) ----------------------------

    /// Masks every macro definition out of the text (spaces, line breaks
    /// kept), registering `\newcommand`s and argument-less `\def`s, and
    /// expands the zero-argument shortcuts whose body is an environment
    /// delimiter or `$$` (`\def\be{\begin{equation}}`) where they are used;
    /// `\hfuzz` / `\vfuzz` assignments are dropped.
    fn preprocess_custom_macros(&mut self, latex_text: &str) -> String {
        let chars: Vec<char> = latex_text.chars().collect();
        self.masked_macro_spans = macro_definition_spans(&chars);
        let byte_at = char_byte_offsets(latex_text);
        let mut shortcuts: Vec<(String, String)> = Vec::new();
        let mut out = String::with_capacity(latex_text.len());
        let mut cursor = 0;
        let spans = self.masked_macro_spans.clone();
        for (start, end) in spans {
            out.push_str(&replace_shortcuts(
                &latex_text[byte_at[cursor]..byte_at[start]],
                &shortcuts,
            ));
            let definition = &latex_text[byte_at[start]..byte_at[end]];
            if definition.starts_with("\\newcommand")
                || definition.starts_with("\\renewcommand")
                || definition.starts_with("\\providecommand")
            {
                let nodes = parse(definition);
                self.extract_custom_macros(&nodes, 0);
            } else if let Some(c) = def_re().captures(definition) {
                // A source definition can replace a built-in accent macro;
                // registering it as argument-less keeps the main parse from
                // reading a following math delimiter as its argument.
                self.set_custom_macro(&c[1], strip_latex_comments(&c[2]), 0);
            }
            for (name, expansion) in &self.custom_macros {
                let body = expansion.trim();
                let alias = self.custom_macro_num_args.get(name) == Some(&0)
                    && environment_alias_re().is_match(body);
                shortcuts.retain(|(n, _)| n != name);
                if alias {
                    shortcuts.push((name.clone(), body.to_string()));
                }
            }
            out.extend(
                definition
                    .chars()
                    .map(|c| if c == '\n' { '\n' } else { ' ' }),
            );
            cursor = end;
        }
        out.push_str(&replace_shortcuts(
            &latex_text[byte_at[cursor]..],
            &shortcuts,
        ));
        out
    }

    /// `_preserve_source_commands`: the masked definitions and the ignored
    /// commands of the preamble (and any `\new…` one) as furniture-layer
    /// text items, in source order, ahead of everything else.
    fn preserve_source_commands(&mut self, nodes: &[N], latex_text: &str, src: &str) {
        let byte_at = char_byte_offsets(latex_text);
        let mut commands: Vec<(usize, String)> = self
            .masked_macro_spans
            .iter()
            .map(|&(start, end)| (start, latex_text[byte_at[start]..byte_at[end]].to_string()))
            .collect();
        // Node offsets are bytes; upstream sorts by character offsets.
        let char_of = char_index_of_bytes(src);
        fn visit(
            items: &[N],
            mut in_preamble: bool,
            char_of: &[usize],
            commands: &mut Vec<(usize, String)>,
        ) {
            for node in items {
                if let Some(name) = node.macro_name() {
                    if MACROS_IGNORED.contains(&name) && (in_preamble || name.starts_with("new")) {
                        commands.push((char_of[node.pos], node.verbatim().to_string()));
                    }
                }
                match node.env_name() {
                    Some("document") => {
                        if let K::Env { nodelist, .. } = &node.kind {
                            visit(nodelist, false, char_of, commands);
                        }
                        in_preamble = false;
                    }
                    Some("verbatim" | "lstlisting" | "minted") => {}
                    _ => {
                        for children in node.child_lists() {
                            visit(children, in_preamble, char_of, commands);
                        }
                    }
                }
            }
        }
        visit(nodes, true, &char_of, &mut commands);
        commands.sort();
        for (_, command) in commands {
            self.doc.add_furniture_text(&command);
        }
    }

    // -- custom macros -------------------------------------------------------

    fn custom_macro(&self, name: &str) -> Option<&str> {
        self.custom_macros
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, def)| def.as_str())
    }

    fn is_custom(&self, name: &str) -> bool {
        self.custom_macros.iter().any(|(n, _)| n == name)
    }

    fn num_args(&self, name: &str) -> usize {
        self.custom_macro_num_args.get(name).copied().unwrap_or(0)
    }

    fn set_custom_macro(&mut self, name: &str, def: String, num_args: usize) {
        match self.custom_macros.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = def,
            None => self.custom_macros.push((name.to_string(), def)),
        }
        self.custom_macro_num_args
            .insert(name.to_string(), num_args);
    }

    fn extract_custom_macros(&mut self, nodes: &[N], depth: usize) {
        if depth > 10 {
            return;
        }
        for node in nodes {
            if let (Some(name), Some(list)) = (node.macro_name(), node.args()) {
                if MACROS_NEWCOMMAND.contains(&name) && !list.is_empty() {
                    let name_arg = list.get(1).and_then(Option::as_ref);
                    let num_args_arg = list.get(2).and_then(Option::as_ref);
                    let def_arg = list.iter().rev().find_map(Option::as_ref);
                    if let (Some(name_arg), Some(def_arg)) = (name_arg, def_arg) {
                        if !std::ptr::eq(name_arg, def_arg) {
                            let raw = name_arg.verbatim();
                            let mut macro_name =
                                raw.trim_matches(|c| "{} \n\t".contains(c)).to_string();
                            if let Some(rest) = macro_name.strip_prefix('\\') {
                                macro_name = rest.to_string();
                            }
                            let macro_def = if matches!(def_arg.kind, K::Group(_)) {
                                let v = def_arg.verbatim();
                                match v.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
                                    Some(inner) => inner.to_string(),
                                    None => v.to_string(),
                                }
                            } else {
                                def_arg
                                    .verbatim()
                                    .trim_matches(|c| "{} ".contains(c))
                                    .to_string()
                            };
                            if !macro_name.is_empty() {
                                let n = parse_custom_macro_num_args(num_args_arg);
                                self.set_custom_macro(
                                    &macro_name,
                                    strip_latex_comments(&macro_def),
                                    n,
                                );
                            }
                        }
                    }
                }
            }
            for children in node.child_lists() {
                self.extract_custom_macros(children, depth + 1);
            }
        }
    }

    // -- metadata (`handlers/metadata.py`) ----------------------------------------

    /// `process_preamble`: the front-matter commands before the `document`
    /// environment, at any depth.
    fn process_preamble(&mut self, nodes: &[N], depth: usize) {
        if depth > 10 {
            return;
        }
        for node in nodes {
            if node.env_name() == Some("document") {
                break;
            }
            if node
                .macro_name()
                .is_some_and(|n| MACROS_METADATA.contains(&n))
            {
                self.process_metadata_macro(node, None, true);
                continue;
            }
            for children in node.child_lists() {
                self.process_preamble(children, depth + 1);
            }
        }
    }

    /// The `metadata` group under `parent`, or its `authors` /
    /// `affiliations` group for those roles — each created on first use.
    fn parent_for_role(&mut self, parent: Option<usize>, role: &str) -> usize {
        let metadata = match self.metadata_groups.get(&parent) {
            Some(&g) => g,
            None => {
                let g = self.doc.add_group(parent, "metadata", "unspecified");
                self.metadata_groups.insert(parent, g);
                g
            }
        };
        let group_name = match role {
            "author" => "authors",
            "affiliation" | "altaffiliation" | "institute" | "address" => "affiliations",
            _ => return metadata,
        };
        match self.role_groups.get(&(parent, group_name)) {
            Some(&g) => g,
            None => {
                let g = self
                    .doc
                    .add_group(Some(metadata), group_name, "unspecified");
                self.role_groups.insert((parent, group_name), g);
                g
            }
        }
    }

    /// `LatexMetadataHandler.process_macro`: the argument as rich text with
    /// the command's role — `title`, a `footnote` for `\thanks` /
    /// `\footnote` / `\altaffiliation`, else `text` — `\and`-split for
    /// authors and affiliations, `\parbox` unwrapped, `\inst` markers as
    /// superscript formulas.
    fn process_metadata_macro(&mut self, node: &N, parent: Option<usize>, in_preamble: bool) {
        let Some(name) = node.macro_name() else {
            return;
        };
        let Some(K::Group(arg_nodes)) = node.last_arg().map(|a| &a.kind) else {
            return;
        };
        let role = if name == "affil" { "affiliation" } else { name };
        let label: &'static str = match role {
            "title" => "title",
            "thanks" | "footnote" | "altaffiliation" => "footnote",
            _ => "text",
        };
        let mut nodes = unwrap_parboxes(arg_nodes);
        if role == "altaffiliation" {
            let args = node.args().unwrap_or(&[]);
            if args.len() > 1 {
                if let Some(Some(N {
                    kind: K::Group(optional),
                    ..
                })) = args.first()
                {
                    if !optional.is_empty() {
                        let mut with = optional.clone();
                        with.push(N::chars(" "));
                        with.extend(nodes);
                        nodes = with;
                    }
                }
            }
        }
        let segments = if matches!(role, "author" | "affiliation" | "institute" | "address") {
            split_on_and(nodes)
        } else {
            vec![nodes]
        };
        let role_parent = if role == "footnote" && !in_preamble {
            parent
        } else {
            Some(self.parent_for_role(parent, role))
        };
        for segment in segments {
            let segment = self.replace_inst_markers(segment);
            self.process_nodes(&segment, Cx::new(role_parent, Some(label)));
        }
    }

    fn replace_inst_markers(&self, nodes: Vec<N>) -> Vec<N> {
        let mut out = Vec::with_capacity(nodes.len());
        for node in nodes {
            if node.macro_name() == Some("inst") && node.has_args() {
                if let Some(Some(arg)) = node.args().and_then(|a| a.first()) {
                    if matches!(arg.kind, K::Group(_)) {
                        let v = arg.verbatim();
                        let marker = v.get(1..v.len().saturating_sub(1)).unwrap_or("");
                        if !marker.is_empty() {
                            let formula = self.clean_math(&format!("$^{{{marker}}}$"), "inline");
                            out.push(N {
                                kind: K::Formula(formula),
                                pos: 0,
                                len: 0,
                                src: Rc::from(""),
                            });
                        }
                        continue;
                    }
                }
            }
            out.push(node);
        }
        out
    }

    // -- the node walk ---------------------------------------------------------

    /// `_process_nodes`: the text buffer collects chars, inline macros and
    /// inline math; environments, structural macros and paragraph breaks
    /// flush it as one item.
    fn process_nodes(&mut self, nodes: &[N], cx: Cx) {
        let mut buf: Vec<Part> = Vec::new();
        let mut strip_next = false;
        let mut idx = 0;
        while idx < nodes.len() {
            let node = &nodes[idx];
            let strip = std::mem::take(&mut strip_next);
            let mut consumed = 0;
            match &node.kind {
                K::Chars(text) => {
                    let text = if strip { text.trim_start() } else { text };
                    self.process_chars_node(text, cx, &mut buf);
                }
                K::Formula(f) => buf.push(Part::Formula(f.clone())),
                K::Macro { .. } => match self.inline_math_argument_nodes(node) {
                    Some(inline) => self.append_inline_nodes(&inline, cx, &mut buf),
                    None => {
                        let (n, s) =
                            self.process_macro_node_inline(node, cx, &mut buf, &nodes[idx + 1..]);
                        consumed = n;
                        strip_next = s;
                    }
                },
                K::Env { name, .. } => {
                    if name == "math" {
                        let math = self.clean_math(node.verbatim(), "math");
                        if !math.is_empty() {
                            buf.push(Part::Formula(math));
                        }
                    } else {
                        self.flush(&mut buf, cx);
                        self.process_environment(node, cx);
                    }
                }
                K::Math { .. } => self.process_math_node(node, cx, &mut buf),
                K::Group(_) => self.process_group_node(node, cx, &mut buf),
                // pylatexenc's specials and comments have no handler upstream.
                K::Specials(_) | K::Comment => {}
            }
            idx += 1 + consumed;
        }
        self.flush(&mut buf, cx);
    }

    /// `flush_text_buffer`: a buffer without formulas is one stripped item
    /// (a heading under `heading_level`); one with formulas is an empty item
    /// holding an `inline` group of its text runs and formulas.
    fn flush(&mut self, buf: &mut Vec<Part>, cx: Cx) {
        if buf.is_empty() {
            return;
        }
        let parts = std::mem::take(buf);
        if !parts.iter().any(|p| matches!(p, Part::Formula(_))) {
            let combined: String = parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => t.as_str(),
                    Part::Formula(_) => "",
                })
                .collect();
            let combined = combined.trim();
            if !combined.is_empty() {
                match cx.heading {
                    Some(level) => {
                        self.doc.add_heading(cx.parent, combined, level);
                    }
                    None => {
                        self.doc
                            .add_text(cx.parent, cx.label.unwrap_or("text"), combined);
                    }
                }
            }
            return;
        }
        let paragraph = match cx.heading {
            Some(level) => self.doc.add_heading(cx.parent, "", level),
            None => self.doc.add_text(cx.parent, cx.label.unwrap_or("text"), ""),
        };
        let group = self.doc.add_group(Some(paragraph), "group", "inline");
        let mut merged: Vec<Part> = Vec::new();
        for part in parts {
            match (part, merged.last_mut()) {
                (Part::Text(t), Some(Part::Text(last))) => last.push_str(&t),
                (part, _) => merged.push(part),
            }
        }
        if let Some(Part::Text(t)) = merged.first_mut() {
            *t = t.trim_start().to_string();
        }
        if let Some(Part::Text(t)) = merged.last_mut() {
            *t = t.trim_end().to_string();
        }
        for part in merged {
            match part {
                Part::Formula(f) => {
                    let f = collapse_layout_whitespace(&f);
                    self.doc.add_text(Some(group), "formula", f.trim());
                }
                Part::Text(t) => {
                    if !t.is_empty() {
                        self.doc.add_text(Some(group), "text", &t);
                    }
                }
            }
        }
    }

    /// `_process_chars_node` (docling#4340): a paragraph break inside a chars
    /// node flushes the buffer with the first part, adds every middle part as
    /// its own `paragraph`, and leaves the last part in the buffer.
    fn process_chars_node(&mut self, text: &str, cx: Cx, buf: &mut Vec<Part>) {
        if text.contains("\n\n") {
            let parts: Vec<&str> = text.split("\n\n").collect();
            buf.push(Part::Text(parts[0].to_string()));
            self.flush(buf, cx);
            for part in &parts[1..parts.len() - 1] {
                let stripped = part.trim();
                if !stripped.is_empty() {
                    self.doc
                        .add_text(cx.parent, cx.label.unwrap_or("paragraph"), stripped);
                }
            }
            buf.push(Part::Text(parts[parts.len() - 1].to_string()));
        } else {
            buf.push(Part::Text(text.to_string()));
        }
    }

    fn process_group_node(&mut self, node: &N, cx: Cx, buf: &mut Vec<Part>) {
        let K::Group(list) = &node.kind else {
            return;
        };
        if list.is_empty() {
            return;
        }
        if self.is_text_only_group(list) {
            if self.contains_math(list) {
                self.append_inline_nodes(list, cx, buf);
            } else {
                let text = self.nodes_to_text(list);
                if !text.is_empty() {
                    buf.push(Part::Text(text));
                }
            }
        } else {
            self.flush(buf, cx);
            self.process_nodes(list, cx);
        }
    }

    fn contains_math(&self, nodes: &[N]) -> bool {
        nodes.iter().any(|node| {
            match &node.kind {
                K::Math { .. } => return true,
                K::Env { name, .. } if name == "math" => return true,
                K::Macro { name, .. } => {
                    if name == "ensuremath" {
                        return true;
                    }
                    if let Some(exp) = self.custom_macro(name) {
                        if exp.contains("\\ensuremath") || exp.contains('$') {
                            return true;
                        }
                    }
                }
                _ => {}
            }
            node.child_lists()
                .into_iter()
                .any(|c| self.contains_math(c))
        })
    }

    /// `_append_inline_nodes`: a node list that holds math, read into the
    /// current buffer — environments other than `math` and specials drop out.
    fn append_inline_nodes(&mut self, nodes: &[N], cx: Cx, buf: &mut Vec<Part>) {
        let mut strip_next = false;
        let mut idx = 0;
        while idx < nodes.len() {
            let node = &nodes[idx];
            let strip = std::mem::take(&mut strip_next);
            let mut consumed = 0;
            match &node.kind {
                K::Chars(text) => {
                    let text = if strip { text.trim_start() } else { text };
                    self.process_chars_node(text, cx, buf);
                }
                K::Math { .. } => self.process_math_node(node, cx, buf),
                K::Env { name, .. } if name == "math" => {
                    let math = self.clean_math(node.verbatim(), "math");
                    if !math.is_empty() {
                        buf.push(Part::Formula(math));
                    }
                }
                K::Group(list) => self.append_inline_nodes(list, cx, buf),
                K::Macro { .. } => match self.inline_math_argument_nodes(node) {
                    Some(inline) => self.append_inline_nodes(&inline, cx, buf),
                    None => {
                        let (n, s) =
                            self.process_macro_node_inline(node, cx, buf, &nodes[idx + 1..]);
                        consumed = n;
                        strip_next = s;
                    }
                },
                _ => {}
            }
            idx += 1 + consumed;
        }
    }

    /// A formatting / style / inline-colour macro whose text argument holds
    /// math: that argument's nodes, read inline.
    fn inline_math_argument_nodes(&self, node: &N) -> Option<Vec<N>> {
        let name = node.macro_name()?;
        if !(MACROS_TEXT_FORMATTING.contains(&name)
            || MACROS_TEXT_STYLE.contains(&name)
            || MACROS_COLOR_INLINE.contains(&name))
        {
            return None;
        }
        let list = node.last_arg()?.group_list()?;
        (!list.is_empty() && self.contains_math(list)).then(|| list.to_vec())
    }

    fn process_math_node(&mut self, node: &N, cx: Cx, buf: &mut Vec<Part>) {
        let K::Math { display, .. } = &node.kind else {
            return;
        };
        let verbatim = node.verbatim();
        let is_display = *display
            || ENV_MATH_DISPLAY_PREFIXES
                .iter()
                .any(|p| verbatim.starts_with(p));
        if is_display {
            self.flush(buf, cx);
            let math = self.clean_math(verbatim, "display");
            self.doc.add_text(cx.parent, "formula", &math);
        } else {
            let math = self.clean_math(verbatim, "inline");
            if !math.is_empty() {
                buf.push(Part::Formula(math));
            }
        }
    }

    /// `_process_macro_node_inline`; returns how many following nodes a
    /// custom macro's arguments consumed, and whether the next chars node
    /// loses its leading whitespace (after an `\item[term]`).
    fn process_macro_node_inline(
        &mut self,
        node: &N,
        cx: Cx,
        buf: &mut Vec<Part>,
        following: &[N],
    ) -> (usize, bool) {
        let Some(name) = node.macro_name() else {
            return (0, false);
        };
        if let Some(text) = self.char_macro_to_text(node) {
            buf.push(Part::Text(text));
        } else if MACROS_INLINE_VERBATIM.contains(&name) {
            buf.push(Part::Text(if name == "~" {
                " ".into()
            } else {
                name.into()
            }));
        } else if name == " " {
            buf.push(Part::Text(" ".into()));
        } else if MACROS_TEXT_FORMATTING.contains(&name) {
            let text = self.extract_macro_arg(node);
            if !text.is_empty() {
                buf.push(Part::Text(text));
            }
        } else if name == "ensuremath" {
            if let Some(arg) = node.last_arg().filter(|a| matches!(a.kind, K::Group(_))) {
                let v = arg.verbatim();
                let math = v.get(1..v.len().saturating_sub(1)).unwrap_or("").trim();
                if !math.is_empty() {
                    buf.push(Part::Formula(self.expand_macros(math)));
                }
            }
        } else if self.is_custom(name) {
            let (expansion, consumed) = self.expand_custom_macro_invocation(node, following);
            if !expansion.is_empty() {
                if self.append_custom_math_expansion(&expansion, cx, buf) {
                    if node.verbatim().ends_with(char::is_whitespace) {
                        buf.push(Part::Text(" ".into()));
                    }
                    return (consumed, false);
                }
                if self.num_args(name) > 0 {
                    buf.push(Part::Text(self.parse_latex_fragment_to_text(&expansion)));
                } else {
                    buf.push(Part::Text(expansion));
                }
            }
            return (consumed, false);
        } else if MACROS_CITATION.contains(&name) {
            let arg = self.extract_macro_arg(node);
            if !arg.is_empty() {
                buf.push(Part::Text(format!("[{arg}]")));
            }
        } else if name == "url" {
            let url = self.extract_macro_arg(node);
            if !url.is_empty() {
                buf.push(Part::Text(url));
            }
        } else if MACROS_COLOR.contains(&name) {
        } else if MACROS_TEXT_STYLE.contains(&name) {
            let text = self.extract_macro_arg(node);
            if !text.is_empty() {
                buf.push(Part::Text(text));
            }
        } else if MACROS_COLOR_INLINE.contains(&name) {
            // The text is always the last argument; the colour is skipped.
            if let Some(list) = node.last_arg().and_then(N::group_list) {
                let text = self.nodes_to_text(list);
                if !text.is_empty() {
                    buf.push(Part::Text(text));
                }
            }
        } else if name == "item" {
            // docling#4644 / #4579: every `\item` flushes; a description
            // term leads the item's text.
            self.flush(buf, cx);
            let term = self.item_term(node);
            if !term.is_empty() {
                buf.push(Part::Text(format!("{term}: ")));
                return (0, true);
            }
        } else if name == "label" {
            self.process_macro(node, cx);
        } else if MACROS_STRUCTURAL.contains(&name) {
            self.flush(buf, cx);
            self.process_macro(node, cx);
        } else if MACROS_SPACING.contains(&name) || MACROS_IGNORED.contains(&name) {
            // Discarded with their arguments (`\vspace{-1mm}` emits nothing).
        } else if node.has_args() {
            let inline = self.extract_all_macro_args_inline(node);
            if !inline.is_empty() {
                buf.push(Part::Text(inline));
            }
        }
        (0, false)
    }

    /// `_append_custom_math_expansion`: an expansion holding math is read
    /// inline, its formulas kept.
    fn append_custom_math_expansion(
        &mut self,
        expansion: &str,
        cx: Cx,
        buf: &mut Vec<Part>,
    ) -> bool {
        if !expansion.contains("\\ensuremath") && !expansion.contains('$') {
            return false;
        }
        let nodes = parse(expansion);
        if !self.contains_math(&nodes) {
            return false;
        }
        self.append_inline_nodes(&nodes, cx, buf);
        true
    }

    /// `_process_macro`: the structural macros, each its own item.
    fn process_macro(&mut self, node: &N, cx: Cx) {
        let Some(name) = node.macro_name() else {
            return;
        };
        let text_label = cx.label.unwrap_or("text");
        if MACROS_HEADING.contains(&name) {
            if let Some(list) = node.last_arg().and_then(N::group_list) {
                let list = list.to_vec();
                self.process_nodes(
                    &list,
                    Cx {
                        parent: cx.parent,
                        label: None,
                        heading: Some(heading_level(name)),
                    },
                );
            }
        } else if MACROS_METADATA.contains(&name) {
            self.process_metadata_macro(node, cx.parent, false);
        } else if name == "maketitle" {
        } else if MACROS_TEXT_STYLE.contains(&name) {
            if let Some(list) = node.last_arg().and_then(N::group_list) {
                let list = list.to_vec();
                self.process_nodes(&list, Cx::new(cx.parent, cx.label));
            }
        } else if MACROS_CITATION.contains(&name) {
            let arg = self.extract_macro_arg(node);
            if !arg.is_empty() {
                self.doc
                    .add_text(cx.parent, "reference", &format!("[{arg}]"));
            }
        } else if name == "url" {
            let url = self.extract_macro_arg(node);
            if !url.is_empty() {
                self.doc.add_text(cx.parent, "reference", &url);
            }
        } else if name == "label" {
            let label = self.extract_macro_arg(node);
            if !label.is_empty() {
                self.labels.insert(label);
            }
        } else if name == "caption" || name == "captionof" {
            self.add_caption(node, cx.parent);
        } else if name == "marginpar" {
            let text = self.extract_macro_arg(node);
            if !text.is_empty() {
                self.doc.add_text(cx.parent, "footnote", &text);
            }
        } else if name == "includegraphics" {
            let img_path = self.extract_macro_arg(node);
            if !img_path.is_empty() {
                let (image, dpi) = self.load_image(&img_path);
                let caption = (!self.suppress_image_path_caption).then(|| {
                    self.doc
                        .add_text(None, "caption", &format!("Image: {img_path}"))
                });
                self.doc.add_picture(cx.parent, caption, image, dpi);
            }
        } else if name == "\\" || MACROS_IGNORED.contains(&name) {
        } else if name == "input" || name == "include" {
            let filepath = self.extract_macro_arg(node);
            if !filepath.is_empty() {
                self.process_input(&filepath, Cx::new(cx.parent, cx.label));
            }
        } else if MACROS_ESCAPED.contains(&name) {
            self.doc.add_text(cx.parent, text_label, name);
        } else if name == "href" {
            let Some(args) = node.args().filter(|a| a.len() >= 2) else {
                return;
            };
            let arg_text = |arg: &Option<N>| -> String {
                match arg {
                    Some(n) => match n.group_list() {
                        Some(list) => self.nodes_to_text(list),
                        None => n.verbatim().trim_matches(|c| "{} ".contains(c)).to_string(),
                    },
                    None => String::new(),
                }
            };
            let url = arg_text(&args[0]);
            let display = arg_text(&args[1]);
            let link = match (url.is_empty(), display.is_empty()) {
                (false, false) => docling_core::hyperlink::markdown_link(&display, &url),
                (false, true) => url,
                (true, false) => display,
                (true, true) => String::new(),
            };
            if !link.is_empty() {
                self.doc.add_text(cx.parent, "reference", &link);
            }
        } else if MACROS_SPACING.contains(&name) {
            if name == "newline" {
                self.doc.add_text(cx.parent, text_label, "\n");
            }
        } else if name == "textcolor" || name == "colorbox" {
            let list = node
                .args()
                .and_then(|a| a.iter().rev().flatten().find_map(N::group_list))
                .map(<[N]>::to_vec);
            if let Some(list) = list {
                self.process_nodes(&list, Cx::new(cx.parent, cx.label));
            }
        } else if name == "item" {
        } else if let Some(args) = node.args() {
            let lists: Vec<Vec<N>> = args
                .iter()
                .flatten()
                .filter_map(N::group_list)
                .map(<[N]>::to_vec)
                .collect();
            for list in lists {
                self.process_nodes(&list, Cx::new(cx.parent, cx.label));
            }
        }
    }

    /// `_add_caption`: the last argument as rich `caption` text.
    fn add_caption(&mut self, node: &N, parent: Option<usize>) {
        if let Some(list) = node.last_arg().and_then(N::group_list) {
            let list = list.to_vec();
            self.process_nodes(&list, Cx::new(parent, Some("caption")));
        }
    }

    /// `\input` / `\include`: the file parsed in place, relative to the
    /// source file, `.tex` implied, kept inside the source's directory.
    fn process_input(&mut self, filepath: &str, cx: Cx) {
        let Some(base_dir) = self.base_dir.clone() else {
            return;
        };
        let mut input_path = base_dir.join(filepath);
        if input_path.extension().is_none() {
            input_path.set_extension("tex");
        }
        let Some(resolved) = within(&base_dir, &input_path) else {
            return;
        };
        if self.input_stack.contains(&resolved) || self.input_stack.len() >= MAX_INPUT_DEPTH {
            return;
        }
        let Ok(bytes) = std::fs::read(&input_path) else {
            return;
        };
        let content = decode_latex(&bytes);
        self.input_stack.insert(resolved.clone());
        let nodes = parse(&content);
        self.process_nodes(&nodes, cx);
        self.input_stack.remove(&resolved);
    }

    /// `\includegraphics`: a raster file becomes the picture's PNG payload
    /// (the `dpi` its metadata records, 72 when none); a PDF's first page is
    /// rendered at scale 2 (144 dpi), upstream's pypdfium2 call, by
    /// docling-pdf's renderer — a build without the `pdf` feature (wasm), or
    /// a missing file, leaves the picture without one.
    fn load_image(&self, img_path: &str) -> (Option<PictureImage>, Option<u32>) {
        if let Some(rendered) = self.figures.get(img_path) {
            return rendered.clone();
        }
        let Some(base_dir) = &self.base_dir else {
            return (None, None);
        };
        load_image_file(base_dir, img_path)
    }

    /// Every `\includegraphics` under `nodes`, rendered before the walk on
    /// worker threads (`render_figures`): a paper's PDF figures — matplotlib
    /// and TikZ vector art, megabytes each — are the bulk of its conversion
    /// time, and the walk wants them one at a time in document order. Keyed
    /// by the argument text, each path once; a figure the walk meets that
    /// was not here (inside an `\input` file) renders in place.
    fn prerender_figures(&mut self, nodes: &[N]) {
        let Some(base_dir) = self.base_dir.clone() else {
            return;
        };
        let mut paths = Vec::new();
        self.collect_figure_paths(nodes, &mut paths);
        if paths.is_empty() {
            return;
        }
        let rendered = render_figures(&base_dir, &paths);
        self.figures = paths.into_iter().zip(rendered).collect();
    }

    fn collect_figure_paths(&self, nodes: &[N], out: &mut Vec<String>) {
        for node in nodes {
            match &node.kind {
                K::Macro { name, args } => {
                    if name == "includegraphics" {
                        let path = self.extract_macro_arg(node);
                        if !path.is_empty() && !out.contains(&path) {
                            out.push(path);
                        }
                    }
                    for arg in args.iter().flatten().flatten() {
                        self.collect_figure_paths(std::slice::from_ref(arg), out);
                    }
                }
                K::Env { nodelist, args, .. } => {
                    for arg in args.iter().flatten().flatten() {
                        self.collect_figure_paths(std::slice::from_ref(arg), out);
                    }
                    self.collect_figure_paths(nodelist, out);
                }
                K::Group(list) | K::Math { nodelist: list, .. } => {
                    self.collect_figure_paths(list, out);
                }
                K::Chars(_) | K::Comment | K::Specials(_) | K::Formula(_) => {}
            }
        }
    }

    // -- environments ----------------------------------------------------------

    fn process_environment(&mut self, node: &N, cx: Cx) {
        let K::Env { name, nodelist, .. } = &node.kind else {
            return;
        };
        let name = name.as_str();
        let unstarred = name.replace('*', "");
        let body = Cx::new(cx.parent, cx.label);
        match name {
            "document" => self.process_nodes(nodelist, body),
            "abstract" => {
                self.doc.add_heading(cx.parent, "Abstract", 1);
                self.process_nodes(nodelist, body);
            }
            _ if ENV_MATH.contains(&unstarred.as_str()) || name == "math" => {
                let math = self.clean_math(node.verbatim(), name);
                self.doc.add_text(cx.parent, "formula", &math);
            }
            "subequations" => self.process_nodes(nodelist, body),
            _ if ENV_THEOREM.contains(&unstarred.as_str()) => {
                let title = capitalize(&unstarred);
                self.doc
                    .add_text(cx.parent, "text", &format!("**{title}.**"));
                self.process_nodes(nodelist, body);
            }
            "proof" => {
                self.doc.add_text(cx.parent, "text", "*Proof.*");
                self.process_nodes(nodelist, body);
                let verbatim = node.verbatim();
                if !verbatim.contains("\\qed") && !verbatim.contains("\\qedsymbol") {
                    self.doc.add_text(cx.parent, "text", "\u{25fb}");
                }
            }
            _ if ENV_QUOTE.contains(&name) => self.process_nodes(nodelist, body),
            _ if ENV_LIST.contains(&name) => self.process_list(nodelist, cx.parent),
            "tabular" | "tabular*" | "tabularx" | "longtable" => {
                if let Some(parsed) = self.parse_table(node) {
                    let table = self.doc.add_table(cx.parent, parsed.table);
                    self.add_rich_table_cells(table, parsed.rich);
                    if name == "longtable" {
                        let start = self.doc.tree.items.len();
                        for child in nodelist {
                            if child.macro_name() == Some("caption") {
                                self.add_caption(child, cx.parent);
                            }
                        }
                        let captions = self.doc.captions_since(start);
                        self.doc.extend_captions(table, captions);
                    }
                }
            }
            "table" | "table*" => {
                let start = self.doc.tree.items.len();
                self.process_nodes(nodelist, body);
                if let Some(table) = self
                    .doc
                    .first_since(start, |k| matches!(k, TreeKind::Table { .. }))
                {
                    let captions = self.doc.captions_since(start);
                    self.doc.extend_captions(table, captions);
                }
            }
            "figure" | "figure*" => {
                let group = self.doc.add_group(cx.parent, "figure", "section");
                let start = self.doc.tree.items.len();
                let previous = std::mem::replace(&mut self.suppress_image_path_caption, true);
                self.process_nodes(nodelist, Cx::new(Some(group), cx.label));
                self.suppress_image_path_caption = previous;
                if let Some(picture) = self
                    .doc
                    .first_since(start, |k| matches!(k, TreeKind::Picture { .. }))
                {
                    let captions = self.doc.captions_since(start);
                    self.doc.extend_captions(picture, captions);
                }
            }
            "tikzpicture" => self.process_tikzpicture(node, cx),
            "verbatim" | "lstlisting" | "minted" => {
                let code = extract_verbatim_content(node.verbatim(), name);
                self.doc.add_code(cx.parent, &code);
            }
            "thebibliography" => {
                self.doc.add_heading(cx.parent, "References", 1);
                self.process_bibliography(nodelist, cx.parent);
            }
            "filecontents" | "filecontents*" => {}
            _ => self.process_nodes(nodelist, body),
        }
    }

    /// `_process_tikzpicture` without an engine: one self-contained
    /// `tikzpicture` is a picture carrying its source as `meta.code`
    /// (language `Tikz`), any other has its content walked.
    fn process_tikzpicture(&mut self, node: &N, cx: Cx) {
        let K::Env { nodelist, .. } = &node.kind else {
            return;
        };
        if !tikz_end_re().is_match(node.verbatim()) || !validate_tikz_nodelist(nodelist, 0) {
            self.process_nodes(nodelist, Cx::new(cx.parent, cx.label));
            return;
        }
        let picture = self.doc.add_picture(cx.parent, None, None, None);
        self.doc.tree.items[picture].code_meta =
            Some((node.verbatim().to_string(), "Tikz".to_string()));
    }

    fn process_list(&mut self, nodelist: &[N], parent: Option<usize>) {
        let group = self.doc.add_group(parent, "list", "list");
        let mut items: Vec<Vec<N>> = Vec::new();
        let mut current: Vec<N> = Vec::new();
        for n in nodelist {
            if n.macro_name() == Some("item") {
                if !current.is_empty() {
                    items.push(std::mem::take(&mut current));
                }
                // The `\item` node itself stays when it has an argument list.
                if n.has_args() {
                    current.push(n.clone());
                }
            } else {
                current.push(n.clone());
            }
        }
        if !current.is_empty() {
            items.push(current);
        }
        for item in items {
            self.process_nodes(&item, Cx::new(Some(group), Some("list_item")));
        }
    }

    /// `_process_bibliography`: one `[key] text` list item per `\bibitem`.
    fn process_bibliography(&mut self, nodelist: &[N], parent: Option<usize>) {
        let group = self.doc.add_group(parent, "bibliography", "list");
        let mut items: Vec<(String, Vec<N>)> = Vec::new();
        let mut current: Vec<N> = Vec::new();
        let mut key = String::new();
        for n in nodelist {
            if n.macro_name() == Some("bibitem") {
                if !current.is_empty() {
                    items.push((std::mem::take(&mut key), std::mem::take(&mut current)));
                }
                key = self.extract_macro_arg(n);
            } else {
                current.push(n.clone());
            }
        }
        if !current.is_empty() {
            items.push((key, current));
        }
        for (key, item) in items {
            let body = self.nodes_to_text(&item);
            let body = body.trim();
            if !body.is_empty() {
                let text = if key.is_empty() {
                    body.to_string()
                } else {
                    format!("[{key}] {body}")
                };
                self.doc.add_text(Some(group), "list_item", &text);
            }
        }
    }

    // -- math ----------------------------------------------------------------

    fn clean_math(&self, latex: &str, env_name: &str) -> String {
        let mut s = latex.to_string();
        if ENV_MATH_CLEAN.contains(&env_name) {
            let begin = format!("\\begin{{{env_name}}}");
            let end = format!("\\end{{{env_name}}}");
            if let Some(start) = s.find(&begin) {
                if let Some(stop) = s[start + begin.len()..].find(&end) {
                    s = s[start + begin.len()..start + begin.len() + stop].to_string();
                }
            }
        }
        let mut s = s.trim().to_string();
        for (open, close) in [("$$", "$$"), ("$", "$"), ("\\[", "\\]"), ("\\(", "\\)")] {
            if s.starts_with(open) && s.ends_with(close) {
                // Python's slice: empty when the delimiters overlap.
                s = s
                    .get(open.len()..s.len().saturating_sub(close.len()))
                    .unwrap_or("")
                    .to_string();
                break;
            }
        }
        let s = label_re().replace_all(&s, "").into_owned();
        let s = self.expand_macros(&s).trim().to_string();
        if env_name == "inline" {
            collapse_layout_whitespace(&s)
        } else {
            s
        }
    }

    /// `_expand_macros`: argument-less custom macros replaced by their
    /// bodies (`\\name(?![a-zA-Z])`, in registration order), repeated until
    /// nothing changes (ten rounds at most) so chains resolve.
    fn expand_macros(&self, latex: &str) -> String {
        let mut s = latex.to_string();
        for _ in 0..10 {
            let previous = s.clone();
            for (name, def) in &self.custom_macros {
                if self.num_args(name) > 0 || !s.contains(&format!("\\{name}")) {
                    continue;
                }
                s = replace_macro_calls(&s, name, def);
            }
            if s == previous {
                break;
            }
        }
        s
    }

    fn expand_custom_macro_invocation(&self, node: &N, following: &[N]) -> (String, usize) {
        let Some(name) = node.macro_name() else {
            return (String::new(), 0);
        };
        let def = self.custom_macro(name).unwrap_or("").to_string();
        let expected = self.num_args(name);
        if expected == 0 {
            return (def, 0);
        }
        let mut values: Vec<String> = Vec::new();
        let mut consumed = 0;
        for next in following {
            if values.len() >= expected {
                break;
            }
            match &next.kind {
                K::Chars(c) if c.trim().is_empty() => consumed += 1,
                K::Group(list) => {
                    values.push(self.nodes_to_text(list));
                    consumed += 1;
                }
                _ => break,
            }
        }
        if values.len() < expected {
            return (def, 0);
        }
        let mut expansion = def;
        for idx in (1..=values.len()).rev() {
            expansion = expansion.replace(&format!("#{idx}"), &values[idx - 1]);
        }
        (expansion, consumed)
    }

    fn parse_latex_fragment_to_text(&self, fragment: &str) -> String {
        self.nodes_to_text(&parse(fragment))
    }

    // -- text extraction (`utils/text.py`) -----------------------------------------

    /// `_char_macro_to_text` (docling#4517): the Unicode text of an accent
    /// or letter macro — `\'e` → é, `\~n` → ñ, `\ss` → ß — or `None` for
    /// any other macro and for a name the document redefines.
    fn char_macro_to_text(&self, node: &N) -> Option<String> {
        let name = node.macro_name()?;
        if self.is_custom(name) {
            return None;
        }
        let text = if MACROS_ACCENTS.contains(&name) {
            let arg = node.args().and_then(|a| a.iter().flatten().next());
            match arg {
                Some(arg) if self.uses_custom_macro(arg) => self.accent_custom_macro_arg(name, arg),
                _ => latex2text(std::slice::from_ref(node)),
            }
        } else if MACROS_LETTERS.contains(&name) {
            latex2text(std::slice::from_ref(node))
        } else {
            return None;
        };
        // `\~{}` and `\^{}` are the usual way to typeset a literal ~ or ^,
        // which latex2text renders as an empty string.
        if text.is_empty() && (name == "~" || name == "^") {
            return Some(name.to_string());
        }
        Some(text)
    }

    /// An accent over an argument that uses document macros: expanded one
    /// level at a time (`\'{\vowel}` → `\'{\letter}` → `\'{e}`), then
    /// accented; one that never resolves is kept without the accent.
    fn accent_custom_macro_arg(&self, name: &str, arg: &N) -> String {
        let mut nodes = vec![arg.clone()];
        let mut text = String::new();
        for _ in 0..10 {
            text = self.nodes_to_text(&nodes);
            nodes = parse(&text);
            if !nodes.iter().any(|n| self.uses_custom_macro(n)) {
                let base: String = nodes.iter().map(N::verbatim).collect();
                return latex2text(&parse(&format!("\\{name}{{{base}}}")));
            }
        }
        text
    }

    fn uses_custom_macro(&self, node: &N) -> bool {
        match &node.kind {
            K::Macro { name, args } => {
                self.is_custom(name)
                    || args
                        .iter()
                        .flatten()
                        .flatten()
                        .any(|a| self.uses_custom_macro(a))
            }
            K::Group(list) => list.iter().any(|n| self.uses_custom_macro(n)),
            _ => false,
        }
    }

    /// The `\item[…]` term's text, or `""` without one.
    fn item_term(&self, node: &N) -> String {
        match node.args().and_then(|a| a.first()) {
            Some(Some(arg)) => match arg.group_list() {
                Some(list) => self.nodes_to_text(list).trim().to_string(),
                None => String::new(),
            },
            _ => String::new(),
        }
    }

    fn extract_macro_arg(&self, node: &N) -> String {
        match node.last_arg() {
            Some(arg) => match arg.group_list() {
                Some(list) => self.nodes_to_text(list),
                None => arg
                    .verbatim()
                    .trim_matches(|c| "{} ".contains(c))
                    .to_string(),
            },
            None => String::new(),
        }
    }

    fn extract_all_macro_args_inline(&self, node: &N) -> String {
        let Some(args) = node.args() else {
            return String::new();
        };
        let mut parts = Vec::new();
        for arg in args.iter().flatten() {
            let text = match arg.group_list() {
                Some(list) => self.nodes_to_text(list),
                None => arg
                    .verbatim()
                    .trim_matches(|c| "{} ".contains(c))
                    .to_string(),
            };
            if !text.is_empty() {
                parts.push(text);
            }
        }
        parts.join(" ")
    }

    fn nodes_to_text(&self, nodes: &[N]) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut idx = 0;
        while idx < nodes.len() {
            let node = &nodes[idx];
            let mut consumed = 0;
            match &node.kind {
                K::Chars(c) => parts.push(c.clone()),
                K::Group(list) => parts.push(self.nodes_to_text(list)),
                K::Macro { .. } => {
                    let (text, n) = self.macro_node_to_text(node, &nodes[idx + 1..]);
                    consumed = n;
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
                K::Math { .. } => parts.push(self.expand_macros(node.verbatim())),
                K::Env { name, nodelist, .. } => {
                    if matches!(name.as_str(), "equation" | "align" | "gather") {
                        parts.push(node.verbatim().to_string());
                    } else {
                        parts.push(self.nodes_to_text(nodelist));
                    }
                }
                K::Specials(_) | K::Comment | K::Formula(_) => {}
            }
            idx += 1 + consumed;
        }
        let result = parts.concat();
        let result = spaces_re().replace_all(&result, " ");
        let result = blank_lines_re().replace_all(&result, "\n\n");
        result.trim().to_string()
    }

    fn macro_node_to_text(&self, node: &N, following: &[N]) -> (String, usize) {
        let Some(name) = node.macro_name() else {
            return (String::new(), 0);
        };
        if let Some(text) = self.char_macro_to_text(node) {
            return (text, 0);
        }
        if MACROS_TEXT_FORMATTING.contains(&name) || MACROS_TEXT_STYLE.contains(&name) {
            return (self.extract_macro_arg(node), 0);
        }
        if MACROS_COLOR_INLINE.contains(&name) {
            return match node.last_arg().and_then(N::group_list) {
                Some(list) => (self.nodes_to_text(list), 0),
                None => (String::new(), 0),
            };
        }
        if MACROS_CITATION.contains(&name) {
            return (node.verbatim().to_string(), 0);
        }
        if name == "\\" {
            return ("\n".into(), 0);
        }
        if name == "~" {
            return (" ".into(), 0);
        }
        if name == "item" {
            let term = self.item_term(node);
            return (
                if term.is_empty() {
                    String::new()
                } else {
                    format!("{term}: ")
                },
                0,
            );
        }
        if MACROS_ESCAPED.contains(&name) {
            return (name.into(), 0);
        }
        if self.is_custom(name) {
            let (expansion, consumed) = self.expand_custom_macro_invocation(node, following);
            if self.num_args(name) > 0 {
                return (self.parse_latex_fragment_to_text(&expansion), consumed);
            }
            return (expansion, consumed);
        }
        if MACROS_SPACING.contains(&name) || MACROS_IGNORED.contains(&name) {
            return (String::new(), 0);
        }
        (self.extract_all_macro_args_inline(node), 0)
    }

    fn is_text_only_group(&self, list: &[N]) -> bool {
        list.iter().all(|n| match &n.kind {
            K::Env { .. } => false,
            K::Macro { name, .. } => !MACROS_STRUCTURAL.contains(&name.as_str()),
            K::Group(inner) => self.is_text_only_group(inner),
            _ => true,
        })
    }

    // -- tables (`utils/table.py`) ---------------------------------------------------

    /// `_parse_table`: cells split at `&`, rows at `\\`, `\multicolumn` /
    /// `\multirow` arguments read from the source at the node's (document)
    /// offset — into the environment's own text, as upstream does, so the
    /// lookup usually misses and, when it lands somewhere, reads whatever
    /// braces follow. A cell holding math is a rich cell: its nodes are
    /// processed into a group under the table (docling#4817).
    fn parse_table(&self, node: &N) -> Option<ParsedTable> {
        let K::Env { nodelist, .. } = &node.kind else {
            return None;
        };
        let source_latex = node.verbatim();
        let mut rows: Vec<Vec<Option<PendingCell>>> = Vec::new();
        let mut current_row: Vec<Option<PendingCell>> = Vec::new();
        let mut current_cell: Vec<N> = Vec::new();

        let finish_cell = |current_cell: &mut Vec<N>,
                           current_row: &mut Vec<Option<PendingCell>>,
                           col_span: usize,
                           row_span: usize| {
            let rich = self
                .contains_math(current_cell)
                .then(|| current_cell.clone());
            let text = if rich.is_some() {
                String::new()
            } else {
                self.nodes_to_text(current_cell).trim().to_string()
            };
            current_row.push(Some(PendingCell {
                text,
                col_span,
                row_span,
                rich,
            }));
            current_cell.clear();
            for _ in 1..col_span {
                current_row.push(None);
            }
        };

        for n in nodelist {
            match &n.kind {
                K::Macro { name, .. } => match name.as_str() {
                    "\\" => {
                        if !current_cell.is_empty() {
                            finish_cell(&mut current_cell, &mut current_row, 1, 1);
                        }
                        if !current_row.is_empty() {
                            rows.push(std::mem::take(&mut current_row));
                        }
                    }
                    "multicolumn" | "multirow" => {
                        // Upstream slices the environment's text with the
                        // node's *document* offset, a character index.
                        let char_pos = n.src[..n.pos].chars().count();
                        let remaining: String = source_latex.chars().skip(char_pos).collect();
                        let args = parse_brace_args(&remaining);
                        if args.len() >= 3 {
                            let limit = if name == "multicolumn" {
                                MAX_COLSPAN
                            } else {
                                MAX_ROWSPAN
                            };
                            let count = clamp_span(&args[0], limit);
                            if !args[2].is_empty() {
                                current_cell.extend(parse(&args[2]));
                            }
                            if name == "multicolumn" {
                                finish_cell(&mut current_cell, &mut current_row, count, 1);
                            } else {
                                finish_cell(&mut current_cell, &mut current_row, 1, count);
                            }
                        } else {
                            current_cell.push(n.clone());
                        }
                    }
                    m if TABLE_MACROS_RULE.contains(&m)
                        || TABLE_MACROS_IGNORE.contains(&m)
                        || m == "caption" => {}
                    "&" => finish_cell(&mut current_cell, &mut current_row, 1, 1),
                    _ => current_cell.push(n.clone()),
                },
                K::Chars(text) => {
                    if text.contains('&') {
                        let parts: Vec<&str> = text.split('&').collect();
                        for (i, part) in parts.iter().enumerate() {
                            if !part.is_empty() {
                                current_cell.push(N::chars(part));
                            }
                            if i < parts.len() - 1 {
                                finish_cell(&mut current_cell, &mut current_row, 1, 1);
                            }
                        }
                    } else {
                        current_cell.push(n.clone());
                    }
                }
                K::Specials(c) if c == "&" => {
                    finish_cell(&mut current_cell, &mut current_row, 1, 1);
                }
                _ => current_cell.push(n.clone()),
            }
        }
        if !current_cell.is_empty() {
            finish_cell(&mut current_cell, &mut current_row, 1, 1);
        }
        if !current_row.is_empty() {
            rows.push(current_row);
        }
        if rows.is_empty() {
            return None;
        }
        let num_rows = rows.len();
        // Columns to the right of the last cell starting in them hold no
        // cell of their own: a span wider than the table does not widen it.
        let spans: Vec<Vec<(usize, usize)>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .flatten()
                    .map(|c| (c.col_span, c.row_span))
                    .collect()
            })
            .collect();
        let num_cols = rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .min(table_width(&spans));
        let mut cells: Vec<TableCell> = Vec::new();
        let mut rich: Vec<(usize, Vec<N>)> = Vec::new();
        for (i, row) in rows.into_iter().enumerate() {
            let len = row.len();
            let mut row = row.into_iter();
            for j in 0..num_cols {
                let (text, col_span, row_span, nodes) = if j < len {
                    match row.next().flatten() {
                        None => continue,
                        Some(c) => (c.text, c.col_span, c.row_span, c.rich),
                    }
                } else {
                    (String::new(), 1, 1, None)
                };
                // Upstream writes the span into the cell's end offsets (and
                // so into the grid) but leaves its `row_span` / `col_span`
                // fields at 1; here the two agree.
                let end_row = (i + row_span).min(num_rows);
                let end_col = (j + col_span).min(num_cols);
                if let Some(nodes) = nodes {
                    rich.push((cells.len(), nodes));
                }
                cells.push(TableCell {
                    text,
                    bbox: None,
                    start_row: i,
                    start_col: j,
                    row_span: end_row - i,
                    col_span: end_col - j,
                    column_header: false,
                    row_header: false,
                    row_section: false,
                });
            }
        }
        let grid = table_grid(&cells, &vec![vec![String::new(); num_cols]; num_rows]);
        Some(ParsedTable {
            table: Table {
                rows: grid,
                location: None,
                structure: None,
                cell_blocks: None,
                cells: Some(cells),
                caption: None,
                caption_parent: Default::default(),
                caption_location: None,
            },
            rich,
        })
    }

    /// `_add_rich_table_cells`: each math cell's nodes processed into a
    /// group under the table, the cell a `RichTableCell` referencing it.
    fn add_rich_table_cells(&mut self, table: usize, rich: Vec<(usize, Vec<N>)>) {
        for (index, nodes) in rich {
            let group = self.doc.add_group(Some(table), "group", "unspecified");
            self.process_nodes(&nodes, Cx::new(Some(group), None));
            if let TreeKind::Table {
                table: t,
                rich_cells,
                ..
            } = &mut self.doc.tree.items[table].kind
            {
                if let Some(cell) = t.cells.as_ref().and_then(|c| c.get(index)) {
                    rich_cells.push((cell.start_row, cell.start_col, group));
                }
            }
        }
    }
}

/// An `\includegraphics` payload and the dpi it was read at — `None`s
/// when the file is missing, outside the source's directory or unreadable.
type Figure = (Option<PictureImage>, Option<u32>);

/// The picture an `\includegraphics` path under `base_dir` yields, as
/// `load_image` documents it; kept inside the source's directory.
fn load_image_file(base_dir: &Path, img_path: &str) -> Figure {
    let full = base_dir.join(img_path);
    if within(base_dir, &full).is_none() || !full.is_file() {
        return (None, None);
    }
    let Ok(data) = std::fs::read(&full) else {
        return (None, None);
    };
    if full
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
    {
        return render_pdf_figure(&data);
    }
    let dpi = super::ooxml::image_dpi(&data).map_or(72, |d| d as u32);
    let Ok(reader) = image::ImageReader::new(std::io::Cursor::new(&data)).with_guessed_format()
    else {
        return (None, None);
    };
    let Ok(decoded) = reader.decode() else {
        return (None, None);
    };
    let mut png = std::io::Cursor::new(Vec::new());
    if decoded.write_to(&mut png, image::ImageFormat::Png).is_err() {
        return (None, None);
    }
    (
        Some(PictureImage {
            dpi: PictureImage::DEFAULT_DPI,
            mimetype: "image/png".into(),
            width: decoded.width(),
            height: decoded.height(),
            data: png.into_inner(),
            display_size: None,
        }),
        Some(dpi),
    )
}

/// `load_image_file` over `paths`, on worker threads, the results in the
/// paths' order. The PDF pipeline's page-worker count applies
/// (`DOCLING_RS_PDF_WORKERS`; else the cores, at most 16): a figure render
/// is a page render, with a bitmap of up to the `DOCLING_RS_MAX_RENDER_PIXELS`
/// side behind each worker. Plain scoped threads, not a pool: the
/// conversion may itself be running on a rayon worker or on a
/// `convert_streaming` producer thread.
#[cfg(not(target_arch = "wasm32"))]
fn render_figures(base_dir: &Path, paths: &[String]) -> Vec<Figure> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    let workers = docling_core::env::parse::<usize>("DOCLING_RS_PDF_WORKERS")
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .min(16)
        })
        .min(paths.len());
    if workers <= 1 {
        return paths.iter().map(|p| load_image_file(base_dir, p)).collect();
    }
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<Option<Figure>>> =
        Mutex::new(std::iter::repeat_with(|| None).take(paths.len()).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = paths.get(i) else {
                    break;
                };
                let image = load_image_file(base_dir, path);
                done.lock().unwrap()[i] = Some(image);
            });
        }
    });
    done.into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.unwrap_or((None, None)))
        .collect()
}

/// No threads on wasm: the figures render one after another.
#[cfg(target_arch = "wasm32")]
fn render_figures(base_dir: &Path, paths: &[String]) -> Vec<Figure> {
    paths.iter().map(|p| load_image_file(base_dir, p)).collect()
}

/// A PDF figure's first page as the picture's PNG payload, at 144 dpi:
/// pypdfium2's `page.render(scale=2)` sizes the bitmap as the ceiling of the
/// scaled page (display frame, `/Rotate` applied), and so does this; a page
/// past the `DOCLING_RS_MAX_RENDER_PIXELS` side cap gets no payload.
#[cfg(feature = "pdf")]
fn render_pdf_figure(data: &[u8]) -> (Option<PictureImage>, Option<u32>) {
    let render = || -> Option<PictureImage> {
        let meta = docling_pdf::pdf_meta::PdfMeta::open(data)?;
        let g = meta.geometry(0)?;
        let cap: f64 = docling_core::env::parse("DOCLING_RS_MAX_RENDER_PIXELS").unwrap_or(15_000.0);
        let side = |pt: f32| (f64::from(pt) * 2.0).ceil().max(1.0);
        let (w, h) = (side(g.width), side(g.height));
        if w > cap || h > cap {
            return None;
        }
        let bitmap = docling_pdf::render::Renderer::with_bitmap_hint(&meta, 0.0)
            .render(0, w as u32, h as u32)?;
        let mut png = std::io::Cursor::new(Vec::new());
        bitmap.write_to(&mut png, image::ImageFormat::Png).ok()?;
        Some(PictureImage {
            dpi: PictureImage::DEFAULT_DPI,
            mimetype: "image/png".into(),
            width: bitmap.width(),
            height: bitmap.height(),
            data: png.into_inner(),
            display_size: None,
        })
    };
    match render() {
        Some(image) => (Some(image), Some(144)),
        None => (None, None),
    }
}

#[cfg(not(feature = "pdf"))]
fn render_pdf_figure(_data: &[u8]) -> (Option<PictureImage>, Option<u32>) {
    (None, None)
}

struct PendingCell {
    text: String,
    col_span: usize,
    row_span: usize,
    rich: Option<Vec<N>>,
}

struct ParsedTable {
    table: Table,
    /// (index into the table's cells, the cell's nodes) per rich cell.
    rich: Vec<(usize, Vec<N>)>,
}

/// `clamp_span(int(text), limit)`: a span is at least one and at most the
/// HTML table model's cap; text that is not an integer spans one.
fn clamp_span(text: &str, limit: i64) -> usize {
    let t = text.trim();
    let value = match t.parse::<i64>() {
        Ok(v) => v,
        // Python's int() has no overflow: a longer number of digits is
        // simply larger (or smaller) than any cap.
        Err(_)
            if t.trim_start_matches(['+', '-'])
                .bytes()
                .all(|b| b.is_ascii_digit())
                && t.trim_start_matches(['+', '-']).len() > 18 =>
        {
            if t.starts_with('-') {
                1
            } else {
                limit
            }
        }
        Err(_) => 1,
    };
    value.clamp(1, limit) as usize
}

/// `table_spans.table_width`: the columns a table needs for cells placed in
/// the first column no earlier rowspan still covers, at least the declared
/// width (the widest row's colspans, cut after the last column a cell
/// starts in).
fn table_width(rows: &[Vec<(usize, usize)>]) -> usize {
    let mut declared = 0;
    let mut last_start: i64 = -1;
    for cells in rows {
        let mut col = 0usize;
        for &(col_span, _) in cells {
            last_start = last_start.max(col as i64);
            col += col_span;
        }
        declared = declared.max(col);
    }
    let declared = declared.min((last_start + 1).max(0) as usize);
    let mut covered_until: HashMap<usize, usize> = HashMap::new();
    let mut last_start: i64 = -1;
    for (row_idx, cells) in rows.iter().enumerate() {
        let mut col = 0usize;
        for &(col_span, row_span) in cells {
            while covered_until.get(&col).copied().unwrap_or(0) > row_idx {
                col += 1;
            }
            last_start = last_start.max(col as i64);
            for c in col..(col + col_span).min(declared) {
                let e = covered_until.entry(c).or_insert(0);
                *e = (*e).max(row_idx + row_span);
            }
            col += col_span;
        }
    }
    declared.max((last_start + 1).max(0) as usize)
}

/// Upstream's `parse_brace_args`: every top-level `{…}` in `text`.
fn parse_brace_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            let mut depth = 1;
            let start = i + 1;
            i += 1;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            let end = if depth == 0 { i - 1 } else { i };
            args.push(chars[start..end.max(start)].iter().collect());
        } else {
            i += 1;
        }
    }
    args
}

// ---------------------------------------------------------------------------
// latex2text: the slice of pylatexenc's `LatexNodes2Text` the accent and
// letter macros go through.

/// latex2text's `unicode_accents_list`, LaTeX's text accents (`\t` has no
/// entry: it renders as nothing).
fn accent_combining(name: &str) -> Option<char> {
    Some(match name {
        "'" => '\u{301}',
        "`" => '\u{300}',
        "\"" => '\u{308}',
        "c" => '\u{327}',
        "^" => '\u{302}',
        "~" => '\u{303}',
        "H" => '\u{30B}',
        "k" => '\u{328}',
        "=" => '\u{304}',
        "b" => '\u{331}',
        "." => '\u{307}',
        "d" => '\u{323}',
        "r" => '\u{30A}',
        "u" => '\u{306}',
        "v" => '\u{30C}',
        _ => return None,
    })
}

fn letter_text(name: &str) -> Option<&'static str> {
    Some(match name {
        "oe" => "\u{153}",
        "OE" => "\u{152}",
        "ae" => "\u{e6}",
        "AE" => "\u{c6}",
        "aa" => "\u{e5}",
        "AA" => "\u{c5}",
        "o" => "\u{f8}",
        "O" => "\u{d8}",
        "ss" => "\u{df}",
        "L" => "\u{141}",
        "l" => "\u{142}",
        "i" => "\u{131}",
        "j" => "\u{237}",
        _ => return None,
    })
}

fn latex2text(nodes: &[N]) -> String {
    let mut out = String::new();
    for node in nodes {
        match &node.kind {
            K::Chars(c) => out.push_str(c),
            K::Group(list) => out.push_str(&latex2text(list)),
            K::Math { .. } => out.push_str(node.verbatim()),
            K::Specials(s) => out.push_str(match s.as_str() {
                "~" => "\u{a0}",
                "--" => "\u{2013}",
                "---" => "\u{2014}",
                "``" => "\u{201c}",
                "''" => "\u{201d}",
                "!`" => "\u{a1}",
                "?`" => "\u{bf}",
                other => other,
            }),
            K::Macro { name, .. } => {
                if let Some(text) = letter_text(name) {
                    out.push_str(text);
                } else if let Some(combining) = accent_combining(name) {
                    // `make_accented_char`: every character of the stripped
                    // argument gets the mark, NFC-composed; no argument → a
                    // space carries it.
                    let base = match node.args().and_then(|a| a.first()) {
                        Some(Some(arg)) => latex2text(std::slice::from_ref(arg)).trim().to_string(),
                        _ => " ".to_string(),
                    };
                    for ch in base.chars() {
                        let ch = match ch {
                            '\u{131}' => 'i',
                            '\u{237}' => 'j',
                            c => c,
                        };
                        out.extend([ch, combining].into_iter().nfc());
                    }
                } else if matches!(
                    name.as_str(),
                    "textbf"
                        | "textit"
                        | "emph"
                        | "texttt"
                        | "textrm"
                        | "textsf"
                        | "textsc"
                        | "textnormal"
                        | "textmd"
                        | "textup"
                        | "textsl"
                        | "mbox"
                        | "text"
                        | "underline"
                ) {
                    if let Some(arg) = node.last_arg() {
                        out.push_str(&latex2text(std::slice::from_ref(arg)));
                    }
                }
            }
            K::Env { nodelist, .. } => out.push_str(&latex2text(nodelist)),
            K::Comment | K::Formula(_) => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers

/// `_unwrap_parboxes`: a `~` is a space, a `\parbox`'s content stands in for it.
fn unwrap_parboxes(nodes: &[N]) -> Vec<N> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        if matches!(&node.kind, K::Specials(s) if s == "~") {
            out.push(N::chars(" "));
            continue;
        }
        if node.macro_name() == Some("parbox") {
            if let Some(list) = node.last_arg().and_then(N::group_list) {
                out.extend(list.iter().cloned());
                continue;
            }
        }
        out.push(node.clone());
    }
    out
}

/// `_split_on_and`: the segments between `\and`s, empty ones dropped.
fn split_on_and(nodes: Vec<N>) -> Vec<Vec<N>> {
    let mut segments: Vec<Vec<N>> = vec![Vec::new()];
    for node in nodes {
        if node.macro_name() == Some("and") {
            segments.push(Vec::new());
        } else if let Some(last) = segments.last_mut() {
            last.push(node);
        }
    }
    segments.retain(|s| !s.is_empty());
    segments
}

/// The byte offset of every char index of `s`, one past the end included.
fn char_byte_offsets(s: &str) -> Vec<usize> {
    let mut v: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    v.push(s.len());
    v
}

/// The char index of every byte offset of `s` (a continuation byte maps to
/// its char's index), one past the end included.
fn char_index_of_bytes(s: &str) -> Vec<usize> {
    let mut v = Vec::with_capacity(s.len() + 1);
    for (i, ch) in s.chars().enumerate() {
        v.extend(std::iter::repeat_n(i, ch.len_utf8()));
    }
    v.push(s.chars().count());
    v
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn skip_latex_space(text: &[char], mut cursor: usize) -> usize {
    while cursor < text.len() && text[cursor].is_whitespace() {
        cursor += 1;
    }
    cursor
}

/// The index past the group opened at `start`, escapes and comments
/// skipped; the end of the text when it never closes.
fn latex_group_end(text: &[char], start: usize, opening: char, closing: char) -> usize {
    let mut depth = 0i64;
    let mut index = start;
    while index < text.len() {
        if text[index] == '\\' && index + 1 < text.len() && "\\{}[]%".contains(text[index + 1]) {
            index += 2;
            continue;
        }
        if text[index] == '%' {
            index = find_newline(text, index).map_or(text.len(), |n| n + 1);
            continue;
        }
        if text[index] == opening {
            depth += 1;
        } else if text[index] == closing {
            depth -= 1;
            if depth == 0 {
                return index + 1;
            }
        }
        index += 1;
    }
    text.len()
}

fn find_newline(text: &[char], from: usize) -> Option<usize> {
    text[from..]
        .iter()
        .position(|&c| c == '\n')
        .map(|p| p + from)
}

fn starts_with_at(text: &[char], at: usize, pat: &str) -> bool {
    (at..)
        .zip(pat.chars())
        .all(|(i, p)| text.get(i) == Some(&p))
}

/// `_strip_latex_comments`: `%` comments out of a definition body; a
/// comment that ends a command name before a letter leaves a space, so
/// `\foo%↵bar` stays two tokens.
fn strip_latex_comments(text: &str) -> String {
    let text: Vec<char> = text.chars().collect();
    let is_command_letter = |c: char| c.is_ascii_alphabetic() || c == '@';
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index] == '\\' && index + 1 < text.len() && matches!(text[index + 1], '\\' | '%') {
            out.push(text[index]);
            out.push(text[index + 1]);
            index += 2;
            continue;
        }
        if text[index] == '%' {
            let mut next = find_newline(&text, index).map_or(text.len(), |n| n + 1);
            while next < text.len() && text[next] == '%' {
                next = find_newline(&text, next).map_or(text.len(), |n| n + 1);
            }
            if next < text.len() && is_command_letter(text[next]) {
                let mut word_start = index;
                while word_start > 0 && is_command_letter(text[word_start - 1]) {
                    word_start -= 1;
                }
                if word_start > 0 && text[word_start - 1] == '\\' {
                    let mut slash = word_start - 1;
                    while slash > 0 && text[slash - 1] == '\\' {
                        slash -= 1;
                    }
                    if (word_start - slash) % 2 == 1 {
                        // A space ends the command without taking its argument.
                        out.push(' ');
                    }
                }
            }
            index = next;
            continue;
        }
        out.push(text[index]);
        index += 1;
    }
    out
}

fn simple_declaration_end(text: &[char], command: &str, cursor: usize) -> Option<usize> {
    let mut cursor = skip_latex_space(text, cursor);
    if command == "newif" {
        if cursor >= text.len() || text[cursor] != '\\' {
            return None;
        }
        cursor += 1;
        while cursor < text.len() && text[cursor].is_alphabetic() {
            cursor += 1;
        }
        return Some(cursor);
    }
    let group_count = if command == "newtheoremstyle" { 9 } else { 1 };
    for _ in 0..group_count {
        cursor = skip_latex_space(text, cursor);
        if cursor >= text.len() || text[cursor] != '{' {
            return None;
        }
        cursor = latex_group_end(text, cursor, '{', '}');
    }
    if command == "newcounter" {
        let optional = skip_latex_space(text, cursor);
        if optional < text.len() && text[optional] == '[' {
            cursor = latex_group_end(text, optional, '[', ']');
        }
    }
    Some(cursor)
}

/// `_macro_definition_spans`: the balanced extent of every macro, length,
/// counter, conditional, environment and theorem definition, as char
/// ranges — comments and verbatim environments skipped.
fn macro_definition_spans(text: &[char]) -> Vec<(usize, usize)> {
    const COMMANDS: &[&str] = &[
        "newcommand",
        "renewcommand",
        "providecommand",
        "newenvironment",
        "newtheoremstyle",
        "newtheorem",
        "newlength",
        "newcounter",
        "newif",
        "def",
        "edef",
        "gdef",
        "xdef",
    ];
    let mut spans = Vec::new();
    let mut index = 0;
    while index < text.len() {
        if text[index] == '%' {
            index = find_newline(text, index).map_or(text.len(), |n| n + 1);
            continue;
        }
        if text[index] != '\\' {
            index += 1;
            continue;
        }
        if index + 1 < text.len() && matches!(text[index + 1], '\\' | '%') {
            index += 2;
            continue;
        }
        if let Some(env) = ["verbatim*", "verbatim", "lstlisting", "minted"]
            .iter()
            .find(|e| starts_with_at(text, index, &format!("\\begin{{{e}}}")))
        {
            let begin_len = format!("\\begin{{{env}}}").chars().count();
            let end_pat: Vec<char> = format!("\\end{{{env}}}").chars().collect();
            let from = index + begin_len;
            let found = (from..text.len().saturating_sub(end_pat.len() - 1))
                .find(|&i| text[i..i + end_pat.len()] == end_pat[..]);
            // Upstream steps past the end marker by the *begin* marker's length.
            index = found.map_or(text.len(), |end| end + begin_len);
            continue;
        }
        let command = COMMANDS.iter().find(|c| {
            starts_with_at(text, index + 1, c)
                && !text
                    .get(index + 1 + c.chars().count())
                    .is_some_and(|&n| is_word_char(n))
        });
        let Some(&command) = command else {
            index += 1;
            continue;
        };
        let match_end = index + 1 + command.len();
        let mut cursor = match_end;
        if matches!(
            command,
            "newif" | "newlength" | "newcounter" | "newtheoremstyle"
        ) {
            let end = simple_declaration_end(text, command, cursor);
            if let Some(end) = end {
                spans.push((index, end));
            }
            index = end.unwrap_or(match_end);
            continue;
        }
        if MACROS_NEWCOMMAND.contains(&command)
            || command == "newenvironment"
            || command == "newtheorem"
        {
            cursor = skip_latex_space(text, cursor);
            if cursor < text.len() && text[cursor] == '*' {
                cursor += 1;
            }
            cursor = skip_latex_space(text, cursor);
            if cursor < text.len() && text[cursor] == '{' {
                cursor = latex_group_end(text, cursor, '{', '}');
            } else if cursor < text.len() && text[cursor] == '\\' {
                cursor += 1;
                while cursor < text.len() && text[cursor].is_alphabetic() {
                    cursor += 1;
                }
            }
            let option_count = if command == "newtheorem" { 1 } else { 2 };
            for _ in 0..option_count {
                cursor = skip_latex_space(text, cursor);
                if cursor < text.len() && text[cursor] == '[' {
                    cursor = latex_group_end(text, cursor, '[', ']');
                }
            }
        } else {
            while cursor < text.len() && text[cursor] != '{' {
                cursor += 1;
            }
        }
        cursor = skip_latex_space(text, cursor);
        if cursor < text.len() && text[cursor] == '{' {
            let mut end = latex_group_end(text, cursor, '{', '}');
            if command == "newenvironment" {
                let second = skip_latex_space(text, end);
                if second < text.len() && text[second] == '{' {
                    end = latex_group_end(text, second, '{', '}');
                }
            } else if command == "newtheorem" {
                let second = skip_latex_space(text, end);
                if second < text.len() && text[second] == '[' {
                    end = latex_group_end(text, second, '[', ']');
                }
            }
            spans.push((index, end));
            index = end;
        } else {
            index = match_end;
        }
    }
    spans
}

/// The `replace` step of `_preprocess_custom_macros`: each shortcut use
/// (`\\(?:names)(?![A-Za-z@])`, longest name first) becomes its body, and
/// `\hfuzz` / `\vfuzz` assignments are dropped.
fn replace_shortcuts(segment: &str, shortcuts: &[(String, String)]) -> String {
    let mut s = segment.to_string();
    if !shortcuts.is_empty() {
        let mut names: Vec<&(String, String)> = shortcuts.iter().collect();
        names.sort_by_key(|(n, _)| std::cmp::Reverse(n.chars().count()));
        let mut out = String::with_capacity(s.len());
        let mut rest = s.as_str();
        'scan: while let Some(i) = rest.find('\\') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            for (name, body) in &names {
                if let Some(tail) = after.strip_prefix(name.as_str()) {
                    if !tail.starts_with(|c: char| c.is_ascii_alphabetic() || c == '@') {
                        out.push_str(body);
                        rest = tail;
                        continue 'scan;
                    }
                }
            }
            out.push('\\');
            rest = after;
        }
        out.push_str(rest);
        s = out;
    }
    fuzz_re().replace_all(&s, "").into_owned()
}

/// Every `\name` not followed by an ASCII letter, replaced by `def`.
fn replace_macro_calls(s: &str, name: &str, def: &str) -> String {
    let pattern = format!("\\{name}");
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(&pattern) {
        let after = &rest[i + pattern.len()..];
        out.push_str(&rest[..i]);
        if after.starts_with(|c: char| c.is_ascii_alphabetic()) {
            out.push_str(&pattern);
        } else {
            out.push_str(def);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn find_document_env(nodes: &[N], depth: usize) -> Option<&K> {
    if depth > 10 {
        return None;
    }
    for node in nodes {
        if node.env_name() == Some("document") {
            return Some(&node.kind);
        }
        for children in node.child_lists() {
            if let Some(found) = find_document_env(children, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn parse_custom_macro_num_args(arg: Option<&N>) -> usize {
    arg.and_then(|a| {
        a.verbatim()
            .trim_matches(|c| "{}[] \n\t".contains(c))
            .parse::<i64>()
            .ok()
    })
    .filter(|n| *n > 0)
    .map_or(0, |n| n as usize)
}

fn heading_level(name: &str) -> u8 {
    match name {
        "part" | "chapter" | "section" => 1,
        "subsection" => 2,
        "subsubsection" => 3,
        "paragraph" => 4,
        "subparagraph" => 5,
        _ => 1,
    }
}

/// Python's `str.capitalize()`.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// `_extract_verbatim_content`: the body between `\begin{env}[…]` and
/// `\end{env}`, leading blank lines and trailing whitespace dropped — the
/// first line keeps its indentation (docling#4502); the whole text when the
/// pattern fails.
fn extract_verbatim_content(latex: &str, env: &str) -> String {
    let begin = format!("\\begin{{{env}}}");
    let end = format!("\\end{{{env}}}");
    let Some(start) = latex.find(&begin) else {
        return latex.to_string();
    };
    let mut body_start = start + begin.len();
    // An optional `[…]` right after `\begin{env}` (non-greedy, may span lines).
    if latex[body_start..].starts_with('[') {
        if let Some(close) = latex[body_start..].find(']') {
            body_start += close + 1;
        }
    }
    let Some(stop) = latex[body_start..].find(&end) else {
        return latex.to_string();
    };
    let mut body = &latex[body_start..body_start + stop];
    // `\A(?:[ \t]*\r?\n)+`
    loop {
        let trimmed = body.trim_start_matches([' ', '\t']);
        let rest = trimmed
            .strip_prefix('\n')
            .or_else(|| trimmed.strip_prefix("\r\n"));
        match rest {
            Some(rest) => body = rest,
            None => break,
        }
    }
    body.trim_end().to_string()
}

fn validate_tikz_nodelist(nodes: &[N], depth: usize) -> bool {
    if depth > 50 {
        return false;
    }
    for node in nodes {
        if node.env_name() == Some("tikzpicture") && !tikz_end_re().is_match(node.verbatim()) {
            return false;
        }
        for children in node.child_lists() {
            if !validate_tikz_nodelist(children, depth + 1) {
                return false;
            }
        }
    }
    true
}

/// `path` resolved, when it stays under `base` (upstream's traversal guard).
fn within(base: &Path, path: &Path) -> Option<PathBuf> {
    let base = base.canonicalize().ok()?;
    let resolved = path.canonicalize().ok()?;
    resolved.starts_with(&base).then_some(resolved)
}

fn tikz_end_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\end\s*\{\s*tikzpicture\s*\}").expect("tikz regex"))
}

fn label_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\\label\{[^\n]*?\}").expect("label regex"))
}

fn spaces_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(" +").expect("spaces regex"))
}

fn blank_lines_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new("\n\n+").expect("blank lines regex"))
}

/// `\def\name{body}` (`re.fullmatch`, the body up to the last brace).
fn def_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?s)\A\\(?:def|edef|gdef|xdef)\s*\\([A-Za-z@]+)\s*\{(.*)\}\z")
            .expect("def regex")
    })
}

fn environment_alias_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\A(?:\\(?:begin|end)\{[^{}]+\}|\$\$)\z").expect("alias regex"))
}

fn fuzz_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\\[vh]fuzz\s*=\s*[+-]?(?:\d+(?:\.\d*)?|\.\d+)\s*(?:pt|pc|in|bp|cm|mm|dd|cc|sp|em|ex)\b",
        )
        .expect("fuzz regex")
    })
}

#[cfg(test)]
mod tests;
