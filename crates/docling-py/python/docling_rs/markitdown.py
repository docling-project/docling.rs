"""MarkItDown-compatible facade (#674) — Microsoft MarkItDown's Python API over
the docling.rs engine::

    # was: from markitdown import MarkItDown, StreamInfo
    from docling_rs.markitdown import MarkItDown, StreamInfo

    md = MarkItDown()
    result = md.convert("report.docx")          # path, URL, file:/data: URI,
    print(result.markdown)                      # binary stream or requests.Response
    print(result.title)

    with open("scan.pdf", "rb") as fh:
        md.convert_stream(fh, stream_info=StreamInfo(extension=".pdf"))

The class, its ``convert`` / ``convert_local`` / ``convert_stream`` /
``convert_uri`` / ``convert_url`` / ``convert_response`` methods,
:class:`StreamInfo`, :class:`DocumentConverterResult` (``.markdown``,
``.text_content``, ``.title``) and the exception hierarchy follow MarkItDown
0.1's ``_markitdown.py`` / ``_base_converter.py`` / ``_exceptions.py``, so
code written against MarkItDown runs unchanged. What differs, deliberately:

* **The Markdown is docling's**, not MarkItDown's: headings, tables and lists
  come from docling's document model (layout analysis and TableFormer for
  PDFs), pictures are ``<!-- image -->`` placeholders — ``keep_data_uris=True``
  embeds them as ``data:`` URIs, as in MarkItDown. Tables are compact
  (``| a | b |``, as MarkItDown writes them — docling-core 2.9x), and a
  workbook gets MarkItDown's ``## <sheet>`` heading per sheet. Byte-identical
  output is a non-goal; the content is what the ported MarkItDown test
  checks pin.
* **Plugins** (#680): ``register_converter(converter, priority=…)`` takes a
  MarkItDown ``DocumentConverter`` (``accepts`` / ``convert``) and
  ``enable_plugins`` loads the ``markitdown.plugin`` entry points, whose
  ``register_converters(markitdown, **kwargs)`` registers theirs. A converter
  at or below ``PRIORITY_SPECIFIC_FILE_FORMAT`` is tried before the engine
  (lower first, the later registration first among equals — MarkItDown's
  order), a generic one after the engine finds no format. Engine-level
  backends that every surface sees are ``docling_rs.register_backend``.
  ``exiftool_path``, ``style_map`` and the Azure ``docintel_*`` / ``cu_*``
  options are accepted and ignored with a warning; any other unknown
  keyword warns too.
* **LLM image captions** — ``llm_client`` / ``llm_model`` / ``llm_prompt``
  (#670): an OpenAI-style client describes every embedded picture, the
  description lands in docling's picture metadata and in the Markdown after
  the picture; a standalone image input gets MarkItDown's ``# Description:``
  section. Same request as MarkItDown's ``llm_caption``.
* **PDFs without the models**: the full PDF pipeline (layout, tables, OCR)
  needs ``docling_rs.download_models()``; until then a PDF converts from its
  embedded text layer — MarkItDown's own PDF behavior — with a one-time
  warning, instead of failing.
* **Format detection** uses the hints (``StreamInfo``, file name, URL,
  ``Content-Type``) and, without any, the file's magic bytes (PDF, OOXML,
  ODF, EPUB, ZIP, Outlook/legacy Office, images, audio, HTML, CSV, text).
  A text input that is not UTF-8 and carries no charset hint is decoded by
  ``charset_normalizer``'s guess when it is installed (``requests`` brings
  it); otherwise the engine's own detection applies — BOM, UTF-8, a
  statistical guess for a non-Latin encoding (``chardetng``, #675), else
  windows-1252.
* **ZIP archives** convert entry by entry under MarkItDown's ``## File:``
  headings; entries docling.rs cannot convert are skipped. A ZIP inside the
  archive is read when ``DOCLING_RS_ZIP_MAX_DEPTH`` allows (default 1: it
  is skipped; at most 3), its entries as ``inner.zip/<path>`` (#677).
* **YouTube URLs** (#681, MarkItDown's ``_youtube_converter``) come out as
  ``# YouTube`` / ``## <title>`` / ``### Video Metadata`` / ``### Description``
  / ``### Transcript`` — the transcript as the caption cues, one line each
  (MarkItDown joins them into one line), in the language ``asr_lang``
  asks for (``docling_options={"asr_lang": "de"}``; English by default,
  YouTube's translation when the video has no such track — MarkItDown's
  ``youtube_transcript_languages``). A video without captions is an error,
  where MarkItDown returns the metadata alone.

``MarkItDown(docling_options={...})`` (a docling.rs extension) forwards
keyword arguments to :class:`docling_rs.DocumentConverter` — e.g.
``{"text_layer_only": True}`` for MarkItDown-speed PDFs, ``{"ocr_lang": "ch"}``.
The converted ``DoclingDocument`` is on ``result.document``.

``python -m docling_rs.markitdown FILE`` mirrors the ``markitdown`` CLI.
"""

from __future__ import annotations

import base64
import email.message
import io
import mimetypes
import os
import sys
import warnings
import zipfile
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, BinaryIO, ClassVar, Dict, Iterable, List, Optional, Tuple, Union
from urllib.parse import unquote, unquote_to_bytes, urlparse, parse_qs
from urllib.request import url2pathname

from . import ConversionError, DocumentConverter
from . import detect_format as _detect_format
from .picture_description import PictureDescriptionBaseOptions, describe_pictures

__all__ = [
    "MarkItDown",
    "DocumentConverterResult",
    "StreamInfo",
    "MarkItDownException",
    "MissingDependencyException",
    "UnsupportedFormatException",
    "FailedConversionAttempt",
    "FileConversionException",
    "PRIORITY_SPECIFIC_FILE_FORMAT",
    "PRIORITY_GENERIC_FILE_FORMAT",
]

# Accepted for `register_converter` call-site compatibility (MarkItDown's values).
PRIORITY_SPECIFIC_FILE_FORMAT = 0.0
PRIORITY_GENERIC_FILE_FORMAT = 10.0

# MarkItDown's `llm_caption` default prompt.
DEFAULT_LLM_PROMPT = "Write a detailed caption for this image."


# ---- MarkItDown's data types --------------------------------------------

# MarkItDown declares StreamInfo `kw_only` (Python 3.10+); this package
# supports 3.9, where the fields are simply keyword-or-positional.
_KW_ONLY = {"kw_only": True} if sys.version_info >= (3, 10) else {}


@dataclass(frozen=True, **_KW_ONLY)
class StreamInfo:
    """What is known about a stream — MarkItDown's ``StreamInfo``. Every field
    is optional; the facade fills in what the source tells it (a local path,
    a URL, ``Content-Type`` / ``Content-Disposition``)."""

    mimetype: Optional[str] = None
    extension: Optional[str] = None
    charset: Optional[str] = None
    filename: Optional[str] = None
    local_path: Optional[str] = None
    url: Optional[str] = None

    def copy_and_update(self, *args: "StreamInfo", **kwargs: Any) -> "StreamInfo":
        """A copy updated with the non-``None`` fields of each ``StreamInfo``
        in ``args``, then with ``kwargs`` (as MarkItDown)."""
        new_info = asdict(self)
        for si in args:
            assert isinstance(si, StreamInfo)
            new_info.update({k: v for k, v in asdict(si).items() if v is not None})
        new_info.update(kwargs)
        return StreamInfo(**new_info)


class DocumentConverterResult:
    """The result of a conversion — MarkItDown's ``DocumentConverterResult``:
    ``markdown`` (also ``text_content``, MarkItDown's soft-deprecated alias,
    and ``str(result)``) and an optional ``title``. docling.rs adds
    ``document``, the converted ``DoclingDocument`` (``None`` for a ZIP)."""

    def __init__(self, markdown: str, *, title: Optional[str] = None, document: Any = None):
        self.markdown = markdown
        self.title = title
        self.document = document

    @property
    def text_content(self) -> str:
        """Soft-deprecated alias for :attr:`markdown` (as in MarkItDown)."""
        return self.markdown

    @text_content.setter
    def text_content(self, markdown: str) -> None:
        self.markdown = markdown

    def __str__(self) -> str:
        return self.markdown


class MarkItDownException(Exception):
    """Base class of the facade's exceptions (MarkItDown's)."""


class MissingDependencyException(MarkItDownException):
    """Kept for ``except`` compatibility: docling.rs has no optional
    per-format dependencies, so it is never raised."""


class UnsupportedFormatException(MarkItDownException):
    """No converter for the input — the format could not be determined, or
    docling.rs has no backend for it."""


class FailedConversionAttempt:
    """One failed attempt, as MarkItDown records them: the converter and the
    ``sys.exc_info()`` triple."""

    def __init__(self, converter: Any, exc_info: Optional[tuple] = None):
        self.converter = converter
        self.exc_info = exc_info


class FileConversionException(MarkItDownException):
    """The format is supported, but converting this input failed. The engine's
    error is the ``__cause__`` and the first attempt's ``exc_info``."""

    def __init__(
        self,
        message: Optional[str] = None,
        attempts: Optional[List[FailedConversionAttempt]] = None,
    ):
        self.attempts = attempts
        if message is None:
            if attempts is None:
                message = "File conversion failed."
            else:
                message = f"File conversion failed after {len(attempts)} attempts:\n"
                for attempt in attempts:
                    name = type(attempt.converter).__name__
                    if attempt.exc_info is None:
                        message += f" -  {name} provided no execution info."
                    else:
                        message += (
                            f" - {name} threw {attempt.exc_info[0].__name__} "
                            f"with message: {attempt.exc_info[1]}\n"
                        )
        super().__init__(message)


# ---- format resolution ---------------------------------------------------

# Media type → the extension docling.rs routes on (it detects by extension).
_MIME_EXTENSIONS = {
    "application/pdf": ".pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document": ".docx",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation": ".pptx",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet": ".xlsx",
    "application/vnd.ms-excel.sheet.binary.macroenabled.12": ".xlsb",
    "application/msword": ".doc",
    "application/vnd.ms-excel": ".xls",
    "application/vnd.ms-powerpoint": ".ppt",
    "application/vnd.ms-outlook": ".msg",
    "message/rfc822": ".eml",
    "application/epub+zip": ".epub",
    "application/zip": ".zip",
    "application/x-zip-compressed": ".zip",
    "application/vnd.oasis.opendocument.text": ".odt",
    "application/vnd.oasis.opendocument.spreadsheet": ".ods",
    "application/vnd.oasis.opendocument.presentation": ".odp",
    "application/rtf": ".rtf",
    "text/rtf": ".rtf",
    "text/html": ".html",
    "application/xhtml+xml": ".html",
    "multipart/related": ".mhtml",
    "application/x-mimearchive": ".mhtml",
    "text/markdown": ".md",
    "text/x-markdown": ".md",
    "text/plain": ".txt",
    "application/x-ipynb+json": ".ipynb",
    "text/csv": ".csv",
    "text/tab-separated-values": ".tsv",
    "text/vtt": ".vtt",
    "text/asciidoc": ".adoc",
    "application/x-tex": ".tex",
    "text/x-tex": ".tex",
    "application/xml": ".xml",
    "text/xml": ".xml",
    "application/rss+xml": ".rss",
    "application/atom+xml": ".atom",
    "application/json": ".json",
    "image/svg+xml": ".svg",
    "image/png": ".png",
    "image/jpeg": ".jpg",
    "image/gif": ".gif",
    "image/bmp": ".bmp",
    "image/tiff": ".tiff",
    "image/webp": ".webp",
    "audio/mpeg": ".mp3",
    "audio/wav": ".wav",
    "audio/x-wav": ".wav",
    "audio/mp4": ".m4a",
    "audio/x-m4a": ".m4a",
    "video/mp4": ".mp4",
}

#: The extension the facade routes an engine-detected format id (#672) as.
_ENGINE_EXTENSIONS = {
    "pages": ".pages", "numbers": ".numbers", "keynote": ".key", "djvu": ".djvu",
    "wordperfect": ".wpd", "abiword": ".abw", "video": ".mp4", "audio": ".mp3",
    "vtt": ".vtt", "email": ".eml", "mhtml": ".mhtml", "latex": ".tex", "svg": ".svg",
    "doctags": ".doctags", "ipynb": ".ipynb", "epub": ".epub", "visio": ".vsdx",
    "odt": ".odt", "ods": ".ods", "odp": ".odp", "doc": ".doc", "xls": ".xls", "ppt": ".ppt",
}

_TEXT_EXTENSIONS = {
    ".txt", ".md", ".markdown", ".text", ".qmd", ".rmd", ".csv", ".tsv", ".html", ".htm",
    ".xhtml", ".xml", ".json", ".ipynb", ".vtt", ".adoc", ".asciidoc", ".asc", ".tex", ".latex", ".svg",
    ".rss", ".atom", ".rdf",
}
_IMAGE_EXTENSIONS = {".jpg", ".jpeg", ".png", ".gif", ".bmp", ".tif", ".tiff", ".webp", ".heic", ".heif"}
_AUDIO_EXTENSIONS = {".mp3", ".wav", ".m4a", ".aac", ".flac", ".ogg", ".mp4", ".mov", ".mkv", ".webm"}


def _normalize_ext(ext: Optional[str]) -> Optional[str]:
    if not ext:
        return None
    ext = ext.strip().lower()
    return ext if ext.startswith(".") else "." + ext


def _ext_from_mime(mimetype: Optional[str]) -> Optional[str]:
    if not mimetype:
        return None
    mt = mimetype.split(";")[0].strip().lower()
    return _MIME_EXTENSIONS.get(mt) or _normalize_ext(mimetypes.guess_extension(mt))


def _ext_from_url(url: Optional[str]) -> Optional[str]:
    if not url:
        return None
    return _normalize_ext(os.path.splitext(unquote(urlparse(url).path))[1])


def _cfb_names(data: bytes) -> Iterable[str]:
    """The stream names a Compound File Binary (OLE2) directory holds that
    tell its kind apart, found as their UTF-16LE spelling — the directory
    sectors can sit anywhere, so the whole buffer is searched."""
    for name in ("__substg1.0_", "WordDocument", "Workbook", "Book", "PowerPoint Document"):
        if name.encode("utf-16-le") in data:
            yield name


def _ooxml_main_part(zf: zipfile.ZipFile, names: set) -> Optional[str]:
    """The target of ``_rels/.rels``'s ``officeDocument`` relationship, as a
    package path, when that member exists (#1236 — OPC names the main part
    by relationship, not by a fixed name)."""
    if "_rels/.rels" not in names:
        return None
    import xml.etree.ElementTree as ET

    try:
        root = ET.fromstring(zf.read("_rels/.rels")[: 1 << 20])
    except (ET.ParseError, KeyError, zipfile.BadZipFile):
        return None
    for rel in root.iter():
        if rel.tag.endswith("Relationship") and rel.get("Type", "").endswith("/officeDocument"):
            parts = []
            for seg in rel.get("Target", "").split("/"):
                if seg == "..":
                    if parts:
                        parts.pop()
                elif seg not in ("", "."):
                    parts.append(seg)
            target = "/".join(parts)
            return target if target in names else None
    return None


def sniff_extension(data: bytes) -> Optional[str]:
    """The extension a buffer's content says it is, or ``None`` — the
    no-hint path of ``convert_stream`` (MarkItDown asks magika; this reads
    magic bytes and container structure)."""
    head = data[:2048]
    if head.startswith(b"%PDF-") or b"%PDF-" in head[:1024]:
        return ".pdf"
    if head.startswith(b"PK\x03\x04") or head.startswith(b"PK\x05\x06"):
        try:
            with zipfile.ZipFile(io.BytesIO(data)) as zf:
                names = set(zf.namelist())
                if "mimetype" in names:
                    mt = zf.read("mimetype").decode("ascii", "replace").strip()
                    if mt == "application/epub+zip":
                        return ".epub"
                    if mt in _MIME_EXTENSIONS:
                        return _MIME_EXTENSIONS[mt]
                if "word/document.xml" in names:
                    return ".docx"
                if "ppt/presentation.xml" in names:
                    return ".pptx"
                if "xl/workbook.xml" in names:
                    return ".xlsx"
                if "xl/workbook.bin" in names:
                    return ".xlsb"
                # An OOXML main part under another name (#1236:
                # `word/document2.xml`): the root relationships name it.
                main = _ooxml_main_part(zf, names)
                if main:
                    top = main.split("/", 1)[0]
                    ext = {"word": ".docx", "ppt": ".pptx", "xl": ".xlsx"}.get(top)
                    if ext:
                        return ext
        except zipfile.BadZipFile:
            return None
        return ".zip"
    if head.startswith(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"):
        names = set(_cfb_names(data))
        if "__substg1.0_" in names:
            return ".msg"
        if "WordDocument" in names:
            return ".doc"
        if "PowerPoint Document" in names:
            return ".ppt"
        if names & {"Workbook", "Book"}:
            return ".xls"
        return None
    if head.startswith(b"\x89PNG\r\n\x1a\n"):
        return ".png"
    if head.startswith(b"\xff\xd8\xff"):
        return ".jpg"
    if head[:6] in (b"GIF87a", b"GIF89a"):
        return ".gif"
    if head[:4] in (b"II*\x00", b"MM\x00*"):
        return ".tiff"
    # "BM" alone also starts text ("BMW,…"): require BMP's zero reserved
    # fields and a known DIB header size.
    if head.startswith(b"BM") and head[6:10] == b"\0\0\0\0" and head[14:18] in (
        b"\x0c\0\0\0", b"(\0\0\0", b"4\0\0\0", b"8\0\0\0", b"l\0\0\0", b"|\0\0\0",
    ):
        return ".bmp"
    if head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return ".webp"
    if head[:4] == b"RIFF" and head[8:12] == b"WAVE":
        return ".wav"
    if head.startswith(b"ID3") or head[:2] in (b"\xff\xfb", b"\xff\xf3", b"\xff\xf2"):
        return ".mp3"
    if head[4:8] == b"ftyp":
        return ".m4a" if head[8:11] == b"M4A" else ".mp4"
    if head.startswith(b"{\\rtf"):
        return ".rtf"
    # The engine's own detection (#672) for the rest of the binary and
    # markup signatures: iWork, DjVu, WordPerfect, video, WebVTT, a message.
    engine = _ENGINE_EXTENSIONS.get(_detect_format(data))
    if engine is not None:
        return engine
    # Text: decodable as UTF-8 (BOM allowed) or at least mostly printable.
    try:
        text = head.decode("utf-8-sig")
    except UnicodeDecodeError as e:
        # A multi-byte character cut by the 2 KiB window is still UTF-8.
        if e.start < len(head) - 4:
            if sum(b < 9 or 13 < b < 32 for b in head) > len(head) // 20:
                return None
        text = head.decode("utf-8", "ignore")
    lowered = text.lstrip().lower()
    if lowered.startswith(("<!doctype html", "<html")) or "<html" in lowered[:512]:
        return ".html"
    if lowered.startswith("<?xml") or lowered.startswith("<"):
        return ".html" if "<body" in lowered or "<div" in lowered else ".xml"
    if lowered.startswith("{\\rtf"):
        return ".rtf"
    # CSV: the same non-zero delimiter count on every line of the sample (the
    # last line of a cut sample may be partial, so it does not vote).
    lines = [ln for ln in text.splitlines() if ln.strip()][:10]
    if len(data) > len(head):
        lines = lines[:-1]
    if len(lines) >= 2:
        for delim in (",", "\t", ";"):
            counts = [ln.count(delim) for ln in lines]
            if counts[0] > 0 and all(c == counts[0] for c in counts):
                return ".tsv" if delim == "\t" else ".csv"
    return ".txt"


def _content_disposition_filename(value: str) -> Optional[str]:
    msg = email.message.Message()
    msg["Content-Disposition"] = value
    return msg.get_filename() or None


def _file_uri_to_path(uri: str) -> Tuple[Optional[str], str]:
    parsed = urlparse(uri)
    if parsed.scheme != "file":
        raise ValueError(f"Not a file URL: {uri}")
    decoded = unquote_to_bytes(parsed.path).replace(b"\\", b"/")
    if decoded.startswith(b"//"):
        raise ValueError(f"Unsupported file URI: {uri}. UNC and Windows device paths are not supported.")
    path = url2pathname(parsed.path)
    if os.name == "nt" and path[:1] in "/\\" and path[2:3] == ":":
        path = path[1:]
    return (parsed.netloc or None), os.path.abspath(path)


def _parse_data_uri(uri: str) -> Tuple[Optional[str], Dict[str, str], bytes]:
    if uri[:5].lower() != "data:":
        raise ValueError("Not a data URI")
    header, sep, data = uri.partition(",")
    if not sep:
        raise ValueError("Malformed data URI, missing ',' separator")
    parts = header[5:].split(";")
    is_base64 = parts[-1].lower() == "base64"
    if is_base64:
        parts.pop()
    mime_type = parts.pop(0) if parts and parts[0] else None
    attributes: Dict[str, str] = {}
    for part in parts:
        if "=" in part:
            key, value = part.split("=", 1)
            attributes[key.lower()] = value
        elif part:
            attributes[part.lower()] = ""
    content = base64.b64decode(data) if is_base64 else unquote_to_bytes(data)
    return mime_type, attributes, content


class _UrllibResponse:
    """The slice of ``requests.Response`` ``convert_response`` reads, for
    fetches made without a ``requests_session``."""

    def __init__(self, url: str, headers: Dict[str, str], content: bytes):
        self.url = url
        self.headers = headers
        self.content = content


def _fetch(uri: str, session: Any) -> Any:
    if session is not None:
        response = session.get(uri, stream=True)
        response.raise_for_status()
        return response
    import urllib.request

    from . import __version__

    req = urllib.request.Request(uri, headers={"User-Agent": f"docling-rs/{__version__} (MarkItDown facade)"})
    with urllib.request.urlopen(req) as resp:
        headers = {k.lower(): v for k, v in resp.headers.items()}
        return _UrllibResponse(resp.geturl(), headers, resp.read())


def _is_response(obj: Any) -> bool:
    try:
        import requests  # optional, as in MarkItDown's own API surface

        if isinstance(obj, requests.Response):
            return True
    except ImportError:
        pass
    return isinstance(obj, _UrllibResponse) or (
        hasattr(obj, "headers") and hasattr(obj, "content") and hasattr(obj, "url") and not hasattr(obj, "read")
    )


def _header(headers: Any, name: str) -> Optional[str]:
    if headers is None:
        return None
    value = headers.get(name)
    if value is None and isinstance(headers, dict):
        value = {k.lower(): v for k, v in headers.items()}.get(name.lower())
    return value


# ---- LLM picture captions (#670) -------------------------------------------


def _llm_caption(client: Any, model: str, prompt: Optional[str], data_uri: str) -> str:
    """MarkItDown's ``llm_caption`` request: one user message with the prompt
    and the picture as an ``image_url``; the reply's first choice."""
    messages = [
        {
            "role": "user",
            "content": [
                {"type": "text", "text": prompt or DEFAULT_LLM_PROMPT},
                {"type": "image_url", "image_url": {"url": data_uri}},
            ],
        }
    ]
    response = client.chat.completions.create(model=model, messages=messages)
    return (response.choices[0].message.content or "").strip()


@dataclass
class _LlmClientDescriber(PictureDescriptionBaseOptions):
    """Picture description with an OpenAI-style ``llm_client``. Every picture
    is described (MarkItDown captions them all), so no size threshold."""

    kind: ClassVar[str] = "llm"

    client: Any = None
    model: Optional[str] = None
    prompt: Optional[str] = None
    picture_area_threshold: float = 0.0

    @property
    def provenance_label(self) -> str:
        return f"llm-{self.model}" if self.model else "llm"

    def _annotate_images(self, images: List[Any]) -> Iterable[str]:
        for image in images:
            buffered = io.BytesIO()
            image.save(buffered, format="PNG")
            uri = "data:image/png;base64," + base64.b64encode(buffered.getvalue()).decode("ascii")
            try:
                yield _llm_caption(self.client, self.model, self.prompt, uri)
            except Exception as e:  # a failed caption leaves the picture undescribed
                warnings.warn(f"llm_client could not describe a picture: {e}", stacklevel=4)
                yield ""


# ---- the facade -----------------------------------------------------------

# Constructor options MarkItDown has and docling.rs does not act on.
_IGNORED_OPTIONS = {
    "exiftool_path": "EXIF metadata is not extracted yet (#679)",
    "style_map": "DOCX is read by docling.rs's own backend, not mammoth",
    "docintel_endpoint": "Azure Document Intelligence is not used; layout and OCR run locally",
    "docintel_credential": "Azure Document Intelligence is not used",
    "docintel_file_types": "Azure Document Intelligence is not used",
    "docintel_api_version": "Azure Document Intelligence is not used",
    "cu_endpoint": "Azure Content Understanding is not used; layout and OCR run locally",
    "cu_credential": "Azure Content Understanding is not used",
    "cu_analyzer_id": "Azure Content Understanding is not used",
    "cu_file_types": "Azure Content Understanding is not used",
}
_INIT_OPTIONS = {"requests_session", "llm_client", "llm_model", "llm_prompt", "docling_options"}
# Per-call keyword arguments the facade reads.
_CALL_OPTIONS = {"keep_data_uris", "llm_client", "llm_model", "llm_prompt"}

_UNSUPPORTED_MARKERS = (
    "could not determine input format",
    "cannot detect input format",
    "no backend implemented yet",
)

_text_layer_warned = False


class MarkItDown:
    """Microsoft MarkItDown's ``MarkItDown`` class, converting with docling.rs.
    See the module docstring for what is supported and what differs."""

    def __init__(
        self,
        *,
        enable_builtins: Union[None, bool] = None,
        enable_plugins: Union[None, bool] = None,
        **kwargs: Any,
    ):
        self._requests_session = None
        self._llm_client: Any = None
        self._llm_model: Optional[str] = None
        self._llm_prompt: Optional[str] = None
        # MarkItDown writes a picture's alt text (`![alt](…)`): render it
        # (#676) unless the caller's docling_options say otherwise.
        self._docling_options: Dict[str, Any] = {"picture_alt_text": True, "metadata": "json"}
        self._converters: Dict[bool, DocumentConverter] = {}
        # MarkItDown converters registered with register_converter (#680):
        # (priority, -registration sequence, converter).
        self._md_converters: List[Tuple[float, int, Any]] = []
        self._plugins_enabled = False
        if enable_builtins is False:
            warnings.warn(
                "enable_builtins=False has no effect: docling.rs has no separate built-in "
                "converters to disable",
                stacklevel=2,
            )
        self.enable_builtins(**kwargs)
        if enable_plugins:
            self.enable_plugins()

    def enable_builtins(self, **kwargs: Any) -> None:
        """MarkItDown's ``enable_builtins``: takes the converter options
        (``llm_client``, ``llm_model``, ``llm_prompt``, ``requests_session``,
        plus docling.rs's ``docling_options``). Options docling.rs does not
        act on, and unknown ones, warn."""
        for key in kwargs:
            if key in _IGNORED_OPTIONS:
                warnings.warn(f"MarkItDown option {key!r} is ignored: {_IGNORED_OPTIONS[key]}", stacklevel=3)
            elif key not in _INIT_OPTIONS:
                warnings.warn(f"unknown MarkItDown option {key!r} is ignored", stacklevel=3)
        if "requests_session" in kwargs:
            self._requests_session = kwargs["requests_session"]
        self._llm_client = kwargs.get("llm_client", self._llm_client)
        self._llm_model = kwargs.get("llm_model", self._llm_model)
        self._llm_prompt = kwargs.get("llm_prompt", self._llm_prompt)
        if "docling_options" in kwargs:
            self._docling_options = {"picture_alt_text": True, "metadata": "json", **(kwargs["docling_options"] or {})}
            self._converters.clear()

    def enable_plugins(self, **kwargs: Any) -> None:
        """MarkItDown's ``enable_plugins`` (#680): load every installed
        ``markitdown.plugin`` entry point and call its
        ``register_converters(self, **kwargs)``. A plugin that fails to load
        warns and is skipped. Once per instance, as in MarkItDown."""
        if self._plugins_enabled:
            warnings.warn("plugins are already enabled", stacklevel=2)
            return
        self._plugins_enabled = True
        from importlib.metadata import entry_points

        eps = entry_points()
        group = eps.select(group="markitdown.plugin") if hasattr(eps, "select") else eps.get("markitdown.plugin", [])
        for ep in group:
            try:
                plugin = ep.load()
                plugin.register_converters(self, **kwargs)
            except Exception as e:  # MarkItDown warns and goes on, too
                warnings.warn(f"plugin {ep.name!r} failed to load: {e}", stacklevel=2)

    def register_converter(self, converter: Any, *, priority: float = PRIORITY_SPECIFIC_FILE_FORMAT) -> None:
        """MarkItDown's ``register_converter`` (#680): ``converter`` has
        ``accepts(file_stream, stream_info, **kwargs) -> bool`` and
        ``convert(file_stream, stream_info, **kwargs)`` returning a result
        with ``.markdown`` (and ``.title``). Lower ``priority`` is tried
        first and the later registration first among equals; at or below
        ``PRIORITY_SPECIFIC_FILE_FORMAT`` it runs before docling.rs's engine,
        above it only when the engine finds no format for the input or fails
        on it."""
        if not (hasattr(converter, "accepts") and hasattr(converter, "convert")):
            raise TypeError("register_converter needs an object with accepts() and convert()")
        self._md_converters.append((float(priority), -len(self._md_converters), converter))
        self._md_converters.sort(key=lambda entry: (entry[0], entry[1]))

    def _run_converters(
        self,
        before_engine: bool,
        info: StreamInfo,
        hinted: Optional[str],
        path: Optional[str],
        data: Optional[bytes],
        call_options: Dict[str, Any],
    ) -> Optional[DocumentConverterResult]:
        """The registered MarkItDown converters on one side of the engine:
        the first that accepts the input and converts it wins; converters
        that raise are recorded and the next is tried, as in MarkItDown."""
        chosen = [c for p, _, c in self._md_converters if (p <= PRIORITY_SPECIFIC_FILE_FORMAT) == before_engine]
        if not chosen:
            return None
        if data is None:
            with open(path, "rb") as fh:
                data = fh.read()
        if hinted and not info.extension:
            info = info.copy_and_update(extension=hinted)
        kwargs = {
            k: v
            for k, v in {
                "llm_client": self._llm_client,
                "llm_model": self._llm_model,
                "llm_prompt": self._llm_prompt,
                **call_options,
            }.items()
            if v is not None
        }
        failures = []
        for converter in chosen:
            stream = io.BytesIO(data)
            try:
                accepted = converter.accepts(stream, info, **kwargs)
            finally:
                stream.seek(0)
            if not accepted:
                continue
            try:
                out = converter.convert(stream, info, **kwargs)
            except Exception:
                failures.append(FailedConversionAttempt(converter, sys.exc_info()))
                continue
            if out is not None:
                return DocumentConverterResult(out.markdown, title=getattr(out, "title", None))
        if failures:
            raise FileConversionException(
                f"{info.filename or 'stream'}: every registered converter that accepted it failed",
                attempts=failures,
            )
        return None

    def register_page_converter(self, converter: Any) -> None:
        """MarkItDown's deprecated alias of :meth:`register_converter`."""
        self.register_converter(converter)

    # -- entry points (MarkItDown's dispatch) --

    def convert(
        self,
        source: Union[str, "os.PathLike[str]", BinaryIO, Any],
        *,
        stream_info: Optional[StreamInfo] = None,
        **kwargs: Any,
    ) -> DocumentConverterResult:
        """Convert a local path (``str`` / ``Path``), an ``http(s)``/``file``/
        ``data`` URI, a ``requests.Response`` or a binary stream."""
        if isinstance(source, str):
            scheme = urlparse(source.strip()).scheme.lower()
            if scheme in ("http", "https", "file", "data"):
                _kwargs = dict(kwargs)
                if "url" in _kwargs:
                    _kwargs["mock_url"] = _kwargs.pop("url")
                return self.convert_uri(source, stream_info=stream_info, **_kwargs)
            return self.convert_local(source, stream_info=stream_info, **kwargs)
        if isinstance(source, os.PathLike):
            return self.convert_local(source, stream_info=stream_info, **kwargs)
        if _is_response(source):
            return self.convert_response(source, stream_info=stream_info, **kwargs)
        if hasattr(source, "read") and callable(source.read) and not isinstance(source, io.TextIOBase):
            return self.convert_stream(source, stream_info=stream_info, **kwargs)
        raise TypeError(f"Invalid source type: {type(source)}. Expected str, requests.Response, BinaryIO.")

    def convert_local(
        self,
        path: Union[str, "os.PathLike[str]"],
        *,
        stream_info: Optional[StreamInfo] = None,
        file_extension: Optional[str] = None,
        url: Optional[str] = None,
        **kwargs: Any,
    ) -> DocumentConverterResult:
        path = os.fspath(path)
        info = StreamInfo(
            local_path=path,
            extension=os.path.splitext(path)[1] or None,
            filename=os.path.basename(path),
        )
        if stream_info is not None:
            info = info.copy_and_update(stream_info)
        if file_extension is not None:
            info = info.copy_and_update(extension=file_extension)
        if url is not None:
            info = info.copy_and_update(url=url)
        return self._convert(info, path=path, call_options=kwargs)

    def convert_stream(
        self,
        stream: BinaryIO,
        *,
        stream_info: Optional[StreamInfo] = None,
        file_extension: Optional[str] = None,
        url: Optional[str] = None,
        **kwargs: Any,
    ) -> DocumentConverterResult:
        """Convert the stream's content from its current position (as
        MarkItDown, which never rewinds it)."""
        info = stream_info or StreamInfo()
        if file_extension is not None:
            info = info.copy_and_update(extension=file_extension)
        if url is not None:
            info = info.copy_and_update(url=url)
        data = stream.read()
        if isinstance(data, str):
            raise TypeError("convert_stream needs a binary stream, got a text stream")
        return self._convert(info, data=bytes(data), call_options=kwargs)

    def convert_url(
        self,
        url: str,
        *,
        stream_info: Optional[StreamInfo] = None,
        file_extension: Optional[str] = None,
        mock_url: Optional[str] = None,
        **kwargs: Any,
    ) -> DocumentConverterResult:
        """Alias of :meth:`convert_uri` (as in MarkItDown)."""
        return self.convert_uri(url, stream_info=stream_info, file_extension=file_extension, mock_url=mock_url, **kwargs)

    def convert_uri(
        self,
        uri: str,
        *,
        stream_info: Optional[StreamInfo] = None,
        file_extension: Optional[str] = None,
        mock_url: Optional[str] = None,
        **kwargs: Any,
    ) -> DocumentConverterResult:
        """Convert a ``file:``, ``data:`` or ``http(s):`` URI. ``http(s)``
        goes through the ``requests_session`` when one was given, else
        ``urllib``."""
        uri = uri.strip()
        scheme = urlparse(uri).scheme.lower()
        if scheme == "file":
            netloc, path = _file_uri_to_path(uri)
            if netloc and netloc != "localhost":
                raise ValueError(f"Unsupported file URI: {uri}. Netloc must be empty or localhost.")
            return self.convert_local(path, stream_info=stream_info, file_extension=file_extension, url=mock_url, **kwargs)
        if scheme == "data":
            mimetype, attributes, data = _parse_data_uri(uri)
            info = StreamInfo(mimetype=mimetype, charset=attributes.get("charset"))
            if stream_info is not None:
                info = info.copy_and_update(stream_info)
            return self.convert_stream(io.BytesIO(data), stream_info=info, file_extension=file_extension, url=mock_url, **kwargs)
        if scheme in ("http", "https"):
            response = _fetch(uri, self._requests_session)
            return self.convert_response(
                response, stream_info=stream_info, file_extension=file_extension, url=mock_url, **kwargs
            )
        raise ValueError(
            f"Unsupported URI scheme: {uri.split(':')[0]}. Supported schemes are: file:, data:, http:, https:"
        )

    def convert_response(
        self,
        response: Any,
        *,
        stream_info: Optional[StreamInfo] = None,
        file_extension: Optional[str] = None,
        url: Optional[str] = None,
        **kwargs: Any,
    ) -> DocumentConverterResult:
        """Convert a ``requests.Response``: ``Content-Type`` gives the media
        type and charset, ``Content-Disposition`` the file name, the URL path
        the extension."""
        headers = getattr(response, "headers", None)
        mimetype = charset = None
        content_type = _header(headers, "content-type")
        if content_type:
            parts = content_type.split(";")
            mimetype = parts.pop(0).strip() or None
            for part in parts:
                if part.strip().lower().startswith("charset="):
                    charset = part.split("=", 1)[1].strip().strip('"') or None
        filename = extension = None
        disposition = _header(headers, "content-disposition")
        if disposition:
            filename = _content_disposition_filename(disposition)
            if filename:
                extension = os.path.splitext(filename)[1] or None
        response_url = getattr(response, "url", None)
        if extension is None:
            extension = _ext_from_url(response_url)
        info = StreamInfo(mimetype=mimetype, extension=extension, charset=charset, filename=filename, url=response_url)
        if stream_info is not None:
            info = info.copy_and_update(stream_info)
        if file_extension is not None:
            info = info.copy_and_update(extension=file_extension)
        if url is not None:
            info = info.copy_and_update(url=url)
        return self._convert(info, data=bytes(response.content), call_options=kwargs)

    # -- the conversion --

    def _converter(self, text_layer_only: bool = False) -> DocumentConverter:
        if text_layer_only not in self._converters:
            opts = dict(self._docling_options)
            if text_layer_only:
                opts["text_layer_only"] = True
            self._converters[text_layer_only] = DocumentConverter(**opts)
        return self._converters[text_layer_only]

    def _convert(
        self,
        info: StreamInfo,
        *,
        path: Optional[str] = None,
        data: Optional[bytes] = None,
        call_options: Dict[str, Any],
    ) -> DocumentConverterResult:
        for key in call_options:
            if key not in _CALL_OPTIONS and key not in _IGNORED_OPTIONS:
                warnings.warn(f"unknown MarkItDown convert() option {key!r} is ignored", stacklevel=4)
        keep_data_uris = bool(call_options.get("keep_data_uris", False))
        llm_client = call_options.get("llm_client", self._llm_client)
        llm_model = call_options.get("llm_model", self._llm_model)
        llm_prompt = call_options.get("llm_prompt", self._llm_prompt)

        # The extension to route on: the explicit hint, then the file name /
        # URL / media type, then the content itself.
        hinted = (
            _normalize_ext(info.extension)
            or _normalize_ext(os.path.splitext(info.filename or "")[1])
            or _ext_from_url(info.url)
            or _ext_from_mime(info.mimetype)
        )
        # Registered MarkItDown converters (#680): the specific ones first …
        result = self._run_converters(True, info, hinted, path, data, call_options)
        if result is not None:
            return result
        try:
            return self._convert_engine(hinted, info, path, data, keep_data_uris, llm_client, llm_model, llm_prompt)
        except (UnsupportedFormatException, FileConversionException):
            # … the generic ones when the engine has no format for the input
            # or fails on it, as MarkItDown goes on to its next converter.
            result = self._run_converters(False, info, hinted, path, data, call_options)
            if result is not None:
                return result
            raise

    def _convert_engine(
        self,
        hinted: Optional[str],
        info: StreamInfo,
        path: Optional[str],
        data: Optional[bytes],
        keep_data_uris: bool,
        llm_client: Any,
        llm_model: Optional[str],
        llm_prompt: Optional[str],
    ) -> DocumentConverterResult:
        try:
            return self._convert_as(hinted, info, path, data, keep_data_uris, llm_client, llm_model, llm_prompt)
        except UnsupportedFormatException:
            # A hint docling.rs cannot route (".bin", "application/octet-stream",
            # a wrong extension): fall back to what the content says it is.
            if hinted is None:
                raise
            if data is None:
                with open(path, "rb") as fh:
                    data = fh.read()
            sniffed = sniff_extension(data)
            if sniffed is None or sniffed == hinted:
                raise
            return self._convert_as(sniffed, info, None, data, keep_data_uris, llm_client, llm_model, llm_prompt)

    def _convert_as(
        self,
        ext: Optional[str],
        info: StreamInfo,
        path: Optional[str],
        data: Optional[bytes],
        keep_data_uris: bool,
        llm_client: Any,
        llm_model: Optional[str],
        llm_prompt: Optional[str],
    ) -> DocumentConverterResult:
        if ext is None:
            if data is None:
                with open(path, "rb") as fh:
                    data = fh.read()
            ext = sniff_extension(data)
        if ext is None:
            raise UnsupportedFormatException(
                f"{info.filename or 'stream'}: could not determine the input format — "
                "pass StreamInfo(extension=…) or mimetype"
            )

        # A text format in another charset: transcode to UTF-8 (with a BOM,
        # which every text backend honors first), so any Python codec name
        # works. The charset is the hint, else — for bytes that are not UTF-8 —
        # charset_normalizer's guess when it is installed (it ships with
        # requests); else the engine guesses itself (chardetng, #675).
        charset = info.charset
        if ext in _TEXT_EXTENSIONS and (charset or path is None or os.path.getsize(path) < 64 << 20):
            raw = data
            if raw is None:
                with open(path, "rb") as fh:
                    raw = fh.read()
            if charset is None and not _is_utf8(raw):
                charset = _detect_charset(raw)
        else:
            charset = None
        if charset:
            data = raw
            try:
                text = data.decode(charset)
                data = ("\ufeff" + text.lstrip("\ufeff")).encode("utf-8")
            except (UnicodeDecodeError, LookupError) as e:
                raise FileConversionException(
                    f"could not decode the input as {charset!r}: {e}. The charset may be wrong; "
                    "retry with the right StreamInfo(charset=…)",
                    attempts=[FailedConversionAttempt(self, sys.exc_info())],
                ) from e

        stem = Path(info.filename or (os.path.basename(path) if path else "stream")).stem or "stream"
        name = stem + ext
        if ext == ".zip":
            if data is None:
                with open(path, "rb") as fh:
                    data = fh.read()
            return self._convert_zip(info.filename or name, data, keep_data_uris)

        result = self._run(name, ext, path, data, info)
        document = result.document
        is_image = ext in _IMAGE_EXTENSIONS
        if llm_client is not None and llm_model and not is_image:
            describe_pictures(document, _LlmClientDescriber(client=llm_client, model=llm_model, prompt=llm_prompt))

        markdown = _export(document, keep_data_uris)
        block = _metadata_block(ext, getattr(result, "metadata", None))
        if block:
            if ext in _AUDIO_EXTENSIONS:
                transcript = markdown.strip()
                markdown = block + ("\n\n### Audio Transcript:\n" + transcript + "\n" if transcript else "\n")
            else:
                body = markdown.lstrip()
                markdown = block + ("\n\n" + body if body.strip() else "\n")
        if is_image and llm_client is not None and llm_model:
            if data is None:
                with open(path, "rb") as fh:
                    data = fh.read()
            mimetype = info.mimetype or mimetypes.guess_type("_" + ext)[0] or "application/octet-stream"
            uri = f"data:{mimetype};base64," + base64.b64encode(data).decode("ascii")
            try:
                caption = _llm_caption(llm_client, llm_model, llm_prompt, uri)
            except Exception as e:
                warnings.warn(f"llm_client could not describe the image: {e}", stacklevel=3)
                caption = ""
            if caption:
                markdown = markdown.rstrip() + "\n\n# Description:\n" + caption + "\n"
        title = _title(document)
        if _youtube_video_id(info.url) and title == "YouTube":
            # MarkItDown's title is the video's, not the `# YouTube` head.
            title = _section_after_title(document)
        return DocumentConverterResult(markdown, title=title, document=document)

    def _run(self, name: str, ext: str, path: Optional[str], data: Optional[bytes], info: StreamInfo) -> Any:
        """One engine conversion, with MarkItDown's exceptions; a PDF whose
        models are missing degrades to its text layer (see the module docs)."""
        global _text_layer_warned
        own_name = path is not None and data is None and _normalize_ext(os.path.splitext(path)[1]) == ext
        try:
            try:
                if own_name:
                    result = self._converter().convert(path)
                else:
                    if data is None:
                        with open(path, "rb") as fh:
                            data = fh.read()
                    result = self._converter().convert_bytes(
                        name, data, base_url=_http_url(info.url), media_type=info.mimetype
                    )
            except ConversionError as e:
                if ext != ".pdf" or "model not found" not in str(e):
                    raise
                if not _text_layer_warned:
                    _text_layer_warned = True
                    warnings.warn(
                        "the PDF layout models are not installed, so PDFs convert from their embedded "
                        "text layer only (no tables, headings or OCR) — run docling_rs.download_models() "
                        "for the full pipeline",
                        stacklevel=4,
                    )
                if own_name:
                    result = self._converter(text_layer_only=True).convert(path)
                else:
                    result = self._converter(text_layer_only=True).convert_bytes(
                        name, data, base_url=_http_url(info.url), media_type=info.mimetype
                    )
        except ConversionError as e:
            message = str(e)
            if any(m in message.lower() for m in _UNSUPPORTED_MARKERS):
                raise UnsupportedFormatException(f"{info.filename or name}: {message}") from e
            raise FileConversionException(
                f"{info.filename or name}: {message}",
                attempts=[FailedConversionAttempt(self, sys.exc_info())],
            ) from e
        if result.status == "failure":
            raise FileConversionException(f"{info.filename or name}: conversion failed")
        return result

    def _convert_zip(self, filename: str, data: bytes, keep_data_uris: bool) -> DocumentConverterResult:
        """MarkItDown's ZIP output — each convertible entry under a ``## File:``
        heading; entries docling.rs cannot convert are skipped, as there."""
        md = f"Content from the zip file `{filename}`:\n\n"
        try:
            items = self._converter().convert_archive(data)
            for item in items:
                if item.result is None:
                    continue
                md += f"## File: {item.path}\n\n" + _export(item.result.document, keep_data_uris) + "\n\n"
        except ConversionError as e:
            raise FileConversionException(
                f"{filename}: {e}", attempts=[FailedConversionAttempt(self, sys.exc_info())]
            ) from e
        return DocumentConverterResult(md.strip())


def _is_utf8(data: bytes) -> bool:
    if data.startswith((b"\xef\xbb\xbf", b"\xff\xfe", b"\xfe\xff")):
        return True  # a BOM: the engine reads it
    try:
        data.decode("utf-8")
        return True
    except UnicodeDecodeError:
        return False


def _detect_charset(data: bytes) -> Optional[str]:
    try:
        import charset_normalizer
    except ImportError:
        return None
    best = charset_normalizer.from_bytes(data).best()
    return best.encoding if best is not None else None


_COMPACT_TABLES: Optional[bool] = None


def _metadata_block(ext: Optional[str], metadata: Optional[Dict[str, Any]]) -> str:
    """MarkItDown's metadata lines for an EPUB (``**Title:** …`` from the
    OPF), an image (exiftool's ``ImageSize``, ``Title``, ``Description``,
    ``Keywords``, ``Artist``, ``Author``, ``DateTimeOriginal``,
    ``CreateDate``, ``GPSPosition``) or an audio file (``Title``,
    ``Artist``, ``Band``, ``Album``, ``Genre``, ``Track``,
    ``DateTimeOriginal``, ``NumChannels``, ``SampleRate``) — from the
    engine's own metadata (#679), only the fields the file has. Empty for
    every other format, and for a file without any."""
    if not metadata:
        return ""
    join = lambda items: ", ".join(items) if items else None  # noqa: E731
    if ext == ".epub":
        fields = [
            ("Title", metadata.get("title")),
            ("Authors", join(metadata.get("authors"))),
            ("Language", metadata.get("language")),
            ("Publisher", metadata.get("publisher")),
            ("Date", metadata.get("date")),
            ("Description", metadata.get("description")),
            ("Identifier", metadata.get("identifier")),
        ]
        return "\n".join(f"**{k}:** {v}" for k, v in fields if v)
    if ext in _IMAGE_EXTENSIONS:
        exif = metadata.get("exif") or {}
        gps = metadata.get("gps")
        fields = [
            ("ImageSize", (metadata.get("properties") or {}).get("image_size")),
            ("Title", metadata.get("title")),
            ("Description", exif.get("ImageDescription")),
            ("Keywords", join(metadata.get("keywords"))),
            ("Artist", exif.get("Artist")),
            ("Author", exif.get("XPAuthor")),
            ("DateTimeOriginal", exif.get("DateTimeOriginal")),
            ("CreateDate", exif.get("DateTimeDigitized")),
            ("GPSPosition", f"{gps['latitude']}, {gps['longitude']}" if gps else None),
        ]
        return "\n".join(f"{k}: {v}" for k, v in fields if v)
    if ext in _AUDIO_EXTENSIONS:
        audio = metadata.get("audio") or {}
        fields = [
            ("Title", audio.get("title")),
            ("Artist", audio.get("artist")),
            ("Band", audio.get("album_artist")),
            ("Album", audio.get("album")),
            ("Genre", audio.get("genre")),
            ("Track", audio.get("track")),
            ("DateTimeOriginal", audio.get("date")),
            ("NumChannels", audio.get("channels")),
            ("SampleRate", audio.get("sample_rate")),
        ]
        return "\n".join(f"{k}: {v}" for k, v in fields if v)
    return ""


def _export_options() -> Dict[str, Any]:
    """``compact_tables=True`` when docling-core has it (2.9x): unpadded
    ``| a | b |`` rows, as MarkItDown writes them — and no padding spaces
    spent as LLM tokens."""
    global _COMPACT_TABLES
    if _COMPACT_TABLES is None:
        import inspect

        from docling_core.types.doc import DoclingDocument

        _COMPACT_TABLES = "compact_tables" in inspect.signature(DoclingDocument.export_to_markdown).parameters
    return {"compact_tables": True} if _COMPACT_TABLES else {}


def _export(document: Any, keep_data_uris: bool) -> str:
    """The document's Markdown: docling-core's export, pictures as
    placeholders or (``keep_data_uris``) ``data:`` URIs, compact tables. A
    workbook (every top-level group a sheet, one page each) gets MarkItDown's
    ``## <sheet>`` heading before each sheet — docling's own export names no
    sheet."""
    from docling_core.types.doc import ImageRefMode

    mode = ImageRefMode.EMBEDDED if keep_data_uris else ImageRefMode.PLACEHOLDER
    extra = _export_options()
    children = [getattr(ref, "resolve", lambda _d: None)(document) for ref in document.body.children]
    sheets = [c for c in children if getattr(getattr(c, "label", None), "value", None) == "sheet"]
    if not sheets or len(sheets) != len(children):
        return document.export_to_markdown(image_mode=mode, **extra)
    parts = []
    for sheet in sheets:
        page = next(
            (item.prov[0].page_no for item, _ in document.iterate_items(root=sheet) if getattr(item, "prov", None)),
            None,
        )
        body = document.export_to_markdown(image_mode=mode, page_no=page, **extra).strip() if page is not None else ""
        parts.append(f"## {sheet.name}\n\n{body}".rstrip())
    return "\n\n".join(parts) + "\n"


def _title(document: Any) -> Optional[str]:
    """The document's title item, if it has one."""
    for item in getattr(document, "texts", ()):
        label = getattr(item, "label", None)
        if getattr(label, "value", label) == "title" and getattr(item, "text", ""):
            return item.text
    return None


def _section_after_title(document: Any) -> Optional[str]:
    """The first section header after the title item (a YouTube page's
    video title under ``# YouTube``)."""
    seen_title = False
    for item in getattr(document, "texts", ()):
        label = getattr(item, "label", None)
        label = getattr(label, "value", label)
        if label == "title":
            seen_title = True
        elif seen_title and label == "section_header" and getattr(item, "text", ""):
            return item.text
    return None


def _http_url(url: Optional[str]) -> Optional[str]:
    """``url`` when it is an http(s) URL the bytes were fetched from."""
    if url and url.lower().startswith(("http://", "https://")):
        return url
    return None


def _youtube_video_id(url: Optional[str]) -> Optional[str]:
    """MarkItDown's ``_get_video_id``: the video of a ``youtube.com/watch?v=``,
    ``youtu.be/``, ``/shorts/``, ``/embed/`` or ``/live/`` URL."""
    if not url:
        return None
    parsed = urlparse(url)
    host = parsed.netloc.lower().rsplit("@", 1)[-1].split(":")[0]
    parts = [p for p in parsed.path.split("/") if p]
    if host in {"youtu.be", "www.youtu.be"}:
        return parts[0] if parts else None
    if host == "youtube.com" or host.endswith(".youtube.com"):
        if parts[:1] == ["watch"]:
            return parse_qs(parsed.query).get("v", [None])[0]
        if parts[:1] in (["shorts"], ["embed"], ["live"]):
            return parts[1] if len(parts) > 1 else None
    return None


# ---- CLI: python -m docling_rs.markitdown --------------------------------


def main(argv: Optional[List[str]] = None) -> int:
    """The ``markitdown`` CLI's options: a file (or stdin), ``-o`` output,
    ``-x`` extension / ``-m`` mime-type / ``-c`` charset hints,
    ``--keep-data-uris``."""
    import argparse

    from . import __version__

    parser = argparse.ArgumentParser(
        prog="python -m docling_rs.markitdown",
        description="Convert a document to Markdown with docling.rs (MarkItDown-compatible CLI).",
    )
    parser.add_argument("-v", "--version", action="version", version=f"docling.rs {__version__}")
    parser.add_argument("-o", "--output", help="output file (default: stdout)")
    parser.add_argument("-x", "--extension", help="extension hint for the input, e.g. pdf")
    parser.add_argument("-m", "--mime-type", help="media-type hint for the input")
    parser.add_argument("-c", "--charset", help="charset hint for a text input, e.g. cp932")
    parser.add_argument("--keep-data-uris", action="store_true", help="embed pictures as data: URIs")
    parser.add_argument("filename", nargs="?", help="file or URI to convert (default: stdin)")
    args = parser.parse_args(argv)

    hints = StreamInfo(
        extension=_normalize_ext(args.extension),
        mimetype=args.mime_type,
        charset=args.charset,
    )
    md = MarkItDown()
    if args.filename is None:
        result = md.convert_stream(sys.stdin.buffer, stream_info=hints, keep_data_uris=args.keep_data_uris)
    else:
        result = md.convert(args.filename, stream_info=hints, keep_data_uris=args.keep_data_uris)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as fh:
            fh.write(result.markdown)
    else:
        sys.stdout.write(result.markdown)
        if not result.markdown.endswith("\n"):
            sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
