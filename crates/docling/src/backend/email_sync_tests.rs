//! The email cases of the 2026-11 upstream sync (#1208), ported from
//! docling's `tests/test_backend_email.py`: each test builds the message
//! upstream's builds and asserts what upstream asserts. The expected
//! Markdown is what docling 2.151.1 writes for the same bytes.

use serde_json::Value;

use super::email::EmailBackend;
use super::DeclarativeBackend;
use crate::format::InputFormat;
use crate::source::SourceDocument;
use docling_core::DoclingDocument;

/// One MIME part: `(content type with parameters, extra headers, body)`.
pub(super) type Part<'a> = (&'a str, &'a str, &'a str);

/// A message with `headers`, one body part, or a `multipart/{kind}` of
/// `parts`.
pub(super) fn message(headers: &str, kind: Option<&str>, parts: &[Part]) -> Vec<u8> {
    let mut m = format!("{headers}MIME-Version: 1.0\r\n");
    match kind {
        None => {
            let (ct, extra, body) = parts[0];
            m.push_str(&format!("Content-Type: {ct}\r\n{extra}\r\n{body}\r\n"));
        }
        Some(kind) => {
            m.push_str(&format!(
                "Content-Type: multipart/{kind}; boundary=\"BOUNDARY\"\r\n\r\n"
            ));
            for (ct, extra, body) in parts {
                m.push_str(&format!(
                    "--BOUNDARY\r\nContent-Type: {ct}\r\n{extra}\r\n{body}\r\n"
                ));
            }
            m.push_str("--BOUNDARY--\r\n");
        }
    }
    m.into_bytes()
}

pub(super) fn convert_with(eml: Vec<u8>, backend: &EmailBackend) -> DoclingDocument {
    let src = SourceDocument::from_bytes("t.eml", InputFormat::Email, eml);
    backend.convert(&src).unwrap()
}

pub(super) fn convert(eml: Vec<u8>) -> DoclingDocument {
    convert_with(eml, &EmailBackend::default())
}

/// `(text, content_layer)` of every text item.
pub(super) fn texts(doc: &DoclingDocument) -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["text"].as_str().unwrap().to_string(),
                t["content_layer"].as_str().unwrap_or("body").to_string(),
            )
        })
        .collect()
}

pub(super) fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// `export_to_markdown(included_content_layers={BODY, FURNITURE})`.
pub(super) fn md_all(doc: &DoclingDocument) -> String {
    let options = docling_core::MarkdownExportOptions {
        layers: docling_core::ContentLayers {
            body: true,
            furniture: true,
            notes: false,
            invisible: false,
            background: false,
        },
        ..Default::default()
    };
    doc.export_to_markdown_with_options(&options)
        .0
        .trim_end()
        .to_string()
}

pub(super) fn md(doc: &DoclingDocument) -> String {
    doc.export_to_markdown().trim_end().to_string()
}

/// docling#4505 (`test_email_backend_keeps_cc_recipients`): the Cc
/// recipients follow `To:`, formatted like it.
#[test]
fn keeps_cc_recipients() {
    let eml = b"From: Alice Example <alice@example.com>\r\nTo: Bob Example <bob@example.com>\r\n\
        Cc: Carol Example <carol@example.com>, dave@example.com\r\nSubject: Copied Email\r\n\
        Date: Tue, 20 May 2026 10:30:00 +0000\r\nMIME-Version: 1.0\r\n\
        Content-Type: text/plain; charset=\"utf-8\"\r\n\r\nHello all.\r\n";
    let doc = convert(eml.to_vec());
    let got: Vec<String> = texts(&doc).into_iter().map(|(t, _)| t).collect();
    assert_eq!(
        got,
        [
            "Copied Email",
            "From: Alice Example <alice@example.com>",
            "To: Bob Example <bob@example.com>",
            "Cc: Carol Example <carol@example.com>, dave@example.com",
            "Date: 2026-05-20T10:30:00+00:00",
            "Hello all.",
        ]
    );
    assert!(md(&doc).contains(
        "To: Bob Example &lt;bob@example.com&gt;\n\n\
         Cc: Carol Example &lt;carol@example.com&gt;, dave@example.com\n\n"
    ));
}

const HTML: &str = "<html><body><p>Body: 1 &lt; 2 &amp; 3 &gt; 2, a_b.</p>\
                    <footer>Footer: terms &amp; conditions, c_d.</footer></body></html>";

/// An HTML-only message, or a blank `text/plain` beside the HTML.
fn html_message(subject: &str, html: &str, blank_plain: bool) -> Vec<u8> {
    let headers = format!("Subject: {subject}\r\n");
    let html_part = ("text/html; charset=\"utf-8\"", "", html);
    if blank_plain {
        message(
            &headers,
            Some("alternative"),
            &[("text/plain; charset=\"utf-8\"", "", " \r\n"), html_part],
        )
    } else {
        message(&headers, None, &[html_part])
    }
}

/// docling#5228 (`test_email_html_body_keeps_furniture_outside_the_body`,
/// html-only and blank-plain): the HTML body goes through the HTML backend
/// and its `<footer>` stays furniture, escaped once in Markdown.
#[test]
fn html_body_keeps_furniture_outside_the_body() {
    for blank_plain in [false, true] {
        let doc = convert(html_message("HTML furniture", HTML, blank_plain));
        let footer = "Footer: terms & conditions, c_d.";
        assert_eq!(
            texts(&doc),
            pairs(&[
                ("HTML furniture", "body"),
                ("Body: 1 < 2 & 3 > 2, a_b.", "body"),
                (footer, "furniture"),
            ]),
            "blank plain: {blank_plain}"
        );
        assert!(!md(&doc).contains("Footer:"));
        let all = md_all(&doc);
        assert_eq!(
            all.matches("Footer: terms &amp; conditions, c\\_d.")
                .count(),
            1,
            "{all}"
        );
        assert!(
            all.contains("Body: 1 &lt; 2 &amp; 3 &gt; 2, a\\_b."),
            "{all}"
        );
    }
}

/// docling#5228 (`test_email_with_only_html_furniture_does_not_fall_back_to_raw_html`).
#[test]
fn only_html_furniture_does_not_fall_back_to_raw_html() {
    for blank_plain in [false, true] {
        let doc = convert(html_message(
            "Footer only email",
            "<html><body><footer>Footer only.</footer></body></html>",
            blank_plain,
        ));
        assert_eq!(
            texts(&doc),
            pairs(&[("Footer only email", "body"), ("Footer only.", "furniture")])
        );
        assert_eq!(md(&doc), "# Footer only email");
        assert_eq!(md_all(&doc), "# Footer only email\n\nFooter only.");
    }
}

/// docling#5228 (`test_email_keeps_nonempty_plain_body_instead_of_html_furniture`).
#[test]
fn nonempty_plain_body_wins_over_html_furniture() {
    let doc = convert(message(
        "Subject: Plain preferred\r\n",
        Some("alternative"),
        &[
            (
                "text/plain; charset=\"utf-8\"",
                "",
                "Plain body.\r\n\r\nShared footer.\r\n",
            ),
            (
                "text/html; charset=\"utf-8\"",
                "",
                "<html><body><p>HTML body.</p><footer>Shared footer.</footer></body></html>",
            ),
        ],
    ));
    assert_eq!(
        texts(&doc),
        pairs(&[
            ("Plain preferred", "body"),
            ("Plain body.", "body"),
            ("Shared footer.", "body"),
        ])
    );
    assert_eq!(
        md_all(&doc),
        "# Plain preferred\n\nPlain body.\n\nShared footer."
    );
}

/// docling#5228 (`test_email_keeps_furniture_from_each_html_part_once`).
#[test]
fn furniture_from_each_html_part_once() {
    let html = "text/html; charset=\"utf-8\"";
    let doc = convert(message(
        "Subject: Multipart HTML\r\n",
        Some("mixed"),
        &[
            (html, "", "<html><body> </body></html>"),
            (
                html,
                "",
                "<html><body><p>First body.</p><footer>First footer.</footer></body></html>",
            ),
            (
                html,
                "",
                "<html><body><p>Second body.</p><footer>Second footer.</footer></body></html>",
            ),
        ],
    ));
    assert_eq!(
        texts(&doc),
        pairs(&[
            ("Multipart HTML", "body"),
            ("First body.", "body"),
            ("First footer.", "furniture"),
            ("Second body.", "body"),
            ("Second footer.", "furniture"),
        ])
    );
    assert_eq!(md(&doc), "# Multipart HTML\n\nFirst body.\n\nSecond body.");
}

/// The HTML body is the HTML backend's Markdown — a heading, bold, a list
/// and a table, each block a paragraph whose line breaks print as hard
/// breaks (docling 2.151.1's output) — where mail-parser's html-to-text
/// flattened it to `HeadPara bold.  \nonetwoAB12`.
#[test]
fn rich_html_body_keeps_its_structure() {
    let doc = convert(message(
        "Subject: Rich\r\n",
        None,
        &[(
            "text/html; charset=\"utf-8\"",
            "",
            "<html><body><h2>Head</h2><p>Para <b>bold</b>.</p><ul><li>one</li><li>two</li></ul>\
             <table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table></body></html>",
        )],
    ));
    assert_eq!(
        md(&doc),
        "# Rich\n\n## Head\n\nPara **bold**.\n\n- one  \n- two\n\n\
         |   A |   B |  \n|-----|-----|  \n|   1 |   2 |"
    );
}

const INVITATION_ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
    PRODID:-//Docling//Invitation test//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
    UID:planning-2026@example.com\r\nSEQUENCE:0\r\nDTSTAMP:20261113T080000Z\r\n\
    DTSTART;TZID=Europe/Berlin:20261120T100000\r\nDTEND;TZID=Europe/Berlin:20261120T110000\r\n\
    SUMMARY:Quarterly planning\r\nLOCATION:Room 4\r\n\
    ORGANIZER;CN=Alice Example:mailto:alice@example.com\r\n\
    ATTENDEE;CN=Bob Example;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION:mailto:bob@example.com\r\n\
    DESCRIPTION:Agenda: budget and hiring.\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

const INVITATION_MARKDOWN: &str = "### Calendar\n\n\
    Message type: Invitation\n\n\
    #### Quarterly planning\n\n\
    Start: 2026-11-20 10:00 Europe/Berlin\n\n\
    End: 2026-11-20 11:00 Europe/Berlin\n\n\
    Location: Room 4\n\n\
    Organizer: Alice Example &lt;alice@example.com&gt;\n\n\
    Agenda: budget and hiring.\n\n\
    ##### Attendees\n\n\
    - Bob Example &lt;bob@example.com&gt; (required, awaiting response)";

const INVITE_HEADERS: &str = "From: Alice Example <alice@example.com>\r\nTo: bob@example.com\r\n";
const PLAIN: &str = "text/plain; charset=\"utf-8\"";
const CALENDAR: &str = "text/calendar; charset=\"utf-8\"; method=\"REQUEST\"";

/// docling#5357 (`test_email_adds_the_events_of_an_inline_invitation`):
/// Outlook's `text/calendar` part beside the body is a Calendar section.
#[test]
fn adds_the_events_of_an_inline_invitation() {
    let eml = message(
        &format!("Subject: Quarterly planning\r\n{INVITE_HEADERS}"),
        Some("alternative"),
        &[
            (PLAIN, "", "When: 20 November, 10:00.\r\n"),
            (CALENDAR, "", INVITATION_ICS),
        ],
    );
    let md = md(&convert(eml.clone()));
    assert!(
        md.ends_with(&format!(
            "When: 20 November, 10:00.\n\n{INVITATION_MARKDOWN}"
        )),
        "{md}"
    );
    // Off, the section is gone and the body is the plain part alone.
    let off = EmailBackend {
        parse_calendar_invitations: false,
        ..EmailBackend::default()
    };
    let md = super::email_sync_tests::md(&convert_with(eml, &off));
    assert!(md.ends_with("When: 20 November, 10:00."), "{md}");
}

/// docling#5357 (`test_email_adds_an_invitation_sent_inline_and_attached_once`):
/// Gmail's inline part and `invite.ics` attachment give the event once, and
/// the inline part is no attachment.
#[test]
fn adds_an_invitation_sent_inline_and_attached_once() {
    let eml = message(
        &format!("Subject: Invitation: Quarterly planning\r\n{INVITE_HEADERS}"),
        Some("mixed"),
        &[
            (PLAIN, "", "You are invited to Quarterly planning.\r\n"),
            (
                "text/html; charset=\"utf-8\"",
                "",
                "<p>You are invited to Quarterly planning.</p>",
            ),
            (CALENDAR, "", INVITATION_ICS),
            (
                "application/ics",
                "Content-Disposition: attachment; filename=\"invite.ics\"\r\n",
                INVITATION_ICS,
            ),
        ],
    );
    let backend = EmailBackend {
        list_attachments: true,
        ..EmailBackend::default()
    };
    let md = md(&convert_with(eml, &backend));
    assert!(
        md.ends_with(&format!(
            "You are invited to Quarterly planning.\n\n{INVITATION_MARKDOWN}\
             \n\n### Attachments\n\n- invite.ics (application/ics)"
        )),
        "{md}"
    );
}

/// docling#5455 (`test_email_leaves_out_an_event_description_that_repeats_the_body`).
#[test]
fn leaves_out_an_event_description_that_repeats_the_body() {
    let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
        UID:quarter@example.com\r\nDTSTART:20261120T090000Z\r\nSUMMARY:Quarter planning\r\n\
        DESCRIPTION:Hi team,\\n\\nLet us plan the quarter.\\n\\nMicrosoft Teams\r\n  \
        meeting\\nJoin: https://teams.example.com/l/1\\n\\n\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let md = md(&convert(message(
        &format!("Subject: Quarter planning\r\n{INVITE_HEADERS}"),
        Some("alternative"),
        &[
            (
                PLAIN,
                "",
                "Hi team,\r\n\r\nLet us plan the quarter.\r\n\r\nMicrosoft Teams meeting\r\n\
                 Join: https://teams.example.com/l/1\r\n",
            ),
            (CALENDAR, "", ics),
        ],
    )));
    assert_eq!(md.matches("Let us plan the quarter.").count(), 1, "{md}");
    assert!(
        md.ends_with(
            "Join: https://teams.example.com/l/1\n\n### Calendar\n\n\
             Message type: Invitation\n\n#### Quarter planning\n\nStart: 2026-11-20 09:00 UTC"
        ),
        "{md}"
    );
}

/// docling#5357 (`test_email_reads_a_calendar_attachment_from_its_bytes`):
/// an `.ics` attachment is found by its name and unfolded before it is
/// decoded (the fold splits the `ü`); its calendar name is no second title.
#[test]
fn reads_a_calendar_attachment_from_its_bytes() {
    let ics = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-CALNAME:Team\r\nBEGIN:VEVENT\r\n\
        UID:offsite@example.com\r\nDTSTART;VALUE=DATE:20270115\r\n\
        SUMMARY:Planung f\xc3\r\n \xbcr 2027\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(ics);
    let eml = message(
        &format!("Subject: Team calendar\r\n{INVITE_HEADERS}"),
        Some("mixed"),
        &[
            (PLAIN, "", "The calendar is attached.\r\n"),
            (
                "application/octet-stream",
                "Content-Transfer-Encoding: base64\r\n\
                 Content-Disposition: attachment; filename=\"team.ics\"\r\n",
                &b64,
            ),
        ],
    );
    let doc = convert(eml);
    let titles = texts(&doc)
        .into_iter()
        .filter(|(t, _)| t == "Team calendar" || t == "Team")
        .count();
    assert_eq!(titles, 1);
    let md = md(&doc);
    assert!(
        md.ends_with(
            "The calendar is attached.\n\n### Calendar\n\nCalendar name: Team\n\n\
             #### Planung für 2027\n\nDate: 2027-01-15 (all day)"
        ),
        "{md}"
    );
}

/// docling#5357 (`test_email_calendar_body_is_not_kept_as_raw_text`).
#[test]
fn calendar_body_is_not_kept_as_raw_text() {
    let md = md(&convert(message(
        "Subject: Quarterly planning\r\n",
        None,
        &[(CALENDAR, "", INVITATION_ICS)],
    )));
    assert_eq!(md, format!("# Quarterly planning\n\n{INVITATION_MARKDOWN}"));
}

/// docling#5357 (`test_email_backend_preserves_calendar_body_as_text`):
/// with the option off, another text subtype is kept as text.
#[test]
fn calendar_body_is_kept_as_text_when_parsing_is_off() {
    let calendar = "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Docling//Email fallback test//EN\n\
        BEGIN:VEVENT\nUID:synthetic@example.org\nDTSTAMP:20261107T000000Z\n\
        DTSTART:20261108T090000Z\nDESCRIPTION:Review <terms> & conditions for a_b.\n\
        END:VEVENT\nEND:VCALENDAR";
    let off = EmailBackend {
        parse_calendar_invitations: false,
        ..EmailBackend::default()
    };
    let doc = convert_with(
        message(
            "Subject: Calendar body\r\n",
            None,
            &[("text/calendar; charset=\"utf-8\"", "", calendar)],
        ),
        &off,
    );
    assert_eq!(
        texts(&doc),
        pairs(&[("Calendar body", "body"), (calendar, "body")])
    );
    let escaped = calendar
        .replace(
            "<terms> & conditions for a_b.",
            "&lt;terms&gt; &amp; conditions for a\\_b.",
        )
        .replace('\n', "  \n");
    assert_eq!(md(&doc), format!("# Calendar body\n\n{escaped}"));
}

/// docling#5357 (`test_email_adds_the_name_of_a_calendar_without_entries`).
#[test]
fn adds_the_name_of_a_calendar_without_entries() {
    let md = md(&convert(message(
        "Subject: Team calendar\r\n",
        None,
        &[(
            "text/calendar; charset=\"utf-8\"",
            "",
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-CALNAME:Team\r\n\
             X-WR-CALDESC:Shared team dates.\r\nEND:VCALENDAR\r\n",
        )],
    )));
    assert_eq!(
        md,
        "# Team calendar\n\n### Calendar\n\nCalendar name: Team\n\nShared team dates."
    );
}

/// docling#5357 (`test_email_keeps_a_calendar_part_with_nothing_to_show_as_text`).
#[test]
fn keeps_a_calendar_part_with_nothing_to_show_as_text() {
    let md = md(&convert(message(
        "Subject: Time zone\r\n",
        None,
        &[(
            "text/calendar; charset=\"utf-8\"",
            "",
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\n\
             END:VTIMEZONE\r\nEND:VCALENDAR\r\n",
        )],
    )));
    assert!(md.contains("TZID:Europe/Berlin"), "{md}");
    assert!(!md.contains("### Calendar"), "{md}");
}

/// docling#5357 (`test_email_skips_a_calendar_part_that_cannot_be_decoded`).
#[test]
fn skips_a_calendar_part_that_cannot_be_decoded() {
    let md = md(&convert(message(
        &format!("Subject: Quarterly planning\r\n{INVITE_HEADERS}"),
        Some("mixed"),
        &[
            (PLAIN, "", "See the invitation.\r\n"),
            (
                "text/calendar; charset=\"x-unknown\"",
                "Content-Disposition: attachment; filename=\"invite.ics\"\r\n",
                INVITATION_ICS,
            ),
        ],
    )));
    assert!(md.ends_with("See the invitation."), "{md}");
}
