//! DOC (Word 97–2004 binary, [MS-DOC]) backend — issue #127.
//!
//! Native parsing, no external converter (docling proper shells out to
//! LibreOffice for these files — `docling` PR #3804). The format is a CFB
//! container ([`cfb`]) holding a `WordDocument` stream (FIB header + raw text)
//! and a `0Table`/`1Table` stream (the piece table and formatting):
//!
//! 1. The **FIB** locates the CLX in the table stream and says how long the
//!    main document text is (`ccpText`).
//! 2. The **piece table** (CLX → PlcPcd) maps character positions (CPs) to
//!    file offsets (FCs) — pieces are either 8-bit CP1252 or UTF-16LE.
//! 3. Text is split into paragraphs at the paragraph mark (`\r`) / cell mark
//!    (`0x07`); each paragraph's properties come from its **PAPX** (found via
//!    the PlcfBtePapx → FKP page for the mark's FC): the style index `istd`,
//!    `fInTable` (table cell content) and `fTtp` (table row terminator).
//! 4. The **stylesheet** (STSH) maps `istd` to the built-in style identifier
//!    `sti` — 1–9 are the Heading 1–9 styles, giving real headings.
//!
//! Scope: headings, paragraphs, list items (by list-format reference),
//! tables (cell/row marks), and embedded pictures — both inline (`0x01`
//! anchor → `sprmCPicLocation` → PICF in the Data stream) and floating
//! (`0x08` anchor → PlcfSpa → the drawing's shape → BLIP store, with
//! delay-stream data in `WordDocument`), decoded via [`officeart`].
//! Footnotes and headers/footers remain out of scope.

use docling_core::{DoclingDocument, Node, PictureImage, Table};

use crate::backend::cfb::CompoundFile;
use crate::backend::officeart;
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

pub struct DocBackend;

impl DeclarativeBackend for DocBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let cfb = CompoundFile::open(&source.bytes)
            .ok_or_else(|| ConversionError::Parse("doc: not a compound file".into()))?;
        let word = cfb
            .stream("WordDocument")
            .ok_or_else(|| ConversionError::Parse("doc: no WordDocument stream".into()))?;
        if u16_at(&word, 0) != Some(0xA5EC) {
            return Err(ConversionError::Parse("doc: bad FIB magic".into()));
        }
        let flags = u16_at(&word, 0x0A).unwrap_or(0);
        if flags & 0x0100 != 0 {
            return Err(ConversionError::Parse("doc: document is encrypted".into()));
        }
        let table_name = if flags & 0x0200 != 0 {
            "1Table"
        } else {
            "0Table"
        };
        let table = cfb
            .stream(table_name)
            .ok_or_else(|| ConversionError::Parse(format!("doc: no {table_name} stream")))?;

        let ccp_text = u32_at(&word, 76).unwrap_or(0) as u64;
        let fc_clx = u32_at(&word, 418).unwrap_or(0) as usize;
        let lcb_clx = u32_at(&word, 422).unwrap_or(0) as usize;
        let pieces = parse_piece_table(table.get(fc_clx..fc_clx + lcb_clx).unwrap_or(&[]))
            .ok_or_else(|| ConversionError::Parse("doc: bad piece table".into()))?;

        // Styles (istd → sti) and paragraph properties (FC → PAPX).
        let fc_stsh = u32_at(&word, 162).unwrap_or(0) as usize;
        let lcb_stsh = u32_at(&word, 166).unwrap_or(0) as usize;
        let stis = parse_stsh(table.get(fc_stsh..fc_stsh + lcb_stsh).unwrap_or(&[]));
        let fc_bte = u32_at(&word, 258).unwrap_or(0) as usize;
        let lcb_bte = u32_at(&word, 262).unwrap_or(0) as usize;
        let bte = table.get(fc_bte..fc_bte + lcb_bte).unwrap_or(&[]);
        // Character-run properties (bold/italic): PlcfBteChpx → CHPX FKPs.
        let fc_btec = u32_at(&word, 250).unwrap_or(0) as usize;
        let lcb_btec = u32_at(&word, 254).unwrap_or(0) as usize;
        let btec = table.get(fc_btec..fc_btec + lcb_btec).unwrap_or(&[]);
        let mut chpx_cache = ChpxCache::default();

        // List tables: ilfo → numbering kind/start per level (ordered lists).
        let fc_lst = u32_at(&word, 738).unwrap_or(0) as usize;
        let lcb_lst = u32_at(&word, 742).unwrap_or(0) as usize;
        let fc_lfo = u32_at(&word, 746).unwrap_or(0) as usize;
        let lcb_lfo = u32_at(&word, 750).unwrap_or(0) as usize;
        // `lcbPlfLst` covers only the LSTF array; the per-list LVL structures
        // follow it directly in the table stream, so hand `parse` the tail.
        let lists = ListTables::parse(
            table.get(fc_lst..).unwrap_or(&[]),
            lcb_lst,
            table.get(fc_lfo..fc_lfo + lcb_lfo).unwrap_or(&[]),
        );

        // Pictures: the Data stream holds inline PICFs; the drawing tables
        // (fcDggInfo, in the table stream) + PlcfSpa anchor floating shapes.
        let data = cfb.stream("Data").unwrap_or_default();
        let fc_spa = u32_at(&word, 474).unwrap_or(0) as usize;
        let lcb_spa = u32_at(&word, 478).unwrap_or(0) as usize;
        let spa = parse_plcf_spa(table.get(fc_spa..fc_spa + lcb_spa).unwrap_or(&[]));
        let fc_dgg = u32_at(&word, 554).unwrap_or(0) as usize;
        let lcb_dgg = u32_at(&word, 558).unwrap_or(0) as usize;
        let drawings = Drawings::parse(table.get(fc_dgg..fc_dgg + lcb_dgg).unwrap_or(&[]), &word);

        // Walk the main-document text paragraph by paragraph, assembling nodes.
        let mut doc = DoclingDocument::new(&source.name);
        let mut builder = NodeBuilder::new(lists);
        let mut para = ParaAccum::default();
        let mut cp: u64 = 0;
        'pieces: for piece in &pieces {
            let count = piece.cp_end.saturating_sub(piece.cp_start);
            for i in 0..count {
                if cp >= ccp_text {
                    break 'pieces;
                }
                let ch = piece_char(&word, piece, i);
                let fc = piece_fc(piece, i);
                cp += 1;
                match ch {
                    '\r' | '\u{0007}' | '\u{000C}' => {
                        // Paragraph / cell / page mark: property lookup is by
                        // the mark's own FC.
                        let props = paragraph_props(&word, bte, fc);
                        para.finish(ch, props, &stis, &mut builder, &mut doc);
                    }
                    // Inline picture anchor: the run's CHPX locates the PICF.
                    '\u{0001}' => {
                        if let Some(pic_fc) = chpx_cache.props(&word, btec, fc).pic_fc {
                            para.add_picture(inline_picture(&data, pic_fc));
                        }
                    }
                    // Floating-shape anchor: PlcfSpa maps this CP to a shape.
                    '\u{0008}' => {
                        if let Some(spid) = spa_shape_at(&spa, cp - 1) {
                            for image in drawings.shape_pictures(spid, 0) {
                                para.add_picture(image);
                            }
                        }
                    }
                    _ => para.push(ch, chpx_cache.props(&word, btec, fc)),
                }
            }
        }
        para.finish('\r', ParaProps::default(), &stis, &mut builder, &mut doc);
        builder.flush(&mut doc);
        Ok(doc)
    }
}

/// One piece-table entry: characters `cp_start..cp_end` live at byte offset
/// `fc` (already unmasked) — CP1252 bytes when `compressed`, else UTF-16LE.
struct Piece {
    cp_start: u64,
    cp_end: u64,
    fc: u64,
    compressed: bool,
}

/// The `i`-th character of a piece (bounds-safe; U+FFFD off the end).
fn piece_char(word: &[u8], piece: &Piece, i: u64) -> char {
    if piece.compressed {
        let b = word.get((piece.fc + i) as usize).copied().unwrap_or(0);
        cp1252(b)
    } else {
        let o = (piece.fc + 2 * i) as usize;
        let u = u16_at(word, o).unwrap_or(0xFFFD);
        char::from_u32(u as u32).unwrap_or('\u{FFFD}')
    }
}

/// The byte offset (FC) of the `i`-th character of a piece.
fn piece_fc(piece: &Piece, i: u64) -> u64 {
    if piece.compressed {
        piece.fc + i
    } else {
        piece.fc + 2 * i
    }
}

/// Parse the CLX into pieces. The CLX is a run of `Prc` blocks (0x01, skipped)
/// followed by the `Pcdt` (0x02) holding the PlcPcd.
fn parse_piece_table(clx: &[u8]) -> Option<Vec<Piece>> {
    let mut pos = 0usize;
    loop {
        match clx.get(pos)? {
            0x01 => {
                let cb = u16_at(clx, pos + 1)? as usize;
                pos += 3 + cb;
            }
            0x02 => {
                let lcb = u32_at(clx, pos + 1)? as usize;
                let plc = clx.get(pos + 5..pos + 5 + lcb)?;
                // n pieces: (n+1) CPs (4 bytes) + n PCDs (8 bytes).
                let n = (lcb.checked_sub(4)?) / 12;
                let mut pieces = Vec::with_capacity(n);
                for i in 0..n {
                    let cp_start = u32_at(plc, i * 4)? as u64;
                    let cp_end = u32_at(plc, (i + 1) * 4)? as u64;
                    let pcd = (n + 1) * 4 + i * 8;
                    let fc_raw = u32_at(plc, pcd + 2)?;
                    let compressed = fc_raw & 0x4000_0000 != 0;
                    let fc = if compressed {
                        ((fc_raw & 0x3FFF_FFFF) / 2) as u64
                    } else {
                        fc_raw as u64
                    };
                    pieces.push(Piece {
                        cp_start,
                        cp_end,
                        fc,
                        compressed,
                    });
                }
                return Some(pieces);
            }
            _ => return None,
        }
    }
}

/// Properties of one paragraph, read from its PAPX.
#[derive(Default, Clone, Copy)]
struct ParaProps {
    istd: u16,
    in_table: bool,
    ttp: bool,
    /// List-format reference (`sprmPIlfo`): non-zero → the paragraph is a
    /// numbered/bulleted list item.
    ilfo: u16,
    /// List nesting level (`sprmPIlvl`), 0-based.
    ilvl: u8,
}

/// Look up the PAPX for the paragraph containing byte offset `fc`:
/// PlcfBtePapx → FKP page (512 bytes in the WordDocument stream) → PapxInFkp.
fn paragraph_props(word: &[u8], bte: &[u8], fc: u64) -> ParaProps {
    let mut props = ParaProps::default();
    let Some(n) = bte.len().checked_sub(4).map(|l| l / 8) else {
        return props;
    };
    if n == 0 {
        return props;
    }
    // aFc[i] <= fc < aFc[i+1] selects PN i.
    let mut pn = None;
    for i in 0..n {
        let lo = u32_at(bte, i * 4).unwrap_or(u32::MAX) as u64;
        let hi = u32_at(bte, (i + 1) * 4).unwrap_or(0) as u64;
        if fc >= lo && fc < hi {
            pn = u32_at(bte, (n + 1) * 4 + i * 4);
            break;
        }
    }
    let Some(pn) = pn else { return props };
    let page_off = (pn & 0x003F_FFFF) as usize * 512;
    let Some(page) = word.get(page_off..page_off + 512) else {
        return props;
    };
    let crun = page[511] as usize;
    if crun == 0 || (crun + 1) * 4 + crun * 13 > 511 {
        return props;
    }
    // rgfc[j] <= fc < rgfc[j+1] selects BxPap j.
    let mut run = None;
    for j in 0..crun {
        let lo = u32_at(page, j * 4).unwrap_or(u32::MAX) as u64;
        let hi = u32_at(page, (j + 1) * 4).unwrap_or(0) as u64;
        if fc >= lo && fc < hi {
            run = Some(j);
            break;
        }
    }
    let Some(j) = run else { return props };
    let b_offset = page[(crun + 1) * 4 + j * 13] as usize;
    if b_offset == 0 {
        return props; // default PAP
    }
    // PapxInFkp at word offset b_offset: cb, or 0 + cb'.
    let mut o = b_offset * 2;
    let Some(&cb) = page.get(o) else { return props };
    let grpprl_len = if cb == 0 {
        o += 2;
        page.get(b_offset * 2 + 1).map(|&c| c as usize * 2)
    } else {
        o += 1;
        Some(cb as usize * 2 - 1)
    };
    let Some(len) = grpprl_len else { return props };
    let Some(grpprl) = page.get(o..(o + len).min(512)) else {
        return props;
    };
    if grpprl.len() < 2 {
        return props;
    }
    props.istd = u16::from_le_bytes([grpprl[0], grpprl[1]]);
    apply_pap_sprms(&grpprl[2..], &mut props);
    props
}

/// Scan a PAP grpprl for the sprms this backend cares about.
fn apply_pap_sprms(mut sprms: &[u8], props: &mut ParaProps) {
    while sprms.len() >= 2 {
        let sprm = u16::from_le_bytes([sprms[0], sprms[1]]);
        sprms = &sprms[2..];
        let spra = sprm >> 13;
        let operand_len = match spra {
            0 | 1 => 1,
            2 | 4 | 5 => 2,
            3 => 4,
            7 => 3,
            _ => {
                // Variable: first byte is the operand size (sprmTDefTable uses
                // a 16-bit size; it never appears in a PAP grpprl we scan).
                match sprms.first() {
                    Some(&cb) => 1 + cb as usize,
                    None => return,
                }
            }
        };
        if sprms.len() < operand_len {
            return;
        }
        match sprm {
            0x2416 => props.in_table = sprms[0] != 0, // sprmPFInTable
            0x2417 => props.ttp = sprms[0] != 0,      // sprmPFTtp
            0x460B => props.ilfo = u16::from_le_bytes([sprms[0], sprms[1]]), // sprmPIlfo
            0x260A => props.ilvl = sprms[0],          // sprmPIlvl
            _ => {}
        }
        sprms = &sprms[operand_len..];
    }
}

/// Parse the STSH into `istd → sti` (built-in style identifier). `sti` 1–9 are
/// the Heading 1–9 styles.
fn parse_stsh(stsh: &[u8]) -> Vec<u16> {
    let Some(cb_stshi) = u16_at(stsh, 0) else {
        return Vec::new();
    };
    let Some(cstd) = u16_at(stsh, 2) else {
        return Vec::new();
    };
    let mut stis = Vec::with_capacity(cstd as usize);
    let mut pos = 2 + cb_stshi as usize;
    for _ in 0..cstd {
        let Some(cb_std) = u16_at(stsh, pos) else {
            break;
        };
        pos += 2;
        // Empty slot: cbStd == 0.
        let sti = if cb_std >= 2 {
            u16_at(stsh, pos).map(|w| w & 0x0FFF).unwrap_or(0x0FFF)
        } else {
            0x0FFF
        };
        stis.push(sti);
        pos += cb_std as usize;
        // LPStd entries are 2-byte aligned.
        pos += pos & 1;
    }
    stis
}

/// Character formatting of one run (the subset the Markdown output shows),
/// plus the picture-data offset when the run is a picture anchor.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
struct CharFmt {
    bold: bool,
    italic: bool,
    /// `sprmCPicLocation`: offset of the run's PICF in the Data stream (the
    /// run's `0x01` character is an inline-picture anchor).
    pic_fc: Option<u32>,
}

/// FC → [`CharFmt`] through the PlcfBteChpx and CHPX FKPs, memoizing the last
/// run's FC range — consecutive characters nearly always share a run, so the
/// per-character lookup is amortized to a range check.
#[derive(Default)]
struct ChpxCache {
    lo: u64,
    hi: u64,
    fmt: CharFmt,
}

impl ChpxCache {
    fn props(&mut self, word: &[u8], btec: &[u8], fc: u64) -> CharFmt {
        if fc >= self.lo && fc < self.hi {
            return self.fmt;
        }
        let (fmt, lo, hi) = char_props(word, btec, fc);
        self.lo = lo;
        self.hi = hi;
        self.fmt = fmt;
        fmt
    }
}

/// Look up the CHPX for the character at `fc`: PlcfBteChpx → CHPX FKP page →
/// grpprl scan for `sprmCFBold`/`sprmCFItalic`. Returns the format and the FC
/// range it covers (for the cache).
fn char_props(word: &[u8], btec: &[u8], fc: u64) -> (CharFmt, u64, u64) {
    let fmt = CharFmt::default();
    let Some(n) = btec.len().checked_sub(4).map(|l| l / 8) else {
        return (fmt, fc, fc + 1);
    };
    if n == 0 {
        return (fmt, fc, fc + 1);
    }
    let mut pn = None;
    for i in 0..n {
        let lo = u32_at(btec, i * 4).unwrap_or(u32::MAX) as u64;
        let hi = u32_at(btec, (i + 1) * 4).unwrap_or(0) as u64;
        if fc >= lo && fc < hi {
            pn = u32_at(btec, (n + 1) * 4 + i * 4);
            break;
        }
    }
    let Some(pn) = pn else {
        return (fmt, fc, fc + 1);
    };
    let page_off = (pn & 0x003F_FFFF) as usize * 512;
    let Some(page) = word.get(page_off..page_off + 512) else {
        return (fmt, fc, fc + 1);
    };
    let crun = page[511] as usize;
    if crun == 0 || (crun + 1) * 4 + crun > 511 {
        return (fmt, fc, fc + 1);
    }
    for j in 0..crun {
        let lo = u32_at(page, j * 4).unwrap_or(u32::MAX) as u64;
        let hi = u32_at(page, (j + 1) * 4).unwrap_or(0) as u64;
        if fc < lo || fc >= hi {
            continue;
        }
        // rgb: crun 1-byte word offsets after the FC array; 0 → no CHPX.
        let b = page[(crun + 1) * 4 + j] as usize;
        let mut out = CharFmt::default();
        if b != 0 {
            if let Some(&cb) = page.get(b * 2) {
                if let Some(grpprl) = page.get(b * 2 + 1..(b * 2 + 1 + cb as usize).min(512)) {
                    apply_chp_sprms(grpprl, &mut out);
                }
            }
        }
        return (out, lo, hi);
    }
    (fmt, fc, fc + 1)
}

/// Scan a CHP grpprl for bold/italic. Operand 1 = on, 0x81 = "opposite of the
/// style" — treated as on (the styles the Markdown output cares about are not
/// themselves bold/italic).
fn apply_chp_sprms(mut sprms: &[u8], fmt: &mut CharFmt) {
    while sprms.len() >= 2 {
        let sprm = u16::from_le_bytes([sprms[0], sprms[1]]);
        sprms = &sprms[2..];
        let operand_len = match sprm >> 13 {
            0 | 1 => 1,
            2 | 4 | 5 => 2,
            3 => 4,
            7 => 3,
            _ => match sprms.first() {
                Some(&cb) => 1 + cb as usize,
                None => return,
            },
        };
        if sprms.len() < operand_len {
            return;
        }
        match sprm {
            0x0835 => fmt.bold = sprms[0] == 1 || sprms[0] == 0x81, // sprmCFBold
            0x0836 => fmt.italic = sprms[0] == 1 || sprms[0] == 0x81, // sprmCFItalic
            // sprmCPicLocation: PICF offset in the Data stream.
            0x6A03 => {
                fmt.pic_fc = Some(u32::from_le_bytes([sprms[0], sprms[1], sprms[2], sprms[3]]))
            }
            _ => {}
        }
        sprms = &sprms[operand_len..];
    }
}

/// Decode an inline picture: PICF at `pic_fc` in the Data stream — a header
/// of `cbHeader` bytes (with the total size in `lcb`), then the OfficeArt
/// record tree holding the BLIP. `MM_SHAPEFILE` (0x66) interposes a
/// length-prefixed picture name.
fn inline_picture(data: &[u8], pic_fc: u32) -> Option<PictureImage> {
    let base = pic_fc as usize;
    let lcb = u32::from_le_bytes(data.get(base..base + 4)?.try_into().ok()?) as usize;
    let cb_header = u16::from_le_bytes(data.get(base + 4..base + 6)?.try_into().ok()?) as usize;
    let mm = u16::from_le_bytes(data.get(base + 6..base + 8)?.try_into().ok()?);
    let mut start = base + cb_header;
    if mm == 0x0066 {
        // MM_SHAPEFILE: cchPicName + name precede the OfficeArt data.
        let cch = *data.get(start)? as usize;
        start += 1 + cch;
    }
    let body = data.get(start..base + lcb.max(cb_header))?;
    officeart::first_blip(body, 0)
}

/// Parse the PlcfSpa: floating-shape anchors as `(anchor CP, spid)`.
fn parse_plcf_spa(plc: &[u8]) -> Vec<(u64, u32)> {
    // PLC with 26-byte SPA data: n = (lcb - 4) / 30.
    let Some(n) = plc.len().checked_sub(4).map(|l| l / 30) else {
        return Vec::new();
    };
    (0..n)
        .filter_map(|i| {
            let cp = u32_at(plc, i * 4)? as u64;
            let spid = u32_at(plc, (n + 1) * 4 + i * 26)?;
            Some((cp, spid))
        })
        .collect()
}

/// The spid anchored exactly at `cp`, if any.
fn spa_shape_at(spa: &[(u64, u32)], cp: u64) -> Option<u32> {
    spa.iter()
        .find(|(acp, _)| *acp == cp)
        .map(|(_, spid)| *spid)
}

/// The document's OfficeArt drawing tables: shape id → BLIP index (`pib`),
/// and the BLIP store — each entry either embeds its BLIP record or points
/// at one in the WordDocument (delay) stream via `foDelay`.
struct Drawings {
    /// spid → 1-based pib.
    shape_pib: std::collections::HashMap<u32, u32>,
    /// Group-frame spid → member spids (an anchored group shows every
    /// member's picture).
    groups: std::collections::HashMap<u32, Vec<u32>>,
    /// BStore order: decoded pictures (delay-stream ones resolved eagerly).
    blips: Vec<Option<PictureImage>>,
}

impl Drawings {
    fn parse(dgg: &[u8], delay_stream: &[u8]) -> Self {
        let mut out = Self {
            shape_pib: std::collections::HashMap::new(),
            groups: std::collections::HashMap::new(),
            blips: Vec::new(),
        };
        // The OfficeArtContent in a Word file is NOT a plain record stream:
        // each per-drawing DgContainer is preceded by a raw byte
        // (OfficeArtWordDrawing.dgglbl), which would desynchronize a straight
        // record walk. Parse top-level records with a resync: a position that
        // doesn't hold a well-formed OfficeArt header (0xF0xx type, in-bounds
        // length) skips one byte.
        let mut pos = 0usize;
        while pos + 8 <= dgg.len() {
            let rec_type = u16::from_le_bytes([dgg[pos + 2], dgg[pos + 3]]);
            let len = u32::from_le_bytes([dgg[pos + 4], dgg[pos + 5], dgg[pos + 6], dgg[pos + 7]])
                as usize;
            if (rec_type & 0xFF00) == 0xF000 && pos + 8 + len <= dgg.len() {
                out.walk(&dgg[pos..pos + 8 + len], delay_stream, 0);
                pos += 8 + len;
            } else {
                pos += 1;
            }
        }
        out
    }

    /// spid of an SpContainer body (its FSP record).
    fn sp_spid(sp_body: &[u8]) -> Option<u32> {
        officeart::Records::new(sp_body)
            .find(|(h, _)| h.rec_type == 0xF00A)
            .and_then(|(_, b)| {
                b.get(..4)
                    .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
            })
    }

    fn walk(&mut self, body: &[u8], delay: &[u8], depth: usize) {
        if depth > 16 {
            return;
        }
        for (h, b) in officeart::Records::new(body) {
            match h.rec_type {
                // OfficeArtFBSE: BLIP store entry — embedded record or foDelay.
                0xF007 => {
                    let embedded = b.get(36..).and_then(|tail| officeart::first_blip(tail, 0));
                    let img = embedded.or_else(|| {
                        let fo = u32::from_le_bytes(b.get(28..32)?.try_into().ok()?) as usize;
                        let rec = delay.get(fo..)?;
                        officeart::Records::new(rec)
                            .next()
                            .filter(|(rh, _)| officeart::is_blip(rh.rec_type))
                            .and_then(|(rh, rb)| officeart::decode_blip(&rh, rb))
                    });
                    self.blips.push(img);
                }
                // A group: the frame shape's spid maps to the member spids.
                0xF003 => {
                    let mut frame = None;
                    let mut members = Vec::new();
                    for (h2, b2) in officeart::Records::new(b) {
                        match h2.rec_type {
                            0xF004 => {
                                let spid = Self::sp_spid(b2);
                                let is_frame = officeart::Records::new(b2)
                                    .any(|(h3, _)| h3.rec_type == 0xF009);
                                match (is_frame, frame) {
                                    (true, None) => frame = spid,
                                    _ => members.extend(spid),
                                }
                            }
                            0xF003 => {
                                // Nested group: its frame spid joins as a member.
                                let nested_frame = officeart::Records::new(b2)
                                    .find(|(h3, _)| h3.rec_type == 0xF004)
                                    .and_then(|(_, b3)| Self::sp_spid(b3));
                                members.extend(nested_frame);
                            }
                            _ => {}
                        }
                    }
                    if let Some(frame) = frame {
                        self.groups.insert(frame, members);
                    }
                    // Fall through to the generic recursion below for pib/blip
                    // collection inside the group.
                    self.walk(b, delay, depth + 1);
                }
                // OfficeArtSpContainer: read the FSP spid + FOPT pib property.
                0xF004 => {
                    let mut spid = None;
                    let mut pib = None;
                    for (h2, b2) in officeart::Records::new(b) {
                        match h2.rec_type {
                            0xF00A => {
                                spid = b2
                                    .get(..4)
                                    .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]));
                            }
                            0xF00B => {
                                // Fixed 6-byte property entries; id bits 0–13.
                                for e in 0..h2.instance as usize {
                                    let o = e * 6;
                                    let Some(entry) = b2.get(o..o + 6) else { break };
                                    let id = u16::from_le_bytes([entry[0], entry[1]]) & 0x3FFF;
                                    if id == 260 {
                                        pib = Some(u32::from_le_bytes([
                                            entry[2], entry[3], entry[4], entry[5],
                                        ]));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if let (Some(spid), Some(pib)) = (spid, pib) {
                        self.shape_pib.insert(spid, pib);
                    }
                }
                _ if h.version == 0xF => self.walk(b, delay, depth + 1),
                _ => {}
            }
        }
    }

    /// The pictures behind a shape id: a picture shape yields one; a group
    /// yields each member's, in order. `None` entries are undecodable images
    /// (still emitted as placeholders).
    fn shape_pictures(&self, spid: u32, depth: usize) -> Vec<Option<PictureImage>> {
        if depth > 8 {
            return Vec::new();
        }
        if let Some(pib) = self.shape_pib.get(&spid) {
            let img = pib
                .checked_sub(1)
                .and_then(|ix| self.blips.get(ix as usize))
                .cloned()
                .flatten();
            return vec![img];
        }
        match self.groups.get(&spid) {
            Some(members) => members
                .iter()
                .flat_map(|&m| self.shape_pictures(m, depth + 1))
                .collect(),
            None => Vec::new(),
        }
    }
}

/// One list level's numbering: kind and start value.
#[derive(Clone, Copy)]
struct LvlInfo {
    /// Number format code (`nfc`): 0x17 = bullet, 0xFF = none, else numbered.
    nfc: u8,
    start: u32,
}

/// The document's list tables: `ilfo` (1-based, from `sprmPIlfo`) resolves
/// through the LFO array to a list (`lsid`) and its per-level numbering.
#[derive(Default)]
struct ListTables {
    /// LFO index (0-based) → lsid.
    lfo_lsids: Vec<u32>,
    /// lsid → per-level info (1 entry for simple lists, 9 otherwise).
    lists: std::collections::HashMap<u32, Vec<LvlInfo>>,
}

impl ListTables {
    /// Parse the PlfLst (`lst_tail` starts at `fcPlfLst`; `lcb_lst` covers the
    /// LSTF array, and the per-list LVLs follow it in the stream) and the
    /// PlfLfo (LFO array). Any malformed structure yields what was parsed so
    /// far — unresolvable items degrade to bullets, never to an error.
    fn parse(lst_tail: &[u8], lcb_lst: usize, plflfo: &[u8]) -> Self {
        let plflst = lst_tail;
        let mut out = Self::default();
        // PlfLst: cLst u16, then cLst LSTFs of 28 bytes, then each list's LVLs.
        let c_lst = u16_at(plflst, 0).unwrap_or(0) as usize;
        let mut lstfs = Vec::with_capacity(c_lst);
        for i in 0..c_lst {
            let base = 2 + i * 28;
            let Some(lsid) = u32_at(plflst, base) else {
                break;
            };
            let simple = plflst.get(base + 26).is_some_and(|&f| f & 0x01 != 0);
            lstfs.push((lsid, if simple { 1usize } else { 9usize }));
        }
        let mut pos = (2 + c_lst * 28).max(lcb_lst);
        'lists: for (lsid, nlvl) in lstfs {
            let mut lvls = Vec::with_capacity(nlvl);
            for _ in 0..nlvl {
                // LVL = LVLF (28 bytes) + grpprlPapx + grpprlChpx + xst.
                let Some(start) = u32_at(plflst, pos) else {
                    break 'lists;
                };
                let Some(&nfc) = plflst.get(pos + 4) else {
                    break 'lists;
                };
                let cb_chpx = plflst.get(pos + 24).copied().unwrap_or(0) as usize;
                let cb_papx = plflst.get(pos + 25).copied().unwrap_or(0) as usize;
                pos += 28 + cb_papx + cb_chpx;
                let cch = u16_at(plflst, pos).unwrap_or(0) as usize;
                pos += 2 + cch * 2;
                lvls.push(LvlInfo { nfc, start });
            }
            out.lists.insert(lsid, lvls);
        }
        // PlfLfo: lfoMac u32, then lfoMac LFOs of 16 bytes (lsid first).
        let lfo_mac = u32_at(plflfo, 0).unwrap_or(0) as usize;
        for i in 0..lfo_mac {
            match u32_at(plflfo, 4 + i * 16) {
                Some(lsid) => out.lfo_lsids.push(lsid),
                None => break,
            }
        }
        out
    }

    /// Numbering info for `(ilfo, ilvl)`, when the tables resolve it.
    fn level(&self, ilfo: u16, ilvl: u8) -> Option<LvlInfo> {
        let lsid = *self.lfo_lsids.get(ilfo.checked_sub(1)? as usize)?;
        let lvls = self.lists.get(&lsid)?;
        lvls.get(ilvl as usize).or_else(|| lvls.first()).copied()
    }
}

/// Accumulates one paragraph's characters as `(text, format)` segments, then
/// classifies the paragraph on its mark.
#[derive(Default)]
struct ParaAccum {
    /// Consecutive same-format runs of the paragraph.
    segments: Vec<(String, CharFmt)>,
    /// Pictures anchored in this paragraph: `(before any text, image)`.
    pictures: Vec<(bool, Option<PictureImage>)>,
    /// Result-text state of any field (`0x13 code 0x14 result 0x15`) stack:
    /// characters inside the *code* part are dropped.
    field_stack: Vec<bool>, // true = in result part
}

impl ParaAccum {
    /// Queue a picture anchored at the current position (`None` = a picture
    /// whose bytes couldn't be decoded — still a placeholder node).
    fn add_picture(&mut self, image: Option<PictureImage>) {
        let before_text = self.segments.iter().all(|(t, _)| t.trim().is_empty());
        self.pictures.push((before_text, image));
    }

    fn push(&mut self, ch: char, fmt: CharFmt) {
        match ch {
            '\u{0013}' => self.field_stack.push(false),
            '\u{0014}' => {
                if let Some(top) = self.field_stack.last_mut() {
                    *top = true;
                }
            }
            '\u{0015}' => {
                self.field_stack.pop();
            }
            // Object/drawing/note anchors and other control marks: dropped.
            '\u{0001}' | '\u{0002}' | '\u{0005}' | '\u{0008}' => {}
            '\u{000B}' => self.keep('\n', fmt), // hard line break
            '\u{001E}' => self.keep('-', fmt),  // non-breaking hyphen
            '\u{001F}' => {}                    // soft hyphen
            _ => self.keep(ch, fmt),
        }
    }

    fn keep(&mut self, ch: char, fmt: CharFmt) {
        if !self.field_stack.iter().all(|&r| r) {
            return;
        }
        match self.segments.last_mut() {
            Some((text, last)) if *last == fmt => text.push(ch),
            _ => self.segments.push((ch.to_string(), fmt)),
        }
    }

    /// Flat text, formatting ignored (headings, table-structure decisions).
    fn plain(&self) -> String {
        self.segments.iter().map(|(t, _)| t.as_str()).collect()
    }

    /// Markdown text with `**bold**` / `*italic*` markers per run, whitespace
    /// kept outside the markers (matching the DOCX backend's rendering).
    fn markdown(&self) -> String {
        let mut out = String::new();
        for (text, fmt) in &self.segments {
            if !fmt.bold && !fmt.italic {
                out.push_str(text);
                continue;
            }
            let core = text.trim();
            if core.is_empty() {
                out.push_str(text);
                continue;
            }
            let lead = &text[..text.len() - text.trim_start().len()];
            let trail = &text[text.trim_end().len()..];
            let marker = match (fmt.bold, fmt.italic) {
                (true, true) => "***",
                (true, false) => "**",
                (false, true) => "*",
                _ => unreachable!(),
            };
            out.push_str(lead);
            out.push_str(marker);
            out.push_str(core);
            out.push_str(marker);
            out.push_str(trail);
        }
        out
    }

    fn finish(
        &mut self,
        mark: char,
        props: ParaProps,
        stis: &[u16],
        builder: &mut NodeBuilder,
        doc: &mut DoclingDocument,
    ) {
        let plain = self.plain();
        let markdown = self.markdown();
        let pictures = std::mem::take(&mut self.pictures);
        self.segments.clear();
        self.field_stack.clear();
        builder.paragraph(plain, markdown, pictures, mark, props, stis, doc);
    }
}

/// Turns the classified paragraph stream into nodes, assembling table runs.
#[derive(Default)]
struct NodeBuilder {
    /// In-progress table rows, current row's cells, and the current cell's
    /// accumulated paragraphs (a cell may span several).
    rows: Vec<Vec<String>>,
    cells: Vec<String>,
    cell_text: String,
    /// The `ilfo` of the most recent list item — a new item starts a new list
    /// (`first_in_list`) when its `ilfo` differs. Empty paragraphs between
    /// items do NOT break a list (docling's DOCX behavior: numbering and
    /// grouping continue across gaps); any other node kind does.
    last_ilfo: Option<u16>,
    lists: ListTables,
    /// Running ordered-list counters, keyed by `(ilfo, ilvl)`.
    counters: std::collections::HashMap<(u16, u8), u64>,
}

impl NodeBuilder {
    fn new(lists: ListTables) -> Self {
        Self {
            lists,
            ..Self::default()
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn paragraph(
        &mut self,
        plain: String,
        markdown: String,
        pictures: Vec<(bool, Option<PictureImage>)>,
        mark: char,
        props: ParaProps,
        stis: &[u16],
        doc: &mut DoclingDocument,
    ) {
        if props.ttp {
            // Row terminator: close the row.
            if !self.cell_text.is_empty() {
                self.cells.push(std::mem::take(&mut self.cell_text));
            }
            if !self.cells.is_empty() {
                self.rows.push(std::mem::take(&mut self.cells));
            }
            return;
        }
        if props.in_table {
            if !self.cell_text.is_empty() {
                // Multi-paragraph cells join with a blank line, matching the
                // DOCX backend's rich cells (the Markdown table serializer
                // then folds each newline into a space).
                self.cell_text.push_str("\n\n");
            }
            self.cell_text
                .push_str(markdown.trim_end_matches('\u{0007}'));
            if mark == '\u{0007}' {
                self.cells.push(std::mem::take(&mut self.cell_text));
            }
            return;
        }
        self.flush(doc);

        let picture_node = |image: Option<PictureImage>| Node::Picture {
            caption: None,
            image,
            classification: None,
        };
        let plain = plain.trim().to_string();
        let text = markdown.trim().to_string();
        if plain.is_empty() {
            // Pictures anchored in an otherwise-empty paragraph are blocks of
            // their own. A blank paragraph does not break a list run
            // (docling's DOCX behavior: items continue across gaps).
            for (_, image) in pictures {
                doc.push(picture_node(image));
            }
            return;
        }
        // Anchored pictures surround the paragraph text by anchor position.
        for (_, image) in pictures.iter().filter(|(before, _)| *before) {
            doc.push(picture_node(image.clone()));
        }
        let after: Vec<_> = pictures
            .into_iter()
            .filter(|(before, _)| !before)
            .map(|(_, image)| image)
            .collect();
        let sti = stis.get(props.istd as usize).copied().unwrap_or(0x0FFF);
        if (1..=9).contains(&sti) || sti == 62 {
            // Mirrors the DOCX backend: docling renders "heading N" at
            // Markdown level N+1 and Title (sti 62) at level 1.
            let level = if sti == 62 { 1 } else { sti as u8 + 1 };
            // Headings render without run markers (the style carries the look).
            doc.push(Node::Heading { level, text: plain });
            self.last_ilfo = None;
        } else if props.ilfo != 0 {
            let lvl = self.lists.level(props.ilfo, props.ilvl);
            // Bullet (0x17) / no-number (0xFF) levels — and unresolvable
            // references — are unordered; everything else numbers.
            let ordered = lvl.is_some_and(|l| l.nfc != 0x17 && l.nfc != 0xFF);
            let number = if ordered {
                let start = lvl.map(|l| l.start as u64).unwrap_or(1);
                let counter = self
                    .counters
                    .entry((props.ilfo, props.ilvl))
                    .or_insert(start);
                let n = *counter;
                *counter += 1;
                // A new item at this level restarts deeper levels.
                self.counters
                    .retain(|&(f, l), _| f != props.ilfo || l <= props.ilvl);
                n
            } else {
                1
            };
            doc.push(Node::ListItem {
                ordered,
                number,
                first_in_list: self.last_ilfo != Some(props.ilfo),
                text,
                level: props.ilvl,
                marker: None,
                location: None,
                dclx: None,
                href: None,
                layer: None,
            });
            self.last_ilfo = Some(props.ilfo);
        } else {
            doc.push(Node::Paragraph { text });
            self.last_ilfo = None;
        }
        for image in after {
            doc.push(picture_node(image));
        }
    }

    /// Emit any table under construction.
    fn flush(&mut self, doc: &mut DoclingDocument) {
        if !self.cell_text.is_empty() {
            self.cells.push(std::mem::take(&mut self.cell_text));
        }
        if !self.cells.is_empty() {
            self.rows.push(std::mem::take(&mut self.cells));
        }
        if self.rows.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.rows);
        // Rectangularize (rows may have ragged cell counts).
        let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let rows: Vec<Vec<String>> = rows
            .into_iter()
            .map(|mut r| {
                r.resize(width, String::new());
                r
            })
            .collect();
        doc.push(Node::Table(Table {
            rows,
            location: None,
            structure: None,
            cell_blocks: None,
        }));
        self.last_ilfo = None;
    }
}

fn u16_at(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

/// Windows-1252 to Unicode (the 0x80–0x9F block differs from Latin-1).
pub(crate) fn cp1252(b: u8) -> char {
    match b {
        0x80 => '€',
        0x82 => '‚',
        0x83 => 'ƒ',
        0x84 => '„',
        0x85 => '…',
        0x86 => '†',
        0x87 => '‡',
        0x88 => 'ˆ',
        0x89 => '‰',
        0x8A => 'Š',
        0x8B => '‹',
        0x8C => 'Œ',
        0x8E => 'Ž',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x98 => '˜',
        0x99 => '™',
        0x9A => 'š',
        0x9B => '›',
        0x9C => 'œ',
        0x9E => 'ž',
        0x9F => 'Ÿ',
        other => other as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InputFormat;

    fn fixture(name: &str) -> SourceDocument {
        let path = format!(
            "{}/tests/data/doc/sources/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).expect("fixture exists");
        SourceDocument::from_bytes(name, InputFormat::Doc, bytes)
    }

    #[test]
    fn extracts_headings_lists_and_paragraphs() {
        let doc = DocBackend
            .convert(&fixture("docx_lists.doc"))
            .expect("converts");
        let headings = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::Heading { .. }))
            .count();
        let lists = doc
            .nodes
            .iter()
            .filter(|n| matches!(n, Node::ListItem { .. }))
            .count();
        assert!(headings > 0, "expected headings: {:?}", doc.nodes);
        assert!(lists > 0, "expected list items: {:?}", doc.nodes);
    }

    #[test]
    fn extracts_tables_with_cells() {
        let doc = DocBackend
            .convert(&fixture("docx_rich_tables_01.doc"))
            .expect("converts");
        let tables: Vec<&Table> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Table(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(!tables.is_empty(), "expected tables: {:?}", doc.nodes);
        assert!(tables[0].rows.len() > 1 && tables[0].rows[0].len() > 1);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let src = SourceDocument::from_bytes("x.doc", InputFormat::Doc, vec![0u8; 128]);
        assert!(DocBackend.convert(&src).is_err());
    }

    #[test]
    fn extracts_inline_and_floating_images_with_bytes() {
        let doc = DocBackend
            .convert(&fixture("docx_grouped_images.doc"))
            .expect("converts");
        let images: Vec<_> = doc
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Picture { image, .. } => Some(image),
                _ => None,
            })
            .collect();
        // 2 grouped + 2 wrapped (floating, via PlcfSpa → Escher pib → delay
        // stream) + 2 inline (Data-stream PICF) — and every one decodable.
        assert_eq!(images.len(), 6, "expected 6 pictures: {:?}", doc.nodes);
        assert!(
            images.iter().all(|i| i.is_some()),
            "every picture should carry decoded bytes"
        );
        let img = images[0].as_ref().unwrap();
        assert!(img.width > 0 && img.height > 0 && !img.data.is_empty());
    }
}
