//! RTF backend (issue #209) — native, where Python docling reads RTF through
//! LibreOffice (a path it gained after this backend landed). That LibreOffice
//! route publishes groundtruth, so there *is* a reference now, if a narrow
//! one: `legacy_sample` is byte-exact against it (#387). Everything the
//! upstream corpus does not cover follows the DOCX backend's shapes.
//!
//! RTF is a plain-text control-word format (`\b`, `\par`, `\trowd`, …) with
//! `{}` groups scoping formatting state, so this is a hand-rolled tokenizer in
//! the spirit of the AsciiDoc/LaTeX backends — no external parser crate, pure
//! Rust, wasm-clean. What it covers:
//!
//! - paragraphs, `**bold**` / `*italic*` / `~~strike~~` runs (baked into the
//!   text, the docling.rs convention), `\line` breaks, `\page` page breaks
//! - headings from `\outlinelevel` or the stylesheet (`\sN` whose stylesheet
//!   name is "heading N")
//! - lists via the `\listtext`/`\pntext` compatibility markers (`·`-style
//!   markers → bullets, `1.`-style → ordered items with their number)
//! - tables: `\trowd` … `\cell` … `\row` rows, ragged rows padded; nested
//!   groups inside cells contribute their text
//! - embedded pictures: `\pict` with `\pngblip`/`\jpegblip` hex data becomes a
//!   [`Node::Picture`] with the decoded bytes; `\emfblip`/`\wmetafile` ones
//!   are rendered to PNG (`{\nonshppict}` fallback copies are skipped)
//! - equations: `{\mmath{\*\moMath …}}` — OMML in control-word form — is
//!   rebuilt as OMML and converted to LaTeX by the DOCX backend's `omml.rs`
//!   (inline `$…$`, or a formula when the paragraph holds nothing else); the
//!   `{\mmathPict}` fallback picture is skipped (#578)
//! - encodings: `\'xx` bytes through the `\ansicpg` codepage (1252 default,
//!   1250/1251 supported), `\uN` unicode with the `\uc` skip protocol
//!
//! Header/footer/info/font/color destinations are skipped, as are unknown
//! `{\*\…}` destinations (per spec, a reader that does not understand a
//! starred destination must ignore it). Fields keep their `\fldrslt` (the
//! last rendered result) and drop the instruction.

use docling_core::{DoclingDocument, Node, PictureImage, Table};

use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct RtfBackend;

impl DeclarativeBackend for RtfBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        // RTF is 7-bit ASCII by design (non-ASCII travels as \'xx / \uN), but
        // real files are sometimes saved with raw high bytes — treat those
        // through the document codepage rather than failing UTF-8 validation.
        let text: String = source.bytes.iter().map(|&b| b as char).collect();
        if !text.trim_start().starts_with("{\\rtf") {
            return Err(ConversionError::Parse("rtf: missing {\\rtf header".into()));
        }
        let mut doc = DoclingDocument::new(&source.name);
        Parser::new(&text).run(&mut doc);
        Ok(doc)
    }
}

/// Per-group state, cloned on `{` and restored on `}` (RTF's scoping rule).
#[derive(Clone, Default)]
struct GroupState {
    bold: bool,
    italic: bool,
    strike: bool,
    /// Inside a destination whose content must not become body text
    /// (fonttbl, info, header, an unknown `{\*\…}`, …).
    skip: bool,
    /// `\ucN`: how many fallback characters follow each `\uN`.
    uc: usize,
    /// `\intbl`: this paragraph belongs to the current table row.
    in_table: bool,
    /// `\sN` style handle (heading lookup via the stylesheet).
    style: Option<i32>,
    /// `\outlinelevelN` (0-based).
    outline: Option<u8>,
    /// `\ilvlN` list nesting level (0-based).
    ilvl: u8,
    /// `\lsN`: the list override the paragraph belongs to — Word's list
    /// identity, what `numId` is in DOCX. Writers that emit only the
    /// `\listtext` compatibility markers leave it unset.
    ls: Option<i32>,
    /// Inside `{\nonshppict …}`: the WMF copy Word writes for readers that
    /// predate `\shppict`, of a picture already read from the `\shppict`.
    nonshppict: bool,
}

/// One formatted run of paragraph text.
struct Run {
    text: String,
    bold: bool,
    italic: bool,
    strike: bool,
    /// An equation: `text` is its LaTeX, rendered `$…$` inline (or `$$…$$`
    /// when the paragraph holds nothing else).
    math: bool,
}

/// The pending `\listtext`/`\pntext` marker for the paragraph being built:
/// `(ordered, display number, marker, multilevel prefix)`. A multilevel
/// number ("1.1.") renders as a bullet with the prefix baked into the text —
/// the DOCX backend's docling convention.
type ListMarker = (bool, u64, String, Option<String>);

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    stack: Vec<GroupState>,
    state: GroupState,
    codepage: u32,
    /// Style handle → heading level, from `{\stylesheet …}` names.
    heading_styles: Vec<(i32, u8)>,
    runs: Vec<Run>,
    list_marker: Option<ListMarker>,
    prev_was_list: bool,
    /// The previous list item's `(\ls, ordered, level)`, for the list-boundary
    /// flag (#385): a different `\ls` is a different list; without `\ls` on
    /// both sides, a marker-kind flip at the top level is.
    prev_list_key: Option<(Option<i32>, bool, u8)>,
    /// The table being assembled: completed rows (cells + their defs) and
    /// the current row's cells.
    rows: Vec<(Vec<String>, Vec<CellDef>)>,
    cells: Vec<String>,
    /// Per-cell definitions from the row prelude (`\cellx` closes one):
    /// right boundary in twips + merge-continuation flags. LibreOffice
    /// expresses horizontal merges as one *wide* cell, so the boundary grid
    /// is what recovers the column structure; Word-style `\clmrg` flags are
    /// carried too.
    cell_defs: Vec<CellDef>,
    pending_hmerge: bool,
    pending_vmerge: bool,
}

/// One `\cellx` definition from a row prelude.
#[derive(Clone, Copy)]
struct CellDef {
    right: i64,
    hcont: bool,
    vcont: bool,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Parser {
            bytes: text.as_bytes(),
            pos: 0,
            stack: Vec::new(),
            state: GroupState {
                uc: 1,
                ..GroupState::default()
            },
            codepage: 1252,
            heading_styles: Vec::new(),
            runs: Vec::new(),
            list_marker: None,
            prev_was_list: false,
            prev_list_key: None,
            rows: Vec::new(),
            cells: Vec::new(),
            cell_defs: Vec::new(),
            pending_hmerge: false,
            pending_vmerge: false,
        }
    }

    fn run(&mut self, doc: &mut DoclingDocument) {
        while let Some(b) = self.next_byte() {
            match b {
                b'{' => self.stack.push(self.state.clone()),
                b'}' => {
                    if let Some(prev) = self.stack.pop() {
                        self.state = prev;
                    }
                }
                b'\\' => self.control(doc),
                b'\r' | b'\n' => {} // literal newlines are insignificant
                _ => self.text_byte(b),
            }
        }
        self.flush_paragraph(doc);
        self.flush_table(doc);
    }

    fn next_byte(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// A control word (`\word` + optional signed integer + one optional
    /// delimiting space) or control symbol (single non-alphabetic byte).
    fn control(&mut self, doc: &mut DoclingDocument) {
        let Some(b) = self.peek() else { return };
        if !b.is_ascii_alphabetic() {
            self.pos += 1;
            self.control_symbol(b);
            return;
        }
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        let word: String = self.bytes[start..self.pos]
            .iter()
            .map(|&b| b as char)
            .collect();
        let mut param: Option<i64> = None;
        let num_start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos > num_start {
            param = String::from_utf8_lossy(&self.bytes[num_start..self.pos])
                .parse()
                .ok();
        }
        // The single space after a control word is part of the control word.
        if self.peek() == Some(b' ') {
            self.pos += 1;
        }
        self.control_word(&word, param, doc);
    }

    fn control_symbol(&mut self, b: u8) {
        match b {
            b'\'' => {
                // \'xx — one byte in the document codepage.
                let hex: String = (0..2)
                    .filter_map(|_| self.next_byte().map(|b| b as char))
                    .collect();
                if let Ok(v) = u8::from_str_radix(&hex, 16) {
                    let ch = decode_byte(v, self.codepage);
                    self.push_char(ch);
                }
            }
            b'*' => {
                // {\*\dest …}: ignorable destination. The known ones we *do*
                // read re-enable themselves in control_word.
                self.state.skip = true;
            }
            b'~' => self.push_char('\u{00A0}'),
            b'-' => {} // optional hyphen
            b'_' => self.push_char('-'),
            b'\\' | b'{' | b'}' => self.push_char(b as char),
            b'\r' | b'\n' => {} // \<newline> == \par in old writers; treat as space
            _ => {}
        }
    }

    fn control_word(&mut self, word: &str, param: Option<i64>, doc: &mut DoclingDocument) {
        match word {
            "ansicpg" => {
                if let Some(cp) = param {
                    self.codepage = cp as u32;
                }
            }
            "uc" => self.state.uc = param.unwrap_or(1).max(0) as usize,
            "u" => {
                if let Some(v) = param {
                    // Signed 16-bit: negative values wrap (e.g. -3999 → 61537).
                    let cp = if v < 0 { v + 65536 } else { v } as u32;
                    if let Some(ch) = char::from_u32(cp) {
                        self.push_char(ch);
                    }
                    self.skip_unicode_fallback();
                }
            }
            // Formatting toggles: \b on, \b0 off.
            "b" => self.state.bold = param != Some(0),
            "i" => self.state.italic = param != Some(0),
            "strike" => self.state.strike = param != Some(0),
            "plain" => {
                self.state.bold = false;
                self.state.italic = false;
                self.state.strike = false;
            }
            "s" => self.state.style = param.map(|p| p as i32),
            "outlinelevel" => self.state.outline = param.map(|p| p.clamp(0, 8) as u8),
            "ilvl" => self.state.ilvl = param.unwrap_or(0).clamp(0, 8) as u8,
            "ls" => self.state.ls = param.map(|p| p as i32),
            "pard" => {
                // Paragraph-default reset clears paragraph-scoped properties.
                self.state.style = None;
                self.state.outline = None;
                self.state.in_table = false;
                self.state.ilvl = 0;
                self.state.ls = None;
            }
            "intbl" => self.state.in_table = true,
            // Row prelude: \trowd starts the cell definitions, each \cellx
            // closes one carrying any pending merge-continuation flags.
            "trowd" => {
                self.cell_defs.clear();
                self.pending_hmerge = false;
                self.pending_vmerge = false;
            }
            "clmrg" => self.pending_hmerge = true,
            "clvmrg" => self.pending_vmerge = true,
            "cellx" => {
                self.cell_defs.push(CellDef {
                    right: param.unwrap_or(0),
                    hcont: self.pending_hmerge,
                    vcont: self.pending_vmerge,
                });
                self.pending_hmerge = false;
                self.pending_vmerge = false;
            }
            "par" => self.flush_paragraph(doc),
            "line" => self.push_char('\n'),
            "tab" => self.push_char('\t'),
            // Typographic entities Word/LibreOffice write as control words.
            "emdash" => self.push_char('\u{2014}'),
            "endash" => self.push_char('\u{2013}'),
            "bullet" => self.push_char('\u{2022}'),
            "lquote" => self.push_char('\u{2018}'),
            "rquote" => self.push_char('\u{2019}'),
            "ldblquote" => self.push_char('\u{201C}'),
            "rdblquote" => self.push_char('\u{201D}'),
            "enspace" | "emspace" | "qmspace" => self.push_char(' '),
            "zwnj" => self.push_char('\u{200C}'),
            "zwj" => self.push_char('\u{200D}'),
            "page" => {
                self.flush_paragraph(doc);
                self.flush_table(doc);
                doc.push(Node::PageBreak);
            }
            "cell" => self.end_cell(),
            "row" => self.end_row(),
            "nestcell" | "nestrow" => {} // nested tables flatten into the cell
            "stylesheet" => self.read_stylesheet(),
            "pict" if !self.state.nonshppict => self.read_picture(doc),
            // `{\mmathPict …}` is the picture of the equation that precedes
            // it, for readers without math support — skipped like a
            // `\nonshppict` copy (#578; it used to surface as an image
            // placeholder ahead of the paragraph).
            "nonshppict" | "mmathPict" => {
                self.state.skip = true;
                self.state.nonshppict = true;
            }
            // `{\mmath …}`: an equation (RTF's spelling of OMML, #578).
            "mmath" => self.read_math(),
            "fonttbl" | "colortbl" | "info" | "listtable" | "listoverridetable" | "header"
            | "headerl" | "headerr" | "headerf" | "footer" | "footerl" | "footerr" | "footerf"
            | "footnote" | "ftnsep" | "ftnsepc" => {
                self.state.skip = true;
            }
            // Text inside a shape (`{\shp{\*\shpinst …{\shptxt …}}}`) is body
            // content — re-enable it inside the otherwise-skipped shape
            // destination, the way docling renders DOCX textboxes.
            "shptxt" => self.state.skip = false,
            // A field: HYPERLINK instructions become `[result](url)` (the
            // docling DOCX convention); any other field keeps its \fldrslt
            // (the last rendered result) and drops the instruction.
            "field" => self.read_field(),
            // Fallbacks for a bare \fldinst/\fldrslt outside a \field group
            // (malformed writers): machinery skipped, result kept.
            "fldinst" => self.state.skip = true,
            "fldrslt" => self.state.skip = false,
            // The list-marker compatibility text: capture it to type the
            // paragraph, but keep it out of the body text.
            "listtext" | "pntext" => {
                let marker = self.capture_group_text();
                self.list_marker = Some(classify_marker(&marker));
            }
            _ => {} // unknown control words are ignored per spec
        }
    }

    /// `{\mmath{\*\moMath …}}` — read the rest of the group, rebuild the
    /// OMML it spells (see [`rtf_math_to_omml`]) and push each equation's
    /// LaTeX as a math run, through the converter the DOCX backend uses.
    /// A group with no equation in it — LibreOffice writes
    /// `{\mmath {\*\shppict …}}` for a formula it could only keep as a
    /// picture — is left to the main loop, so that picture still surfaces.
    fn read_math(&mut self) {
        if self.state.skip {
            return;
        }
        let start = self.pos;
        let end = group_end(self.bytes, start);
        let xml = rtf_math_to_omml(&self.bytes[start..end], self.codepage);
        let Ok(tree) = roxmltree::Document::parse(&xml) else {
            return;
        };
        let equations: Vec<String> = tree
            .descendants()
            .filter(|n| n.has_tag_name("oMath"))
            .map(crate::backend::omml::to_latex)
            .filter(|latex| !latex.is_empty())
            .collect();
        if equations.is_empty() {
            return;
        }
        self.pos = end; // the closing `}` is left to the main loop
        self.runs.extend(equations.into_iter().map(|text| Run {
            text,
            bold: false,
            italic: false,
            strike: false,
            math: true,
        }));
    }

    /// After `\uN`, skip the next `uc` fallback characters (each `\'xx`
    /// counts as one).
    fn skip_unicode_fallback(&mut self) {
        for _ in 0..self.state.uc {
            match self.peek() {
                Some(b'\\') if self.bytes.get(self.pos + 1) == Some(&b'\'') => {
                    self.pos += 4; // \'xx
                }
                Some(b'{') | Some(b'}') | Some(b'\\') | None => break,
                _ => self.pos += 1,
            }
        }
    }

    /// Consume the rest of the current group (after `\listtext`-style words)
    /// and return its plain text; formatting and destinations inside are
    /// dropped. Assumes the group's `{` was already consumed.
    fn capture_group_text(&mut self) -> String {
        let mut depth = 0usize;
        let mut out = String::new();
        while let Some(b) = self.next_byte() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    if depth == 0 {
                        // Re-run the group close on the main loop's stack.
                        self.pos -= 1;
                        break;
                    }
                    depth -= 1;
                }
                b'\\' => {
                    // Only \'xx and \tab contribute text inside a marker.
                    if self.peek() == Some(b'\'') {
                        self.pos += 1;
                        let hex: String = (0..2)
                            .filter_map(|_| self.next_byte().map(|b| b as char))
                            .collect();
                        if let Ok(v) = u8::from_str_radix(&hex, 16) {
                            out.push(decode_byte(v, self.codepage));
                        }
                    } else {
                        while self
                            .peek()
                            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'-')
                        {
                            self.pos += 1;
                        }
                        if self.peek() == Some(b' ') {
                            self.pos += 1;
                        }
                    }
                }
                b'\r' | b'\n' => {}
                _ => out.push(b as char),
            }
        }
        out
    }

    /// `{\stylesheet {\s1 …heading 1;}{\s2 …heading 2;}…}`: map style handles
    /// whose *name* (the trailing plain text before `;`) is "heading N".
    fn read_stylesheet(&mut self) {
        let mut depth = 0usize;
        let mut style: Option<i32> = None;
        let mut name = String::new();
        while let Some(b) = self.next_byte() {
            match b {
                b'{' => {
                    depth += 1;
                    style = None;
                    name.clear();
                }
                b'}' => {
                    if depth == 0 {
                        self.pos -= 1; // main loop pops the stylesheet group
                        break;
                    }
                    depth -= 1;
                }
                b';' => {
                    if let (Some(s), Some(level)) = (style, heading_level(&name)) {
                        self.heading_styles.push((s, level));
                    }
                }
                b'\\' => {
                    let start = self.pos;
                    while self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
                        self.pos += 1;
                    }
                    let word: String = self.bytes[start..self.pos]
                        .iter()
                        .map(|&b| b as char)
                        .collect();
                    let num_start = self.pos;
                    if self.peek() == Some(b'-') {
                        self.pos += 1;
                    }
                    while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                        self.pos += 1;
                    }
                    if word == "s" && self.pos > num_start {
                        style = String::from_utf8_lossy(&self.bytes[num_start..self.pos])
                            .parse()
                            .ok();
                    }
                    if self.peek() == Some(b' ') {
                        self.pos += 1;
                    }
                }
                b'\r' | b'\n' => {}
                _ => name.push(b as char),
            }
        }
    }

    /// `{\pict \pngblip|\jpegblip … <hex>}` → a picture node with the decoded
    /// bytes. A metafile (`\emfblip`, `\wmetafileN` — a WMF without its
    /// placeable header, sized by `\picwgoal`/`\pichgoal` twips) is rendered
    /// to PNG (#536); the rare inline-binary `\bin` form is skipped.
    fn read_picture(&mut self, doc: &mut DoclingDocument) {
        let mut depth = 0usize;
        let mut mimetype: Option<&'static str> = None;
        let mut metafile = false;
        let mut goal: (Option<i64>, Option<i64>) = (None, None);
        let mut hex = String::new();
        let mut skip_binary = false;
        while let Some(b) = self.next_byte() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    if depth == 0 {
                        self.pos -= 1;
                        break;
                    }
                    depth -= 1;
                }
                b'\\' => {
                    let start = self.pos;
                    while self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
                        self.pos += 1;
                    }
                    let word: String = self.bytes[start..self.pos]
                        .iter()
                        .map(|&b| b as char)
                        .collect();
                    let num = self.pos;
                    while self.peek().is_some_and(|b| b.is_ascii_digit() || b == b'-') {
                        self.pos += 1;
                    }
                    let param: Option<i64> = std::str::from_utf8(&self.bytes[num..self.pos])
                        .ok()
                        .and_then(|n| n.parse().ok());
                    if self.peek() == Some(b' ') {
                        self.pos += 1;
                    }
                    match word.as_str() {
                        "pngblip" => mimetype = Some("image/png"),
                        "jpegblip" => mimetype = Some("image/jpeg"),
                        "emfblip" | "wmetafile" => metafile = true,
                        "picwgoal" => goal.0 = param,
                        "pichgoal" => goal.1 = param,
                        "bin" => skip_binary = true,
                        _ => {}
                    }
                }
                b if b.is_ascii_hexdigit() => hex.push(b as char),
                _ => {}
            }
        }
        if skip_binary || hex.is_empty() || (mimetype.is_none() && !metafile) {
            return;
        }
        let data: Vec<u8> = hex
            .as_bytes()
            .chunks_exact(2)
            .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect();
        let Some(mimetype) = mimetype else {
            // 15 twips per pixel at 96 dpi.
            let hint = match goal {
                (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as f64 / 15.0, h as f64 / 15.0)),
                _ => None,
            };
            if let Some(image) = super::metafile::render(&data, hint) {
                doc.push(Node::Picture {
                    caption: None,
                    caption_href: None,
                    image: Some(image),
                    classification: None,
                    description: None,
                    caption_parent: Default::default(),
                    caption_location: None,
                });
            }
            return;
        };
        let (width, height) = image_size(mimetype, &data).unwrap_or((0, 0));
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image: Some(PictureImage {
                dpi: PictureImage::DEFAULT_DPI,
                mimetype: mimetype.to_string(),
                width,
                height,
                data,
                display_size: None,
            }),
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
    }

    /// Consume the whole `{\field …}` group (its `{` already handled by the
    /// main loop): a HYPERLINK instruction wraps the rendered `\fldrslt` as a
    /// Markdown link; other instructions (PAGEREF, TOC, …) keep the result
    /// text alone. The result is inline RTF, so it runs through a nested
    /// parser — formatting inside the link stays baked (`[**docs**](url)`).
    fn read_field(&mut self) {
        let start = self.pos;
        let mut depth = 0usize;
        while let Some(b) = self.next_byte() {
            match b {
                b'{' => depth += 1,
                b'}' => {
                    if depth == 0 {
                        self.pos -= 1; // main loop pops the field group
                        break;
                    }
                    depth -= 1;
                }
                b'\\' => {
                    // Escaped braces must not distort the depth count.
                    if matches!(self.peek(), Some(b'{') | Some(b'}') | Some(b'\\')) {
                        self.pos += 1;
                    }
                }
                _ => {}
            }
        }
        let raw: String = self.bytes[start..self.pos]
            .iter()
            .map(|&b| b as char)
            .collect();
        let Some(inner) = extract_group(&raw, "fldrslt") else {
            return;
        };
        let mut tmp = DoclingDocument::new("field");
        let wrapped = format!("{{\\rtf1 {inner}}}");
        let mut nested = Parser::new(&wrapped);
        nested.codepage = self.codepage;
        nested.run(&mut tmp);
        let result = tmp
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Paragraph { text } | Node::Heading { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        if result.is_empty() {
            return;
        }
        // Inside a table cell docling renders a hyperlink as its plain text
        // (the DOCX backend's cell convention); body text gets the Markdown
        // link.
        let text = match field_instruction_url(&raw) {
            Some(url) if !self.state.in_table => {
                docling_core::hyperlink::markdown_link(&result, &url)
            }
            _ => result,
        };
        // The nested parser already baked the formatting — append verbatim.
        self.runs.push(Run {
            text,
            bold: false,
            italic: false,
            strike: false,
            math: false,
        });
    }

    fn push_char(&mut self, ch: char) {
        if self.state.skip {
            return;
        }
        let (bold, italic, strike) = (self.state.bold, self.state.italic, self.state.strike);
        match self.runs.last_mut() {
            Some(run)
                if !run.math
                    && run.bold == bold
                    && run.italic == italic
                    && run.strike == strike =>
            {
                run.text.push(ch);
            }
            _ => self.runs.push(Run {
                text: ch.to_string(),
                bold,
                italic,
                strike,
                math: false,
            }),
        }
    }

    fn text_byte(&mut self, b: u8) {
        self.push_char(decode_byte(b, self.codepage));
    }

    /// Render the accumulated runs as one Markdown-baked string (the docling.rs
    /// convention — `**bold**`, `*italic*`, `~~strike~~`), keeping run-edge
    /// whitespace outside the markers so the Markdown stays valid.
    fn take_text(&mut self) -> String {
        let mut out = String::new();
        for run in self.runs.drain(..) {
            if run.math {
                // The DOCX backend's inline-equation spacing (docling joins
                // the paragraph's text and formula parts with a space, so a
                // word run's own trailing space doubles up) — the same
                // equation as `.docx` reads `Synthetic equation:  $E=mc^{2}$`.
                out.push_str(&format!(" ${}$ ", run.text));
                continue;
            }
            let trimmed = run.text.trim();
            if trimmed.is_empty() {
                out.push_str(&run.text);
                continue;
            }
            let lead = &run.text[..run.text.len() - run.text.trim_start().len()];
            let trail = &run.text[run.text.trim_end().len()..];
            let mut s = trimmed.to_string();
            if run.bold {
                s = format!("**{s}**");
            }
            if run.italic {
                s = format!("*{s}*");
            }
            if run.strike {
                s = format!("~~{s}~~");
            }
            out.push_str(lead);
            out.push_str(&s);
            out.push_str(trail);
        }
        out.trim().to_string()
    }

    fn end_cell(&mut self) {
        let text = self.take_text();
        self.cells.push(text);
        self.list_marker = None;
    }

    fn end_row(&mut self) {
        if self.cells.is_empty() {
            return;
        }
        let cells = std::mem::take(&mut self.cells);
        self.rows.push((cells, self.cell_defs.clone()));
    }

    fn flush_table(&mut self, doc: &mut DoclingDocument) {
        self.end_row();
        if self.rows.is_empty() {
            return;
        }
        let raw = std::mem::take(&mut self.rows);
        // Column grid = the union of every row's cell boundaries; a cell
        // whose span covers several grid columns replicates its text into
        // each (docling's merged-cell convention). Rows whose defs don't
        // line up with their cells (malformed writer) stay as-is.
        let mut bounds: Vec<i64> = raw
            .iter()
            .flat_map(|(_, defs)| defs.iter().map(|d| d.right))
            .collect();
        bounds.sort_unstable();
        bounds.dedup();
        let mut rows: Vec<Vec<String>> = Vec::with_capacity(raw.len());
        let mut vconts: Vec<Vec<bool>> = Vec::with_capacity(raw.len());
        for (cells, defs) in &raw {
            if defs.len() != cells.len() || bounds.is_empty() {
                rows.push(cells.clone());
                vconts.push(vec![false; cells.len()]);
                continue;
            }
            let mut expanded = Vec::with_capacity(bounds.len());
            let mut vc = Vec::with_capacity(bounds.len());
            let mut prev = i64::MIN;
            for (cell, def) in cells.iter().zip(defs) {
                let span = bounds
                    .iter()
                    .filter(|&&b| b > prev && b <= def.right)
                    .count()
                    .max(1);
                for _ in 0..span {
                    expanded.push(cell.clone());
                    vc.push(def.vcont);
                }
                prev = def.right;
            }
            rows.push(expanded);
            vconts.push(vc);
        }
        // Word-style vertical merges (\clvmrg): an empty continuation cell
        // takes the text of the cell above it.
        for r in 1..rows.len() {
            for c in 0..rows[r].len() {
                if vconts[r].get(c).copied().unwrap_or(false) && rows[r][c].is_empty() {
                    if let Some(above) = rows.get(r - 1).and_then(|row| row.get(c)) {
                        rows[r][c] = above.clone();
                    }
                }
            }
        }
        // Word-style horizontal continuations (\clmrg) replicate leftward.
        for ((_, defs), row) in raw.iter().zip(rows.iter_mut()) {
            if defs.len() != row.len() {
                continue;
            }
            for c in 1..row.len() {
                if defs[c].hcont && row[c].is_empty() {
                    row[c] = row[c - 1].clone();
                }
            }
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        for row in &mut rows {
            row.resize(width, String::new());
        }
        doc.push(Node::Table(Table {
            footnotes: Vec::new(),
            rows,
            location: None,
            structure: None,
            cell_blocks: None,
            cells: None,
            caption: None,
            caption_parent: Default::default(),
            caption_location: None,
        }));
        self.prev_was_list = false;
    }

    fn flush_paragraph(&mut self, doc: &mut DoclingDocument) {
        if self.state.in_table {
            // \par inside a cell is a soft break within that cell's text.
            self.push_char('\n');
            return;
        }
        // Leaving the table region: emit the assembled table first.
        self.flush_table(doc);

        // A paragraph of equations alone: standalone `$$…$$` formulas, as the
        // DOCX backend renders an OMML-only paragraph.
        if self.runs.iter().any(|r| r.math)
            && self.runs.iter().all(|r| r.math || r.text.trim().is_empty())
        {
            self.list_marker = None;
            for run in self.runs.drain(..).filter(|r| r.math) {
                doc.push(Node::Formula {
                    orig: run.text.clone(),
                    latex: run.text,
                    location: None,
                });
            }
            self.prev_was_list = false;
            return;
        }
        let marker = self.list_marker.take();
        let text = self.take_text();
        if text.is_empty() {
            // An empty paragraph (spacing) must not split a list in two —
            // dropping it silently keeps `first_in_list` accurate.
            return;
        }
        // docling's DOCX convention: Title renders as `#`, Heading N as
        // `#`×(N+1) — outline level 0 *is* Heading 1, so it maps to 2.
        let heading = self
            .state
            .outline
            .map(|o| o + 2)
            .or_else(|| {
                let style = self.state.style?;
                self.heading_styles
                    .iter()
                    .find(|(s, _)| *s == style)
                    .map(|(_, level)| *level)
            })
            .map(|level| level.clamp(1, 6));
        if let Some(level) = heading {
            doc.push(Node::Heading { level, text });
            self.prev_was_list = false;
            return;
        }
        if let Some((ordered, number, marker, prefix)) = marker {
            let level = self.state.ilvl;
            // A new list starts after non-list content, or at a list-identity
            // change: another `\ls` override, or — for `\listtext`-only files
            // — a marker-kind flip between two top-level items. An empty
            // spacing paragraph in between breaks nothing (it is dropped above).
            let key = (self.state.ls, ordered, level);
            let first_in_list = !self.prev_was_list
                || match (self.prev_list_key, key) {
                    (Some((Some(prev_ls), _, _)), (Some(ls), _, _)) => prev_ls != ls,
                    (Some((_, prev_ordered, prev_level)), (_, ordered, level)) => {
                        level == 0 && prev_level == 0 && prev_ordered != ordered
                    }
                    (None, _) => true,
                };
            self.prev_list_key = Some(key);
            let text = match &prefix {
                Some(p) => format!("{p} {text}"),
                None => text,
            };
            if ordered {
                doc.push(Node::ListItem {
                    ordered,
                    number,
                    first_in_list,
                    text,
                    level,
                    marker: Some(marker),
                    location: None,
                    dclx: None,
                    href: None,
                    layer: None,
                });
            } else {
                doc.push(Node::ListItem {
                    ordered: false,
                    number: 0,
                    first_in_list,
                    text,
                    level,
                    marker: None,
                    location: None,
                    dclx: None,
                    href: None,
                    layer: None,
                });
            }
            self.prev_was_list = true;
            return;
        }
        doc.push(Node::Paragraph { text });
        self.prev_was_list = false;
    }
}

/// Position of the `}` closing the group whose content starts at `start`
/// (or the end of input), stepping over escaped braces.
fn group_end(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' if depth == 0 => return i,
            b'}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// OMML elements whose text content is math text (a bare string becomes an
/// `<m:r>` run); in every other element text is the property's value.
const MATH_CONTAINERS: &[&str] = &[
    "oMathPara",
    "oMath",
    "e",
    "num",
    "den",
    "sub",
    "sup",
    "deg",
    "fName",
    "lim",
];

/// Rebuild the OMML an RTF math group spells. RTF's math destinations are
/// OMML transliterated (RTF 1.9.1, "Math"): every element `m:X` is a group
/// `{\mX …}` (often starred, `{\*\moMath …}`), a run's text sits directly in
/// its `{\mr …}` group, a property's value is either the group's text
/// (`{\mbegChr (}`, `{\mchr \u8721?}`) or a control-word parameter
/// (`\msty2`), and character-formatting groups (`{\rtlch\f34 …}`) are
/// interleaved freely — transparent here. Element nesting is capped at the
/// XML depth limit (deeper elements are flattened into their parent).
fn rtf_math_to_omml(raw: &[u8], codepage: u32) -> String {
    /// One open RTF group: the element it opened (if its head was `\mX`),
    /// where that element's start tag ends in `out` (for a late `m:val`),
    /// the property text it gathered, and the `\uc` in force.
    struct Frame {
        elem: Option<String>,
        tag_end: usize,
        val: String,
        uc: usize,
    }
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let limit = crate::backend::xml_depth::max_depth().saturating_sub(2);
    let mut out = String::from(
        r#"<m:root xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">"#,
    );
    let mut frames: Vec<Frame> = vec![Frame {
        elem: None,
        tag_end: 0,
        val: String::new(),
        uc: 1,
    }];
    let mut open_elems = 0usize;
    // The innermost open element's name.
    let current =
        |frames: &[Frame]| -> Option<String> { frames.iter().rev().find_map(|f| f.elem.clone()) };
    let push_text =
        |frames: &mut Vec<Frame>, out: &mut String, text: &str| match current(frames).as_deref() {
            Some("r") => out.push_str(&format!("<m:t>{}</m:t>", esc(text))),
            Some(e) if MATH_CONTAINERS.contains(&e) => {
                if !text.trim().is_empty() {
                    out.push_str(&format!("<m:r><m:t>{}</m:t></m:r>", esc(text)));
                }
            }
            Some(_) => {
                if let Some(f) = frames.iter_mut().rev().find(|f| f.elem.is_some()) {
                    f.val.push_str(text);
                }
            }
            None => {}
        };
    let mut i = 0;
    let mut text = String::new();
    let mut group_head = false;
    while i < raw.len() {
        let b = raw[i];
        if !matches!(b, b'{' | b'}' | b'\\' | b'\r' | b'\n') {
            text.push(decode_byte(b, codepage));
            i += 1;
            continue;
        }
        // A `\'xx` byte continues the text it sits in; anything else ends it.
        let hex_byte = b == b'\\' && raw.get(i + 1) == Some(&b'\'');
        if !text.is_empty() && !hex_byte {
            push_text(&mut frames, &mut out, &std::mem::take(&mut text));
        }
        match b {
            b'{' => {
                let uc = frames.last().map_or(1, |f| f.uc);
                frames.push(Frame {
                    elem: None,
                    tag_end: 0,
                    val: String::new(),
                    uc,
                });
                group_head = true;
                i += 1;
                continue;
            }
            b'}' => {
                if frames.len() > 1 {
                    let f = frames.pop().expect("non-root frame");
                    if let Some(name) = f.elem {
                        let val = f.val.trim();
                        if !val.is_empty() {
                            out.insert_str(f.tag_end - 1, &format!(r#" m:val="{}""#, esc(val)));
                        }
                        out.push_str(&format!("</m:{name}>"));
                        open_elems -= 1;
                    }
                }
                group_head = false;
                i += 1;
                continue;
            }
            b'\r' | b'\n' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        // A control word or symbol.
        i += 1;
        let Some(&c) = raw.get(i) else { break };
        if !c.is_ascii_alphabetic() {
            i += 1;
            match c {
                b'\'' => {
                    let hex =
                        std::str::from_utf8(raw.get(i..i + 2).unwrap_or_default()).unwrap_or("");
                    if let Ok(v) = u8::from_str_radix(hex, 16) {
                        text.push(decode_byte(v, codepage));
                    }
                    i += 2;
                }
                b'\\' | b'{' | b'}' => text.push(c as char),
                b'~' => text.push('\u{00A0}'),
                // `\*` keeps the group-head position for the word after it.
                b'*' => continue,
                _ => {}
            }
            group_head = false;
            continue;
        }
        let start = i;
        while raw.get(i).is_some_and(u8::is_ascii_alphabetic) {
            i += 1;
        }
        let word = std::str::from_utf8(&raw[start..i]).unwrap_or("");
        let num_start = i;
        if raw.get(i) == Some(&b'-') {
            i += 1;
        }
        while raw.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let param: Option<i64> = std::str::from_utf8(&raw[num_start..i])
            .ok()
            .and_then(|p| p.parse().ok());
        if raw.get(i) == Some(&b' ') {
            i += 1;
        }
        let head = std::mem::replace(&mut group_head, false);
        match (word, param) {
            ("u", Some(v)) => {
                let v = if v < 0 { v + 65536 } else { v } as u32;
                if let Some(ch) = char::from_u32(v) {
                    text.push(ch);
                }
                // Skip the `\uc` fallback characters (`\'xx` counts as one).
                for _ in 0..frames.last().map_or(1, |f| f.uc) {
                    match raw.get(i) {
                        Some(b'\\') if raw.get(i + 1) == Some(&b'\'') => i += 4,
                        Some(b'{' | b'}' | b'\\') | None => break,
                        _ => i += 1,
                    }
                }
            }
            ("uc", p) => {
                if let Some(f) = frames.last_mut() {
                    f.uc = p.unwrap_or(1).max(0) as usize;
                }
            }
            (w, p) if w.len() > 1 && w.starts_with('m') => {
                let name = &w[1..];
                if head && open_elems < limit {
                    out.push_str(&format!("<m:{name}>"));
                    let f = frames.last_mut().expect("a frame per open group");
                    f.elem = Some(name.to_string());
                    f.tag_end = out.len();
                    if let Some(p) = p {
                        f.val = p.to_string();
                    }
                    open_elems += 1;
                } else if let Some(p) = p {
                    // `\msty2`-style: a property of the enclosing element.
                    out.push_str(&format!(r#"<m:{name} m:val="{p}"/>"#));
                }
            }
            _ => {} // character formatting and the like
        }
    }
    if !text.is_empty() {
        push_text(&mut frames, &mut out, &text);
    }
    // Close whatever an unterminated group left open.
    while let Some(f) = frames.pop() {
        if let Some(name) = f.elem {
            out.push_str(&format!("</m:{name}>"));
        }
    }
    out.push_str("</m:root>");
    out
}

/// Markdown heading level from a style name, matching the DOCX backend's
/// docling mapping: Title → `#` (1), Subtitle → `##`, "heading N" → N+1.
fn heading_level(style_name: &str) -> Option<u8> {
    let name = style_name.trim().to_ascii_lowercase();
    match name.as_str() {
        "title" => return Some(1),
        "subtitle" => return Some(2),
        _ => {}
    }
    let rest = name.strip_prefix("heading ")?;
    rest.parse::<u8>()
        .ok()
        .filter(|n| (1..=9).contains(n))
        .map(|n| n + 1)
}

/// The inner source of the first `{\<dest> …}` /  `{\*\<dest> …}` group inside
/// a raw field group, brace-balanced.
fn extract_group(raw: &str, dest: &str) -> Option<String> {
    let needle = format!("\\{dest}");
    let at = raw.find(&needle)?;
    let rest = &raw[at + needle.len()..];
    let mut depth = 0i32;
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if matches!(chars.peek(), Some('{') | Some('}') | Some('\\')) => {
                out.push(ch);
                out.push(chars.next().unwrap());
            }
            '{' => {
                depth += 1;
                out.push(ch);
            }
            '}' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    Some(out)
}

/// The URL of a `HYPERLINK "…"` field instruction, if that is what the field
/// is (`\l` local anchors and non-HYPERLINK instructions → `None`).
fn field_instruction_url(raw: &str) -> Option<String> {
    let inst = extract_group(raw, "fldinst")?;
    // Plain text of the instruction: control words stripped, groups flattened.
    let mut text = String::new();
    let mut chars = inst.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                while chars
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-')
                {
                    chars.next();
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            '{' | '}' => {}
            _ => text.push(c),
        }
    }
    let rest = text.trim().strip_prefix("HYPERLINK")?.trim();
    let url = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()?.to_string()
    } else {
        rest.split_whitespace().next()?.to_string()
    };
    (!url.is_empty() && !url.starts_with("\\l")).then_some(url)
}

/// Type a `\listtext` marker: `1.` / `12)` → ordered with that number;
/// a multilevel number (`1.1.`) → bullet with the number kept as a text
/// prefix (the DOCX backend's docling convention); anything else (`·`, `-`,
/// `o`, Symbol-font bullets) → plain bullet.
fn classify_marker(marker: &str) -> ListMarker {
    let trimmed = marker.trim();
    let m = trimmed.trim_end_matches(['.', ')']);
    if let Ok(n) = m.parse::<u64>() {
        return (true, n, format!("{n}."), None);
    }
    if !m.is_empty() && m.split('.').all(|p| p.parse::<u64>().is_ok()) {
        // "1.1" / "2.3.4" (with or without the trailing dot).
        return (false, 0, String::new(), Some(format!("{m}.")));
    }
    (false, 0, String::new(), None)
}

/// One byte → char through the document codepage. ASCII is universal; the
/// supported high-byte pages are Windows-1252 (default), -1250 and -1251.
pub(crate) fn decode_byte(b: u8, codepage: u32) -> char {
    if b < 0x80 {
        return b as char;
    }
    let table: &[u16; 128] = match codepage {
        1250 => &CP1250,
        1251 => &CP1251,
        _ => &CP1252,
    };
    char::from_u32(table[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}')
}

/// PNG IHDR / JPEG SOF dimensions, best-effort (`None` → 0×0 metadata).
pub(crate) fn image_size(mimetype: &str, data: &[u8]) -> Option<(u32, u32)> {
    match mimetype {
        "image/png" => {
            if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
                return None;
            }
            let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            Some((be(&data[16..20]), be(&data[20..24])))
        }
        "image/jpeg" => {
            // Walk the segment chain to the first SOFn frame header.
            let mut i = 2usize;
            while i + 9 < data.len() {
                if data[i] != 0xFF {
                    return None;
                }
                let marker = data[i + 1];
                let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                    let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                    return Some((w, h));
                }
                i += 2 + len;
            }
            None
        }
        _ => None,
    }
}

/// Windows-1252, upper half (0x80–0xFF).
const CP1252: [u16; 128] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178, 0x00A0, 0x00A1, 0x00A2, 0x00A3,
    0x00A4, 0x00A5, 0x00A6, 0x00A7, 0x00A8, 0x00A9, 0x00AA, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x00AF,
    0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7, 0x00B8, 0x00B9, 0x00BA, 0x00BB,
    0x00BC, 0x00BD, 0x00BE, 0x00BF, 0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7,
    0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF, 0x00D0, 0x00D1, 0x00D2, 0x00D3,
    0x00D4, 0x00D5, 0x00D6, 0x00D7, 0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF,
    0x00E0, 0x00E1, 0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7, 0x00E8, 0x00E9, 0x00EA, 0x00EB,
    0x00EC, 0x00ED, 0x00EE, 0x00EF, 0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7,
    0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD, 0x00FE, 0x00FF,
];

/// Windows-1250 (Central European), upper half.
const CP1250: [u16; 128] = [
    0x20AC, 0x0081, 0x201A, 0x0083, 0x201E, 0x2026, 0x2020, 0x2021, 0x0088, 0x2030, 0x0160, 0x2039,
    0x015A, 0x0164, 0x017D, 0x0179, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x0098, 0x2122, 0x0161, 0x203A, 0x015B, 0x0165, 0x017E, 0x017A, 0x00A0, 0x02C7, 0x02D8, 0x0141,
    0x00A4, 0x0104, 0x00A6, 0x00A7, 0x00A8, 0x00A9, 0x015E, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x017B,
    0x00B0, 0x00B1, 0x02DB, 0x0142, 0x00B4, 0x00B5, 0x00B6, 0x00B7, 0x00B8, 0x0105, 0x015F, 0x00BB,
    0x013D, 0x02DD, 0x013E, 0x017C, 0x0154, 0x00C1, 0x00C2, 0x0102, 0x00C4, 0x0139, 0x0106, 0x00C7,
    0x010C, 0x00C9, 0x0118, 0x00CB, 0x011A, 0x00CD, 0x00CE, 0x010E, 0x0110, 0x0143, 0x0147, 0x00D3,
    0x00D4, 0x0150, 0x00D6, 0x00D7, 0x0158, 0x016E, 0x00DA, 0x0170, 0x00DC, 0x00DD, 0x0162, 0x00DF,
    0x0155, 0x00E1, 0x00E2, 0x0103, 0x00E4, 0x013A, 0x0107, 0x00E7, 0x010D, 0x00E9, 0x0119, 0x00EB,
    0x011B, 0x00ED, 0x00EE, 0x010F, 0x0111, 0x0144, 0x0148, 0x00F3, 0x00F4, 0x0151, 0x00F6, 0x00F7,
    0x0159, 0x016F, 0x00FA, 0x0171, 0x00FC, 0x00FD, 0x0163, 0x02D9,
];

/// Windows-1251 (Cyrillic), upper half.
const CP1251: [u16; 128] = [
    0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039,
    0x040A, 0x040C, 0x040B, 0x040F, 0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x0098, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F, 0x00A0, 0x040E, 0x045E, 0x0408,
    0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
    0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB,
    0x0458, 0x0405, 0x0455, 0x0457, 0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417,
    0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F, 0x0420, 0x0421, 0x0422, 0x0423,
    0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F,
    0x0430, 0x0431, 0x0432, 0x0433, 0x0434, 0x0435, 0x0436, 0x0437, 0x0438, 0x0439, 0x043A, 0x043B,
    0x043C, 0x043D, 0x043E, 0x043F, 0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447,
    0x0448, 0x0449, 0x044A, 0x044B, 0x044C, 0x044D, 0x044E, 0x044F,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::InputFormat;

    fn convert(rtf: &str) -> DoclingDocument {
        let src = SourceDocument::from_bytes("t.rtf", InputFormat::Rtf, rtf.as_bytes().to_vec());
        RtfBackend.convert(&src).unwrap()
    }

    #[test]
    fn paragraphs_and_inline_formatting() {
        let doc = convert(r"{\rtf1\ansi Plain {\b bold} and \i slanted\i0  text.\par}");
        assert_eq!(
            doc.nodes,
            vec![Node::Paragraph {
                text: "Plain **bold** and *slanted* text.".into()
            }]
        );
    }

    #[test]
    fn headings_from_stylesheet_and_outline() {
        let doc = convert(
            r"{\rtf1\ansi{\stylesheet{\s0 Normal;}{\s1\b heading 1;}}\pard\s1\b Title\b0\par \pard\outlinelevel1 Sub\par \pard Body\par}",
        );
        // docling's mapping: "heading 1" renders as ## (level 2), outline
        // level 1 (= Heading 2) as ### (level 3).
        assert_eq!(
            doc.nodes,
            vec![
                Node::Heading {
                    level: 2,
                    text: "**Title**".into()
                },
                Node::Heading {
                    level: 3,
                    text: "Sub".into()
                },
                Node::Paragraph {
                    text: "Body".into()
                },
            ]
        );
    }

    #[test]
    fn table_rows_and_cells() {
        let doc = convert(
            r"{\rtf1\ansi\trowd\intbl a\cell b\cell\row \trowd\intbl 1\cell 2\cell\row \pard After\par}",
        );
        let Node::Table(t) = &doc.nodes[0] else {
            panic!("expected a table, got {:?}", doc.nodes)
        };
        assert_eq!(t.rows, vec![vec!["a", "b"], vec!["1", "2"]]);
        assert_eq!(
            doc.nodes[1],
            Node::Paragraph {
                text: "After".into()
            }
        );
    }

    #[test]
    fn lists_from_listtext_markers() {
        let doc = convert(
            r"{\rtf1\ansi\pard{\listtext \'b7\tab}First\par\pard{\listtext \'b7\tab}Second\par\pard{\listtext 1.\tab}Num\par}",
        );
        assert!(matches!(
            &doc.nodes[0],
            Node::ListItem { ordered: false, first_in_list: true, text, .. } if text == "First"
        ));
        assert!(matches!(
            &doc.nodes[1],
            Node::ListItem { ordered: false, first_in_list: false, text, .. } if text == "Second"
        ));
        // #385: without `\ls`, a marker-kind flip between top-level items is
        // the list boundary — a numbered item after bullets opens a new list.
        assert!(matches!(
            &doc.nodes[2],
            Node::ListItem { ordered: true, number: 1, first_in_list: true, text, .. } if text == "Num"
        ));
    }

    /// #385: `\lsN` is Word's list identity (DOCX's `numId`). Items of one
    /// override are one list whatever their marker kinds — a nested bullet
    /// under a numbered item, a numbered item after them — and a different
    /// override starts a new one; an empty spacing paragraph breaks nothing.
    #[test]
    fn ls_overrides_identify_lists() {
        let doc = convert(concat!(
            r"{\rtf1\ansi",
            r"\pard\ls1\ilvl0{\listtext 1.\tab}One\par",
            r"\pard\ls1\ilvl1{\listtext \'b7\tab}Sub\par",
            r"\pard\ls1\ilvl0{\listtext 2.\tab}Two\par",
            r"\pard\par",
            r"\pard\ls1\ilvl0{\listtext 3.\tab}Three\par",
            r"\pard\ls2\ilvl0{\listtext 1.\tab}Other\par}",
        ));
        let flags: Vec<(&str, bool)> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::ListItem {
                    text,
                    first_in_list,
                    ..
                } => Some((text.as_str(), *first_in_list)),
                _ => None,
            })
            .collect();
        assert_eq!(
            flags,
            vec![
                ("One", true),
                ("Sub", false),
                ("Two", false),
                ("Three", false),
                ("Other", true),
            ]
        );
    }

    #[test]
    fn unicode_escapes_and_codepages() {
        // \uN with a \'3f fallback that must be skipped; cp1252 \'e9 = é.
        let doc = convert(
            r"{\rtf1\ansi\ansicpg1252 caf\'e9 \u1055\'3f\u1088\'3f\u1080\'3f\u1074\'3f\u1077\'3f\u1090\'3f\par}",
        );
        assert_eq!(
            doc.nodes,
            vec![Node::Paragraph {
                text: "café Привет".into()
            }]
        );
    }

    #[test]
    fn skips_furniture_destinations() {
        let doc = convert(
            r"{\rtf1\ansi{\fonttbl{\f0 Arial;}}{\colortbl;\red0\green0\blue0;}{\info{\title secret}}{\*\generator Word}Visible\par}",
        );
        assert_eq!(
            doc.nodes,
            vec![Node::Paragraph {
                text: "Visible".into()
            }]
        );
    }

    #[test]
    fn field_keeps_result_drops_instruction() {
        let doc = convert(
            r#"{\rtf1\ansi{\field{\*\fldinst HYPERLINK "http://x"}{\fldrslt docling.rs}} rules\par}"#,
        );
        assert_eq!(
            doc.nodes,
            vec![Node::Paragraph {
                text: "[docling.rs](http://x) rules".into()
            }]
        );
    }

    #[test]
    fn decodes_embedded_png_picture() {
        // A 1×1 PNG, hex-encoded the way Word embeds it.
        let png_hex = "89504e470d0a1a0a0000000d494844520000000100000001080600000\
                       01f15c4890000000d49444154789c626001000000ffff03000006000\
                       557bfabd40000000049454e44ae426082";
        let rtf = format!(r"{{\rtf1\ansi{{\pict\pngblip\picw1\pich1 {png_hex}}}\par Text\par}}");
        let doc = convert(&rtf);
        let Node::Picture {
            image: Some(img), ..
        } = &doc.nodes[0]
        else {
            panic!("expected a picture, got {:?}", doc.nodes)
        };
        assert_eq!(img.mimetype, "image/png");
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(&img.data[..8], b"\x89PNG\r\n\x1a\n");
    }

    /// A `\wmetafile8` picture renders to PNG at its `\picwgoal`/`\pichgoal`
    /// size; the `{\nonshppict}` WMF fallback of a `\shppict` PNG does not
    /// add a second picture.
    #[cfg(feature = "pdf")]
    #[test]
    fn renders_metafile_pictures_once() {
        let wmf = crate::backend::metafile::tests::wmf(
            None,
            &crate::backend::metafile::tests::wmf_body(),
        );
        let hex: String = wmf.iter().map(|b| format!("{b:02x}")).collect();
        let rtf = format!(
            r"{{\rtf1\ansi{{\pict\wmetafile8\picw1000\pich500\picwgoal1800\pichgoal900 {hex}}}\par}}"
        );
        let doc = convert(&rtf);
        let Node::Picture {
            image: Some(img), ..
        } = &doc.nodes[0]
        else {
            panic!("expected a picture, got {:?}", doc.nodes)
        };
        assert_eq!(
            (img.mimetype.as_str(), img.width, img.height),
            ("image/png", 120, 60)
        );

        let rtf = format!(
            r"{{\rtf1\ansi{{\*\shppict{{\pict\wmetafile8 {hex}}}}}{{\nonshppict{{\pict\wmetafile8 {hex}}}}}\par}}"
        );
        let pictures = convert(&rtf)
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Picture { .. }))
            .count();
        assert_eq!(pictures, 1);
    }

    /// #578: an `\mmath` group is OMML spelled in control words — inline in
    /// a sentence it is `$…$` (the DOCX backend's spacing), the
    /// `\mmathPict` picture after it is the fallback rendering and dropped.
    #[test]
    fn inline_math_becomes_latex_and_its_picture_is_skipped() {
        let doc = convert(concat!(
            r"{\rtf1\ansi{\mmathPr\mmathFont34\mbrkBin0}{Synthetic equation: }",
            r"{\mmath{\*\moMath{\rtlch\i\f34 {\mr\mscr0\msty2 E}}{\mr =}{\mr m}",
            r"{\msSup{\msSupPr{\mctrlPr\i\f34 }}{\me{\mr c}}{\msup{\mr 2}}}}}",
            r"{\mmathPict{\*\shppict{\pict\pngblip 89504e47}}{\nonshppict{\pict\wmetafile8 0100}}}",
            r"\par}"
        ));
        assert_eq!(
            doc.nodes,
            vec![Node::Paragraph {
                text: "Synthetic equation:  $E=mc^{2}$".into()
            }]
        );
    }

    /// A paragraph of equations alone yields one display formula each, and
    /// the structures + property values (`\mchr \u8721`, `\mbegChr`) reach
    /// the shared OMML converter.
    #[test]
    fn display_math_structures() {
        let doc = convert(concat!(
            r"{\rtf1\ansi{\mmath{\*\moMath{\mnary{\mnaryPr{\mchr \u8721\'3f}}",
            r"{\msub{\mr k}{\mr =}{\mr 0}}{\msup{\mr n}}{\me{\mf{\mnum{\mr 1}}{\mden{\mr k}}}}}}}",
            r"{\mmath{\*\moMath{\md{\mdPr{\mbegChr [}{\mendChr ]}}{\me{\mrad{\mradPr{\mdegHide 1}}{\mdeg}{\me{\mr x}}}}}}}",
            r"\par}"
        ));
        assert_eq!(
            doc.nodes,
            vec![
                Node::Formula {
                    latex: r"\sum_{k=0}^{n}\frac{1}{k}".into(),
                    orig: r"\sum_{k=0}^{n}\frac{1}{k}".into(),
                    location: None,
                },
                Node::Formula {
                    latex: r"\left[\sqrt{x}\right]".into(),
                    orig: r"\left[\sqrt{x}\right]".into(),
                    location: None,
                },
            ]
        );
    }

    /// LibreOffice writes `{\mmath {\*\shppict …}}` for a formula it kept
    /// only as a picture: with no equation inside, the picture still counts.
    #[test]
    fn math_group_without_an_equation_keeps_its_picture() {
        let png = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d4944415478da63f8cfc0f01f0005000201a2b0b1e10000000049454e44ae426082";
        let doc = convert(&format!(
            r"{{\rtf1\ansi{{\mmath {{\*\shppict{{\pict\pngblip {png}}}}}}}\par}}"
        ));
        assert!(matches!(doc.nodes.as_slice(), [Node::Picture { .. }]));
    }

    #[test]
    fn rejects_non_rtf() {
        let src = SourceDocument::from_bytes("t.rtf", InputFormat::Rtf, b"not rtf at all".to_vec());
        assert!(RtfBackend.convert(&src).is_err());
    }
}
