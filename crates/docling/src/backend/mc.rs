//! OOXML markup compatibility (ECMA-376 Part 3): `mc:AlternateContent`.
//!
//! Word writes a feature newer than the base schema twice: an `mc:Choice
//! Requires="wps"` holding the modern markup (a DrawingML text box, a 2010
//! shape group, a `w14` text effect) and an `mc:Fallback` holding what an
//! older reader shows instead (the same text box as VML, a picture of the
//! shape). A reader is to take the first `Choice` whose required namespaces it
//! understands, else the `Fallback`, and never both.
//!
//! The DOCX backends used to see both branches: a body-level block was skipped
//! outright (as docling did until 2.142 read its selected branch,
//! docling#4615), and a text box arrived twice — once from the
//! `wps` drawing, once from the VML fallback — which a text-keyed dedup then
//! folded, dropping a paragraph legitimately repeated inside one box along
//! with the copy (#572). [`resolve_alternate_content`] settles the choice
//! before the XML is parsed into a tree: every `mc:AlternateContent` is
//! replaced by the content of its selected branch, so the two backends, with
//! their many `descendants()` scans, read one document with no alternatives
//! in it and need no dedup. A deliberate divergence from docling — Word, its
//! COM mirror of the same file as `.doc`, and anydoc all keep the content.

use std::borrow::Cow;

use roxmltree::{Document, Node};

/// The markup-compatibility namespace of `mc:AlternateContent` and friends.
pub(super) const MC_NS: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";

/// The namespaces whose markup the DOCX backends read, so a `Choice`
/// requiring them is taken over the fallback: docling's own
/// `_OOXML_NAMESPACES` (2.142's `_select_alternate_content_branch`,
/// docling#4615 — the base WordprocessingML, DrawingML, chart and
/// relationship schemas, `wp` anchors, VML and its `w10` wrap, `wps` text
/// boxes, the `w14` and `a14` extensions: the same runs and pictures with
/// extra properties, `w14:checkbox` is read), plus 2010 shape groups and
/// canvases and the 2010 drawing extensions, whose text boxes and pictures
/// both walks read too (docling takes their fallback — the same content, as
/// VML or a picture). Anything else — ink (`wpi`), chartex (`cx`), the
/// `w15`/`w16*` extensions — takes the fallback Word wrote for readers
/// without it.
const SUPPORTED: &[&str] = &[
    "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    "http://schemas.openxmlformats.org/drawingml/2006/main",
    "http://schemas.openxmlformats.org/drawingml/2006/chart",
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships",
    "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing",
    MC_NS,
    "urn:schemas-microsoft-com:vml",
    "urn:schemas-microsoft-com:office:word",
    "http://schemas.microsoft.com/office/word/2010/wordprocessingShape",
    "http://schemas.microsoft.com/office/word/2010/wordprocessingGroup",
    "http://schemas.microsoft.com/office/word/2010/wordprocessingCanvas",
    "http://schemas.microsoft.com/office/word/2010/wordprocessingDrawing",
    "http://schemas.microsoft.com/office/word/2010/wordml",
    "http://schemas.microsoft.com/office/drawing/2010/main",
];

/// Nesting the loop tolerates: a selected branch may itself hold an
/// `mc:AlternateContent` (a text box inside a shape group), each pass settles
/// one level, and real documents nest two or three deep.
const MAX_DEPTH: usize = 8;

/// `xml` with every `mc:AlternateContent` replaced by the content of its
/// selected branch — the first `mc:Choice` whose `Requires` prefixes all
/// resolve to a [`SUPPORTED`] namespace, else the `mc:Fallback`, else nothing.
/// Namespace declarations the dropped wrapper elements carried are
/// re-declared on the kept content's top-level elements, so it parses as it
/// did. XML that does not parse, or has no alternatives, is returned as is.
pub(super) fn resolve_alternate_content(xml: &str) -> Cow<'_, str> {
    if !xml.contains("AlternateContent") {
        return Cow::Borrowed(xml);
    }
    let mut current = Cow::Borrowed(xml);
    for _ in 0..MAX_DEPTH {
        let Ok(doc) = Document::parse(&current) else {
            return current;
        };
        let edits = outermost_edits(&doc);
        if edits.is_empty() {
            return current;
        }
        let mut out = String::with_capacity(current.len());
        let mut pos = 0;
        for (range, replacement) in edits {
            out.push_str(&current[pos..range.start]);
            out.push_str(&replacement);
            pos = range.end;
        }
        out.push_str(&current[pos..]);
        current = Cow::Owned(out);
    }
    current
}

/// The attribute [`resolve_chartex_frames`] puts on a chartex frame: the
/// relationship id of the picture its `mc:Fallback` held (no namespace: it
/// cannot collide with OOXML's).
pub(super) const CHARTEX_FALLBACK_ATTR: &str = "docling-chartex-fallback";

/// PowerPoint's Office 2016 charts (docling#5371, #1165): every
/// `mc:AlternateContent` whose `mc:Choice` holds a `p:graphicFrame` with a
/// chartex `cx:chart` is replaced by that frame — the chart is read
/// (`pptx_chartex`), where [`resolve_alternate_content`] would take the
/// `Fallback` (`cx1` is not [`SUPPORTED`]) — and the picture the Fallback
/// held, if any, is noted on the frame as [`CHARTEX_FALLBACK_ATTR`], to be
/// the chart's image rather than a second picture. docling's
/// `_with_chartex_frames` / `_chartex_fallback_image`; PPTX only, as
/// upstream changed only its PowerPoint backend. Run before
/// [`resolve_alternate_content`]; anything else is returned as is.
pub(super) fn resolve_chartex_frames(xml: &str) -> Cow<'_, str> {
    let chartex = super::pptx_chartex::CHARTEX_NS;
    if !xml.contains(chartex) || !xml.contains("AlternateContent") {
        return Cow::Borrowed(xml);
    }
    let Ok(doc) = Document::parse(xml) else {
        return Cow::Borrowed(xml);
    };
    let mc = |n: &Node, name: &str| n.is_element() && n.tag_name() == (MC_NS, name).into();
    let is_alt = |n: Node| mc(&n, "AlternateContent");
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for alt in doc
        .descendants()
        .filter(|n| is_alt(*n) && !n.ancestors().skip(1).any(is_alt))
    {
        let frame = alt
            .children()
            .filter(|c| mc(c, "Choice"))
            .find_map(|choice| {
                choice.children().find(|f| {
                    f.is_element()
                        && f.tag_name().name() == "graphicFrame"
                        && f.descendants().any(|n| {
                            n.is_element()
                                && n.tag_name().name() == "chart"
                                && n.tag_name().namespace() == Some(chartex)
                        })
                })
            });
        let Some(frame) = frame else {
            continue;
        };
        let local = |n: &Node, name: &str| n.is_element() && n.tag_name().name() == name;
        let embed = alt
            .children()
            .find(|c| mc(c, "Fallback"))
            .and_then(|fb| fb.children().find(|c| local(c, "pic")))
            .and_then(|pic| pic.children().find(|c| local(c, "blipFill")))
            .and_then(|fill| fill.children().find(|c| local(c, "blip")))
            .and_then(|blip| {
                blip.attributes()
                    .find(|a| a.name() == "embed")
                    .map(|a| a.value().to_string())
            });
        let src = doc.input_text();
        let outside = alt.parent().unwrap_or_else(|| doc.root());
        let mut extra: String = frame
            .namespaces()
            .filter(|ns| outside.lookup_namespace_uri(ns.name()) != Some(ns.uri()))
            .map(|ns| match ns.name() {
                Some(p) => format!(" xmlns:{p}=\"{}\"", ns.uri()),
                None => format!(" xmlns=\"{}\"", ns.uri()),
            })
            .collect();
        if let Some(rid) = embed {
            extra.push_str(&format!(" {CHARTEX_FALLBACK_ATTR}=\"{rid}\""));
        }
        let range = frame.range();
        let at = qname_end(src, range.start);
        let text = format!("{}{extra}{}", &src[range.start..at], &src[at..range.end]);
        edits.push((alt.range(), text));
    }
    if edits.is_empty() {
        return Cow::Borrowed(xml);
    }
    let mut out = String::with_capacity(xml.len());
    let mut pos = 0;
    for (range, replacement) in edits {
        out.push_str(&xml[pos..range.start]);
        out.push_str(&replacement);
        pos = range.end;
    }
    out.push_str(&xml[pos..]);
    Cow::Owned(out)
}

/// `(range of the mc:AlternateContent element, its replacement text)` for
/// every alternative not nested in another, in document order.
fn outermost_edits(doc: &Document) -> Vec<(std::ops::Range<usize>, String)> {
    let is_alt = |n: Node| n.is_element() && n.tag_name() == (MC_NS, "AlternateContent").into();
    doc.descendants()
        .filter(|n| is_alt(*n) && !n.ancestors().skip(1).any(is_alt))
        .map(|alt| (alt.range(), branch_text(doc, alt)))
        .collect()
}

/// The selected branch's inner markup, with the namespace declarations of
/// the wrapper elements hoisted onto its top-level elements.
fn branch_text(doc: &Document, alt: Node) -> String {
    let Some(branch) = select_branch(alt) else {
        return String::new();
    };
    let src = doc.input_text();
    let (Some(first), Some(last)) = (branch.first_child(), branch.last_child()) else {
        return String::new();
    };
    let inner = first.range().start..last.range().end;
    // Declarations in scope inside the branch but not outside the
    // alternative: those lived on the wrapper elements being removed.
    let outside = alt.parent().unwrap_or_else(|| doc.root());
    let hoisted: Vec<String> = branch
        .namespaces()
        .filter(|ns| outside.lookup_namespace_uri(ns.name()) != Some(ns.uri()))
        .map(|ns| match ns.name() {
            Some(p) => format!(" xmlns:{p}=\"{}\"", ns.uri()),
            None => format!(" xmlns=\"{}\"", ns.uri()),
        })
        .collect();
    if hoisted.is_empty() {
        return src[inner].to_string();
    }
    // Insert after each top-level element's qualified name, back to front so
    // the earlier offsets stay valid.
    let mut text = src[inner.clone()].to_string();
    let decls = hoisted.concat();
    let mut tops: Vec<Node> = branch.children().filter(Node::is_element).collect();
    tops.reverse();
    for el in tops {
        let at = qname_end(src, el.range().start) - inner.start;
        text.insert_str(at, &decls);
    }
    text
}

/// The offset just past an element's qualified name, `start` being its `<`:
/// where an attribute may be inserted.
fn qname_end(src: &str, start: usize) -> usize {
    let tag = &src[start + 1..];
    let len = tag
        .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
        .unwrap_or(tag.len());
    start + 1 + len
}

/// The branch a reader with [`SUPPORTED`] takes: the first `mc:Choice` whose
/// `Requires` prefixes (resolved in the choice's own scope — an unknown
/// prefix matches nothing) are all supported, else the `mc:Fallback`.
fn select_branch<'a, 'i>(alt: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    let mc = |n: &Node, name: &str| n.is_element() && n.tag_name() == (MC_NS, name).into();
    alt.children()
        .filter(|n| mc(n, "Choice"))
        .find(|choice| {
            choice
                .attribute((MC_NS, "Requires"))
                .or_else(|| choice.attribute("Requires"))
                .is_none_or(|req| {
                    req.split_whitespace().all(|prefix| {
                        choice
                            .lookup_namespace_uri(Some(prefix))
                            .is_some_and(|uri| SUPPORTED.contains(&uri))
                    })
                })
        })
        .or_else(|| alt.children().find(|n| mc(n, "Fallback")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:wpi="http://schemas.microsoft.com/office/word/2010/wordprocessingInk" mc:Ignorable="wps wpi"><w:body>"#;
    const TAIL: &str = "</w:body></w:document>";

    fn paragraphs(xml: &str) -> Vec<String> {
        let doc = Document::parse(xml).unwrap();
        doc.descendants()
            .filter(|n| n.has_tag_name("t"))
            .map(|t| t.text().unwrap_or("").to_string())
            .collect()
    }

    /// docling#4615: a Choice requiring a namespace of docling's own
    /// `_OOXML_NAMESPACES` (VML here) is the branch read, as docling 2.142
    /// reads it; one requiring another (ink) yields its Fallback.
    #[test]
    fn choice_requiring_doclings_namespaces_is_read() {
        let head = HEAD.replace(
            "mc:Ignorable",
            r#"xmlns:v="urn:schemas-microsoft-com:vml" mc:Ignorable"#,
        );
        let alt = |req: &str| {
            format!(
                "{head}<mc:AlternateContent><mc:Choice Requires=\"{req}\">\
                 <w:p><w:r><w:t>choice</w:t></w:r></w:p></mc:Choice>\
                 <mc:Fallback><w:p><w:r><w:t>fallback</w:t></w:r></w:p></mc:Fallback>\
                 </mc:AlternateContent>{TAIL}"
            )
        };
        assert_eq!(
            paragraphs(&resolve_alternate_content(&alt("v"))),
            ["choice"]
        );
        assert_eq!(
            paragraphs(&resolve_alternate_content(&alt("v wpi"))),
            ["fallback"]
        );
    }

    #[test]
    fn supported_choice_wins_and_fallback_is_gone() {
        let xml = format!(
            "{HEAD}<w:p><w:r><w:t>before</w:t></w:r></w:p>\
             <mc:AlternateContent><mc:Choice Requires=\"wps\">\
             <w:p><w:r><w:t>choice</w:t></w:r></w:p><w:p><w:r><w:t>twice</w:t></w:r></w:p>\
             <w:p><w:r><w:t>twice</w:t></w:r></w:p></mc:Choice>\
             <mc:Fallback><w:p><w:r><w:t>fallback</w:t></w:r></w:p></mc:Fallback>\
             </mc:AlternateContent><w:p><w:r><w:t>after</w:t></w:r></w:p>{TAIL}"
        );
        let out = resolve_alternate_content(&xml);
        assert!(!out.contains("AlternateContent") && !out.contains("fallback"));
        assert_eq!(
            paragraphs(&out),
            ["before", "choice", "twice", "twice", "after"]
        );
        // The body's children are now the paragraphs themselves.
        let doc = Document::parse(&out).unwrap();
        let body = doc.descendants().find(|n| n.has_tag_name("body")).unwrap();
        assert_eq!(body.children().filter(Node::is_element).count(), 5);
    }

    #[test]
    fn unsupported_requirement_takes_the_fallback() {
        let xml = format!(
            "{HEAD}<mc:AlternateContent><mc:Choice Requires=\"wpi\">\
             <w:p><w:r><w:t>ink</w:t></w:r></w:p></mc:Choice>\
             <mc:Choice Requires=\"wps unknown\"><w:p><w:r><w:t>partly</w:t></w:r></w:p></mc:Choice>\
             <mc:Fallback><w:p><w:r><w:t>picture</w:t></w:r></w:p></mc:Fallback>\
             </mc:AlternateContent>{TAIL}"
        );
        assert_eq!(paragraphs(&resolve_alternate_content(&xml)), ["picture"]);
        // No fallback either: the alternative contributes nothing.
        let xml = format!(
            "{HEAD}<mc:AlternateContent><mc:Choice Requires=\"wpi\">\
             <w:p><w:r><w:t>ink</w:t></w:r></w:p></mc:Choice></mc:AlternateContent>\
             <w:p><w:r><w:t>kept</w:t></w:r></w:p>{TAIL}"
        );
        assert_eq!(paragraphs(&resolve_alternate_content(&xml)), ["kept"]);
    }

    #[test]
    fn nested_alternatives_and_local_namespaces_survive() {
        // A choice declaring a prefix its content uses: the declaration moves
        // onto the kept element. The inner alternative settles on the next pass.
        let xml = format!(
            "{HEAD}<w:p><w:r><mc:AlternateContent xmlns:x=\"urn:x\">\
             <mc:Choice Requires=\"wps\" xmlns:y=\"urn:y\"><x:a y:k=\"1\"><w:t>outer</w:t>\
             <mc:AlternateContent><mc:Choice Requires=\"wps\"><w:t>inner</w:t></mc:Choice>\
             <mc:Fallback><w:t>no</w:t></mc:Fallback></mc:AlternateContent></x:a></mc:Choice>\
             <mc:Fallback><w:t>no</w:t></mc:Fallback></mc:AlternateContent></w:r></w:p>{TAIL}"
        );
        let out = resolve_alternate_content(&xml);
        assert!(!out.contains("AlternateContent"), "{out}");
        assert_eq!(paragraphs(&out), ["outer", "inner"]);
        let doc = Document::parse(&out).unwrap();
        let a = doc.descendants().find(|n| n.has_tag_name("a")).unwrap();
        assert_eq!(a.tag_name().namespace(), Some("urn:x"));
        assert_eq!(a.attribute(("urn:y", "k")), Some("1"));
    }

    #[test]
    fn untouched_without_alternatives() {
        let xml = format!("{HEAD}<w:p/>{TAIL}");
        assert!(matches!(resolve_alternate_content(&xml), Cow::Borrowed(_)));
        assert!(matches!(
            resolve_alternate_content("<a><b"),
            Cow::Borrowed(_)
        ));
    }
}
