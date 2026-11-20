//! Reader for the `index.xml` of an iWork '09 (and earlier) Pages document
//! (docling's `docling/backend/iwork/pages_xml.py`, docling#4062, #383).
//!
//! Pages wrote a plain XML tree before 2013, in the `sf` namespace, and the
//! content the modern container keeps in an object graph is spelled out in
//! elements and attributes instead. Page furniture and comments each carry
//! their own `sf:text-body`, so the body walk prunes them and they are read
//! separately.

use std::collections::HashMap;

use roxmltree::{Document, Node as XmlNode};

use super::iwork::{grid_table, gunzip_capped, text_cell};
use super::ooxml::Package;
use super::pages::{
    clean, label_for_style, script_of, trim, unique_paragraphs, Block, Comment, Content,
    Formatting, ListLabel, ListStyle, Paragraph, Picture, Run, LABEL_TYPE_NONE, LABEL_TYPE_NUMBER,
    LABEL_TYPE_STRING,
};
use crate::error::ConversionError;

pub(crate) const SF_NS: &str = "http://developer.apple.com/namespaces/sf";
pub(crate) const SFA_NS: &str = "http://developer.apple.com/namespaces/sfa";
/// Decompressed-size ceiling for a legacy `index.xml.gz` (docling's
/// `MAX_LEGACY_XML_BYTES`): the package size caps only see the stored size.
const MAX_LEGACY_XML_BYTES: u64 = 100 * 1024 * 1024;

/// How many `sf:parent-ident` links a style chain is followed for.
const MAX_STYLE_INHERITANCE: usize = 8;

/// `sf:text-label` types that draw a fixed marker rather than a number.
const BULLET_LABEL_TYPES: [&str; 4] = ["bullet", "image", "string", "text"];

pub(crate) fn is_sf(node: XmlNode, name: &str) -> bool {
    node.is_element()
        && node.tag_name().namespace() == Some(SF_NS)
        && node.tag_name().name() == name
}

/// `sf_attr`: an attribute iWork '09 may or may not have qualified. Most carry
/// the `sf` namespace, but a few — `href` on `sf:link`, `path` on `sf:data` —
/// are written unqualified, and which spelling a document uses varies with the
/// release that wrote it.
pub(crate) fn sf_attr<'a>(node: XmlNode<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attribute((SF_NS, name))
        .or_else(|| node.attribute(name))
}

pub(crate) fn sfa_attr<'a>(node: XmlNode<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attribute((SFA_NS, name))
}

/// `int_attr`: an integer `sf:` attribute, tolerating absent or malformed values.
pub(crate) fn int_attr(node: XmlNode, name: &str) -> Option<usize> {
    node.attribute((SF_NS, name))?.trim().parse().ok()
}

/// The elements whose paragraphs are not body content: each carries its own
/// `sf:text-body`, so they are pruned from the body walk by element.
fn is_furniture(node: XmlNode) -> bool {
    ["header", "footer", "footnotes", "annotations"]
        .iter()
        .any(|t| is_sf(node, t))
}

pub(crate) fn is_media(node: XmlNode) -> bool {
    is_sf(node, "media") || is_sf(node, "image")
}

fn is_placeholder(node: XmlNode) -> bool {
    is_sf(node, "ghost-text") || is_sf(node, "ghost-text-ref")
}

/// docling's `read_content`: the content of an iWork '09 document out of its
/// `index.xml` member (optionally gzipped, decompressed against a ceiling).
pub(crate) fn read_content(pkg: &mut Package, member: &str) -> Result<Content, ConversionError> {
    let xml = read_index_xml(pkg, member, "Pages")?;
    let dom = Document::parse(&xml)
        .map_err(|e| ConversionError::Parse(format!("iwork: could not parse '{member}': {e}")))?;
    let root = dom.root_element();

    let style_names: HashMap<String, Option<String>> = legacy_styles(root, "paragraphstyle", |e| {
        e.attribute((SF_NS, "name")).map(str::to_string)
    });
    let character_styles: HashMap<String, Option<Formatting>> =
        legacy_styles(root, "characterstyle", legacy_formatting);
    let list_styles = legacy_list_styles(root);

    let mut blocks = Vec::new();
    for element in iter_body_elements(root) {
        if is_sf(element, "tabular-model") {
            if let Some(table) = legacy_table(element) {
                blocks.push(Block::Table(table));
            }
            continue;
        }
        if is_media(element) {
            if let Some(picture) = legacy_picture(element, pkg) {
                blocks.push(Block::Picture(picture));
            }
            continue;
        }
        let runs = legacy_runs(element, &character_styles);
        if runs.is_empty() {
            continue;
        }
        let style = element
            .attribute((SF_NS, "style"))
            .and_then(|s| style_names.get(s))
            .cloned()
            .flatten();
        let anchors = element
            .descendants()
            .filter(|n| is_sf(*n, "annotation-field"))
            .filter_map(|f| sfa_attr(f, "ID"))
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        blocks.push(Block::Paragraph(Paragraph {
            runs,
            label: label_for_style(style.as_deref()),
            list: legacy_list_label(element, &list_styles),
            anchors,
        }));
    }

    let furniture = |tag: &str| legacy_furniture(root, tag, &style_names, &character_styles);
    Ok(Content {
        blocks,
        headers: furniture("header"),
        footers: furniture("footer"),
        footnotes: furniture("footnotes"),
        comments: legacy_comments(root),
    })
}

/// docling's `parse_index`, up to the parse: the index member's XML text
/// (gunzipped against a ceiling when it is a `.gz`).
pub(crate) fn read_index_xml(
    pkg: &mut Package,
    member: &str,
    kind: &str,
) -> Result<String, ConversionError> {
    let raw = pkg.read_bytes(member).ok_or_else(|| {
        ConversionError::Parse(format!(
            "iwork: could not read '{member}' from the {kind} document"
        ))
    })?;
    let raw = if member.ends_with(".gz") {
        gunzip_capped(&raw, MAX_LEGACY_XML_BYTES, member)?
    } else {
        raw
    };
    String::from_utf8(raw)
        .map_err(|_| ConversionError::Parse(format!("iwork: '{member}' is not UTF-8")))
}

/// `legacy_geometry`: where an iWork '09 drawable sits, from the first
/// `sf:geometry` below it — its `sf:position` (required) and `sf:size`
/// (missing → zero). `None` is what an element that merely refers to a
/// positioned one looks like.
pub(crate) fn legacy_geometry(element: XmlNode) -> Option<super::keynote::Geometry> {
    let geometry = element.descendants().find(|n| is_sf(*n, "geometry"))?;
    let position = geometry.descendants().find(|n| is_sf(*n, "position"))?;
    let left = float_attr(position, "x")?;
    let top = float_attr(position, "y")?;
    let size = geometry.descendants().find(|n| is_sf(*n, "size"));
    let width = size.and_then(|s| float_attr(s, "w")).unwrap_or(0.0);
    let height = size.and_then(|s| float_attr(s, "h")).unwrap_or(0.0);
    Some(super::keynote::Geometry {
        left,
        top,
        width,
        height,
    })
}

/// `float_attr`: an `sfa:` measurement attribute, tolerating absent or
/// malformed values.
pub(crate) fn float_attr(node: XmlNode, name: &str) -> Option<f64> {
    sfa_attr(node, name)?.trim().parse().ok()
}

/// `legacy_inherited_lists`: the list style each paragraph style ends up
/// carrying, keyed by every name the paragraph style answers to. A style
/// that names none inherits its parent's (`sf:parent-ident`), so the chains
/// are walked once here and flattened into a single lookup.
pub(crate) fn legacy_inherited_lists<'a>(root: XmlNode<'a, 'a>) -> HashMap<String, String> {
    let mut styles: HashMap<&'a str, XmlNode<'a, 'a>> = HashMap::new();
    for element in root.descendants().filter(|n| is_sf(*n, "paragraphstyle")) {
        for key in [element.attribute((SF_NS, "ident")), sfa_attr(element, "ID")]
            .into_iter()
            .flatten()
        {
            styles.entry(key).or_insert(element);
        }
    }
    let mut resolved = HashMap::new();
    for (key, element) in &styles {
        let mut current = Some(*element);
        for _ in 0..MAX_STYLE_INHERITANCE {
            let Some(style) = current else {
                break;
            };
            if let Some(named) = own_list_style(style) {
                resolved.insert(key.to_string(), named.to_string());
                break;
            }
            current = style
                .attribute((SF_NS, "parent-ident"))
                .and_then(|p| styles.get(p).copied());
        }
    }
    resolved
}

/// `own_list_style`: the list style one paragraph style names
/// (`sf:listStyle` → `sf:liststyle-ref`), ignoring what it inherits.
fn own_list_style<'a>(style: XmlNode<'a, 'a>) -> Option<&'a str> {
    style
        .descendants()
        .filter(|n| is_sf(*n, "listStyle"))
        .flat_map(|named| named.descendants().filter(|n| is_sf(*n, "liststyle-ref")))
        .find_map(|r| sfa_attr(r, "IDREF").filter(|id| !id.is_empty()))
}

/// `legacy_table`: table data from one `sf:tabular-model`. Cells are stored
/// flat in `sf:datasource`, in row-major order, so the grid dimensions on
/// `sf:grid` are what give them their positions.
pub(crate) fn legacy_table(model: XmlNode) -> Option<docling_core::Table> {
    let grid = model.descendants().find(|n| is_sf(*n, "grid"))?;
    let num_cols = int_attr(grid, "numcols")?;
    let num_rows = int_attr(grid, "numrows")?;
    let header_rows = int_attr(model, "num-header-rows").unwrap_or(0);
    if num_cols == 0 || num_rows == 0 {
        return None;
    }
    // `legacy_cell_text`: the datasource holds one element per cell of the
    // grid, in row-major order, named for what the cell holds — `sf:t` for
    // text, `sf:n` for a number, and so on. Only text is recovered (a number,
    // a date or a formula result is left empty rather than guessed at), but
    // every cell still takes its place, or everything after the first
    // non-text one would shift along a column (docling#4330, #466).
    let source = grid.descendants().find(|n| is_sf(*n, "datasource"))?;
    let values: Vec<String> = source
        .children()
        .filter(XmlNode::is_element)
        .map(|cell| {
            let Some(ct) = cell.descendants().find(|n| is_sf(*n, "ct")) else {
                return String::new();
            };
            let text = match sfa_attr(ct, "s") {
                Some(s) if !s.is_empty() => s.to_string(),
                // Text *nodes* only: an element's `text()` is its first text
                // child, which would count the same text twice.
                _ => ct
                    .descendants()
                    .filter(|n| n.is_text())
                    .filter_map(|n| n.text())
                    .collect::<String>(),
            };
            clean(&text).trim().to_string()
        })
        .collect();
    if values.iter().all(String::is_empty) {
        return None;
    }
    let cells = values
        .into_iter()
        .take(num_cols * num_rows)
        .enumerate()
        .map(|(index, text)| text_cell(text, index / num_cols, index % num_cols, header_rows))
        .collect();
    Some(grid_table(num_rows, num_cols, cells))
}

/// `legacy_picture`: an '09 image, whose bytes are a member of the container.
/// `None` when the element names no stored data; a named member that is
/// missing still places the picture, without an image.
pub(crate) fn legacy_picture(media: XmlNode, pkg: &mut Package) -> Option<Picture> {
    for data in media.descendants().filter(|n| is_sf(*n, "data")) {
        let Some(path) = sf_attr(data, "path").filter(|p| !p.is_empty()) else {
            continue;
        };
        return Some(Picture {
            data: pkg.read_bytes(path),
            name: path.to_string(),
        });
    }
    None
}

/// `iter_body_elements`: the paragraph, table and image elements of the body,
/// in document order, skipping page furniture. A table and an image are not
/// descended into once found, so the paragraphs inside a table cell stay in
/// the table rather than reappearing as body text.
fn iter_body_elements<'a>(root: XmlNode<'a, 'a>) -> Vec<XmlNode<'a, 'a>> {
    let mut elements = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_sf(node, "p") || is_sf(node, "tabular-model") || is_media(node) {
            elements.push(node);
            continue;
        }
        for child in node.children().rev() {
            if child.is_element() && !is_furniture(child) {
                stack.push(child);
            }
        }
    }
    elements
}

/// `legacy_runs`: the runs of an '09 paragraph. `sf:span` carries the
/// character style, `sf:link` the address, so the paragraph is walked node by
/// node rather than flattened; a child's tail text sits outside it and keeps
/// the parent's formatting, and template placeholder text is skipped. Walked
/// with an explicit stack: nesting depth is attacker-controlled.
pub(crate) fn legacy_runs(
    paragraph: XmlNode,
    character_styles: &HashMap<String, Option<Formatting>>,
) -> Vec<Run> {
    let mut runs = Vec::new();
    // Every node is a frame: children are pushed back to front so popping
    // yields document order, and a text node becomes a run the moment it is
    // popped — that is what keeps a child's tail text after the child.
    let mut stack: Vec<(XmlNode, Option<Formatting>, Option<String>)> =
        vec![(paragraph, None, None)];
    while let Some((node, fmt, link)) = stack.pop() {
        if node.is_text() {
            if let Some(text) = node.text() {
                runs.push(Run {
                    text: clean(text),
                    fmt,
                    link: link.clone(),
                });
            }
            continue;
        }
        for child in node.children().rev() {
            if child.is_text() {
                stack.push((child, fmt, link.clone()));
                continue;
            }
            if !child.is_element() || is_placeholder(child) {
                continue;
            }
            let inherited = if is_sf(child, "span") {
                match child.attribute((SF_NS, "style")) {
                    Some(style) => character_styles.get(style).copied().unwrap_or(fmt),
                    None => fmt,
                }
            } else {
                fmt
            };
            // docling's `_hyperlink` (docling#5384, #1164): an address
            // `urlparse` / `AnyUrl` reject is dropped, its text kept.
            let nested = if is_sf(child, "link") {
                match sf_attr(child, "href") {
                    Some(href) => super::odf::hyperlink_from_href(Some(href)),
                    None => link.clone(),
                }
            } else {
                link.clone()
            };
            stack.push((child, inherited, nested));
        }
    }
    trim(runs)
}

/// `legacy_furniture`: the paragraphs of one kind of '09 page furniture, each
/// identical text once.
fn legacy_furniture(
    root: XmlNode,
    tag: &str,
    style_names: &HashMap<String, Option<String>>,
    character_styles: &HashMap<String, Option<Formatting>>,
) -> Vec<Paragraph> {
    let mut paragraphs = Vec::new();
    for element in root.descendants().filter(|n| is_sf(*n, tag)) {
        for para in element.descendants().filter(|n| is_sf(*n, "p")) {
            let runs = legacy_runs(para, character_styles);
            if runs.is_empty() {
                continue;
            }
            let style = para
                .attribute((SF_NS, "style"))
                .and_then(|s| style_names.get(s))
                .cloned()
                .flatten();
            paragraphs.push(Paragraph::new(runs, label_for_style(style.as_deref())));
        }
    }
    unique_paragraphs(paragraphs)
}

/// `legacy_comments`: the comments of an '09 document, with the text each one
/// annotates. An `sf:annotation` holds its text in a storage of its own and
/// names the `sf:annotation-field` in the body that it targets.
fn legacy_comments(root: XmlNode) -> Vec<Comment> {
    let no_styles = HashMap::new();
    let mut comments = Vec::new();
    for annotation in root.descendants().filter(|n| is_sf(*n, "annotation")) {
        let text = annotation
            .descendants()
            .filter(|n| is_sf(*n, "p"))
            .map(|para| {
                Paragraph::new(legacy_runs(para, &no_styles), super::pages::Label::Text).text()
            })
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if !text.is_empty() {
            comments.push(Comment {
                text,
                anchor: annotation
                    .attribute((SF_NS, "target"))
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    comments
}

/// `legacy_styles`: one kind of iWork '09 style, keyed by every name it
/// answers to. A paragraph or a span names its style through `sf:style`, and
/// what it puts there is sometimes the style's `sf:ident` and sometimes its
/// `sfa:ID`. Both are indexed so a reference resolves either way; the first
/// definition of a key wins.
pub(crate) fn legacy_styles<'a, T: Clone>(
    root: XmlNode<'a, 'a>,
    tag: &str,
    decode: impl Fn(XmlNode<'a, 'a>) -> T,
) -> HashMap<String, T> {
    let mut styles = HashMap::new();
    for element in root.descendants().filter(|n| is_sf(*n, tag)) {
        let keys = [element.attribute((SF_NS, "ident")), sfa_attr(element, "ID")];
        if keys.iter().all(|k| k.is_none()) {
            continue;
        }
        let value = decode(element);
        for key in keys.into_iter().flatten() {
            styles
                .entry(key.to_string())
                .or_insert_with(|| value.clone());
        }
    }
    styles
}

/// `legacy_list_styles`: the `sf:liststyle` definitions by identifier — one
/// `sf:list-label-typeinfo` per nesting level.
pub(crate) fn legacy_list_styles(root: XmlNode) -> HashMap<String, ListStyle> {
    let mut styles: HashMap<String, ListStyle> = HashMap::new();
    for element in root.descendants().filter(|n| is_sf(*n, "liststyle")) {
        let keys = [element.attribute((SF_NS, "ident")), sfa_attr(element, "ID")];
        if !keys.iter().flatten().any(|k| !styles.contains_key(*k)) {
            continue;
        }
        let mut style = ListStyle::default();
        for level in element
            .descendants()
            .filter(|n| is_sf(*n, "list-label-typeinfo"))
        {
            if level.attribute((SF_NS, "type")) == Some("none") {
                style.label_types.push(LABEL_TYPE_NONE);
                style.strings.push(String::new());
                continue;
            }
            let text_label = level.descendants().find(|n| is_sf(*n, "text-label"));
            let kind = text_label.and_then(|t| t.attribute((SF_NS, "type")));
            if let Some(kind) = kind {
                if !BULLET_LABEL_TYPES.contains(&kind) {
                    // Anything else names a numbering sequence: decimal,
                    // upper-roman, lower-alpha and the rest, which Pages counts
                    // rather than draws.
                    style.label_types.push(LABEL_TYPE_NUMBER);
                    style.strings.push(String::new());
                    continue;
                }
            }
            style.label_types.push(LABEL_TYPE_STRING);
            style.strings.push(
                text_label
                    .and_then(|t| t.attribute((SF_NS, "format")))
                    .unwrap_or("")
                    .to_string(),
            );
        }
        for key in keys.into_iter().flatten() {
            styles
                .entry(key.to_string())
                .or_insert_with(|| style.clone());
        }
    }
    styles
}

/// `legacy_list_label`: how an '09 paragraph is labelled as a list item, if it
/// is one. `sf:list-level` is the rung of the style's ladder, counted from the
/// unlabelled one ordinary body text sits on (a paragraph naming no level is
/// on that rung); docling counts nesting from the first labelled rung
/// instead, so the depth is one less — docling's reading since its Keynote
/// reader (2.131, docling#4330), which the Keynote '09 reader here shares.
/// Pages' own ladders label every rung alike, so its documents read the same
/// as under the earlier one-based reading.
fn legacy_list_label(
    paragraph: XmlNode,
    list_styles: &HashMap<String, ListStyle>,
) -> Option<ListLabel> {
    let style = list_styles.get(paragraph.attribute((SF_NS, "list-style")).unwrap_or(""))?;
    let rung = int_attr(paragraph, "list-level").unwrap_or(0);
    let mut label = style.label(rung)?;
    label.depth = rung.saturating_sub(1);
    Some(label)
}

/// `legacy_formatting`: an iWork '09 character style's property map as
/// formatting. A property element (`sf:bold`, `sf:superscript`, …) holds its
/// value in a child carrying `sfa:number`; zero (or none) leaves it unset.
pub(crate) fn legacy_formatting(style: XmlNode) -> Option<Formatting> {
    let (mut bold, mut italic, mut underline, mut strike) = (false, false, false, false);
    let mut script = None;
    for element in style.descendants().filter(|n| n.is_element()) {
        let number = element
            .children()
            .filter(|c| c.is_element())
            .find_map(|c| sfa_attr(c, "number"));
        let Some(number) = number.filter(|n| *n != "0") else {
            continue;
        };
        if is_sf(element, "bold") {
            bold = true;
        } else if is_sf(element, "italic") {
            italic = true;
        } else if is_sf(element, "underline") {
            underline = true;
        } else if is_sf(element, "strikethru") {
            strike = true;
        } else if is_sf(element, "superscript") {
            script = script_of(as_int(number));
        }
    }
    Formatting::build(bold, italic, underline, strike, script)
}

/// `as_int`: an iWork property number, which may be written as a float.
fn as_int(number: &str) -> u64 {
    number
        .trim()
        .parse::<f64>()
        .map(|f| {
            if f.is_finite() && f >= 0.0 {
                f as u64
            } else {
                0
            }
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_of(xml: &str) -> Content {
        // Wrap the index.xml into a minimal zip package.
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("index.xml", opts).unwrap();
            std::io::Write::write_all(&mut zip, xml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let mut pkg = Package::open(&buf.into_inner()).unwrap();
        read_content(&mut pkg, "index.xml").unwrap()
    }

    const HEAD: &str = r#"<sl:document xmlns:sl="http://developer.apple.com/namespaces/sl" xmlns:sf="http://developer.apple.com/namespaces/sf" xmlns:sfa="http://developer.apple.com/namespaces/sfa">"#;

    /// Spans carry character styles, links carry hrefs, the tail after a span
    /// keeps the paragraph's formatting, placeholders are skipped, and the
    /// spaces around a formatted phrase survive (trimmed only at the ends).
    #[test]
    fn runs_follow_spans_links_and_placeholders() {
        let xml = format!(
            r#"{HEAD}<sf:stylesheet>
              <sf:characterstyle sf:ident="cs-bold" sf:name="Bold"><sf:property-map><sf:bold><sf:number sfa:number="1"/></sf:bold></sf:property-map></sf:characterstyle>
              <sf:characterstyle sfa:ID="cs-sup"><sf:property-map><sf:superscript><sf:number sfa:number="1.0"/></sf:superscript></sf:property-map></sf:characterstyle>
              <sf:paragraphstyle sf:ident="ps-h" sf:name="Heading 2"/>
            </sf:stylesheet>
            <sf:text-body>
              <sf:p sf:style="ps-h"> A heading </sf:p>
              <sf:p>Plain <sf:span sf:style="cs-bold">bold</sf:span> then <sf:link href="https://x.y/">a <sf:span sf:style="cs-sup">link</sf:span></sf:link>.<sf:ghost-text>placeholder</sf:ghost-text> end</sf:p>
            </sf:text-body></sl:document>"#
        );
        let content = content_of(&xml);
        let paras: Vec<&Paragraph> = content
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(paras.len(), 2);
        assert_eq!(paras[0].label, super::super::pages::Label::Heading(2));
        assert_eq!(paras[0].text(), "A heading");
        let bold = Formatting::build(true, false, false, false, None);
        let sup = Formatting::build(
            false,
            false,
            false,
            false,
            Some(docling_core::Script::Super),
        );
        let link = Some("https://x.y/".to_string());
        assert_eq!(
            paras[1].runs,
            vec![
                Run::plain("Plain "),
                Run {
                    text: "bold".into(),
                    fmt: bold,
                    link: None
                },
                Run::plain(" then "),
                Run {
                    text: "a ".into(),
                    fmt: None,
                    link: link.clone()
                },
                Run {
                    text: "link".into(),
                    fmt: sup,
                    link
                },
                Run::plain("."),
                Run::plain(" end"),
            ]
        );
    }

    /// docling#5384 (#1164, `test_legacy_malformed_hyperlink_keeps_the_following_paragraph`):
    /// a link `urlparse` / `AnyUrl` reject loses its target, not its text
    /// nor the paragraphs after it; a URL and a relative file stay links.
    #[test]
    fn malformed_hyperlink_keeps_the_following_paragraph() {
        let xml = format!(
            r#"{HEAD}<sf:text-body>
              <sf:p><sf:link href="https://[broken">Broken</sf:link></sf:p>
              <sf:p><sf:link href="https://example.org/valid">Valid</sf:link></sf:p>
              <sf:p><sf:link href="notes.docx">Notes</sf:link></sf:p>
              <sf:p>After</sf:p>
            </sf:text-body></sl:document>"#
        );
        let content = content_of(&xml);
        let links: Vec<(String, Option<String>)> = content
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(p) => {
                    Some((p.text(), p.runs.first().and_then(|r| r.link.clone())))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            links,
            [
                ("Broken".to_string(), None),
                ("Valid".into(), Some("https://example.org/valid".into())),
                ("Notes".into(), Some("notes.docx".into())),
                ("After".into(), None),
            ]
        );
    }

    /// Lists, furniture, comments, tables and pictures come out of the '09
    /// vocabulary as upstream reads them.
    #[test]
    fn lists_furniture_comments_and_tables() {
        let xml = format!(
            r#"{HEAD}<sf:stylesheet>
              <sf:liststyle sf:ident="ls">
                <sf:list-label-typeinfo sf:type="none"/>
                <sf:list-label-typeinfo><sf:text-label sf:type="bullet" sf:format="•"/></sf:list-label-typeinfo>
                <sf:list-label-typeinfo><sf:text-label sf:type="decimal" sf:format="%L."/></sf:list-label-typeinfo>
                <sf:list-label-typeinfo sf:type="none"/>
              </sf:liststyle>
            </sf:stylesheet>
            <sf:header><sf:text-body><sf:p>HEAD</sf:p><sf:p>HEAD</sf:p></sf:text-body></sf:header>
            <sf:footer><sf:text-body><sf:p>FOOT</sf:p></sf:text-body></sf:footer>
            <sf:footnotes><sf:text-body><sf:p>Note one</sf:p></sf:text-body></sf:footnotes>
            <sf:annotations><sf:annotation sf:target="af-1"><sf:text-body><sf:p>a comment</sf:p><sf:p>more</sf:p></sf:text-body></sf:annotation></sf:annotations>
            <sf:text-body>
              <sf:p>Body <sf:annotation-field sfa:ID="af-1">annotated</sf:annotation-field> text</sf:p>
              <sf:p sf:list-style="ls" sf:list-level="1">first</sf:p>
              <sf:p sf:list-style="ls" sf:list-level="2">second</sf:p>
              <sf:p sf:list-style="ls" sf:list-level="3">plain again</sf:p>
              <sf:tabular-model sf:num-header-rows="1"><sf:grid sf:numcols="2" sf:numrows="2">
                <sf:datasource><sf:ct sfa:s="A"/><sf:ct><sf:p>B</sf:p></sf:ct><sf:ct sfa:s="1"/><sf:ct sfa:s="2"/></sf:datasource>
              </sf:grid></sf:tabular-model>
              <sf:media><sf:content><sf:image-media><sf:data sf:path="missing.png"/></sf:image-media></sf:content></sf:media>
            </sf:text-body></sl:document>"#
        );
        let content = content_of(&xml);
        let kinds: Vec<&str> = content
            .blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph(_) => "p",
                Block::Table(_) => "table",
                Block::Picture(_) => "picture",
                Block::Chart(_) => "chart",
            })
            .collect();
        assert_eq!(kinds, ["p", "p", "p", "p", "table", "picture"]);
        let Block::Paragraph(body) = &content.blocks[0] else {
            unreachable!()
        };
        assert_eq!(body.text(), "Body annotated text");
        assert_eq!(body.anchors, vec!["af-1".to_string()]);
        let Block::Paragraph(first) = &content.blocks[1] else {
            unreachable!()
        };
        assert_eq!(
            first.list,
            Some(ListLabel {
                depth: 0,
                enumerated: false,
                marker: "•".into()
            })
        );
        let Block::Paragraph(second) = &content.blocks[2] else {
            unreachable!()
        };
        assert_eq!(
            second.list,
            Some(ListLabel {
                depth: 1,
                enumerated: true,
                marker: String::new()
            })
        );
        let Block::Paragraph(third) = &content.blocks[3] else {
            unreachable!()
        };
        assert_eq!(third.list, None, "a `none` level is body text");
        let Block::Table(table) = &content.blocks[4] else {
            unreachable!()
        };
        assert_eq!(table.rows, vec![vec!["A", "B"], vec!["1", "2"]]);
        assert!(table.cells.as_ref().unwrap()[0].column_header);
        let Block::Picture(pic) = &content.blocks[5] else {
            unreachable!()
        };
        assert_eq!(
            (pic.name.as_str(), pic.data.is_none()),
            ("missing.png", true)
        );
        assert_eq!(content.headers.len(), 1, "identical header variants once");
        assert_eq!(content.footers[0].text(), "FOOT");
        assert_eq!(content.footnotes[0].text(), "Note one");
        assert_eq!(
            content.comments,
            vec![Comment {
                text: "a comment more".into(),
                anchor: "af-1".into()
            }]
        );
    }
}
