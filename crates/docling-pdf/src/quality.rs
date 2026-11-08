//! Per-page conversion-confidence scoring (#183) — the Rust port of docling's
//! confidence plumbing: `rate_text_quality` from the page-preprocessing stage
//! (parse quality of the extracted text layer) and the layout/OCR score
//! aggregation the layout-postprocessing and OCR stages assign.

use std::sync::OnceLock;

use docling_core::confidence::{nanmean, nanquantile, PageConfidence};
use regex::Regex;

use crate::layout::Region;
use crate::pdfium_backend::TextCell;

/// docling's `rate_text_quality`: score one text cell's extraction quality.
/// Hard garbage (replacement chars, `GLYPH<..>` references, `/G123`-style
/// glyph ids, mostly-slash-number content) is a 0.0; otherwise fragmented-word
/// runs (`W/or.ds/sp.lit` artifacts) are penalised 0.1 each once at least
/// three appear.
pub fn rate_text_quality(text: &str) -> f64 {
    static GLYPH_RE: OnceLock<Regex> = OnceLock::new();
    static SLASH_G_RE: OnceLock<Regex> = OnceLock::new();
    static FRAG_RE: OnceLock<Regex> = OnceLock::new();
    static SLASH_NUMBER_GARBAGE_RE: OnceLock<Regex> = OnceLock::new();
    let glyph = GLYPH_RE.get_or_init(|| Regex::new(r"GLYPH<[0-9A-Fa-f]+>").unwrap());
    let slash_g = SLASH_G_RE.get_or_init(|| Regex::new(r"(?:/G\d+){2,}").unwrap());
    let frag =
        FRAG_RE.get_or_init(|| Regex::new(r"\b[A-Za-z](?:/[a-z]{1,3}\.[a-z]{1,3}){2,}\b").unwrap());
    // Python's `.match` anchors at the start of the string only.
    let slash_number =
        SLASH_NUMBER_GARBAGE_RE.get_or_init(|| Regex::new(r"^(?:/\w+\s*){2,}").unwrap());

    if text.contains('\u{fffd}')
        || glyph.is_match(text)
        || slash_g.is_match(text)
        || slash_number.is_match(text)
    {
        return 0.0;
    }
    let frag_matches = frag.find_iter(text).count();
    let mut penalty = 0.0;
    if frag_matches >= 3 {
        penalty += 0.1 * frag_matches as f64;
    }
    (1.0 - penalty).max(0.0)
}

/// The page `parse_score`: the 10th-percentile `rate_text_quality` over the
/// extracted text-layer cells (the quantile emphasises problem cells, matching
/// docling's page-preprocessing stage). Unset when the page has no text layer
/// (a scanned page — docling's `nanquantile([])` is `NaN` there too).
pub fn parse_score(cells: &[TextCell]) -> Option<f64> {
    let scores: Vec<Option<f64>> = cells
        .iter()
        .map(|c| Some(rate_text_quality(&c.text)))
        .collect();
    nanquantile(&scores, 0.10)
}

/// docling's `is_broken_text_layer` (docling#4654, #1006): whether a page's
/// text layer is one a broken font mapping produced rather than the text drawn
/// on the page — what the opt-in `replace_broken_text_layer` OCRs instead.
/// `text` is the page's cells joined by `\n`. Two checks hold for every
/// script (control / U+FFFD / private-use characters on the page, or U+FFFD /
/// control characters in many of its lines); two for Latin letters only, with
/// no language profile (mostly U+00C0–U+024F accented letters — an 8-bit
/// non-Latin font read as Latin-1 — or a vowel share under 0.25 — a ToUnicode
/// map that moved the letters). When unsure it says `false`, so pages keep
/// their PDF text. Python's character classes are reproduced where they
/// differ from Rust's: `str.isspace` also counts U+001C–U+001F, and
/// `splitlines` also breaks on them, on U+000B/U+000C, U+0085 and
/// U+2028/U+2029.
pub fn is_broken_text_layer(text: &str) -> bool {
    const MIN_CHARS: usize = 20;
    const JUNK_SHARE: f64 = 0.03;
    const JUNK_LINE_SHARE: f64 = 0.10;
    const MIN_JUNK_LINES: f64 = 3.0;

    let stripped = text.trim_matches(py_isspace);
    if stripped.chars().count() < MIN_CHARS {
        return false;
    }
    let lines: Vec<&str> = py_splitlines(stripped)
        .into_iter()
        .map(without_bullet)
        .collect();
    let chars: Vec<char> = lines
        .iter()
        .flat_map(|l| l.chars())
        .filter(|&c| !py_isspace(c))
        .collect();
    if chars.is_empty() {
        return false;
    }
    // A font whose glyphs were mapped to control, private-use or no code points.
    let junk = chars.iter().filter(|&&c| is_junk(c)).count();
    if junk as f64 >= JUNK_SHARE * chars.len() as f64 {
        return true;
    }
    // The same in many lines, such as unmapped ligatures (`publica�on`).
    let junk_lines = lines.iter().filter(|l| has_replacement(l)).count();
    if junk_lines as f64 >= MIN_JUNK_LINES.max(JUNK_LINE_SHARE * lines.len() as f64) {
        return true;
    }
    is_wrong_latin(stripped)
}

/// Python's `str.isspace`: Unicode whitespace plus the information
/// separators U+001C–U+001F, which Rust does not count.
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `str.splitlines()` (no `keepends`).
fn py_splitlines(s: &str) -> Vec<&str> {
    let is_break = |c: char| {
        matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        )
    };
    let mut out = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if is_break(c) {
            out.push(&s[start..i]);
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = iter.peek() {
                    iter.next();
                    end = j + 1;
                }
            }
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn is_private_use(c: char) -> bool {
    matches!(c as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
}

/// A control (other than whitespace), private-use or U+FFFD character
/// (surrogates, docling's `Cs`, cannot occur in a Rust string).
fn is_junk(c: char) -> bool {
    c == '\u{fffd}' || is_private_use(c) || (c.is_control() && !py_isspace(c))
}

/// U+FFFD or a control character after the line's first character —
/// private-use is not counted here: icon fonts put it in many lines.
fn has_replacement(line: &str) -> bool {
    line.trim_start_matches(py_isspace)
        .chars()
        .skip(1)
        .any(|c| c == '\u{fffd}' || (c.is_control() && !py_isspace(c)))
}

/// The line without a lone junk character at its start: an unmapped bullet
/// glyph before the item text, where a broken font puts junk inside words.
fn without_bullet(line: &str) -> &str {
    let stripped = line.trim_start_matches(py_isspace);
    let mut it = stripped.chars();
    match (it.next(), it.next()) {
        (Some(first), Some(second)) if is_junk(first) && py_isspace(second) => {
            &stripped[first.len_utf8()..]
        }
        (Some(first), None) if is_junk(first) => "",
        _ => line,
    }
}

/// A letter whose Unicode name starts with `LATIN` (docling tests
/// `"LATIN" in unicodedata.name(ch)`): the Latin blocks' letters, IPA and
/// the phonetic extensions included; `ª`/`º`/`µ` are not named Latin.
fn is_latin(c: char) -> bool {
    if !c.is_alphabetic() {
        return false;
    }
    matches!(c as u32,
        0x41..=0x5A | 0x61..=0x7A
        | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x24F
        | 0x250..=0x2AF
        | 0x1D00..=0x1DBF
        | 0x1E00..=0x1EFF
        | 0x2C60..=0x2C7F
        | 0xA720..=0xA7FF
        | 0xAB30..=0xAB6F
        | 0xFB00..=0xFB06
        | 0xFF21..=0xFF3A | 0xFF41..=0xFF5A)
}

/// Whether the Latin letters stand for another script or were moved: mostly
/// U+00C0–U+024F letters (`Îò÷ò`), or too few vowels among word letters.
/// Accented vowels count by their base letter, so no Latin-script language
/// is penalized.
fn is_wrong_latin(text: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    const MIN_LATIN_LETTERS: usize = 50;
    const ACCENTED_LATIN_SHARE: f64 = 0.5;
    const MIN_WORD_LETTERS: usize = 200;
    const MIN_VOWEL_SHARE: f64 = 0.25;
    static PLACEHOLDER: OnceLock<Regex> = OnceLock::new();
    let placeholder = PLACEHOLDER.get_or_init(|| Regex::new(r"GLYPH<[^>]*>").unwrap());
    let text = placeholder.replace_all(text, " ");

    let latin: Vec<char> = text.chars().filter(|&c| is_latin(c)).collect();
    if latin.len() < MIN_LATIN_LETTERS {
        return false;
    }
    let accented = latin
        .iter()
        .filter(|&&c| ('\u{c0}'..='\u{24f}').contains(&c))
        .count();
    if accented as f64 >= ACCENTED_LATIN_SHARE * latin.len() as f64 {
        return true;
    }
    // Tokens of 3–20 Latin letters with a lowercase one: shorter tokens are
    // math variables, longer ones base64 or hashes.
    let mut words: Vec<char> = Vec::new();
    for token in text.split(py_isspace).filter(|t| !t.is_empty()) {
        let letters: Vec<char> = token.chars().filter(|&c| is_latin(c)).collect();
        if (3..=20).contains(&letters.len()) && letters.iter().any(|c| c.is_lowercase()) {
            words.extend(letters);
        }
    }
    if words.len() < MIN_WORD_LETTERS {
        return false;
    }
    let vowels = words
        .iter()
        .filter(|&&c| {
            let base = std::iter::once(c).nfd().next().unwrap_or(c);
            let lower = base.to_lowercase().next().unwrap_or(base);
            matches!(
                lower,
                'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'æ' | 'œ' | 'ø' | 'ı' | 'ə'
            )
        })
        .count();
    (vowels as f64) < MIN_VOWEL_SHARE * words.len() as f64
}

/// The page `layout_score`: mean confidence of the final (postprocessed)
/// layout regions — `Some(0.0)` for a region-less page, exactly like
/// docling's `float(np.mean(...)) if clusters else 0.0`.
///
/// Orphan-text regions carry the sentinel score `0.0` (detector scores always
/// clear their ≥ 0.3 label thresholds, so 0.0 can only mean "rescued cell").
/// docling scores an orphan cluster with its *cell's* confidence: 1.0 for a
/// text-layer cell, the recognition confidence for an OCR cell — substitute
/// accordingly (`ocr_mean` is the page's mean OCR confidence when the cells
/// came from OCR, `None` on a digital page).
pub fn layout_score(regions: &[Region], ocr_mean: Option<f64>) -> Option<f64> {
    if regions.is_empty() {
        return Some(0.0);
    }
    let scores: Vec<Option<f64>> = regions
        .iter()
        .map(|r| {
            if r.score == 0.0 {
                Some(ocr_mean.unwrap_or(1.0))
            } else {
                Some(r.score as f64)
            }
        })
        .collect();
    nanmean(&scores)
}

/// The page `ocr_score`: mean recognition confidence over the page's OCR'd
/// cells; unset when nothing on the page came from OCR (docling only assigns
/// it when OCR cells exist).
pub fn ocr_score(confs: &[f32]) -> Option<f64> {
    if confs.is_empty() {
        return None;
    }
    Some(confs.iter().map(|&c| c as f64).sum::<f64>() / confs.len() as f64)
}

/// Assemble the page's [`PageConfidence`] (`table_score` stays unset — docling
/// never assigns it either).
pub fn page_confidence(
    parse: Option<f64>,
    regions: &[Region],
    ocr_confs: &[f32],
) -> PageConfidence {
    let ocr = ocr_score(ocr_confs);
    PageConfidence {
        parse_score: parse,
        layout_score: layout_score(regions, ocr),
        table_score: None,
        ocr_score: ocr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_quality_matches_docling_rules() {
        assert_eq!(rate_text_quality("A perfectly ordinary sentence."), 1.0);
        // Hard garbage → 0.0.
        assert_eq!(rate_text_quality("bad \u{fffd} char"), 0.0);
        assert_eq!(rate_text_quality("see GLYPH<0a3F> here"), 0.0);
        assert_eq!(rate_text_quality("/G12/G34 rest"), 0.0);
        assert_eq!(rate_text_quality("/x1 /y2 tail"), 0.0);
        // The slash-number pattern only fires anchored at the start.
        assert_eq!(rate_text_quality("word /x1 /y2"), 1.0);
        // Fragmented words (a letter then ≥2 `/xx.yy` runs): below three
        // matches no penalty applies; at three, 0.1 each.
        let frag = "W/or.ds/sp.lit";
        assert_eq!(rate_text_quality(frag), 1.0, "one match is tolerated");
        assert_eq!(rate_text_quality(&format!("{frag} {frag}")), 1.0);
        let three = format!("{frag} {frag} {frag}");
        assert!((rate_text_quality(&three) - 0.7).abs() < 1e-12, "{three}");
    }

    #[test]
    fn parse_score_is_tenth_percentile() {
        let cell = |text: &str| TextCell {
            text: text.into(),
            l: 0.0,
            t: 0.0,
            r: 1.0,
            b: 1.0,
        };
        assert_eq!(parse_score(&[]), None);
        // Nine clean cells and one garbage cell: the 10th percentile sits at
        // the interpolation between the sorted [0.0, 1.0 × 9] head.
        let mut cells = vec![cell("ok"); 9];
        cells.push(cell("GLYPH<12>"));
        let s = parse_score(&cells).unwrap();
        assert!((s - 0.9).abs() < 1e-9, "{s}");
    }

    #[test]
    fn layout_score_substitutes_orphan_sentinel() {
        let region = |score: f32| Region {
            label: "text",
            score,
            l: 0.0,
            t: 0.0,
            r: 1.0,
            b: 1.0,
        };
        assert_eq!(layout_score(&[], None), Some(0.0));
        // Digital page: the 0.0-score orphan counts as a 1.0-confidence cell.
        // (Tolerance covers the f32 detector score widened to f64.)
        let s = layout_score(&[region(0.8), region(0.0)], None).unwrap();
        assert!((s - 0.9).abs() < 1e-6, "{s}");
        // OCR'd page: orphans inherit the page's mean OCR confidence.
        let s = layout_score(&[region(0.8), region(0.0)], Some(0.6)).unwrap();
        assert!((s - 0.7).abs() < 1e-6, "{s}");
    }

    // ---- is_broken_text_layer (#1006): docling's tests/test_text_quality.py.

    /// Words made of the letters in a code-point range — a stand-in for a script.
    fn words(first: u32, last: u32, count: usize) -> String {
        let letters: Vec<char> = (first..=last)
            .filter_map(char::from_u32)
            .filter(|c| c.is_alphabetic())
            .collect();
        (0..count)
            .map(|i| {
                (0..5)
                    .map(|j| letters[(i + j) % letters.len()])
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn prose() -> String {
        "The quarterly report shows that revenue increased across all regions, \
         driven by strong demand for cloud services and improved margins. "
            .repeat(3)
    }

    /// Letters moved by a constant, as a broken ToUnicode map does.
    fn shift(text: &str, offset: i32) -> String {
        text.chars()
            .map(|c| {
                if c.is_alphabetic() {
                    char::from_u32((c as i32 + offset) as u32).unwrap()
                } else {
                    c
                }
            })
            .collect()
    }

    fn welsh() -> String {
        "Mae'r llywodraeth wedi cyhoeddi cynllun newydd i gefnogi busnesau bach \
         yng nghefn gwlad Cymru dros y blynyddoedd nesaf. "
            .repeat(3)
    }

    fn ordinary() -> Vec<(&'static str, String)> {
        let p = prose();
        vec![
            ("latex", r"The loss $L = \frac{1}{n}\sum_{i=1}^{n} (y_i - \hat{y}_i)^2$ with \alpha \in [0,1] and \beta^{2} \leq \gamma_{k}. ".repeat(3)),
            ("shell", "export PATH=$HOME/bin:$PATH; cd ${USER}/work && ls ~/docs/*.md | grep -v '#draft' > ~/out/${DATE}.log; echo $? ".repeat(3)),
            ("python", "def f(x):\n    return {k: v**2 for k, v in x.items() if v > 0}\nprint(f({'a': 1, 'b': -2}))\n".repeat(3)),
            ("json", "{\"id\": 42, \"tags\": [\"a\", \"b\"], \"meta\": {\"ok\": true}, \"path\": \"/v/a.log\"}\n".repeat(3)),
            ("markdown_table", "| Metric | Value |\n|---|---|\n| F1 | 0.91 |\n**Note:** see `config.yaml` and [docs](https://x.y/z).\n".repeat(3)),
            ("dot_leaders", "Net revenues . . . . . . $ 12,345 $ 11,234 (1)\nOperating income . . . . 2,345 (1,987)\n".repeat(5)),
            ("caption", "Q3 2024: $12,345 (+5.2%) vs 11,734 in the prior year".into()),
            ("units", "RDS(ON) < 8.7 m\u{2126} at VGS=10V; TJ = \u{2212}55 to 150 \u{b0}C; t \u{2264} 10 \u{b5}s. ".repeat(3)),
            ("bullets_and_checkboxes", "\u{2022} Dallas (DFW4)\n\u{25aa} Chicago\n\u{2610} No \u{2612} Yes\n______________\n".repeat(4)),
            ("icon_font_glyphs", "\u{f002} Brabant Konzession, Teil von Arriva. ".repeat(4)),
            ("accented_european", "L\u{2019}\u{c9}cole doctorale accueille aujourd\u{2019}hui des \u{e9}tudiants de la r\u{e9}gion. ".repeat(4)),
            ("greek", words(0x03B1, 0x03C9, 40)),
            ("cyrillic", words(0x0430, 0x044F, 40)),
            ("hebrew", words(0x05D0, 0x05EA, 40)),
            ("arabic", words(0x0627, 0x064A, 40)),
            ("thai", words(0x0E01, 0x0E2E, 40)),
            ("devanagari", words(0x0915, 0x0939, 40)),
            ("cjk", words(0x4E00, 0x4E40, 40)),
            ("english_prose", p.clone()),
            ("czech", "Vl\u{e1}da schv\u{e1}lila n\u{e1}vrh z\u{e1}kona, kter\u{fd} zjednodu\u{161}uje stavebn\u{ed} \u{159}\u{ed}zen\u{ed} a podporuje v\u{fd}stavbu nov\u{fd}ch byt\u{16f} ve velk\u{fd}ch m\u{11b}stech. ".repeat(3)),
            ("welsh", welsh()),
            ("vietnamese", "\u{110}\u{1eeb}\u{1a1}ng \u{111}\u{1ec1}n t\u{1b0}\u{1a1}ng lai c\u{1ee7}a nh\u{1eef}ng ng\u{1b0}\u{1edd}i d\u{e2}n Vi\u{1ec7}t Nam r\u{1ea7}t d\u{e0}i. ".repeat(4)),
            ("c_code", "for (int i = 0; i < n; i++) { ptr->buf[i] = xmm_mul(cfg.k, src[i]); if (!chk(ptr)) return -EINVAL; } ".repeat(4)),
            ("part_numbers", "STM32F407VGT6 LQFP100 GPIO PB12 SPI2_NSS PB13 SPI2_SCK TIM1_CH1N HCLK PCLK1 PCLK2 RCC_CFGR SYSCLK PLLM PLLN\n".repeat(4)),
            ("base64", "TWFueSBoYW5kcyBtYWtlIGxpZ2h0IHdvcmsuIFRoZSBxdWljayBicm93biBmb3gganVtcHM=\n".repeat(5)),
            ("dna", "ACGTTGCAAGCTTGCATGCCTGCAGGTCGACTCTAGAGGATCCCCGGGTACCGAGCTCGAATTC\n".repeat(5)),
            ("icon_rating_and_footer", format!(
                "{p}\n{}\u{f0aa}\n{}",
                "\u{f0ab}".repeat(4),
                (0..5).map(|i| char::from_u32(0xF09A + i).unwrap().to_string()).collect::<Vec<_>>().join(" ")
            )),
            ("single_replacement_character", format!("{p}\nx \u{fffd} y\n")),
            ("hyphen_control", p.replacen("regions", "re\u{2}\ngions", 1)),
            ("glyph_placeholders", format!("{p}{}", "\nGLYPH<s45>GLYPH<s52>GLYPH<s48>".repeat(60))),
            ("math_variables", "(p, q, k) and (p, k, q)\nPP[(q)] = 5 e2 [(k2 g - k k) - (q2 g - q q)]\n= 2 e sin(p ^ q) B (p, q, k) = i2 g sin(k ^ q) (p, q, k) ln(s) ln(p) mn pq\n".repeat(6)),
            ("dotted_capital_i", format!("{p}\nThe office in \u{130}stanbul opened.\n")),
            ("ipa_vowel_chart", ["\u{26a} \u{28f} \u{28a} e \u{f8} \u{259} \u{250} \u{e6} a \u{251} \u{252} \u{28c} \u{254} \u{153} \u{25c} \u{25e} \u{264} o \u{258} \u{275} \u{26f} \u{268} \u{289} y u"; 3].join("\n")),
            ("bullet_replacement_markers", (0..3).map(|n| format!("\u{fffd} Revenue grew in region {n}"))
                .chain((0..22).map(|n| format!("Line {n}: the regional teams reported stable demand.")))
                .collect::<Vec<_>>().join("\n")),
            ("bullet_slide", format!("Quarterly results\n{}", ["Revenue grew by 5%", "Costs fell", "Margins improved"]
                .iter().map(|i| format!("\u{fffd}\n{i}")).collect::<Vec<_>>().join("\n"))),
            ("bullet_agenda", format!("Agenda\n{}", ["Welcome", "Results", "Outlook", "Questions", "Close"]
                .iter().map(|i| format!("\u{fffd} {i}")).collect::<Vec<_>>().join("\n"))),
            ("one_private_use_cell", format!("{}\n{}\n", p.repeat(2),
                (0..6).map(|i| char::from_u32(0xF041 + i).unwrap()).collect::<String>())),
            ("one_control_code_cell", format!("{}\nTotal {}\n", p.repeat(2),
                (0..6).map(|i| char::from_u32(0x10 + i).unwrap()).collect::<String>())),
        ]
    }

    fn broken() -> Vec<(&'static str, String)> {
        let p = prose();
        vec![
            ("glyphs_as_control_codes", format!("Net assets 2,635 2,849\n{}\nTotal 3,004\n",
                (0..40).map(|i| char::from_u32(0x10 + i % 15).unwrap()).collect::<String>())),
            ("replacement_characters", "Annual report \u{fffd}\u{fffd} 2024 ".repeat(6)),
            ("glyphs_as_private_use", (0..30).map(|i| (0..4).map(|j| char::from_u32(0xF021 + (i + j) % 60).unwrap()).collect::<String>())
                .collect::<Vec<_>>().join(" ")),
            ("glyphs_as_accented_latin", words(0x0100, 0x017F, 40)),
            ("glyphs_as_shifted_letters", shift(&p, 3)),
            ("glyphs_as_letters_and_digits", shift(&p, -11)),
            ("welsh_as_shifted_letters", shift(&welsh(), 3)),
            ("replacement_characters_in_many_cells", format!("{p}\n{}",
                [4, 3, 3].iter().map(|&n| format!("Item {}", "\u{fffd}".repeat(n))).collect::<Vec<_>>().join("\n"))),
            ("one_replacement_in_many_lines", ["BIB@QOLKF@ PB@QOFQFBP QO>AFKD\u{fffd} IBKAFKD >KA M>VJBKQ PBOSF@BP CLO @LJM>KFBP >KA FKAFSFA\u{a7}>IP"; 8].join("\n")),
        ]
    }

    #[test]
    fn ordinary_text_layers_are_kept() {
        for (name, text) in ordinary() {
            assert!(!is_broken_text_layer(&text), "{name} judged broken");
        }
    }

    #[test]
    fn unmapped_fonts_are_detected() {
        for (name, text) in broken() {
            assert!(is_broken_text_layer(&text), "{name} not detected");
        }
    }

    #[test]
    fn short_or_empty_text_is_left_alone() {
        assert!(!is_broken_text_layer(""));
        assert!(!is_broken_text_layer("\u{1}\u{2}\u{3}"));
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(py_splitlines("a\r\nb\rc\u{1c}d\n"), ["a", "b", "c", "d"]);
        assert_eq!(py_splitlines("a\n\nb"), ["a", "", "b"]);
    }
}
