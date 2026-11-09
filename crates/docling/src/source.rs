//! Input document loading.
//!
//! The Rust analogue of `docling.datamodel.document.InputDocument`. A
//! `SourceDocument` holds the raw bytes plus a resolved [`InputFormat`]; it is
//! what you hand to [`crate::DocumentConverter::convert`].

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::error::ConversionError;
use crate::format::InputFormat;

/// A loaded input document: its name, detected format, and raw bytes.
#[derive(Debug, Clone)]
pub struct SourceDocument {
    pub name: String,
    pub format: InputFormat,
    pub bytes: Vec<u8>,
    /// The file extension the document arrived with (lowercase, no dot),
    /// when it had one — what tells a [`InputFormat::Text`] source its
    /// language (`py` → a `python` code block). `None` for nameless input.
    pub extension: Option<String>,
    /// The filesystem path it was loaded from, if any (`from_file`). Used to
    /// resolve relative `<img src>` paths when image fetching is enabled; `None`
    /// for in-memory sources.
    pub path: Option<PathBuf>,
    /// The URL this document was fetched from, if any. Used to resolve
    /// relative / protocol-relative `<img src>` against the page's origin when
    /// image fetching is enabled (an HTML page fetched from the web references
    /// its images by relative path). `None` for local / in-memory sources.
    pub base_url: Option<String>,
    /// The character encoding to decode the bytes with when a backend reads
    /// them as text — docling's `TextBackendOptions.encoding`
    /// (`MarkdownBackendOptions(encoding="shift_jis")`). A WHATWG encoding
    /// label (`shift_jis`, `koi8-r`, `windows-1251`, `latin1`; Python codec
    /// spellings with `_` are accepted too). `None` (default) detects the
    /// encoding — see [`Self::text`]. Set by [`Self::with_encoding`] or, for
    /// every source a converter handles, `DocumentConverter::encoding`.
    pub encoding: Option<String>,
    /// The media type the document arrived with (`text/html`), when one did
    /// — an upload's or a fetched URL's `Content-Type` — so a backend
    /// registered for it (#680, [`crate::BackendMatcher::media_types`]) still
    /// sees it at conversion time. `None` for files and nameless bytes.
    pub media_type: Option<String>,
}

impl SourceDocument {
    /// Load a document from a filesystem path, detecting the format from the
    /// extension.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConversionError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        Self::from_path_and_bytes(path, bytes)
    }

    /// [`Self::from_file`] for bytes already read from `path`.
    pub(crate) fn from_path_and_bytes(
        path: &Path,
        bytes: Vec<u8>,
    ) -> Result<Self, ConversionError> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        // The extension decides; without one, or with one the table does not
        // know, the bytes do (#672, `InputFormat::detect`): a known signature
        // — a PPTX renamed to nothing, a PDF saved as `.bin` — else text as
        // plain text (#671): a `Makefile`, a `.cfg` in a dialect the table
        // lacks.
        let format = match ext.as_deref().and_then(InputFormat::from_extension) {
            Some(f) => f,
            None => crate::sniff::detect_any(&bytes)
                .or_else(|| crate::sniff::looks_like_text(&bytes).then_some(InputFormat::Text))
                .ok_or_else(|| ConversionError::UnknownFormat {
                    hint: match &ext {
                        Some(e) => format!(
                            "unrecognized extension '.{e}' and the content is neither a known \
                             signature nor text"
                        ),
                        None => format!(
                            "no extension on {} and the content is neither a known signature \
                             nor text",
                            path.display()
                        ),
                    },
                })?,
        };
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("document")
            .to_string();
        Ok(Self {
            name,
            format,
            bytes,
            extension: ext,
            path: Some(path.to_path_buf()),
            base_url: None,
            encoding: None,
            media_type: None,
        })
    }

    /// Build a document from bytes that arrived with a name and, maybe, a
    /// media type — an upload, a fetched URL, an email attachment. The
    /// format comes from the name's extension, else from `media_type`
    /// ([`InputFormat::from_mime`]), else from the bytes themselves
    /// ([`InputFormat::detect`]: the packages, the OLE files, PDF, RTF,
    /// HTML, the raster image headers, DjVu, audio and video containers,
    /// iWork, XML, JSON and notebooks, MIME messages, …; else plain text
    /// when the bytes are text, #671). The name's stem is the document's
    /// name, as with [`Self::from_file`].
    ///
    /// # Errors
    /// [`ConversionError::UnknownFormat`] when none of the three decides —
    /// a bare name, no (or a generic) media type, unrecognized bytes.
    pub fn from_named_bytes(
        name: &str,
        bytes: Vec<u8>,
        media_type: Option<&str>,
    ) -> Result<Self, ConversionError> {
        let path = Path::new(name);
        let ext = path.extension().and_then(|e| e.to_str());
        let format = ext
            .and_then(InputFormat::from_extension)
            .or_else(|| media_type.and_then(InputFormat::from_mime))
            .or_else(|| crate::sniff::detect_any(&bytes))
            .or_else(|| crate::sniff::looks_like_text(&bytes).then_some(InputFormat::Text))
            .ok_or_else(|| ConversionError::UnknownFormat {
                hint: match (ext, media_type) {
                    (Some(e), _) => format!("unrecognized extension '.{e}'"),
                    (None, Some(m)) => format!(
                        "cannot determine the format of '{name}': no file extension, \
                         media type '{m}' is not one docling converts, and the content \
                         matches no known signature"
                    ),
                    (None, None) => format!(
                        "cannot determine the format of '{name}': no file extension, \
                         no media type, and the content matches no known signature"
                    ),
                },
            })?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("document")
            .to_string();
        let mut doc = Self::from_bytes(stem, format, bytes);
        doc.extension = ext.map(str::to_ascii_lowercase);
        doc.media_type = media_type.map(str::to_string);
        Ok(doc)
    }

    /// Read a document to the end of `reader` — stdin, a socket, a pipe —
    /// and resolve its format as [`Self::from_named_bytes`] does from `name`
    /// (`-` or `stdin` is fine: a name without an extension decides nothing)
    /// and `media_type`.
    pub fn from_reader<R: std::io::Read>(
        name: &str,
        mut reader: R,
        media_type: Option<&str>,
    ) -> Result<Self, ConversionError> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Self::from_named_bytes(name, bytes, media_type)
    }

    /// Fetch a document over HTTP(S) under `policy` (#668). The name comes
    /// from the response's `Content-Disposition`, else the URL path's last
    /// segment; the format from that name's extension, else the response's
    /// `Content-Type`, else the bytes ([`Self::from_named_bytes`]). The URL
    /// becomes the document's [`base_url`](Self::base_url) so relative
    /// `<img src>` on a fetched page resolve against its origin when image
    /// fetching is on. Blocking; every hop passes the SSRF guard
    /// ([`crate::fetch::check_outbound_url`]).
    #[cfg(feature = "fetch-url")]
    pub fn from_url(
        url: &str,
        policy: &crate::fetch::FetchPolicy,
    ) -> Result<Self, ConversionError> {
        let fetched = crate::fetch::fetch(url, policy)?;
        let name = crate::fetch::fetched_name(&fetched.url, fetched.content_disposition.as_deref());
        Ok(
            Self::from_named_bytes(&name, fetched.bytes, fetched.content_type.as_deref())?
                .with_base_url(fetched.url),
        )
    }

    /// Construct directly from in-memory bytes (no disk access). An
    /// extension on `name` (`notes.dclg`) is kept as the document's
    /// [`extension`](Self::extension); the name itself stays as given.
    pub fn from_bytes(name: impl Into<String>, format: InputFormat, bytes: Vec<u8>) -> Self {
        let name = name.into();
        let extension = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        Self {
            name,
            format,
            bytes,
            extension,
            path: None,
            base_url: None,
            encoding: None,
            media_type: None,
        }
    }

    /// Record the media type the bytes arrived with (see
    /// [`Self::media_type`]); the format is not re-resolved.
    pub fn with_media_type(mut self, media_type: Option<String>) -> Self {
        self.media_type = media_type;
        self
    }

    /// Decode the bytes with this character encoding when read as text —
    /// nothing is guessed, and bytes the encoding cannot decode fail the
    /// conversion (docling raises `DocumentLoadError`). `None` restores
    /// detection.
    pub fn with_encoding(mut self, label: Option<String>) -> Self {
        self.encoding = label;
        self
    }

    /// Record the URL this document was fetched from (for resolving relative
    /// `<img src>` against the page origin when image fetching is enabled).
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// The directory containing the source file, for resolving relative asset
    /// paths. `None` for in-memory sources. A bare relative file name
    /// (`page.html`, as `cd dir && docling-rs page.html` passes it) has the
    /// empty path as its `Path::parent`, which `canonicalize` rejects, so no
    /// local image resolved (#1030); it names the working directory, `.`.
    pub fn base_dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent).map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        })
    }

    /// The bytes as text, for text-based backends — docling's `decode_text`
    /// (docling#4202, 2.129). A byte-order mark settles the encoding first
    /// (UTF-32, UTF-8, UTF-16, and is dropped — docling#4098/#4109: Excel and
    /// Google Sheets write one when exporting "CSV UTF-8", and kept it
    /// prefixes the first cell/line, so `# Title` stops being a heading and
    /// WebVTT's signature check misses `WEBVTT`). Without one the text must
    /// be UTF-8; when it is not, the share of non-ASCII bytes that still form
    /// well-formed UTF-8 sequences decides: at 50% or more the file reads as
    /// *damaged* UTF-8 and is an error (decoding it as a code page would turn
    /// the damage into text that looks fine and is not); below that the
    /// encoding is guessed statistically (#675, `chardetng` — Firefox's
    /// detector, what MarkItDown asks `charset_normalizer` for): a guess of
    /// a non-Latin encoding — Shift_JIS, EUC-JP, GBK, Big5, EUC-KR, KOI8-R,
    /// windows-1251, Greek, Hebrew, Arabic, Thai, … — that decodes cleanly
    /// into letters is taken, with a warning naming it; otherwise the file
    /// is the one code page docling falls back to, windows-1252 — decoded
    /// with a warning, or an error on the five bytes that page leaves
    /// undefined (Python's `cp1252` rejects them where the WHATWG table
    /// maps them to C1 controls). A Latin code page other than 1252
    /// (windows-1250, ISO-8859-2, Turkish, Baltic) is not guessed — its
    /// bytes read as text under 1252 either way, as they always did — so
    /// name it with `encoding`, which wins over every guess. Borrowed when
    /// the bytes are already UTF-8.
    pub fn text(&self) -> Result<Cow<'_, str>, ConversionError> {
        match &self.encoding {
            Some(label) => decode_text_as(&self.bytes, label),
            None => decode_text(&self.bytes),
        }
    }
}

/// Decode with a requested encoding — docling's `decode_text(…, encoding)`
/// branch: the label is looked up, the bytes are decoded strictly (a leading
/// byte-order mark of that encoding is dropped) and a byte the encoding cannot
/// map is an error, never a guess or a replacement character.
pub(crate) fn decode_text_as<'a>(
    bytes: &'a [u8],
    label: &str,
) -> Result<Cow<'a, str>, ConversionError> {
    if let Some(codec) = unicode_codec(label) {
        return decode_unicode_as(bytes, codec, label);
    }
    let encoding = lookup_encoding(label).ok_or_else(|| {
        ConversionError::Parse(format!(
            "unknown character encoding {label:?}: use a WHATWG encoding label such as \
             utf-8, windows-1252, latin1, shift_jis, euc-jp, gbk, big5, euc-kr or koi8-r"
        ))
    })?;
    let (text, had_errors) = encoding.decode_with_bom_removal(bytes);
    if had_errors {
        return Err(ConversionError::Parse(format!(
            "input is not valid {}: it cannot be decoded with the requested encoding {label:?}",
            encoding.name()
        )));
    }
    Ok(text)
}

/// A Unicode codec named by a Python codec alias, endianness spelled out or
/// left to the mark (docling#4849, #1029): `u8`, `utf_16_le`, `utf-32-be`, …
/// — Python accepts them, WHATWG has no UTF-32 at all and spells UTF-16's
/// byte orders differently, so `encoding_rs` alone rejected them as unknown.
#[derive(Clone, Copy)]
enum UnicodeCodec {
    Utf8,
    /// `Some(true)` = little-endian; `None` = the mark decides, else LE
    /// (Python's `utf-16` / `utf-32` on a little-endian host).
    Utf16(Option<bool>),
    Utf32(Option<bool>),
}

fn unicode_codec(label: &str) -> Option<UnicodeCodec> {
    let l = label.trim().to_ascii_lowercase().replace('_', "-");
    // Python's normalization also drops the dash between the name and its
    // width (`utf16le`, `utf-16le`): compare without dashes.
    let key = l.replace('-', "");
    Some(match key.as_str() {
        "utf8" | "u8" | "utf" | "cp65001" | "utf8sig" => UnicodeCodec::Utf8,
        "utf16" | "u16" => UnicodeCodec::Utf16(None),
        "utf16le" => UnicodeCodec::Utf16(Some(true)),
        "utf16be" => UnicodeCodec::Utf16(Some(false)),
        "utf32" | "u32" => UnicodeCodec::Utf32(None),
        "utf32le" => UnicodeCodec::Utf32(Some(true)),
        "utf32be" => UnicodeCodec::Utf32(Some(false)),
        _ => return None,
    })
}

/// A requested Unicode codec, decoded strictly. A leading byte-order mark is
/// never content, so it is dropped under every alias and either explicit
/// endianness, as the auto-detect path does (docling#4849) — Python's
/// `utf-16-le` codec would keep it as U+FEFF.
fn decode_unicode_as<'a>(
    bytes: &'a [u8],
    codec: UnicodeCodec,
    label: &str,
) -> Result<Cow<'a, str>, ConversionError> {
    let invalid = |name: &str| {
        ConversionError::Parse(format!(
            "input is not valid {name}: it cannot be decoded with the requested encoding {label:?}"
        ))
    };
    let text: Cow<'a, str> = match codec {
        UnicodeCodec::Utf8 => {
            let rest = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
            Cow::Borrowed(std::str::from_utf8(rest).map_err(|_| invalid("UTF-8"))?)
        }
        UnicodeCodec::Utf16(order) => {
            let (le, rest) = match (order, bytes) {
                (None, [0xfe, 0xff, rest @ ..]) => (false, rest),
                (None, [0xff, 0xfe, rest @ ..]) => (true, rest),
                (None, _) => (true, bytes),
                (Some(le), _) => (le, bytes),
            };
            let encoding = if le {
                encoding_rs::UTF_16LE
            } else {
                encoding_rs::UTF_16BE
            };
            let text = encoding
                .decode_without_bom_handling_and_without_replacement(rest)
                .ok_or_else(|| invalid(encoding.name()))?;
            Cow::Owned(text.into_owned())
        }
        UnicodeCodec::Utf32(order) => {
            let (le, rest) = match (order, bytes) {
                (None, [0x00, 0x00, 0xfe, 0xff, rest @ ..]) => (false, rest),
                (None, [0xff, 0xfe, 0x00, 0x00, rest @ ..]) => (true, rest),
                (None, _) => (true, bytes),
                (Some(le), _) => (le, bytes),
            };
            let name = if le { "UTF-32LE" } else { "UTF-32BE" };
            decode_utf32(rest, le).map_err(|_| invalid(name))?
        }
    };
    Ok(match text {
        Cow::Borrowed(t) => Cow::Borrowed(t.strip_prefix('\u{feff}').unwrap_or(t)),
        Cow::Owned(t) => match t.strip_prefix('\u{feff}') {
            Some(rest) => Cow::Owned(rest.to_string()),
            None => Cow::Owned(t),
        },
    })
}

/// Resolve an encoding label: the WHATWG label as given, then with Python's
/// `_` spelling folded to `-` (`shift_jis` is a WHATWG label, `euc_jp` is
/// not), then the Python codec names WHATWG spells differently.
pub(crate) fn lookup_encoding(label: &str) -> Option<&'static encoding_rs::Encoding> {
    let label = label.trim();
    let dashed = label.replace('_', "-").to_ascii_lowercase();
    encoding_rs::Encoding::for_label(label.as_bytes())
        .or_else(|| encoding_rs::Encoding::for_label(dashed.as_bytes()))
        .or_else(|| {
            let alias = match dashed.as_str() {
                "latin-1" | "iso8859-1" | "iso-latin-1" => "latin1",
                "cp932" | "ms932" | "sjis" => "shift_jis",
                "mac-roman" | "macroman" => "macintosh",
                "cp1361" | "johab" | "euc-tw" => return None,
                _ => return None,
            };
            encoding_rs::Encoding::for_label(alias.as_bytes())
        })
}

/// See [`SourceDocument::text`].
pub(crate) fn decode_text(bytes: &[u8]) -> Result<Cow<'_, str>, ConversionError> {
    // UTF-32 before UTF-16: the UTF-32 LE mark starts with the UTF-16 LE one.
    // A mark settles the encoding: bytes it cannot decode are an error that
    // names the BOM-detected encoding (docling#4949, #1029), never a fallback
    // guess — a UTF-8 mark over damaged text used to read as windows-1252
    // with the mark itself as `ï»¿`.
    let bom_error = |codec: &str, detail: &str| {
        ConversionError::Parse(format!(
            "could not decode the document with the BOM-detected encoding '{codec}': {detail}"
        ))
    };
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe, 0x00, 0x00]) {
        return decode_utf32(rest, true).map_err(|e| bom_error("utf-32", &e.to_string()));
    }
    if let Some(rest) = bytes.strip_prefix(&[0x00, 0x00, 0xfe, 0xff]) {
        return decode_utf32(rest, false).map_err(|e| bom_error("utf-32", &e.to_string()));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        return std::str::from_utf8(rest)
            .map(Cow::Borrowed)
            .map_err(|e| bom_error("utf-8-sig", &e.to_string()));
    }
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        // `decode` sniffs and drops the mark itself.
        let (text, _, malformed) = encoding_rs::UTF_16LE.decode(bytes);
        if malformed {
            return Err(bom_error("utf-16", "the input is not valid UTF-16"));
        }
        return Ok(Cow::Owned(text.into_owned()));
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(Cow::Borrowed(text)),
        Err(utf8_error) => {
            let coverage = utf8_high_byte_coverage(bytes);
            if coverage >= 0.5 {
                return Err(ConversionError::Parse(format!(
                    "input is not valid UTF-8 ({utf8_error}), but {:.0}% of its non-ASCII \
                     bytes still form well-formed UTF-8 sequences, so it reads as damaged \
                     UTF-8 rather than as another encoding; decoding it as windows-1252 \
                     would turn the damage into text that looks fine and is not",
                    coverage * 100.0
                )));
            }
            // A legacy encoding of another script (#675): Firefox's detector
            // on the bytes, taken when it names a non-Latin encoding that
            // decodes them cleanly into letters.
            if let Some((encoding, text)) = guess_legacy_encoding(bytes) {
                eprintln!(
                    "warning: input is not UTF-8; it reads as {} and was decoded as that — pass \
                     encoding=… if it is another encoding",
                    encoding.name()
                );
                return Ok(Cow::Owned(text));
            }
            // Python's cp1252 has no mapping for these five bytes.
            if bytes
                .iter()
                .any(|b| matches!(b, 0x81 | 0x8d | 0x8f | 0x90 | 0x9d))
            {
                return Err(ConversionError::Parse(
                    "input is neither UTF-8 nor windows-1252, and its encoding is not \
                     declared, so it cannot be decoded reliably"
                        .into(),
                ));
            }
            eprintln!(
                "warning: input is not UTF-8; decoded it as windows-1252 — text in any \
                 other single-byte or multi-byte encoding will be wrong"
            );
            let (text, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes);
            Ok(Cow::Owned(text.into_owned()))
        }
    }
}

/// The statistical guess for bytes that are not UTF-8 (#675): `chardetng`
/// (Firefox's detector) on the whole input, UTF-8 and ISO-2022-JP ruled
/// out. Accepted only for an encoding of a non-Latin script — the Latin
/// code pages share their letters with windows-1252, which stays the
/// fallback for them (docling's behaviour, and the one the `encoding`
/// option overrides) — and only when the bytes decode without error into
/// at least three non-ASCII characters of which four in five are letters
/// or digits: a short or odd file is not guessed either.
fn guess_legacy_encoding(bytes: &[u8]) -> Option<(&'static encoding_rs::Encoding, String)> {
    use encoding_rs::*;
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Deny);
    const NON_LATIN: &[&Encoding] = &[
        SHIFT_JIS,
        EUC_JP,
        GBK,
        GB18030,
        BIG5,
        EUC_KR,
        KOI8_R,
        KOI8_U,
        IBM866,
        WINDOWS_1251,
        ISO_8859_5,
        WINDOWS_1253,
        ISO_8859_7,
        WINDOWS_1255,
        ISO_8859_8,
        ISO_8859_8_I,
        WINDOWS_1256,
        ISO_8859_6,
        WINDOWS_874,
    ];
    if !NON_LATIN.contains(&encoding) {
        return None;
    }
    let (text, malformed) = encoding.decode_without_bom_handling(bytes);
    if malformed {
        return None;
    }
    let non_ascii: Vec<char> = text.chars().filter(|c| !c.is_ascii()).collect();
    let letters = non_ascii
        .iter()
        .filter(|c| c.is_alphabetic() || c.is_numeric())
        .count();
    if non_ascii.len() < 3 || letters * 5 < non_ascii.len() * 4 {
        return None;
    }
    Some((encoding, text.into_owned()))
}

/// Python's `utf-32` codec on the bytes after the mark.
fn decode_utf32(rest: &[u8], little_endian: bool) -> Result<Cow<'static, str>, ConversionError> {
    if !rest.len().is_multiple_of(4) {
        return Err(ConversionError::Parse("truncated UTF-32 input".into()));
    }
    rest.chunks_exact(4)
        .map(|c| {
            let b = [c[0], c[1], c[2], c[3]];
            let u = if little_endian {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            };
            char::from_u32(u)
                .ok_or_else(|| ConversionError::Parse(format!("invalid UTF-32 code point {u:#x}")))
        })
        .collect::<Result<String, _>>()
        .map(Cow::Owned)
}

/// docling's `_utf8_high_byte_coverage`: the share of the non-ASCII bytes
/// that belong to well-formed UTF-8 sequences. UTF-8 with a few damaged bytes
/// scores near 1; a single-byte legacy encoding near 0 (its high bytes form
/// no sequences); Shift-JIS / GBK around 0.3.
fn utf8_high_byte_coverage(raw: &[u8]) -> f64 {
    let (mut high, mut covered, mut i) = (0usize, 0usize, 0usize);
    while i < raw.len() {
        let lead = raw[i];
        if lead < 0x80 {
            i += 1;
            continue;
        }
        let len = match lead {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        let well_formed = len > 0
            && raw
                .get(i..i + len)
                .is_some_and(|chunk| std::str::from_utf8(chunk).is_ok());
        if well_formed {
            high += len;
            covered += len;
            i += len;
        } else {
            high += 1;
            i += 1;
        }
    }
    if high == 0 {
        0.0
    } else {
        covered as f64 / high as f64
    }
}

#[cfg(test)]
mod decode_tests {
    use super::{decode_text, decode_text_as};

    /// docling's `encoding` option: the requested encoding is used as is —
    /// WHATWG or Python spelling — its own BOM dropped, and undecodable bytes
    /// or an unknown label are errors rather than guesses.
    #[test]
    fn explicit_encoding_decodes_strictly() {
        // "日本" in Shift-JIS (windows-1252 would happily mojibake it).
        let sjis = b"\x93\xfa\x96\x7b";
        assert_eq!(decode_text_as(sjis, "shift_jis").unwrap(), "日本");
        assert_eq!(decode_text_as(sjis, "Shift_JIS").unwrap(), "日本");
        assert_eq!(decode_text_as(sjis, "cp932").unwrap(), "日本");
        // "Привет" in KOI8-R; the Python `koi8_r` spelling resolves too.
        let koi = b"\xf0\xd2\xc9\xd7\xc5\xd4";
        assert_eq!(decode_text_as(koi, "koi8-r").unwrap(), "Привет");
        assert_eq!(decode_text_as(koi, "koi8_r").unwrap(), "Привет");
        assert_eq!(decode_text_as(b"caf\xe9", "latin-1").unwrap(), "caf\u{e9}");
        assert_eq!(decode_text_as(b"\xef\xbb\xbfa", "utf-8").unwrap(), "a");
        assert!(matches!(
            decode_text_as(b"plain", "utf-8").unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
        // Not UTF-8 under a UTF-8 request: an error, not a cp1252 fallback.
        let err = decode_text_as(b"caf\xe9", "utf-8").unwrap_err().to_string();
        assert!(err.contains("UTF-8"), "{err}");
        let err = decode_text_as(b"x", "klingon-1").unwrap_err().to_string();
        assert!(err.contains("unknown character encoding"), "{err}");
    }

    /// docling#4849 (#1029): every Python alias of a Unicode codec resolves,
    /// either endianness spelled out, and a leading mark is dropped under
    /// all of them; a single-byte codec still reads the mark's bytes as the
    /// characters they are. Upstream's `tests/test_text_decoding.py`.
    #[test]
    fn unicode_codec_aliases_drop_the_mark() {
        let enc16 = |s: &str, le: bool| -> Vec<u8> {
            s.encode_utf16()
                .flat_map(|u| if le { u.to_le_bytes() } else { u.to_be_bytes() })
                .collect()
        };
        let enc32 = |s: &str, le: bool| -> Vec<u8> {
            s.chars()
                .flat_map(|c| {
                    let u = c as u32;
                    if le {
                        u.to_le_bytes()
                    } else {
                        u.to_be_bytes()
                    }
                })
                .collect()
        };
        let text = "# Title\nHello";
        let utf8_bom = [&b"\xef\xbb\xbf"[..], text.as_bytes()].concat();
        for label in ["utf-8", "utf8", "UTF-8", "utf_8", "UTF8", "u8", "utf-8-sig"] {
            assert_eq!(decode_text_as(&utf8_bom, label).unwrap(), text, "{label}");
        }
        let cases: [(&str, Vec<u8>); 4] = [
            (
                "utf-16-le",
                [&[0xff, 0xfe][..], &enc16(text, true)].concat(),
            ),
            (
                "utf-16-be",
                [&[0xfe, 0xff][..], &enc16(text, false)].concat(),
            ),
            (
                "utf-32-le",
                [&[0xff, 0xfe, 0, 0][..], &enc32(text, true)].concat(),
            ),
            (
                "utf-32-be",
                [&[0, 0, 0xfe, 0xff][..], &enc32(text, false)].concat(),
            ),
        ];
        for (label, bytes) in &cases {
            assert_eq!(decode_text_as(bytes, label).unwrap(), text, "{label}");
            let python = label.replace('-', "_");
            assert_eq!(decode_text_as(bytes, &python).unwrap(), text, "{python}");
        }
        // The generic codecs read the mark's byte order.
        assert_eq!(decode_text_as(&cases[1].1, "utf-16").unwrap(), text);
        assert_eq!(decode_text_as(&cases[3].1, "utf-32").unwrap(), text);
        // Explicit and auto-detected UTF-8 agree; no mark, nothing changes.
        assert_eq!(decode_text(&utf8_bom).unwrap(), text);
        assert_eq!(
            decode_text_as(text.as_bytes(), "utf-8").unwrap(),
            decode_text(text.as_bytes()).unwrap()
        );
        // cp1252 has no mark: its bytes are content.
        assert_eq!(
            decode_text_as(&utf8_bom, "cp1252").unwrap(),
            "ï»¿# Title\nHello"
        );
        assert!(decode_text_as(b"x", "not-a-codec").is_err());
        assert!(decode_text_as(b"\xff\xfeA", "utf-16-le").is_err());
    }

    /// docling#4949 (#1029): a mark settles the encoding even when the bytes
    /// after it are damaged — the error names the BOM-detected encoding,
    /// never a windows-1252 reading of the mark and the damage. Upstream's
    /// `test_damaged_bom_input_reports_detected_encoding`.
    #[test]
    fn damaged_input_behind_a_mark_names_its_encoding() {
        for (raw, codec) in [
            (&b"\xef\xbb\xbf\xff"[..], "utf-8-sig"),
            (b"\xef\xbb\xbfcaf\xe9 na\xefve", "utf-8-sig"),
            (b"\xff\xfeA", "utf-16"),
            (b"\xfe\xff\x00", "utf-16"),
            (b"\xff\xfe\x00\x00A\x00\x00", "utf-32"),
            (b"\x00\x00\xfe\xff\x00\x00\x00", "utf-32"),
        ] {
            let err = decode_text(raw).unwrap_err().to_string();
            assert!(err.contains(&format!("'{codec}'")), "{raw:?}: {err}");
        }
    }

    /// docling#4202: a BOM settles the encoding (and is dropped); UTF-8 is
    /// borrowed; a legacy single-byte file decodes as windows-1252; damaged
    /// UTF-8 and bytes cp1252 leaves undefined are errors, not guesses —
    /// and a legacy encoding of another script is guessed (#675).
    #[test]
    fn text_documents_decode_like_docling() {
        assert_eq!(decode_text("caf\u{e9}".as_bytes()).unwrap(), "caf\u{e9}");
        assert!(matches!(
            decode_text(b"plain").unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(decode_text(b"\xef\xbb\xbf# T").unwrap(), "# T");
        // UTF-16 LE / BE and UTF-32 LE with their marks.
        assert_eq!(decode_text(b"\xff\xfeh\x00i\x00").unwrap(), "hi");
        assert_eq!(decode_text(b"\xfe\xff\x00h\x00i").unwrap(), "hi");
        assert_eq!(decode_text(b"\xff\xfe\x00\x00h\x00\x00\x00").unwrap(), "h");
        // windows-1252: no UTF-8 sequence at all among the high bytes.
        assert_eq!(
            decode_text(b"caf\xe9 \x93q\x94").unwrap(),
            "caf\u{e9} \u{201c}q\u{201d}"
        );
        // Mostly valid UTF-8 with one stray byte: damaged, refused.
        let damaged = decode_text(b"caf\xc3\xa9 na\xc3\xafve \xff");
        assert!(damaged.unwrap_err().to_string().contains("damaged UTF-8"));
        // A byte cp1252 does not define.
        assert!(decode_text(b"x\x81y").is_err());
        // #675: Shift_JIS, KOI8-R and GBK text is read as such, with the
        // bytes' own letters; Western text keeps the windows-1252 reading.
        let sjis = "名前,年齢,住所\n佐藤太郎,30,東京\n髙橋淳,35,名古屋\n";
        let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode(sjis);
        assert_eq!(decode_text(&bytes).unwrap(), sjis);
        let koi = "Отчёт о командировке: встречи с заказчиком, обсуждение сроков.";
        let (bytes, _, _) = encoding_rs::KOI8_R.encode(koi);
        assert_eq!(decode_text(&bytes).unwrap(), koi);
        let gbk = "本周完成了数据导入模块的开发，并与测试团队一起验证了常见的输入格式。";
        let (bytes, _, _) = encoding_rs::GBK.encode(gbk);
        assert_eq!(decode_text(&bytes).unwrap(), gbk);
        // Latin code pages stay windows-1252: Polish in windows-1250 reads
        // with 1252's letters, as before, until `encoding` names it.
        let (bytes, _, _) =
            encoding_rs::WINDOWS_1250.encode("Zażółć gęślą jaźń, zażółć gęślą jaźń.");
        let (want, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(&bytes);
        assert_eq!(decode_text(&bytes).unwrap(), want);
        assert_eq!(
            decode_text_as(&bytes, "windows-1250").unwrap(),
            "Zażółć gęślą jaźń, zażółć gęślą jaźń."
        );
    }
}
