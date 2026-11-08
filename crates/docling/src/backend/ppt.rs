//! PPT (PowerPoint 97–2003 binary, [MS-PPT]) backend — issue #127.
//!
//! Native parsing, no external converter (docling proper shells out to
//! LibreOffice — `docling` PR #3804). The format is a CFB container whose
//! `PowerPoint Document` stream is a tree of tagged records; the drawing
//! layer inside each slide is OfficeArt ([`officeart`]).
//!
//! Slide content is assembled from two cooperating sources:
//! - the `SlideListWithText` (SLWT) inside the `DocumentContainer` holds the
//!   slide's *text blocks* (`TextHeaderAtom` + `TextCharsAtom`/`TextBytesAtom`
//!   per block, `SlidePersistAtom` opening each slide's group);
//! - each `Slide` container's OfficeArt drawing holds the *shapes* with their
//!   geometry (anchors) — a placeholder shape references its SLWT block by
//!   index (`OutlineTextRefAtom`), a plain textbox embeds its own text atoms.
//!
//! Shapes are walked with their anchors, SLWT references are resolved, and
//! items are emitted in docling's reading order (rows by top edge with a
//! 0.05" tolerance, left to right within a row — [`by_position`]). A
//! paragraph's bullet is its own `StyleTextPropAtom` flag when it sets one,
//! else the one its master's `TextMasterStyleAtom` gives its text type and
//! indent level — how PowerPoint stores an untouched body placeholder's
//! bullets, and how LibreOffice (docling's `.ppt` reader) resolves them
//! (#627).
//! A **group** of shapes whose child anchors tile a ≥2×≥2 grid is
//! reconstructed into a [`Node::Table`] — this is how legacy PPT stores
//! tables (a table *is* a shape group), so docling's PPTX table output has a
//! native equivalent here. SLWT blocks no shape consumed are appended after,
//! so text never goes missing on files that only fill the SLWT. Titles become
//! headings, other text paragraphs (lines split on `\r`); slides are
//! separated by page breaks, matching the PPTX backend's shape.

use docling_core::{DoclingDocument, Node, Table};

use crate::backend::cfb::CompoundFile;
use crate::backend::officeart::Records;
use crate::backend::pptx::{auto_number_marker, AutoScheme, MAX_AUTO_NUMBER_START};
use crate::backend::DeclarativeBackend;
use crate::error::ConversionError;
use crate::source::SourceDocument;

const RT_DOCUMENT: u16 = 0x03E8; // DocumentContainer
const RT_SLIDE: u16 = 0x03EE; // SlideContainer
const RT_SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0;
const RT_SLIDE_PERSIST_ATOM: u16 = 0x03F3;
const RT_TEXT_HEADER_ATOM: u16 = 0x0F9F;
const RT_OUTLINE_TEXT_REF_ATOM: u16 = 0x0F9E;
const RT_TEXT_CHARS_ATOM: u16 = 0x0FA0;
const RT_TEXT_BYTES_ATOM: u16 = 0x0FA8;
const RT_STYLE_TEXT_PROP_ATOM: u16 = 0x0FA1;
const RT_STYLE_TEXT_PROP9_ATOM: u16 = 0x0FAC;
const RT_BINARY_TAG_DATA: u16 = 0x138B;
const RT_MAIN_MASTER: u16 = 0x03F8;
const RT_SLIDE_ATOM: u16 = 0x03EF;
const RT_TEXT_MASTER_STYLE_ATOM: u16 = 0x0FA3;
const OA_CLIENT_DATA: u16 = 0xF011;

// OfficeArt ([MS-ODRAW]) record types.
const OA_DG_CONTAINER: u16 = 0xF002;
const OA_SPGR_CONTAINER: u16 = 0xF003;
const OA_SP_CONTAINER: u16 = 0xF004;
const OA_FSPGR: u16 = 0xF009;
const OA_CHILD_ANCHOR: u16 = 0xF00F;
const OA_CLIENT_ANCHOR: u16 = 0xF010;
const OA_CLIENT_TEXTBOX: u16 = 0xF00D;

/// Text-run types (TextHeaderAtom): 0 = title, 6 = centered title.
const TX_TITLE: u32 = 0;
const TX_CENTER_TITLE: u32 = 6;

pub struct PptBackend;

impl DeclarativeBackend for PptBackend {
    fn convert(&self, source: &SourceDocument) -> Result<DoclingDocument, ConversionError> {
        let cfb = CompoundFile::open(&source.bytes).ok_or_else(|| {
            ConversionError::Parse(CompoundFile::open_error("ppt", &source.bytes))
        })?;
        let stream = cfb.stream("PowerPoint Document").ok_or_else(|| {
            ConversionError::Parse(cfb.stream_error("ppt", "PowerPoint Document"))
        })?;
        // #624: an encrypted presentation used to convert to an empty
        // document — its records are ciphertext, so the walk below found no
        // slide. `EncryptedSummary` alone never fired on PowerPoint's own
        // files; the reliable marker is the current edit's reference to a
        // `CryptSession10Container`.
        // `EncryptedSummary` decides only when there is no edit chain to read:
        // a decrypted file (#625) keeps the stream, but no longer the
        // reference.
        let current_user = cfb.stream("Current User").unwrap_or_default();
        let edits = UserEdits::read(&current_user, &stream);
        let encrypted = match &edits {
            Some(edits) => edits.encrypted(),
            None => cfb.stream("EncryptedSummary").is_some(),
        };
        if encrypted {
            return Err(crate::backend::offcrypto::encrypted("ppt"));
        }

        // SLWT text blocks per slide, in presentation order.
        let mut slwt: Vec<Vec<TextBlock>> = Vec::new();
        for (header, body) in Records::new(&stream) {
            if header.rec_type != RT_DOCUMENT {
                continue;
            }
            for (h2, b2) in Records::new(body) {
                // instance 0 = the slide list (1 = masters, 2 = notes).
                if h2.rec_type == RT_SLIDE_LIST_WITH_TEXT && h2.instance == 0 {
                    collect_slwt_slides(b2, &mut slwt);
                }
            }
        }

        // Shape items per slide, from each Slide container's drawing, with
        // the text styles of the master the slide follows.
        let masters = master_styles(&stream, edits.as_ref());
        let fallback = Records::new(&stream)
            .find(|(h, _)| h.rec_type == RT_MAIN_MASTER)
            .map(|(_, body)| MasterStyles::read(body))
            .unwrap_or_default();
        let slide_shapes: Vec<(Vec<ShapeItem>, &MasterStyles)> = Records::new(&stream)
            .filter(|(h, _)| h.rec_type == RT_SLIDE)
            .map(|(_, body)| {
                let master = master_id_ref(body)
                    .and_then(|id| masters.get(&id))
                    .unwrap_or(&fallback);
                (slide_items(body), master)
            })
            .collect();

        let n = slwt.len().max(slide_shapes.len());
        let mut doc = DoclingDocument::new(&source.name);
        let mut first = true;
        for i in 0..n {
            let blocks = slwt.get(i).cloned().unwrap_or_default();
            let (shapes, master) = slide_shapes
                .get(i)
                .map_or((Vec::new(), &fallback), |(s, m)| (s.clone(), *m));
            let nodes = assemble_slide(blocks, shapes, master);
            if nodes.is_empty() {
                continue;
            }
            if !first {
                doc.push(Node::PageBreak);
            }
            first = false;
            for node in nodes {
                doc.push(node);
            }
        }
        Ok(doc)
    }
}

/// A slide's (or title master's) `SlideAtom.masterIdRef` — the master's
/// slide id ([MS-PPT] 2.4.3).
fn master_id_ref(slide_body: &[u8]) -> Option<u32> {
    Records::new(slide_body)
        .find(|(h, _)| h.rec_type == RT_SLIDE_ATOM)
        .and_then(|(_, b)| Some(u32::from_le_bytes(b.get(12..16)?.try_into().ok()?)))
}

/// The text styles of every master, by master id: the master list
/// (`SlideListWithText` instance 1) names each master's persist object and
/// id, and the persist directory says where it lives. PowerPoint 2007 and
/// later write each slide layout as a master of its own, so slides of one
/// deck follow different ones. A title master (a `SlideContainer` in the
/// list) carries no text styles of its own and takes its main master's.
fn master_styles(
    stream: &[u8],
    edits: Option<&UserEdits>,
) -> std::collections::HashMap<u32, MasterStyles> {
    let mut styles = std::collections::HashMap::new();
    let Some(edits) = edits else {
        return styles;
    };
    let mut title_masters = Vec::new();
    for (header, body) in Records::new(stream) {
        if header.rec_type != RT_DOCUMENT {
            continue;
        }
        for (h, list) in Records::new(body) {
            if h.rec_type != RT_SLIDE_LIST_WITH_TEXT || h.instance != 1 {
                continue;
            }
            for (h2, atom) in Records::new(list) {
                if h2.rec_type != RT_SLIDE_PERSIST_ATOM || atom.len() < 16 {
                    continue;
                }
                let persist = u32::from_le_bytes(atom[0..4].try_into().unwrap());
                let id = u32::from_le_bytes(atom[12..16].try_into().unwrap());
                let Some(&off) = edits.persist.get(&persist) else {
                    continue;
                };
                let Some((h3, master)) = stream
                    .get(off as usize..)
                    .and_then(|d| Records::new(d).next())
                else {
                    continue;
                };
                match h3.rec_type {
                    RT_MAIN_MASTER => {
                        styles.insert(id, MasterStyles::read(master));
                    }
                    RT_SLIDE => title_masters.extend(master_id_ref(master).map(|m| (id, m))),
                    _ => {}
                }
            }
        }
    }
    for (id, main) in title_masters {
        if let Some(main) = styles.get(&main).cloned() {
            styles.insert(id, main);
        }
    }
    styles
}

const RT_USER_EDIT_ATOM: u16 = 0x0FF5;
const RT_PERSIST_DIRECTORY_ATOM: u16 = 0x1772;
const RT_CRYPT_SESSION10_CONTAINER: u16 = 0x2F14;

/// The user-edit chain of a `PowerPoint Document` stream ([MS-PPT] 2.3.2,
/// 2.3.4): the `Current User` stream's `CurrentUserAtom` names the newest
/// `UserEditAtom`, each edit points to the one before it and to its
/// `PersistDirectoryAtom`, and the persist directory maps persist object ids
/// to stream offsets — the newest edit's mapping of an id wins. None of
/// these records is encrypted in an encrypted file (2.3.7), which is what
/// makes them the place to look for the encryption.
pub(crate) struct UserEdits {
    /// Persist object id → offset in the stream, the newest edit's wins.
    persist: std::collections::HashMap<u32, u32>,
    /// Every edit's (persist id, offset) pairs, superseded ones included —
    /// decryption (#625) must reach every object the stream holds.
    pub(crate) objects: Vec<(u32, u32)>,
    /// The newest edit's `encryptSessionPersistIdRef` — present only when
    /// that `UserEditAtom` is 0x20 bytes long, i.e. the file is encrypted.
    encrypt_ref: Option<u32>,
    /// Where that field sits in the stream.
    encrypt_ref_at: Option<usize>,
}

impl UserEdits {
    pub(crate) fn read(current_user: &[u8], doc: &[u8]) -> Option<Self> {
        let u32_at =
            |d: &[u8], o: usize| Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?));
        // CurrentUserAtom: 8-byte header, size, headerToken, offsetToCurrentEdit.
        let mut off = u32_at(current_user, 16)? as usize;
        let mut edits = Self {
            persist: std::collections::HashMap::new(),
            objects: Vec::new(),
            encrypt_ref: None,
            encrypt_ref_at: None,
        };
        // Each edit lies before the previous one's offset in a well-formed
        // file; the hop count bounds a cyclic chain either way.
        for hop in 0..doc.len() / 40 + 1 {
            let (header, body) = Records::new(doc.get(off..)?).next()?;
            if header.rec_type != RT_USER_EDIT_ATOM || body.len() < 28 {
                return (hop > 0).then_some(edits);
            }
            if hop == 0 && body.len() >= 32 {
                edits.encrypt_ref = u32_at(body, 28);
                edits.encrypt_ref_at = Some(off + 8 + 28);
            }
            let dir_off = u32_at(body, 12)? as usize;
            if let Some((h, dir)) = doc.get(dir_off..).and_then(|d| Records::new(d).next()) {
                if h.rec_type == RT_PERSIST_DIRECTORY_ATOM {
                    // PersistDirectoryEntry: persistId (20 bits) + cPersist
                    // (12 bits), then cPersist offsets for consecutive ids.
                    let mut p = 0;
                    while let Some(v) = u32_at(dir, p) {
                        let (id, count) = (v & 0xF_FFFF, (v >> 20) as usize);
                        for k in 0..count {
                            let Some(o) = u32_at(dir, p + 4 + k * 4) else {
                                break;
                            };
                            edits.persist.entry(id + k as u32).or_insert(o);
                            edits.objects.push((id + k as u32, o));
                        }
                        p += 4 + count * 4;
                    }
                }
            }
            match u32_at(body, 8)? as usize {
                0 => break,
                prev => off = prev,
            }
        }
        Some(edits)
    }

    /// The `CryptSession10Container` body the newest edit references — the
    /// file's `EncryptionInfo` ([MS-PPT] 2.3.7).
    pub(crate) fn crypt_session<'a>(&self, doc: &'a [u8]) -> Option<&'a [u8]> {
        let off = *self.persist.get(&self.encrypt_ref?)? as usize;
        let (header, body) = Records::new(doc.get(off..)?).next()?;
        (header.rec_type == RT_CRYPT_SESSION10_CONTAINER).then_some(body)
    }

    /// The stream offset of the `CryptSession10Container` (left in the
    /// clear by encryption, so decryption skips it).
    pub(crate) fn session_offset(&self) -> Option<usize> {
        self.persist.get(&self.encrypt_ref?).map(|&o| o as usize)
    }

    /// Where the newest edit's `encryptSessionPersistIdRef` sits in the
    /// stream — decryption zeroes it, after which the file reads as plain.
    pub(crate) fn encrypt_ref_offset(&self) -> Option<usize> {
        self.encrypt_ref_at
    }

    /// Whether the presentation is encrypted: the newest edit references an
    /// encryption session. A reference the persist directory cannot resolve
    /// still counts — no unencrypted writer sets one.
    pub(crate) fn encrypted(&self) -> bool {
        self.encrypt_ref.is_some_and(|r| r != 0)
    }
}

/// One SLWT text block: title flag + raw text (runs already concatenated).
#[derive(Clone, Default)]
struct TextBlock {
    is_title: bool,
    /// The `TextHeaderAtom` text type (`Tx_TYPE_*`), which picks the master
    /// text style the block inherits from.
    text_type: u32,
    text: String,
    styles: Vec<ParaStyle>,
    consumed: bool,
}

/// One paragraph's list-relevant style, from the `StyleTextPropAtom`
/// paragraph runs (bullet flag, indent level) merged with the PP9
/// `StyleTextProp9Atom` autonumber extension (numbered lists).
#[derive(Clone, Copy, Default)]
struct ParaStyle {
    /// Characters covered by this run (paragraph text + terminator).
    count: usize,
    indent: u8,
    /// The paragraph's own `fHasBullet`, when its `hasBullet` mask is set;
    /// `None` inherits it from the master text style.
    bullet: Option<bool>,
    /// `(scheme, start)` when the paragraph auto-numbers (PP9).
    autonum: Option<(u16, u16)>,
}

/// Parse a `StyleTextPropAtom` body's paragraph-level runs. Each run is
/// `{count u32, indentLevel u16, TextPFException}`; the exception's optional
/// fields are sized by its masks ([MS-PPT] 2.9.31), which this walks exactly
/// so the next run starts at the right offset. Returns runs until `text_len`
/// is covered (the last run also covers the block terminator).
fn parse_para_styles(body: &[u8], text_len: usize) -> Vec<ParaStyle> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut covered = 0usize;
    while covered <= text_len && pos + 10 <= body.len() {
        let count =
            u32::from_le_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]) as usize;
        let indent = u16::from_le_bytes([body[pos + 4], body[pos + 5]]);
        let Some((next, bullet)) = pf_exception(body, pos + 6) else {
            break;
        };
        pos = next;
        covered += count;
        out.push(ParaStyle {
            count,
            indent: indent.min(u8::MAX as u16) as u8,
            bullet,
            autonum: None,
        });
    }
    out
}

/// Walk one `TextPFException` ([MS-PPT] 2.9.18) at `pos`: its optional fields
/// are sized by its masks, walked exactly so whatever follows starts at the
/// right offset. Returns the offset past it and the `fHasBullet` it sets —
/// only when the `hasBullet` mask bit says so (the other three bullet-flag
/// bits share the field without setting this one), as LibreOffice's
/// `PPTParaSheet::Read` applies the masked bits alone.
fn pf_exception(body: &[u8], mut pos: usize) -> Option<(usize, Option<bool>)> {
    let u16_at = |p: usize| Some(u16::from_le_bytes(body.get(p..p + 2)?.try_into().ok()?));
    let masks = u32::from_le_bytes(body.get(pos..pos + 4)?.try_into().ok()?);
    pos += 4;
    let mut bullet = None;
    // bulletFlags: present when any of hasBullet/font/color/size masks set.
    if masks & 0x0000_000F != 0 {
        let flags = u16_at(pos)?;
        if masks & 0x0000_0001 != 0 {
            bullet = Some(flags & 0x01 != 0);
        }
        pos += 2;
    }
    // Remaining optional fields, in on-disk order, sized per masks.
    for (bit, size) in [
        (0x0000_0080u32, 2usize), // bulletChar
        (0x0000_0010, 2),         // bulletFontRef
        (0x0000_0040, 2),         // bulletSize
        (0x0000_0020, 4),         // bulletColor
        (0x0000_0800, 2),         // textAlignment
        (0x0000_1000, 2),         // lineSpacing
        (0x0000_2000, 2),         // spaceBefore
        (0x0000_4000, 2),         // spaceAfter
        (0x0000_0100, 2),         // leftMargin
        (0x0000_0400, 2),         // indent
        (0x0000_8000, 2),         // defaultTabSize
    ] {
        if masks & bit != 0 {
            pos += size;
        }
    }
    if masks & 0x0010_0000 != 0 {
        // tabStops: count u16 + count × 4 bytes.
        pos += 2 + u16_at(pos)? as usize * 4;
    }
    for (bit, size) in [
        (0x0001_0000u32, 2usize), // fontAlign
        (0x000E_0000, 2),         // wrap flags (one field for the three bits)
        (0x0020_0000, 2),         // textDirection
    ] {
        if masks & bit != 0 {
            pos += size;
        }
    }
    (pos <= body.len()).then_some((pos, bullet))
}

/// Walk one `TextCFException` ([MS-PPT] 2.9.13) at `pos`, returning the
/// offset past it: the character half of a master style level, skipped to
/// reach the next level's paragraph properties. `fontStyle` is read for any
/// of the low 16 mask bits, as LibreOffice does.
fn cf_exception(body: &[u8], mut pos: usize) -> Option<usize> {
    let masks = u32::from_le_bytes(body.get(pos..pos + 4)?.try_into().ok()?);
    pos += 4;
    if masks & 0x0000_FFFF != 0 {
        pos += 2; // fontStyle
    }
    for (bit, size) in [
        (0x0001_0000u32, 2usize), // fontRef
        (0x0020_0000, 2),         // oldEAFontRef
        (0x0040_0000, 2),         // ansiFontRef
        (0x0080_0000, 2),         // symbolFontRef
        (0x0002_0000, 2),         // fontSize
        (0x0004_0000, 4),         // color
        (0x0008_0000, 2),         // position
        (0x0010_0000, 4),         // pp10runid + unused
        (0x0100_0000, 2),         // newEAFontRef
        (0x0200_0000, 2),         // csFontRef
        (0x0400_0000, 4),         // pp11ext
    ] {
        if masks & bit != 0 {
            pos += size;
        }
    }
    (pos <= body.len()).then_some(pos)
}

/// The bullet flags a master's `TextMasterStyleAtom`s declare
/// ([MS-PPT] 2.9.36): per text type (the atom's instance, `Tx_TYPE_*`), the
/// `fHasBullet` each indent level sets, `None` where the level leaves it to
/// inheritance.
#[derive(Clone, Default)]
struct MasterStyles {
    types: std::collections::HashMap<u32, [Option<bool>; 5]>,
}

impl MasterStyles {
    /// Read every `TextMasterStyleAtom` directly inside a `MainMaster` body.
    fn read(master: &[u8]) -> Self {
        let mut styles = Self::default();
        for (h, b) in Records::new(master) {
            if h.rec_type != RT_TEXT_MASTER_STYLE_ATOM {
                continue;
            }
            let inst = h.instance as u32;
            let mut levels = [None; 5];
            let Some(n) = b.get(..2).map(|x| u16::from_le_bytes([x[0], x[1]])) else {
                continue;
            };
            let mut pos = 2;
            for i in 0..n.min(5) as usize {
                // The center/half/quarter types number their levels.
                let level = if inst >= 5 {
                    let Some(l) = b.get(pos..pos + 2) else { break };
                    pos += 2;
                    u16::from_le_bytes([l[0], l[1]]) as usize
                } else {
                    i
                };
                let Some((next, bullet)) = pf_exception(b, pos) else {
                    break;
                };
                let Some(next) = cf_exception(b, next) else {
                    break;
                };
                pos = next;
                if let Some(slot) = levels.get_mut(level) {
                    *slot = bullet;
                }
            }
            styles.types.insert(inst, levels);
        }
        styles
    }

    /// Whether a paragraph of `text_type` at `indent` inherits a bullet —
    /// LibreOffice's `PPTStyleSheet` resolution, which is what docling reads
    /// a `.ppt` through: a level the style leaves unset takes the level below
    /// it; the center-body/half-body/quarter-body types start from Body's
    /// resolved levels and the center title from Title's.
    fn bullet(&self, text_type: u32, indent: u8) -> bool {
        let parent = match text_type {
            5 | 7 | 8 => Some(1),
            6 => Some(0),
            _ => None,
        };
        let level = (indent as usize).min(4);
        let own = self.types.get(&text_type);
        match parent {
            Some(base) => own
                .and_then(|l| l[level])
                .unwrap_or_else(|| self.bullet(base, indent)),
            None => own
                .and_then(|l| l[..=level].iter().rev().find_map(|b| *b))
                .unwrap_or(false),
        }
    }
}

/// Parse a PP9 `StyleTextProp9Atom` (inside the shape's `___PPT9` binary tag):
/// per paragraph `{TextPFException9, TextCFException9, TextSIException}`.
/// Only the autonumber fields are read; a non-empty exception this parser
/// doesn't model ends the walk (styles parsed so far still apply).
fn parse_para_styles9(body: &[u8]) -> Vec<Option<(u16, u16)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= body.len() {
        let masks = u32::from_le_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        let mut has_auto = false;
        let mut scheme_start = None;
        if masks & 0x0080_0000 != 0 {
            pos += 2; // bulletBlipRef
        }
        if masks & 0x0100_0000 != 0 {
            has_auto = body.get(pos).is_some_and(|&b| b != 0);
            pos += 2; // fBulletHasAutoNumber
        }
        if masks & 0x0200_0000 != 0 {
            if pos + 4 > body.len() {
                break;
            }
            let scheme = u16::from_le_bytes([body[pos], body[pos + 1]]);
            let start = u16::from_le_bytes([body[pos + 2], body[pos + 3]]);
            // docling#5122 (#1018): a start outside `ST_TextBulletStartAtNum`'s
            // 1..=32767 is ignored — the `startAt` LibreOffice writes for
            // docling's `.ppt` → `.pptx` route is capped there.
            let start = if (1..=MAX_AUTO_NUMBER_START).contains(&u64::from(start)) {
                start
            } else {
                1
            };
            scheme_start = Some((scheme, start));
            pos += 4;
        }
        if masks & !0x0380_0000 != 0 {
            // A PF9 field this parser doesn't size — stop cleanly.
            break;
        }
        // TextCFException9 + TextSIException: only the all-empty form is
        // modeled; anything else ends the walk.
        let Some(cf) = body.get(pos..pos + 4) else {
            break;
        };
        if cf != [0, 0, 0, 0] {
            break;
        }
        pos += 4;
        let Some(si) = body.get(pos..pos + 4) else {
            break;
        };
        if si != [0, 0, 0, 0] {
            break;
        }
        pos += 4;
        out.push(
            (has_auto || scheme_start.is_some())
                .then_some(scheme_start)
                .flatten(),
        );
    }
    out
}

/// Walk a SlideListWithText body: `SlidePersistAtom` starts a new slide, a
/// `TextHeaderAtom` starts a new text block, text atoms append to it.
fn collect_slwt_slides(body: &[u8], slides: &mut Vec<Vec<TextBlock>>) {
    for (h, b) in Records::new(body) {
        match h.rec_type {
            RT_SLIDE_PERSIST_ATOM => slides.push(Vec::new()),
            RT_TEXT_HEADER_ATOM => {
                let tx = b
                    .get(..4)
                    .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                    .unwrap_or(u32::MAX);
                if let Some(slide) = slides.last_mut() {
                    slide.push(TextBlock {
                        is_title: tx == TX_TITLE || tx == TX_CENTER_TITLE,
                        text_type: tx,
                        ..TextBlock::default()
                    });
                }
            }
            RT_TEXT_CHARS_ATOM => {
                if let Some(block) = slides.last_mut().and_then(|s| s.last_mut()) {
                    block.text.push_str(&utf16_text(b));
                }
            }
            RT_TEXT_BYTES_ATOM => {
                if let Some(block) = slides.last_mut().and_then(|s| s.last_mut()) {
                    block.text.push_str(&bytes_text(b));
                }
            }
            RT_STYLE_TEXT_PROP_ATOM => {
                if let Some(block) = slides.last_mut().and_then(|s| s.last_mut()) {
                    let len = block.text.chars().count();
                    block.styles.extend(parse_para_styles(b, len));
                }
            }
            _ => {}
        }
    }
}

/// The text carried by one shape: embedded atoms, or a reference to the
/// slide's SLWT block by index.
#[derive(Clone, Default)]
struct ShapeText {
    is_title: bool,
    text_type: u32,
    text: String,
    styles: Vec<ParaStyle>,
    outline_ref: Option<u32>,
}

/// A shape's anchor rectangle: `(left, top, right, bottom)`.
type Anchor = (i32, i32, i32, i32);

/// One item discovered in a slide's drawing, in encounter order.
#[derive(Clone)]
enum ShapeItem {
    Text {
        anchor: Option<Anchor>,
        text: ShapeText,
    },
    Table {
        table: Table,
    },
}

/// Extract a slide's drawing items: find the OfficeArtDgContainer, walk its
/// root group's children — plain shapes become text items, nested groups are
/// tried as tables (a legacy PPT table *is* a group whose child anchors tile
/// a grid) and otherwise flattened — in reading order ([`by_position`]).
fn slide_items(slide_body: &[u8]) -> Vec<ShapeItem> {
    let Some(dg) = find_container(slide_body, OA_DG_CONTAINER, 0) else {
        return Vec::new();
    };
    let mut units = Vec::new();
    for (h, b) in Records::new(dg) {
        if h.rec_type == OA_SPGR_CONTAINER {
            // Root group: first SpContainer is the canvas frame (FSPGR).
            for (h2, b2) in Records::new(b) {
                match h2.rec_type {
                    OA_SP_CONTAINER if !has_record(b2, OA_FSPGR) => {
                        if let Some(item) = shape_item(b2) {
                            units.push((shape_anchor(b2), vec![item]));
                        }
                    }
                    OA_SPGR_CONTAINER => {
                        let mut items = Vec::new();
                        group_items(b2, &mut items, 0);
                        units.push((group_anchor(b2), items));
                    }
                    _ => {}
                }
            }
        }
    }
    by_position(units)
}

/// A group's own anchor: that of its frame, the first `SpContainer`
/// (the one holding the `FSPGR`).
fn group_anchor(group_body: &[u8]) -> Option<Anchor> {
    Records::new(group_body)
        .find(|(h, b)| h.rec_type == OA_SP_CONTAINER && has_record(b, OA_FSPGR))
        .and_then(|(_, b)| shape_anchor(b))
}

/// Shapes (each with the items it yields) in visual reading order — docling's
/// `_iter_shapes_by_position` (docling#3393), which a `.ppt` reaches through
/// LibreOffice's PPTX export with the geometry intact: sort by top edge, start
/// a new row when a top is more than 0.05" below the previous one, and read
/// each row left to right; shapes without an anchor go last, in drawing
/// order. A "Section Header" slide puts its body above the title, so docling
/// reads the body first (#627).
fn by_position(units: Vec<(Option<Anchor>, Vec<ShapeItem>)>) -> Vec<ShapeItem> {
    // 0.05" in master units (576 per inch): 28.8, compared in tenths.
    const ROW_TOLERANCE_TENTHS: i64 = 288;
    let mut keyed: Vec<(i64, i64, usize, Vec<ShapeItem>)> = units
        .into_iter()
        .enumerate()
        .map(|(index, (anchor, items))| {
            let (left, top) =
                anchor.map_or((i64::MAX, i64::MAX), |(l, t, _, _)| (l as i64, t as i64));
            (top, left, index, items)
        })
        .collect();
    keyed.sort_by_key(|&(top, _, index, _)| (top, index));
    let mut out = Vec::new();
    let mut row: Vec<(i64, usize, Vec<ShapeItem>)> = Vec::new();
    let mut prev_top: Option<i64> = None;
    let flush = |row: &mut Vec<(i64, usize, Vec<ShapeItem>)>, out: &mut Vec<ShapeItem>| {
        row.sort_by_key(|&(left, index, _)| (left, index));
        out.extend(row.drain(..).flat_map(|(_, _, items)| items));
    };
    for (top, left, index, items) in keyed {
        if prev_top.is_some_and(|p| top.saturating_sub(p).saturating_mul(10) > ROW_TOLERANCE_TENTHS)
        {
            flush(&mut row, &mut out);
        }
        prev_top = Some(top);
        row.push((left, index, items));
    }
    flush(&mut row, &mut out);
    out
}

/// Handle one group container: reconstruct a table from the child grid, else
/// flatten the children as ordinary items in reading order (recursing into
/// nested groups).
fn group_items(group_body: &[u8], out: &mut Vec<ShapeItem>, depth: usize) {
    if depth > 16 {
        return;
    }
    let mut cells: Vec<(Anchor, ShapeText)> = Vec::new();
    let mut children = Vec::new();
    for (h, b) in Records::new(group_body) {
        match h.rec_type {
            OA_SP_CONTAINER => {
                if has_record(b, OA_FSPGR) {
                    // The group frame: geometry container only, not a cell.
                    continue;
                }
                if let Some(ShapeItem::Text { anchor, text }) = shape_item(b) {
                    // Border/line shapes are degenerate rectangles; they are
                    // not cells (they'd mint phantom rows/columns).
                    if let Some((l, t, r, b)) = anchor {
                        if (r - l).abs() > 1 && (b - t).abs() > 1 {
                            cells.push(((l, t, r, b), text.clone()));
                        }
                    }
                    children.push((anchor, vec![ShapeItem::Text { anchor, text }]));
                }
            }
            OA_SPGR_CONTAINER => {
                let mut items = Vec::new();
                group_items(b, &mut items, depth + 1);
                children.push((group_anchor(b), items));
            }
            _ => {}
        }
    }
    if let Some(table) = grid_table(&cells) {
        out.push(ShapeItem::Table { table });
    } else {
        out.extend(by_position(children));
    }
}

/// Parse one SpContainer into a text item (anchor + text/outline reference).
fn shape_item(sp_body: &[u8]) -> Option<ShapeItem> {
    let anchor = shape_anchor(sp_body);
    let mut text = ShapeText::default();
    let mut autonums: Vec<Option<(u16, u16)>> = Vec::new();
    for (h, b) in Records::new(sp_body) {
        match h.rec_type {
            OA_CLIENT_TEXTBOX => {
                for (h2, b2) in Records::new(b) {
                    match h2.rec_type {
                        RT_TEXT_HEADER_ATOM => {
                            let tx = b2
                                .get(..4)
                                .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                                .unwrap_or(u32::MAX);
                            text.is_title = tx == TX_TITLE || tx == TX_CENTER_TITLE;
                            text.text_type = tx;
                        }
                        RT_OUTLINE_TEXT_REF_ATOM => {
                            text.outline_ref = b2
                                .get(..4)
                                .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]));
                        }
                        RT_TEXT_CHARS_ATOM => text.text.push_str(&utf16_text(b2)),
                        RT_TEXT_BYTES_ATOM => text.text.push_str(&bytes_text(b2)),
                        RT_STYLE_TEXT_PROP_ATOM => {
                            let len = text.text.chars().count();
                            text.styles.extend(parse_para_styles(b2, len));
                        }
                        _ => {}
                    }
                }
            }
            // The PP9 extension rides in the shape's client data as a
            // `___PPT9` binary tag holding a StyleTextProp9Atom: the
            // per-paragraph autonumber (numbered list) info.
            OA_CLIENT_DATA => {
                if let Some(blob) = find_container(b, RT_BINARY_TAG_DATA, 0) {
                    for (h3, b3) in Records::new(blob) {
                        if h3.rec_type == RT_STYLE_TEXT_PROP9_ATOM {
                            autonums = parse_para_styles9(b3);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for (style, auto) in text.styles.iter_mut().zip(autonums) {
        style.autonum = auto;
    }
    Some(ShapeItem::Text { anchor, text })
}

/// A shape's anchor: the child anchor (within a group, 4×i32) or the PPT
/// client anchor (on the slide, 4×i16 as top/left/right/bottom, or 4×i32).
fn shape_anchor(sp_body: &[u8]) -> Option<Anchor> {
    for (h, b) in Records::new(sp_body) {
        match h.rec_type {
            OA_CHILD_ANCHOR if b.len() >= 16 => {
                let v: Vec<i32> = b[..16]
                    .chunks_exact(4)
                    .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                return Some((v[0], v[1], v[2], v[3])); // l, t, r, b
            }
            OA_CLIENT_ANCHOR if b.len() >= 8 => {
                if b.len() >= 16 {
                    let v: Vec<i32> = b[..16]
                        .chunks_exact(4)
                        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    return Some((v[1], v[0], v[3], v[2])); // t,l,r,b → l,t,r,b
                }
                let v: Vec<i32> = b[..8]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
                    .collect();
                return Some((v[1], v[0], v[3], v[2])); // t,l,r,b → l,t,r,b
            }
            _ => {}
        }
    }
    None
}

/// Reconstruct a table from cell shapes when their anchors tile a grid:
/// cluster the child anchors' left/top edges into column/row boundaries and
/// require at least a 2×2 grid with most positions covered. Merged cells span
/// the boundaries they cross (OTSL-style continuations, like the XLSX/DOCX
/// backends emit).
fn grid_table(cells: &[(Anchor, ShapeText)]) -> Option<Table> {
    if cells.len() < 4 {
        return None;
    }
    let tolerance = edge_tolerance(cells);
    let col_edges = cluster(cells.iter().map(|((l, ..), _)| *l), tolerance);
    let row_edges = cluster(cells.iter().map(|((_, t, ..), _)| *t), tolerance);
    let (nrows, ncols) = (row_edges.len(), col_edges.len());
    if nrows < 2 || ncols < 2 {
        return None;
    }
    // Most grid positions must be covered for this to read as a table.
    if cells.len() * 10 < nrows * ncols * 6 {
        return None;
    }

    let index_of = |edges: &[i32], v: i32| -> usize {
        edges
            .iter()
            .position(|&e| (v - e).abs() <= tolerance)
            .unwrap_or_else(|| edges.iter().filter(|&&e| e < v).count().saturating_sub(1))
    };
    let mut grid: Vec<Vec<Option<String>>> = vec![vec![None; ncols]; nrows];
    let mut col_cont = vec![vec![false; ncols]; nrows];
    let mut row_cont = vec![vec![false; ncols]; nrows];
    for ((l, t, r, b), text) in cells {
        let ci = index_of(&col_edges, *l);
        let ri = index_of(&row_edges, *t);
        // The span covers every further boundary strictly inside (l, r)/(t, b).
        let col_span = 1 + col_edges[ci + 1..]
            .iter()
            .take_while(|&&e| e < *r - tolerance)
            .count();
        let row_span = 1 + row_edges[ri + 1..]
            .iter()
            .take_while(|&&e| e < *b - tolerance)
            .count();
        let value = text.text.replace('\r', "\n").trim().to_string();
        for rr in ri..(ri + row_span).min(nrows) {
            for cc in ci..(ci + col_span).min(ncols) {
                let cell = grid.get_mut(rr).and_then(|row| row.get_mut(cc))?;
                if cell.is_none() {
                    *cell = Some(value.clone());
                }
                if (rr, cc) == (ri, ci) {
                    continue;
                }
                if cc > ci {
                    col_cont[rr][cc] = true;
                }
                if rr > ri && cc == ci {
                    row_cont[rr][cc] = true;
                }
                if rr > ri && cc > ci {
                    row_cont[rr][cc] = true;
                }
            }
        }
    }
    let rows: Vec<Vec<String>> = grid
        .into_iter()
        .map(|row| row.into_iter().map(Option::unwrap_or_default).collect())
        .collect();
    let any_span = col_cont
        .iter()
        .flatten()
        .chain(row_cont.iter().flatten())
        .any(|&x| x);
    let structure = any_span.then(|| {
        let mut header_row = vec![false; nrows];
        if let Some(h) = header_row.first_mut() {
            *h = true;
        }
        docling_core::TableStructure {
            header_row,
            col_continuation: col_cont,
            row_continuation: row_cont,
            row_header: Vec::new(),
            col_header: Vec::new(),
        }
    });
    Some(Table {
        footnotes: Vec::new(),
        rows,
        location: None,
        structure,
        cell_blocks: None,
        cells: None,
        caption: None,
        caption_parent: Default::default(),
        caption_location: None,
    })
}

/// Cluster tolerance scaled from the cells' typical size, so the grid check
/// works whatever coordinate space the anchors use.
fn edge_tolerance(cells: &[(Anchor, ShapeText)]) -> i32 {
    let avg_w: i32 = cells
        .iter()
        .map(|((l, _, r, _), _)| (r - l).abs())
        .sum::<i32>()
        / cells.len().max(1) as i32;
    (avg_w / 8).max(2)
}

/// Sort + merge values within `tolerance` into representative edges.
fn cluster(values: impl Iterator<Item = i32>, tolerance: i32) -> Vec<i32> {
    let mut v: Vec<i32> = values.collect();
    v.sort_unstable();
    let mut out: Vec<i32> = Vec::new();
    for x in v {
        match out.last() {
            Some(&last) if (x - last).abs() <= tolerance => {}
            _ => out.push(x),
        }
    }
    out
}

/// `true` if the record tree body directly contains a record of `rec_type`.
fn has_record(body: &[u8], rec_type: u16) -> bool {
    Records::new(body).any(|(h, _)| h.rec_type == rec_type)
}

/// Depth-first search for the first container of `rec_type`.
fn find_container(body: &[u8], rec_type: u16, depth: usize) -> Option<&[u8]> {
    if depth > 16 {
        return None;
    }
    for (h, b) in Records::new(body) {
        if h.rec_type == rec_type {
            return Some(b);
        }
        if h.version == 0xF {
            if let Some(found) = find_container(b, rec_type, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Merge one slide's SLWT blocks and drawing shapes into nodes: shapes emit
/// in geometric order (resolving outline references into the blocks), then
/// any block no shape consumed is appended, so nothing is lost.
fn assemble_slide(
    mut blocks: Vec<TextBlock>,
    shapes: Vec<ShapeItem>,
    master: &MasterStyles,
) -> Vec<Node> {
    let mut nodes = Vec::new();
    for item in shapes {
        match item {
            ShapeItem::Table { table } => nodes.push(Node::Table(table)),
            ShapeItem::Text { text, .. } => {
                let (is_title, text_type, content, styles, autonums) = match text.outline_ref {
                    Some(ix) => match blocks.get_mut(ix as usize) {
                        Some(block) => {
                            block.consumed = true;
                            // Outline text: bullets come from the block's own
                            // style runs; the shape may add PP9 autonumbers.
                            let autos: Vec<_> = text.styles.iter().map(|s| s.autonum).collect();
                            (
                                block.is_title,
                                block.text_type,
                                block.text.clone(),
                                block.styles.clone(),
                                autos,
                            )
                        }
                        None => continue,
                    },
                    None => {
                        // Embedded text: mark the matching SLWT twin (if any)
                        // consumed so the tail append doesn't duplicate it.
                        if let Some(block) = blocks
                            .iter_mut()
                            .find(|b| !b.consumed && b.text == text.text)
                        {
                            block.consumed = true;
                        }
                        let autos: Vec<_> = text.styles.iter().map(|s| s.autonum).collect();
                        (
                            text.is_title,
                            text.text_type,
                            text.text.clone(),
                            text.styles.clone(),
                            autos,
                        )
                    }
                };
                let para = Paragraphs {
                    is_title,
                    text: &content,
                    styles: &styles,
                    autonums: &autonums,
                    inherited: |indent| master.bullet(text_type, indent),
                };
                push_text(&mut nodes, para);
            }
        }
    }
    for block in blocks.iter().filter(|b| !b.consumed) {
        let autos: Vec<_> = block.styles.iter().map(|s| s.autonum).collect();
        let para = Paragraphs {
            is_title: block.is_title,
            text: &block.text,
            styles: &block.styles,
            autonums: &autos,
            inherited: |indent| master.bullet(block.text_type, indent),
        };
        push_text(&mut nodes, para);
    }
    nodes
}

/// A list still accepting items while a text frame is walked — docling's
/// `_OpenList` (docling#4397), which a `.ppt` reaches through LibreOffice's
/// PPTX export: the stack is per text frame, a deeper paragraph nests a new
/// list under the open one, a shallower one pops back, a non-list paragraph
/// closes them all, and each list numbers its enumerated items on its own
/// counter.
struct OpenList {
    /// The indent level of the paragraphs this list holds.
    level: u8,
    /// Items so far, numbered or not.
    items: u64,
    /// Numbering so far — starts at the first enumerated item's PP9
    /// `startAt` (docling's `_get_auto_number_start`).
    counter: u64,
    /// Whether the first item was numbered: docling-core's Markdown numbers
    /// an unmarked item by its position when its group's first item is
    /// enumerated (`first_item_is_enumerated`), so a bullet that joins a
    /// numbered group renders `N.`, not `-`.
    first_enumerated: Option<bool>,
}

/// One text block to emit: its paragraphs' text and style runs, and the
/// master's bullet for an indent level, which a paragraph inherits when its
/// own style run leaves `fHasBullet` unset.
struct Paragraphs<'a, F: Fn(u8) -> bool> {
    is_title: bool,
    text: &'a str,
    styles: &'a [ParaStyle],
    autonums: &'a [Option<(u16, u16)>],
    inherited: F,
}

/// Emit a text run as heading/paragraph/list-item nodes, one per
/// `\r`-separated line, list-classifying each line by its paragraph style —
/// the paragraph's own bullet flag, else its master text style's (#627:
/// PowerPoint leaves a body placeholder's bullets to the master, and
/// LibreOffice — docling's `.ppt` reader — resolves them from it). List
/// items nest and number the way docling's PPTX backend does ([`OpenList`]):
/// the frame's lists are its own, an indent level only nests relative to the
/// list already open (a level-2 paragraph after plain text starts a flat
/// list), and a bullet inside a numbered group takes the group's numbering.
/// The PPTX scheme a PP9 `TextAutoNumberScheme` ([MS-PPT] 2.13.31) is — what
/// LibreOffice writes as `a:buAutoNum/@type` when docling reads a `.ppt` —
/// for the letter and Roman ones; the Arabic and East Asian schemes count in
/// decimals (`None`).
fn ppt_auto_scheme(scheme: u16) -> Option<AutoScheme> {
    let ty = match scheme {
        0x00 => "alphaLcPeriod",
        0x01 => "alphaUcPeriod",
        0x04 => "romanLcParenBoth",
        0x05 => "romanLcParenR",
        0x06 => "romanLcPeriod",
        0x07 => "romanUcPeriod",
        0x08 => "alphaLcParenBoth",
        0x09 => "alphaLcParenR",
        0x0A => "alphaUcParenBoth",
        0x0B => "alphaUcParenR",
        0x0E => "romanUcParenBoth",
        0x0F => "romanUcParenR",
        _ => return None,
    };
    AutoScheme::parse(ty)
}

fn push_text<F: Fn(u8) -> bool>(nodes: &mut Vec<Node>, para: Paragraphs<F>) {
    let Paragraphs {
        is_title,
        text,
        styles,
        autonums,
        inherited,
    } = para;
    let mut open_lists: Vec<OpenList> = Vec::new();
    // Map each paragraph to its style run by cumulative character position.
    let mut run_ix = 0usize;
    let mut run_left = styles.first().map(|s| s.count).unwrap_or(usize::MAX);
    for line in text.split('\r') {
        let chars = line.chars().count() + 1; // + terminator
        let style = styles.get(run_ix).copied().unwrap_or_default();
        let autonum = autonums.get(run_ix).copied().flatten().or(style.autonum);
        // Advance the run cursor.
        if run_left <= chars {
            run_ix += 1;
            run_left = styles.get(run_ix).map(|s| s.count).unwrap_or(usize::MAX);
        } else {
            run_left -= chars;
        }

        // docling keeps the run's own spacing (a trailing space in the
        // source survives into Markdown); only whitespace-only lines drop.
        if line.trim().is_empty() {
            continue;
        }
        if is_title {
            nodes.push(Node::Heading {
                level: 1,
                text: line.to_string(),
            });
            open_lists.clear();
            continue;
        }
        let numbered = autonum.is_some();
        let bullet = style.bullet.unwrap_or_else(|| inherited(style.indent));
        if !bullet && !numbered {
            nodes.push(Node::Paragraph {
                text: line.to_string(),
            });
            open_lists.clear();
            continue;
        }
        let level = style.indent;
        while open_lists.len() > 1 && open_lists.last().is_some_and(|l| l.level > level) {
            open_lists.pop();
        }
        let first_in_list = open_lists.is_empty();
        if open_lists.is_empty() || open_lists.last().is_some_and(|l| level > l.level) {
            open_lists.push(OpenList {
                level,
                items: 0,
                counter: 0,
                first_enumerated: None,
            });
        }
        let depth = open_lists.len() - 1;
        let current = open_lists.last_mut().expect("a list is open");
        let n = if numbered {
            if current.counter == 0 {
                current.counter = autonum
                    .map_or(0, |(_, start)| start as u64)
                    .saturating_sub(1);
            }
            current.counter += 1;
            current.counter
        } else {
            0
        };
        current.items += 1;
        let first_enumerated = *current.first_enumerated.get_or_insert(numbered);
        let (ordered, number) = match (numbered, first_enumerated) {
            (false, true) => (true, current.items),
            _ => (numbered, n),
        };
        // docling#5122: a letter or Roman scheme prints its own marker
        // (`a.`, `(iv)`); the decimal ones keep the plain `N.` numbering.
        let marker = autonum
            .filter(|_| numbered)
            .and_then(|(scheme, _)| ppt_auto_scheme(scheme))
            .map(|s| auto_number_marker(n, Some(s)));
        nodes.push(Node::ListItem {
            ordered,
            number,
            first_in_list,
            text: line.to_string(),
            level: depth as u8,
            marker,
            location: None,
            dclx: None,
            href: None,
            layer: None,
        });
    }
}

/// UTF-16LE text of a `TextCharsAtom` body.
fn utf16_text(b: &[u8]) -> String {
    b.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .map(|u| char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
        .filter(|&c| c != '\u{0000}')
        .collect()
}

/// CP1252 text of a `TextBytesAtom` body (high bytes match Latin-1 closely
/// enough for slide text; the smart-quote block goes through the same table
/// as the DOC backend).
fn bytes_text(b: &[u8]) -> String {
    b.iter().map(|&x| super::doc::cp1252(x)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #624: every PowerPoint-encrypted fixture — open password, modify
    /// password, both, a version 4 file — references a
    /// `CryptSession10Container` from its current edit, and the plain files
    /// do not. The `Current User` header token is no marker: B's is the
    /// plain-file value.
    #[test]
    fn user_edit_chain_finds_the_encryption_session() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/encrypted/");
        let edits = |path: &str| {
            let data = std::fs::read(path).unwrap();
            let cfb = CompoundFile::open(&data).unwrap();
            let doc = cfb.stream("PowerPoint Document").unwrap();
            let user = cfb.stream("Current User").unwrap();
            let edits = UserEdits::read(&user, &doc).expect("edit chain");
            let session = edits.crypt_session(&doc).map(<[u8]>::len);
            (edits.encrypted(), session)
        };
        for name in [
            "min_encrypted.ppt",
            "min_writepw.ppt",
            "B_openpw.ppt",
            "C_writepw.ppt",
            "D_both.ppt",
            "H_A_addpw_save.ppt",
        ] {
            let (encrypted, session) = edits(&format!("{dir}{name}"));
            assert!(encrypted, "{name}");
            // RC4 CryptoAPI EncryptionInfo: 4.2 version, header, verifier.
            assert_eq!(session, Some(198), "{name}");
        }
        for plain in [
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/data/ppt/sources/powerpoint_sample.ppt"
            ),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/data/ppt/sources/ppt_cfb_v4_edit_save.ppt"
            ),
        ] {
            assert_eq!(edits(plain), (false, None), "{plain}");
        }
    }
    use crate::InputFormat;

    #[test]
    fn grid_table_rejects_a_column_of_shapes() {
        // Four stacked shapes (one column) must NOT read as a table.
        let cells: Vec<(Anchor, ShapeText)> = (0..4)
            .map(|i| ((0, i * 100, 200, i * 100 + 90), ShapeText::default()))
            .collect();
        assert!(grid_table(&cells).is_none());
    }

    #[test]
    fn grid_table_builds_2x2_with_span() {
        let mk = |s: &str| ShapeText {
            text: s.into(),
            ..ShapeText::default()
        };
        // 2×2 grid; the top row's single cell spans both columns.
        let cells = vec![
            ((0, 0, 200, 90), mk("header spans")),
            ((0, 100, 100, 190), mk("a")),
            ((100, 100, 200, 190), mk("b")),
            ((0, 200, 100, 290), mk("c")),
            ((100, 200, 200, 290), mk("d")),
        ];
        let t = grid_table(&cells).expect("is a table");
        assert_eq!(t.rows.len(), 3);
        // Spanned text repeats across the covered cells (docling's table-grid
        // convention); the continuation flags mark the span for DocLang.
        assert_eq!(
            t.rows[0],
            vec!["header spans".to_string(), "header spans".to_string()]
        );
        assert_eq!(t.rows[1], vec!["a".to_string(), "b".to_string()]);
        let s = t.structure.expect("span structure");
        assert!(s.col_continuation[0][1], "top row spans");
    }

    /// A record: header (version 0, `instance`, `rec_type`, length) + body.
    fn record(instance: u16, rec_type: u16, body: &[u8]) -> Vec<u8> {
        let mut out = (instance << 4).to_le_bytes().to_vec();
        out.extend(rec_type.to_le_bytes());
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(body);
        out
    }

    /// #627: a paragraph without a bullet flag of its own inherits the
    /// master's, resolved as LibreOffice's `PPTStyleSheet` does: an unset
    /// level takes the level below it, the center/half/quarter types start
    /// from Body's levels, and a type with no style at all from its parent's.
    /// The levels sit back to back, so each `TextCFException` is sized.
    #[test]
    fn master_text_styles_resolve_like_libreoffice() {
        let pf = |bullet: Option<bool>| -> Vec<u8> {
            match bullet {
                Some(b) => [
                    1u32.to_le_bytes().to_vec(),
                    (b as u16).to_le_bytes().to_vec(),
                ]
                .concat(),
                None => 0u32.to_le_bytes().to_vec(),
            }
        };
        // fontStyle (2) + color (4): the next level starts after them.
        let cf = [0x0004_0001u32.to_le_bytes().to_vec(), vec![0; 6]].concat();
        let mut body_style = 3u16.to_le_bytes().to_vec();
        for bullet in [Some(true), None, Some(false)] {
            body_style.extend(pf(bullet));
            body_style.extend(&cf);
        }
        // Center body (instance 5) numbers its one level: level 0, no bullet.
        let mut center = 1u16.to_le_bytes().to_vec();
        center.extend(0u16.to_le_bytes());
        center.extend(pf(Some(false)));
        center.extend(&cf);
        let mut other = 1u16.to_le_bytes().to_vec();
        other.extend(pf(Some(false)));
        other.extend(&cf);
        let master = [
            record(1, RT_TEXT_MASTER_STYLE_ATOM, &body_style),
            record(5, RT_TEXT_MASTER_STYLE_ATOM, &center),
            record(4, RT_TEXT_MASTER_STYLE_ATOM, &other),
        ]
        .concat();
        let styles = MasterStyles::read(&master);
        assert!(styles.bullet(1, 0), "body level 1 bullets");
        assert!(styles.bullet(1, 1), "unset level 2 takes level 1's");
        assert!(!styles.bullet(1, 2), "level 3 says no bullet");
        assert!(!styles.bullet(1, 4), "and level 5 inherits that");
        assert!(!styles.bullet(5, 0), "the center body's own level 1");
        assert!(styles.bullet(5, 1), "its unset level 2 is Body's");
        assert!(styles.bullet(7, 0), "a half body with no style is Body");
        assert!(!styles.bullet(4, 0), "other text");
        assert!(!styles.bullet(0, 0), "no title style: no bullet");
    }

    /// #627: shapes are read in docling's order — by top edge, a row for tops
    /// within 0.05" (28.8 master units) of the previous one, left to right in
    /// a row, anchorless shapes last.
    #[test]
    fn shapes_are_read_in_rows_then_left_to_right() {
        let item = |s: &str| ShapeItem::Text {
            anchor: None,
            text: ShapeText {
                text: s.into(),
                ..ShapeText::default()
            },
        };
        let units = vec![
            (None, vec![item("anchorless")]),
            (Some((0, 500, 10, 600)), vec![item("title below")]),
            (Some((50, 10, 60, 20)), vec![item("row right")]),
            (
                Some((0, 38, 10, 48)),
                vec![item("row left, 28 units lower")],
            ),
            (Some((0, 80, 10, 90)), vec![item("next row")]),
        ];
        let order: Vec<String> = by_position(units)
            .into_iter()
            .map(|i| match i {
                ShapeItem::Text { text, .. } => text.text,
                ShapeItem::Table { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(
            order,
            [
                "row left, 28 units lower",
                "row right",
                "next row",
                "title below",
                "anchorless"
            ]
        );
    }

    /// A body frame's list items nest and number like docling's PPTX backend
    /// (`_OpenList`, which a `.ppt` reaches through LibreOffice's export): a
    /// deeper level nests only relative to the open list, plain text closes
    /// the stack so a later level-2 run is flat, a numbered run starts at its
    /// PP9 `startAt`, and a bullet joining a numbered group takes its number.
    #[test]
    fn lists_nest_and_number_like_docling() {
        let para = |count: usize, indent: u8, bullet: Option<bool>, autonum| ParaStyle {
            count,
            indent,
            bullet,
            autonum,
        };
        // "one\rtwo\rthree\rfour\rfive": bullet, plain (level 1), two
        // numbered at level 2, bullet at level 0 — the J deck.
        let styles = [
            para(4, 0, Some(true), None),
            para(4, 1, Some(false), None),
            para(6, 2, Some(true), Some((3, 1))),
            para(5, 2, Some(true), Some((3, 1))),
            para(5, 0, Some(true), None),
        ];
        let mut nodes = Vec::new();
        push_text(
            &mut nodes,
            Paragraphs {
                is_title: false,
                text: "one\rtwo\rthree\rfour\rfive",
                styles: &styles,
                autonums: &[],
                inherited: |_| false,
            },
        );
        let shape: Vec<(bool, u64, bool, u8)> = nodes
            .iter()
            .map(|n| match n {
                Node::ListItem {
                    ordered,
                    number,
                    first_in_list,
                    level,
                    ..
                } => (*ordered, *number, *first_in_list, *level),
                Node::Paragraph { .. } => (false, 0, false, u8::MAX),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (false, 0, true, 0),
                (false, 0, false, u8::MAX),
                (true, 1, true, 0), // flat: nothing is open after plain text
                (true, 2, false, 0),
                (true, 3, false, 0), // the bullet joins the numbered group
            ]
        );

        // "four\rfive\rbullet": numbered from 4, then a bullet — the K deck;
        // and a nested bullet under a bullet keeps its depth.
        let styles = [
            para(5, 0, Some(true), Some((3, 4))),
            para(5, 0, Some(true), Some((3, 1))),
            para(7, 0, Some(true), None),
            para(7, 1, Some(true), None),
        ];
        let mut nodes = Vec::new();
        push_text(
            &mut nodes,
            Paragraphs {
                is_title: false,
                text: "four\rfive\rbullet\rnested",
                styles: &styles,
                autonums: &[],
                inherited: |_| false,
            },
        );
        let numbers: Vec<(bool, u64, u8)> = nodes
            .iter()
            .map(|n| match n {
                Node::ListItem {
                    ordered,
                    number,
                    level,
                    ..
                } => (*ordered, *number, *level),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            numbers,
            vec![(true, 4, 0), (true, 5, 0), (true, 3, 0), (false, 0, 1)]
        );
    }

    /// docling#5122 (#1018) on the `.ppt` route: a PP9 letter / Roman scheme
    /// prints its marker, an Arabic one keeps the plain numbering, and a
    /// `startAt` above 32767 starts at 1.
    #[test]
    fn auto_number_schemes_pick_the_marker() {
        let para = |count: usize, autonum| ParaStyle {
            count,
            indent: 0,
            bullet: Some(true),
            autonum,
        };
        let markers = |styles: &[ParaStyle], text: &str| -> Vec<(Option<String>, u64)> {
            let mut nodes = Vec::new();
            push_text(
                &mut nodes,
                Paragraphs {
                    is_title: false,
                    text,
                    styles,
                    autonums: &[],
                    inherited: |_| false,
                },
            );
            nodes
                .iter()
                .map(|n| match n {
                    Node::ListItem { marker, number, .. } => (marker.clone(), *number),
                    other => panic!("unexpected {other:?}"),
                })
                .collect()
        };
        // alphaLcPeriod, then romanUcParenBoth from 3 in a frame of its own.
        let lettered = [para(5, Some((0x00, 1))), para(6, Some((0x00, 1)))];
        assert_eq!(
            markers(&lettered, "Plan\rBuild"),
            [(Some("a.".into()), 1), (Some("b.".into()), 2)]
        );
        let roman = [para(6, Some((0x0E, 3))), para(6, Some((0x0E, 3)))];
        assert_eq!(
            markers(&roman, "Annex\rIndex"),
            [(Some("(III)".into()), 3), (Some("(IV)".into()), 4)]
        );
        // arabicParenRight keeps the decimal numbering, no marker.
        let arabic = [para(6, Some((0x02, 1))), para(7, Some((0x02, 1)))];
        assert_eq!(markers(&arabic, "Draft\rReview"), [(None, 1), (None, 2)]);
        // An out-of-range start read from the PP9 atom: 1 instead.
        let mut atom = Vec::new();
        atom.extend_from_slice(&0x0300_0000u32.to_le_bytes()); // autonum + scheme masks
        atom.extend_from_slice(&1u16.to_le_bytes()); // fBulletHasAutoNumber
        atom.extend_from_slice(&0u16.to_le_bytes()); // alphaLcPeriod
        atom.extend_from_slice(&40000u16.to_le_bytes()); // startAt
        atom.extend_from_slice(&[0; 8]); // empty CF9 + SI exceptions
        assert_eq!(
            parse_para_styles9(&atom).first().copied().flatten(),
            Some((0, 1))
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let src = SourceDocument::from_bytes("x.ppt", InputFormat::Ppt, vec![0u8; 128]);
        assert!(PptBackend.convert(&src).is_err());
    }
}
