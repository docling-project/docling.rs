//! AbiWord backend (`.abw`/`.zabw`/`.awt`) — a docling.rs extension (#216);
//! docling has no AbiWord reader (Python reaches it only via LibreOffice's
//! libabw import filter).
//!
//! The format is a single XML document (AWML): `<section>`s hold `<p>`
//! paragraphs whose `style` maps like DOCX styles (`Title` → `#`,
//! `Subtitle` → `##`, `heading N` → `N+1`), character runs are `<c>` elements
//! with CSS-like `props` (`font-weight:bold`, `font-style:italic`,
//! `text-decoration:underline|line-through`, `text-position:superscript|
//! subscript`), hyperlinks are `<a xlink:href>` wrappers, lists are `<p>`s
//! with `listid`/`level` plus a `type="list_label"` field marker (whose
//! rendered label and following tab are dropped — the serializer draws its
//! own), tables are `<table>`/`<cell>` with an attach grid
//! (`left-attach`/`right-attach`/`top-attach`/`bot-attach`; spans replicate
//! their anchor's text, docling-style), and images are `<image dataid>`
//! references into the base64 `<data>` block. A `.zabw` (or gzip-compressed
//! `.abw` — AbiWord writes both) is the same XML behind a gzip magic;
//! templates (`.awt`) carry the same body.

use std::collections::HashMap;
use std::io::Read;

use roxmltree::{Document, Node as XmlNode};

use crate::backend::markdown::escape_text;
use crate::backend::rtf::image_size;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{inline_paragraph_node, DoclingDocument, InlineRun, Node, Script, Table};

pub struct AbwBackend;

impl DeclarativeBackend for AbwBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        // .zabw / gzip-compressed .abw: same XML behind the gzip magic.
        let text: String = if source.bytes.starts_with(&[0x1f, 0x8b]) {
            let mut out = String::new();
            flate2::read::GzDecoder::new(source.bytes.as_slice())
                .read_to_string(&mut out)
                .map_err(|e| ConversionError::with_source("abw: gzip", e))?;
            out
        } else {
            source.text()?.to_string()
        };
        super::xml_depth::check(&text, "abw")?;
        // AbiWord files open with a DOCTYPE; allow it (roxmltree refuses
        // DTDs by default).
        let dom = Document::parse_with_options(
            &text,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .map_err(|e| ConversionError::with_source("abw", e))?;
        if dom.root_element().tag_name().name() != "abiword" {
            return Err(ConversionError::Parse(
                "abw: not an AbiWord document (no <abiword> root)".into(),
            ));
        }

        let images = load_data_blocks(&dom);
        let mut doc = DoclingDocument::new(&source.name);
        let mut lists = ListState::default();
        for section in dom
            .root_element()
            .children()
            .filter(|n| n.has_tag_name("section"))
        {
            // Header/footer sections (incl. the -even/-first variants) are
            // page furniture — docling drops them.
            if attr(section, "type")
                .is_some_and(|t| t.starts_with("header") || t.starts_with("footer"))
            {
                continue;
            }
            walk_blocks(section, &images, &mut doc, &mut lists);
        }
        Ok(doc)
    }
}

/// Per-`listid` ordered-item counters, so numbering survives interleaved lists.
#[derive(Default)]
struct ListState {
    counters: HashMap<String, u64>,
    last_listid: Option<String>,
}

/// `<data><d name=… mime-type=… base64="yes">…` → decoded image bytes by name.
fn load_data_blocks(dom: &Document) -> HashMap<String, (String, Vec<u8>)> {
    let mut out = HashMap::new();
    for d in dom.descendants().filter(|n| n.has_tag_name("d")) {
        let (Some(name), Some(mime)) = (attr(d, "name"), attr(d, "mime-type")) else {
            continue;
        };
        if !mime.starts_with("image/") || attr(d, "base64") != Some("yes") {
            continue;
        }
        let b64: String = d
            .text()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if let Some(bytes) = docling_core::base64::decode(&b64) {
            out.insert(name.to_string(), (mime.to_string(), bytes));
        }
    }
    out
}

fn walk_blocks(
    parent: XmlNode,
    images: &HashMap<String, (String, Vec<u8>)>,
    doc: &mut DoclingDocument,
    lists: &mut ListState,
) {
    for child in parent.children().filter(XmlNode::is_element) {
        match child.tag_name().name() {
            "p" => emit_paragraph(child, images, doc, lists),
            "table" => {
                lists.last_listid = None;
                if let Some(table) = parse_table(child) {
                    doc.push(Node::Table(table));
                }
            }
            // Frames and other wrappers: their block content still counts.
            _ => walk_blocks(child, images, doc, lists),
        }
    }
}

/// One formatted run of paragraph text.
struct Run {
    text: String,
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
    script: u8, // 0 none, 1 sub, 2 super
    href: Option<String>,
}

fn emit_paragraph(
    p: XmlNode,
    images: &HashMap<String, (String, Vec<u8>)>,
    doc: &mut DoclingDocument,
    lists: &mut ListState,
) {
    // Anchored images become pictures before the paragraph text (docling's
    // DOCX order).
    for img in p.descendants().filter(|n| n.has_tag_name("image")) {
        let image = attr(img, "dataid")
            .and_then(|id| images.get(id))
            .map(|(mime, bytes)| {
                let (width, height) = image_size(mime, bytes).unwrap_or((0, 0));
                docling_core::PictureImage {
                    dpi: docling_core::PictureImage::DEFAULT_DPI,
                    mimetype: mime.clone(),
                    width,
                    height,
                    data: bytes.clone(),
                    display_size: None,
                }
            });
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image,
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
    }

    let mut runs = Vec::new();
    collect_runs(p, false, None, &mut runs);
    let text = runs_markdown(&runs);
    if text.is_empty() {
        return;
    }

    let style = attr(p, "style").unwrap_or("");
    // A list paragraph: an explicit list id (with its label marker dropped by
    // `collect_runs`). Style names decide numbering, docling's DOCX reading.
    if let Some(listid) = attr(p, "listid").filter(|_| has_list_label(p)) {
        let level = attr(p, "level")
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(0);
        let ordered = style.contains("Number");
        let number = if ordered {
            let n = lists.counters.entry(listid.to_string()).or_insert(0);
            *n += 1;
            *n
        } else {
            0
        };
        let first = lists.last_listid.as_deref() != Some(listid);
        lists.last_listid = Some(listid.to_string());
        doc.push(Node::ListItem {
            ordered,
            number,
            first_in_list: first,
            text,
            level,
            marker: None,
            location: None,
            split: None,
            dclx: None,
            href: None,
            layer: None,
        });
        return;
    }
    lists.last_listid = None;

    // Style mapping mirrors our DOCX conversions: Title → #, "heading N" →
    // N+1; everything else (Subtitle, Author, captions, Normal) is body text.
    let lower = style.to_ascii_lowercase();
    if lower == "title" {
        doc.push(Node::Heading { level: 1, text });
    } else if let Some(n) = lower
        .strip_prefix("heading ")
        .and_then(|v| v.parse::<u8>().ok())
    {
        doc.push(Node::Heading {
            level: n.saturating_add(1).min(6),
            text,
        });
    } else {
        doc.push(inline_paragraph_node(text, runs_inline(&runs), false));
    }
}

/// Whether the paragraph carries a rendered list label (field or labeled run) —
/// the marker AbiWord writes for real list items.
fn has_list_label(p: XmlNode) -> bool {
    p.children().any(|c| {
        attr(c, "type") == Some("list_label")
            || (c.has_tag_name("field") && attr(c, "type") == Some("list_label"))
    })
}

/// Collect a paragraph's runs in order. `after_label` state drops the tab that
/// separates a list label from the item text.
fn collect_runs(el: XmlNode, in_label_tail: bool, href: Option<&str>, out: &mut Vec<Run>) {
    let mut strip_tab = in_label_tail;
    for child in el.children() {
        if child.is_text() {
            let mut t = child.text().unwrap_or("").to_string();
            if strip_tab {
                t = t.trim_start_matches('\t').to_string();
                strip_tab = t.is_empty();
            }
            if !t.is_empty() {
                out.push(plain_run(t, href, None));
            }
        } else if child.is_element() {
            match child.tag_name().name() {
                // The rendered list-label text is dropped: the serializer
                // draws its own marker.
                "field" if attr(child, "type") == Some("list_label") => strip_tab = true,
                "c" => {
                    let props = attr(child, "props").unwrap_or("");
                    let mut t: String = child
                        .descendants()
                        .filter(|n| n.is_text())
                        .filter_map(|n| n.text())
                        .collect();
                    if strip_tab || attr(child, "type") == Some("list_label") {
                        t = t.trim_start_matches('\t').to_string();
                        strip_tab = false;
                    }
                    if !t.is_empty() {
                        out.push(plain_run(t, href, Some(props)));
                    }
                }
                "a" => {
                    let target = attr_local(child, "href");
                    collect_runs(child, false, target, out);
                    strip_tab = false;
                }
                "br" => out.push(plain_run("\n".into(), None, None)),
                "image" | "data" => {}
                _ => {
                    let before = out.len();
                    collect_runs(child, strip_tab, href, out);
                    strip_tab = strip_tab && out.len() == before;
                }
            }
        }
    }
}

fn plain_run(text: String, href: Option<&str>, props: Option<&str>) -> Run {
    let has = |k: &str| props.is_some_and(|p| p.contains(k));
    Run {
        text,
        bold: has("font-weight:bold"),
        italic: has("font-style:italic"),
        strike: has("line-through"),
        underline: has("text-decoration:underline"),
        script: if has("text-position:superscript") {
            2
        } else if has("text-position:subscript") {
            1
        } else {
            0
        },
        href: href.map(str::to_string),
    }
}

/// The paragraph's Markdown: escaped text with bold/italic/strike markers and
/// `[anchor](href)` links, whitespace-trimmed at both ends.
fn runs_markdown(runs: &[Run]) -> String {
    let mut out = String::new();
    for r in runs {
        let mut s = escape_text(&r.text);
        if r.bold {
            s = format!("**{s}**");
        }
        if r.italic {
            s = format!("*{s}*");
        }
        if r.strike {
            s = format!("~~{s}~~");
        }
        if let Some(href) = &r.href {
            s = docling_core::hyperlink::markdown_link(&s, href);
        }
        out.push_str(&s);
    }
    out.trim().to_string()
}

/// The structured [`InlineRun`]s (DocLang-only; underline and sub/superscript
/// survive only here).
fn runs_inline(runs: &[Run]) -> Vec<InlineRun> {
    runs.iter()
        .filter(|r| !r.text.trim().is_empty())
        .map(|r| InlineRun {
            text: r.text.clone(),
            bold: r.bold,
            italic: r.italic,
            underline: r.underline,
            strike: r.strike,
            script: match r.script {
                1 => Script::Sub,
                2 => Script::Super,
                _ => Script::Baseline,
            },
            code: false,
            formula: false,
        })
        .collect()
}

/// `<table>`: cells carry their grid rectangle as `left/right/top/bot-attach`
/// props; spans replicate the anchor's text across the covered positions.
fn parse_table(table: XmlNode) -> Option<Table> {
    struct Cell {
        l: usize,
        r: usize,
        t: usize,
        b: usize,
        text: String,
    }
    let mut cells = Vec::new();
    for cell in table.children().filter(|n| n.has_tag_name("cell")) {
        let props = attr(cell, "props").unwrap_or("");
        let get = |k: &str| -> Option<usize> {
            props.split(';').find_map(|kv| {
                let (key, val) = kv.split_once(':')?;
                (key.trim() == k).then(|| val.trim().parse().ok())?
            })
        };
        let (l, r, t, b) = (
            get("left-attach")?,
            get("right-attach")?,
            get("top-attach")?,
            get("bot-attach")?,
        );
        let text = cell
            .children()
            .filter(|n| n.has_tag_name("p"))
            .map(|p| {
                let mut runs = Vec::new();
                collect_runs(p, false, None, &mut runs);
                runs_markdown(&runs)
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        cells.push(Cell { l, r, t, b, text });
    }
    let rows_n = cells.iter().map(|c| c.b).max()?;
    let cols_n = cells.iter().map(|c| c.r).max()?;
    if rows_n == 0 || cols_n == 0 {
        return None;
    }
    let mut rows = vec![vec![String::new(); cols_n]; rows_n];
    for c in &cells {
        for row in rows.iter_mut().take(c.b).skip(c.t) {
            for slot in row.iter_mut().take(c.r).skip(c.l) {
                *slot = c.text.clone();
            }
        }
    }
    Some(Table {
        rows,
        ..Default::default()
    })
}

/// Attribute by local name (`xlink:href` etc. are namespaced).
fn attr_local<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

/// Plain attribute.
fn attr<'a>(node: XmlNode<'a, '_>, name: &str) -> Option<&'a str> {
    node.attribute(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(xml: &str) -> DoclingDocument {
        let source = SourceDocument::from_bytes(
            "t.abw".to_string(),
            InputFormat::Abiword,
            xml.as_bytes().to_vec(),
        );
        AbwBackend.convert(&source).unwrap()
    }

    #[test]
    fn styles_runs_lists_and_links_map() {
        let xml = r#"<abiword xmlns:xlink="http://www.w3.org/1999/xlink"><section>
            <p style="Title">Der Titel</p>
            <p style="heading 1">Abschnitt</p>
            <p>Mit <c props="font-weight:bold">fettem</c> und <c props="font-style:italic">kursivem</c> Text
               und einem <a xlink:href="https://example.org"><c>Link</c></a>.</p>
            <p listid="7" level="0" style="List Bullet"><field type="list_label"></field><c type="list_label">	Erster Punkt</c></p>
            <p listid="9" level="0" style="List Number"><field type="list_label"></field><c type="list_label">	Nummer eins</c></p>
            <p listid="9" level="0" style="List Number"><field type="list_label"></field><c type="list_label">	Nummer zwei</c></p>
        </section></abiword>"#;
        let md = convert(xml).export_to_markdown();
        assert!(md.contains("# Der Titel"), "{md}");
        assert!(md.contains("## Abschnitt"), "{md}");
        assert!(md.contains("**fettem**"), "{md}");
        assert!(md.contains("*kursivem*"), "{md}");
        assert!(md.contains("[Link](https://example.org)"), "{md}");
        assert!(md.contains("- Erster Punkt"), "label tab stripped:\n{md}");
        assert!(md.contains("1. Nummer eins"), "{md}");
        assert!(md.contains("2. Nummer zwei"), "per-list numbering:\n{md}");
    }

    #[test]
    fn attach_grid_tables_with_spans() {
        let xml = r#"<abiword><section><table>
            <cell props="left-attach:0; right-attach:2; top-attach:0; bot-attach:1"><p>Breit</p></cell>
            <cell props="left-attach:2; right-attach:3; top-attach:0; bot-attach:1"><p>C</p></cell>
            <cell props="left-attach:0; right-attach:1; top-attach:1; bot-attach:2"><p>a</p></cell>
            <cell props="left-attach:1; right-attach:2; top-attach:1; bot-attach:2"><p>b</p></cell>
            <cell props="left-attach:2; right-attach:3; top-attach:1; bot-attach:2"><p>c</p></cell>
        </table></section></abiword>"#;
        let doc = convert(xml);
        let Node::Table(t) = &doc.nodes[0] else {
            panic!("table expected");
        };
        assert_eq!(
            t.rows,
            vec![
                vec!["Breit".to_string(), "Breit".into(), "C".into()],
                vec!["a".to_string(), "b".into(), "c".into()],
            ],
            "span text replicated docling-style"
        );
    }

    #[test]
    fn zabw_gzip_unwraps_and_header_sections_drop() {
        use std::io::Write;
        let xml = r#"<abiword><section type="header"><p>Kopfzeile</p></section>
            <section><p>Nur der Inhalt.</p></section></abiword>"#;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(xml.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let source = SourceDocument::from_bytes("t.zabw".to_string(), InputFormat::Abiword, gz);
        let md = AbwBackend.convert(&source).unwrap().export_to_markdown();
        assert!(md.contains("Nur der Inhalt."), "{md}");
        assert!(!md.contains("Kopfzeile"), "furniture dropped:\n{md}");
    }
}
