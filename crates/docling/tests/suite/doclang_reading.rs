//! DocLang 0.9 input (docling-core 2.106): the reading rules, the checkbox
//! run, the layers DocLang has, float captions and footnotes — ported from
//! docling-core's `test_inline_whitespace_contract.py` and
//! `test_serialization_doclang.py` (docling-core#693, #880, #955, #800).

use docling::{
    ContentLayers, DocumentConverter, InputFormat, MarkdownExportOptions, SourceDocument,
};
use docling_core::{ContentLayer, DoclingDocument, Node};
use serde_json::Value;

fn read(xml: &str) -> DoclingDocument {
    DocumentConverter::new()
        .convert(SourceDocument::from_bytes(
            "t.dclg",
            InputFormat::XmlDoclang,
            xml.as_bytes().to_vec(),
        ))
        .expect("doclang")
        .document
}

fn read_body(body: &str) -> DoclingDocument {
    read(&format!("<doclang>{body}</doclang>"))
}

/// The JSON `texts` as `(text, label)`.
fn texts(doc: &DoclingDocument) -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["text"].as_str().unwrap().to_string(),
                t["label"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// `test_doclang_reads_whitespace_per_spec`: a whitespace sequence reads as
/// one space, none at a text block's edges; `<content>` is literal; only
/// space, tab, CR and LF are whitespace.
#[test]
fn whitespace_reads_per_spec() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "spec_example",
            "<text>\n  Water (H<subscript>2</subscript>O) is <bold>clear </bold>\n  <italic> and odorless</italic>.\n  This keeps <content>two  spaces</content> as written.\n</text>",
            "Water (H2O) is clear and odorless. This keeps two  spaces as written.",
        ),
        ("newline_next_to_tag", "<text>Hello\n  <bold>world</bold>\n</text>", "Hello world"),
        ("tight_tags", "<text>2<superscript>nd</superscript></text>", "2nd"),
        ("tabs_and_runs", "<text>a \t\r\n b</text>", "a b"),
        ("comment_disregarded", "<text>a<!-- note -->b c <!-- note --> d</text>", "ab c d"),
        (
            "element_head_disregarded",
            "<text>\n  <location value=\"1\"/>\n  a\n  <label value=\"x\"/>\n</text>",
            "a",
        ),
        ("newline_before_tag", "<text>Hi\n<bold>there</bold></text>", "Hi there"),
        ("inline_rtl", "<text>Check <rtl>out</rtl> these</text>", "Check out these"),
        ("empty_content_between_spaces", "<text>a <content></content> b</text>", "a b"),
        ("whitespace_only_formatting_run", "<text>a<italic> </italic></text>", "a"),
    ];
    let mut failures = Vec::new();
    for (name, body, want) in cases {
        let got = read_body(body).export_to_text();
        if got != *want {
            failures.push(format!("{name}: {got:?} != {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `test_doclang_literal_readings`: what an export may normalize, read from
/// the item — a no-break or em space is no whitespace, `<content>` literal.
#[test]
fn literal_readings() {
    let cases: &[(&str, &str)] = &[
        ("<text>\u{a0}a\u{a0}</text>", "\u{a0}a\u{a0}"),
        ("<text>\u{2003}a\u{2003}</text>", "\u{2003}a\u{2003}"),
        ("<heading><content> a </content></heading>", " a "),
        (
            "<picture><caption><content> a </content></caption></picture>",
            " a ",
        ),
    ];
    for (body, want) in cases {
        assert_eq!(texts(&read_body(body))[0].0, *want, "{body}");
    }
}

/// `test_doclang_checkbox_text_is_the_run_after_it`: a checkbox item's text
/// is the plain run right after the symbol, verbatim, a leading space
/// included; a missing class is unselected.
#[test]
fn checkbox_text_is_the_run_after_it() {
    let unsel = "checkbox_unselected";
    let sel = "checkbox_selected";
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("<text><checkbox/> y</text>", &[(" y", unsel)]),
        ("<text><checkbox/>y</text>", &[("y", unsel)]),
        (
            "<text><checkbox class=\"selected\"/> y</text>",
            &[(" y", sel)],
        ),
        (
            "<text><checkbox/><content> y</content></text>",
            &[(" y", unsel)],
        ),
        ("<text><checkbox/></text>", &[("", unsel)]),
        ("<text>y <checkbox/></text>", &[("y ", "text"), ("", unsel)]),
        (
            "<text>a <checkbox/> b</text>",
            &[("a ", "text"), (" b", unsel)],
        ),
        (
            "<text><checkbox/><bold>y</bold></text>",
            &[("", unsel), ("y", "text")],
        ),
    ];
    let mut failures = Vec::new();
    for (body, want) in cases {
        let got = texts(&read_body(body));
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(t, l)| (t.to_string(), l.to_string()))
            .collect();
        if got != want {
            failures.push(format!("{body}: {got:?} != {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `test_markdown_checkbox_does_not_double_a_space`: Markdown needs a space
/// after the box, unless the text brings its own.
#[test]
fn markdown_checkbox_does_not_double_a_space() {
    for text in ["y", " y"] {
        let mut doc = DoclingDocument::new("t");
        doc.push(Node::CheckboxItem {
            checked: true,
            text: text.into(),
        });
        assert_eq!(doc.export_to_markdown().trim_end(), "- [x] y", "{text:?}");
    }
    assert_eq!(
        read_body("<text><checkbox class=\"selected\"/> y</text>")
            .export_to_markdown()
            .trim_end(),
        "- [x] y"
    );
}

/// `test_track_is_skipped_with_a_warning`: a media track is not supported
/// yet — none of its content leaks into the body.
#[test]
fn track_is_skipped() {
    let doc = read(
        "<doclang version=\"0.8\"><text>Before</text><track><caption>Talk</caption>\
         <bdiv/><seconds value=\"0\"/><seconds value=\"4\"/><chapter><text>Intro</text></chapter>Hello world\
         <bdiv/><seconds value=\"4\"/>Second cue</track><text>After</text></doclang>",
    );
    assert_eq!(doc.export_to_markdown().trim_end(), "Before\n\nAfter");
    assert_eq!(
        texts(&doc),
        [
            ("Before".into(), "text".into()),
            ("After".into(), "text".into())
        ]
    );
}

/// `test_doclang_invisible_layer_stays_out_of_furniture_export`: DocLang has
/// no notes or invisible layer — written as furniture and background
/// (docling-core#800) — and a background item read back stays out of a
/// body + furniture export.
#[test]
fn invisible_layer_stays_out_of_furniture_export() {
    let mut doc = DoclingDocument::new("layers");
    doc.push(Node::Paragraph {
        text: "visible body".into(),
    });
    for (layer, text) in [
        (ContentLayer::Furniture, "page header"),
        (ContentLayer::Notes, "speaker note"),
        (ContentLayer::Invisible, "hidden text"),
    ] {
        doc.push(Node::Furniture {
            layer,
            inner: Box::new(Node::Paragraph { text: text.into() }),
        });
    }
    let xml = doc.export_to_doclang();
    assert!(xml.contains("<layer value=\"background\"/>"), "{xml}");
    assert!(
        !xml.contains("\"notes\"") && !xml.contains("\"invisible\""),
        "{xml}"
    );
    let back = read(&xml);
    let v: Value = serde_json::from_str(&back.export_to_json()).unwrap();
    let layers: Vec<&str> = v["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["content_layer"].as_str().unwrap())
        .collect();
    assert_eq!(layers, ["body", "furniture", "furniture", "background"]);
    let opts = MarkdownExportOptions {
        layers: ContentLayers::BODY.with(ContentLayer::Furniture),
        ..Default::default()
    };
    assert_eq!(
        back.export_to_markdown_with_options(&opts).0.trim_end(),
        "visible body\n\npage header\n\nspeaker note"
    );
}

/// docling-core#880: every head caption of a float is read, and a float
/// group's footnotes become the float's own (`multi_captions_footnotes`'s
/// four picture captions and four footnotes, not one caption).
#[test]
fn float_keeps_all_captions_and_its_footnotes() {
    let doc = read(
        "<doclang version=\"0.9\"><group><footnote>above</footnote><picture>\
         <caption>c1</caption><caption>c2</caption></picture><footnote>below</footnote></group></doclang>",
    );
    let v: Value = serde_json::from_str(&doc.export_to_json()).unwrap();
    let refs = |key: &str| -> Vec<String> {
        v["pictures"][0][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["$ref"].as_str().unwrap().to_string())
            .collect()
    };
    let text_of = |r: &str| -> String {
        let i: usize = r.rsplit('/').next().unwrap().parse().unwrap();
        v["texts"][i]["text"].as_str().unwrap().to_string()
    };
    let captions: Vec<String> = refs("captions").iter().map(|r| text_of(r)).collect();
    let footnotes: Vec<String> = refs("footnotes").iter().map(|r| text_of(r)).collect();
    assert_eq!(captions, ["c1", "c2"]);
    assert_eq!(footnotes, ["above", "below"]);
    assert_eq!(
        doc.export_to_markdown().trim_end(),
        "c1 c2\n\n<!-- image -->\n\nabove\n\nbelow"
    );
    let html = doc.export_to_html();
    assert!(
        html.contains(
            "<div class=\"footnote\">above</div><div class=\"footnote\">below</div></figure>"
        ),
        "{html}"
    );
}
