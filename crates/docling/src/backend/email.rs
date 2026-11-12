//! Email (`.eml` / Outlook `.msg`) backend — a port of docling's
//! `EmailDocumentBackend` (#251 for `.msg`, docling#3873).
//!
//! The subject becomes the document title; `From:`/`To:`/`Date:` headers become
//! text paragraphs; the body (preferring `text/plain`) is split into paragraphs
//! on blank lines. All emitted text is HTML/underscore-escaped like docling-core
//! (so `<a@b>` renders as `&lt;a@b&gt;`). A CFB-magic input is an Outlook
//! `.msg`: it projects onto RFC 822 (see [`super::msg`]) and flows through the
//! **same** parse below, so `.msg` and `.eml` output match by construction —
//! docling's own architecture for the format. `list_attachments` (opt-in,
//! docling's `EmailBackendOptions.list_attachments`) appends an `Attachments`
//! section listing names and content types; payload bytes are never embedded.
//!
//! Inline images (#646): docling flattens an HTML body to Markdown and
//! splits it into paragraphs, which loses the pictures pasted into it (their
//! `cid:` references name image parts of the same message). Under
//! [`ImageSources::Embedded`](crate::ImageSources::Embedded) and above, when an HTML body references
//! images the message carries — `Content-ID` parts of an `.eml`,
//! PR_ATTACH_CONTENT_ID attachments of a `.msg`, or `data:` URIs — the body
//! is appended as the HTML backend's nodes instead (headings, lists, tables
//! and the pictures in body order). A body without a resolvable image, and
//! every conversion at the default tier, takes docling's paragraph path, so
//! the default output is unchanged.

use std::collections::HashMap;

use mail_parser::{Address, Message, MessageParser, MimeHeaders};

use crate::backend::images::{build_picture, normalize_cid, ImagePolicy, MapImageResolver};
use crate::backend::markdown::escape_text;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;
use docling_core::{DoclingDocument, Node, PictureImage};

pub struct EmailBackend {
    /// Append an `Attachments` section (names + content types, never the
    /// payload) — docling's opt-in `list_attachments`.
    pub list_attachments: bool,
    /// Which images of an HTML body resolve (#646): the message's own
    /// `cid:` parts and `data:` URIs under `Embedded` and above; none by
    /// default (see the module docs).
    pub images: ImagePolicy,
}

impl DeclarativeBackend for EmailBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        // Outlook .msg (CFB magic): project to RFC 822 first; the projection
        // also carries the attachment labels straight from MAPI.
        let projected = crate::backend::cfb::CompoundFile::detect(&source.bytes)
            .then(|| super::msg::project(&source.bytes))
            .flatten();
        let raw: &[u8] = projected.as_ref().map_or(&source.bytes, |p| &p.rfc822);
        let msg = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| ConversionError::Parse("email: could not parse message".into()))?;
        let mut doc = DoclingDocument::new(&source.name);

        if let Some(subject) = msg.subject().map(header_safe).filter(|s| !s.is_empty()) {
            doc.push(Node::Heading {
                level: 1,
                text: escape_text(&subject),
            });
        }
        for (label, addrs) in [("From", msg.from()), ("To", msg.to())] {
            let text = format_addresses(addrs);
            if !text.is_empty() {
                doc.push(Node::Paragraph {
                    text: escape_text(&format!("{label}: {text}")),
                });
            }
        }
        if let Some(date) = msg.date() {
            // Python docling formats via datetime.isoformat(), which spells
            // UTC as "+00:00" — mail-parser's RFC 3339 uses "Z". Align.
            let date = date.to_rfc3339().replace('Z', "+00:00");
            doc.push(Node::Paragraph {
                text: escape_text(&format!("Date: {date}")),
            });
        }
        match self.body_with_images(source, &msg, projected.as_ref()) {
            Some(nodes) => doc.nodes.extend(nodes),
            None => {
                for para in body_paragraphs(&msg) {
                    doc.push(Node::Paragraph {
                        text: escape_text(&para),
                    });
                }
            }
        }
        if self.list_attachments {
            let labels = match &projected {
                Some(p) => p.attachment_labels.clone(),
                None => eml_attachment_labels(&msg),
            };
            if !labels.is_empty() {
                // docling adds the heading at level 2 under the title → "###".
                doc.push(Node::Heading {
                    level: 3,
                    text: "Attachments".into(),
                });
                for label in labels {
                    doc.push(Node::ListItem {
                        ordered: false,
                        number: 1,
                        first_in_list: false,
                        text: escape_text(&label),
                        level: 0,
                        marker: None,
                        location: None,
                        split: None,
                        dclx: None,
                        href: None,
                        layer: None,
                    });
                }
            }
        }
        Ok(doc)
    }
}

impl EmailBackend {
    /// The HTML body as nodes with its pictures (#646), when the tier allows
    /// container parts and a body references an image the message carries;
    /// `None` takes the paragraph path. An `.eml`'s images are its
    /// `Content-ID` parts (every image part, wherever it nests); a `.msg`'s
    /// are its attachments with a PR_ATTACH_CONTENT_ID, keyed the way the
    /// body spells them (`cid:` + the id, brackets and case normalized).
    fn body_with_images(
        &self,
        source: &SourceDocument,
        msg: &Message,
        projected: Option<&super::msg::ProjectedMsg>,
    ) -> Option<Vec<Node>> {
        if !self.images.sources.embedded() {
            return None;
        }
        let (bodies, cids): (Vec<String>, HashMap<String, PictureImage>) = match projected {
            Some(p) => {
                let html = p
                    .html_body
                    .as_deref()
                    .map(|b| super::html::decode_html_bytes(b).into_owned())
                    .filter(|h| !h.trim().is_empty());
                (html.into_iter().collect(), msg_cid_images(&source.bytes))
            }
            None => (
                (0..msg.html_body_count())
                    .filter_map(|i| msg.body_html(i).map(|h| h.into_owned()))
                    .collect(),
                eml_cid_images(msg),
            ),
        };
        for html in bodies {
            let resolver = MapImageResolver::new(cids.clone(), &self.images);
            let html_doc = super::html::convert_html("email-body.html", &html, &resolver);
            if html_doc
                .nodes
                .iter()
                .any(|n| matches!(n, Node::Picture { image: Some(_), .. }))
            {
                return Some(html_doc.nodes);
            }
        }
        None
    }
}

/// An `.eml`'s image parts by normalized `cid:` key (#646).
fn eml_cid_images(msg: &Message) -> HashMap<String, PictureImage> {
    let mut out = HashMap::new();
    for part in &msg.parts {
        if part.is_multipart() {
            continue;
        }
        let Some(ct) = part.content_type() else {
            continue;
        };
        if !ct.ctype().eq_ignore_ascii_case("image") {
            continue;
        }
        let Some(id) = part.content_id() else {
            continue;
        };
        let mimetype =
            format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("")).to_ascii_lowercase();
        if let Some(pic) = build_picture(mimetype, part.contents().to_vec()) {
            out.entry(normalize_cid(id)).or_insert(pic);
        }
    }
    out
}

/// A `.msg`'s inline image attachments by normalized `cid:` key (#646):
/// a by-value attachment with a PR_ATTACH_CONTENT_ID whose bytes decode as
/// a raster (the MIME tag names the type, else the file name's extension).
fn msg_cid_images(bytes: &[u8]) -> HashMap<String, PictureImage> {
    let mut out = HashMap::new();
    for att in super::msg::attachments(bytes).unwrap_or_default() {
        let (Some(id), Some(payload)) = (att.content_id.as_deref(), att.payload) else {
            continue;
        };
        let pic = match att.mime.as_deref().filter(|m| m.starts_with("image/")) {
            Some(mime) => build_picture(mime.to_ascii_lowercase(), payload),
            None => super::ooxml::picture_image(att.name.as_deref().unwrap_or(""), payload),
        };
        if let Some(pic) = pic {
            out.entry(normalize_cid(id)).or_insert(pic);
        }
    }
    out
}

/// Attachment display labels for a parsed `.eml`: `name (type/subtype)`,
/// falling back to `attachment-N` for nameless parts — docling's
/// `_get_attachment_labels`.
fn eml_attachment_labels(msg: &Message) -> Vec<String> {
    use mail_parser::MimeHeaders;
    msg.attachments()
        .enumerate()
        .map(|(i, part)| {
            // Header-derived, so single-line like every header (docling#4242).
            let name = part
                .attachment_name()
                .map(header_safe)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("attachment-{}", i + 1));
            match part.content_type() {
                Some(ct) => {
                    let mime = match ct.subtype() {
                        Some(sub) => format!("{}/{sub}", ct.ctype()),
                        None => ct.ctype().to_string(),
                    };
                    format!("{name} ({})", header_safe(&mime))
                }
                None => name,
            }
        })
        .collect()
}

/// docling's `_header_safe` (2.128, docling#4242): a header value collapsed
/// to one line of single-spaced text — `" ".join(value.split())`, which
/// covers every line terminator `str.splitlines()` knows and unfolds a
/// folded header to one space — so a decoded RFC 2047 encoded-word cannot
/// stand up a forged header line in the rendered document.
fn header_safe(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// docling's `_quote_display_name`: a rendered display name holding an RFC
/// 5322 special (`()<>[]:;@,\"` — `.` excepted, it parses back unquoted and
/// is common in real names) is quoted, with `\` and `"` escaped, so
/// `Name <email>` parses back as itself; a non-ASCII name stays readable
/// text (where `formataddr` would RFC 2047-encode it).
fn quote_display_name(name: &str) -> String {
    if name.chars().any(|c| {
        matches!(
            c,
            '(' | ')' | '<' | '>' | '[' | ']' | ':' | ';' | '@' | ',' | '"' | '\\'
        )
    }) {
        let escaped: String = name
            .chars()
            .flat_map(|c| {
                if matches!(c, '"' | '\\') {
                    vec!['\\', c]
                } else {
                    vec![c]
                }
            })
            .collect();
        format!("\"{escaped}\"")
    } else {
        name.to_string()
    }
}

/// `"Name <email>"` per address (or bare `email`), joined with `", "`.
fn format_addresses(addr: Option<&Address>) -> String {
    let Some(addr) = addr else {
        return String::new();
    };
    addr.iter()
        .filter_map(|a| {
            let name = a.name().map(header_safe).filter(|s| !s.is_empty());
            let email = a.address().map(str::trim).filter(|s| !s.is_empty());
            match (name, email) {
                (Some(n), Some(e)) => Some(format!("{} <{e}>", quote_display_name(&n))),
                (None, Some(e)) => Some(e.to_string()),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Body paragraphs (split on blank lines), preferring `text/plain`. CRLF and
/// a lone CR are normalised to `\n` first (docling#4248, 2.129), so a
/// CR-only body still splits into paragraphs.
fn body_paragraphs(msg: &Message) -> Vec<String> {
    let re = cached_regex!(r"\n\s*\n+");
    let split = |text: &str, out: &mut Vec<String>| {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        for p in re.split(text.trim()) {
            let p = p.trim();
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
    };
    let mut out = Vec::new();
    for i in 0..msg.text_body_count() {
        if let Some(t) = msg.body_text(i) {
            split(&t, &mut out);
        }
    }
    // A present `text/plain` part is not necessarily a body: many senders
    // emit a blank one beside the real `text/html` in a multipart/alternative
    // message, so only a part that produced text wins (docling#4295).
    if !out.is_empty() {
        return out;
    }
    // No plain text — the `text/html` part(s), converted through the HTML
    // backend and split into paragraphs (docling's `_convert_html_part` +
    // `_split_paragraphs` over its Markdown).
    for i in 0..msg.html_body_count() {
        if let Some(t) = msg.body_html(i) {
            let html_doc =
                super::html::convert_html("email-body.html", &t, &super::images::NoFetch);
            split(&html_doc.export_to_markdown(), &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    #[test]
    fn title_headers_escaped_and_body_split() {
        let eml = "From: Alice <a@x.com>\r\nTo: Bob <b@y.com>\r\nSubject: Hi\r\n\
                   Content-Type: text/plain\r\n\r\nLine one.\r\n\r\nLine two.\r\n";
        let src = SourceDocument::from_bytes("m", InputFormat::Email, eml.as_bytes().to_vec());
        let md = EmailBackend {
            list_attachments: false,
            images: ImagePolicy::default(),
        }
        .convert(&src)
        .unwrap()
        .export_to_markdown();
        // angle brackets HTML-escaped; body split into separate paragraphs.
        assert_eq!(
            md.trim(),
            "# Hi\n\nFrom: Alice &lt;a@x.com&gt;\n\nTo: Bob &lt;b@y.com&gt;\n\nLine one.\n\nLine two."
        );
    }

    /// #251: `list_attachments` appends the section with `name (type)` labels
    /// for `.eml` too — the docling label format, payload never embedded.
    /// docling#4242/#4248 (2.128–2.129): a display name with RFC 5322 specials
    /// is quoted (with `"` and `\` escaped), header values are collapsed to
    /// one line, and a CR-only body still splits into paragraphs.
    #[test]
    fn display_names_and_bodies_follow_docling_2129() {
        assert_eq!(quote_display_name("Doe, John"), "\"Doe, John\"");
        assert_eq!(quote_display_name("Ann \"Q\" Lee"), "\"Ann \\\"Q\\\" Lee\"");
        assert_eq!(quote_display_name("J. R. Hartley"), "J. R. Hartley");
        assert_eq!(quote_display_name("Zoë Müller"), "Zoë Müller");
        assert_eq!(header_safe("  two\r\n  lines  "), "two lines");
        let eml = "From: \"Doe, John\" <j@x.com>\r\nTo: b@y.com\r\nSubject: Re:\r\n folded\r\n\
                   Content-Type: text/plain\r\n\r\nOne.\r\rTwo.\r";
        let src = SourceDocument::from_bytes("m.eml", InputFormat::Email, eml.as_bytes().to_vec());
        let md = EmailBackend {
            list_attachments: false,
            images: ImagePolicy::default(),
        }
        .convert(&src)
        .unwrap()
        .export_to_markdown();
        assert!(
            md.starts_with("# Re: folded\n\nFrom: \"Doe, John\" &lt;j@x.com&gt;\n"),
            "{md}"
        );
        assert!(md.ends_with("One.\n\nTwo.\n"), "{md}");
    }

    #[test]
    fn eml_list_attachments_appends_labels() {
        let eml = concat!(
            "From: A <a@x.com>\r\n",
            "To: B <b@y.com>\r\n",
            "Subject: S\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"bb\"\r\n\r\n",
            "--bb\r\nContent-Type: text/plain\r\n\r\nBody.\r\n",
            "--bb\r\nContent-Type: text/plain\r\n",
            "Content-Disposition: attachment; filename=\"note.txt\"\r\n\r\n",
            "hi\r\n--bb--\r\n",
        );
        let src = SourceDocument::from_bytes("m", crate::InputFormat::Email, eml.into());
        let md = EmailBackend {
            list_attachments: true,
            images: ImagePolicy::default(),
        }
        .convert(&src)
        .unwrap()
        .export_to_markdown();
        assert!(md.contains("### Attachments"), "{md}");
        assert!(md.contains("- note.txt (text/plain)"), "{md}");
        let md_off = EmailBackend {
            list_attachments: false,
            images: ImagePolicy::default(),
        }
        .convert(&src)
        .unwrap()
        .export_to_markdown();
        assert!(!md_off.contains("Attachments"), "{md_off}");
    }
}
