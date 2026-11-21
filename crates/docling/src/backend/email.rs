//! Email (`.eml` / Outlook `.msg`) backend — a port of docling's
//! `EmailDocumentBackend` (#251 for `.msg`, docling#3873).
//!
//! The subject becomes the document title; `From:`/`To:`/`Date:` headers become
//! text paragraphs; the body (a non-empty `text/plain`, else each `text/html`
//! part through the HTML backend with its furniture kept apart, docling#5228)
//! is split into paragraphs on blank lines. All emitted text is HTML/underscore-escaped like docling-core
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

use std::borrow::Cow;
use std::collections::HashMap;

use mail_parser::{Address, Message, MessageParser, MimeHeaders, PartType};

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
    /// Append the events of the message's calendar parts, inline or
    /// attached, as a `Calendar` section — docling's
    /// `EmailBackendOptions.parse_calendar_invitations` (docling#5357, on
    /// by default). Off, a calendar body is kept as text.
    pub parse_calendar_invitations: bool,
}

impl Default for EmailBackend {
    fn default() -> Self {
        Self {
            list_attachments: false,
            images: ImagePolicy::default(),
            parse_calendar_invitations: true,
        }
    }
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
        // `Cc:` after `To:` (docling#4505): the copied recipients are part of
        // who the message was addressed to. A `.msg` gets the line from its
        // projection, which writes the MAPI Cc recipients as a `Cc` header.
        for (label, addrs) in [("From", msg.from()), ("To", msg.to()), ("Cc", msg.cc())] {
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
        let calendar_parts = if self.parse_calendar_invitations {
            calendar_parts(&msg)
        } else {
            Vec::new()
        };
        let (shown_parts, calendars) = super::calendar::merge_calendar_parts(calendar_parts);
        // The body's own text, whitespace-collapsed: a calendar description
        // it holds is left out (docling#5455).
        let body_text = match self.body_with_images(source, &msg, projected.as_ref()) {
            Some(nodes) => {
                let text = DoclingDocument {
                    nodes: nodes.clone(),
                    ..DoclingDocument::new("body")
                }
                .export_to_text();
                doc.nodes.extend(nodes);
                text
            }
            None => {
                let paragraphs = body_paragraphs(&text_parts(&msg), &shown_parts);
                let text = paragraphs
                    .iter()
                    .filter(|(_, furniture)| !furniture)
                    .map(|(p, _)| p.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                for (para, furniture) in paragraphs {
                    let text = escape_text(&para);
                    // A furniture paragraph is docling's furniture-layer text
                    // item: in the JSON, out of the default Markdown.
                    doc.push(if furniture {
                        Node::FurnitureText {
                            label: "text".into(),
                            text,
                        }
                    } else {
                        Node::Paragraph { text }
                    });
                }
                text
            }
        };
        if !calendars.is_empty() {
            let body_text = body_text.split_whitespace().collect::<Vec<_>>().join(" ");
            doc.nodes.extend(super::calendar::email_calendar_nodes(
                &calendars, &body_text,
            ));
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
            // docling's `_convert_html_part` options (no title, no inferred
            // furniture), as on the paragraph path.
            let html_doc =
                super::html::convert_html_opts("email-body.html", &html, &resolver, false, false);
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
/// `_get_attachment_labels`, over the parts mailparser counts as
/// attachments ([`is_attachment`]; an inline calendar alternative is not
/// one, docling#5357's `invite.ics` test).
fn eml_attachment_labels(msg: &Message) -> Vec<String> {
    use mail_parser::MimeHeaders;
    msg.attachments()
        .filter(|part| is_attachment(part))
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

/// The calendar file extensions (docling's `FormatToExtensions[CALENDAR]`).
const CALENDAR_EXTENSIONS: &[&str] = &["ics", "ical", "icalendar", "ifb"];

/// docling's `_is_calendar_part`: `text/calendar` or `application/ics` (as
/// Gmail sends the `invite.ics` copy), or a file name with a calendar
/// extension.
fn is_calendar_part(part: &mail_parser::MessagePart) -> bool {
    let typed = part.content_type().is_some_and(|ct| {
        let mime = format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("")).to_ascii_lowercase();
        mime == "text/calendar" || mime == "application/ics"
    });
    typed
        || part.attachment_name().is_some_and(|name| {
            std::path::Path::new(name)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| CALENDAR_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        })
}

/// The bytes of a part as sent, the transfer encoding undone but no charset
/// applied — mail-parser's own text would already be decoded, and a folded
/// iCalendar line may split a UTF-8 character.
fn part_bytes<'m>(msg: &'m Message, part: &'m mail_parser::MessagePart) -> Cow<'m, [u8]> {
    let raw = msg
        .raw_message()
        .get(part.offset_body as usize..part.offset_end as usize)
        .unwrap_or_default();
    let decoded = match part.encoding {
        mail_parser::Encoding::Base64 => mail_parser::decoders::base64::base64_decode(raw),
        mail_parser::Encoding::QuotedPrintable => {
            mail_parser::decoders::quoted_printable::quoted_printable_decode(raw)
        }
        mail_parser::Encoding::None => None,
    };
    match decoded {
        Some(bytes) => Cow::Owned(bytes),
        None => Cow::Borrowed(raw),
    }
}

/// docling's `_get_calendar_parts`: each calendar part of the message,
/// inline or attached, parsed from its bytes with its charset; a part that
/// cannot be decoded is skipped with a warning, one without a calendar
/// component left out.
fn calendar_parts(msg: &Message) -> Vec<Vec<super::calendar::Component>> {
    let mut out = Vec::new();
    for part in &msg.parts {
        if part.is_multipart()
            || matches!(part.body, PartType::Message(_))
            || !is_calendar_part(part)
        {
            continue;
        }
        let charset = part.content_type().and_then(|ct| ct.attribute("charset"));
        match super::calendar::email_calendar_part(&part_bytes(msg, part), charset) {
            Ok(Some(components)) => out.push(components),
            Ok(None) => {}
            Err(e) => eprintln!("docling: skipped a calendar part that cannot be decoded: {e}"),
        }
    }
    out
}

/// The delimiter mailparser puts between the parts of its whole-message
/// `body`, the last-resort text.
const MAILPARSER_BODY_DELIMITER: &str = "\n--- mail_boundary ---\n";

/// A message's leaf text the way docling's mailparser files it: every
/// non-multipart part that is not an attachment, in order — `text/html` to
/// `html`, `text/plain` (or no content type) to `plain`, any other subtype
/// to `other` (`text_not_managed`). A part is an attachment when it has a
/// file name, a `Content-ID` and a subtype other than html/plain, the `rtf`
/// subtype, or `Content-Disposition: attachment`; an `inline` text/plain or
/// text/html part is a body all the same. mail-parser's own body lists are
/// not used: they put an HTML-only alternative among the text bodies as its
/// own html-to-text conversion, so the HTML backend — and the furniture it
/// keeps apart (docling#5228) — never saw it.
#[derive(Default)]
struct TextParts {
    plain: Vec<String>,
    html: Vec<String>,
    other: Vec<String>,
}

/// A part's content subtype, lower-cased; `plain` without a content type
/// (Python's `get_content_subtype()`).
fn subtype(part: &mail_parser::MessagePart) -> String {
    part.content_type()
        .map(|c| c.subtype().unwrap_or("").to_ascii_lowercase())
        .unwrap_or_else(|| "plain".into())
}

fn disposition(part: &mail_parser::MessagePart) -> Option<String> {
    part.content_disposition()
        .map(|d| d.ctype().to_ascii_lowercase())
}

/// mailparser's attachment rule: a file name, a `Content-ID` on a subtype
/// other than html/plain, the `rtf` subtype, or `Content-Disposition:
/// attachment`. An inline `text/calendar` alternative is none of these —
/// it is body text, which mail-parser's own attachment list counts.
fn is_attachment(part: &mail_parser::MessagePart) -> bool {
    let subtype = subtype(part);
    part.attachment_name().is_some()
        || (part.content_id().is_some() && !matches!(subtype.as_str(), "plain" | "html"))
        || subtype == "rtf"
        || disposition(part).as_deref() == Some("attachment")
}

fn text_parts(msg: &Message) -> TextParts {
    let mut out = TextParts::default();
    for part in &msg.parts {
        let payload = match &part.body {
            PartType::Text(t) | PartType::Html(t) => t.to_string(),
            PartType::Binary(b) | PartType::InlineBinary(b) => {
                String::from_utf8_lossy(b).into_owned()
            }
            PartType::Message(_) | PartType::Multipart(_) => continue,
        };
        let subtype = subtype(part);
        let body_type = part
            .content_type()
            .is_none_or(|c| c.ctype().eq_ignore_ascii_case("text"))
            && matches!(subtype.as_str(), "plain" | "html");
        let inline_body = body_type && disposition(part).as_deref() == Some("inline");
        if (is_attachment(part) && !inline_body) || payload.is_empty() {
            continue;
        }
        match subtype.as_str() {
            "html" => out.html.push(payload),
            "plain" => out.plain.push(payload),
            _ => out.other.push(payload),
        }
    }
    out
}

/// docling's `_split_paragraphs`: CRLF and a lone CR normalised to `\n`
/// (docling#4248), then split on blank lines.
fn split_paragraphs(text: &str, out: &mut Vec<String>) {
    let re = cached_regex!(r"\n\s*\n+");
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    for p in re.split(text.trim()) {
        let p = p.trim();
        if !p.is_empty() {
            out.push(p.to_string());
        }
    }
}

/// One body paragraph and whether it is furniture (docling's
/// `_BodyParagraph`), unescaped — the caller escapes it like every text.
type BodyParagraph = (String, bool);

/// docling's `_get_body_paragraphs`: the `text/plain` parts' paragraphs, if
/// any part produced one (a blank plain part beside the real HTML is common,
/// docling#4295); else each `text/html` part through the HTML backend
/// (`add_title=False, infer_furniture=False`), its body then its furniture
/// exported as unescaped Markdown and split (docling#5228: a `<footer>`
/// stays furniture, out of the default export); else mailparser's whole
/// body.
fn body_paragraphs(
    parts: &TextParts,
    shown_calendars: &[Vec<super::calendar::Component>],
) -> Vec<BodyParagraph> {
    let mut plain = Vec::new();
    for text in &parts.plain {
        split_paragraphs(text, &mut plain);
    }
    if !plain.is_empty() {
        return plain.into_iter().map(|p| (p, false)).collect();
    }
    let mut out = Vec::new();
    for html in &parts.html {
        let html_doc = super::html::convert_html_opts(
            "email-body.html",
            html,
            &super::images::NoFetch,
            false,
            false,
        );
        for furniture in [false, true] {
            let layers = docling_core::ContentLayers {
                body: !furniture,
                furniture,
                notes: false,
                invisible: false,
                background: false,
            };
            let options = docling_core::MarkdownExportOptions {
                layers,
                escape_html: false,
                escape_underscores: false,
                ..Default::default()
            };
            let (markdown, _) = html_doc.export_to_markdown_with_options(&options);
            let mut paragraphs = Vec::new();
            split_paragraphs(&markdown, &mut paragraphs);
            out.extend(paragraphs.into_iter().map(|p| (p, furniture)));
        }
    }
    if !out.is_empty() {
        return out;
    }
    // mailparser puts a calendar part in its body too: one that parses to a
    // calendar part shown in the `Calendar` section is left out, so its text
    // is never lost and never doubled (docling#5357's `_get_fallback_body`).
    let body: Vec<&str> = parts
        .plain
        .iter()
        .chain(&parts.html)
        .chain(
            parts
                .other
                .iter()
                .filter(|t| !super::calendar::is_shown_calendar(t, shown_calendars)),
        )
        .map(String::as_str)
        .collect();
    let mut fallback = Vec::new();
    split_paragraphs(&body.join(MAILPARSER_BODY_DELIMITER), &mut fallback);
    fallback.into_iter().map(|p| (p, false)).collect()
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
            parse_calendar_invitations: true,
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
            parse_calendar_invitations: true,
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
            parse_calendar_invitations: true,
        }
        .convert(&src)
        .unwrap()
        .export_to_markdown();
        assert!(md.contains("### Attachments"), "{md}");
        assert!(md.contains("- note.txt (text/plain)"), "{md}");
        let md_off = EmailBackend {
            list_attachments: false,
            images: ImagePolicy::default(),
            parse_calendar_invitations: true,
        }
        .convert(&src)
        .unwrap()
        .export_to_markdown();
        assert!(!md_off.contains("Attachments"), "{md_off}");
    }
}
