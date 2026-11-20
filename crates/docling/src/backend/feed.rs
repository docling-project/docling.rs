//! RSS / Atom / RDF feeds (#678): the shape MarkItDown's `_rss_converter`
//! gives a feed — the feed's title as the title, its description (RSS,
//! RDF) or subtitle (Atom) as a paragraph, then per item a level-2
//! heading, a `Published on: …` (`pubDate`, `dc:date`) or `Updated on: …`
//! (Atom `updated`) line, and the item's body — RSS's `description` then
//! `content:encoded`, Atom's `summary` then `content`, RDF's `description`
//! — read through the HTML backend (a feed's body is HTML more often than
//! not, and plain text reads the same way), with its relative links and
//! image sources resolved against the item's `link` (or the feed's, or an
//! `xml:base`). Atom text constructs follow their `type`: `text` is a
//! paragraph, `html` the escaped markup it holds, `xhtml` the inline
//! `<div>`'s content; a binary type is skipped. Nothing is fetched: the
//! articles an item links to are out of scope.
//!
//! A `.rss` / `.atom` / `.rdf` file, the `application/rss+xml` /
//! `application/atom+xml` media types, and a bare `.xml` whose root is
//! `<rss>`, an Atom `<feed>` or an RSS 1.0 `<rdf:RDF>` come here
//! ([`looks_like_feed`], read by the `xml` route's dialect sniff). A file
//! that turns out not to be a feed is kept as one `xml` code block with a
//! warning, like any other XML docling does not read.

use docling_core::{DoclingDocument, Node};
use roxmltree::{Document, Node as XmlNode};

use crate::backend::images::{ImagePolicy, MapImageResolver};
use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
const RSS1_NS: &str = "http://purl.org/rss/1.0/";
const CONTENT_NS: &str = "http://purl.org/rss/1.0/modules/content/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// Whether the head of an XML file is a feed's: the first element is
/// `<rss>`, an Atom `<feed>` (the Atom namespace declared on it) or an
/// RSS 1.0 `<rdf:RDF>` (the RSS 1.0 namespace in the head).
pub(crate) fn looks_like_feed(head: &str) -> bool {
    let Some(tag) = first_element_tag(head) else {
        return false;
    };
    let name = tag
        .trim_start_matches('<')
        .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .next()
        .unwrap_or("");
    let local = name.rsplit(':').next().unwrap_or(name);
    match local {
        "rss" => true,
        "feed" => tag.contains(ATOM_NS),
        "RDF" => head.contains(RSS1_NS),
        _ => false,
    }
}

/// The first element's start tag (through its `>`), past the XML
/// declaration, comments, processing instructions and a DOCTYPE.
pub(crate) fn first_element_tag(head: &str) -> Option<&str> {
    let mut rest = head;
    loop {
        let at = rest.find('<')?;
        rest = &rest[at..];
        if rest.starts_with("<?") {
            let end = rest.find("?>")?;
            rest = &rest[end + 2..];
        } else if rest.starts_with("<!--") {
            let end = rest.find("-->")?;
            rest = &rest[end + 3..];
        } else if rest.starts_with("<!") {
            let end = rest.find('>')?;
            rest = &rest[end + 1..];
        } else {
            let end = rest.find('>')?;
            return Some(&rest[..=end]);
        }
    }
}

/// The feed reader (#678): see the module docs.
pub struct FeedBackend {
    pub images: ImagePolicy,
}

impl DeclarativeBackend for FeedBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let text = source.text()?;
        let dom = match Document::parse_with_options(
            &text,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        ) {
            Ok(dom) => dom,
            Err(e) => {
                return Err(ConversionError::Parse(format!(
                    "feed: not well-formed XML ({e})"
                )))
            }
        };
        let root = dom.root_element();
        let feed = match root.tag_name().name() {
            "rss" => root
                .children()
                .find(|n| n.has_tag_name("channel"))
                .map(|c| (Kind::Rss, c)),
            "feed" if root.tag_name().namespace() == Some(ATOM_NS) => Some((Kind::Atom, root)),
            "RDF" => root
                .children()
                .find(|n| n.is_element() && n.tag_name().name() == "channel")
                .map(|c| (Kind::Rdf, c)),
            _ => None,
        };
        let Some((kind, channel)) = feed else {
            // Not a feed after all: the generic XML reading, with its warning.
            return crate::backend::text::convert_xml_text(source);
        };
        let mut doc = DoclingDocument::new(&source.name);
        let resolver = MapImageResolver::new(Default::default(), &self.images);
        let base = xml_base(root)
            .or_else(|| child_text(channel, "link").filter(|l| l.starts_with("http")))
            .or_else(|| {
                channel
                    .children()
                    .filter(|n| n.has_tag_name("link"))
                    .find_map(|l| l.attribute("href").map(str::to_string))
            });
        if let Some(title) = child_text(channel, "title").map(|t| flatten(&t)) {
            if !title.is_empty() {
                doc.push(Node::Heading {
                    level: 1,
                    text: escape_text(&title),
                });
            }
        }
        let blurb = match kind {
            Kind::Atom => child_text(channel, "subtitle"),
            Kind::Rss | Kind::Rdf => child_text(channel, "description"),
        };
        if let Some(blurb) = blurb.map(|b| flatten(&b)).filter(|b| !b.is_empty()) {
            doc.push(Node::Paragraph {
                text: escape_text(&blurb),
            });
        }
        let items: Vec<XmlNode> = match kind {
            Kind::Rss => channel
                .children()
                .filter(|n| n.has_tag_name("item"))
                .collect(),
            Kind::Atom => channel
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "entry")
                .collect(),
            // RSS 1.0 lists the items beside the channel, not inside it.
            Kind::Rdf => root
                .children()
                .filter(|n| n.is_element() && n.tag_name().name() == "item")
                .collect(),
        };
        for item in items {
            let item_base = xml_base(item)
                .or_else(|| match kind {
                    Kind::Atom => item
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "link")
                        .find(|l| matches!(l.attribute("rel"), None | Some("alternate")))
                        .and_then(|l| l.attribute("href").map(str::to_string)),
                    Kind::Rss | Kind::Rdf => child_text(item, "link"),
                })
                .or_else(|| base.clone());
            if let Some(title) = child_text(item, "title").map(|t| flatten(&t)) {
                if !title.is_empty() {
                    doc.push(Node::Heading {
                        level: 2,
                        text: escape_text(&title),
                    });
                }
            }
            let date = match kind {
                Kind::Rss => child_text(item, "pubDate").map(|d| ("Published on", d)),
                Kind::Atom => child_text(item, "updated").map(|d| ("Updated on", d)),
                Kind::Rdf => ns_child_text(item, DC_NS, "date").map(|d| ("Published on", d)),
            };
            if let Some((label, date)) = date {
                let date = flatten(&date);
                if !date.is_empty() {
                    doc.push(Node::Paragraph {
                        text: escape_text(&format!("{label}: {date}")),
                    });
                }
            }
            let bodies: Vec<Body> = match kind {
                Kind::Rss => [
                    child_text(item, "description").map(Body::Html),
                    ns_child_text(item, CONTENT_NS, "encoded").map(Body::Html),
                ]
                .into_iter()
                .flatten()
                .collect(),
                Kind::Rdf => child_text(item, "description")
                    .map(Body::Html)
                    .into_iter()
                    .collect(),
                Kind::Atom => ["summary", "content"]
                    .iter()
                    .filter_map(|name| {
                        item.children()
                            .find(|n| n.is_element() && n.tag_name().name() == *name)
                    })
                    .filter_map(|n| atom_text(n, &text))
                    .collect(),
            };
            for body in bodies {
                match body {
                    Body::Text(t) => {
                        let t = t.trim();
                        if !t.is_empty() {
                            doc.push(Node::Paragraph {
                                text: escape_text(t),
                            });
                        }
                    }
                    Body::Html(html) => {
                        if html.trim().is_empty() {
                            continue;
                        }
                        let html = resolve_links(&html, item_base.as_deref());
                        let sub = crate::backend::html::convert_html_with(
                            "item", &html, &resolver, false,
                        );
                        doc.nodes.extend(sub.nodes);
                        doc.links.extend(sub.links);
                    }
                }
            }
        }
        Ok(doc)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Rss,
    Atom,
    Rdf,
}

enum Body {
    Text(String),
    Html(String),
}

/// An Atom text construct by its `type` ([RFC 4287] 3.1): `text` (the
/// default) is plain, `html` holds escaped markup, `xhtml` an inline
/// `<div>` whose content is taken from the source as written; a media
/// type of `content` that is markup reads as HTML, another text type as
/// plain text, anything else is skipped.
fn atom_text(node: XmlNode, source: &str) -> Option<Body> {
    let ty = node.attribute("type").unwrap_or("text").trim();
    match ty {
        "text" => Some(Body::Text(node.text().unwrap_or_default().to_string())),
        "html" => Some(Body::Html(node.text().unwrap_or_default().to_string())),
        "xhtml" => {
            let div = node.children().find(|n| n.is_element()).unwrap_or(node);
            let mut first = None;
            let mut last = None;
            for c in div.children() {
                let r = c.range();
                first.get_or_insert(r.start);
                last = Some(r.end);
            }
            let (Some(a), Some(b)) = (first, last) else {
                return None;
            };
            source.get(a..b).map(|s| Body::Html(s.to_string()))
        }
        t if t.ends_with("+xml") || t == "application/xml" || t == "text/html" => {
            Some(Body::Html(node.text().unwrap_or_default().to_string()))
        }
        t if t.starts_with("text/") => {
            Some(Body::Text(node.text().unwrap_or_default().to_string()))
        }
        _ => None,
    }
}

/// `xml:base` on `node`, when it is absolute.
fn xml_base(node: XmlNode) -> Option<String> {
    node.attribute((XML_NS, "base"))
        .filter(|b| b.contains("://"))
        .map(str::to_string)
}

/// The text of the first child element named `name` (any namespace).
fn child_text(node: XmlNode, name: &str) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .map(|n| n.text().unwrap_or_default().to_string())
}

fn ns_child_text(node: XmlNode, ns: &str, name: &str) -> Option<String> {
    node.children()
        .find(|n| {
            n.is_element() && n.tag_name().name() == name && n.tag_name().namespace() == Some(ns)
        })
        .map(|n| n.text().unwrap_or_default().to_string())
}

/// One line: every line trimmed, the empty ones dropped, joined with a
/// space — MarkItDown's `_get_text`.
fn flatten(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `dir` (a `/…/` path under `origin`) joined with the relative `value`,
/// its `.` and `..` segments folded (`/blog/2024/` + `../about` →
/// `/blog/about`); a query or fragment on `value` rides along.
fn join_dots(origin: &str, dir: &str, value: &str) -> String {
    let (path, tail) = match value.find(['?', '#']) {
        Some(i) => (&value[..i], &value[i..]),
        None => (value, ""),
    };
    let mut segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let trailing_slash = path.ends_with('/');
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut out = format!("{origin}/{}", segments.join("/"));
    if trailing_slash && !segments.is_empty() {
        out.push('/');
    }
    out.push_str(tail);
    out
}

/// Relative `href` / `src` attribute values of `html` joined to `base`
/// (an absolute `http(s)` URL): `/path` against its origin, `//host/…`
/// with its scheme, anything else against its directory. Absolute URLs,
/// fragments, `data:` / `mailto:` and the like are left alone, as is
/// everything without a base.
pub(crate) fn resolve_links(html: &str, base: Option<&str>) -> String {
    let Some(base) = base.filter(|b| b.starts_with("http://") || b.starts_with("https://")) else {
        return html.to_string();
    };
    let (scheme, after) = base.split_once("://").unwrap_or(("https", base));
    let (host, path) = after.split_once('/').unwrap_or((after, ""));
    let origin = format!("{scheme}://{host}");
    let dir = match path.rsplit_once('/') {
        Some((d, _)) => format!("{origin}/{d}/"),
        None => format!("{origin}/"),
    };
    let re = cached_regex!(r#"(?i)\b(href|src)\s*=\s*(["'])([^"']*)(["'])"#);
    re.replace_all(html, |caps: &regex::Captures| {
        let value = caps[3].trim();
        let resolved = if value.is_empty()
            || value.starts_with('#')
            || value.contains("://")
            || value.starts_with("data:")
            || value.starts_with("mailto:")
            || value.starts_with("tel:")
            || value.starts_with("javascript:")
        {
            value.to_string()
        } else if let Some(rest) = value.strip_prefix("//") {
            format!("{scheme}://{rest}")
        } else if let Some(rest) = value.strip_prefix('/') {
            format!("{origin}/{rest}")
        } else {
            join_dots(&origin, &dir[origin.len()..], value)
        };
        format!("{}={}{}{}", &caps[1], &caps[2], resolved, &caps[4])
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(name: &str, xml: &str) -> DoclingDocument {
        FeedBackend {
            images: ImagePolicy::default(),
        }
        .convert(&SourceDocument::from_bytes(
            name,
            InputFormat::Feed,
            xml.as_bytes().to_vec(),
        ))
        .unwrap()
    }

    #[test]
    fn rss_atom_and_rdf_heads_are_feeds() {
        assert!(looks_like_feed(
            "<?xml version=\"1.0\"?>\n<!-- c -->\n<rss version=\"2.0\"><channel>"
        ));
        assert!(looks_like_feed(
            "<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:lang=\"en\">"
        ));
        assert!(looks_like_feed(
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns=\"http://purl.org/rss/1.0/\">"
        ));
        assert!(
            !looks_like_feed("<feed><entry/></feed>"),
            "no Atom namespace"
        );
        assert!(!looks_like_feed("<article><front/></article>"));
        assert!(!looks_like_feed("<!DOCTYPE html><html><feed/></html>"));
    }

    #[test]
    fn an_rss_feed_renders_like_markitdown() {
        let doc = convert(
            "f.rss",
            r#"<?xml version="1.0"?><rss version="2.0" xmlns:content="http://purl.org/rss/1.0/modules/content/">
<channel><title> The
  Blog </title><link>https://example.com/blog/</link><description>News &amp; notes</description>
<item><title>First_post</title><link>https://example.com/blog/2024/first</link>
<pubDate>Mon, 09 Dec 2024 20:26:59 +0000</pubDate>
<description>&lt;p&gt;Hello &lt;a href="../about"&gt;us&lt;/a&gt; and &lt;img src="/i.png"&gt;&lt;/p&gt;</description>
<content:encoded><![CDATA[<p>Full <b>text</b></p><ul><li>one</li></ul>]]></content:encoded>
</item><item><title>Second</title><description>plain words only</description></item>
</channel></rss>"#,
        );
        let md = doc.export_to_markdown();
        // Text is Markdown-escaped like every backend's (`&` → `&amp;`);
        // the relative link resolves against the item's link, its `..`
        // folded; an image the policy does not fetch is a placeholder, a
        // paragraph's image too since #1177 (it was dropped).
        assert_eq!(
            md,
            "# The Blog\n\nNews &amp; notes\n\n## First\\_post\n\nPublished on: Mon, 09 Dec 2024 20:26:59 +0000\n\nHello [us](https://example.com/blog/about) and\n\n<!-- image -->\n\nFull **text**\n\n- one\n\n## Second\n\nplain words only\n"
        );
        let json = doc.export_to_json();
        assert!(json.contains("\"label\": \"title\""), "{json}");
        assert!(!json.contains("<rss"), "{json}");
    }

    #[test]
    fn an_atom_feed_reads_its_text_constructs() {
        let doc = convert(
            "f.atom",
            r#"<feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://example.org/">
<title>Atom Site</title><subtitle>what we write</subtitle><link href="https://example.org/"/>
<entry><title>Plain</title><updated>2024-01-02T03:04:05Z</updated><link rel="alternate" href="https://example.org/p/1"/>
  <summary>a &lt;summary&gt; as text</summary>
  <content type="html">&lt;p&gt;html &lt;em&gt;body&lt;/em&gt;&lt;/p&gt;</content></entry>
<entry><title>Markup</title>
  <content type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>inline <strong>xhtml</strong></p><p>second</p></div></content></entry>
<entry><title>Binary</title><content type="image/png">iVBORw0KGgo=</content></entry>
</feed>"#,
        );
        assert_eq!(
            doc.export_to_markdown(),
            "# Atom Site\n\nwhat we write\n\n## Plain\n\nUpdated on: 2024-01-02T03:04:05Z\n\na &lt;summary&gt; as text\n\nhtml *body*\n\n## Markup\n\ninline **xhtml**\n\nsecond\n\n## Binary\n"
        );
    }

    #[test]
    fn an_rdf_feed_and_a_non_feed_xml() {
        let doc = convert(
            "f.rdf",
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns="http://purl.org/rss/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/">
<channel rdf:about="https://example.net/"><title>Advisories</title><description>security notes</description></channel>
<item rdf:about="https://example.net/a1"><title>DSA-1</title><link>https://example.net/a1</link><dc:date>2024-05-06</dc:date><description>Fixed &lt;b&gt;now&lt;/b&gt;</description></item>
</rdf:RDF>"#,
        );
        assert_eq!(
            doc.export_to_markdown(),
            "# Advisories\n\nsecurity notes\n\n## DSA-1\n\nPublished on: 2024-05-06\n\nFixed **now**\n"
        );
        let doc = convert("x.rss", "<root><a>1</a></root>");
        let md = doc.export_to_markdown();
        assert!(
            md.starts_with("```") && md.contains("<root><a>1</a></root>"),
            "{md}"
        );
    }

    #[test]
    fn relative_links_resolve_against_the_base() {
        let html = r##"<a href="../a">x</a><img src='/i.png'><a href="//cdn.x/y">y</a><a href="#top">t</a><a href="https://z/">z</a><a href="mailto:a@b">m</a>"##;
        assert_eq!(
            resolve_links(html, Some("https://example.com/blog/2024/post")),
            r##"<a href="https://example.com/blog/a">x</a><img src='https://example.com/i.png'><a href="https://cdn.x/y">y</a><a href="#top">t</a><a href="https://z/">z</a><a href="mailto:a@b">m</a>"##
        );
        assert_eq!(resolve_links(html, None), html);
        assert_eq!(resolve_links(html, Some("not a url")), html);
    }
}
