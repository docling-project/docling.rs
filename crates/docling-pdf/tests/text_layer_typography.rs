//! Text-layer typography regressions (#919–#923): synthetic one-page PDFs run
//! through the pure-Rust `convert_text_layer` (no models, so no `ml` gate).

#[path = "support/one_page_pdf.rs"]
mod one_page_pdf;
use one_page_pdf::pdf_bytes;

fn pdf(content: &str) -> Vec<u8> {
    pdf_bytes(content.as_bytes())
}

fn text_of(pdf: &[u8]) -> String {
    docling_pdf::convert_text_layer(pdf, "t.pdf")
        .expect("text-layer conversion")
        .export_to_text()
}

/// Long enough to be a text layer, not a vestigial form.
const BODY: &str = "Der Betriebsrat hat die Belegschaft ueber die geplanten Massnahmen rechtzeitig zu unterrichten.";

fn body_line(y: i32) -> String {
    format!("BT /F1 12 Tf 72 {y} Td ({BODY}) Tj ET\n")
}

/// #922: WinAnsi 0x84 (`„`) is in the 0x80–0x9F row the table lacked; the
/// closing `“` (0x93) was kept, the opening mark vanished. docling's sanitizer
/// maps both to `"` (`clean_text`; `'` before docling-parse#349).
#[test]
fn low_opening_quote_survives() {
    let content = format!(
        "{}BT /F1 12 Tf 72 680 Td (Teilen \\204Gewinn oder Verlust\\223 und \\204sonstiges\\223 Ergebnis) Tj ET",
        body_line(700)
    );
    let text = text_of(&pdf(&content));
    assert!(
        text.contains("Teilen \"Gewinn oder Verlust\" und \"sonstiges\" Ergebnis"),
        "{text:?}"
    );
}

/// #921: `pdf_keep_typography` keeps `„ “ ‚ ‘` and the dashes as drawn — a
/// legal citation's quotes stay distinguishable from ASCII ones.
#[test]
fn typography_is_kept_when_asked() {
    let content = format!(
        "{}BT /F1 12 Tf 72 680 Td (Teilen \\204Gewinn\\223 und \\202Verlust\\221 \\226 \\24710\\226\\24712) Tj ET",
        body_line(700)
    );
    let pdf = pdf(&content);
    let text = |keep| {
        docling_pdf::convert_text_layer_pages_opts(
            &pdf,
            "t.pdf",
            None,
            docling_pdf::TextOptions {
                word_gap: None,
                keep_typography: keep,
                ..Default::default()
            },
        )
        .expect("text-layer conversion")
        .export_to_text()
    };
    let kept = text(true);
    assert!(
        kept.contains("Teilen \u{201e}Gewinn\u{201c} und \u{201a}Verlust\u{2018} \u{2013} \u{a7}10\u{2013}\u{a7}12"),
        "{kept:?}"
    );
    let folded = text(false);
    assert!(
        folded.contains("Teilen \"Gewinn\" und ,Verlust' - \u{a7}10-\u{a7}12"),
        "{folded:?}"
    );
}

/// #922: the rest of the 0x80–0x9F row (`€`, `™`, `œ`, …) read as WinAnsi.
#[test]
fn winansi_c1_row_is_complete() {
    let content = format!(
        "{}BT /F1 12 Tf 72 680 Td (Preis \\200 5 \\231 \\234uvre \\212 \\236) Tj ET",
        body_line(700)
    );
    let text = text_of(&pdf(&content));
    assert!(
        text.contains("Preis \u{20ac} 5 \u{2122} \u{153}uvre \u{160} \u{17e}"),
        "{text:?}"
    );
}

/// #919: a soft hyphen ending a line is a word split across it -- the halves
/// fuse, with no space left where the hyphen was.
#[test]
fn soft_hyphen_at_line_end_fuses_the_word() {
    let content = format!(
        "{}BT /F1 12 Tf 72 686 Td (Die Veroeffent\\255) Tj ET\nBT /F1 12 Tf 72 672 Td (lichung der Verordnung ist wichtig.) Tj ET",
        body_line(700)
    );
    let text = text_of(&pdf(&content));
    assert!(
        text.contains("Veroeffentlichung der Verordnung"),
        "{text:?}"
    );
    assert!(!text.contains('\u{ad}'), "{text:?}");
}

/// A soft hyphen nothing continues is dropped, not left as an invisible char.
#[test]
fn unmerged_soft_hyphen_is_dropped() {
    let content = format!(
        "{}BT /F1 12 Tf 72 686 Td (Ende Veroeffent\\255) Tj ET",
        body_line(700)
    );
    let text = text_of(&pdf(&content));
    assert!(
        text.contains("Ende Veroeffent") && !text.contains('\u{ad}'),
        "{text:?}"
    );
}

/// #920: `Warte-` / `und Behaltefristen` is an elliptical compound -- the
/// hyphen and the space stay; a wrapped word still fuses.
#[test]
fn hard_hyphen_before_a_conjunction_is_kept() {
    let content = format!(
        "{}BT /F1 12 Tf 72 686 Td (Es gelten die Warte-) Tj ET\nBT /F1 12 Tf 72 672 Td (und Behaltefristen des Gesetzes.) Tj ET",
        body_line(700)
    );
    let text = text_of(&pdf(&content));
    assert!(text.contains("Warte- und Behaltefristen"), "{text:?}");

    let content = format!(
        "{}BT /F1 12 Tf 72 686 Td (Die Veroeffent-) Tj ET\nBT /F1 12 Tf 72 672 Td (lichung der Verordnung ist wichtig.) Tj ET",
        body_line(700)
    );
    let text = text_of(&pdf(&content));
    assert!(text.contains("Veroeffentlichung der"), "{text:?}");
}

/// #923: a raised note at the end of a line reads after that line, not ahead
/// of it.
#[test]
fn raised_note_follows_its_host_line() {
    let content = format!(
        "{}BT /F1 12 Tf 72 686 Td (Die Beschaeftigten, die zu entsenden sind) Tj ET\n\
         BT /F1 12 Tf 72 672 Td (und die Belegschaft ist nicht groesser als zehn besteht.) Tj ET\n\
         BT /F1 7 Tf 368 676 Td (\\(Anm. 1\\)) Tj ET",
        body_line(700)
    );
    let text = text_of(&pdf(&content));
    let host = text.find("und die Belegschaft").expect("host line");
    let note = text.find("(Anm. 1)").expect("note");
    assert!(host < note, "{text:?}");
    assert!(text.contains("besteht. (Anm. 1)"), "{text:?}");
}
