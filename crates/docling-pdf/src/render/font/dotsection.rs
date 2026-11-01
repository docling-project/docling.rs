//! Strip the Type 2 `dotsection` operator from a CFF program before the
//! font parser reads it (#531).
//!
//! `dotsection` (`12 0`) is a Type 1 hint Adobe's converters carry over
//! into CFF: FrameMaker-era Helvetica / Times subsets put it in front of
//! every dot — the period, the dot of `i` and `j`, the colon. The Type 2
//! spec (5177, Appendix B) lists it as deprecated and to be *ignored*, and
//! FreeType, pdfium and docling-parse do, but ttf-parser — the renderer's
//! font parser until #952 — stopped the glyph at it with
//! `UnsupportedOperator`: the period drew nothing and the `i` kept its stem
//! and lost its dot. Dot leaders in an index vanished, and the layout model
//! read the page as a table instead of a document index. skrifa (read-fonts)
//! ignores the operator as the spec says; the rewrite stays as a belt to
//! that brace — it is cheap, exact, and keeps every reader of the stripped
//! bytes (the fallback-face probe, a future parser) on the same footing.
//!
//! The operator is removed in place. Every CFF structure is located through
//! absolute offsets, so a charstring INDEX can be rewritten shorter at its
//! own position — the same offset size, the freed bytes left behind as an
//! unused gap — without moving anything else. Finding the operators needs a
//! real tokenizer, not a byte search: a `hintmask` is followed by mask bytes
//! whose count depends on the stems declared so far (in the glyph or in a
//! subroutine it calls), so the glyphs are walked with their stem count, the
//! operand stack (for subroutine indices) and the subroutines they call. An
//! occurrence is removed only where the stack is empty in every context it
//! is reached from — the case Adobe's converters write, and the one where
//! deleting it cannot change what the next operator sees.

use std::collections::BTreeMap;

/// Remove the `dotsection` operators of a bare CFF program in place;
/// returns how many were removed. A font without any (or one this cannot
/// walk) is left untouched.
pub fn strip(cff: &mut [u8]) -> usize {
    if !cff.windows(2).any(|w| w == [12, 0]) {
        return 0;
    }
    let Some(font) = Font::parse(cff) else {
        return 0;
    };
    let mut walker = Walker {
        d: cff,
        font: &font,
        stems: 0,
        stack: Vec::new(),
        budget: 4_000_000,
        found: BTreeMap::new(),
    };
    for g in 0..font.char_strings.count() {
        walker.stems = 0;
        walker.stack.clear();
        let local = font.local_for(g);
        let (s, e) = font.char_strings.entry(g);
        if let Flow::Fail = walker.walk(s, e, Target::Glyph, local, 0) {
            // A glyph we cannot follow (arithmetic operators, a broken
            // subroutine index): leave the font as the parser sees it.
            return 0;
        }
    }
    let mut removals: BTreeMap<Target, Vec<usize>> = BTreeMap::new();
    for ((target, pos), safe) in walker.found {
        if safe {
            removals.entry(target).or_default().push(pos);
        }
    }
    let mut removed = 0;
    for (target, positions) in removals {
        let index = match target {
            Target::Glyph => &font.char_strings,
            Target::Global => &font.global_subrs,
            Target::Local(fd) => match font.local_subrs.get(fd).and_then(Option::as_ref) {
                Some(i) => i,
                None => continue,
            },
        };
        removed += compact(cff, index, &positions);
    }
    removed
}

/// Run [`strip`] over the `CFF ` table of an OpenType font.
pub fn strip_sfnt(sfnt: &mut [u8]) -> usize {
    let be16 = |d: &[u8], at: usize| Some(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]));
    let be32 = |d: &[u8], at: usize| {
        Some(u32::from_be_bytes([
            *d.get(at)?,
            *d.get(at + 1)?,
            *d.get(at + 2)?,
            *d.get(at + 3)?,
        ]))
    };
    let Some(n) = be16(sfnt, 4) else { return 0 };
    for k in 0..usize::from(n) {
        let rec = 12 + k * 16;
        if sfnt.get(rec..rec + 4) != Some(b"CFF ") {
            continue;
        }
        let (Some(off), Some(len)) = (be32(sfnt, rec + 8), be32(sfnt, rec + 12)) else {
            return 0;
        };
        let (off, len) = (off as usize, len as usize);
        return match sfnt.get_mut(off..off.saturating_add(len)) {
            Some(table) => strip(table),
            None => 0,
        };
    }
    0
}

/// Which INDEX a charstring lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    Glyph,
    Global,
    /// The local subroutines of font dict `n` (0 for a name-keyed font).
    Local(usize),
}

/// A CFF INDEX: entry `i` is `d[offsets[i]..offsets[i + 1]]`.
struct Index {
    off_size: usize,
    /// Where the offset array starts.
    offsets_at: usize,
    /// Absolute entry boundaries (`count + 1` of them).
    bounds: Vec<usize>,
}

impl Index {
    fn read(d: &[u8], at: usize) -> Option<(Index, usize)> {
        let count = usize::from(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]));
        if count == 0 {
            let empty = Index {
                off_size: 1,
                offsets_at: at + 3,
                bounds: Vec::new(),
            };
            return Some((empty, at + 2));
        }
        let off_size = usize::from(*d.get(at + 2)?);
        if !(1..=4).contains(&off_size) {
            return None;
        }
        let offsets_at = at + 3;
        let data_start = offsets_at + (count + 1) * off_size - 1;
        let mut bounds = Vec::with_capacity(count + 1);
        for i in 0..=count {
            let o = offsets_at + i * off_size;
            let raw = d
                .get(o..o + off_size)?
                .iter()
                .fold(0usize, |v, &b| (v << 8) | usize::from(b));
            bounds.push(data_start + raw);
        }
        if bounds.windows(2).any(|w| w[0] > w[1]) || *bounds.last()? > d.len() {
            return None;
        }
        let end = *bounds.last()?;
        Some((
            Index {
                off_size,
                offsets_at,
                bounds,
            },
            end,
        ))
    }

    fn count(&self) -> usize {
        self.bounds.len().saturating_sub(1)
    }

    fn entry(&self, i: usize) -> (usize, usize) {
        (self.bounds[i], self.bounds[i + 1])
    }

    /// Type 2 subroutine bias.
    fn bias(&self) -> i64 {
        match self.count() {
            n if n < 1240 => 107,
            n if n < 33900 => 1131,
            _ => 32768,
        }
    }
}

/// The parts of a CFF font the walk needs.
struct Font {
    char_strings: Index,
    global_subrs: Index,
    /// Local subroutines per font dict (one for a name-keyed font).
    local_subrs: Vec<Option<Index>>,
    /// Glyph → font dict, for a CID-keyed font.
    fd_select: Option<Vec<u8>>,
}

impl Font {
    fn parse(d: &[u8]) -> Option<Font> {
        let hdr = usize::from(*d.get(2)?);
        let (_, after_names) = Index::read(d, hdr)?;
        let (top, after_top) = Index::read(d, after_names)?;
        if top.count() == 0 {
            return None;
        }
        let (ts, te) = top.entry(0);
        let top = dict(d.get(ts..te)?)?;
        let (_, after_strings) = Index::read(d, after_top)?;
        let (global_subrs, _) = Index::read(d, after_strings)?;
        // Charstring type 2 only (the default).
        if top.get(&0x0c06).is_some_and(|v| v.first() != Some(&2.0)) {
            return None;
        }
        let cs_at = *top.get(&17)?.first()? as usize;
        let (char_strings, _) = Index::read(d, cs_at)?;
        let n_glyphs = char_strings.count();

        let (local_subrs, fd_select) = if let Some(fda) = top.get(&0x0c24) {
            let (fd_array, _) = Index::read(d, *fda.first()? as usize)?;
            let mut locals = Vec::with_capacity(fd_array.count());
            for k in 0..fd_array.count() {
                let (s, e) = fd_array.entry(k);
                locals.push(private_subrs(d, &dict(d.get(s..e)?)?));
            }
            let sel_at = *top.get(&0x0c25)?.first()? as usize;
            (locals, Some(fd_select(d, sel_at, n_glyphs)?))
        } else {
            (vec![private_subrs(d, &top)], None)
        };
        Some(Font {
            char_strings,
            global_subrs,
            local_subrs,
            fd_select,
        })
    }

    /// The local subroutines glyph `g` calls into, with their font dict.
    fn local_for(&self, g: usize) -> Option<usize> {
        let fd = match &self.fd_select {
            Some(sel) => usize::from(*sel.get(g)?),
            None => 0,
        };
        self.local_subrs.get(fd)?.as_ref().map(|_| fd)
    }
}

/// A Private DICT's `Subrs` INDEX (its offset is relative to the dict).
fn private_subrs(d: &[u8], font_dict: &BTreeMap<u16, Vec<f64>>) -> Option<Index> {
    let p = font_dict.get(&18)?;
    let (size, off) = (*p.first()? as usize, *p.get(1)? as usize);
    let private = dict(d.get(off..off.checked_add(size)?)?)?;
    let subrs = off + *private.get(&19)?.first()? as usize;
    Index::read(d, subrs).map(|(i, _)| i)
}

/// FDSelect formats 0 and 3 as a glyph → font dict table.
fn fd_select(d: &[u8], at: usize, n_glyphs: usize) -> Option<Vec<u8>> {
    match *d.get(at)? {
        0 => Some(d.get(at + 1..at + 1 + n_glyphs)?.to_vec()),
        3 => {
            let be16 = |o: usize| {
                Some(usize::from(u16::from_be_bytes([
                    *d.get(o)?,
                    *d.get(o + 1)?,
                ])))
            };
            let n = be16(at + 1)?;
            let mut out = vec![0u8; n_glyphs];
            for r in 0..n {
                let rec = at + 3 + r * 3;
                let (first, fd) = (be16(rec)?, *d.get(rec + 2)?);
                let next = be16(rec + 3)?;
                for slot in out.iter_mut().take(next.min(n_glyphs)).skip(first) {
                    *slot = fd;
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// A DICT's operators (an escaped one as `0x0c00 | op2`) with their operands.
fn dict(b: &[u8]) -> Option<BTreeMap<u16, Vec<f64>>> {
    let mut out = BTreeMap::new();
    let mut operands = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let b0 = b[i];
        match b0 {
            0..=21 => {
                let op = if b0 == 12 {
                    i += 1;
                    0x0c00 | u16::from(*b.get(i)?)
                } else {
                    u16::from(b0)
                };
                out.insert(op, std::mem::take(&mut operands));
                i += 1;
            }
            28 => {
                operands.push(f64::from(i16::from_be_bytes([
                    *b.get(i + 1)?,
                    *b.get(i + 2)?,
                ])));
                i += 3;
            }
            29 => {
                let v = i32::from_be_bytes([
                    *b.get(i + 1)?,
                    *b.get(i + 2)?,
                    *b.get(i + 3)?,
                    *b.get(i + 4)?,
                ]);
                operands.push(f64::from(v));
                i += 5;
            }
            30 => {
                // A real: nibbles up to the 0xf terminator; its value is
                // never an offset, so a placeholder will do.
                i += 1;
                while i < b.len() && b[i] & 0x0f != 0x0f && b[i] >> 4 != 0x0f {
                    i += 1;
                }
                i += 1;
                operands.push(0.0);
            }
            32..=246 => {
                operands.push(f64::from(b0) - 139.0);
                i += 1;
            }
            247..=250 => {
                operands.push((f64::from(b0) - 247.0) * 256.0 + f64::from(*b.get(i + 1)?) + 108.0);
                i += 2;
            }
            251..=254 => {
                operands.push(-(f64::from(b0) - 251.0) * 256.0 - f64::from(*b.get(i + 1)?) - 108.0);
                i += 2;
            }
            _ => return None,
        }
    }
    Some(out)
}

enum Flow {
    /// The charstring returned (`return`, or ran off its end).
    Continue,
    /// `endchar`.
    End,
    /// Something this walk does not model.
    Fail,
}

struct Walker<'a> {
    d: &'a [u8],
    font: &'a Font,
    stems: usize,
    stack: Vec<f64>,
    budget: usize,
    /// `dotsection` positions → removable (the stack was empty everywhere
    /// it was reached).
    found: BTreeMap<(Target, usize), bool>,
}

impl Walker<'_> {
    fn walk(
        &mut self,
        start: usize,
        end: usize,
        target: Target,
        local: Option<usize>,
        depth: u8,
    ) -> Flow {
        if depth > 10 {
            return Flow::Fail;
        }
        let d = self.d;
        let mut pos = start;
        while pos < end {
            if self.budget == 0 || self.stack.len() > 48 {
                return Flow::Fail;
            }
            self.budget -= 1;
            let b0 = d[pos];
            let byte = |k: usize| d.get(pos + k).copied().filter(|_| pos + k < end);
            match b0 {
                32..=246 => {
                    self.stack.push(f64::from(b0) - 139.0);
                    pos += 1;
                }
                247..=250 => {
                    let Some(b1) = byte(1) else { return Flow::Fail };
                    self.stack
                        .push((f64::from(b0) - 247.0) * 256.0 + f64::from(b1) + 108.0);
                    pos += 2;
                }
                251..=254 => {
                    let Some(b1) = byte(1) else { return Flow::Fail };
                    self.stack
                        .push(-(f64::from(b0) - 251.0) * 256.0 - f64::from(b1) - 108.0);
                    pos += 2;
                }
                28 => {
                    let (Some(a), Some(b)) = (byte(1), byte(2)) else {
                        return Flow::Fail;
                    };
                    self.stack.push(f64::from(i16::from_be_bytes([a, b])));
                    pos += 3;
                }
                255 => {
                    if pos + 5 > end {
                        return Flow::Fail;
                    }
                    let v = i32::from_be_bytes([d[pos + 1], d[pos + 2], d[pos + 3], d[pos + 4]]);
                    self.stack.push(f64::from(v) / 65536.0);
                    pos += 5;
                }
                // hstem, vstem, hstemhm, vstemhm (the first may carry the
                // width, which the halving drops).
                1 | 3 | 18 | 23 => {
                    self.stems += self.stack.len() / 2;
                    self.stack.clear();
                    pos += 1;
                }
                // hintmask / cntrmask: operands are an implicit vstem.
                19 | 20 => {
                    self.stems += self.stack.len() / 2;
                    self.stack.clear();
                    pos += 1 + self.stems.div_ceil(8);
                }
                10 | 29 => {
                    let Some(n) = self.stack.pop() else {
                        return Flow::Fail;
                    };
                    let (index, sub_target) = if b0 == 10 {
                        let Some(fd) = local else { return Flow::Fail };
                        match self.font.local_subrs.get(fd).and_then(Option::as_ref) {
                            Some(i) => (i, Target::Local(fd)),
                            None => return Flow::Fail,
                        }
                    } else {
                        (&self.font.global_subrs, Target::Global)
                    };
                    let k = n as i64 + index.bias();
                    if k < 0 || k as usize >= index.count() {
                        return Flow::Fail;
                    }
                    let (s, e) = index.entry(k as usize);
                    match self.walk(s, e, sub_target, local, depth + 1) {
                        Flow::Continue => {}
                        other => return other,
                    }
                    pos += 1;
                }
                11 => return Flow::Continue,
                14 => return Flow::End,
                12 => {
                    let Some(op2) = byte(1) else {
                        return Flow::Fail;
                    };
                    match op2 {
                        0 => {
                            let empty = self.stack.is_empty();
                            let safe = self.found.entry((target, pos)).or_insert(true);
                            *safe &= empty;
                        }
                        // The flex family; anything else (arithmetic,
                        // storage) a parser rejects too.
                        34..=37 => {}
                        _ => return Flow::Fail,
                    }
                    self.stack.clear();
                    pos += 2;
                }
                // Path and other operators clear the stack.
                0..=31 => {
                    self.stack.clear();
                    pos += 1;
                }
            }
        }
        Flow::Continue
    }
}

/// Rewrite `index` in place without the two bytes at each of `positions`
/// (absolute, each inside one entry); returns how many were dropped.
fn compact(d: &mut [u8], index: &Index, positions: &[usize]) -> usize {
    let mut data = Vec::with_capacity(index.bounds.last().copied().unwrap_or(0) - index.bounds[0]);
    let mut bounds = Vec::with_capacity(index.bounds.len());
    let mut cut = positions.iter().copied().peekable();
    let mut removed = 0;
    for i in 0..index.count() {
        bounds.push(data.len());
        let (s, e) = index.entry(i);
        let mut p = s;
        while p < e {
            if cut.peek() == Some(&p) && p + 2 <= e {
                cut.next();
                removed += 1;
                p += 2;
                continue;
            }
            data.push(d[p]);
            p += 1;
        }
    }
    bounds.push(data.len());
    if removed == 0 {
        return 0;
    }
    // Same offset size: every offset only shrinks.
    for (i, &b) in bounds.iter().enumerate() {
        let v = b + 1;
        let at = index.offsets_at + i * index.off_size;
        for k in 0..index.off_size {
            d[at + k] = (v >> (8 * (index.off_size - 1 - k))) as u8;
        }
    }
    let start = index.bounds[0];
    d[start..start + data.len()].copy_from_slice(&data);
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An INDEX of `entries` with a 2-byte offset size.
    fn index(entries: &[&[u8]]) -> Vec<u8> {
        let mut out = (entries.len() as u16).to_be_bytes().to_vec();
        if entries.is_empty() {
            return out;
        }
        out.push(2);
        let mut off = 1u16;
        out.extend_from_slice(&off.to_be_bytes());
        for e in entries {
            off += e.len() as u16;
            out.extend_from_slice(&off.to_be_bytes());
        }
        for e in entries {
            out.extend_from_slice(e);
        }
        out
    }

    /// A DICT integer operand (5-byte form, so offsets are fixed-size).
    fn int(v: i32) -> Vec<u8> {
        let mut out = vec![29];
        out.extend_from_slice(&v.to_be_bytes());
        out
    }

    /// A Type 2 charstring number.
    fn n(v: i32) -> Vec<u8> {
        if (-107..=107).contains(&v) {
            vec![(v + 139) as u8]
        } else {
            let mut out = vec![28];
            out.extend_from_slice(&(v as i16).to_be_bytes());
            out
        }
    }

    fn cs(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    /// A name-keyed CFF: `.notdef`, a period (`rmoveto <dot> hlineto…`)
    /// and an `i` whose dot comes through a local subroutine after a
    /// `hintmask`.
    fn font() -> Vec<u8> {
        font_with(&[12, 0])
    }

    fn font_with(dot: &[u8]) -> Vec<u8> {
        let notdef = cs(&[&[14]]);
        let period = cs(&[
            &n(100),
            &n(0),
            &n(106),
            &[1], // width hstem… (100 = width, 0 106 = the stem)
            &n(87),
            &n(104),
            &[3],
            &n(191),
            &n(106),
            &[21], // rmoveto
            dot,
            &n(-104),
            &n(-106),
            &n(104),
            &[6], // hlineto
            &[14],
        ]);
        let i = cs(&[
            &n(0),
            &n(100),
            &n(500),
            &n(100),
            &[18], // hstemhm: two stems
            &n(67),
            &n(88),
            &[19, 0b1110_0000], // hintmask with an implicit vstem (3 stems)
            &n(155),
            &n(400),
            &[21],
            &n(-88),
            &n(-400),
            &n(88),
            &[6],
            &n(-107), // local subr 0 (bias 107)
            &[10],
            &[14],
        ]);
        let dot_subr = cs(&[
            &n(500),
            &[4],
            &[12, 0],
            &n(100),
            &n(-88),
            &n(-100),
            &[7],
            &[11],
        ]);
        let charset = [0u8, 0, 15, 0, 74]; // format 0: period (SID 15), i (SID 74)

        // Layout: header, Name, Top DICT, String, GSubr, charset,
        // CharStrings, Private, Subrs.
        let header = [1u8, 0, 4, 4];
        let names = index(&[b"T"]);
        let strings = index(&[]);
        let gsubrs = index(&[]);
        let chars = index(&[&notdef, &period, &i]);
        let subrs = index(&[&dot_subr]);
        let private = cs(&[&int(6), &[19]]); // Subrs right after the 6-byte dict
        let top = |charset_at: usize, cs_at: usize, private_at: usize| {
            cs(&[
                &int(charset_at as i32),
                &[15],
                &int(cs_at as i32),
                &[17],
                &int(private.len() as i32),
                &int(private_at as i32),
                &[18],
            ])
        };
        let top_len = index(&[&top(0, 0, 0)]).len();
        let charset_at = header.len() + names.len() + top_len + strings.len() + gsubrs.len();
        let cs_at = charset_at + charset.len();
        let private_at = cs_at + chars.len();
        let top = top(charset_at, cs_at, private_at);
        let mut out = cs(&[
            &header,
            &names,
            &index(&[&top]),
            &strings,
            &gsubrs,
            &charset,
        ]);
        out.extend_from_slice(&chars);
        out.extend_from_slice(&private);
        out.extend_from_slice(&subrs);
        out
    }

    struct Bounds(f32, f32, f32, f32, usize);
    impl skrifa::font::glyph::outline::PathSink for Bounds {
        fn move_to(&mut self, x: f32, y: f32) {
            self.line_to(x, y);
        }
        fn line_to(&mut self, x: f32, y: f32) {
            self.0 = self.0.min(x);
            self.1 = self.1.min(y);
            self.2 = self.2.max(x);
            self.3 = self.3.max(y);
            self.4 += 1;
        }
        fn quad_to(&mut self, _: f32, _: f32, x: f32, y: f32) {
            self.line_to(x, y);
        }
        fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, x: f32, y: f32) {
            self.line_to(x, y);
        }
        fn close(&mut self) {}
    }

    fn outline(data: &[u8], gid: u16) -> (bool, Bounds) {
        use skrifa::font::glyph::outline::{DrawMemory, DrawOptions};
        let font = skrifa::font::Font::new(data.to_vec(), 0).expect("parses");
        let mut b = Bounds(f32::MAX, f32::MAX, f32::MIN, f32::MIN, 0);
        let ok = font
            .outline_glyphs()
            .get(skrifa::GlyphId::new(u32::from(gid)))
            .is_some_and(|g| {
                g.draw(&DrawOptions::new(), DrawMemory::Scratch, &mut b)
                    .is_ok()
            });
        (ok, b)
    }

    #[test]
    fn stripped_font_draws_the_dots() {
        let mut data = font();
        let len = data.len();
        assert_eq!(
            strip(&mut data),
            2,
            "one in the period, one in the subroutine"
        );
        assert_eq!(data.len(), len, "rewritten in place");
        let (ok, b) = outline(&data, 1);
        assert!(ok && b.4 >= 4, "period drawn");
        assert_eq!((b.0, b.1, b.2, b.3), (87.0, 0.0, 191.0, 106.0));
        let (ok, b) = outline(&data, 2);
        assert!(ok, "i drawn");
        // The dot from the subroutine: y 500 → 600.
        assert_eq!(b.3, 600.0);
        // Nothing left to strip; a font without the operator is untouched.
        let again = data.clone();
        assert_eq!(strip(&mut data), 0);
        assert_eq!(data, again);
    }

    #[test]
    fn dotsection_with_operands_is_kept() {
        // `5 dotsection` — deleting it would hand the 5 to the `hlineto`.
        let mut data = font_with(&[n(5)[0], 12, 0]);
        assert_eq!(strip(&mut data), 1, "only the subroutine's");
        assert_eq!(data.windows(2).filter(|w| *w == [12, 0]).count(), 1);
    }

    #[test]
    fn sfnt_cff_table_is_found() {
        let cff = font();
        let mut sfnt = b"OTTO".to_vec();
        sfnt.extend_from_slice(&1u16.to_be_bytes());
        sfnt.extend_from_slice(&[0; 6]);
        sfnt.extend_from_slice(b"CFF ");
        sfnt.extend_from_slice(&[0; 4]);
        sfnt.extend_from_slice(&28u32.to_be_bytes());
        sfnt.extend_from_slice(&(cff.len() as u32).to_be_bytes());
        sfnt.extend_from_slice(&cff);
        assert_eq!(strip_sfnt(&mut sfnt), 2);
        assert!(!sfnt[28..].windows(2).any(|w| w == [12, 0]));
    }

    #[test]
    fn garbage_is_left_alone() {
        let mut junk = vec![12u8, 0, 1, 2, 3, 4, 5];
        assert_eq!(strip(&mut junk), 0);
        assert_eq!(junk, [12, 0, 1, 2, 3, 4, 5]);
    }
}
