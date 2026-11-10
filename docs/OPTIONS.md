# Conversion options — one set, every surface

Every way of driving docling.rs — the `docling-rs` CLI, `docling-rs serve` /
`docling-serve` (query string, JSON body or multipart text parts), the
Python `docling_rs.DocumentConverter`, the Node `docling.rs` package, the C
ABI (`docling-ffi`), the wasm module (`convert_with_options`) and the MCP
server's `convert` tool (`options`, #669) — speaks
the same conversion options: the fields of
[`docling::ConvertOptions`](../crates/docling/src/options.rs) (#577).
Validation (`ConvertOptions::validate`) and the mapping onto the converter
(`ConvertOptions::apply`) live there once; the surfaces only parse their
input shape. The engine defaults are defined once too, in
`DocumentConverter::default()`: an option a caller leaves unset means that
default on every surface.

The **wire** column is the JSON key (serve, C ABI, wasm) and the Rust field;
the CLI flag is `--` + the name with `_` → `-`, the Node option is the name
in camelCase, the Python keyword argument is the name itself — except where
the table says otherwise. The inventory test
(`crates/docling/tests/options_inventory.rs`) holds every surface's
documentation to this table, and the table to the struct: an option added
to `ConvertOptions` fails CI until it is a flag, a request option, a kwarg,
a property of the Node option objects and a row here.

What is *not* in the set: how and where to emit the result (`to`, the image
mode, `pandoc_api_version`, the chunker settings). Those depend on what the
surface can do with the output — serve streams, the browser cannot write
files, the CLI writes several formats at once — so each surface keeps its
own output options next to the shared ones.

| Wire name | Values | Default | CLI | Python | Node | Meaning |
|---|---|---|---|---|---|---|
| `strict` | bool | false | `--strict` | — | `strict` | Cleaner, more conformant Markdown instead of the readable default. Python: at export time (`export_to_markdown` reads `document.strict_markdown`). |
| `compact_tables` | bool | false | `--compact-tables` | `compact_tables` | `compactTables` | Unpadded `| a | b |` Markdown tables (#271). |
| `picture_alt_text` | bool | false | `--picture-alt-text` | `picture_alt_text` | `pictureAltText` | Render pictures' author-written alt text (the `descr`/`title` of a DOCX, PPTX or XLSX picture, #676) in Markdown and HTML: the `![alt](…)` of an image line, a text line above a placeholder, the `<img alt>` and the description text in HTML. `strict` implies it for Markdown; the JSON carries it always as the picture's `meta.description` with `created_by: alt_text` — until a description pass (`picture_description`, the picture OCR reader) replaces it with its own. |
| `page_break_placeholder` | string | unset = no breaks | `--page-break-placeholder` | — | `pageBreakPlaceholder` | Text inserted between pages in the Markdown export (docling-core's `MarkdownParams.page_break_placeholder`). serve also accepts the historical `md_page_break_placeholder`; Python: `export_to_markdown(page_break_placeholder=…)`; Node: an output option (`OutputOptions`). |
| `fetch_images` | bool | false | `--fetch-images` | `fetch_images` | `fetchImages` | Resolve external `<img src>` for HTML/EPUB/MHTML/JATS (network access) — the pre-#646 switch, the alias of `image_sources=remote` (`false` = `none`; an explicit `image_sources` wins). serve honours it only under `--allow-url-fetch`. |
| `image_sources` | `none` \| `embedded` \| `local` \| `remote` | `none` | `--image-sources` | `image_sources` | `imageSources` | Which image references resolve (#646): `embedded` = `data:` URIs and parts of the same container (EPUB/MHTML entries, email `cid:` attachments — no filesystem, no network); `local` = plus files under the source file's directory (never an absolute path, never outside it); `remote` = plus `http(s)` fetches. HTML, EPUB, MHTML, JATS, AsciiDoc, ODF, Markdown (`![…](…)`, inline `<img>`, embedded HTML) and email bodies. serve: `remote` without `--allow-url-fetch` is held to `embedded`; `local` needs `--allow-local-images` (400 otherwise). |
| `image_hosts` | list / comma-separated | unset = any host | `--image-hosts` | `image_hosts` | `imageHosts` | Hosts a `remote` image fetch may reach (#646): exact names or `*.suffix` wildcards, case-insensitive; a redirect is held to the same list. The private/loopback block-list applies regardless (`DOCLING_RS_ALLOW_PRIVATE_IP_FETCH` is the only way around it). |
| `max_image_bytes` | bytes > 0 | 32 MiB (`DOCLING_RS_MAX_IMAGE_BYTES`) | `--max-image-bytes` | `max_image_bytes` | `maxImageBytes` | Largest image that resolves (#646); a larger one stays a placeholder. Also the remote fetch's read cap. |
| `max_images` | int ≥ 0 | unlimited (`DOCLING_RS_MAX_IMAGES`) | `--max-images` | `max_images` | `maxImages` | Images resolved per document (#646); the rest stay placeholders, one warning per document says so. |
| `max_image_total_mb` | MiB ≥ 0 | unlimited (`DOCLING_RS_MAX_IMAGE_TOTAL_MB`) | `--max-image-total-mb` | `max_image_total_mb` | `maxImageTotalMb` | Total resolved image bytes per document (#646). |
| `min_image_bytes` | bytes ≥ 0 | 0 (`DOCLING_RS_MIN_IMAGE_BYTES`) | `--min-image-bytes` | `min_image_bytes` | `minImageBytes` | Smallest image that resolves (#646) — spacers and tracking pixels are a few dozen bytes; skipped quietly, no budget charged. |
| `list_attachments` | bool | false | `--list-attachments` | `list_attachments` | `listAttachments` | Email (.eml/.msg): append an Attachments section — names and content types, never the payload (#251). |
| `skip_empty_cells` | bool | false | `--skip-empty-cells` | `skip_empty_cells` | `skipEmptyCells` | Omit empty cells from sparse XLSX/XLS table grids (#271). |
| `ebcdic_layout` | string | unset = the `<stem>.layout.json` sidecar | `--ebcdic-layout` | `ebcdic_layout` | `ebcdicLayout` | EBCDIC copybook layout (#252): inline `EbcdicLayout` JSON or a file path. |
| `encoding` | WHATWG label | unset = detect (BOM, UTF-8, a guess for a non-Latin encoding, else windows-1252) | `--encoding` | `encoding` | `encoding` | Character encoding of text inputs (docling's `TextBackendOptions.encoding`). |
| `metadata` | `none` \| `json` \| `frontmatter` | `none` | `--metadata` | `metadata` | `metadata` | Document metadata (#679): where the file's own metadata goes — an EPUB's Dublin Core, an Office package's `docProps/core.xml` + `app.xml`, an ODF `meta.xml` (flat documents too), a photo's EXIF (JPEG, PNG, TIFF, WebP, HEIF; the GPS position decoded), an audio or video file's tags, a PDF's Info dictionary. `none` reads nothing and leaves every export as it was; `json` adds a `metadata` object to the docling JSON after `origin` (`title`, `authors`, `subject`, `description`, `keywords`, `language`, `publisher`, `identifier`, `rights`, `date`, `created`, `modified`, `last_modified_by`, `creator_tool`, `properties`, `gps`, `exif`, `audio`; empty fields left out) — Python's `result.metadata` — and a docling-JSON input keeps the object it carried; `frontmatter` adds YAML front matter at the top of the Markdown as well (`authors: [Test Author]`, `exif:` / `audio:` as nested maps; never in the plain-text export). `redact_pii` masks the author fields as persons and drops the GPS position (the `kinds` filter applies). |
| `track_changes` | `accept` \| `reject` \| `all` | unset = each reader's own view | `--track-changes` | `track_changes` | `trackChanges` | Tracked changes in Office documents (#851), Pandoc's `--track-changes`: `accept` shows insertions and leaves deletions out, `reject` the reverse, `all` keeps both as typed revisions with their author and date — the Pandoc AST's `Span .insertion` / `.deletion` — and the reviewer comments with them; an explicit `accept` / `reject` leaves the comments out of the Pandoc AST (as Pandoc does), the docling JSON keeps them on the notes layer either way. Unset keeps every reader's view before #851: DOCX and ODT accept, the `.doc` reader every run's text. What each reader implements — anything else is refused (`ConversionError::UnsupportedOption`, a 400 on serve), never answered with another view: **DOCX** `accept` / `reject` / `all` for `w:ins` / `w:del` / `w:moveTo` / `w:moveFrom` runs (a move's destination counts as an insertion, its source as a deletion) in paragraphs, headings, list items and cells, and inserted / deleted table rows (`w:trPr/w:ins|w:del`) under `accept` / `reject`; **DOC** `accept` / `reject` for runs marked `sprmCFRMark` / `sprmCFRMarkDel`; **ODT** `accept`; **HWP** (#216) `accept` / `reject` for HWPX's inline `hp:insertBegin…End` / `hp:deleteBegin…End` marks, HWP 5 `accept` (its text as stored). Not read in any mode, and documented here rather than guessed: paragraph-mark revisions (a deleted paragraph mark does not merge paragraphs), property changes (`w:rPrChange`, `w:pPrChange`), cell revisions (`w:cellIns` / `w:cellDel`), a DOC run's reviewer and date (`all` is refused there), ODT `reject` / `all` (its deletions live out of line in `text:tracked-changes`). Under `all` a DOCX heading's text is the accepted text (headings carry no runs in docling's model) and the Markdown shows a deletion struck through (`~~old~~`). |
| `extract_diagrams` | `mermaid` \| `none` | `mermaid` | `--extract-diagrams` | `extract_diagrams` | `extractDiagrams` | Structured diagrams from DOCX DrawingML drawings (#858): `mermaid` (the default) adds a `mermaid` code item after a drawing's picture with the flowchart its labelled shapes and connectors describe — the edges only from connectors naming their two end shapes (`a:stCxn` / `a:endCxn`), a `tailEnd` arrow as `A --> B`, a `headEnd` arrow as `B --> A`, both as `A <--> B`, none as `A --- B`. A drawing whose lines are laid by eye gets no graph (a `DOCLING_RS_DEBUG` diagnostic says why), never an invented one. The picture stays in the document (its SVG in JSON / HTML, `![…]` in Markdown's embedded and referenced modes); in Markdown's placeholder mode the block takes the `<!-- image -->`'s place, and the fence keeps the `mermaid` tag in the legacy mode too. `none` turns the extraction off. |
| `use_outline_level_for_headings` | bool | `true` | `--no-outline-level-headings` | `use_outline_level_for_headings` | `useOutlineLevelForHeadings` | Word documents (DOCX, DOC): a paragraph style's outline level (`w:outlineLvl`; the `.doc` `sprmPOutLvl`) alone makes it a heading — docling's `MsWordBackendOptions.use_outline_level_for_headings` (docling#4480). It is how a localized or custom heading style ("Nadpis1", "Rubrik 1") is recognized; `false` stops it for templates whose body styles carry an outline level only for the table of contents (numbered legal clauses), which otherwise all become headings. A style named a heading (its id, name or `basedOn` chain) keeps taking its level from the outline either way. |
| `ipynb_outputs` | `none` \| `text` | `none` | `--ipynb-outputs` | `ipynb_outputs` | `ipynbOutputs` | Jupyter notebooks (#673): which cell outputs the document keeps. `none` reads the notebook as written (markdown cells as Markdown, code cells as code items in the kernel's language, raw cells as code blocks — MarkItDown's reading); `text` keeps stream and `text/plain` results as code blocks after their cell and image outputs as pictures (read under at least the `embedded` image tier: they are bytes of the file). |
| `use_web_browser` | bool | false | `--use-web-browser` | `use_web_browser` | — | Pre-render HTML with a headless browser (the `web-browser` Cargo feature). Not in the npm addon (not built with the feature). |
| `xbrl_taxonomy` | directory | unset = the instance's own directory | `--xbrl-taxonomy` | `xbrl_taxonomy` | `xbrlTaxonomy` | XBRL: the directory the instance's taxonomy is read from (docling's `XBRLBackendOptions.taxonomy`). serve: a server-local relative path without `..`. |
| `asr_model` | preset name | unset = Whisper tiny | `--asr-model` | `asr_model` | `asrModel` | ASR preset for audio/video (`whisper_*`, `parakeet_tdt_0.6b_v3`). |
| `asr_lang` | Whisper code \| `auto` | `auto` | `--asr-lang` | `asr_lang` | `asrLang` | Transcription language for audio/video; the caption language of a YouTube URL (#681). |
| `video_frames` | int ≥ 0 \| `all` | 8 | `--video-frames` | `video_frames` | `videoFrames` | Max frames sampled from a video (0 = transcript only; needs ffmpeg). `all` = every distinct cut (#647) — on the JSON surfaces also any count ≥ 4294967295, which is how the npm addon (32-bit integers) asks for it; Python also takes `docling_rs.VIDEO_FRAMES_ALL`. |
| `video_scene_threshold` | 0.0–1.0 | 0.27 (`DOCLING_RS_VIDEO_SCENE_THRESHOLD`) | `--video-scene-threshold` | `video_scene_threshold` | `videoSceneThreshold` | ffmpeg's `scene` score a frame must exceed to count as a cut (#647): a slide change scores well above the default, a fade or re-lighting around 0.5, a hard cut near 1.0 — 0.6 keeps hard cuts only. |
| `video_frame_max_side` | int ≥ 0 px | 0 = source resolution (`DOCLING_RS_VIDEO_FRAME_MAX_SIDE`) | `--video-frame-max-side` | `video_frame_max_side` | `videoFrameMaxSide` | Downscale each sampled frame inside ffmpeg's filter chain so its longer side is at most this (#647): a 1080p recording at 640 yields 640×360 PNGs, so neither the embedded images nor the OCR input reach memory at full size. |
| `video_frame_dedupe` | int 0–64 | unset = keep every frame (`DOCLING_RS_VIDEO_FRAME_DEDUPE`) | `--video-frame-dedupe` | `video_frame_dedupe` | `videoFrameDedupe` | Drop a sampled frame whose 64-bit difference hash (9×8 grey thumbnail) is within this Hamming distance of a frame already kept (#647): the same slide after a fade, or shown again after a camera cut, becomes one picture. 4–6 is a good distance. |
| `pages` | `A-B` \| `N` | unset = every page | `--pages` (also `--page-range`) | `page_range` | `pages` | PDF page window, 1-based inclusive (#80); also DjVu / AFP pages and Numbers sheets (#1031). Python: `page_range=(first, last)`. |
| `document_timeout` | seconds > 0 | unset = unlimited | `--document-timeout` | `document_timeout` | `documentTimeout` | Per-document budget for the PDF pipeline (#497): checked between pages, the pages done so far are the partial document. |
| `no_ocr` | bool | false | `--no-ocr` | `do_ocr` | `noOcr` | Keep layout + TableFormer, never run OCR — docling's `--no-ocr` / `do_ocr=False` (#611; until 2.0 `no_ocr` was the text-layer path, now `text_layer_only`). Python: `do_ocr` (inverted). |
| `skip_ocr` | bool | false | `--skip-ocr` | — | `skipOcr` | `no_ocr` under its pre-2.0 name (#244), still read: either one set skips OCR. Python: `do_ocr`. |
| `text_layer_only` | bool \| `auto` | false | `--text-layer-only[=auto]` | `text_layer_only` | `textLayerOnly` | Skip the whole ML stack: the embedded text layer as flat paragraphs (what `no_ocr` meant before 2.0, #611). `auto` decides per document: the text layer alone when it carries the document (no model touched), the full pipeline — OCR included — when it does not (a scanned / image-only file, or a vestigial layer of a few typed-in form fields over scans). The verdict is the whole document's: a digital file with a few scanned pages inserted keeps the text path. On an SVG, `auto` is the native `<text>` extraction when the file has text and the ML image route when it has none. |
| `password` | string | unset | `--password` (also `--pdf-password`, docling's spelling; `--password-file PATH` reads it from a file) | `password` (also `pdf_password`) | `password` (also `pdfPassword`) | The password of an encrypted PDF — docling's `--pdf-password` (#611) — and of an encrypted Office document (`.docx`/`.xlsx`/`.pptx`/`.doc`/`.xls`/`.ppt`, #625; a docling.rs extension). `pdf_password`, docling's PDF-only name, is accepted on every surface. Python also reads docling's `PdfFormatOption(backend_options=PdfBackendOptions(password=…))`. The `pdf-text` / wasm build reads it for Office documents only. |
| `force_full_page_ocr` | bool | false | `--force-full-page-ocr` | `force_full_page_ocr` | `forceFullPageOcr` | OCR every page, discarding the text layer. docling's deprecated `--force-ocr` is this (or `--ocr-mode full_page`); it gets no alias, as docling itself retires it. |
| `replace_broken_text_layer` | bool | false | `--replace-broken-text-layer` | `replace_broken_text_layer` (also docling's `ocr_options.replace_broken_text_layer`) | `replaceBrokenTextLayer` | OCR only the PDF pages whose text layer a broken font mapping produced — docling's `replace_broken_text_layer` (docling#4654, #1006): ≥ 3 % control / U+FFFD / private-use characters, such characters in ≥ 10 % of the lines, ≥ 50 % of the Latin letters in U+00C0–U+024F, or a vowel share under 0.25. That page is read with full-page OCR and its `parse_score` drops to 0; every other page keeps its text layer, and a low `parse_score` alone replaces nothing. Ignored with `no_ocr` or without an OCR model (the page keeps its layer). |
| `no_table_former` | bool | false | `--no-table-former` (also `--no-tables`) | `do_table_structure` | `noTableFormer` | Skip TableFormer (geometric tables instead). Python: `do_table_structure` (inverted). |
| `no_text_panels` | bool | false | `--no-text-panels` | `no_text_panels` | `noTextPanels` | Disable the text-panel heuristic (#173). |
| `heading_hierarchy` | bool | false | `--heading-hierarchy` | `heading_hierarchy` | `headingHierarchy` | Infer PDF/image heading levels after assembly (#302). |
| `script_positions` | bool | false | `--script-positions` | `script_positions` (also docling's `pipeline_options.script_position_options.enabled`) | `scriptPositions` | Mark PDF superscript and subscript text — docling's `ScriptPositionModel` (docling#4815, #1005): characters drawn smaller than their line and raised or lowered against it (footnote markers, `1st`, `mc²`, `H₂O`) become runs with `formatting.script`. A paragraph or footnote holding them keeps its place and its text moves into an inline group of the runs in the JSON (HTML `<sup>`/`<sub>`, DocLang `<superscript>`/`<subscript>`); Markdown joins the runs (`E = mc2 in 1905.`). Headings, captions, tables, code, formulas, list items, OCR'd and invisible text are left alone; the PDF pipeline only (`text_layer_only` skips it). |
| `ocr_engine` | `ppocr` \| `tesseract` | `ppocr` (`DOCLING_RS_OCR_ENGINE`) | `--ocr-engine` | `ocr_engine` | `ocrEngine` | Which OCR engine reads scanned pages (#460). |
| `ocr_lang` | `en` \| `ch` \| BCP-47; tessdata stems under Tesseract | `en` (`DOCLING_RS_OCR_LANG`) | `--ocr-lang` | `ocr_lang` | `ocrLang` | OCR recognition language, validated against the engine it will drive. |
| `ocr_mode` | `default` \| `full_page` \| `layout_regions` \| `pdf_aware_layout_regions` | `default` | `--ocr-mode` | `ocr_mode` | `ocrMode` | Which regions feed the OCR (docling's `OcrMode`, #254). |
| `ocr_scale` | px/pt > 0 | unset = the 2.0 render | `--ocr-scale` | `ocr_scale` | `ocrScale` | OCR input scale (docling's `OcrOptions.scale`, #254). |
| `pdf_word_gap` | `adaptive` \| `legacy` \| factor in (0, 4] | `adaptive` (`DOCLING_RS_PDF_WORD_GAP`) | `--pdf-word-gap` | `pdf_word_gap` | `pdfWordGap` | Where a PDF's text layer splits words the file draws with a cursor move and no space glyph (#921). `adaptive` is docling-parse ≥ 7's rule, what docling ≥ 2.142 runs on: each gap is divided by the mean advance of the two glyphs flanking it and compared with a line-local two-cluster threshold (0.18–0.35, 0.25 when a line's gaps do not separate; explicit space glyphs stay authoritative). A factor `F` is a fixed threshold for that ratio on lines without space glyphs (0.1–0.23 suits most body text); a line that draws its spaces keeps docling-parse 7's floor of 0.35, so its loosely kerned pairs (`f|l` at 0.24) stay whole — and so does every line of a page whose lines mostly draw their spaces (a lone word in a table cell), except a line whose space glyphs are drawn apart from its letters, whose gap is inferred as in docling-parse. Under `adaptive` a gap enters the line's clustering at most 0.5 advances wide, so one wide gap before a tab stop no longer lifts the threshold over the line's real word gaps (a divergence from docling-parse 7, which glues them). Both keep letter-spaced (Sperrsatz) words whole — a chain of equal cursor moves between letters is tracking, which docling-parse 7 cuts mid-word (`Di ens tk lasse`) — and read four or more dots in a row as one leader (`docs/MIGRATION.md` §4, item 21). `legacy` is docling-parse ≤ 6's rule, the default until #921: a gap splits when it exceeds 0.33 × the running mean advance of the cell built so far, and cells restart at every font change, so the threshold drifts along a line (a borderline gap after a font switch glues: `unddem`). Applies to the exported text and the word cells, on the ML pipeline and the text-layer path alike. |
| `pdf_keep_typography` | bool | false (`DOCLING_RS_PDF_KEEP_TYPOGRAPHY`) | `--pdf-keep-typography` | `pdf_keep_typography` | `pdfKeepTypography` | Keep a PDF text layer's typographic punctuation as drawn (#921) — `„ “ ‚ ‘`, en/em dashes, `…`, `⁄`, `•` — instead of folding it to ASCII the way docling-parse's sanitizer does. Off by default for docling parity; on keeps a legal citation's `„Gewinn“` distinguishable from a straight `"`. |
| `pdf_report_repairs` | bool | false (`DOCLING_RS_PDF_REPORT_REPAIRS`) | `--pdf-report-repairs` | `pdf_report_repairs` | `pdfReportRepairs` | Report what the PDF reader repaired to read the input (#934): every stream whose declared `/Length` does not end at its `endstream` (object, offset, declared and recovered length) and a padded cross-reference table, as `ConversionResult::pdf_repairs` / Python `result.pdf_repairs` / Node `pdfRepairs` / serve's `X-Docling-Pdf-Repairs` header (`pdf_repairs` on a batch / async item) / the CLI's `repaired:` stderr lines; `[]` for a well-formed file. The reader repairs silently (lopdf overwrites `/Length` with what it recovered), so a caller that must refuse recovered input asks for this; the conversion itself is unchanged. Not on the VLM pipeline; the C ABI does not return it. |
| `ocr_textline_orientation` | bool | false (`DOCLING_RS_OCR_TEXTLINE_ORIENTATION`) | `--ocr-textline-orientation` | `ocr_textline_orientation` | `ocrTextlineOrientation` | RapidOCR's text-line orientation handling (#666): oriented line crops, tall lines turned 90°, upside-down lines turned by the 0/180 classifier. PP-OCR pages and pictures alike. Python also maps docling's `RapidOcrOptions.use_cls`. |
| `images_scale` | 0.1–4.0 px/pt | unset = the 2.0 render | `--images-scale` | `images_scale` | `imagesScale` | Picture-crop / page-image scale (docling's `images_scale`, #520). |
| `page_images` | bool | false | `--page-images` | `generate_page_images` | `pageImages` | Keep each page's render as the JSON page image (docling's `generate_page_images`, #520). Python: `generate_page_images` (also accepted as a JSON alias). |
| `use_embedded_picture_images` | bool | false | `--use-embedded-picture-images` | `use_embedded_picture_images` | `useEmbeddedPictureImages` | A PDF picture whose region holds exactly one embedded image (mutual intersection-over-self ≥ 0.8) with nothing drawn over it gets that bitmap at native resolution, `dpi` 72, instead of the render crop (docling's option, docling#5143, #1007). Masked, transparent, clipped, rotated or overlapped images, a second image or text in the box, and `/Rotate`d pages keep the crop. |
| `do_picture_classification` | bool | false | `--enrich-picture-classes` | `do_picture_classification` | `doPictureClassification` | Enrichment: classify pictures with DocumentFigureClassifier (#423). CLI: `--enrich-picture-classes`. |
| `do_code_enrichment` | bool | false | `--enrich-code` | `do_code_enrichment` | `doCodeEnrichment` | Enrichment: rewrite code blocks with CodeFormulaV2 (#423). CLI: `--enrich-code`. |
| `do_formula_enrichment` | bool | false | `--enrich-formula` | `do_formula_enrichment` | `doFormulaEnrichment` | Enrichment: decode display formulas to LaTeX with CodeFormulaV2 (#423). CLI: `--enrich-formula`. |
| `do_picture_ocr` | bool | false | `--picture-ocr` | `do_picture_ocr` | `doPictureOcr` | Enrichment: OCR the pictures embedded in non-PDF documents (DOCX/PPTX/HTML/…, video frames) with the OCR models; the text is the picture's description annotation — Markdown prints it after the caption, JSON carries `meta.description` (#645). CLI: `--picture-ocr`. |
| `picture_ocr_classes` | comma-separated classifier labels | unset = every picture | `--picture-ocr-classes` | `picture_ocr_classes` (str or list) | `pictureOcrClasses` | With `do_picture_ocr`: only read pictures the DocumentFigureClassifier labels as one of these (its 26 classes; an unknown label is rejected). |
| `picture_ocr_min_side` | px ≥ 0 | 32 (`DOCLING_RS_PICTURE_OCR_MIN_SIDE`) | `--picture-ocr-min-side` | `picture_ocr_min_side` | `pictureOcrMinSide` | With `do_picture_ocr`: skip pictures whose smaller side is under this (icons, bullets). |
| `picture_ocr_scale` | px/px > 0 | unset = the image-input rule (`DOCLING_RS_PICTURE_OCR_SCALE`) | `--picture-ocr-scale` | `picture_ocr_scale` | `pictureOcrScale` | With `do_picture_ocr`: the pictures' OCR resolution in px per picture pixel, shrunk to RapidOCR's 2000 px longer side (#666) — `1.0` reads them at their native pixels, as RapidOCR does. Pages and image inputs are unaffected. |
| `do_picture_description` | bool | false | `--picture-description` | `do_picture_description` | `doPictureDescription` | Enrichment (#670): describe every picture — the embedded pictures of DOCX/PPTX/XLSX/HTML, a PDF's figures, video frames, an image input as a whole — through an OpenAI-compatible vision endpoint; the answer is the picture's description annotation, where `do_picture_ocr` puts its text (Markdown after the caption, JSON `meta.description` with the model as `created_by`). `picture_ocr_min_side` / `picture_ocr_classes` select the pictures here too; identical images are sent once; an endpoint that rejects a picture warns and the rest go on, one that cannot be reached warns once. Runs before the local picture OCR. Python: docling's `PictureDescriptionApiOptions` maps onto these; serve: docling-serve's `picture_description_api` too. |
| `picture_description_endpoint` | URL | `vlm_endpoint`, else `DOCLING_RS_VLM_ENDPOINT` | `--picture-description-endpoint` | `picture_description_endpoint` | `pictureDescriptionEndpoint` | The server's `/v1` base or the full `chat/completions` URL. serve: a request-supplied endpoint needs `--allow-url-fetch`, like `vlm_endpoint`. |
| `picture_description_model` | string | `vlm_model`, else `DOCLING_RS_VLM_MODEL` | `--picture-description-model` | `picture_description_model` | `pictureDescriptionModel` | Model name as the server knows it. |
| `picture_description_api_key` | string | `vlm_api_key`, else `DOCLING_RS_VLM_API_KEY` | `--picture-description-api-key` | `picture_description_api_key` | `pictureDescriptionApiKey` | Bearer token. |
| `picture_description_prompt` | string | the mode's prompt | `--picture-description-prompt` | `picture_description_prompt` | `pictureDescriptionPrompt` | The instruction sent with every picture (not `DOCLING_RS_VLM_PROMPT`, which asks for a page's markup). |
| `picture_description_mode` | `caption` \| `ocr` | `caption` | `--picture-description-mode` | `picture_description_mode` | `pictureDescriptionMode` | `caption` sends MarkItDown's "Write a detailed caption for this image."; `ocr` markitdown-ocr's "Extract all text from this image. Return ONLY the extracted text, maintaining the original layout and order. Do not add any commentary or description." — LLM-vision OCR where the local models read poorly. An unknown value is rejected. |
| `picture_description_concurrency` | int > 0 | 1 | `--picture-description-concurrency` | `picture_description_concurrency` | `pictureDescriptionConcurrency` | Pictures in flight at once; 429s retry with backoff. 0 is rejected. |
| `keep_picture_images` | bool | true | `--no-picture-images` | `keep_picture_images` | `keepPictureImages` | `false` drops the embedded image bytes from every picture after the enrichment pass: slim JSON/DCLX, placeholder-only Markdown, the OCR text kept. CLI: `--no-picture-images`. |
| `redact_pii` | bool | false | `--redact-pii` | `redact_pii` | `redactPii` | Redact personal data from the converted document before any export (#621): e-mail, phone, card (Luhn), IBAN (mod-97), IP, URL credentials, national IDs, and names / organizations / locations with the NER model. The result carries the counts (`redaction`; serve: `X-Docling-Redaction` / item `redaction`). A docling.rs extension. |
| `redact_mode` | `label` \| `pseudonym` \| `fixed:<text>` | `label` | `--redact-mode` | `redact_mode` | `redactMode` | What a span becomes: `[EMAIL]`; `[EMAIL_1]` / `[EMAIL_2]` (one number per distinct value, consistent within the document); a fixed string. |
| `redact_kinds` | comma-separated kinds | unset = every kind | `--redact-kinds` | `redact_kinds` | `redactKinds` | `email, phone, credit_card, iban, ip_address, url_credentials, national_id, person, organization, location, address`; an unknown kind is rejected. |
| `redact_pattern` | `NAME=REGEX` lines | unset | `--redact-pattern` (repeatable) | `redact_pattern` (str or list) | `redactPattern` | Extra patterns, redacted as `[NAME]` (the match or capture group 1); an invalid regex is rejected. Several entries are newline-separated on the wire. |
| `redact_images` | `drop` \| `box_out` \| `keep` | `drop` | `--redact-images` | `redact_images` | `redactImages` | Embedded images and page renders: removed; OCR'd and painted over where a line carries a value (needs the OCR models; the PDF streaming path drops instead); left alone. |
| `redact_allow_terms` | comma-separated terms | unset | `--redact-allow-terms` | `redact_allow_terms` (str or list) | `redactAllowTerms` | With `redact_pii` (#694): never redacted even when a detector flags them (case-insensitive, the whole span) — `support@example.com`, the company's own name. |
| `redact_deny_terms` | comma-separated terms | unset | `--redact-deny-terms` | `redact_deny_terms` (str or list) | `redactDenyTerms` | With `redact_pii`: always redacted, as `[REDACTED]` (whole words, case-insensitive) — a project code name, a client the NER misses. |
| `redact_allow_patterns` | regexes, one per line | unset | `--redact-allow-pattern` (repeatable) | `redact_allow_patterns` (str or list) | `redactAllowPatterns` | With `redact_pii` (#694): a flagged span that matches one of these in full is never redacted, whichever detector flagged it — the document's own part numbers (`[A-Z]{2}\d{6}`), record IDs, ticket formats. An invalid regex is rejected by name. |
| `redact_ner_min_score` | 0.0–1.0 | 0.85 | `--redact-ner-min-score` | `redact_ner_min_score` | `redactNerMinScore` | With `redact_pii`: the NER detector's minimum confidence for a name, organization or location. |
| `pipeline` | `standard` \| `vlm` | `standard` | `--pipeline` | `pipeline` | `pipeline` | The remote vision-model pipeline instead of the local ML stack (#77); the `vlm_*` options are inert under `standard`. |
| `vlm_endpoint` | URL | `DOCLING_RS_VLM_ENDPOINT` | `--vlm-endpoint` | `vlm_endpoint` | `vlmEndpoint` | OpenAI-compatible endpoint for `pipeline=vlm`. serve: a request-supplied endpoint needs `--allow-url-fetch`. |
| `vlm_model` | string | `DOCLING_RS_VLM_MODEL` | `--vlm-model` | `vlm_model` | `vlmModel` | Model name for `pipeline=vlm`. |
| `vlm_api_key` | string | `DOCLING_RS_VLM_API_KEY` | `--vlm-api-key` | `vlm_api_key` | `vlmApiKey` | Bearer token for the endpoint. |
| `vlm_prompt` | string | `DOCLING_RS_VLM_PROMPT`, else docling's default | `--vlm-prompt` | `vlm_prompt` | `vlmPrompt` | Per-page instruction. |
| `vlm_max_tokens` | int > 0 | 8192 | `--vlm-max-tokens` | `vlm_max_tokens` | `vlmMaxTokens` | `max_tokens` per completion. |

## Validation rules (shared)

Checked before any work starts, with the same message on every surface
(the CLI prints the flag spelling, Node the camelCase one):

- `pages` — `A-B` or `N`, 1-based, `A ≤ B`.
- `document_timeout`, `ocr_scale` — finite and positive.
- `images_scale` — in 0.1–4.0.
- `ocr_engine` — `ppocr` | `tesseract`; `ocr_mode` — one of the four modes.
- `pdf_word_gap` — `adaptive` | `legacy` | a number in (0, 4].
- `pdf_keep_typography` — a boolean.
- `pdf_report_repairs` — a boolean.
- `ocr_lang` — a language the selected engine reads (`en`/`ch`/BCP-47 under
  PP-OCR; tessdata stems or BCP-47 tags under Tesseract).
- `pipeline` — `standard` | `vlm`; `vlm_max_tokens` — positive; under `vlm`,
  an endpoint and a model must come from the options or the
  `DOCLING_RS_VLM_*` environment.

`asr_lang`, `encoding` and `ebcdic_layout` are checked against the model,
codec or copybook when the conversion runs.

## Surface notes

- **CLI** — flags fill a `ConvertOptions`, `validate()` runs once after the
  command line is parsed; errors read `--ocr-scale must be a positive
  number, got 0`. Python `docling convert`'s spellings are accepted too
  (#611): `--page-range` (`--pages`), `--image-export-mode` (`--images`),
  `--no-tables` (`--no-table-former`), `--no-ocr` with docling's meaning,
  `--pdf-password` (= `--password`), and `--output-file PATH` — the one result written to
  exactly PATH, refused unless there is exactly one input document and one
  `--to` format (docling's checks and messages).
  A source is a path, `-` (stdin), an `http(s)://` URL, a `data:` URL or a
  `file:` URL (#668, MarkItDown's `markitdown URL` and `cat f | markitdown`).
  A fetched URL is named by its `Content-Disposition`, else the URL path's
  last segment, and typed by that name's extension, else the `Content-Type`,
  else the bytes' signature (PDF, OOXML/ODF/EPUB, OLE, RTF, HTML, the raster
  images, DjVu); redirects are followed (5) and every hop passes the SSRF
  guard docling-serve uses, enforced at connect time (#752) — a private,
  loopback or other non-global address needs
  `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1`, the body is capped by
  `DOCLING_RS_MAX_FETCH_BYTES`. `--from FORMAT` (an identifier from
  `--list-input-formats`) names the format outright, for stdin, a bare URL
  or a file whose extension lies; `--mime-type TYPE` stands in for a
  missing `Content-Type`; a source none of these can type is a usage error
  (exit 2) naming both flags. `--no-url-fetch` refuses http(s) sources.
  Library: `SourceDocument::from_reader`, `from_named_bytes`, `from_url`
  (with a `FetchPolicy`), `docling::fetch::from_data_url`.
- **YouTube URLs** (#681, MarkItDown's `_youtube_converter`). A
  `youtube.com/watch?v=…` (or `youtu.be/…`, `/shorts/`, `/embed/`, `/live/`)
  source converts to the video, not the page: `# YouTube`, `## <title>`, a
  `### Video Metadata` list (views, keywords, runtime), `### Description`,
  and `### Transcript` — the caption track fetched from YouTube's
  `timedtext` endpoint as WebVTT and read by the WebVTT backend, so the
  cues are timed items (`--to vtt` writes them back; `--to json` carries
  `start_time` / `end_time`). The language is `--asr-lang` (`auto` /
  unset: English, else the video's first track; a written track over an
  auto-generated one; a language the video has no track for is translated
  by YouTube when it offers it). A video without captions is an error
  naming it. Nothing but the captions is fetched — no video, no audio, no
  playlist. The route is by the URL the page came from (as MarkItDown's
  `accepts`): a saved watch page converts as HTML. Behind the `youtube`
  cargo feature (default on; off in the wasm build). Library: the
  converter routes it; `docling::backend::youtube` has the pieces
  (`video_id`, `parse_watch_page`, `pick_track`, `transcript_document`).
  Two CLI-only export options sit beside `--images`, outside the shared
  table: `--layers body|all|LIST` (LIST from `body,furniture,notes,invisible,background`;
  default `body`) and `--traverse-pictures` — docling-core's
  `export_to_markdown(included_content_layers=…, traverse_pictures=…)` for
  `--to md`/`text`/`html` (HTML takes the layers only), inert for the other
  formats. `furniture` keeps page headers/footers; `--layers furniture`
  alone drops the body, as upstream's set does. Markdown keeps streaming
  with them (`convert_streaming_with_options`).
- **serve** — the query string, the JSON body and multipart text parts are
  three layers of the same struct (body over query, text parts over query),
  merged generically; a text value is read as the option's type (string,
  integer, number, then `1`/`true`/`yes`/`on` and `0`/`false`/`no`/`off` for
  booleans), an unreadable one is a 400 naming the option. Unknown names are
  ignored, as unknown query parameters always were. The two server-side
  policies stay in serve: `fetch_images` / `image_sources=remote` and a
  request-supplied `vlm_endpoint` need `--allow-url-fetch` (`remote` is
  held to `embedded` without it), `image_sources=local` needs
  `--allow-local-images`, `xbrl_taxonomy` must be a relative path without
  `..`.
- **Python** — the keyword arguments keep docling's spellings (`do_ocr`,
  `do_table_structure`, `page_range`, `generate_page_images`) and map onto
  the struct; a rejected option is a `ValueError` at construction.
- **Node** — the option objects (`ConverterOptions`, `ConvertOptions`,
  `StreamOptions`) serialize into the struct, so every property is honoured
  by `convertFile`, `new DocumentConverter`, `new Pipeline` and the
  streaming functions alike; a rejected option throws `InvalidArg` naming
  the property (`ocrLang`, `documentTimeout`).
- **C ABI / wasm** — one JSON object: `to`, `images` plus the wire names;
  an unknown key is an error.

## OCR engines and languages

- **Engines** (`ocr_engine`, #460). `ppocr` is the built-in PP-OCR stack
  every conformance baseline is pinned against: RapidOCR's PP-OCRv6 text
  detector (`.models/ocr_det.onnx`, `DOCLING_OCR_DET_ONNX`) finds the lines
  on a bitmap page — inside the layout regions its boxes are the
  recognizer's crops, the lines no region covers come out as orphan text
  (#570; `DOCLING_RS_OCR_LINES=projection` restores the pre-#570
  ink-projection strips) — and the recognizer reads them: PP-OCRv6
  (`.models/ocr_rec_v6.onnx`) when installed, else the PP-OCRv3 `en` / `ch`
  pairs; `DOCLING_OCR_REC_ONNX` + `DOCLING_OCR_DICT` pin a pair explicitly.
  `tesseract` runs the system `tesseract` binary — docling's
  `TesseractCliOcrOptions`: one subprocess per layout-region crop, `tsv` on
  stdout, no bindings — so any of its 100+ languages reads through the same
  pipeline (regions, orphan recovery, TableFormer word matching); orientation
  comes from Tesseract's own OSD (needs the `osd` traineddata). Install
  `tesseract-ocr` plus language packs (`tesseract --list-langs`; the
  container images build Tesseract 5.5.3 and ship every tessdata_fast
  language model, `TESSDATA_LANGS` slims or extends that — docs/DEPLOYMENT.md;
  serve's `/v1/config` lists them as `tesseract_languages`).
  `DOCLING_TESSERACT` names the binary, `DOCLING_RS_TESSERACT_PSM` a page
  segmentation mode (unset = Tesseract's automatic 3; 6 = one uniform block,
  11 = sparse text),
  `DOCLING_RS_TESSDATA_DIR` the data directory (`TESSDATA_PREFIX` is honoured
  by Tesseract itself). A missing binary or traineddata warns and degrades
  to no OCR, like a missing model; under `force_full_page_ocr` it is an
  error. Each crop is one process, dealt across the OCR lanes
  (`DOCLING_RS_OCR_SESSIONS`), each pinned to one OpenMP thread. Python also
  maps a docling-shaped `TesseractCliOcrOptions` (`lang`, `tesseract_cmd`,
  `path`, `psm`).
- **Languages** (`ocr_lang`). Under PP-OCR: `en` (the default — the English
  PP-OCRv3 model, good Latin word spacing) or `ch` (the multilingual model
  the docling conformance corpus was generated with — use it for
  byte-for-byte comparisons; `DOCLING_RS_OCR_LANG=ch` process-wide, the
  conformance scripts pin it themselves). BCP-47 tags resolve to the same
  two (#388, docling#4075): `en-US`, `en_GB`, `eng`, `english` → `en`; `zh`,
  `zh-Hans`, `zh-CN`, `zh-TW`, `zho`, `chinese`, EasyOCR's `ch_sim` → `ch`,
  with or without docling's `iso:` prefix — script and region subtags are
  ignored; any other language (`de`, `ja`, …) is rejected (the env var
  warns and defaults). Under Tesseract: tessdata stems (`deu`, `eng+fra`,
  `script/Cyrillic`, a traineddata of your own) or BCP-47 tags mapped onto
  them (`de` → `deu`, `zh-Hant` → `chi_tra`); `en` and `ch` keep working.
  Without a request `ocr_lang`, `DOCLING_RS_OCR_LANG` is Tesseract's list
  too (`DOCLING_RS_OCR_ENGINE=tesseract DOCLING_RS_OCR_LANG=rus+eng` in a
  container's environment, #889 — it used to reach only the PP-OCR model
  choice, warn, and leave Tesseract on `eng`). The list is Tesseract's preference
  order: for mostly-Latin pages with some Cyrillic write `eng+rus` — under
  `rus+eng` Tesseract reads look-alike Latin words as Cyrillic (`My` → `Му`).
- **Regions and scale** (#254). `ocr_mode` is docling's `OcrMode`: the
  default (= `pdf_aware_layout_regions`) keeps the text layer and OCRs what
  lacks one — every layout region no programmatic text touches, so text
  drawn as vector outlines beside a native text layer is read too (#931); `full_page` and `layout_regions` both discard the text layer,
  exactly like `force_full_page_ocr` (the whole-page vs per-region *detector*
  input they distinguish upstream has no analogue here: the detector always
  sees the whole page, the recognizer always reads line crops). `ocr_scale`
  is docling's `OcrOptions.scale` in px/pt: unset, OCR reads the pipeline's
  2.0 px/pt (144 dpi) render — the pinned conformance baseline — and an
  image input at docling's effective resolution (#570): 3 px/pt shrunk so
  the longer side stays within RapidOCR's 2000 px (a 754 × 1000 scan reads
  at 2.0; measured best on FUNSD: 0.825 / 0.856 / 0.836 word recall at
  1 / 2 / 3 px/pt); a set value resamples the OCR input only, layout and
  TableFormer pixels are untouched. docling's default is 3 (216 dpi); lower
  it when the source raster is already high-resolution. Pictures (the
  picture-OCR enrichment) follow the image-input rule unless
  `picture_ocr_scale` (#666) sets their own: px per picture pixel under the
  same 2000 px cap, `1.0` being RapidOCR's native-pixel reading — a 720 ×
  180 screenshot reads at 720 × 180 instead of 2000 × 500.
- **Line crops** (#981). Every line the text detector finds — on scanned
  pages, images and in tables — reaches the recognizer the way RapidOCR
  crops it: warped upright out of the detector's rotated rectangle
  (`get_rotate_crop_image`: bicubic, replicated borders), a crop at least
  1.5× taller than wide turned 90° counter-clockwise. The crop used to be
  the box's axis-aligned bounding rectangle, which on a scan skewed by even
  half a degree is `width · tan θ` taller than the line: the long
  small-print lines reached the recognizer squeezed and slanted and came
  back with letters dropped and words glued while the short ones read fine.
  The reported boxes stay axis-aligned, so reading order and geometry do
  not move.
- **Line orientation** (#666, off by default). `ocr_textline_orientation`
  adds PaddleOCR's `PP-LCNet_x0_25_textline_ori` classifier
  (`.models/ocr_textline_ori.onnx`, `DOCLING_OCR_TEXTLINE_ORI_ONNX`;
  80 × 160 BGR input, batches of 6, `180` above 0.9) on top of those crops,
  turning upside-down lines so they read too. It applies to every PP-OCR
  line read off the detector inside the layout regions, not to table words,
  the projection fallback or Tesseract. A missing classifier warns once and
  keeps the crops.
- **Detector and recognizer knobs** (env): `DOCLING_RS_OCR_DET_MAX_SIDE`
  (2000 = RapidOCR's `max_side_len`; `0` lifts the cap, 960 — PaddleOCR's
  budget — is about a third of the time at ~0.02 recall on FUNSD),
  `DOCLING_RS_OCR_DET_BUDGET` (`auto`, the default: a scanned PDF page is
  detected at the input that keeps its small print — the 25th-percentile
  text-line height — at 20 px: the full cap for 72-dpi forms, down to 960
  for a 2 px/pt render of large type, #945; standalone images stay at the
  cap; `full` always uses the cap),
  `DOCLING_RS_OCR_TEXT_SCORE` (RapidOCR's `text_score`, 0.5 — a line whose
  mean character confidence is under it is dropped; `0` keeps all),
  `DOCLING_RS_OCR_ORIENTATION=off` (no un-rotation probe of raster-rotated
  scans, #225/#571). The detector runs on bitmap pages only and
  concurrently with the layout model.
- **Picture crops and page images** (#520). `images_scale` (0.1–4.0 px/pt)
  resamples the 2.0 px/pt render for picture crops and page images (above
  2.0 upsamples, it does not re-render); each picture's `image.dpi` in the
  JSON is 72·scale (#519). `page_images` keeps every page's render, at
  `images_scale`, as the JSON `pages[n].image` (docling-core's
  `PageItem.image`), so `TableItem.get_image(doc)` / `FormulaItem.get_image(doc)`
  crop from it; off by default (a PNG per page in memory), no render under
  `text_layer_only`, none in streamed Markdown. Python also takes both
  docling-shaped through `PdfPipelineOptions`, where `images_scale` applies
  once `generate_picture_images` or `generate_page_images` is on.
  `use_embedded_picture_images` (#1007, docling#5143) swaps a picture's crop
  for the embedded image it shows when that is unambiguous: exactly one image
  XObject covering ≥ 0.8 of the region and covered ≥ 0.8 by it, no other
  image and no text cell overlapping it, nothing painted *after* it over its
  box (a path, shading, image or form-field appearance), opaque (no
  `/SMask`/`/Mask`/`/ImageMask`, no ExtGState soft mask, alpha or blend
  mode), drawn upright and unclipped, on a page without `/Rotate`, and
  decodable. The bitmap keeps its native pixel size (`dpi` 72; `images_scale`
  sizes render crops only). Everything else keeps the crop.

## VLM pipeline

`pipeline=vlm` (#77) renders each page in Rust and sends it, with docling's
page-conversion prompt, to an OpenAI-compatible vision endpoint — LM Studio,
Ollama, vLLM, a hosted service; an image input is sent as-is. No ONNX
models load.

- `vlm_endpoint` takes the server's `/v1` base or the full
  `…/chat/completions` URL; `vlm_api_key` is sent as a Bearer token;
  `vlm_max_tokens` defaults to 8192 (#312). `DOCLING_RS_VLM_ENDPOINT` /
  `_MODEL` / `_API_KEY` / `_PROMPT` are the env fallbacks of the options.
  They supply values, never switch the pipeline on — a stale endpoint
  cannot reroute an ordinary conversion over the network — and the `vlm_*`
  options are inert under `standard` (the Node bindings ignore stray `vlm*`
  options likewise). `DOCLING_RS_VLM_TIMEOUT` (seconds, 600) caps each page
  request for slow, e.g. CPU-served, endpoints; `DOCLING_RS_VLM_EXTRA_BODY`
  merges extra JSON into every request; `DOCLING_RS_VLM_DEBUG` logs the
  exchange.
- Transient failures (transport errors, 408/429, 5xx) retry with
  exponential backoff. A page that still fails (a 4xx, retries exhausted,
  the timeout) is left out and recorded as `VLM inference failed: …`; a page
  the server cut off (`finish_reason` `length` — raise `vlm_max_tokens` —
  or `content_filter`) keeps its text and is recorded as `VLM output
  incomplete (stop_reason=…)`. Either makes the result docling's
  `partial_success`, each error carrying its `page_no` (docling#4561,
  docling#4778; #762, #763): the CLI writes the document and warns on
  stderr, serve answers `X-Docling-Status: partial_success`, Python/Node
  return the status and `errors`. Only a document where no page produced
  output fails — on serve, that request only.
- `pages` composes (only the window is rendered and sent); `to` and
  `strict` work as usual.
- Answer grammars are detected per response (#322): DocTags
  (granite-docling-class models), DocLang XML, Chandra layout HTML
  (`<div data-bbox=… data-label=…>` blocks — tables with spans, Form-held
  tables, lists, figures, page furniture; docling 2.123–2.125 semantics
  including `<br>`-as-spacing), Unlimited-OCR grounding output (normalized
  into the DeepSeek-OCR annotation shape) and raw DeepSeek-OCR annotated
  Markdown; plain prose degrades to text — hostile model output never
  errors. Known models get their official prompt when `vlm_prompt` is
  unset: `unlimited*` → the model card's `<image>document parsing.` (any
  other phrasing returns an empty completion) plus the
  `skip_special_tokens=false` request flag its grounding markers need;
  `chandra*` → docling's Chandra layout prompt; everything else the
  DocLang-eliciting default.
- serve: a request-supplied `vlm_endpoint` is outbound traffic steered by
  the caller, so it needs `--allow-url-fetch` and passes the same SSRF
  resolution check as URL inputs (private/loopback endpoints refused;
  `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1` for local development). The
  operator-pinned mode — `DOCLING_RS_VLM_ENDPOINT` / `DOCLING_RS_VLM_MODEL`
  on the server, requests send only `pipeline=vlm` — needs no gate.
- Measured (#311): the PDF corpus through one granite-docling endpoint from
  docling.rs and from Python docling's `VlmPipeline` scores 87.7 % mean
  whitespace-normalized similarity over 18 fixtures, 3 byte-exact; the gap
  is dominated by each side rendering at its own scale (144 vs 216 dpi).
  Table and method in `docs/PDF_CONFORMANCE.md`; harness
  `scripts/conformance/vlm_conformance.sh` (a GPU-served endpoint — CPU
  inference measures hours per page).

## CLI batch mode

`docling-rs SOURCE… --output DIR` / `--input GLOB|DIR --output DIR` (#205,
#489): one warm process, every source a batch item.

- **Sources.** Positional files, directories (swept recursively, taking
  every file with a convertible extension — stray `.log` / `.tmp` files are
  ignored, not failures), quoted globs, and `--input` with a glob (quote it)
  or a directory. `--output` with a single file works too (a batch of one).
  Which extensions count is the binary's own list (#603): `--list-input-formats`
  prints them sorted, one per line, no dot, `zip` included;
  `--list-output-formats` the `--to` values; a format behind a cargo feature
  the build lacks (PDF/images without `pdf`, audio/video without `asr`,
  `.heic` without `heif`) is left out. Library: `InputFormat::supported_extensions()`,
  `docling::OUTPUT_FORMATS`.
- **Outputs.** `--to` is repeatable (`--to md --to json` = `--to md,json`):
  each document converts once and is written in every format named, as
  `<stem>.<ext>` (`.md`, `.json`, `.html`, `.txt`, `.dclx`, `.chunks.json`,
  `.tex`, `.pandoc.json`, `.vtt`); several formats need `--output`, since
  stdout carries one document. `--output-file PATH` (#611, docling's)
  writes the one result to exactly PATH — refused unless there is exactly
  one input and one format; `--images referenced` pictures land in
  `<stem>_artifacts/` next to it.
- **Layout** (`--output-dirs`, #496). `auto` (default): a directory or glob
  source mirrors its tree below the directory / the pattern's static
  prefix (`/data/reports/**/*.pdf` → `out/2024/q1/a.json`), a plain file
  lands by stem. `flat`: every output directly in `--output`. `mirror`:
  every input by its path relative to the current directory, explicit files
  included (`a/README.md b/README.md` → `out/a/README.md`, `out/b/README.md`);
  an input outside the current directory is refused. Two sources that would
  write the same file (`sub/b.docx` and `other/b.docx` → `b.md`) are refused
  before anything converts — pass a common parent, use `mirror`, or
  separate `--output` directories.
- **Parallelism and progress.** The PDF/image models load once and every
  file reuses the warm sessions; `--jobs N` converts declarative formats in
  parallel (PDFs already parallelize internally). Output paths print to
  stdout one per line; stderr gets `start: <file> (N pages)`, a dot every
  10 finished pages and `ok: … (12.8s, 800 ms/page)`. Pipeline diagnostics
  are quiet unless `DOCLING_RS_DEBUG=1`.
- **Failures.** A failing file is reported and skipped; the exit code is
  non-zero if anything failed; `--abort-on-error` (Python's flag) stops at
  the first. One exception: an execution-provider failure (an explicit
  `DOCLING_RS_EP` whose runtime libraries are missing) would fail every
  remaining PDF identically, so the first one aborts the batch (`fatal: …`,
  the rest `skipped`).
- **ZIP archives** (#557). A `.zip` named as a source (or matched by a glob)
  converts every document inside, each its own batch item — one broken
  document fails only itself; `out/bundle/<entry path>.<ext>`. Entries are
  listed from the central directory before anything is inflated, nothing is
  extracted to disk, and the ones that do not convert are reported
  (`skip: bundle.zip:tool.exe: unsupported file type`) and counted:
  unsupported types, nested archives beyond the depth, `__MACOSX` metadata,
  encrypted entries, `..` paths, and entries over the limits — 10 000
  entries, 256 MiB per entry, 1 GiB in all, a 200:1 compression ratio
  (`DOCLING_RS_ZIP_MAX_ENTRIES` / `_MAX_ENTRY_MB` / `_MAX_TOTAL_MB` /
  `_MAX_RATIO`, `ArchiveLimits::from_env`). A directory sweep does not open
  archives it finds — only explicitly named ones expand. Library:
  `DocumentConverter::convert_archive(reader)`, a lazy iterator of
  `Converted` / `Skipped` / `Failed` outcomes (`docling::archive`); Python
  `convert_archive`, Node `convertArchiveFile` / `convertArchive`.
- **One document per archive** (#677, `--archive-merge`). A lone `.zip` on
  stdout (`docling-rs bundle.zip`) — and under `--output-file` — is one
  document: per convertible entry, in archive order, a `## <entry path>`
  heading and the entry's content, MarkItDown's `_zip_converter` shape
  (its `## File:` prefix aside). Every `--to` format applies to it. Under
  `--output DIR` the flag opts in (`DIR/<archive>.<ext>` instead of one
  output per entry); `--no-archive-merge` keeps a lone `.zip` a usage
  error. Entries that do not convert are reported on stderr (`skip:
  bundle.zip:tool.exe: unsupported file type`, `error: bundle.zip:x.docx:
  …`) and left out; an archive with nothing convertible fails. Library:
  `DocumentConverter::convert_archive_merged(name, reader)` → a
  `MergedArchive` (the document, the counts, the skipped and failed
  entries); the entries' page markers are dropped from the merged
  document.
- **Nested archives** (#677). A ZIP inside the archive is read when
  `ArchiveLimits::max_depth` allows (`DOCLING_RS_ZIP_MAX_DEPTH`; default 1
  = nested archives skipped, at most 3), its entries listed as
  `inner.zip/<path>` beside the outer ones — one shared entry count and
  size budget across the levels, the nested archive's own bytes counted
  too. tar / 7z / rar / gzip are never opened.
- **Email attachments** (#561). An `.eml` / `.msg` converts as one document
  (headers + body, the attachment *names* with `--list-attachments`); the
  payloads are reachable from the library and bindings, not the CLI or
  serve. `docling::EmailAttachments::open(bytes, &limits)` lists them — a
  safe unique file name (`report-2.pdf` for a second `report.pdf`), media
  type, size, inline-image flag, and the format it converts as (extension,
  else media type, else the bytes) or why it will not (no payload — an
  attachment by reference or an OLE object; over a limit; a nested archive
  or unsupported type, whose bytes stay available through `data(i)`). A
  forwarded message (`message/rfc822`, an embedded `.msg` message) is an
  `.eml` entry. `DocumentConverter::convert_email_attachments(bytes)`
  converts them with the archive outcomes above under the same
  `ArchiveLimits` (no ratio: MIME cannot bomb). Python:
  `docling_rs.email_attachments(path | bytes | DocumentStream)` →
  `EmailAttachment(…, data)` with `.as_stream()` for `convert` (the stream
  carries the detected format, so a `scan.bin` sent as `application/pdf`
  converts as a PDF); Node: `emailAttachments({ name, data })` /
  `emailAttachmentsFile(path)` (+ `*Async`), then `convert({ name, data,
  format })`. Payloads are the bytes as sent — only the transfer encoding is
  undone (#564).
