//! iCalendar (#1032, docling#4880): ports of upstream's
//! `tests/test_backend_calendar.py`, its `test_guess_format_calendar` cases in
//! `test_input_doc.py` and the calendar case of `test_backend_text_encoding.py`.
//! The four mirrored fixtures' Markdown and JSON are pinned by the regression
//! suite (`tests/data/calendar/mirror.txt`); here they are checked against
//! upstream's groundtruth itself.

use std::path::{Path, PathBuf};

use docling::{DocumentConverter, InputFormat, SourceDocument};
use docling_core::tree::TreeKind;

fn corpus(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/data/calendar")
        .join(rel)
}

fn convert(raw: &[u8]) -> Result<docling_core::DoclingDocument, docling::ConversionError> {
    let source = SourceDocument::from_bytes("test", InputFormat::Calendar, raw.to_vec());
    DocumentConverter::new().convert(source).map(|r| r.document)
}

/// `(label, text)` of every text item in the JSON tree, in order — upstream's
/// `doc.texts`.
fn items(doc: &docling_core::DoclingDocument) -> Vec<(String, String)> {
    doc.tree
        .as_ref()
        .expect("calendar documents carry the item tree")
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            TreeKind::Text { label, text, .. } => Some((label.clone(), text.clone())),
            _ => None,
        })
        .collect()
}

fn texts(doc: &docling_core::DoclingDocument) -> Vec<String> {
    items(doc).into_iter().map(|(_, t)| t).collect()
}

fn calendar_with(lines: &[&str], newline: &str) -> Vec<u8> {
    let mut body = vec!["BEGIN:VCALENDAR", "VERSION:2.0"];
    body.extend_from_slice(lines);
    body.push("END:VCALENDAR");
    (body.join(newline) + newline).into_bytes()
}

fn calendar(lines: &[&str]) -> Vec<u8> {
    calendar_with(lines, "\r\n")
}

/// `test_e2e_calendar_conversions`: Markdown and JSON against upstream's
/// groundtruth (the JSON without `origin` / `version`, which carry the
/// converter's own hash and schema version).
#[test]
fn fixtures_match_upstream_groundtruth() {
    for name in [
        "apple_mixed",
        "google_export",
        "outlook_cancel",
        "outlook_invite",
    ] {
        let Ok(source) = SourceDocument::from_file(corpus(&format!("sources/{name}.ics"))) else {
            return; // the corpus is not packaged with the crate
        };
        assert_eq!(source.format, InputFormat::Calendar);
        let doc = DocumentConverter::new().convert(source).unwrap().document;
        let md = std::fs::read_to_string(corpus(&format!("groundtruth/{name}.ics.md"))).unwrap();
        assert_eq!(doc.export_to_markdown().trim_end(), md.trim_end(), "{name}");

        let strip = |mut v: serde_json::Value| {
            let o = v.as_object_mut().unwrap();
            o.remove("origin");
            o.remove("version");
            v
        };
        let gt: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(corpus(&format!("groundtruth/{name}.ics.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(strip(doc.export_to_json_value()), strip(gt), "{name}");
    }
}

#[test]
fn inline_attachment_payload_is_not_copied() {
    let Ok(source) = SourceDocument::from_file(corpus("sources/outlook_invite.ics")) else {
        return;
    };
    let doc = DocumentConverter::new().convert(source).unwrap().document;
    assert!(texts(&doc).contains(&"budget-draft.txt (text/plain)".to_string()));
    assert!(!doc.export_to_json().contains("UTQgYnVkZ2V0"));
}

/// RFC 5545 lets a fold split a multi-byte character, so folds go before
/// decoding — with or without other UTF-8 in the file.
#[test]
fn fold_inside_a_utf8_character() {
    for other in [None, Some("DESCRIPTION:naïve")] {
        let mut lines = vec!["BEGIN:VEVENT", "SUMMARY:Café au lait"];
        lines.extend(other);
        lines.push("END:VEVENT");
        let raw = calendar(&lines);
        let raw = String::from_utf8(raw).unwrap();
        let mut bytes = Vec::new();
        for (i, part) in raw.split('é').enumerate() {
            if i > 0 {
                bytes.extend_from_slice(b"\xc3\r\n \xa9");
            }
            bytes.extend_from_slice(part.as_bytes());
        }
        let doc = convert(&bytes).unwrap();
        assert!(
            items(&doc).contains(&("section_header".into(), "Café au lait".into())),
            "{other:?}: {:?}",
            items(&doc)
        );
    }
}

/// Folds join with a space or a tab — also inside a quoted parameter
/// value — under every line ending.
#[test]
fn line_endings_and_folds() {
    for newline in ["\r\n", "\n", "\r"] {
        let raw = calendar_with(
            &[
                "BEGIN:VEVENT",
                "SUMMARY:Quarterly",
                "\tplanning",
                "ORGANIZER;CN=\"Martin, Ca",
                " rla\":mailto:carla@example.com",
                "END:VEVENT",
            ],
            newline,
        );
        let texts = texts(&convert(&raw).unwrap());
        assert!(texts.contains(&"Quarterlyplanning".into()), "{newline:?}");
        assert!(
            texts.contains(&"Organizer: \"Martin, Carla\" <carla@example.com>".into()),
            "{newline:?}: {texts:?}"
        );
    }
}

#[test]
fn utf16_file_is_unfolded_after_decoding() {
    let raw = calendar(&["BEGIN:VEVENT", "SUMMARY:Résumé re", " view", "END:VEVENT"]);
    let text = String::from_utf8(raw).unwrap();
    let mut utf16 = vec![0xff, 0xfe];
    for u in text.encode_utf16() {
        utf16.extend_from_slice(&u.to_le_bytes());
    }
    // A nameless input is recognized by its first line behind the BOM.
    let source = DocumentConverter::new()
        .load_named_bytes("calendar", utf16, None)
        .unwrap();
    assert_eq!(source.format, InputFormat::Calendar);
    let doc = DocumentConverter::new().convert(source).unwrap().document;
    assert!(texts(&doc).contains(&"Résumé review".into()));
}

/// A BEGIN the open component cannot hold closes it first, so a VTIMEZONE
/// with no END does not swallow the event after it.
#[test]
fn missing_end_does_not_swallow_the_next_entry() {
    let raw = calendar(&[
        "BEGIN:VTIMEZONE",
        "TZID:Europe/Berlin",
        "BEGIN:VEVENT",
        "SUMMARY:Kept event",
        "END:VTODO",
        "END:VEVENT",
        "BEGIN:VEVENT",
        "SUMMARY:Truncated event",
    ]);
    let raw = String::from_utf8(raw)
        .unwrap()
        .replace("END:VCALENDAR\r\n", "");
    let texts = texts(&convert(raw.as_bytes()).unwrap());
    assert!(texts.contains(&"Kept event".into()));
    assert!(texts.contains(&"Truncated event".into()));
}

#[test]
fn entries_outside_a_calendar_are_rendered() {
    let doc = convert(b"BEGIN:VEVENT\r\nSUMMARY:Lonely event\r\nEND:VEVENT\r\n").unwrap();
    assert!(texts(&doc).contains(&"Lonely event".into()));
}

#[test]
fn empty_calendar_converts_to_an_empty_document() {
    let doc = convert(&calendar(&[])).unwrap();
    assert!(texts(&doc).is_empty());
}

#[test]
fn text_without_calendar_fails() {
    assert!(convert(b"Meeting on Monday at 10.\r\n").is_err());
}

/// All-day ends are exclusive, `Z` wins over a TZID, a bad value stays raw.
#[test]
fn event_times() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["DTSTART;VALUE=DATE:20261020"],
            &["Date: 2026-10-20 (all day)"],
        ),
        (
            &["DTSTART;VALUE=DATE:20261020", "DTEND;VALUE=DATE:20261021"],
            &["Date: 2026-10-20 (all day)"],
        ),
        (
            &["DTSTART;VALUE=DATE:20261020", "DTEND;VALUE=DATE:20261023"],
            &["Start: 2026-10-20 (all day)", "End: 2026-10-22 (all day)"],
        ),
        (
            &["DTSTART:20261020", "DURATION:P3D"],
            &["Start: 2026-10-20 (all day)", "End: 2026-10-22 (all day)"],
        ),
        (
            &[
                "DTSTART;TZID=Europe/Paris:20261020T090000Z",
                "DURATION:PT1H30M",
            ],
            &["Start: 2026-10-20 09:00 UTC", "Duration: 1 hour 30 minutes"],
        ),
        (&["DTSTART:20261020T090000"], &["Start: 2026-10-20 09:00"]),
        (&["DTSTART:2026-13-45"], &["Start: 2026-13-45"]),
    ];
    for (lines, expected) in cases {
        let mut all = vec!["BEGIN:VEVENT", "SUMMARY:E"];
        all.extend_from_slice(lines);
        all.push("END:VEVENT");
        let texts = texts(&convert(&calendar(&all)).unwrap());
        assert_eq!(&texts[1..1 + expected.len()], *expected, "{lines:?}");
    }
}

#[test]
fn alarms() {
    let raw = calendar(&[
        "BEGIN:VTODO",
        "SUMMARY:T",
        "DUE:20261031T170000Z",
        "BEGIN:VALARM",
        "ACTION:DISPLAY",
        "TRIGGER;RELATED=END:-PT30M",
        "END:VALARM",
        "BEGIN:VALARM",
        "ACTION:EMAIL",
        "TRIGGER:PT0S",
        "REPEAT:2",
        "DURATION:PT5M",
        "END:VALARM",
        "BEGIN:VALARM",
        "ACTION:NONE",
        "TRIGGER:-PT5M",
        "END:VALARM",
        "END:VTODO",
    ]);
    let texts = texts(&convert(&raw).unwrap());
    assert_eq!(
        texts[texts.len() - 2..],
        [
            "Display, 30 minutes before due",
            "Email, at start, repeated 2 times every 5 minutes",
        ]
    );
}

/// `test_guess_format_calendar`: a nameless input is a calendar by its first
/// line (BOM, leading blank lines, lower case, UTF-16 too); a name with a
/// calendar extension is one whatever the content; a named text file keeps
/// its own route; a `BEGIN:VCALENDAR` past the first line is no calendar.
#[test]
fn format_detection() {
    let ics: &[u8] =
        b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nSUMMARY:a;b,c\r\nEND:VEVENT\r\n";
    let mut bom = b"\xef\xbb\xbf".to_vec();
    bom.extend_from_slice(ics);
    let mut lower = b"\r\n\r\n".to_vec();
    lower.extend(ics.to_ascii_lowercase());
    let mut utf16 = vec![0xff, 0xfe];
    for u in std::str::from_utf8(ics).unwrap().encode_utf16() {
        utf16.extend_from_slice(&u.to_le_bytes());
    }
    for bytes in [ics.to_vec(), bom, lower, utf16] {
        assert_eq!(InputFormat::detect(&bytes), Some(InputFormat::Calendar));
    }
    assert_ne!(
        InputFormat::detect(b"Agenda\r\nBEGIN:VCALENDAR\r\n"),
        Some(InputFormat::Calendar)
    );
    assert_ne!(
        InputFormat::detect(b"BEGIN:VCALENDARX\r\n"),
        Some(InputFormat::Calendar)
    );
    for ext in ["ics", "ical", "icalendar", "ifb", "ICS"] {
        assert_eq!(
            InputFormat::from_extension(&ext.to_ascii_lowercase()),
            Some(InputFormat::Calendar),
            "{ext}"
        );
    }
    let busy = SourceDocument::from_named_bytes("busy.ifb", ics.to_vec(), None).unwrap();
    assert_eq!(busy.format, InputFormat::Calendar);
    let txt = SourceDocument::from_named_bytes("invite.txt", ics.to_vec(), None).unwrap();
    assert_ne!(txt.format, InputFormat::Calendar);
    assert_eq!(
        InputFormat::from_mime("text/calendar; charset=utf-8"),
        Some(InputFormat::Calendar)
    );
    assert_eq!(
        InputFormat::from_id("calendar"),
        Some(InputFormat::Calendar)
    );
    assert_eq!(InputFormat::Calendar.as_str(), "calendar");
}

/// The calendar case of `test_backend_text_encoding.py`: a requested
/// encoding decodes the file (after unfolding, on the bytes).
#[test]
fn requested_encoding_decodes_the_file() {
    let text = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Rapport\nDESCRIPTION:Café crème déjà vu\n\
                END:VEVENT\nEND:VCALENDAR\n";
    let (latin1, _, _) = encoding_rs::WINDOWS_1252.encode(text);
    let source = SourceDocument::from_bytes("cal", InputFormat::Calendar, latin1.into_owned());
    let doc = DocumentConverter::new()
        .encoding(Some("cp1252".into()))
        .convert(source)
        .unwrap()
        .document;
    assert!(texts(&doc).contains(&"Café crème déjà vu".into()));
    // An unknown label fails the conversion rather than guessing.
    let source = SourceDocument::from_bytes("cal", InputFormat::Calendar, text.as_bytes().to_vec());
    assert!(DocumentConverter::new()
        .encoding(Some("no-such-codec".into()))
        .convert(source)
        .is_err());
}
