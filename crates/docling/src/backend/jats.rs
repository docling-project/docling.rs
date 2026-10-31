//! JATS XML backend — a port of docling's `JatsDocumentBackend` (scientific
//! article XML). Emits the title, authors, affiliations and abstract from
//! `article-meta`, then walks `<body>` and the `<back>` matter with a port of
//! docling's `_walk_linear`: sections → headings, paragraphs → text, plus the
//! full article-body machinery — `<table-wrap>` tables (with caption), `<fig>`
//! figures (caption + picture), `<list>`/`<list-item>` bullet lists,
//! `<ref-list>` references and `<element-citation>`/`<mixed-citation>` citations,
//! `<fn-group>` footnotes, and `<disp-formula>` equations (`$$…$$`). Inline
//! emphasis (`<italic>`/`<bold>`/…) and `<inline-formula>` are preserved as
//! styled runs / `$…$` (docling PR #3726), and a `<table-wrap>`'s label+caption
//! and header/span structure carry onto the table.

use std::path::{Path, PathBuf};

use roxmltree::{Document, Node as XmlNode, ParsingOptions};

use crate::backend::images::ImagePolicy;
use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::tree::{Formatting, ItemTree, ListMeta, TreeKind};
use docling_core::{
    inline_paragraph_node, DoclingDocument, InlineRun, ListItemDclx, Node, PictureImage, Script,
    Table, TableStructure,
};

/// JATS backend. `images` at `Local` or above is docling's `JatsBackendOptions.fetch_images`
/// together with its `enable_local_fetch` (docling#4041, #392): when set, a
/// `<fig>`'s `<graphic xlink:href>` is read from disk relative to the source
/// file's directory and embedded; off (the default), a figure is a picture
/// with no image, as docling emits it.
#[derive(Default)]
pub struct JatsBackend {
    pub images: ImagePolicy,
}

const SKIP_TEXT: &[&str] = &["term", "disp-formula", "inline-formula"];

/// Inline emphasis accumulated from the enclosing JATS tags (docling PR #3726's
/// `_JATS_FORMAT_TAG_MAP`): `<bold>`, `<italic>`, `<underline>`, `<strike>` and
/// sub/superscript. Threaded down the walk and attached to each text run.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Fmt {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    script: Script,
}

impl Fmt {
    /// Apply the formatting a JATS emphasis tag adds (unknown tags pass through).
    fn with_tag(self, tag: &str) -> Fmt {
        match tag {
            "bold" => Fmt { bold: true, ..self },
            "italic" => Fmt {
                italic: true,
                ..self
            },
            "underline" => Fmt {
                underline: true,
                ..self
            },
            "strike" => Fmt {
                strike: true,
                ..self
            },
            "sub" => Fmt {
                script: Script::Sub,
                ..self
            },
            "sup" => Fmt {
                script: Script::Super,
                ..self
            },
            _ => self,
        }
    }

    fn to_inline_run(self, text: &str) -> InlineRun {
        InlineRun {
            text: text.to_string(),
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
            strike: self.strike,
            script: self.script,
            code: false,
            formula: false,
        }
    }
}

/// One inline run of a JATS paragraph: styled text, or an inline formula whose
/// `text` is the LaTeX body (docling PR #3726's `InlineSegment`).
struct Seg {
    formula: bool,
    text: String,
    fmt: Fmt,
    /// External link target inherited from an enclosing `<ext-link>`
    /// (docling#4029); runs coalesce only when formatting *and* link match.
    hyperlink: Option<String>,
}

/// Tags that, inside a `<p>`, flush the accumulated paragraph text before they
/// are handled — docling's `_walk_linear` `flush_tags`.
const FLUSH_TAGS: &[&str] = &["ack", "sec", "list", "boxed-text", "disp-formula", "fig"];

const DEFAULT_HEADER_ACKNOWLEDGMENTS: &str = "Acknowledgments";
const DEFAULT_HEADER_FOOTNOTES: &str = "Footnotes";
const DEFAULT_HEADER_REFERENCES: &str = "References";
const DEFAULT_TEXT_ETAL: &str = "et al.";
/// `list-type` values that number the items, from the JATS 1.3 tag library —
/// docling's `_ORDERED_LIST_TYPES` (docling#4770). Every other value
/// (`bullet`, `simple`, `none`, unknown, absent) is a bullet list.
const ORDERED_LIST_TYPES: &[&str] = &[
    "order",
    "alpha-lower",
    "alpha-upper",
    "roman-lower",
    "roman-upper",
];

/// The two outputs the walk fills side by side: the flat nodes (Markdown /
/// DocLang / LaTeX) and docling's item tree (JSON), the latter mirroring
/// `JatsDocumentBackend`'s `parent` threading — the title is the root every
/// section hangs off, a `<sec>` heading parents what follows it, a `<list>`
/// is a `list` group, a paragraph of several runs an `inline` group, figure
/// and table captions body-level `caption` items the picture / table
/// references, a `<fn-group>` a heading over a `footnotes` list of empty
/// items each holding an inline group with the `footnote` text.
struct Out<'a> {
    doc: &'a mut DoclingDocument,
    tree: ItemTree,
}

impl Out<'_> {
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        parent: Option<usize>,
        label: &str,
        text: &str,
        formatting: Option<Formatting>,
        hyperlink: Option<String>,
        level: Option<u8>,
        list: Option<ListMeta>,
    ) -> usize {
        self.tree.add(
            parent,
            None,
            TreeKind::Text {
                label: label.into(),
                text: text.into(),
                orig: None,
                formatting,
                hyperlink,
                level,
                list,
            },
        )
    }

    /// `add_heading(text, parent, level)`.
    fn heading(&mut self, parent: Option<usize>, text: &str, level: i32) -> usize {
        self.text(
            parent,
            "section_header",
            text,
            None,
            None,
            Some(level.clamp(1, 100) as u8),
            None,
        )
    }

    fn group(&mut self, parent: Option<usize>, label: &str, name: &str) -> usize {
        self.tree.add(
            parent,
            None,
            TreeKind::Group {
                label: label.into(),
                name: name.into(),
            },
        )
    }

    /// `add_list_item(text, parent)` with docling's defaults (`enumerated=False`,
    /// marker `""`).
    fn list_item(&mut self, parent: Option<usize>, text: &str) -> usize {
        self.text(
            parent,
            "list_item",
            text,
            None,
            None,
            None,
            Some(ListMeta::default()),
        )
    }

    /// `add_list_item(text, enumerated=…, marker=…, parent)` — a JATS
    /// `<list-item>` with its list's numbering and its `<label>` (docling#4770).
    fn list_item_meta(&mut self, parent: Option<usize>, text: &str, meta: ListMeta) -> usize {
        self.text(parent, "list_item", text, None, None, None, Some(meta))
    }

    /// Whether `parent` is a `list` group — docling's `_add_citation` test.
    fn is_list(&self, parent: Option<usize>) -> bool {
        parent.is_some_and(|p| {
            matches!(&self.tree.items[p].kind, TreeKind::Group { label, .. } if label == "list")
        })
    }
}

/// docling's `Formatting` for a run: `None` while no emphasis tag applied
/// (`_merge_formatting` only creates one under a mapped tag).
fn tree_formatting(fmt: Fmt) -> Option<Formatting> {
    (fmt != Fmt::default()).then_some(Formatting {
        bold: fmt.bold,
        italic: fmt.italic,
        underline: fmt.underline,
        strikethrough: fmt.strike,
        script: fmt.script,
    })
}

impl DeclarativeBackend for JatsBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let xml = source.text()?;
        super::xml_depth::check(&xml, "jats")?;
        // JATS files carry a DOCTYPE/DTD reference, which roxmltree rejects by default.
        let opts = ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let dom = Document::parse_with_options(&xml, opts)
            .map_err(|e| ConversionError::with_source("jats", e))?;
        let mut doc = DoclingDocument::new(&source.name);
        let mut out = Out {
            doc: &mut doc,
            tree: ItemTree::default(),
        };

        // --- metadata -------------------------------------------------------
        // docling's `_add_title`: the title item is *always* the root the
        // whole document hangs off (an empty text when the article has no
        // title-group).
        let title = parse_title(&dom);
        if let Some(title) = &title {
            out.doc.push(Node::Heading {
                level: 1,
                text: escape_text(title),
            });
        }
        let root = out.text(
            None,
            "title",
            title.as_deref().unwrap_or(""),
            None,
            None,
            None,
            None,
        );
        let root = Some(root);
        let (authors, affiliations) = parse_authors(&dom);
        if !authors.is_empty() {
            let joined = authors.join(", ");
            out.doc.push(Node::Paragraph {
                text: escape_text(&joined),
            });
            out.text(root, "paragraph", &joined, None, None, None, None);
        }
        if !affiliations.is_empty() {
            let joined = affiliations.join("; ");
            out.doc.push(Node::Paragraph {
                text: escape_text(&joined),
            });
            out.text(root, "paragraph", &joined, None, None, None, None);
        }
        // Figure images resolve against the source *file's* directory only
        // (docling's `base_path`: its `source_uri` or the path it was opened
        // from) — an in-memory source, or one fetched from a URL, embeds none,
        // as docling's does (`_load_figure_image` requires a local base).
        let fig_base = if self.images.sources.local() {
            source.base_dir()
        } else {
            None
        };
        for abs in parse_abstracts(&dom) {
            // docling skips an abstract with neither plain paragraphs nor
            // sections (`_add_abstract`, docling#4172).
            if abs.paragraphs.is_empty() && abs.sections.is_empty() {
                continue;
            }
            let label = if abs.label.is_empty() {
                "Abstract"
            } else {
                &abs.label
            };
            // The levels are docling's `self.hlevel + 1` (the abstract heading)
            // and `+ 2` (a section's), with `hlevel` still 0: the abstract is
            // added before the body walk, the only thing that moves it — so
            // the constants are exact, not a shortcut.
            out.doc.push(Node::Heading {
                level: 2,
                text: escape_text(label),
            });
            let abs_heading = Some(out.heading(root, label, 1));
            // Each paragraph is walked like a body paragraph, its styling
            // kept (docling#4361) — the plain ones first, then each `<sec>`
            // of a structured abstract (docling#4172) as a heading one level
            // below the abstract's, none when it has no title.
            let mut no_sections = 0;
            for p in &abs.paragraphs {
                walk_linear(
                    *p,
                    abs_heading,
                    Fmt::default(),
                    None,
                    &mut no_sections,
                    fig_base,
                    &mut out,
                );
            }
            for (title, paragraphs) in &abs.sections {
                let section_parent = if title.is_empty() {
                    abs_heading
                } else {
                    out.doc.push(Node::Heading {
                        level: 3,
                        text: escape_text(title),
                    });
                    Some(out.heading(abs_heading, title, 2))
                };
                for p in paragraphs {
                    walk_linear(
                        *p,
                        section_parent,
                        Fmt::default(),
                        None,
                        &mut no_sections,
                        fig_base,
                        &mut out,
                    );
                }
            }
        }

        // --- body + back ----------------------------------------------------
        // `hlevel` is a running section depth carried across body and back
        // (docling's `self.hlevel`); it is balanced by each `<sec>`.
        let mut hlevel: i32 = 0;
        // The article's own body and back matter — not a sub-article's
        // (docling#4826) — then each top-level sub-article.
        for tag in ["body", "back"] {
            if let Some(node) = dom
                .descendants()
                .find(|n| n.has_tag_name(tag) && !in_sub_article(*n))
            {
                walk_linear(
                    node,
                    root,
                    Fmt::default(),
                    None,
                    &mut hlevel,
                    fig_base,
                    &mut out,
                );
            }
        }
        let subs: Vec<XmlNode> = dom
            .descendants()
            .filter(|n| is_sub_article(*n) && !in_sub_article(*n))
            .collect();
        for sub in subs {
            add_sub_article(sub, root, &mut hlevel, fig_base, &mut out);
        }
        let tree = out.tree;
        doc.tree = Some(tree);
        Ok(doc)
    }
}

/// `<sub-article>` / `<response>`: a whole article inside another one — a
/// decision letter, an author reply (JATS 1.3, docling's `_SUB_ARTICLE_TAGS`).
fn is_sub_article(n: XmlNode) -> bool {
    n.has_tag_name("sub-article") || n.has_tag_name("response")
}

/// Whether `n` lies inside a sub-article.
fn in_sub_article(n: XmlNode) -> bool {
    n.ancestors().skip(1).any(is_sub_article)
}

/// docling's `_add_sub_article` (docling#4826): a sub-article's body and back
/// matter, like a `<sec>`, under a heading with its title when it has one;
/// a nested sub-article under the heading of the one that holds it. They
/// were dropped — the decision letter and the author response of an eLife
/// paper.
fn add_sub_article(
    node: XmlNode,
    parent: Option<usize>,
    hlevel: &mut i32,
    fig_base: Option<&std::path::Path>,
    out: &mut Out,
) {
    let title = node
        .children()
        .filter(|c| c.has_tag_name("front-stub") || c.has_tag_name("front"))
        .flat_map(|front| {
            let meta = if front.has_tag_name("front") {
                front.children().find(|c| c.has_tag_name("article-meta"))
            } else {
                Some(front)
            };
            meta.into_iter()
                .flat_map(|m| m.children().filter(|c| c.has_tag_name("title-group")))
                .flat_map(|g| g.children().filter(|c| c.has_tag_name("article-title")))
        })
        .next();
    let text = title.map(|t| normalize(&get_text(t))).unwrap_or_default();
    let mut new_parent = parent;
    if !text.is_empty() {
        *hlevel += 1;
        out.doc.push(Node::Heading {
            level: fw_level(*hlevel),
            text: escape_text(&text),
        });
        new_parent = Some(out.heading(parent, &text, *hlevel));
    }
    for child in node
        .children()
        .filter(|c| c.has_tag_name("body") || c.has_tag_name("back"))
    {
        walk_linear(
            child,
            new_parent,
            Fmt::default(),
            None,
            hlevel,
            fig_base,
            out,
        );
    }
    for child in node.children().filter(|c| is_sub_article(*c)) {
        add_sub_article(child, new_parent, hlevel, fig_base, out);
    }
    if !text.is_empty() {
        *hlevel -= 1;
    }
}

/// Recursive text of a node: its text + descendants + tails, skipping formula
/// tags, then whitespace-normalized — docling's `_get_text` + `_normalize`.
fn raw_text(node: XmlNode, out: &mut String) {
    if let Some(t) = node.text() {
        out.push_str(&t.replace('\n', " "));
    }
    for child in node.children() {
        if child.is_element() {
            if !SKIP_TEXT.contains(&child.tag_name().name()) {
                raw_text(child, out);
            }
            if let Some(tail) = child.tail() {
                out.push_str(&tail.replace('\n', " "));
            }
        } else if child.is_text() {
            // handled by node.text()/tail above for elements; bare text nodes here
        }
    }
}

/// Generic XML text reconstruction — docling's fallback for an XML document
/// saved with a `.txt` extension (it is *not* parsed with the semantic JATS
/// backend). Every leaf/mixed element with text becomes a `<text>` item in
/// document order; `<table-wrap>`/`<table>` become tables. This mirrors
/// docling's behaviour of walking such a file element-by-element.
pub(crate) fn convert_generic(source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
    let xml = source.text()?;
    super::xml_depth::check(&xml, "xml")?;
    let opts = ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let dom = Document::parse_with_options(&xml, opts)
        .map_err(|e| ConversionError::with_source("xml", e))?;
    let mut doc = DoclingDocument::new(&source.name);
    walk_generic(dom.root_element(), &mut doc);
    Ok(doc)
}

fn walk_generic(node: XmlNode, doc: &mut DoclingDocument) {
    for child in node.children().filter(XmlNode::is_element) {
        let tag = child.tag_name().name();
        if tag == "table-wrap" {
            // The generic XML path stays flat: the table's tree entry is
            // built and dropped.
            let mut out = Out {
                doc,
                tree: ItemTree::default(),
            };
            add_table(&mut out, None, child);
            continue;
        }
        if tag == "table" {
            if let Some(t) = parse_jats_table(child) {
                doc.push(Node::Table(t));
            }
            continue;
        }
        // A leaf element (no child elements) or a mixed-content element (its own
        // text interleaved with inline markup) is one text item; a pure
        // container (only child elements, no direct text) is walked into.
        let has_direct_text = child
            .children()
            .any(|c| c.is_text() && !c.text().unwrap_or("").trim().is_empty());
        let has_element_children = child.children().any(|c| c.is_element());
        if !has_element_children || has_direct_text {
            let t = normalize(&sanitize_generic(&generic_text(child)));
            if !t.trim().is_empty() {
                doc.push(Node::Paragraph {
                    text: escape_text(&t),
                });
            }
        } else {
            walk_generic(child, doc);
        }
    }
}

/// docling's Unicode text sanitization (em/en dashes → hyphen, curly quotes →
/// straight) applied to the generically reconstructed text.
fn sanitize_generic(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2014}' | '\u{2013}' => '-',
            '\u{2019}' | '\u{2018}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            c => c,
        })
        .collect()
}

/// Concatenate an element's text as docling does for the generic reconstruction:
/// a nested element's text is padded with a space on each side (so inline
/// cross-references read `text ( ref ) text`); collapsed by [`normalize`].
fn generic_text(node: XmlNode) -> String {
    let mut s = String::new();
    for child in node.children() {
        if child.is_text() {
            s.push_str(child.text().unwrap_or(""));
        } else if child.is_element() {
            s.push(' ');
            s.push_str(&generic_text(child));
            s.push(' ');
        }
    }
    s
}

fn node_text(node: XmlNode) -> String {
    let mut s = String::new();
    raw_text(node, &mut s);
    normalize(&s)
}

/// Collapse runs of ASCII whitespace to one space and trim the ends — what
/// docling's `_get_text(...).strip()` amounts to on clean JATS sources. Only
/// *ASCII* whitespace collapses: a no-break space (`A. S. de\u{a0}Castro` in
/// a citation) is content docling keeps verbatim, and Rust's
/// `split_whitespace` would eat it.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for c in s.trim().chars() {
        if c.is_ascii_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        }
    }
    out
}

/// docling's `_parse_title`: every `title-group` directly under an
/// `article-meta`/`collection-meta`/`book-meta`/`book-part-meta`, its
/// `article-title`/`subtitle`/`title`/`label` children joined by a space,
/// the groups joined by ` - `. Each child contributes `elem.text` — the text
/// **before its first child element** only — so a title with inline markup
/// (`… of <italic>Yersinia pestis</italic> to …`) is cut at the markup, as
/// upstream's `pmc2231364` groundtruth shows (`# Global transcriptional
/// response of`). Replicated for byte parity (#391; docs/MIGRATION.md).
fn parse_title(dom: &Document) -> Option<String> {
    const METAS: [&str; 4] = [
        "article-meta",
        "collection-meta",
        "book-meta",
        "book-part-meta",
    ];
    const NAMES: [&str; 4] = ["article-title", "subtitle", "title", "label"];
    let titles: Vec<String> = dom
        .descendants()
        .filter(|n| {
            n.has_tag_name("title-group")
                && n.parent()
                    .is_some_and(|p| METAS.contains(&p.tag_name().name()))
        })
        .map(|group| {
            group
                .children()
                .filter(|c| c.is_element() && NAMES.contains(&c.tag_name().name()))
                .map(|c| direct_text(c).replace('\n', " ").trim().to_string())
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        })
        .collect();
    let text = titles.join(" - ");
    (!text.is_empty()).then_some(text)
}

/// lxml's `elem.text`: the character data before the element's first child
/// element (empty when the element opens with markup).
fn direct_text<'a>(node: XmlNode<'a, 'a>) -> &'a str {
    node.first_child()
        .filter(XmlNode::is_text)
        .and_then(|c| c.text())
        .unwrap_or("")
}

/// Authors (`given-names surname`) and their (deduplicated) affiliation names.
fn parse_authors(dom: &Document) -> (Vec<String>, Vec<String>) {
    let Some(meta) = dom.descendants().find(|n| n.has_tag_name("article-meta")) else {
        return (Vec::new(), Vec::new());
    };
    // id -> affiliation name
    let mut aff_by_id = std::collections::HashMap::new();
    for aff in meta.descendants().filter(|n| n.has_tag_name("aff")) {
        let Some(id) = aff.attribute("id") else {
            continue;
        };
        // docling joins the affiliation's text fragments (itertext) with ", ".
        let mut name = aff
            .descendants()
            .filter(|n| n.is_text())
            .filter_map(|n| n.text())
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
            .replace('\n', " ");
        // strip a leading "<label>, " prefix
        if let Some(label) = aff
            .children()
            .find(|c| c.has_tag_name("label"))
            .and_then(|l| l.text())
        {
            name = name
                .strip_prefix(&format!("{label}, "))
                .unwrap_or(&name)
                .to_string();
        }
        aff_by_id.insert(id.to_string(), name);
    }

    let mut authors = Vec::new();
    let mut affiliations = Vec::new();
    for contrib in meta
        .descendants()
        .filter(|n| n.has_tag_name("contrib") && n.attribute("contrib-type") == Some("author"))
    {
        let name = contrib_name(contrib);
        if name.is_empty() {
            continue;
        }
        authors.push(name);
        for xref in contrib
            .children()
            .filter(|c| c.has_tag_name("xref") && c.attribute("ref-type") == Some("aff"))
        {
            if let Some(aff) = xref.attribute("rid").and_then(|id| aff_by_id.get(id)) {
                if !affiliations.contains(aff) {
                    affiliations.push(aff.clone());
                }
            }
        }
    }
    (authors, affiliations)
}

/// `prefix given-names surname suffix`, space-joined (docling `_parse_structured_name`).
fn contrib_name(contrib: XmlNode) -> String {
    let name = contrib.children().find(|c| c.has_tag_name("name"));
    let Some(name) = name else {
        return String::new();
    };
    ["prefix", "given-names", "surname", "suffix"]
        .iter()
        .filter_map(|tag| {
            name.children()
                .find(|c| c.has_tag_name(*tag))
                .and_then(|c| c.text())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// One `<abstract>`, as docling's `_parse_abstract` reads it (docling#4172).
struct Abstract<'a, 'i> {
    /// Its `title` or `label` child (the first in document order), or empty.
    label: String,
    /// The direct `<p>` children with text, walked at emit time so their
    /// styling stays (docling#4361).
    paragraphs: Vec<XmlNode<'a, 'i>>,
    /// The direct `<sec>` children that hold paragraphs: `(title, paragraphs)`.
    /// A section's title is its `title`/`label`; its paragraphs are its
    /// direct `<p>` children only — a nested `<sec>` is not descended into.
    sections: Vec<(String, Vec<XmlNode<'a, 'i>>)>,
}

/// A `<p>` whose text (`"".join(itertext())`) is not blank.
fn has_text(p: XmlNode) -> bool {
    p.descendants()
        .filter(|n| n.is_text())
        .any(|n| n.text().is_some_and(|t| !t.trim().is_empty()))
}

fn parse_abstracts<'a, 'i>(dom: &'a Document<'i>) -> Vec<Abstract<'a, 'i>> {
    let mut out = Vec::new();
    for abs in dom.descendants().filter(|n| n.has_tag_name("abstract")) {
        let mut paragraphs = Vec::new();
        let mut sections = Vec::new();
        for child in abs.children().filter(XmlNode::is_element) {
            match child.tag_name().name() {
                "p" if has_text(child) => paragraphs.push(child),
                "sec" => {
                    let section = abstract_section(child);
                    if !section.1.is_empty() {
                        sections.push(section);
                    }
                }
                _ => {}
            }
        }
        out.push(Abstract {
            label: title_or_label(abs),
            paragraphs,
            sections,
        });
    }
    out
}

/// docling's `_parse_abstract_section`: `(title, paragraphs)` of one `<sec>`.
fn abstract_section<'a, 'i>(section: XmlNode<'a, 'i>) -> (String, Vec<XmlNode<'a, 'i>>) {
    let paragraphs = section
        .children()
        .filter(|c| c.has_tag_name("p") && has_text(*c))
        .collect();
    (title_or_label(section), paragraphs)
}

/// The first `title` or `label` child in document order (docling's
/// `xpath("title|label")[0]`), or empty.
fn title_or_label(node: XmlNode) -> String {
    node.children()
        .find(|c| c.has_tag_name("title") || c.has_tag_name("label"))
        .map(node_text)
        .unwrap_or_default()
}

/// `_get_text`, un-normalized (newlines → spaces, formula tags skipped).
fn get_text(node: XmlNode) -> String {
    let mut s = String::new();
    raw_text(node, &mut s);
    s
}

/// The un-normalized text of a node, trimmed and whitespace-normalized — used for
/// list items, captions and citations (docling `_get_text(...).strip()`, which on
/// clean JATS sources is equivalent to a whitespace collapse).
fn norm_text(node: XmlNode) -> String {
    normalize(&get_text(node))
}

/// Markdown heading level for docling heading level `dl` (docling renders a
/// heading at level `N` with `N+1` hashes; `docling.rs`'s serializer emits a
/// `Heading{level}` with `level` hashes, so `docling.rs level = dl + 1`).
fn fw_level(dl: i32) -> u8 {
    (dl + 1).clamp(1, 6) as u8
}

/// A `<sec>`/`<ack>` header text (`title|label`), or a default for `<ack>`.
fn header_text(child: XmlNode) -> Option<String> {
    child
        .children()
        .find(|c| c.has_tag_name("title") || c.has_tag_name("label"))
        .map(get_text)
        .map(|s| normalize(&s))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            child
                .has_tag_name("ack")
                .then(|| DEFAULT_HEADER_ACKNOWLEDGMENTS.to_string())
        })
}

/// docling's `_add_citation`: a list item under a `list` group, a text item
/// elsewhere.
fn add_citation(out: &mut Out, parent: Option<usize>, text: &str) {
    if text.is_empty() {
        return;
    }
    if out.is_list(parent) {
        out.doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: false,
            text: escape_text(text),
            level: 0,
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
        out.list_item(parent, text);
    } else {
        out.doc.push(Node::Paragraph {
            text: escape_text(text),
        });
        out.text(parent, "text", text, None, None, None, None);
    }
}

/// Port of docling's `_walk_linear`: a depth-first walk that accumulates a
/// paragraph's inline runs (styled text + inline formulas) while emitting
/// block-level items (sections, lists, figures, tables, citations, footnotes,
/// formulas) as it goes. `fmt` is the emphasis inherited from the enclosing
/// tags. Returns the runs it could not emit, backpropagated to the enclosing
/// paragraph.
fn walk_linear(
    node: XmlNode,
    parent: Option<usize>,
    fmt: Fmt,
    hyperlink: Option<&str>,
    hlevel: &mut i32,
    fig_base: Option<&Path>,
    out: &mut Out,
) -> Vec<Seg> {
    let node_tag = node.tag_name().name();
    // An emphasis tag (`<italic>`, `<bold>`, …) contributes its formatting to
    // every run beneath it (docling PR #3726); other tags leave it unchanged.
    let current = fmt.with_tag(node_tag);
    // An `<ext-link xlink:href>` makes every run beneath it a hyperlink
    // (docling#4029); a blank href keeps the enclosing link, if any.
    let own_link = (node_tag == "ext-link")
        .then(|| ext_link_href(node))
        .flatten();
    let current_link: Option<&str> = own_link.as_deref().or(hyperlink);
    let mut segments: Vec<Seg> = Vec::new();
    if node_tag != "term" {
        if let Some(t) = node.text() {
            append_run(&mut segments, t, current, current_link);
        }
    }

    // docling's `new_parent`: what the recursion hangs items off. Set once
    // per element, *not* per child — so once a `<sec>` (or `<list>`,
    // `<ref-list>`) child has moved it, the following siblings are walked
    // under that heading / group too. `ptag100`'s back matter relies on it:
    // the paragraphs after the `Funding` section's `<sec>` sit under its
    // heading in upstream's tree.
    let mut new_parent = parent;
    for child in node.children().filter(XmlNode::is_element) {
        let mut stop_walk = false;
        let ctag = child.tag_name().name();

        // Flush accumulated inline runs before a block-level child.
        if node_tag == "p" && FLUSH_TAGS.contains(&ctag) {
            emit_inline(out, parent, std::mem::take(&mut segments));
        }

        // Whether this child opened a section (so we decrement `hlevel` after).
        let mut opened_section = false;

        match ctag {
            "sec" | "ack" => {
                if let Some(text) = header_text(child) {
                    *hlevel += 1;
                    out.doc.push(Node::Heading {
                        level: fw_level(*hlevel),
                        text: escape_text(&text),
                    });
                    new_parent = Some(out.heading(parent, &text, *hlevel));
                    opened_section = true;
                }
            }
            "list" => {
                new_parent = Some(out.group(parent, "list", "list"));
            }
            "list-item" => {
                // The item's text is its `<p>` children, its `<label>` the
                // marker and its list's `list-type` the numbering
                // (docling#4770); a nested <list> is real structure whose
                // items follow one level deeper (docling PR #3619).
                add_list_item(out, parent, child, 0);
                stop_walk = true;
            }
            "fig" => {
                add_figure(out, parent, child, fig_base);
                stop_walk = true;
            }
            "table-wrap" => {
                add_table(out, parent, child);
                stop_walk = true;
            }
            "supplementary-material" => {
                stop_walk = true;
            }
            "fn-group" => {
                add_footnote_group(out, parent, child, *hlevel);
                stop_walk = true;
            }
            "ref-list" if node_tag != "ref-list" => {
                let text = child
                    .children()
                    .find(|c| c.has_tag_name("title") || c.has_tag_name("label"))
                    .map(|h| normalize(&get_text(h)))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_HEADER_REFERENCES.to_string());
                out.doc.push(Node::Heading {
                    level: fw_level(1),
                    text: escape_text(&text),
                });
                // docling: `add_heading(text, parent)` at its default level 1,
                // then a `list` group under it for the citations.
                let heading = out.heading(parent, &text, 1);
                new_parent = Some(out.group(Some(heading), "list", "list"));
            }
            "element-citation" => {
                let text = parse_element_citation(child);
                add_citation(out, parent, &text);
                stop_walk = true;
            }
            "mixed-citation" => {
                let text = norm_text(child);
                add_citation(out, parent, &text);
                stop_walk = true;
            }
            "tex-math" => {
                add_equation(out, parent, child);
                stop_walk = true;
            }
            "inline-formula" => {
                // Inline formula: the `<tex-math>` stays inline (unlike a block
                // `<disp-formula>`), carrying any enclosing emphasis (#3726).
                extend_segments(
                    &mut segments,
                    walk_inline_formula(child, current, current_link),
                );
                stop_walk = true;
            }
            _ => {}
        }

        if !stop_walk {
            let child_segments = walk_linear(
                child,
                new_parent,
                current,
                current_link,
                hlevel,
                fig_base,
                out,
            );
            // Don't fold a flushed block's runs back into an enclosing paragraph.
            let parent_is_p = node.parent().map(|p| p.has_tag_name("p")).unwrap_or(false);
            if !(parent_is_p && FLUSH_TAGS.contains(&node_tag)) {
                extend_segments(&mut segments, child_segments);
            }
            if opened_section {
                *hlevel -= 1;
            }
        }

        if let Some(tail) = child.tail() {
            append_run(&mut segments, tail, current, current_link);
        }
    }

    if node_tag == "p" {
        emit_inline(out, parent, segments);
        Vec::new()
    } else {
        segments
    }
}

/// Walk an `<inline-formula>`: recognize its `<tex-math>` as a formula run and
/// keep every other text run inline, carrying `fmt` (docling's
/// `_walk_inline_formula`).
fn walk_inline_formula(node: XmlNode, fmt: Fmt, hyperlink: Option<&str>) -> Vec<Seg> {
    let current = fmt.with_tag(node.tag_name().name());
    let mut segments = Vec::new();
    if let Some(t) = node.text() {
        append_run(&mut segments, t, current, hyperlink);
    }
    for child in node.children().filter(XmlNode::is_element) {
        if child.tag_name().name() == "tex-math" {
            if let Some(formula) = extract_tex_math(child) {
                segments.push(Seg {
                    formula: true,
                    text: formula,
                    fmt: Fmt::default(),
                    hyperlink: hyperlink.map(str::to_string),
                });
            }
        } else {
            extend_segments(
                &mut segments,
                walk_inline_formula(child, current, hyperlink),
            );
        }
        if let Some(tail) = child.tail() {
            append_run(&mut segments, tail, current, hyperlink);
        }
    }
    segments
}

/// The `xlink:href` of an `<ext-link>` (any namespace prefix), trimmed;
/// `None` when absent or blank. URLs are normalized the way pydantic's
/// `AnyUrl` serializes them (docling stores the parsed URL), so a bare
/// `https://host` gains its trailing slash.
fn ext_link_href(node: XmlNode) -> Option<String> {
    let href = node
        .attributes()
        .find(|a| a.name() == "href")
        .map(|a| a.value().trim())
        .filter(|v| !v.is_empty())?;
    Some(crate::backend::html::normalize_url(href))
}

/// The formula body of a `<tex-math>` — the text between `$$…$$` or `$…$`
/// delimiters, else the trimmed text (docling's `_extract_tex_math`).
fn extract_tex_math(node: XmlNode) -> Option<String> {
    let text = node.text()?.trim().to_string();
    for delim in ["$$", "$"] {
        if text.len() > 2 * delim.len() && text.starts_with(delim) && text.ends_with(delim) {
            let inner = text[delim.len()..text.len() - delim.len()]
                .trim()
                .to_string();
            return (!inner.is_empty()).then_some(inner);
        }
    }
    (!text.is_empty()).then_some(text)
}

/// docling's `_normalize_xml_line_breaks`: the source's line breaks and the
/// indentation around them collapse to one space — a line break's leading
/// and trailing blanks go, blank inner lines drop out.
fn xml_line_breaks(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\n') {
        return std::borrow::Cow::Borrowed(text);
    }
    let chunks: Vec<&str> = text.split('\n').collect();
    let last = chunks.len() - 1;
    let mut parts: Vec<&str> = vec![chunks[0].trim_end_matches([' ', '\t'])];
    parts.extend(
        chunks[1..last]
            .iter()
            .map(|c| c.trim_matches([' ', '\t']))
            .filter(|c| !c.is_empty()),
    );
    parts.push(chunks[last].trim_start_matches([' ', '\t']));
    std::borrow::Cow::Owned(parts.join(" "))
}

/// Append a text run, coalescing into the previous run when the formatting
/// matches (docling's `_append_run`, which normalizes the run's XML line
/// breaks first).
fn append_run(segments: &mut Vec<Seg>, text: &str, fmt: Fmt, hyperlink: Option<&str>) {
    let text = xml_line_breaks(text);
    let text = text.as_ref();
    if text.is_empty() {
        return;
    }
    if let Some(last) = segments.last_mut() {
        if !last.formula && last.fmt == fmt && last.hyperlink.as_deref() == hyperlink {
            last.text.push_str(text);
            return;
        }
    }
    segments.push(Seg {
        formula: false,
        text: text.to_string(),
        fmt,
        hyperlink: hyperlink.map(str::to_string),
    });
}

/// Extend `segments` with `more`, coalescing adjacent equal-format text runs
/// (docling's `_extend_segments`).
fn extend_segments(segments: &mut Vec<Seg>, more: Vec<Seg>) {
    for seg in more {
        if seg.formula {
            segments.push(seg);
        } else {
            append_run(segments, &seg.text, seg.fmt, seg.hyperlink.as_deref());
        }
    }
}

/// docling's `_strip_segments`: each run trimmed, the blank ones dropped.
fn strip_segments(segments: Vec<Seg>) -> Vec<Seg> {
    segments
        .into_iter()
        .filter_map(|s| {
            let text = s.text.trim().to_string();
            (!text.is_empty()).then_some(Seg { text, ..s })
        })
        .collect()
}

/// Emit inline runs as a paragraph, dropping blank runs and wrapping several
/// runs in an inline group (docling's `_emit_inline` / `_strip_segments`). A
/// formula run renders as `$…$` in Markdown and a `<formula>` in DocLang; a
/// styled run carries its emphasis into DocLang while Markdown keeps the
/// baked-in `*…*`/`**…**` markers.
fn emit_inline(out: &mut Out, parent: Option<usize>, segments: Vec<Seg>) {
    let stripped = strip_segments(segments);
    if stripped.is_empty() {
        return;
    }
    // Markdown joins the runs with a single space (docling's inline-group
    // serialization); each run carries its own emphasis / `$…$` markers.
    let md_text = stripped
        .iter()
        .map(seg_markdown)
        .collect::<Vec<_>>()
        .join(" ");
    let runs = stripped
        .iter()
        .map(|s| {
            if s.formula {
                InlineRun {
                    text: s.text.clone(),
                    formula: true,
                    ..InlineRun::default()
                }
            } else {
                s.fmt.to_inline_run(&s.text)
            }
        })
        .collect();
    // JATS inline groups serialize unwrapped in DocLang (docling adds them
    // directly under the section/body, not inside a `<text>` wrapper).
    out.doc.push(inline_paragraph_node(md_text, runs, true));
    // Tree: one item under `parent`, or an `inline` group of one item per run
    // (docling's `_emit_inline`); a formula run is a `formula` item.
    let container = if stripped.len() > 1 {
        Some(out.group(parent, "inline", "group"))
    } else {
        parent
    };
    for seg in &stripped {
        let label = if seg.formula { "formula" } else { "text" };
        out.text(
            container,
            label,
            &seg.text,
            tree_formatting(seg.fmt),
            seg.hyperlink.clone(),
            None,
            None,
        );
    }
}

/// One run's Markdown: `$formula$`, or the escaped text wrapped in its emphasis
/// markers (matching docling's serializer).
fn seg_markdown(s: &Seg) -> String {
    if s.formula {
        return format!("${}$", s.text);
    }
    let mut out = escape_text(&s.text);
    if s.fmt.bold {
        out = format!("**{out}**");
    }
    if s.fmt.italic {
        out = format!("*{out}*");
    }
    if s.fmt.strike {
        out = format!("~~{out}~~");
    }
    if let Some(url) = &s.hyperlink {
        out = docling_core::hyperlink::markdown_link(&out, url);
    }
    out
}

/// A `<list-item>` at `level` — docling's `list-item` branch of
/// `_walk_linear`. The item's text joins its `<p>` children (each
/// `_get_text(p).strip()`), so a `<label>`, loose text or other block content
/// stays out of it; each nested `<list>` emits its items one level deeper
/// (docling PR #3619) under a new `list` group hung off the item. Since
/// docling#4770 the item is `enumerated` when its own list's `list-type`
/// numbers the items ([`ORDERED_LIST_TYPES`]; a nested list goes by its own
/// attribute) and its `<label>` is the item `marker` instead of text.
///
/// The flat Markdown node is shaped so the serializer prints what
/// docling-core's list serializer does with that marker
/// ([`markdown_shape`]); the DocLang overlay keeps the clean
/// marker/text split and the list kind, which is what DocLang shows.
fn add_list_item(out: &mut Out, parent: Option<usize>, item: XmlNode, level: u8) {
    let list = item.parent_element();
    let enumerated = list.is_some_and(is_enumerated_list);
    // docling numbers an enumerated item by its position in the list group.
    let number = list.map_or(1, |l| {
        l.children()
            .filter(|c| c.has_tag_name("list-item"))
            .position(|c| c == item)
            .unwrap_or(0) as u64
            + 1
    });
    let label = item_label(item);
    let mut text = String::new();
    for p in item.children().filter(|c| c.has_tag_name("p")) {
        let t = norm_text(p);
        if t.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&t);
    }
    if !text.is_empty() || !label.is_empty() {
        let shape = markdown_shape(enumerated, &label, &text);
        out.doc.push(Node::ListItem {
            ordered: shape.ordered,
            number,
            first_in_list: number == 1,
            text: escape_text(&shape.text),
            level,
            marker: shape.marker,
            location: None,
            dclx: shape.dclx.then(|| ListItemDclx {
                ordered: enumerated,
                marker: (!label.is_empty()).then(|| label.clone()),
                text: escape_text(&text),
                runs: Vec::new(),
            }),
            href: None,
            layer: None,
        });
    }
    // docling adds the item whatever its text (an empty one too).
    let item_id = Some(out.list_item_meta(
        parent,
        &text,
        ListMeta {
            enumerated,
            marker: label,
        },
    ));
    for nested in item.children().filter(|c| c.has_tag_name("list")) {
        let group = Some(out.group(item_id, "list", "list"));
        for sub in nested.children().filter(|c| c.has_tag_name("list-item")) {
            add_list_item(out, group, sub, level.saturating_add(1));
        }
    }
}

/// Whether a `<list>` numbers its items: its `list-type`, trimmed and
/// lower-cased, is one of [`ORDERED_LIST_TYPES`] (docling#4770).
fn is_enumerated_list(list: XmlNode) -> bool {
    list.attribute("list-type")
        .is_some_and(|t| ORDERED_LIST_TYPES.contains(&t.trim().to_lowercase().as_str()))
}

/// docling's `_get_label`: the text of the item's `<label>` child, stripped
/// (`""` without one).
fn item_label(item: XmlNode) -> String {
    item.children()
        .find(|c| c.has_tag_name("label"))
        .map(|l| get_text(l).trim().to_string())
        .unwrap_or_default()
}

/// The flat Markdown fields of a list item.
struct MarkdownShape {
    ordered: bool,
    marker: Option<String>,
    /// The item text the Markdown line carries after the serializer's own
    /// `-` / `N.` (the label joined in front when docling prints it there).
    text: String,
    /// Whether DocLang needs the [`ListItemDclx`] overlay: the flat fields
    /// above differ from the item's real list kind / marker / text.
    dclx: bool,
}

/// docling-core's Markdown list-item rule (default
/// `orig_list_item_marker_mode=AUTO`, `ensure_valid_list_item_marker=True`)
/// for a JATS item, mapped onto the flat fields the shared serializer reads
/// (it prints `N.` / a verbatim `\d+.` marker for an ordered item and `-`
/// otherwise):
///
/// * no label — `N. text` when enumerated, else `- text`;
/// * a valid label (`\d+.`) — printed verbatim, `1. text`, whatever the list;
/// * a label with a letter or digit (`(a)`, `ii`) — `- (a) text`, enumerated
///   or not;
/// * any other label (`•`) — dropped: `- text`.
///
/// docling-core also keeps a label of `*` / `+` verbatim as the bullet; the
/// serializer always bullets with `-`, so such an item reads `- text` here —
/// a cosmetic difference for a label JATS does not use in practice.
fn markdown_shape(enumerated: bool, label: &str, text: &str) -> MarkdownShape {
    let digits_dot = label
        .strip_suffix('.')
        .is_some_and(|d| !d.is_empty() && d.chars().all(char::is_numeric));
    if label.is_empty() {
        MarkdownShape {
            ordered: enumerated,
            marker: None,
            text: text.to_string(),
            dclx: false,
        }
    } else if digits_dot {
        MarkdownShape {
            ordered: true,
            marker: Some(label.to_string()),
            text: text.to_string(),
            dclx: !enumerated,
        }
    } else if label.chars().any(char::is_alphanumeric) {
        MarkdownShape {
            ordered: false,
            marker: None,
            text: format!("{label} {text}"),
            dclx: true,
        }
    } else {
        MarkdownShape {
            ordered: false,
            marker: None,
            text: text.to_string(),
            dclx: true,
        }
    }
}

/// A `<disp-formula>`'s `<tex-math>` child (`…$$formula$$…`) → a `$$…$$` block.
/// A block `<tex-math>` → a formula item holding the LaTeX body (docling's
/// `_add_equation`: `add_text(label=FORMULA, text=formula)`). Emitted as
/// [`Node::Formula`] so Markdown prints `$$…$$` verbatim — a multi-line body
/// keeps its newlines, the GFM hard-line-break rule applies to text items only.
fn add_equation(out: &mut Out, parent: Option<usize>, node: XmlNode) {
    if let Some(formula) = extract_tex_math(node) {
        out.doc.push(Node::Formula {
            orig: formula.clone(),
            latex: formula.clone(),
            location: None,
        });
        out.text(parent, "formula", &formula, None, None, None, None);
    }
}

/// A `<fig>` → its label + caption as a picture caption, then a picture marker
/// — carrying the figure's image when `fig_base` allows reading it (#392). In
/// the tree the caption is a body-level `caption` item (docling's
/// `add_text(label=CAPTION)` with no parent) the picture references.
fn add_figure(out: &mut Out, parent: Option<usize>, node: XmlNode, fig_base: Option<&Path>) {
    let label = node
        .children()
        .find(|c| c.has_tag_name("label"))
        .map(|l| get_text(l).trim().to_string())
        .unwrap_or_default();
    let caption = node
        .children()
        .find(|c| c.has_tag_name("caption"))
        .map(caption_text)
        .unwrap_or_default();
    let sep = if !label.is_empty() && !caption.is_empty() {
        " "
    } else {
        ""
    };
    let fig_text = format!("{label}{sep}{caption}");
    let image = fig_base.and_then(|base| load_figure_image(node, base));
    out.doc.push(Node::Picture {
        caption: (!fig_text.is_empty()).then(|| escape_text(&fig_text)),
        caption_href: None,
        image: image.clone(),
        classification: None,
        description: None,
        caption_parent: Default::default(),
        caption_location: None,
    });
    let captions = if fig_text.is_empty() {
        Vec::new()
    } else {
        vec![out.text(None, "caption", &fig_text, None, None, None, None)]
    };
    out.tree.add(
        parent,
        None,
        TreeKind::Picture {
            captions,
            image,
            extra_classes: Vec::new(),
            classification: None,
            description: None,
            confidence: None,
            chart: None,
            dpi: None,
        },
    );
}

/// The raster suffixes docling probes for an extensionless `xlink:href`
/// (`_RASTER_IMAGE_SUFFIXES`), in its order.
const RASTER_IMAGE_SUFFIXES: [&str; 6] = [".jpg", ".jpeg", ".png", ".tif", ".tiff", ".gif"];

/// docling's `_load_figure_image` (docling#4041): the first `<graphic>` of the
/// figure — direct or inside `<alternatives>`, in document order — whose
/// `xlink:href` names a readable, decodable raster under `base`.
///
/// Per rendition: a non-local href (a URL) is skipped silently; an absolute
/// path is refused with a warning but does not stop a later relative one; an
/// `.svg` is skipped; an extensionless href is probed with the raster
/// suffixes; a candidate that resolves outside `base` aborts the **whole
/// figure** (path traversal); a file that exists but does not decode warns
/// and falls through to the next rendition. Existence is checked before
/// decoding, so probe misses do not warn per suffix; a figure that resolved
/// no file at all warns once, naming every href that missed.
fn load_figure_image(fig: XmlNode, base: &Path) -> Option<PictureImage> {
    let graphics = fig.children().filter(XmlNode::is_element).flat_map(|c| {
        let own = std::iter::once(c).filter(|c| c.has_tag_name("graphic"));
        let alternatives = c
            .children()
            .filter(move |g| c.has_tag_name("alternatives") && g.has_tag_name("graphic"));
        own.chain(alternatives)
    });
    let mut missing: Vec<String> = Vec::new();
    for graphic in graphics {
        let Some(href) = graphic
            .attributes()
            .find(|a| a.name() == "href")
            .map(|a| a.value().trim())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        if !is_local_path(href) {
            continue;
        }
        // An absolute rendition is invalid for a confined local base, but it
        // must not prevent a later relative rendition from being used.
        if is_absolute_path(href) {
            eprintln!(
                "docling: warning: Could not process an image from {href}: \
                 Absolute paths are not allowed with local base_path."
            );
            continue;
        }
        let suffix = Path::new(href)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if suffix.as_deref() == Some("svg") {
            continue;
        }
        let mut candidates = vec![href.to_string()];
        if suffix.is_none() {
            candidates.extend(RASTER_IMAGE_SUFFIXES.iter().map(|s| format!("{href}{s}")));
        }
        let mut found = false;
        for candidate in &candidates {
            let Some(resolved) = confined_path(base, candidate) else {
                eprintln!(
                    "docling: warning: Could not process an image from {href}: \
                     Path traversal blocked: '{candidate}' resolves outside base directory"
                );
                return None;
            };
            if !resolved.is_file() {
                continue;
            }
            found = true;
            let decoded = std::fs::read(&resolved)
                .ok()
                .and_then(|data| crate::backend::ooxml::picture_image(candidate, data));
            match decoded {
                Some(image) => return Some(image),
                None => eprintln!(
                    "docling: warning: Could not process an image from {}: \
                     cannot identify image file",
                    resolved.display()
                ),
            }
        }
        if !found {
            missing.push(href.to_string());
        }
    }
    if !missing.is_empty() {
        eprintln!(
            "docling: warning: Could not process JATS figure image(s) {}: \
             no matching local file exists.",
            missing.join(", ")
        );
    }
    None
}

/// docling's `ImageResourceLoader.is_local_path`: no host, and either no
/// scheme or a one-letter one (a Windows drive).
fn is_local_path(value: &str) -> bool {
    let Some(colon) = value.find(':') else {
        return !value.starts_with("//");
    };
    let scheme = &value[..colon];
    let is_scheme = scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !is_scheme {
        return !value.starts_with("//");
    }
    let rest = &value[colon + 1..];
    !rest.starts_with("//") && scheme.len() == 1
}

/// docling's `ImageResourceLoader.is_absolute_path`: a rooted path, or a
/// Windows drive spelling (`C:…`).
fn is_absolute_path(value: &str) -> bool {
    // `has_root`, not `is_absolute`: the href was written into the XML on
    // any OS, and `/abs/a.png` is docling's `os.path.isabs` — which
    // `Path::is_absolute` denies on Windows, where the path is root-relative
    // yet still replaces the base in a `join`.
    Path::new(value).has_root()
        || (value.len() >= 2
            && value.as_bytes()[1] == b':'
            && value.as_bytes()[0].is_ascii_alphabetic()
            && !value[2..].starts_with("//"))
}

/// `base / rel`, lexically normalized, when it stays under `base` —
/// docling's `resolve_relative_path` traversal guard (`Path.resolve()` +
/// `is_relative_to`). `None` when a `..` climbs out.
fn confined_path(base: &Path, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = base.to_path_buf();
    let mut depth = 0usize;
    for comp in Path::new(rel).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                out.pop();
                depth -= 1;
            }
            Component::Normal(seg) => {
                out.push(seg);
                depth += 1;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// A `<caption>`'s paragraphs, space-joined and trimmed (skipping any that hold
/// supplementary material) — docling's caption assembly.
fn caption_text(caption: XmlNode) -> String {
    let mut out = String::new();
    for par in caption.children().filter(XmlNode::is_element) {
        if par
            .descendants()
            .any(|d| d.has_tag_name("supplementary-material"))
        {
            continue;
        }
        out.push_str(get_text(par).trim());
        out.push(' ');
    }
    out.trim().to_string()
}

/// A `<table-wrap>` → an optional caption paragraph followed by the table grid.
/// In the tree the label+caption is a body-level `caption` item (created
/// before the table, whether or not the table parses — docling's order) and
/// the table hangs off `parent` referencing it.
fn add_table(out: &mut Out, parent: Option<usize>, node: XmlNode) {
    let content = node
        .children()
        .find(|c| c.has_tag_name("table"))
        .or_else(|| {
            node.children()
                .find(|c| c.has_tag_name("alternatives"))
                .and_then(|a| a.children().find(|c| c.has_tag_name("table")))
        });
    let label = node
        .children()
        .find(|c| c.has_tag_name("label"))
        .and_then(|l| l.text())
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    let caption = node
        .children()
        .find(|c| c.has_tag_name("caption"))
        .map(caption_text)
        .unwrap_or_default();
    let sep = if !label.is_empty() && !caption.is_empty() {
        " "
    } else {
        ""
    };
    let cap_text = format!("{label}{sep}{caption}");
    let Some(table_node) = content else { return };
    let captions = if cap_text.is_empty() {
        Vec::new()
    } else {
        vec![out.text(None, "caption", &cap_text, None, None, None, None)]
    };
    let Some(mut table) = parse_jats_table(table_node) else {
        return;
    };
    out.tree.add(
        parent,
        None,
        TreeKind::Table {
            table: table.clone(),
            rich_cells: Vec::new(),
            captions,
        },
    );
    // The label+caption becomes the table's own caption (docling attaches it to
    // the `TableItem` rather than emitting a standalone paragraph before it).
    // Stored escaped, matching the backend's text-node convention.
    table.caption = (!cap_text.is_empty()).then(|| escape_text(&cap_text));
    out.doc.push(Node::Table(table));
}

/// Parse a JATS/XHTML `<table>` into a row-major grid, expanding `colspan`
/// (duplicated across columns) and `rowspan` (filled down). Header rows (`<th>`
/// or `<thead>`) come first, matching docling's `parse_table_data` layout.
fn parse_jats_table(table: XmlNode) -> Option<Table> {
    // A nested table is unsupported (docling bails on `element.find("table")`).
    let rows_nodes: Vec<XmlNode> = table
        .descendants()
        .filter(|n| n.has_tag_name("tr"))
        .collect();
    if rows_nodes.iter().any(|r| {
        r.descendants()
            .any(|d| d.has_tag_name("table") && d != table)
    }) {
        return None;
    }

    // Number of columns = the widest row (accounting for colspans).
    let num_cols = rows_nodes
        .iter()
        .map(|r| {
            r.children()
                .filter(|c| c.has_tag_name("td") || c.has_tag_name("th"))
                .map(col_span)
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    if rows_nodes.is_empty() || num_cols == 0 {
        return None;
    }

    let nrows = rows_nodes.len();
    let mut grid: Vec<Vec<String>> = vec![vec![String::new(); num_cols]; nrows];
    // Track which cells are already occupied by a rowspan from above.
    let mut filled: Vec<Vec<bool>> = vec![vec![false; num_cols]; nrows];
    // OTSL structure overlay (docling's cell classification): `<th>`/`<thead>`
    // cells are column headers, and a span's continuation columns/rows are
    // emitted as `<lcel/>`/`<ucel/>` in DocLang instead of the text-replicated
    // cells Markdown/JSON still use.
    let mut col_header: Vec<Vec<bool>> = vec![vec![false; num_cols]; nrows];
    let mut col_continuation: Vec<Vec<bool>> = vec![vec![false; num_cols]; nrows];
    let mut row_continuation: Vec<Vec<bool>> = vec![vec![false; num_cols]; nrows];
    for (ri, row) in rows_nodes.iter().enumerate() {
        let mut ci = 0usize;
        for cell in row
            .children()
            .filter(|c| c.has_tag_name("td") || c.has_tag_name("th"))
        {
            while ci < num_cols && filled[ri][ci] {
                ci += 1;
            }
            if ci >= num_cols {
                break;
            }
            let cs = col_span(cell);
            let rs = row_span(cell);
            let text = normalize(&get_text(cell));
            // A cell is a column header when it is a `<th>` or sits in a
            // `<thead>` (docling's `column_header` flag).
            let header =
                cell.has_tag_name("th") || cell.ancestors().any(|a| a.has_tag_name("thead"));
            for r in ri..(ri + rs).min(nrows) {
                for c in ci..(ci + cs).min(num_cols) {
                    grid[r][c] = text.clone();
                    filled[r][c] = true;
                    // The primary cell carries the text + header flag; the
                    // cells it spans into are span continuations.
                    if c > ci {
                        col_continuation[r][c] = true;
                    }
                    if r > ri {
                        row_continuation[r][c] = true;
                    }
                }
            }
            col_header[ri][ci] = header;
            ci += cs;
        }
    }
    let has_header = col_header.iter().flatten().any(|&h| h);
    let has_span = col_continuation
        .iter()
        .chain(&row_continuation)
        .flatten()
        .any(|&s| s);
    let structure = (has_header || has_span).then(|| TableStructure {
        header_row: Vec::new(),
        col_continuation,
        row_continuation,
        row_header: Vec::new(),
        col_header,
    });
    Some(Table {
        rows: grid,
        location: None,
        structure,
        cell_blocks: None,
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    })
}

fn col_span(cell: XmlNode) -> usize {
    cell.attribute("colspan")
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(1)
}

fn row_span(cell: XmlNode) -> usize {
    cell.attribute("rowspan")
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n >= 1)
        .unwrap_or(1)
}

/// A `<fn-group>` → a "Footnotes" heading and a bullet list of its `<fn>` texts.
fn add_footnote_group(out: &mut Out, parent: Option<usize>, node: XmlNode, hlevel: i32) {
    // Each `<fn>`'s inline runs, its `<label>` and styling kept (docling#4361:
    // `_walk_inline_formula` + `_strip_segments`).
    let footnotes: Vec<Vec<Seg>> = node
        .children()
        .filter(|c| c.has_tag_name("fn"))
        .map(|f| strip_segments(walk_inline_formula(f, Fmt::default(), None)))
        .filter(|segs| !segs.is_empty())
        .collect();
    if footnotes.is_empty() {
        return;
    }
    let title = node
        .children()
        .find(|c| c.has_tag_name("title"))
        .map(|t| normalize(&get_text(t)))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_HEADER_FOOTNOTES.to_string());
    out.doc.push(Node::Heading {
        level: fw_level(hlevel + 1),
        text: escape_text(&title),
    });
    // Tree (docling's `_add_footnote_group`): the heading, a `footnotes`
    // list group under it, and per note an empty list item holding its runs
    // — one `footnote` item, or an inline group of them (formulas stay
    // `formula` items).
    let heading = Some(out.heading(parent, &title, hlevel + 1));
    let group = Some(out.group(heading, "list", "footnotes"));
    for segs in footnotes {
        let li = Some(out.list_item(group, ""));
        let container = if segs.len() > 1 {
            Some(out.group(li, "inline", "group"))
        } else {
            li
        };
        for seg in &segs {
            let label = if seg.formula { "formula" } else { "footnote" };
            out.text(
                container,
                label,
                &seg.text,
                tree_formatting(seg.fmt),
                seg.hyperlink.clone(),
                None,
                None,
            );
        }
        let text = segs.iter().map(seg_markdown).collect::<Vec<_>>().join(" ");
        out.doc.push(Node::ListItem {
            ordered: false,
            number: 0,
            first_in_list: false,
            text,
            level: 0,
            marker: None,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
    }
}

/// Flatten an `<element-citation>` to a single reference string — a port of
/// docling's `_parse_element_citation`.
fn parse_element_citation(node: XmlNode) -> String {
    // Author names ("surname given-names"), plus a trailing "et al." if present.
    let mut names: Vec<String> = Vec::new();
    for name in node.descendants().filter(|n| n.has_tag_name("name")) {
        let surname = name
            .children()
            .find(|c| c.has_tag_name("surname"))
            .and_then(|c| c.text())
            .map(|t| t.replace('\n', " "))
            .map(|t| t.trim().to_string());
        let given = name
            .children()
            .find(|c| c.has_tag_name("given-names"))
            .and_then(|c| c.text())
            .map(|t| t.replace('\n', " "))
            .map(|t| t.trim().to_string());
        // A partial name — only a surname, or only given names — is kept
        // (docling#4272's `_parse_structured_name(order=("surname",
        // "given-names"))`: the present parts, space-joined).
        let name_str = [surname, given]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !name_str.is_empty() {
            names.push(name_str);
        }
    }
    if let Some(etal) = node.descendants().find(|n| n.has_tag_name("etal")) {
        let etal_text = etal
            .text()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_TEXT_ETAL);
        names.push(etal_text.to_string());
    }
    let author_names = names.join(", ");

    // Title (the first of several possible tags).
    let title = [
        "article-title",
        "chapter-title",
        "data-title",
        "issue-title",
        "part-title",
        "trans-title",
    ]
    .iter()
    .find_map(|t| node.children().find(|c| c.has_tag_name(*t)))
    .map(get_text)
    .unwrap_or_else(|| {
        node.text()
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default()
    });

    let field = |name: &str| -> String {
        node.children()
            .find(|c| c.has_tag_name(name))
            .and_then(|c| c.text())
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default()
    };
    let source = field("source");
    let year = field("year");
    let publisher_name = field("publisher-name");
    let publisher_loc = field("publisher-loc");
    let volume = field("volume");

    // Publication identifiers (DOI/PMID/…).
    let mut pub_ids: Vec<String> = Vec::new();
    for id in node.children().filter(|c| c.has_tag_name("pub-id")) {
        let id_type = id
            .attribute("assigning-authority")
            .or_else(|| id.attribute("pub-id-type"));
        if let (Some(t), Some(text)) = (id_type, id.text()) {
            pub_ids.push(format!(
                "{}: {}",
                t.replace('\n', " ").trim().to_uppercase(),
                text.replace('\n', " ").trim()
            ));
        }
    }
    let pub_id = pub_ids.join(", ");

    // Pages: an elocation-id, or an fpage(–lpage) range.
    let page = if let Some(e) = node.children().find(|c| c.has_tag_name("elocation-id")) {
        e.text()
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default()
    } else if let Some(f) = node.children().find(|c| c.has_tag_name("fpage")) {
        let mut p = f
            .text()
            .map(|t| t.replace('\n', " ").trim().to_string())
            .unwrap_or_default();
        // An empty `<lpage/>` adds no dash (docling#4272).
        if let Some(l) = node
            .children()
            .find(|c| c.has_tag_name("lpage"))
            .and_then(|l| l.text())
            .map(|t| t.replace('\n', " ").trim().to_string())
            .filter(|l| !l.is_empty())
        {
            p.push('\u{2013}');
            p.push_str(&l);
        }
        p
    } else {
        String::new()
    };

    // Assemble, mirroring docling's rstrip-and-append sequence.
    let mut text = String::new();
    if !author_names.is_empty() {
        text.push_str(author_names.trim_end_matches('.'));
        text.push_str(". ");
    }
    if !title.is_empty() {
        text.push_str(title.trim());
        text.push_str(". ");
    }
    if !source.is_empty() {
        text.push_str(&source);
        text.push_str(". ");
    }
    if !publisher_name.is_empty() {
        if !publisher_loc.is_empty() {
            text.push_str(&format!("{publisher_loc}: "));
        }
        text.push_str(&publisher_name);
        text.push_str(". ");
    }
    if !volume.is_empty() {
        rstrip_dot_space(&mut text);
        text.push_str(&format!(" {volume}. "));
    }
    if !page.is_empty() {
        rstrip_dot_space(&mut text);
        if !volume.is_empty() {
            text.push(':');
        }
        text.push_str(&page);
        text.push_str(". ");
    }
    if !year.is_empty() {
        rstrip_dot_space(&mut text);
        text.push_str(&format!(" ({year})."));
    }
    if !pub_id.is_empty() {
        while text.ends_with('.') {
            text.pop();
        }
        text.push_str(". ");
        text.push_str(&pub_id);
    }
    text
}

/// Python `str.rstrip(". ")`: drop any trailing run of `.` and space characters.
fn rstrip_dot_space(s: &mut String) {
    while matches!(s.chars().last(), Some('.') | Some(' ')) {
        s.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::images::ImageSources;
    use crate::format::InputFormat;

    /// A JATS article whose body is `body`, converted — docling's
    /// `convert_jats_body` test helper.
    fn body_doc(body: &str) -> DoclingDocument {
        let xml = format!(
            "<article><front><article-meta><title-group><article-title>Body Test\
             </article-title></title-group></article-meta></front><body>{body}</body></article>"
        );
        let src = SourceDocument::from_bytes("b", InputFormat::XmlJats, xml.into_bytes());
        JatsBackend::default().convert(&src).unwrap()
    }

    /// The `(text, enumerated, marker)` of every list item in the JSON export.
    fn json_list_items(doc: &DoclingDocument) -> Vec<(String, bool, String)> {
        let v: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        v["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["label"] == "list_item")
            .map(|t| {
                (
                    t["text"].as_str().unwrap().to_string(),
                    t["enumerated"].as_bool().unwrap(),
                    t["marker"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn two_items(list_attr: &str) -> String {
        format!(
            "<sec><title>Steps</title><list{list_attr}>\
             <list-item><p>first</p></list-item><list-item><p>second</p></list-item>\
             </list></sec>"
        )
    }

    /// docling#4770 `test_jats_list_type_sets_enumerated`: the numbering
    /// `list-type`s (trimmed, any case) enumerate the items; `bullet`,
    /// `simple`, `none`, an unknown value or none at all bullet them.
    #[test]
    fn list_type_sets_enumerated() {
        let cases = [
            (Some("order"), true),
            (Some("alpha-lower"), true),
            (Some("alpha-upper"), true),
            (Some("roman-lower"), true),
            (Some("roman-upper"), true),
            (Some(" Order "), true),
            (Some("ROMAN-UPPER"), true),
            (Some("bullet"), false),
            (Some("simple"), false),
            (Some("none"), false),
            (Some("bogus"), false),
            (None, false),
        ];
        for (list_type, enumerated) in cases {
            let attr = list_type.map_or(String::new(), |t| format!(" list-type=\"{t}\""));
            let doc = body_doc(&two_items(&attr));
            assert_eq!(
                json_list_items(&doc),
                vec![
                    ("first".to_string(), enumerated, String::new()),
                    ("second".to_string(), enumerated, String::new()),
                ],
                "{list_type:?}"
            );
            let want = if enumerated {
                "1. first\n2. second"
            } else {
                "- first\n- second"
            };
            let md = doc.export_to_markdown();
            assert!(md.contains(want), "{list_type:?}: {md}");
            let dclx = doc.export_to_doclang();
            assert_eq!(
                dclx.contains("<list class=\"ordered\">"),
                enumerated,
                "{list_type:?}: {dclx}"
            );
        }
    }

    /// docling#4770 `test_jats_nested_list_takes_its_own_list_type`, and the
    /// converse: a nested list without `list-type` is a bullet list inside a
    /// numbered one.
    #[test]
    fn nested_list_takes_its_own_list_type() {
        let doc = body_doc(
            "<sec><title>Steps</title><list list-type=\"bullet\"><list-item><p>outer</p>\
             <list list-type=\"order\"><list-item><p>inner one</p></list-item>\
             <list-item><p>inner two</p></list-item></list></list-item></list></sec>",
        );
        let enumerated: Vec<(String, bool)> = json_list_items(&doc)
            .into_iter()
            .map(|(t, e, _)| (t, e))
            .collect();
        assert_eq!(
            enumerated,
            vec![
                ("outer".into(), false),
                ("inner one".into(), true),
                ("inner two".into(), true),
            ]
        );
        assert!(
            doc.export_to_markdown()
                .contains("- outer\n    1. inner one\n    2. inner two"),
            "{}",
            doc.export_to_markdown()
        );

        let doc = body_doc(
            "<sec><title>Steps</title><list list-type=\"order\"><list-item><p>outer</p>\
             <list><list-item><p>inner one</p></list-item><list-item><p>inner two</p></list-item>\
             </list></list-item><list-item><p>outer two</p></list-item></list></sec>",
        );
        assert!(
            doc.export_to_markdown()
                .contains("1. outer\n    - inner one\n    - inner two\n2. outer two"),
            "{}",
            doc.export_to_markdown()
        );
    }

    /// docling#4770 `test_jats_list_item_label_becomes_marker`: the `<label>`
    /// is the item's marker, not part of its text. docling-core prints an
    /// alphanumeric marker behind a `-` bullet, and DocLang carries it in the
    /// item's `<ldiv>`.
    #[test]
    fn list_item_label_becomes_marker() {
        let doc = body_doc(
            "<sec><title>Conditions</title><list list-type=\"simple\">\
             <list-item><label>(a)</label><p>first</p></list-item>\
             <list-item><label>(b)</label><p>second</p></list-item></list></sec>",
        );
        assert_eq!(
            json_list_items(&doc),
            vec![
                ("first".to_string(), false, "(a)".to_string()),
                ("second".to_string(), false, "(b)".to_string()),
            ]
        );
        assert!(
            doc.export_to_markdown()
                .contains("- (a) first\n- (b) second"),
            "{}",
            doc.export_to_markdown()
        );
        let dclx = doc.export_to_doclang();
        assert!(
            dclx.contains(
                "<list>\n    <ldiv>\n      <marker>(a)</marker>\n    </ldiv>\n    first\n"
            ),
            "{dclx}"
        );
    }

    /// docling-core's marker rule in a numbered list (checked against docling
    /// 2.138.0): a `\\d+.` label prints verbatim, any other alphanumeric label
    /// falls back to `- label`, and an unlabelled item keeps its position
    /// number. DocLang keeps the list ordered with each label as its marker.
    #[test]
    fn labels_in_a_numbered_list() {
        let doc = body_doc(
            "<sec><title>C</title><list list-type=\"order\">\
             <list-item><label>1.</label><p>first</p></list-item>\
             <list-item><label>(ii)</label><p>second</p></list-item>\
             <list-item><p>third</p></list-item></list></sec>",
        );
        assert_eq!(
            json_list_items(&doc),
            vec![
                ("first".to_string(), true, "1.".to_string()),
                ("second".to_string(), true, "(ii)".to_string()),
                ("third".to_string(), true, String::new()),
            ]
        );
        let md = doc.export_to_markdown();
        assert!(md.contains("1. first\n- (ii) second\n3. third"), "{md}");
        let dclx = doc.export_to_doclang();
        assert!(
            dclx.contains(
                "<list class=\"ordered\">\n    <ldiv>\n      <marker>1.</marker>\n    </ldiv>\n    first\n    <ldiv>\n      <marker>(ii)</marker>\n    </ldiv>\n    second\n    <ldiv/>\n    third\n  </list>"
            ),
            "{dclx}"
        );
    }

    /// The item text is its `<p>` children only (docling's `text_parts`):
    /// loose text beside them is not part of it, and a label-only item is an
    /// empty item carrying its marker (`- x ` in docling's Markdown).
    #[test]
    fn list_item_text_is_its_paragraphs() {
        let doc = body_doc(
            "<sec><title>C</title><list list-type=\"bullet\">\
             <list-item>loose text<p>para one</p><p>para two</p></list-item>\
             <list-item><label>x</label></list-item></list></sec>",
        );
        assert_eq!(
            json_list_items(&doc),
            vec![
                ("para one para two".to_string(), false, String::new()),
                (String::new(), false, "x".to_string()),
            ]
        );
        let md = doc.export_to_markdown();
        assert!(md.contains("- para one para two\n- x \n"), "{md:?}");
    }

    /// docling#4029: `<ext-link xlink:href>` makes its runs hyperlinks —
    /// rendered `[text](url)` and never coalesced with the unlinked text
    /// around them; a no-break space in a citation survives; a block
    /// `<tex-math>` is a formula item whose newlines stay unmarked.
    #[test]
    fn ext_links_nbsp_and_display_formulas() {
        let xml = r#"<article xmlns:xlink="http://www.w3.org/1999/xlink"><front><article-meta>
            <title-group><article-title>T</article-title></title-group>
          </article-meta></front>
          <body><sec><title>S</title>
            <p>See RRID: <ext-link ext-link-type="uri" xlink:href="https://scicrunch.org/resolver/AB_1">AB_1</ext-link> here.</p>
            <p>Plain <ext-link xlink:href="  ">blank</ext-link> link.</p>
            <disp-formula><tex-math><![CDATA[$$\begin{eqnarray}
a=b
\end{eqnarray}$$]]></tex-math></disp-formula>
          </sec></body>
          <back><ref-list><title>References</title>
            <ref><mixed-citation>A. S. de&#xa0;Castro, Phys. Lett. A. 346 (2005).</mixed-citation></ref>
          </ref-list></back></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let doc = JatsBackend::default().convert(&src).unwrap();
        let md = doc.export_to_markdown();
        assert!(
            md.contains("See RRID: [AB\\_1](https://scicrunch.org/resolver/AB_1) here."),
            "{md}"
        );
        assert!(
            md.contains("Plain blank link."),
            "blank href → no link: {md}"
        );
        assert!(
            md.contains("$$\\begin{eqnarray}\na=b\n\\end{eqnarray}$$"),
            "{md}"
        );
        assert!(md.contains("A. S. de\u{a0}Castro"), "{md}");
        assert!(doc
            .nodes
            .iter()
            .any(|n| matches!(n, Node::Formula { latex, .. } if latex.starts_with("\\begin"))));
    }

    #[test]
    fn metadata_and_sections() {
        let xml = r#"<article><front><article-meta>
            <title-group><article-title>My Paper</article-title></title-group>
            <contrib-group>
              <contrib contrib-type="author"><name><surname>Doe</surname><given-names>Jane</given-names></name>
                <xref ref-type="aff" rid="a1"/></contrib>
            </contrib-group>
            <aff id="a1"><label>1</label>Acme &amp; Co</aff>
            <abstract><p>Short summary.</p></abstract>
          </article-meta></front>
          <body><sec><title>Intro</title><p>Body text.</p></sec></body></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let md = JatsBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        // title #, author, label-stripped + escaped affiliation, ## Abstract, ## Intro
        assert!(md.starts_with("# My Paper\n\nJane Doe\n\nAcme &amp; Co\n\n## Abstract\n\nShort summary.\n\n## Intro\n\nBody text."), "got:\n{md}");
    }

    fn json_of(xml: &str) -> serde_json::Value {
        let src = SourceDocument::from_bytes("t", InputFormat::XmlJats, xml.as_bytes().to_vec());
        serde_json::from_str(
            &JatsBackend::default()
                .convert(&src)
                .unwrap()
                .export_to_json(),
        )
        .unwrap()
    }

    /// docling's item tree: the title is the root; authors and the abstract
    /// heading hang off it; a `<sec>` heading parents its paragraphs; a
    /// paragraph of several runs is an `inline` group of `text` items with
    /// `formatting`; a `<list>` is a `list` group of items; a figure's caption
    /// is a body-level `caption` the picture references; footnotes are a
    /// heading over a `footnotes` list of empty items each holding an inline
    /// group with the `footnote` text; references a heading over a `list`
    /// group of citation items.
    #[test]
    fn tree_mirrors_docling_parent_threading() {
        let xml = r#"<article><front><article-meta><title-group><article-title>T</article-title></title-group>
            <contrib-group><contrib contrib-type="author"><name><surname>Doe</surname><given-names>J</given-names></name></contrib></contrib-group>
            <abstract><p>Abs.</p></abstract></article-meta></front>
            <body><sec><title>Intro</title><p>Plain <italic>it</italic> end.</p>
            <list><list-item><p>one</p></list-item><list-item><p>two</p></list-item></list>
            <fig><label>Fig 1</label><caption><p>Cap.</p></caption><graphic xlink:href="a.png" xmlns:xlink="http://www.w3.org/1999/xlink"/></fig></sec></body>
            <back><fn-group><fn><p>Note.</p></fn></fn-group>
            <ref-list><title>References</title><ref><mixed-citation>Ref one.</mixed-citation></ref></ref-list></back></article>"#;
        let v = json_of(xml);
        let texts = v["texts"].as_array().unwrap();
        let by = |i: usize| {
            (
                texts[i]["label"].as_str().unwrap(),
                texts[i]["parent"]["$ref"].as_str().unwrap(),
            )
        };
        assert_eq!(by(0), ("title", "#/body"));
        assert_eq!(by(1), ("paragraph", "#/texts/0")); // authors
        assert_eq!(by(2), ("section_header", "#/texts/0")); // Abstract, level 1
        assert_eq!(texts[2]["level"], 1);
        assert_eq!(by(3), ("text", "#/texts/2"));
        assert_eq!(by(4), ("section_header", "#/texts/0")); // Intro
                                                            // Three runs → an inline group under the section, the italic run styled.
        assert_eq!(v["groups"][0]["label"], "inline");
        assert_eq!(v["groups"][0]["parent"]["$ref"], "#/texts/4");
        assert_eq!(by(5), ("text", "#/groups/0"));
        assert_eq!(texts[6]["formatting"]["italic"], true);
        // The list: a `list` group under the section with two items.
        assert_eq!(v["groups"][1]["label"], "list");
        assert_eq!(v["groups"][1]["parent"]["$ref"], "#/texts/4");
        assert_eq!(by(8), ("list_item", "#/groups/1"));
        // The figure: caption on the body, picture under the section.
        assert_eq!(by(10), ("caption", "#/body"));
        assert_eq!(texts[10]["text"], "Fig 1 Cap.");
        assert_eq!(v["pictures"][0]["parent"]["$ref"], "#/texts/4");
        assert_eq!(v["pictures"][0]["captions"][0]["$ref"], "#/texts/10");
        // Footnotes: heading → `footnotes` list → empty item → its one
        // `footnote` run directly (docling#4361's `_emit_inline`: an inline
        // group only for several runs).
        assert_eq!(by(11), ("section_header", "#/texts/0"));
        assert_eq!(texts[11]["text"], "Footnotes");
        assert_eq!(v["groups"][2]["name"], "footnotes");
        assert_eq!(by(12), ("list_item", "#/groups/2"));
        assert_eq!(texts[12]["text"], "");
        assert_eq!(by(13), ("footnote", "#/texts/12"));
        // References: heading → `list` group → citation list item.
        assert_eq!(by(14), ("section_header", "#/texts/0"));
        assert_eq!(v["groups"][3]["label"], "list");
        assert_eq!(v["groups"][3]["parent"]["$ref"], "#/texts/14");
        assert_eq!(by(15), ("list_item", "#/groups/3"));
    }

    /// docling's `_walk_linear` sets `new_parent` once per element, so the
    /// siblings that follow a `<sec>` are parented under that section's
    /// heading too (`ptag100`'s paragraphs after its `Funding` section).
    #[test]
    fn siblings_after_a_section_inherit_its_heading() {
        let xml = r#"<article><body><sec><title>Funding</title><p>a</p></sec><p>after</p></body></article>"#;
        let v = json_of(xml);
        let texts = v["texts"].as_array().unwrap();
        assert_eq!(texts[1]["text"], "Funding");
        assert_eq!(texts[3]["text"], "after");
        assert_eq!(texts[3]["parent"]["$ref"], "#/texts/1");
    }

    fn md_of(xml: &str) -> String {
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        JatsBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown()
    }

    /// docling#4172 (#391): a structured abstract keeps its `<sec>`s as
    /// headings one level below the abstract's, one text item per `<p>`; an
    /// untitled section's paragraphs sit right under the abstract heading; a
    /// section without paragraphs is dropped; a nested `<sec>` is not walked.
    #[test]
    fn structured_abstract_keeps_its_sections() {
        let md = md_of(
            r#"<article><front><article-meta>
            <title-group><article-title>T</article-title></title-group>
            <abstract>
              <sec><title>Background</title><p>B one.</p><p>B two.</p></sec>
              <sec><p>No title here.</p></sec>
              <sec><title>Empty</title></sec>
              <sec><title>Outer</title><p>O.</p><sec><title>Inner</title><p>I.</p></sec></sec>
            </abstract>
          </article-meta></front><body/></article>"#,
        );
        assert_eq!(
            md,
            "# T

## Abstract

### Background

B one.

B two.

No title here.

### Outer

O.
"
        );
    }

    /// docling#4826: a sub-article (a decision letter, an author response)
    /// converts under a heading with its title, a nested one under the
    /// heading of the one that holds it, after the article's own body —
    /// whose lookup skips the sub-articles' bodies.
    #[test]
    fn sub_articles_are_converted() {
        let md = md_of(
            r#"<article><front><article-meta>
            <title-group><article-title>T</article-title></title-group>
          </article-meta></front><body><p>Main.</p></body>
          <sub-article><front-stub><title-group><article-title>Decision letter</article-title></title-group></front-stub>
            <body><p>Reviewed.</p></body>
            <sub-article><front-stub><title-group><article-title>Reply</article-title></title-group></front-stub>
              <body><p>Thanks.</p></body></sub-article>
          </sub-article></article>"#,
        );
        assert_eq!(
            md,
            "# T\n\nMain.\n\n## Decision letter\n\nReviewed.\n\n### Reply\n\nThanks.\n"
        );
    }

    /// An abstract made only of sections emits no plain text item; one with
    /// nothing usable is skipped; a `<label>` names it like a `<title>` does.
    #[test]
    fn abstract_label_and_skip_rules_follow_docling() {
        let md = md_of(
            r#"<article><front><article-meta>
            <title-group><article-title>T</article-title></title-group>
            <abstract><label>Summary</label><p>S.</p></abstract>
            <abstract abstract-type="graphical"><title>Graphical</title><sec><title>X</title></sec></abstract>
            <abstract><p>Plain.</p><sec><title>Also</title><p>Sectioned.</p></sec></abstract>
          </article-meta></front><body/></article>"#,
        );
        // The third abstract has sections and a plain paragraph: the plain
        // one comes first, then the sections (docling#4361 keeps both).
        assert_eq!(
            md,
            "# T

## Summary

S.

## Abstract

Plain.

### Also

Sectioned.
"
        );
    }

    /// docling's `_parse_title` joins `elem.text` of each title-group child
    /// — the text before its first child element — so inline markup cuts the
    /// title (upstream's `pmc2231364` groundtruth), a subtitle follows the
    /// title after a space, and two title-groups join with ` - `.
    #[test]
    fn title_is_the_direct_text_of_the_title_group_children() {
        let md = md_of(
            r#"<article><front><article-meta>
            <title-group><article-title>Response of <italic>Y. pestis</italic> to stress</article-title>
              <subtitle>A sub</subtitle></title-group>
          </article-meta></front><body/></article>"#,
        );
        assert!(
            md.starts_with(
                "# Response of A sub
"
            ),
            "{md}"
        );
    }

    #[test]
    fn body_tables_figures_and_references() {
        let xml = r#"<article><front><article-meta>
            <title-group><article-title>T</article-title></title-group>
          </article-meta></front>
          <body><sec><title>S</title>
            <fig><label>Fig 1</label><caption><p>A caption.</p></caption><graphic/></fig>
            <table-wrap><label>Table 1</label><caption><p>Table cap.</p></caption>
              <table><thead><tr><th>Name</th><th>N</th></tr></thead>
              <tbody><tr><td>a</td><td>1</td></tr></tbody></table></table-wrap>
          </sec></body>
          <back><ref-list><title>References</title>
            <ref><mixed-citation>Doe J. A title. 2020.</mixed-citation></ref>
          </ref-list></back></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let md = JatsBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert!(
            md.contains("Fig 1 A caption.\n\n<!-- image -->"),
            "figure:\n{md}"
        );
        assert!(md.contains("Table 1 Table cap."), "table caption:\n{md}");
        assert!(md.contains("| Name"), "table grid:\n{md}");
        assert!(md.contains("## References"), "refs heading:\n{md}");
        assert!(md.contains("- Doe J. A title. 2020."), "citation:\n{md}");
    }

    /// The table's label+caption attaches to the table (docling's
    /// `TableItem.captions`): DocLang emits it as a `<caption>` inside
    /// `<table>`, a `<thead>` cell is a `<ched/>`, and a `colspan` header's
    /// extra columns are `<lcel/>` span continuations rather than repeated text.
    #[test]
    fn table_caption_and_span_structure() {
        let xml = r#"<article><body><sec><title>S</title>
            <table-wrap><label>Table 1</label><caption><p>Cap.</p></caption>
              <table>
                <thead><tr><th colspan="2">Group</th><th>N</th></tr></thead>
                <tbody><tr><td>a</td><td>b</td><td>1</td></tr></tbody>
              </table></table-wrap>
          </sec></body></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let doc = JatsBackend::default().convert(&src).unwrap();
        let dclx = doc.export_to_doclang();
        assert!(
            dclx.contains("<table>\n    <caption>Table 1 Cap.</caption>"),
            "caption inside table:\n{dclx}"
        );
        // Header row: `Group` (ched) spanning two columns (+lcel), then `N` (ched).
        assert!(
            dclx.contains("<ched/>\n    Group\n    <lcel/>\n    <ched/>\n    N"),
            "colspan header → ched + lcel:\n{dclx}"
        );
        // Markdown keeps the caption as a line before the grid and the span text.
        let md = doc.export_to_markdown();
        assert!(md.contains("Table 1 Cap."), "md caption:\n{md}");
    }

    /// docling PR #3726: `<italic>`/`<bold>` emphasis renders as Markdown
    /// markers and an `<inline-formula>`'s `<tex-math>` stays inline as `$…$`;
    /// docling's inline-group serialization joins the runs with single spaces.
    #[test]
    fn emphasis_and_inline_formula() {
        let xml = r#"<article><body><sec><title>S</title>
            <p>We combined <italic>B</italic>. <italic>malayi</italic> with a
               <bold>strong</bold> effect and mass <inline-formula><tex-math>$m c^2$</tex-math></inline-formula> energy.</p>
          </sec></body></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let md = JatsBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert!(
            md.contains(
                "We combined *B* . *malayi* with a **strong** effect and mass $m c^2$ energy."
            ),
            "emphasis + inline formula:\n{md}"
        );
    }

    /// docling PR #3619: a nested <list> inside a <list-item> keeps its
    /// structure (deeper items) instead of flattening into the parent's text.
    #[test]
    fn nested_lists_keep_structure() {
        let xml = r#"<article><body><sec><title>S</title>
            <list>
              <list-item><p>Item 1</p>
                <list>
                  <list-item><p>Subitem A</p></list-item>
                  <list-item><p>Subitem B</p></list-item>
                </list>
              </list-item>
              <list-item><p>Item 2</p></list-item>
            </list></sec></body></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let doc = JatsBackend::default().convert(&src).unwrap();
        let items: Vec<(String, u8)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::ListItem { text, level, .. } => Some((text.clone(), *level)),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec![
                ("Item 1".to_string(), 0),
                ("Subitem A".to_string(), 1),
                ("Subitem B".to_string(), 1),
                ("Item 2".to_string(), 0),
            ],
            "nested structure: {items:?}"
        );
        let md = doc.export_to_markdown();
        assert!(
            md.contains("- Item 1\n    - Subitem A"),
            "markdown nesting:\n{md}"
        );
    }

    /// docling PR #3813: an empty <disp-formula> is skipped; everything after
    /// it survives (Python docling used to truncate the document there).
    #[test]
    fn empty_display_formula_does_not_truncate() {
        let xml = r#"<article><body><sec><title>S</title>
            <p>Before.</p>
            <disp-formula><tex-math/></disp-formula>
            <p>After.</p></sec></body></article>"#;
        let src = SourceDocument::from_bytes("p", InputFormat::XmlJats, xml.as_bytes().to_vec());
        let md = JatsBackend::default()
            .convert(&src)
            .unwrap()
            .export_to_markdown();
        assert!(md.contains("Before."), "got:\n{md}");
        assert!(
            md.contains("After."),
            "content after the empty formula lost:\n{md}"
        );
        assert!(!md.contains("$$"), "no phantom formula:\n{md}");
    }

    /// A scratch directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "docling-jats-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_png(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb(rgb)))
            .save_with_format(path, image::ImageFormat::Png)
            .unwrap();
    }

    fn write_jpg(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb(rgb)))
            .save_with_format(path, image::ImageFormat::Jpeg)
            .unwrap();
    }

    fn jats_body(body: &str) -> String {
        format!(
            r#"<article xmlns:xlink="http://www.w3.org/1999/xlink"><front><article-meta>
            <title-group><article-title>T</article-title></title-group>
          </article-meta></front><body><sec><title>S</title>{body}</sec></body></article>"#
        )
    }

    /// Convert `body` written to `dir/article.nxml`, as a file (so the
    /// backend knows the directory).
    fn convert_file(dir: &Path, body: &str, fetch_images: bool) -> DoclingDocument {
        let path = dir.join("article.nxml");
        std::fs::write(&path, jats_body(body)).unwrap();
        let src = SourceDocument::from_file(&path).unwrap();
        JatsBackend {
            images: local_images(fetch_images),
        }
        .convert(&src)
        .unwrap()
    }

    /// The `Local` tier (reads under the source directory) or none.
    fn local_images(on: bool) -> ImagePolicy {
        ImagePolicy::new(if on {
            ImageSources::Local
        } else {
            ImageSources::None
        })
    }

    fn picture_images(doc: &DoclingDocument) -> Vec<Option<&docling_core::PictureImage>> {
        doc.nodes
            .iter()
            .filter_map(|n| match n {
                Node::Picture { image, .. } => Some(image.as_ref()),
                _ => None,
            })
            .collect()
    }

    fn size_and_pixel(img: &docling_core::PictureImage) -> ((u32, u32), [u8; 3]) {
        let rgb = image::load_from_memory(&img.data).unwrap().to_rgb8();
        ((img.width, img.height), rgb.get_pixel(0, 0).0)
    }

    /// #392 (docling#4041): a figure's graphic is read only under
    /// `fetch_images`; then the relative href resolves against the file's
    /// directory and the image reaches the picture (and the JSON), while
    /// Markdown keeps the placeholder marker.
    #[test]
    fn figure_image_is_embedded_only_when_fetching() {
        let dir = TempDir::new("embed");
        write_png(&dir.path().join("images/figure.png"), 7, 5, [255, 0, 0]);
        let body = r#"<fig><label>Figure 1</label><caption><p>A red rectangle.</p></caption>
            <graphic xlink:href="images/figure.png"/></fig>"#;

        let doc = convert_file(dir.path(), body, false);
        assert_eq!(picture_images(&doc), [None]);

        let doc = convert_file(dir.path(), body, true);
        let pics = picture_images(&doc);
        assert_eq!(pics.len(), 1);
        let img = pics[0].expect("embedded");
        assert_eq!(size_and_pixel(img), ((7, 5), [255, 0, 0]));
        assert_eq!(img.mimetype, "image/png");
        let caption = doc.nodes.iter().find_map(|n| match n {
            Node::Picture { caption, .. } => caption.clone(),
            _ => None,
        });
        assert_eq!(caption.as_deref(), Some("Figure 1 A red rectangle."));
        assert!(doc.export_to_markdown().contains("<!-- image -->"));
        let json: serde_json::Value = serde_json::from_str(&doc.export_to_json()).unwrap();
        assert!(json["pictures"][0]["image"]["uri"]
            .as_str()
            .is_some_and(|u| u.starts_with("data:image/png;base64,")));
    }

    /// An in-memory source has no directory to resolve against, and one that
    /// came from a URL is not a local base either: no image, as docling.
    #[test]
    fn figure_image_needs_a_local_source_file() {
        let dir = TempDir::new("stream");
        write_png(&dir.path().join("figure.png"), 7, 5, [255, 0, 0]);
        let body = r#"<fig><graphic xlink:href="figure.png"/></fig>"#;
        let xml = jats_body(body).into_bytes();
        let stream = SourceDocument::from_bytes("article.nxml", InputFormat::XmlJats, xml.clone());
        let doc = JatsBackend {
            images: local_images(true),
        }
        .convert(&stream)
        .unwrap();
        assert_eq!(picture_images(&doc), [None]);

        let remote = SourceDocument::from_bytes("article.nxml", InputFormat::XmlJats, xml.clone())
            .with_base_url("https://example.com/article.nxml");
        let doc = JatsBackend {
            images: local_images(true),
        }
        .convert(&remote)
        .unwrap();
        assert_eq!(picture_images(&doc), [None]);

        // docling's `source_uri` analogue: a path attached to a stream.
        let mut with_path = SourceDocument::from_bytes("article.nxml", InputFormat::XmlJats, xml);
        with_path.path = Some(dir.path().join("source.nxml"));
        let doc = JatsBackend {
            images: local_images(true),
        }
        .convert(&with_path)
        .unwrap();
        assert!(picture_images(&doc)[0].is_some());
    }

    /// An extensionless href is probed with the raster suffixes, directly and
    /// inside `<alternatives>` behind an unsupported SVG rendition.
    #[test]
    fn figure_image_resolves_an_extensionless_href() {
        let dir = TempDir::new("extless");
        write_jpg(&dir.path().join("images/figure.jpg"), 9, 6, [0, 0, 255]);
        for graphic in [
            r#"<graphic xlink:href="images/figure"/>"#,
            r#"<alternatives><graphic xlink:href="images/unsupported.svg"/><graphic xlink:href="images/figure"/></alternatives>"#,
        ] {
            let doc = convert_file(dir.path(), &format!("<fig>{graphic}</fig>"), true);
            let pics = picture_images(&doc);
            assert_eq!(pics.len(), 1, "{graphic}");
            let img = pics[0].expect("probed .jpg");
            assert_eq!((img.width, img.height), (9, 6));
            assert_eq!(img.mimetype, "image/jpeg");
        }
    }

    /// Renditions are tried in document order: an undecodable file and an
    /// absolute path each fall through to the relative rendition after them.
    #[test]
    fn figure_image_falls_back_past_undecodable_and_absolute_renditions() {
        let dir = TempDir::new("fallback");
        std::fs::create_dir_all(dir.path().join("images")).unwrap();
        std::fs::write(dir.path().join("images/broken.png"), b"not an image").unwrap();
        write_png(&dir.path().join("images/figure.png"), 9, 6, [0, 0, 255]);
        write_png(&dir.path().join("absolute.png"), 7, 5, [255, 0, 0]);
        let absolute = dir.path().join("absolute.png");
        for body in [
            r#"<fig><alternatives><graphic xlink:href="images/broken.png"/><graphic xlink:href="images/figure.png"/></alternatives></fig>"#.to_string(),
            format!(
                r#"<fig><alternatives><graphic xlink:href="{}"/><graphic xlink:href="images/figure.png"/></alternatives></fig>"#,
                absolute.display()
            ),
        ] {
            let doc = convert_file(dir.path(), &body, true);
            let img = picture_images(&doc)[0].expect("fell back");
            assert_eq!(size_and_pixel(img), ((9, 6), [0, 0, 255]), "{body}");
        }
    }

    /// No graphic, no/blank href, a remote href, an SVG, a missing file: the
    /// picture stays a placeholder and the walk goes on.
    #[test]
    fn figure_image_skips_unavailable_renditions() {
        let dir = TempDir::new("skip");
        for graphic in [
            "",
            "<graphic/>",
            r#"<graphic xlink:href=" "/>"#,
            r#"<graphic xlink:href="https://example.com/figure.png"/>"#,
            r#"<graphic xlink:href="figure.svg"/>"#,
            r#"<graphic xlink:href="missing.png"/>"#,
        ] {
            let doc = convert_file(
                dir.path(),
                &format!("<fig>{graphic}</fig><p>Content after the unavailable figure.</p>"),
                true,
            );
            assert_eq!(picture_images(&doc), [None], "{graphic}");
            assert!(doc
                .export_to_markdown()
                .contains("Content after the unavailable figure."));
        }
    }

    /// A rendition climbing out of the source directory aborts the whole
    /// figure — the in-directory fallback after it is not tried.
    #[test]
    fn figure_image_blocks_path_traversal() {
        let dir = TempDir::new("traversal");
        write_png(&dir.path().join("outside.png"), 7, 5, [255, 0, 0]);
        let article_dir = dir.path().join("article");
        write_png(&article_dir.join("fallback.png"), 9, 6, [0, 0, 255]);
        let doc = convert_file(
            &article_dir,
            r#"<fig><alternatives><graphic xlink:href="../outside.png"/><graphic xlink:href="fallback.png"/></alternatives></fig>
               <p>Content after the blocked figure.</p>"#,
            true,
        );
        assert_eq!(picture_images(&doc), [None]);
        assert!(doc
            .export_to_markdown()
            .contains("Content after the blocked figure."));
    }

    #[test]
    fn figure_path_helpers_follow_docling() {
        assert!(is_local_path("images/a.png"));
        assert!(is_local_path("/abs/a.png"));
        assert!(is_local_path(r"C:\a.png"));
        assert!(!is_local_path("https://x/a.png"));
        assert!(!is_local_path("//cdn/a.png"));
        assert!(!is_local_path("data:image/png;base64,AA=="));
        assert!(is_absolute_path("/abs/a.png"));
        assert!(is_absolute_path("C:/a.png"));
        assert!(!is_absolute_path("images/a.png"));
        let base = Path::new("/base");
        assert_eq!(
            confined_path(base, "a/../b.png"),
            Some(PathBuf::from("/base/b.png"))
        );
        assert_eq!(confined_path(base, "../b.png"), None);
        assert_eq!(confined_path(base, "/etc/passwd"), None);
    }
}
