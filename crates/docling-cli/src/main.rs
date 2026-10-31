//! Minimal CLI: convert a file and print Markdown or JSON to stdout.
//!
//! The docling.rs counterpart of `docling.cli.main`; `docling-rs serve`
//! (with `--features serve`) starts the HTTP conversion API, `docling-rs mcp`
//! (default `mcp` feature, #669) a Model Context Protocol server for agents.
//!
//! `--no-ocr` keeps layout + TableFormer but never runs OCR — docling's
//! `--no-ocr` (`do_ocr=False`) since 2.0 (#611); `--skip-ocr`, its pre-2.0
//! name, is an alias, and the skip-everything fast path `--no-ocr` used to
//! be is `--text-layer-only`. docling's other spellings are accepted too:
//! `--page-range` (`--pages`), `--image-export-mode` (`--images`),
//! `--no-tables` (`--no-table-former`), `--pdf-password` (`--password`),
//! `--output-file`.
//!
//! `--help` prints the full flag list and `--version` the version plus the
//! optional features the binary carries (execution providers, `serve`,
//! chunking) — both answer without models present. So do
//! `--list-input-formats` / `--list-output-formats` (#603, Pandoc's flags of
//! the same name): one identifier per line, sorted, for scripts that ask the
//! binary what it converts instead of hard-coding a list.
//!
//! Usage: docling-rs [--strict] [--page-break-placeholder TEXT] [--layers body|all|LIST] [--traverse-pictures] [--to md|json|html|text|dclx|chunks|images|latex|pandoc|vtt] [--pages A-B] [--scale X] [--images MODE] [--input GLOB --output DIR [--jobs N]] [--output-file PATH] [--fetch-images] [--image-sources none|embedded|local|remote] [--image-hosts LIST] [--max-images N] [--max-image-bytes N] [--max-image-total-mb N] [--min-image-bytes N] [--list-attachments] [--skip-empty-cells] [--compact-tables] [--picture-alt-text] [--ebcdic-layout JSON|PATH] [--encoding LABEL] [--ipynb-outputs none|text] [--metadata none|json|frontmatter] [--track-changes accept|reject|all] [--extract-diagrams mermaid|none] [--no-outline-level-headings] [--from FORMAT] [--mime-type TYPE] [--no-url-fetch] [--no-stream] [--progress|--no-progress] [--no-table-former] [--no-ocr] [--text-layer-only] [--password PASSWORD | --password-file PATH] [--force-full-page-ocr] [--no-text-panels] [--heading-hierarchy] [--ocr-lang LANG] [--ocr-engine ppocr|tesseract] [--ocr-mode MODE] [--ocr-scale X] [--pdf-word-gap adaptive|legacy|F] [--pdf-keep-typography] [--pdf-report-repairs] [--ocr-textline-orientation] [--images-scale X] [--page-images] [--chunker hierarchical|hybrid] [--chunk-tokenizer PATH] [--chunk-max-tokens N] [--no-chunk-merge-peers] [--pipeline standard|vlm] [--vlm-endpoint URL] [--vlm-model NAME] [--vlm-api-key TOKEN] [--vlm-prompt TEXT] [--vlm-max-tokens N] [--asr-model PRESET] [--asr-lang CODE] [--video-frames N|all] [--video-scene-threshold X] [--video-frame-max-side PX] [--video-frame-dedupe N] [--xbrl-taxonomy DIR] [--use-web-browser] [--enrich-picture-classes] [--enrich-code] [--enrich-formula] [--picture-ocr] [--picture-ocr-classes LABELS] [--picture-ocr-min-side N] [--picture-ocr-scale X] [--no-picture-images] [--picture-description] [--picture-description-endpoint URL] [--picture-description-model NAME] [--picture-description-api-key TOKEN] [--picture-description-prompt TEXT] [--picture-description-mode caption|ocr] [--picture-description-concurrency N] [--redact-pii] [--redact-mode label|pseudonym|fixed:TEXT] [--redact-kinds LIST] [--redact-pattern NAME=REGEX]... [--redact-images drop|box_out|keep] [--redact-allow-terms LIST] [--redact-deny-terms LIST] [--redact-allow-pattern REGEX]... [--redact-ner-min-score X] [--document-timeout SECONDS] [--abort-on-error] [--output-dirs auto|flat|mirror] [--archive-merge|--no-archive-merge] <input-file> | - | URL | SOURCE...
//!   --to FORMAT        repeatable (#491, like Python's `docling convert --to
//!                      md --to json`): each document converts once and is
//!                      written in every format named, `<stem>.md` +
//!                      `<stem>.json` under `--output` (several formats need
//!                      it — stdout carries one document). `--to md,json`
//!                      is the same; a format named twice is written once.
//!   SOURCE...          one positional file converts to stdout; several
//!                      positional sources — files, directories, quoted globs,
//!                      like Python's `docling convert a.docx sub/b.docx
//!                      --output out/` (#489) — are a batch and need
//!                      `--output`. A file lands in `--output` by stem, a
//!                      directory/glob keeps its tree; two sources that would
//!                      write the same output file are refused up front.
//!                      A source need not be a path (#668, MarkItDown's
//!                      `markitdown URL` / `cat f | markitdown`): `-` reads
//!                      stdin, `http(s)://…` is fetched (named by its
//!                      `Content-Disposition`, else the URL path; typed by
//!                      that name, else `Content-Type`, else the bytes'
//!                      signature — redirects followed, every hop through the
//!                      SSRF guard docling-serve uses, so a private/loopback
//!                      address needs `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1`;
//!                      bounded by `DOCLING_RS_MAX_FETCH_BYTES`), `data:` URLs
//!                      carry their document, `file:` URLs name a path. Any
//!                      of them lands in `--output` / `--output-file` like a
//!                      file (stdin as `document.<ext>`).
//!   --ipynb-outputs none|text
//!                      Jupyter notebooks (#673): which cell outputs the
//!                      document keeps — none (default: the notebook as
//!                      written; markdown cells, code cells as code blocks in
//!                      the kernel's language, raw cells as code blocks) or
//!                      text (stream and text/plain results as code blocks
//!                      after their cell, image outputs as pictures).
//!   --metadata none|json|frontmatter
//!   --track-changes accept|reject|all
//!   --extract-diagrams mermaid|none
//!                      DOCX DrawingML drawings (#858): a `mermaid` code item
//!                      after a drawing's picture with the flowchart its
//!                      connectors describe, when every connector names its
//!                      two end shapes (the default); none = the picture alone.
//!   --no-outline-level-headings
//!                      Word (DOCX/DOC): a style's outline level alone no
//!                      longer makes a heading (docling's
//!                      use_outline_level_for_headings=False, docling#4480) —
//!                      for templates whose body styles carry one for the TOC.
//!                      Document metadata (#679): the file's own (an EPUB's
//!                      Dublin Core, Office properties, ODF meta.xml, a
//!                      photo's EXIF, audio tags, a PDF's Info dictionary)
//!                      — none (default: nowhere, every export unchanged),
//!                      json (a `metadata` object in the docling JSON) or
//!                      frontmatter (that, and YAML front matter at the top
//!                      of the Markdown). `--redact-pii` masks the author
//!                      fields and drops the GPS position.
//!   --from FORMAT      the input format (an identifier from
//!                      `--list-input-formats`: `docx`, `pdf`, `html`, `md`…)
//!                      — for stdin, an extensionless URL or a file whose
//!                      extension lies. Overrides every other signal. `text`
//!                      is plain text / source code (#671): the listed code
//!                      extensions (py, rs, yaml, sh, …) become one code block
//!                      in that language, `log` and the rest paragraphs; a
//!                      file with no or an unlisted extension whose bytes are
//!                      text takes the same route, so only binary junk fails.
//!                      `.json` that is not a DoclingDocument, `.jsonl` and a
//!                      `.xml` of no known dialect are kept as one json / xml
//!                      code block.
//!   --mime-type TYPE   the media type (`application/pdf`, `text/markdown`),
//!                      consulted when the name has no extension — in place
//!                      of a missing or generic `Content-Type`. A source that
//!                      neither a name, a media type nor its bytes can type
//!                      is a usage error (exit 2) naming both flags.
//!   --no-url-fetch     refuse http(s) sources (a wrapper that passes
//!                      user-supplied arguments through must not fetch).
//!   --abort-on-error   stop the batch at the first failed file; by default a
//!                      failed file is reported and skipped and the exit code
//!                      is 1 at the end.
//!   --archive-merge    a `.zip` as one document (#677): per convertible entry,
//!                      in archive order, a `## <entry path>` heading and the
//!                      entry's content — MarkItDown's `_zip_converter` shape.
//!                      The default on stdout and for --output-file; under
//!                      --output DIR every entry is its own output unless the
//!                      flag is given (then `DIR/<archive>.<ext>`). Entries
//!                      that do not convert are reported on stderr (`skip:` /
//!                      `error:`) and left out; an archive with nothing
//!                      convertible fails. A ZIP inside the archive is read
//!                      under DOCLING_RS_ZIP_MAX_DEPTH (default 1 = shut, at
//!                      most 3), its entries as `inner.zip/<path>`.
//!   --no-archive-merge a lone `.zip` is a usage error without --output DIR
//!                      (the one-output-per-entry contract, #557).
//!   --input GLOB|DIR   batch mode (#205): convert every file the glob matches
//!                      (`--input '/data/reports/**/*.pdf'` — quote it so the
//!                      shell doesn't expand it) instead of one positional file.
//!                      A plain directory sweeps it recursively, taking every
//!                      file with a convertible extension. One warm process
//!                      converts them all: the PDF/image ML pipeline loads its
//!                      models once and is reused for every file, like
//!                      docling-serve's warm pipeline.
//!   --output DIR       where batch results land. The directory structure below
//!                      the pattern's static prefix is preserved (`a/b/x.pdf`
//!                      under `--input '/data/**/*.pdf'` becomes
//!                      `DIR/a/b/x.md`); extensions follow `--to` (`.md`,
//!                      `.json`, `.txt`, `.dclx`, `.chunks.json`). Also works with a
//!                      single positional input file. Output paths print to
//!                      stdout one per line; progress goes to stderr. A failed
//!                      file is reported and skipped (exit code 1 at the end).
//!   --jobs N           batch workers (default 1). Declarative formats convert
//!                      in parallel; PDF/image files share the one warm ML
//!                      pipeline (which parallelizes internally per document).
//!   --to md|json       output format (default: md). `json` emits docling-core's
//!                      native DoclingDocument JSON (export_to_dict); `text`
//!                      docling's plain text (`export_to_text`, #613: the
//!                      Markdown without `#`, emphasis, link URLs, code fences
//!                      or image placeholders; lists and tables stay); `vtt`
//!                      WebVTT subtitles (#614: a cue per timed text item —
//!                      an audio/video transcript's segments, a `.vtt` input's
//!                      cues; `WEBVTT` alone without them); `images`
//!                      (#243) skips conversion and rasterizes a PDF's pages to
//!                      `<stem>_page_NNNN.png` files (combines with `--pages`).
//!   --scale X          `--to images` render scale in pixels per PDF point:
//!                      0.1-4.0, default 2.0 (144 dpi, the ML pipeline's own
//!                      render scale).
//!   --pages A-B        convert only PDF pages A through B (1-based, inclusive;
//!                      a single page number also works). Skipped pages are
//!                      never rasterized, so a small window over a huge PDF is
//!                      cheap. Non-PDF inputs ignore this.
//!   --images MODE      picture handling for Markdown (mirrors docling's
//!                      image_mode): placeholder (default) | embedded | referenced.
//!                      `--to pandoc` defaults to embedded (#537): its AST is
//!                      meant for `pandoc -t docx`, which needs the pixels.
//!                      `referenced` writes image files under ./artifacts/ —
//!                      streamed to disk page by page, so image-heavy PDFs stay
//!                      memory-bounded. JSON always embeds extracted images as
//!                      data URIs.
//!   --fetch-images     for HTML/EPUB/MHTML/JATS, resolve external <img src> (data:
//!                      URIs, local files, http(s) URLs, EPUB/MHTML archive parts,
//!                      JATS <graphic> files) and embed the bytes. Off by default;
//!                      fetches over the network. The same as
//!                      `--image-sources remote` (#646).
//!   --image-sources none|embedded|local|remote
//!                      Which image references resolve (#646): `embedded` =
//!                      data: URIs and parts of the same container (EPUB/MHTML
//!                      entries, email cid: attachments) — no filesystem, no
//!                      network; `local` = plus files under the source file's
//!                      directory (never an absolute path, never outside it);
//!                      `remote` = plus http(s) fetches. HTML, EPUB, MHTML,
//!                      JATS, AsciiDoc, ODF, Markdown, email. Default none.
//!   --image-hosts LIST Comma-separated hosts a remote image fetch may reach
//!                      (exact, or `*.suffix`); redirects are held to it too.
//!   --max-images N     Images resolved per document; the rest stay
//!                      placeholders (default unlimited, DOCLING_RS_MAX_IMAGES).
//!   --max-image-bytes N
//!                      Largest image that resolves (default 32 MiB,
//!                      DOCLING_RS_MAX_IMAGE_BYTES).
//!   --max-image-total-mb N
//!                      Total resolved image bytes per document, in MiB
//!                      (default unlimited, DOCLING_RS_MAX_IMAGE_TOTAL_MB).
//!   --min-image-bytes N
//!                      Smallest image that resolves — skips spacer/tracking
//!                      pixels (default 0, DOCLING_RS_MIN_IMAGE_BYTES).
//!   --strict           cleaner, more conformant Markdown instead of byte-for-byte
//!                      docling-legacy output (Markdown only).
//!   --page-break-placeholder TEXT
//!                      insert TEXT between pages in the Markdown output
//!                      (docling's `export_to_markdown(page_break_placeholder=…)`,
//!                      e.g. "<!-- page break -->"). Pages come from the PDF /
//!                      image pipeline, slides, sheets and DjVu pages; a break
//!                      lands only between two rendered blocks on different
//!                      pages — never first or last, empty pages collapse. Off
//!                      by default: docling's Markdown carries no page breaks.
//!   --layers body|all|LIST, --traverse-pictures
//!                      docling's `export_to_markdown(included_content_layers=…,
//!                      traverse_pictures=…)`: layers for md/text/html (default
//!                      body; LIST from body,furniture,notes,invisible), picture
//!                      text for md/text.
//!   --no-stream        build the whole document before printing Markdown instead
//!                      of streaming it page by page. Streaming is the default for
//!                      Markdown (placeholder/embedded images); JSON and referenced
//!                      images always use the buffered path.
//!   --progress         progress on stderr (#783, docling's flags): a line per
//!   --no-progress      document, a bar for its pages and each enrichment pass,
//!                      drawn in place on a terminal. The default is --progress
//!                      when stderr is a terminal, --no-progress otherwise (a
//!                      pipe, a log); --progress forces the plain `done/total`
//!                      counts there. A batch logs its `start:` / `ok:` lines
//!                      as before and shows the bars between them.
//!   --no-table-former  skip loading/running the TableFormer table-structure
//!                      model for PDF/image input; tables fall back to simple
//!                      geometric reconstruction from cell positions. Faster
//!                      (no model load, no per-table inference) at the cost of
//!                      table fidelity — helps most in streaming mode.
//!   --video-frames N|all
//!                      Max frames sampled from a video input as timestamped
//!                      pictures (needs the ffmpeg binary; 0 = transcript
//!                      only; `all` = every distinct cut, #647). Default 8.
//!   --video-scene-threshold X
//!                      ffmpeg's scene score (0–1) a frame must exceed to be
//!                      a cut (#647). Default 0.27; 0.6 keeps hard cuts only.
//!   --video-frame-max-side PX
//!                      Downscale each sampled frame inside ffmpeg so its
//!                      longer side is at most PX (#647). Default 0 = the
//!                      source resolution.
//!   --video-frame-dedupe N
//!                      Drop a sampled frame whose difference hash is within
//!                      N bits (0–64) of a kept one (#647) — the same slide
//!                      after a fade. Unset = keep every frame; 4–6 is a
//!                      good distance.
//!   --xbrl-taxonomy DIR
//!                      Directory holding an XBRL instance's taxonomy — its
//!                      schema and linkbases at the paths the instance's
//!                      schemaRef names, plus taxonomy packages (.zip with
//!                      META-INF/catalog.xml) mapping the base taxonomies'
//!                      URLs for offline use (docling's XBRLBackendOptions
//!                      .taxonomy). Default: the instance's own directory.
//!   --asr-model NAME   ASR preset for audio inputs: whisper_tiny_en,
//!                      whisper_base_en, whisper_small_en, whisper_distil_small_en,
//!                      or parakeet_tdt_0.6b_v3 (NVIDIA Parakeet TDT, 25 European
//!                      languages detected by the model; #508) — models under
//!                      .models/asr/<preset>/; fetch them with
//!                      download_dependencies.sh --asr-model=<preset>
//!   --asr-lang CODE    transcription language for audio/video input — and the
//!                      caption language of a YouTube URL (#681): a Whisper
//!                      code (en, de, zh, ...) or auto (the default) to detect
//!                      it from the first 30 seconds. English-only presets
//!                      always transcribe English.
//!   --encoding LABEL   decode text inputs (Markdown, CSV, AsciiDoc, WebVTT,
//!                      LaTeX, XML, …) with this character encoding — a WHATWG
//!                      label such as shift_jis, koi8-r, windows-1251 or
//!                      latin1 — instead of detecting one (BOM, UTF-8, then
//!                      a statistical guess for a non-Latin encoding such as
//!                      shift_jis, gbk or koi8-r, else windows-1252 — #675);
//!                      bytes it cannot decode fail the
//!                      conversion (docling's TextBackendOptions.encoding)
//!   --force-full-page-ocr  OCR every PDF page even when it has a text layer
//!                      (docling's force_full_page_ocr) — for layers that lie:
//!                      broken encodings, forms with a few typed-in fields
//!   --pdf-word-gap adaptive|legacy|F
//!                      where a PDF's text layer splits words the file draws
//!                      with a cursor move and no space glyph (#921): adaptive
//!                      (default, DOCLING_RS_PDF_WORD_GAP) = docling-parse >= 7's
//!                      line-local threshold, what docling runs on; a factor F
//!                      in (0, 4] splits gaps wider than F x the mean advance
//!                      of the two flanking glyphs on lines without space
//!                      glyphs (0.1-0.23 for body text; 0.35 at least on
//!                      lines with them);
//!                      both keep letter-spaced (Sperrsatz) words whole;
//!                      legacy = docling-parse <= 6's running-mean rule
//!   --pdf-keep-typography
//!                      keep a PDF's typographic punctuation as drawn — low and
//!                      curly quotes, en/em dashes, ellipsis — instead of
//!                      folding it to ASCII the way docling-parse's sanitizer
//!                      does (#921; DOCLING_RS_PDF_KEEP_TYPOGRAPHY)
//!   --pdf-report-repairs
//!                      print what the PDF reader repaired to read the input
//!                      (a stream whose /Length misses its endstream, a padded
//!                      xref) to stderr, one `repaired:` line each (#934)
//!   --ocr-textline-orientation
//!                      RapidOCR's line orientation handling (#666): crop each
//!                      detected line along its rotated rectangle, turn tall
//!                      crops 90°, and turn upside-down lines with the 0/180
//!                      classifier (.models/ocr_textline_ori.onnx; missing, it
//!                      warns and skips that step) — vertical axis titles,
//!                      rotated headers, skewed and upside-down lines read.
//!                      PP-OCR only; changes scanned-page output too. Off by
//!                      default (DOCLING_RS_OCR_TEXTLINE_ORIENTATION).
//!   --no-text-panels   keep every detected picture as a picture — disable the
//!                      #157 demotion of uncaptioned text-panel pictures into
//!                      paragraphs (the escape hatch for image-extraction
//!                      workflows, #173)
//!   --heading-hierarchy  infer PDF/image section-header levels after assembly
//!                      (#302, docling's HeadingHierarchyModel): PDF bookmarks
//!                      are authoritative, legal/outline numbering covers the
//!                      rest, font style breaks the ties. Off by default —
//!                      headings then keep the flat level docling emits
//!   --text-layer-only  skip layout detection, OCR, and TableFormer entirely for
//!                      PDF/image input — no model load or inference at all
//!                      (`--no-ocr` before 2.0, #611). `--text-layer-only=auto`
//!                      decides per document: the text layer alone when it
//!                      carries the document, the full pipeline (OCR) when it
//!                      does not — a scanned file, or a vestigial layer of a
//!                      few typed-in form fields.
//!                      Emits the embedded text layer as flat paragraphs in
//!                      reading order (no headings/lists/tables/pictures). The
//!                      fastest option, but a scanned/image-only PDF (no
//!                      embedded text layer) yields no text — convert those
//!                      without this flag. Also works when the models aren't
//!                      installed at all (e.g. a bare `cargo install`):
//!                      a digital PDF falls back to the pure-Rust text-layer
//!                      extraction.
//!   --use-web-browser  pre-render HTML/MHTML/EPUB in the system Chromium (driven
//!                      from Rust) so stylesheet-driven `display:none` elements
//!                      (e.g. a collapsed nav menu) are dropped before parsing.
//!                      Requires building with `--features web-browser`.
//!   --enrich-picture-classes
//!                      classify each detected picture (PDF/image input) with the
//!                      DocumentFigureClassifier model; the 26-class prediction
//!                      distribution lands in the JSON picture item (docling's
//!                      do_picture_classification). Needs
//!                      .models/picture_classifier.onnx.
//!   --enrich-code      rewrite detected code blocks (and detect their language)
//!                      with the CodeFormulaV2 VLM (docling's do_code_enrichment).
//!                      Needs .models/code_formula/. Slow on CPU: an autoregressive
//!                      generation per code block.
//!   --enrich-formula   decode display formulas to LaTeX with CodeFormulaV2
//!                      (docling's do_formula_enrichment); Markdown then renders
//!                      $$latex$$ instead of the formula placeholder comment.
//!   --picture-ocr      OCR the pictures embedded in non-PDF documents — DOCX/PPTX
//!                      screenshots, HTML figures, sampled video frames — and
//!                      attach the text to the picture as docling's description
//!                      annotation (#645): Markdown prints it after the caption,
//!                      before the image placeholder; JSON carries
//!                      meta.description. Needs the OCR models (.models/ocr_rec*,
//!                      ocr_det.onnx for line detection; --ocr-engine tesseract
//!                      works too); under --no-ocr / --text-layer-only it warns
//!                      and reads nothing. PDF/image pages are OCR'd by the
//!                      pipeline already and are left alone.
//!   --picture-ocr-classes LABELS
//!                      only read the pictures the DocumentFigureClassifier
//!                      labels as one of these (comma-separated, e.g.
//!                      screenshot_from_computer,screenshot_from_manual); default:
//!                      every picture. Needs .models/picture_classifier.onnx —
//!                      missing, the filter is waived with a warning.
//!   --picture-ocr-min-side N
//!                      skip pictures whose smaller side is under N px (icons,
//!                      bullets, rules). Default 32 (DOCLING_RS_PICTURE_OCR_MIN_SIDE).
//!   --picture-ocr-scale X
//!                      read pictures at X px per picture pixel, shrunk to
//!                      RapidOCR's 2000 px longer side (#666): 1.0 = native
//!                      pixels, as RapidOCR reads them. Default: the image-input
//!                      rule (--ocr-scale, else 3x under the cap;
//!                      DOCLING_RS_PICTURE_OCR_SCALE). Pages are unaffected.
//!   --picture-description
//!                      describe every picture through an OpenAI-compatible
//!                      vision endpoint (#670) — the embedded pictures of
//!                      DOCX/PPTX/XLSX/HTML, a PDF's figures, video frames, and
//!                      an image input as a whole — and attach the answer as
//!                      the picture's description, where --picture-ocr puts
//!                      its text: MarkItDown's llm_client/llm_model/llm_prompt
//!                      and its markitdown-ocr plugin. --picture-ocr-min-side
//!                      and --picture-ocr-classes select the pictures here too;
//!                      identical images are sent once. An endpoint error
//!                      warns; the conversion goes on.
//!   --picture-description-endpoint URL
//!                      the server's /v1 base or the full chat/completions URL;
//!                      default --vlm-endpoint, else DOCLING_RS_VLM_ENDPOINT.
//!   --picture-description-model NAME
//!                      default --vlm-model, else DOCLING_RS_VLM_MODEL.
//!   --picture-description-api-key TOKEN
//!                      Bearer token; default --vlm-api-key, else
//!                      DOCLING_RS_VLM_API_KEY.
//!   --picture-description-prompt TEXT
//!                      the instruction sent with every picture; default: the
//!                      mode's prompt.
//!   --picture-description-mode caption|ocr
//!                      caption (default): MarkItDown's "Write a detailed
//!                      caption for this image."; ocr: markitdown-ocr's
//!                      "Extract all text from this image …" — LLM-vision OCR
//!                      where the local models read poorly.
//!   --picture-description-concurrency N
//!                      pictures in flight at once (default 1).
//!   --no-picture-images
//!                      drop the embedded image bytes from every picture after
//!                      the enrichment (keep_picture_images=false): a slim
//!                      JSON/DCLX and placeholder-only Markdown, the OCR text kept.
//!   --redact-pii       redact personal data from the converted document before
//!                      any output is written (#621): e-mail, phone, card
//!                      numbers (Luhn), IBANs (mod-97), IP addresses, URL
//!                      credentials, national IDs (US SSN, UK NINO, Aadhaar)
//!                      and — with the NER model under .models/ner/ — names,
//!                      organizations and locations; every string of the
//!                      document model (tree, links, captions, hrefs, code,
//!                      comments, table cells) is rewritten, so every format
//!                      written comes out clean. Buffered conversions print the
//!                      counts per label on stderr. A docling.rs extension; off
//!                      by default.
//!   --redact-mode MODE label (default: [EMAIL], [PERSON]) | pseudonym
//!                      ([EMAIL_1], [EMAIL_2]: one number per distinct value,
//!                      consistent within the document) | fixed:TEXT.
//!   --redact-kinds LIST
//!                      comma-separated kinds to redact (email, phone,
//!                      credit_card, iban, ip_address, url_credentials,
//!                      national_id, person, organization, location, address);
//!                      default: every kind.
//!   --redact-pattern NAME=REGEX
//!                      an extra pattern, redacted as [NAME] (the match, or
//!                      capture group 1); repeatable.
//!   --redact-images MODE
//!                      what happens to embedded images and page renders: drop
//!                      (default — nothing unread leaves the document) |
//!                      box_out (OCR each image and paint over the lines that
//!                      carry a value; needs the OCR models, --no-stream for
//!                      PDFs) | keep.
//!   --redact-allow-terms LIST
//!                      comma-separated terms never redacted even when a
//!                      detector flags them (support@example.com, the
//!                      company's own name).
//!   --redact-deny-terms LIST
//!                      comma-separated terms always redacted, as [REDACTED].
//!   --redact-allow-pattern REGEX
//!                      a span matching this regex in full is never redacted
//!                      — the document's own part numbers, record IDs;
//!                      repeatable.
//!   --redact-ner-min-score X
//!                      the NER detector's minimum confidence, 0-1 (default 0.85).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use docling::chunks::{ChunkOptions, ChunkerKind};
use docling::{
    ContentLayers, DoclingDocument, DocumentConverter, HtmlExportOptions, ImageMode, InputFormat,
    MarkdownExportOptions, Pipeline, SourceDocument,
};

/// The PII pass's counts for stderr: `EMAIL=2 PHONE=1 (3 spans)` (#621).
fn redaction_summary(report: &docling::RedactionReport) -> String {
    let per_label: Vec<String> = report
        .counts
        .iter()
        .map(|(label, n)| format!("{label}={n}"))
        .collect();
    format!(
        "{} ({} span{})",
        if per_label.is_empty() {
            "nothing".to_string()
        } else {
            per_label.join(" ")
        },
        report.total,
        if report.total == 1 { "" } else { "s" }
    )
}

/// `--version` output: the crate version plus the optional features this
/// binary was actually built with. The feature list is the useful half — the
/// execution provider a build can select, whether `serve` is compiled in, and
/// whether tokenizer-backed chunking is available all follow from it, and all
/// three are routine support questions (#324 debugging started exactly here).
fn version_line() -> String {
    let mut features: Vec<&str> = Vec::new();
    if cfg!(feature = "chunking") {
        features.push("chunking");
    }
    if cfg!(feature = "serve") {
        features.push("serve");
    }
    if cfg!(feature = "mcp") {
        features.push("mcp");
    }
    if cfg!(feature = "heif") {
        features.push("heif");
    }
    if cfg!(feature = "web-browser") {
        features.push("web-browser");
    }
    if cfg!(feature = "cuda") {
        features.push("cuda");
    }
    if cfg!(feature = "tensorrt") {
        features.push("tensorrt");
    }
    if cfg!(feature = "directml") {
        features.push("directml");
    }
    if cfg!(feature = "coreml") {
        features.push("coreml");
    }
    if cfg!(feature = "xnnpack") {
        features.push("xnnpack");
    }
    let version = env!("CARGO_PKG_VERSION");
    let mut line = if features.is_empty() {
        format!("docling-rs {version}")
    } else {
        format!("docling-rs {version} ({})", features.join(", "))
    };
    // The runtime behind the ML stages (#523): the linked ONNX Runtime, or
    // the library an ort-load-dynamic build found.
    if let Some((major, minor)) = docling::onnx_runtime_version() {
        line.push_str(&format!("; ONNX Runtime {major}.{minor}"));
    }
    line
}

/// `--list-input-formats` (#603): the file extensions this binary converts —
/// the converter's list ([`docling::DocumentConverter::supported_extensions`]:
/// the build's formats, those behind a missing cargo feature left out, and
/// any registered backend's, #680 — a binary built on the library with its
/// own backends lists them here) plus `zip`, which the CLI itself expands
/// into its documents (#557, with `--output`). Sorted, unique, lowercase, no
/// dot: `grep -qx rtf` answers "does this binary take .rtf".
fn input_format_list() -> Vec<String> {
    let mut list = docling::DocumentConverter::new().supported_extensions();
    list.push("zip".to_string());
    list.sort_unstable();
    list.dedup();
    list
}

/// `--list-output-formats` (#603): the `--to` values, sorted like Pandoc's
/// list (the help text keeps its own reading order). The `markdown` alias of
/// `md` is accepted but not listed — one identifier per format.
fn output_format_list() -> Vec<&'static str> {
    let mut list = docling::OUTPUT_FORMATS.to_vec();
    list.sort_unstable();
    list
}

/// One-line synopsis — the `usage:` prefix an argument error prints.
const USAGE: &str = "usage: docling-rs [OPTIONS] <input-file> | - | URL\n       docling-rs [OPTIONS] --output DIR SOURCE...\n       docling-rs --input GLOB|DIR --output DIR [OPTIONS]\n       docling-rs serve [SERVE OPTIONS]\n       docling-rs mcp [MCP OPTIONS]";

/// `--help`: the synopsis plus every flag, grouped. Kept in sync with the
/// module doc comment above, which carries the long-form rationale.
const HELP: &str = "\
Convert documents to Markdown, JSON, plain text, DocLang, LaTeX, Pandoc AST, WebVTT or chunks.

OUTPUT
  --to md|json|html|text|dclx|chunks|images|latex|pandoc|vtt   output format (default: md); repeat
                          it (or comma-separate) to write several — needs --output;
                          vtt = WebVTT subtitles from a transcript's (or a .vtt's) cues
  --strict                cleaner, more conformant Markdown (Markdown only)
  --page-break-placeholder TEXT   insert TEXT between pages (Markdown only, e.g. <!-- page break -->)
  --layers body|all|LIST  content layers md/text/html render (default body; LIST from
                          body,furniture,notes,invisible)
  --traverse-pictures     render text nested in pictures (md/text)
  --images MODE           picture handling: placeholder (default; embedded for --to pandoc)
                          | embedded | referenced (docling's --image-export-mode)
  --pandoc-api-version V  fail unless the Pandoc AST is this API (`--to pandoc`; only 1.23)
  --pandoc-heading-depth docling|source  Pandoc `Header` depth of a section heading (#848):
                          docling's title-reserving n+1 (default) or the source's own n
  --compact-tables        render Markdown tables without width padding
  --picture-alt-text      render pictures' alt text (DOCX/PPTX/XLSX, #676) in
                          Markdown and HTML; --strict implies it, JSON has it always
  --no-stream             build the whole document before printing
  --progress              progress on stderr: a bar for the pages and each
  --no-progress           enrichment pass (default: on when stderr is a terminal)

INPUT SELECTION
  SOURCE...               one file converts to stdout; several files, directories
                          or quoted globs are a batch and need --output; `-` is stdin,
                          http(s)://, data: and file: URLs are sources too (#668)
  --from FORMAT           input format of stdin / a URL / a mislabelled file (an
                          identifier from --list-input-formats: docx, pdf, html, md…;
                          text = plain text / source code, also the fallback for any
                          other extension whose bytes are text)
  --mime-type TYPE        media type deciding the format when the name has no
                          extension (in place of a missing Content-Type)
  --no-url-fetch          refuse http(s) sources
  --input GLOB|DIR        batch mode: convert everything the glob/directory matches
  --output DIR            where batch (or single-file) results are written
  --output-file PATH      write the one result to exactly PATH (one input document,
                          one --to format; docling's --output-file)
  --jobs N                batch workers (default 1)
  --abort-on-error        stop the batch at the first failed file (default: skip it;
                          a timed-out document counts as failed under this flag)
  --archive-merge         a .zip as one document (#677): `## <entry path>` then the
                          entry's content, per convertible entry; the default on
                          stdout and for --output-file, opt-in under --output DIR
                          (DIR/<archive>.<ext> instead of one output per entry)
  --no-archive-merge      a lone .zip needs --output DIR (one output per entry)
  --output-dirs MODE      where several inputs land under --output (#496): auto
                          (default: a directory/glob mirrors its tree, a plain file
                          lands by stem), flat (every output <stem>.<ext> directly in
                          --output), mirror (every input's path relative to the
                          current directory, inputs outside it are an error)
  --document-timeout SECONDS   per-document budget for the PDF pipeline (docling's
                          document_timeout, #497): checked between pages; once
                          spent, the pages done so far are the (partial) document
  --pages A-B             convert only PDF pages A..B (1-based, inclusive;
                          docling's --page-range)
  --scale X               `--to images` render scale, px per PDF point (0.1-4.0, default 2.0)

FORMAT OPTIONS
  --fetch-images          resolve external <img src> for HTML/EPUB/MHTML/JATS (network access)
                          (= --image-sources remote)
  --image-sources MODE    which image references resolve: none (default), embedded
                          (data: URIs + same-container parts), local (+ files under
                          the source's directory), remote (+ http(s) fetches)
  --image-hosts LIST      hosts a remote image fetch may reach (exact or *.suffix)
  --max-images N          images resolved per document (default unlimited)
  --max-image-bytes N     largest image that resolves (default 32 MiB)
  --max-image-total-mb N  total resolved image bytes per document (default unlimited)
  --min-image-bytes N     smallest image that resolves (default 0)
  --list-attachments      append an Attachments section for .eml/.msg
  --skip-empty-cells      omit empty cells from XLSX/XLS grids
  --ebcdic-layout JSON|PATH   EBCDIC copybook layout
  --encoding LABEL        character encoding of text inputs (shift_jis, koi8-r,
                          windows-1251, …); default: detect (BOM, UTF-8, a guess
                          for a non-Latin encoding, else cp1252)
  --ipynb-outputs MODE    Jupyter notebooks: none (default, the notebook as
                          written) | text (stream/text outputs as code blocks,
                          image outputs as pictures)
  --metadata MODE         document metadata (EPUB/Office/ODF properties, EXIF,
                          audio tags, PDF Info): none (default) | json (a
                          `metadata` object in the JSON) | frontmatter (YAML
                          front matter at the top of the Markdown as well)
  --track-changes MODE    tracked changes in DOCX/DOC/ODT (Pandoc's --track-changes):
                          accept (insertions shown, deletions left out) | reject (the
                          reverse) | all (both, typed with author/date in the Pandoc AST);
                          unset = each reader's own view; a mode a reader lacks is an error
  --extract-diagrams FMT  DOCX DrawingML drawings (#858): mermaid (default) adds a
                          `mermaid` code item after a drawing's picture with the flowchart
                          its shapes and connectors describe — only when every connector
                          names its two end shapes; a drawing laid by eye gets no graph.
                          none = the picture alone
  --no-outline-level-headings  Word (DOCX/DOC): an outline level alone no longer makes a
                          heading (docling's use_outline_level_for_headings=False) — for
                          templates whose body styles carry one only for the TOC
  --use-web-browser       pre-render HTML with a headless browser (feature `web-browser`)

PDF / IMAGE PIPELINE
  --ocr-engine ppocr|tesseract   OCR engine (default: ppocr; tesseract = the system binary)
  --no-table-former       skip the TableFormer model (geometric tables instead;
                          docling's --no-tables)
  --no-ocr                never run OCR, keep layout + tables (docling's --no-ocr;
                          --skip-ocr is the same)
  --text-layer-only       no models at all: the embedded text layer as flat
                          paragraphs (what --no-ocr did before 2.0)
  --text-layer-only=auto  per document: the text layer alone when it carries
                          the document, the full pipeline (OCR) when it does not
  --password PASSWORD     password of an encrypted PDF or Office document
                          (.docx/.xlsx/.pptx/.doc/.xls/.ppt; also
                          --pdf-password, docling's spelling)
  --password-file PATH    the same password, read from the file's first line
                          (keeps it out of the process list)
  --force-full-page-ocr   OCR the whole page, discarding the text layer
                          (docling's deprecated --force-ocr; = --ocr-mode full_page)
  --no-text-panels        disable the text-panel heuristic
  --heading-hierarchy     infer heading levels from font weight/slant/case
  --ocr-lang LANG         OCR recognition model (default: en): en | ch, or a
                          BCP-47 tag for English/Chinese (en-US, eng, zh,
                          zh-Hans, zh-TW; docling's iso: prefix accepted)
  --ocr-mode MODE         auto (default) | full_page | layout_regions
  --ocr-scale X           OCR input scale in px per point
  --pdf-word-gap RULE     where the PDF text layer splits words without a space
                          glyph: adaptive (default, docling-parse >= 7,
                          line-local) | legacy (docling-parse <= 6) | a factor
                          F: split gaps > F x the flanking glyphs' mean advance
  --pdf-keep-typography   keep the PDF text layer's typographic punctuation as
                          drawn („ “ ‚ ‘, dashes, …) instead of folding it to
                          ASCII as docling does
  --pdf-report-repairs    print what the PDF reader repaired to read the input
                          (a stream /Length that misses its endstream, a padded
                          xref) to stderr; off by default
  --ocr-textline-orientation
                          RapidOCR's line orientation: oriented crops, 90° turn of
                          tall lines, 0/180 classifier (off by default)
  --picture-ocr-scale X   picture OCR scale in px per picture pixel under the
                          2000 px cap (1.0 = RapidOCR's native pixels)
  --images-scale X        picture crops (and page images) in px per point,
                          0.1-4.0 (docling's images_scale; default: the 2.0 render)
  --page-images           keep each page's render as the JSON page image
                          (docling's generate_page_images)
  --enrich-picture-classes | --enrich-code | --enrich-formula
                          optional enrichment models (off by default)
  --picture-description   describe every picture (any format) through an
                          OpenAI-compatible vision endpoint; the answer is the
                          picture's description, as --picture-ocr writes it
  --picture-description-endpoint URL | --picture-description-model NAME |
  --picture-description-api-key TOKEN
                          default: the --vlm-* values, else DOCLING_RS_VLM_*
  --picture-description-mode caption|ocr
                          MarkItDown's caption prompt (default) or
                          markitdown-ocr's text-extraction prompt
  --picture-description-prompt TEXT | --picture-description-concurrency N
                          your own instruction; pictures in flight (default 1)

CHUNKING (`--to chunks`)
  --chunker hierarchical|hybrid
  --chunk-tokenizer PATH  tokenizer.json for the hybrid chunker
  --chunk-max-tokens N    chunk budget
  --no-chunk-merge-peers  keep sibling chunks separate

VLM PIPELINE
  --pipeline standard|vlm
  --vlm-endpoint URL | --vlm-model NAME | --vlm-api-key TOKEN
  --vlm-prompt TEXT | --vlm-max-tokens N

AUDIO / VIDEO
  --asr-model PRESET      ASR preset for audio/video transcription (whisper_*,
                          parakeet_tdt_0.6b_v3)
  --asr-lang CODE         force a transcription language (audio/video; a YouTube
                          URL's caption track, #681)
  --video-frames N|all    sample N key frames from a video (all = every cut)
  --video-scene-threshold X
                          scene score a frame must exceed to be a cut (0.27)
  --video-frame-max-side PX
                          downscale frames to at most PX on the longer side
  --video-frame-dedupe N  drop frames within N hash bits of a kept one
  --xbrl-taxonomy DIR     taxonomy directory for XBRL instances (default: the
                          instance's own directory)

OTHER
  -h, --help              print this help
  -V, --version           print the version and compiled-in features
  --list-input-formats    print the input file extensions this binary converts, one per line
  --list-output-formats   print the --to formats, one per line

Environment knobs (execution providers, model paths, worker counts) are
documented in the README: https://github.com/docling-project/docling.rs";

fn main() -> ExitCode {
    // #664: a CUDA build started by bare name re-executes under its absolute
    // path, or ONNX Runtime looks for its providers in the working directory.
    docling::reexec_for_provider_libraries();
    // `--help` / `--version` before anything else: they must answer on a
    // binary whose models are missing, and a smoke test that runs
    // `docling-rs --version` should not be told to convert a file named
    // `--version` (issue-#333's CUDA image test tripped over exactly that).
    // `serve` keeps its own `--help`, so only scan the global position here.
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        // `mcp` (#669) likewise answers its own `--help`.
        let is_subcommand = matches!(args.first().map(String::as_str), Some("serve" | "mcp"));
        if !is_subcommand {
            if args.iter().any(|a| a == "--version" || a == "-V") {
                println!("{}", version_line());
                return ExitCode::SUCCESS;
            }
            if args.iter().any(|a| a == "--help" || a == "-h") {
                println!("{}", version_line());
                println!();
                println!("{USAGE}");
                println!();
                println!("{HELP}");
                return ExitCode::SUCCESS;
            }
            // #603: Pandoc's discovery flags — machine-readable, one
            // identifier per line, sorted, exit 0; the rest of the command
            // line is ignored, as Pandoc does.
            if args.iter().any(|a| a == "--list-input-formats") {
                for ext in input_format_list() {
                    println!("{ext}");
                }
                return ExitCode::SUCCESS;
            }
            if args.iter().any(|a| a == "--list-output-formats") {
                for format in output_format_list() {
                    println!("{format}");
                }
                return ExitCode::SUCCESS;
            }
        }
    }

    // `docling-rs serve …` — the HTTP conversion API (issue-#78 analogue of
    // docling-serve). Compiled in only with `--features serve`; the flags
    // after `serve` are the `docling-serve` binary's (see that crate).
    {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some("serve") {
            // #263: a long-lived server defaults the ONNX CPU arena OFF — measured
            // here, a warm server's retained RSS drops ~3x (2.0 GB -> 0.7 GB after
            // large-PDF requests) at no measurable latency cost, and stops ratcheting
            // with every new page shape. Explicit DOCLING_RS_NO_ARENA=0 restores the
            // arena. Set before any session loads; the process is single-threaded
            // this early.
            if std::env::var_os("DOCLING_RS_NO_ARENA").is_none() {
                std::env::set_var("DOCLING_RS_NO_ARENA", "1");
            }

            return run_serve(args.collect());
        }
    }

    // `docling-rs mcp …` — the Model Context Protocol server (#669): stdio for
    // desktop agents, Streamable HTTP with `--http`. In the default build;
    // the flags after `mcp` are its own (see `mcp.rs`).
    {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some("mcp") {
            // The server defaults DOCLING_RS_NO_ARENA itself (`mcp::run_with`).
            return run_mcp(args.collect());
        }
    }

    // Every conversion option lands in the shared `ConvertOptions` (#577):
    // the flags below only parse their values; the rejection rules and the
    // mapping onto the converter are the library's, the same the other
    // surfaces (serve, the bindings, the C ABI) apply.
    let mut opts = docling::ConvertOptions::default();
    // `--to` is repeatable (#491, Python's `--to md --to json`): every
    // occurrence — or comma-separated entry — is collected here and resolved
    // to a de-duplicated format list below; empty means Markdown.
    let mut to: Vec<String> = Vec::new();
    // `None` = not given: placeholder, except embedded for `--to pandoc` (#537).
    let mut images: Option<String> = None;
    let mut no_stream = false;
    // `--progress` / `--no-progress` (#783, docling's flags): `None` = on
    // when stderr is a terminal.
    let mut progress: Option<bool> = None;
    // `--archive-merge` / `--no-archive-merge` (#677): `None` is the default —
    // one document on stdout and for `--output-file`, one output per entry
    // under `--output DIR`.
    let mut archive_merge: Option<bool> = None;
    let mut export = ExportLayers::default();
    let mut bench_warm: Option<usize> = None;
    let mut scale: f32 = 2.0;
    let mut chunk_opts = docling::chunks::ChunkOptions::default();
    // Positional sources (#489): files, directories or quoted globs, any
    // number of them — one is the classic single-file (stdout) mode, more
    // than one (or a directory) is a batch and needs `--output`.
    let mut paths: Vec<String> = Vec::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut abort_on_error = false;
    let mut output: Option<String> = None;
    // `--output-file PATH` (#611, docling's flag): the one result, written to
    // exactly this path.
    let mut output_file: Option<String> = None;
    let mut output_dirs = OutputDirs::Auto;
    // `--from` / `--mime-type` / `--no-url-fetch` (#668): how a source that
    // carries no usable extension — stdin, a bare URL — is typed.
    let mut hints = SourceHints::default();
    let mut jobs: usize = 1;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => opts.strict = Some(true),
            "--fetch-images" => opts.fetch_images = Some(true),
            // #646: the image-source policy; the tier's spelling is checked
            // by `ConvertOptions::validate`.
            "--image-sources" => match args.next() {
                Some(v) if !v.trim().is_empty() => opts.image_sources = Some(v),
                _ => {
                    eprintln!("error: --image-sources needs none, embedded, local or remote");
                    return ExitCode::from(2);
                }
            },
            "--image-hosts" => match args.next() {
                Some(v) if !v.trim().is_empty() => {
                    opts.image_hosts = Some(
                        v.split(',')
                            .map(|h| h.trim().to_string())
                            .filter(|h| !h.is_empty())
                            .collect(),
                    )
                }
                _ => {
                    eprintln!("error: --image-hosts needs a comma-separated host list");
                    return ExitCode::from(2);
                }
            },
            "--max-images" => match args.next().map(|v| v.trim().parse::<usize>()) {
                Some(Ok(n)) => opts.max_images = Some(n),
                _ => {
                    eprintln!("error: --max-images needs a non-negative integer");
                    return ExitCode::from(2);
                }
            },
            "--max-image-bytes" => match args.next().map(|v| v.trim().parse::<u64>()) {
                Some(Ok(n)) => opts.max_image_bytes = Some(n),
                _ => {
                    eprintln!("error: --max-image-bytes needs a number of bytes");
                    return ExitCode::from(2);
                }
            },
            "--max-image-total-mb" => match args.next().map(|v| v.trim().parse::<u64>()) {
                Some(Ok(n)) => opts.max_image_total_mb = Some(n),
                _ => {
                    eprintln!("error: --max-image-total-mb needs a number of MiB");
                    return ExitCode::from(2);
                }
            },
            "--min-image-bytes" => match args.next().map(|v| v.trim().parse::<u64>()) {
                Some(Ok(n)) => opts.min_image_bytes = Some(n),
                _ => {
                    eprintln!("error: --min-image-bytes needs a number of bytes");
                    return ExitCode::from(2);
                }
            },
            // #251: append an Attachments section to converted emails
            // (.eml/.msg) — names and content types only.
            "--list-attachments" => opts.list_attachments = Some(true),
            // Sparse-spreadsheet compaction (#271, docling.rs extensions):
            // omit empty cells from XLSX/XLS table grids, and/or render all
            // Markdown tables compact (no width padding).
            "--skip-empty-cells" => opts.skip_empty_cells = Some(true),
            "--compact-tables" => opts.compact_tables = Some(true),
            "--picture-alt-text" => opts.picture_alt_text = Some(true),
            // docling's `page_break_placeholder`: the text that separates
            // pages in Markdown (an empty TEXT is allowed — upstream then
            // leaves a doubled blank line between pages).
            "--page-break-placeholder" => match args.next() {
                Some(v) => opts.page_break_placeholder = Some(v),
                None => {
                    eprintln!("error: --page-break-placeholder needs the text to insert");
                    return ExitCode::from(2);
                }
            },
            // #252: EBCDIC copybook layout — inline JSON or a file path
            // (default: the <stem>.layout.json sidecar next to the source).
            "--ebcdic-layout" => match args.next() {
                Some(v) => opts.ebcdic_layout = Some(v),
                None => {
                    eprintln!("error: --ebcdic-layout needs a JSON string or file path");
                    return ExitCode::from(2);
                }
            },
            "--no-stream" => no_stream = true,
            "--progress" => progress = Some(true),
            "--no-progress" => progress = Some(false),
            "--archive-merge" => archive_merge = Some(true),
            "--no-archive-merge" => archive_merge = Some(false),
            // Export-only (docling-core's `included_content_layers` /
            // `traverse_pictures`), streamed and buffered alike.
            "--layers" => match args.next().map(|v| ContentLayers::parse_list(&v)) {
                Some(Ok(l)) if l != ContentLayers::NONE => export.layers = l,
                _ => {
                    eprintln!("error: --layers needs body, all or a list of body,furniture,notes,invisible");
                    return ExitCode::from(2);
                }
            },
            "--traverse-pictures" => export.traverse_pictures = true,
            // `--no-tables` is docling's spelling (`do_table_structure=False`, #611).
            "--no-table-former" | "--no-tables" => opts.no_table_former = Some(true),
            // Since 2.0 (#611) `--no-ocr` is docling's: keep layout +
            // TableFormer, never OCR (`do_ocr=False`). `--skip-ocr` is its
            // pre-2.0 name; the old skip-everything fast path is
            // `--text-layer-only`.
            "--no-ocr" | "--skip-ocr" => opts.no_ocr = Some(true),
            "--text-layer-only" => opts.text_layer_only = Some(docling::TextLayerOnly::On),
            // `--text-layer-only=auto` (or `=true` / `=false`): the `=` form
            // only — a bare value after the flag would be read as a source
            // path.
            s if s.starts_with("--text-layer-only=") => {
                match s["--text-layer-only=".len()..].parse::<docling::TextLayerOnly>() {
                    Ok(mode) => opts.text_layer_only = Some(mode),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(2);
                    }
                }
            }
            // The password of an encrypted PDF or Office document (#611,
            // #625); `--pdf-password` is docling's PDF-only spelling.
            "--password" | "--pdf-password" => match args.next() {
                Some(v) => opts.password = Some(v),
                None => {
                    eprintln!("error: {arg} needs the document's password");
                    return ExitCode::from(2);
                }
            },
            // The same password read from a file (#625): a command-line
            // argument is visible to every user of the machine (`ps`), a
            // file can be private. Its first line, without the line break.
            "--password-file" => match args
                .next()
                .map(|p| std::fs::read_to_string(&p).map(|t| (p, t)))
            {
                Some(Ok((_, text))) => {
                    let line = text.lines().next().unwrap_or_default();
                    opts.password = Some(line.to_string());
                }
                Some(Err(e)) => {
                    eprintln!("error: --password-file: {e}");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --password-file needs a path");
                    return ExitCode::from(2);
                }
            },
            "--force-full-page-ocr" => opts.force_full_page_ocr = Some(true),
            "--no-text-panels" => opts.no_text_panels = Some(true),
            "--heading-hierarchy" => opts.heading_hierarchy = Some(true),
            "--use-web-browser" => opts.use_web_browser = Some(true),
            // Opt-in enrichment models (docling CLI flag names): picture
            // classification, code rewrite + language, formula LaTeX.
            "--enrich-picture-classes" => opts.do_picture_classification = Some(true),
            "--enrich-code" => opts.do_code_enrichment = Some(true),
            "--enrich-formula" => opts.do_formula_enrichment = Some(true),
            // Picture OCR for non-PDF documents (#645) and its filters.
            "--picture-ocr" => opts.do_picture_ocr = Some(true),
            "--picture-ocr-classes" => match args.next() {
                Some(v) => opts.picture_ocr_classes = Some(v),
                None => {
                    eprintln!(
                        "error: --picture-ocr-classes needs a comma-separated list of labels"
                    );
                    return ExitCode::from(2);
                }
            },
            "--picture-ocr-scale" => match args.next().map(|v| v.trim().parse::<f32>()) {
                Some(Ok(s)) => opts.picture_ocr_scale = Some(s),
                Some(_) => {
                    eprintln!("error: --picture-ocr-scale needs a positive number");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --picture-ocr-scale needs a value");
                    return ExitCode::from(2);
                }
            },
            "--ocr-textline-orientation" => opts.ocr_textline_orientation = Some(true),
            // Keep the PDF text layer's typographic punctuation as drawn (#921).
            "--pdf-keep-typography" => opts.pdf_keep_typography = Some(true),
            "--pdf-report-repairs" => opts.pdf_report_repairs = Some(true),
            "--picture-description" => opts.do_picture_description = Some(true),
            "--picture-description-endpoint" => opts.picture_description_endpoint = args.next(),
            "--picture-description-model" => opts.picture_description_model = args.next(),
            "--picture-description-api-key" => opts.picture_description_api_key = args.next(),
            "--picture-description-prompt" => opts.picture_description_prompt = args.next(),
            "--picture-description-mode" => opts.picture_description_mode = args.next(),
            "--picture-description-concurrency" => {
                match args.next().map(|v| v.trim().parse::<usize>()) {
                    // 0 is rejected by `validate()` below, like the other surfaces do.
                    Some(Ok(n)) => opts.picture_description_concurrency = Some(n),
                    _ => {
                        eprintln!(
                            "error: --picture-description-concurrency needs a positive integer"
                        );
                        return ExitCode::from(2);
                    }
                }
            }
            "--picture-ocr-min-side" => match args.next().map(|v| v.trim().parse::<u32>()) {
                Some(Ok(n)) => opts.picture_ocr_min_side = Some(n),
                Some(_) => {
                    eprintln!("error: --picture-ocr-min-side needs a whole number of pixels");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --picture-ocr-min-side needs a value");
                    return ExitCode::from(2);
                }
            },
            "--no-picture-images" => opts.keep_picture_images = Some(false),
            // PII redaction (#621); the pattern flag repeats and joins on
            // newlines, the wire form of the option.
            "--redact-pii" => opts.redact_pii = Some(true),
            "--redact-mode" => match args.next() {
                Some(v) => opts.redact_mode = Some(v),
                None => {
                    eprintln!("error: --redact-mode needs label, pseudonym or fixed:TEXT");
                    return ExitCode::from(2);
                }
            },
            "--redact-kinds" => match args.next() {
                Some(v) => opts.redact_kinds = Some(v),
                None => {
                    eprintln!("error: --redact-kinds needs a comma-separated list of kinds");
                    return ExitCode::from(2);
                }
            },
            "--redact-pattern" => match args.next() {
                Some(v) => {
                    let joined = match opts.redact_pattern.take() {
                        Some(prev) => format!("{prev}\n{v}"),
                        None => v,
                    };
                    opts.redact_pattern = Some(joined);
                }
                None => {
                    eprintln!("error: --redact-pattern needs NAME=REGEX");
                    return ExitCode::from(2);
                }
            },
            "--redact-allow-terms" => opts.redact_allow_terms = args.next(),
            "--redact-deny-terms" => opts.redact_deny_terms = args.next(),
            "--redact-allow-pattern" => match args.next() {
                Some(v) => {
                    let joined = match opts.redact_allow_patterns.take() {
                        Some(prev) => format!("{prev}\n{v}"),
                        None => v,
                    };
                    opts.redact_allow_patterns = Some(joined);
                }
                None => {
                    eprintln!("error: --redact-allow-pattern needs a regular expression");
                    return ExitCode::from(2);
                }
            },
            "--redact-ner-min-score" => match args.next().map(|v| v.trim().parse::<f32>()) {
                Some(Ok(x)) => opts.redact_ner_min_score = Some(x),
                _ => {
                    eprintln!("error: --redact-ner-min-score needs a number in 0-1");
                    return ExitCode::from(2);
                }
            },
            "--redact-images" => match args.next() {
                Some(v) => opts.redact_images = Some(v),
                None => {
                    eprintln!("error: --redact-images needs drop, box_out or keep");
                    return ExitCode::from(2);
                }
            },
            "--abort-on-error" => abort_on_error = true,
            "--output-dirs" => match args.next().as_deref().map(OutputDirs::parse) {
                Some(Some(mode)) => output_dirs = mode,
                Some(None) => {
                    eprintln!("error: --output-dirs expects auto, flat or mirror");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --output-dirs needs a mode (auto, flat or mirror)");
                    return ExitCode::from(2);
                }
            },
            "--document-timeout" => match args.next().as_deref().map(parse_document_timeout) {
                Some(Ok(t)) => opts.document_timeout = Some(t),
                Some(Err(e)) => {
                    eprintln!("error: --document-timeout: {e}");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --document-timeout needs a number of seconds");
                    return ExitCode::from(2);
                }
            },
            "--input" => match args.next() {
                Some(v) => inputs.push(v),
                None => {
                    eprintln!("error: --input needs a glob pattern");
                    return ExitCode::from(2);
                }
            },
            "--output" => match args.next() {
                Some(v) => output = Some(v),
                None => {
                    eprintln!("error: --output needs a directory");
                    return ExitCode::from(2);
                }
            },
            "--output-file" => match args.next() {
                Some(v) if !v.is_empty() => output_file = Some(v),
                _ => {
                    eprintln!("error: --output-file needs a file path");
                    return ExitCode::from(2);
                }
            },
            "--jobs" => match args.next().and_then(|v| v.parse().ok()) {
                Some(n) if n >= 1 => jobs = n,
                _ => {
                    eprintln!("error: --jobs needs a positive integer");
                    return ExitCode::from(2);
                }
            },
            "--to" => match args.next() {
                Some(v) => to.extend(v.split(',').map(|f| f.trim().to_string())),
                None => {
                    eprintln!(
                        "error: --to needs a format ({})",
                        docling::OUTPUT_FORMATS.join(", ")
                    );
                    return ExitCode::from(2);
                }
            },
            // Named Whisper preset for audio inputs (English-only /
            // Distil-Whisper variants under .models/asr/<preset>/; fetch with
            // download_dependencies.sh --asr-model=<preset>).
            "--asr-model" => opts.asr_model = args.next(),
            // Transcription language (or "auto"); validated against the model's
            // vocabulary at conversion time.
            "--asr-lang" => opts.asr_lang = args.next(),
            // Character encoding of text inputs; validated when a text backend
            // first decodes the file.
            "--encoding" => match args.next() {
                Some(label) => opts.encoding = Some(label),
                None => {
                    eprintln!("error: --encoding needs an encoding label (e.g. shift_jis)");
                    std::process::exit(2);
                }
            },
            // Jupyter notebook outputs (#673); the spelling is validated with
            // the other options.
            "--ipynb-outputs" => match args.next() {
                Some(v) => opts.ipynb_outputs = Some(v),
                None => {
                    eprintln!("error: --ipynb-outputs needs none or text");
                    std::process::exit(2);
                }
            },
            "--metadata" => match args.next() {
                Some(v) => opts.metadata = Some(v),
                None => {
                    eprintln!("error: --metadata needs none, json or frontmatter");
                    std::process::exit(2);
                }
            },
            // Tracked changes (#851); the spelling is validated with the
            // other options.
            "--track-changes" => match args.next() {
                Some(v) => opts.track_changes = Some(v),
                None => {
                    eprintln!("error: --track-changes needs accept, reject or all");
                    std::process::exit(2);
                }
            },
            // Diagram extraction (#858); the spelling is validated with the
            // other options.
            // docling#4480's `use_outline_level_for_headings=False`.
            "--no-outline-level-headings" => opts.use_outline_level_for_headings = Some(false),
            "--extract-diagrams" => match args.next() {
                Some(v) => opts.extract_diagrams = Some(v),
                None => {
                    eprintln!("error: --extract-diagrams needs mermaid or none");
                    std::process::exit(2);
                }
            },
            // Input format hint (#668): an identifier from --list-input-formats
            // (`docx`, `pdf`, `html`, `md`…). It overrides whatever the name,
            // the media type or the content would have said.
            "--from" => match args.next() {
                Some(v) => match parse_input_format(&v) {
                    Some(f) => hints.format = Some(f),
                    None => {
                        eprintln!(
                            "error: --from: unknown input format '{v}' \
                             (see --list-input-formats)"
                        );
                        std::process::exit(2);
                    }
                },
                None => {
                    eprintln!("error: --from needs an input format (e.g. docx, pdf, html)");
                    std::process::exit(2);
                }
            },
            // Media-type hint (#668): decides the format of a source whose
            // name has no extension — stdin, a URL answered without a usable
            // Content-Type.
            "--mime-type" => match args.next() {
                Some(v) if InputFormat::from_mime(&v).is_some() => hints.mime = Some(v),
                Some(v) => {
                    eprintln!("error: --mime-type: '{v}' is not a media type docling converts");
                    std::process::exit(2);
                }
                None => {
                    eprintln!("error: --mime-type needs a media type (e.g. application/pdf)");
                    std::process::exit(2);
                }
            },
            // Refuse http(s) sources outright (#668) — for a wrapper that
            // passes user-supplied arguments through and must not fetch.
            "--no-url-fetch" => hints.no_url_fetch = true,
            // Max frames sampled from a video input (needs the ffmpeg binary;
            // 0 = transcript only). Default 8. A missing or non-numeric value
            // is a usage error like every other numeric flag — it used to be
            // swallowed silently and the default applied, so a typo
            // (`--video-frames 1O`) went unnoticed.
            "--video-frames" => match args.next().as_deref().map(str::trim) {
                Some(v) if v.eq_ignore_ascii_case("all") => {
                    opts.video_frames = Some(docling::ALL_VIDEO_FRAMES)
                }
                Some(v) if v.parse::<usize>().is_ok() => {
                    opts.video_frames = v.parse::<usize>().ok()
                }
                _ => {
                    eprintln!("error: --video-frames needs a non-negative integer or `all`");
                    return ExitCode::from(2);
                }
            },
            // The #647 sampling tunables; the range checks are
            // `ConvertOptions::validate`'s, like every other option.
            "--video-scene-threshold" => match args.next().map(|v| v.trim().parse::<f32>()) {
                Some(Ok(t)) => opts.video_scene_threshold = Some(t),
                _ => {
                    eprintln!("error: --video-scene-threshold needs a number in 0.0-1.0");
                    return ExitCode::from(2);
                }
            },
            "--video-frame-max-side" => match args.next().map(|v| v.trim().parse::<u32>()) {
                Some(Ok(px)) => opts.video_frame_max_side = Some(px),
                _ => {
                    eprintln!("error: --video-frame-max-side needs a whole number of pixels");
                    return ExitCode::from(2);
                }
            },
            "--video-frame-dedupe" => match args.next().map(|v| v.trim().parse::<u32>()) {
                Some(Ok(d)) => opts.video_frame_dedupe = Some(d),
                _ => {
                    eprintln!("error: --video-frame-dedupe needs a Hamming distance in 0-64");
                    return ExitCode::from(2);
                }
            },
            "--xbrl-taxonomy" => match args.next() {
                Some(dir) if !dir.trim().is_empty() => opts.xbrl_taxonomy = Some(dir),
                _ => {
                    eprintln!("error: --xbrl-taxonomy needs a directory");
                    return ExitCode::from(2);
                }
            },
            // `--image-export-mode` is docling's spelling (#611); the values
            // and their rules are the same.
            "--images" | "--image-export-mode" => images = Some(args.next().unwrap_or_default()),
            // #515: the Pandoc API the caller's `pandoc` reads. Only one is
            // written, so this is a check, not a choice: an unsupported
            // version fails here instead of feeding Pandoc a document it
            // would reject.
            "--pandoc-api-version" => {
                let v = args.next().unwrap_or_default();
                if let Err(e) = docling::pandoc::check_api_version(&v) {
                    eprintln!("error: --pandoc-api-version: {e}");
                    return ExitCode::from(2);
                }
            }
            // #848: the `Header` depth a section heading gets in the AST.
            "--pandoc-heading-depth" => match args.next().unwrap_or_default().parse() {
                Ok(d) => export.pandoc_heading_depth = d,
                Err(e) => {
                    eprintln!("error: --pandoc-heading-depth: {e}");
                    return ExitCode::from(2);
                }
            },
            // `--to images` render scale, pixels per PDF point (#243).
            "--scale" => match args.next().and_then(|v| v.parse::<f32>().ok()) {
                Some(v) if (0.1..=4.0).contains(&v) => scale = v,
                _ => {
                    eprintln!(
                        "error: --scale needs a number in 0.1-4.0 \
                         (pixels per PDF point; default 2.0 = 144 dpi)"
                    );
                    return ExitCode::from(2);
                }
            },
            // PDF page window, 1-based inclusive: `--pages 3-7` or `--pages 3`
            // (`--page-range` is docling's spelling, same syntax, #611).
            "--pages" | "--page-range" => match args.next() {
                // Checked by `validate()` below like every other option; a
                // missing value is a usage error of its own.
                Some(v) => opts.pages = Some(v),
                None => {
                    eprintln!("error: {arg} needs a range like 1-10 (or a single page)");
                    return ExitCode::from(2);
                }
            },
            // OCR recognition language for scanned PDF/image pages: en
            // (default; proper Latin word spacing) | ch (the multilingual
            // docling-conformance model).
            // Validated against the engine after the loop — `--ocr-engine`
            // may come later on the line, and `deu` is only a language to
            // Tesseract.
            "--ocr-lang" => match args.next() {
                Some(v) => opts.ocr_lang = Some(v),
                None => {
                    eprintln!(
                        "error: --ocr-lang needs a value (en | ch | a BCP-47 tag; tessdata \
                         stems such as deu+fra under --ocr-engine tesseract)"
                    );
                    return ExitCode::from(2);
                }
            },
            // Which OCR engine reads scanned pages (#460): the built-in
            // PP-OCRv3 recognizer (default) or the system tesseract binary.
            "--ocr-engine" => match args.next() {
                Some(v) => opts.ocr_engine = Some(v),
                None => {
                    eprintln!("error: --ocr-engine needs a value (ppocr | tesseract)");
                    return ExitCode::from(2);
                }
            },
            // Which regions feed the OCR (docling's OcrMode, #254):
            // full_page/layout_regions discard the text layer like
            // --force-full-page-ocr; pdf_aware_layout_regions (= default) is
            // the standard text-layer-aware behavior.
            // The PDF text layer's word-gap rule (#921), validated with
            // the option set: legacy | adaptive | a factor.
            "--pdf-word-gap" => match args.next() {
                Some(v) => opts.pdf_word_gap = Some(v),
                None => {
                    eprintln!("error: --pdf-word-gap needs a value");
                    return ExitCode::from(2);
                }
            },
            "--ocr-mode" => match args.next() {
                Some(v) => opts.ocr_mode = Some(v),
                None => {
                    eprintln!("error: --ocr-mode needs a value");
                    return ExitCode::from(2);
                }
            },
            // OCR render scale in px per PDF point (docling's
            // OcrOptions.scale, #254): unset reads the pipeline's own 2.0
            // px/pt render; docling's default is 3 (216 dpi).
            "--ocr-scale" => match args.next().map(|v| v.trim().parse::<f32>()) {
                Some(Ok(s)) => opts.ocr_scale = Some(s),
                Some(_) => {
                    eprintln!("error: --ocr-scale needs a positive number");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --ocr-scale needs a value");
                    return ExitCode::from(2);
                }
            },
            // Picture crops / page images in px per PDF point (docling's
            // images_scale, #520); the same 0.1-4.0 window as `--scale`.
            "--images-scale" => match args.next().and_then(|v| v.trim().parse::<f32>().ok()) {
                Some(v) => opts.images_scale = Some(v),
                _ => {
                    eprintln!(
                        "error: --images-scale needs a number in 0.1-4.0 \
                         (pixels per PDF point; unset = the 2.0 render)"
                    );
                    return ExitCode::from(2);
                }
            },
            "--page-images" => opts.page_images = Some(true),
            // Per-run `--to chunks` configuration (#256, mirrors the serve
            // fields / docling's service-datamodel `HybridChunkerOptions`);
            // the DOCLING_CHUNK_* env knobs stay the defaults.
            "--chunker" => match args.next().as_deref().map(ChunkerKind::parse) {
                Some(Ok(k)) => chunk_opts.chunker = Some(k),
                Some(Err(e)) => {
                    eprintln!("error: --chunker: {e}");
                    return ExitCode::from(2);
                }
                None => {
                    eprintln!("error: --chunker needs a value (hierarchical|hybrid)");
                    return ExitCode::from(2);
                }
            },
            "--chunk-tokenizer" => match args.next() {
                Some(v) => chunk_opts.tokenizer = Some(v),
                None => {
                    eprintln!("error: --chunk-tokenizer needs a tokenizer.json path");
                    return ExitCode::from(2);
                }
            },
            "--chunk-max-tokens" => match args.next().map(|v| v.trim().parse::<usize>()) {
                Some(Ok(n)) if n > 0 => chunk_opts.max_tokens = Some(n),
                _ => {
                    eprintln!("error: --chunk-max-tokens needs a positive integer");
                    return ExitCode::from(2);
                }
            },
            "--no-chunk-merge-peers" => chunk_opts.merge_peers = Some(false),
            // Pipeline selection (#77): `standard` (default, the ML stack) or
            // `vlm` — render pages and convert them through a remote
            // OpenAI-compatible vision endpoint returning DocLang.
            "--pipeline" => match args.next() {
                Some(v) => opts.pipeline = Some(v),
                None => {
                    eprintln!("error: --pipeline needs a value (standard|vlm)");
                    return ExitCode::from(2);
                }
            },
            "--vlm-endpoint" => opts.vlm_endpoint = args.next(),
            "--vlm-model" => opts.vlm_model = args.next(),
            // #312: the remaining VlmOptions knobs, for parity with the Node/
            // Python bindings and serve (previously env-only, or — for
            // max_tokens — not settable at all). Inert without --pipeline vlm,
            // like every other --vlm-* flag.
            "--vlm-api-key" => opts.vlm_api_key = args.next(),
            "--vlm-prompt" => opts.vlm_prompt = args.next(),
            "--vlm-max-tokens" => match args.next().map(|v| v.trim().parse::<usize>()) {
                // 0 is rejected by `validate()` below, like the other surfaces do.
                Some(Ok(n)) => opts.vlm_max_tokens = Some(n),
                _ => {
                    eprintln!("error: --vlm-max-tokens needs a positive integer");
                    return ExitCode::from(2);
                }
            },
            // Hidden benchmarking aid: load the PDF/image pipeline once, then time
            // N warm conversions (models already loaded), printing the avg seconds
            // per conversion to stdout. This is the startup-excluded counterpart to
            // Python docling's in-process "warm" measurement, for a fair head-to-head.
            "--bench-warm" => {
                bench_warm = args.next().and_then(|n| n.parse::<usize>().ok());
                if bench_warm.is_none() {
                    eprintln!("error: --bench-warm needs a positive run count");
                    return ExitCode::from(2);
                }
            }
            _ if arg.starts_with("--") => {
                eprintln!("error: unknown flag '{arg}'");
                eprintln!("run `docling-rs --help` for the full flag list");
                return ExitCode::from(2);
            }
            _ => paths.push(arg),
        }
    }

    if to.is_empty() {
        to.push("md".to_string());
    }
    // `markdown` is `md`; a format named twice is written once (upstream's
    // typer list would write it twice, which is never what was meant).
    let mut formats: Vec<String> = Vec::new();
    for f in &to {
        let f = if f == "markdown" { "md" } else { f.as_str() };
        if !docling::OUTPUT_FORMATS.contains(&f) {
            eprintln!(
                "error: unknown --to '{f}' (expected: {})",
                docling::OUTPUT_FORMATS.join(", ")
            );
            return ExitCode::from(2);
        }
        if !formats.iter().any(|known| known == f) {
            formats.push(f.to_string());
        }
    }
    let to = formats;
    // The one validation pass every surface runs (#577): `--pages`,
    // `--document-timeout`, the OCR engine/mode/scale, `--ocr-lang` against
    // the engine it will drive (whichever order the two flags came in),
    // `--pipeline`, `--vlm-max-tokens` — reported with the CLI's flag
    // spelling, before any file is touched.
    // The Pandoc AST's comment handling follows the policy (#851).
    export.track_changes = opts.track_changes().ok().flatten();
    if let Err(e) = opts.validate() {
        eprintln!("error: {}", e.cli_message());
        return ExitCode::from(2);
    }
    // `--pipeline vlm` resolves its endpoint/model now (flags, else the
    // `DOCLING_RS_VLM_*` environment) so a missing one is a usage error, not
    // a failure after the file was read.
    let vlm = match opts.vlm_options() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}", e.cli_message());
            return ExitCode::from(2);
        }
    };
    // Validated above; read back as typed values where the paths below
    // need them.
    let pages = opts.page_range().ok().flatten();
    let text_opts = docling::PdfTextOptions {
        word_gap: opts.pdf_word_gap().ok().flatten(),
        keep_typography: opts
            .pdf_keep_typography
            .unwrap_or(docling::PdfTextOptions::from_env().keep_typography),
    };
    let strict = opts.strict.unwrap_or(false);
    let text_layer_mode = opts.text_layer_only.unwrap_or_default();
    // The missing-models fallback below is `true`'s alone: under `auto` a
    // digital PDF never asks for a model, and a scanned one needs them.
    let text_layer_only = text_layer_mode == docling::TextLayerOnly::On;
    let page_break_placeholder = opts.page_break_placeholder.clone();
    let image_mode = match images.as_deref().unwrap_or("placeholder") {
        "placeholder" => ImageMode::Placeholder,
        "embedded" => ImageMode::Embedded,
        "referenced" => ImageMode::Referenced,
        other => {
            eprintln!(
                "error: unknown --images '{other}' (expected: placeholder, embedded, referenced)"
            );
            return ExitCode::from(2);
        }
    };
    // #537: the Pandoc AST feeds `pandoc -f json -t docx`, whose writers drop
    // a picture without a target — so unless `--images` says otherwise its
    // pictures are embedded `data:` URIs (the AST alone rebuilds them).
    let pandoc_image_mode = if images.is_some() {
        image_mode
    } else {
        ImageMode::Embedded
    };

    // `--output-file PATH` (#611, docling's flag): exactly one input
    // document in exactly one format, written to that path; `--output` is
    // ignored, as in docling.
    if let Some(target) = output_file {
        if bench_warm.is_some() {
            eprintln!("error: --bench-warm prints timings, not a document; drop --output-file");
            return ExitCode::from(2);
        }
        let sources: Vec<&String> = inputs.iter().chain(&paths).collect();
        let cfg = BatchCfg {
            to,
            image_mode,
            export,
            pandoc_image_mode,
            opts,
            pages,
            progress: progress.unwrap_or_else(docling::ProgressPrinter::is_terminal),
            scale,
            chunk: chunk_opts.clone(),
            vlm,
        };
        // A `.zip` is one document here unless `--no-archive-merge` (#677).
        return write_output_file(
            Path::new(&target),
            &sources,
            &hints,
            &cfg,
            archive_merge != Some(false),
        );
    }

    // Batch mode (#205, #489): `--input <glob>` and/or several positional
    // sources fan one warm process over many files, writing results under
    // `--output`. Each source keeps its own base — a directory or a glob's
    // static prefix mirrors its tree, a plain file lands by stem, like
    // Python's `docling convert a.docx sub/b.docx --output out/`. A single
    // positional file with `--output` routes through the same writer (a
    // batch of one).
    // Several `--to` formats go the same way (#491): stdout carries one
    // document, so every format is a file under `--output`.
    if !inputs.is_empty() || output.is_some() || paths.len() > 1 || to.len() > 1 {
        if bench_warm.is_some() {
            eprintln!("error: --bench-warm is a single-file mode; drop --input/--output");
            return ExitCode::from(2);
        }
        let Some(outdir) = output else {
            if to.len() > 1 {
                eprintln!(
                    "error: several --to formats need --output DIR (stdout carries one document)"
                );
            } else if paths.len() > 1 {
                eprintln!("error: converting several sources needs --output DIR");
            } else {
                eprintln!("error: --input needs --output DIR for the converted files");
            }
            return ExitCode::from(2);
        };
        if inputs.is_empty() && paths.is_empty() {
            eprintln!("error: --output needs --input GLOB or at least one input file");
            return ExitCode::from(2);
        }
        // stdin / URL / `data:` sources (#668) are loaded now — they have no
        // path to expand — and join the batch as items of their own.
        let (files, preloaded) = match expand_sources(inputs.iter().chain(&paths), &hints) {
            Ok(split) => split,
            Err((code, msg)) => {
                eprintln!("error: {msg}");
                return ExitCode::from(code);
            }
        };
        // `--output-dirs` (#496) rewrites every source's base at once: `flat`
        // drops the trees, `mirror` roots them all at the current directory.
        let files = match apply_output_dirs(output_dirs, files) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        };
        // A `.zip` source expands into its documents (#557), each named
        // `<archive minus .zip>/<entry path>` for its outputs; what it holds
        // that does not convert is reported now, before any work starts.
        // Under `--archive-merge` (#677) it is one item, `<archive>.<ext>`.
        let (mut items, archive_report) = expand_archives(files, archive_merge == Some(true));
        items.extend(preloaded);
        let pairs: Vec<(std::path::PathBuf, std::path::PathBuf)> = items
            .iter()
            .map(|i| (i.file.clone(), i.base.clone()))
            .collect();
        if let Err(e) = check_output_collisions(&pairs, Path::new(&outdir), &to) {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
        let cfg = BatchCfg {
            to,
            image_mode,
            export,
            pandoc_image_mode,
            opts,
            pages,
            progress: progress.unwrap_or_else(docling::ProgressPrinter::is_terminal),
            scale,
            chunk: chunk_opts.clone(),
            vlm,
        };
        return run_batch(
            items,
            archive_report,
            Path::new(&outdir),
            jobs,
            abort_on_error,
            &cfg,
        );
    }

    // Past the batch branch exactly one format remains (several returned
    // above): the single-document stdout mode below reads it as before.
    let to = to.into_iter().next().unwrap_or_else(|| "md".to_string());
    let image_mode = if to == "pandoc" {
        pandoc_image_mode
    } else {
        image_mode
    };
    let Some(path) = paths.into_iter().next() else {
        eprintln!("error: no input file");
        eprintln!("{USAGE}");
        eprintln!("run `docling-rs --help` for the full flag list");
        return ExitCode::from(2);
    };

    // A lone `.zip` on stdout is one document (#677): `## <entry path>` and
    // the entry's content, per convertible entry — unless `--no-archive-merge`
    // asks for the one-output-each contract, which needs `--output DIR`.
    if !is_special_source(&path) && is_zip(Path::new(&path)) {
        if archive_merge == Some(false) {
            eprintln!(
                "error: '{path}' is a ZIP archive: its documents convert one output each, \
                 so it needs --output DIR"
            );
            return ExitCode::from(2);
        }
        if bench_warm.is_some() || vlm.is_some() || to == "images" {
            eprintln!(
                "error: '{path}' is a ZIP archive: --bench-warm, --pipeline vlm and --to images \
                 take one document; convert its entries one by one with --output DIR"
            );
            return ExitCode::from(2);
        }
        // The same `DOCLING_RS_ZIP_MAX_*` limits the batch applies.
        let converter = match DocumentConverter::from_options(&opts) {
            Ok(c) => c
                .archive_limits(docling::ArchiveLimits::from_env())
                .show_progress(progress.unwrap_or_else(docling::ProgressPrinter::is_terminal)),
            Err(e) => {
                eprintln!("error: {}", e.cli_message());
                return ExitCode::from(2);
            }
        };
        let document = match merge_archive(Path::new(&path), &converter) {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        return output_document(document, &to, image_mode, &path, &chunk_opts, export);
    }
    let source = match load_source(&path, &hints) {
        Ok(src) => src,
        Err((code, msg)) => {
            eprintln!("error: {msg}");
            return ExitCode::from(code);
        }
    };
    let is_pdf = source.format == InputFormat::Pdf;
    // Outputs beside stdout — `--to dclx`'s archive, `--images referenced`'s
    // artifacts, `--to images`' pages — are named after the file; a source
    // with no file (stdin, a URL) is named after the document.
    let path = if is_special_source(&path) {
        source.name.clone()
    } else {
        path
    };
    // The `--text-layer-only` fallback converts the same bytes again after
    // the pipeline refused them for missing models; keep them (they are
    // moved into the conversion) only when it can apply.
    let text_layer_fallback = (is_pdf && text_layer_only).then(|| {
        let name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| source.name.clone());
        (source.bytes.clone(), name)
    });

    if let Some(runs) = bench_warm {
        return match bench_warm_conversion(
            &source,
            runs,
            opts.no_table_former.unwrap_or(false),
            text_layer_mode,
            opts.ocr_disabled(),
            opts.password.as_deref(),
        ) {
            Ok(avg) => {
                // Bare seconds on stdout for the benchmark harness; a human line on stderr.
                println!("{avg:.6}");
                eprintln!(
                    "warm conversion: {:.4}s/doc over {runs} runs (startup excluded)",
                    avg
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // `--to images` (#243): rasterization only — no conversion, no
    // models, no pipeline (a `--pipeline vlm` selection has nothing to do and
    // is ignored). Files land in the CWD like `--to dclx`'s archive.
    if to == "images" {
        if !is_pdf {
            eprintln!("error: --to images rasterizes PDF inputs only ('{path}' is not a PDF)");
            return ExitCode::from(2);
        }
        let stem = Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let password = opts.password.as_deref();
        return match write_page_images(&source.bytes, password, pages, scale, Path::new(""), &stem)
        {
            Ok(written) => {
                // Humans read stderr; stdout stays the bare paths for scripts
                // (the dclx convention).
                eprintln!("images: {} page(s) written", written.len());
                for p in &written {
                    println!("{}", p.display());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // #77: the remote-VLM pipeline replaces the whole ML stack — convert,
    // then fall through to the regular output selection (md/json/dclx/chunks
    // all work; there is no page-streaming, the endpoint is the bottleneck).
    if let Some(vlm) = &vlm {
        let mut document = match docling::vlm::convert_vlm_result(&source, vlm) {
            // A page that failed or came back cut off (#762, #763): the
            // document is written, the pages go to stderr, exit 0 — like the
            // standard pipeline's partial results below.
            Ok(result) => {
                for problem in &result.errors {
                    eprintln!("warning: partial document: {}", problem.describe());
                }
                result.document
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        document.strict_markdown = strict;
        document.page_break_placeholder = page_break_placeholder.clone();
        return output_document(document, &to, image_mode, &path, &chunk_opts, export);
    }

    // Validated above, so this cannot fail; the mapping onto the builder is
    // the library's, shared with every other surface (#577).
    let converter = match DocumentConverter::from_options(&opts) {
        // `--progress` (#783): the built-in stderr printer, on by default
        // on a terminal.
        Ok(c) => c.show_progress(progress.unwrap_or_else(docling::ProgressPrinter::is_terminal)),
        Err(e) => {
            eprintln!("error: {}", e.cli_message());
            return ExitCode::from(2);
        }
    };
    // `--pdf-report-repairs` (#934): read off the input bytes before the
    // conversion, so the streamed Markdown path reports them too.
    if is_pdf && converter.reports_pdf_repairs() {
        for repair in docling::pdf_repairs(&source.bytes) {
            eprintln!("repaired: {repair}");
        }
    }

    // Stream Markdown by default: print each chunk as the converter produces it
    // (page by page for PDF). Referenced images stream too (#80): each page's
    // files land under ./artifacts/ as that page is printed, so image bytes
    // never accumulate. JSON needs the whole tree, so it keeps the buffered
    // path. `--no-stream` opts back into buffering.
    let is_markdown = matches!(to.as_str(), "md" | "markdown");
    if is_markdown && !no_stream {
        let stream = match converter
            .convert_streaming_with_options(source, &export.md_images(image_mode, "artifacts"))
        {
            Ok(s) => s,
            Err(e) => {
                if let Some(mut doc) = pdf_text_layer_fallback(
                    &e.to_string(),
                    is_pdf,
                    text_layer_only,
                    strict,
                    text_layer_fallback.as_ref(),
                    pages,
                    text_opts,
                ) {
                    doc.page_break_placeholder = page_break_placeholder.clone();
                    return output_document(doc, &to, image_mode, &path, &chunk_opts, export);
                }
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        };
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        let mut wrote_any = false;
        for chunk in stream {
            match chunk {
                Ok(s) => {
                    if let Err(e) = out.write_all(s.as_bytes()) {
                        eprintln!("error: writing output: {e}");
                        return ExitCode::FAILURE;
                    }
                    wrote_any = wrote_any || !s.is_empty();
                }
                // A spent `--document-timeout` (#497) is the stream's last item:
                // the Markdown printed so far is the partial document (docling's
                // PARTIAL_SUCCESS), reported on stderr, exit code 0.
                Err(docling::ConversionError::Timeout(msg)) => {
                    if let Err(e) = out.flush() {
                        eprintln!("error: writing output: {e}");
                        return ExitCode::FAILURE;
                    }
                    eprintln!("warning: partial document: {msg}");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    let _ = out.flush();
                    // The ML pipeline loads its models lazily, so the missing-assets
                    // error can surface here — but only fall back while nothing
                    // has been printed, to never emit a document twice.
                    if !wrote_any {
                        if let Some(mut doc) = pdf_text_layer_fallback(
                            &e.to_string(),
                            is_pdf,
                            text_layer_only,
                            strict,
                            text_layer_fallback.as_ref(),
                            pages,
                            text_opts,
                        ) {
                            doc.page_break_placeholder = page_break_placeholder.clone();
                            return output_document(
                                doc,
                                &to,
                                image_mode,
                                &path,
                                &chunk_opts,
                                export,
                            );
                        }
                    }
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if let Err(e) = out.flush() {
            eprintln!("error: writing output: {e}");
            return ExitCode::FAILURE;
        }
        if image_mode == ImageMode::Referenced {
            eprintln!("referenced images (if any) written to ./artifacts/ as pages completed");
        }
        return ExitCode::SUCCESS;
    }

    let document = match converter.convert(source) {
        Ok(result) => {
            // docling's PARTIAL_SUCCESS (#497): the document is written, the
            // reason goes to stderr, the exit code stays 0.
            for problem in &result.errors {
                eprintln!("warning: partial document: {}", problem.describe());
            }
            if let Some(report) = &result.redaction {
                eprintln!("redacted: {}", redaction_summary(report));
            }
            result.document
        }
        Err(e) => {
            if let Some(mut doc) = pdf_text_layer_fallback(
                &e.to_string(),
                is_pdf,
                text_layer_only,
                strict,
                text_layer_fallback.as_ref(),
                pages,
                text_opts,
            ) {
                doc.page_break_placeholder = page_break_placeholder.clone();
                return output_document(doc, &to, image_mode, &path, &chunk_opts, export);
            }
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    output_document(document, &to, image_mode, &path, &chunk_opts, export)
}

/// Launch-blocker fallback: a bare `cargo install docling-cli` ships no ONNX
/// models, so the first PDF a new user tries dies at pipeline startup. Under
/// `--text-layer-only` (`--no-ocr` before 2.0, #611) the pure-Rust text-layer
/// path needs no runtime assets at all — when the failure is exactly "assets
/// missing" (matched on the markers docling-pdf's enriched errors carry),
/// convert the embedded text layer instead of failing. Any other error, or a
/// run without `--text-layer-only`, returns `None` and the (actionable) error
/// prints as usual.
fn pdf_text_layer_fallback(
    err: &str,
    is_pdf: bool,
    text_layer_only: bool,
    strict: bool,
    source: Option<&(Vec<u8>, String)>,
    pages: Option<(usize, usize)>,
    text: docling::PdfTextOptions,
) -> Option<docling::DoclingDocument> {
    let assets_missing =
        err.contains("pdfium library is not installed") || err.contains("model not found at");
    if !is_pdf || !text_layer_only || !assets_missing {
        return None;
    }
    let (bytes, name) = source?;
    match docling::pdf_text_layer_pages_opts(bytes, name, pages, text) {
        Ok(mut doc) if !doc.nodes.is_empty() => {
            eprintln!(
                "warning: models unavailable — --text-layer-only extracted the embedded text \
                 layer only (run scripts/install/download_dependencies.sh for the full pipeline)"
            );
            doc.strict_markdown = strict;
            Some(doc)
        }
        // A scanned PDF has no text layer; the original error explains the
        // missing assets better than an empty document would.
        _ => None,
    }
}

/// `--layers` / `--traverse-pictures`: docling-core's
/// `export_to_markdown(included_content_layers=…, traverse_pictures=…)` for
/// md/text/html. The default (body, no traversal) is upstream's default
/// export byte for byte. HTML takes the layers only.
#[derive(Clone, Copy, Default)]
struct ExportLayers {
    layers: ContentLayers,
    traverse_pictures: bool,
    /// `--pandoc-heading-depth` (#848): the `Header` depth a section
    /// heading gets in the Pandoc AST.
    pandoc_heading_depth: docling::pandoc::HeadingDepth,
    /// `--track-changes` (#851): an explicit accept / reject view leaves
    /// the reviewer comments out of the Pandoc AST.
    track_changes: Option<docling::TrackChanges>,
}

type Rendered = (String, Vec<(String, Vec<u8>)>);

impl ExportLayers {
    fn markdown(self, doc: &DoclingDocument, mode: ImageMode, dir: &str) -> Rendered {
        doc.export_to_markdown_with_options(&self.md_images(mode, dir))
    }

    /// The Markdown export's options, buffered or streamed.
    fn md_images(self, image_mode: ImageMode, dir: &str) -> MarkdownExportOptions {
        self.md(MarkdownExportOptions {
            image_mode,
            artifacts_dir: dir.into(),
            ..Default::default()
        })
    }

    /// `export_to_text`, layers applied: its options, its trimmed newline.
    fn text(self, doc: &DoclingDocument) -> String {
        let (text, _) =
            doc.export_to_markdown_with_options(&self.md(MarkdownExportOptions::plain_text()));
        text.strip_suffix('\n').unwrap_or(&text).to_string()
    }

    fn html(self, doc: &DoclingDocument, mode: ImageMode, dir: &str) -> Rendered {
        let (image_mode, artifacts_dir, layers) = (mode, dir.into(), self.layers);
        doc.export_to_html_with(&HtmlExportOptions {
            image_mode,
            artifacts_dir,
            layers,
        })
    }

    fn md(self, base: MarkdownExportOptions) -> MarkdownExportOptions {
        let (layers, traverse_pictures) = (self.layers, self.traverse_pictures);
        MarkdownExportOptions {
            layers,
            traverse_pictures,
            ..base
        }
    }
}

/// The buffered output tail shared by the standard (non-streaming) and VLM
/// paths: `--to` selection, image sidecars, exit code.
/// The CLI flags a batch run freezes for every file (#205).
struct BatchCfg {
    /// The `--to` formats, de-duplicated, in the order given (#491): each
    /// document converts once and is written in every one of them.
    to: Vec<String>,
    image_mode: ImageMode,
    export: ExportLayers,
    /// `image_mode` for `--to pandoc`: embedded unless `--images` was given
    /// (#537).
    pandoc_image_mode: ImageMode,
    /// Every conversion option (#577), validated before the batch started.
    opts: docling::ConvertOptions,
    /// `opts.pages` parsed — the PDF page window.
    pages: Option<(usize, usize)>,
    /// `--progress` (#783): the stderr bars for the pages and enrichment
    /// passes of every document.
    progress: bool,
    /// `--to images` render scale (pixels per PDF point, #243).
    scale: f32,
    /// Per-run `--to chunks` configuration (#256).
    chunk: ChunkOptions,
    /// `--pipeline vlm` resolved (#77); `None` = the standard pipeline.
    vlm: Option<docling::vlm::VlmOptions>,
}

/// `--output-dirs` (#496): how several inputs lay out under `--output`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputDirs {
    /// Today's rule: a directory or glob mirrors its tree, a plain file lands
    /// by stem (Python docling's `convert a.docx sub/b.docx --output out/`).
    Auto,
    /// Every output `<stem>.<ext>` directly in `--output`; two inputs with
    /// one stem are rejected before anything is written.
    Flat,
    /// Every input's path *relative to the current directory* under
    /// `--output` — the explicit-file list keeps its folders too, which is
    /// what a RAG corpus full of `README.md`s needs. An input outside the
    /// current directory has no such path and is an error.
    Mirror,
}

impl OutputDirs {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "flat" => Some(Self::Flat),
            "mirror" => Some(Self::Mirror),
            _ => None,
        }
    }
}

/// Rewrite the (file, base) pairs the sources expanded to for the chosen
/// `--output-dirs` mode. `auto` keeps each source's own base; `flat` makes
/// every file's parent its base (so only the stem survives); `mirror` roots
/// every file at the current directory — a relative path keeps its folders
/// verbatim (`a/doc.docx` → `out/a/doc.md`), an absolute one is made
/// relative to the current directory, and a path that escapes it (`../x.pdf`,
/// `/elsewhere/x.pdf`) is refused rather than guessed at.
fn apply_output_dirs(
    mode: OutputDirs,
    files: Vec<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<Vec<(std::path::PathBuf, std::path::PathBuf)>, String> {
    match mode {
        OutputDirs::Auto => Ok(files),
        OutputDirs::Flat => Ok(files
            .into_iter()
            .map(|(f, _)| {
                let base = f.parent().map(Path::to_path_buf).unwrap_or_default();
                (f, base)
            })
            .collect()),
        OutputDirs::Mirror => {
            let cwd = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;
            files
                .into_iter()
                .map(|(f, _)| mirror_pair(&f, &cwd))
                .collect()
        }
    }
}

/// One `mirror` pair: the file as a path relative to `cwd` (lexically
/// normalized — `./`, `a/../b`) with an empty base, so `batch_out_path` keeps
/// the whole relative path.
fn mirror_pair(
    file: &Path,
    cwd: &Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    use std::path::Component;
    let abs = if file.is_absolute() {
        file.to_path_buf()
    } else {
        cwd.join(file)
    };
    // Normalize `.` and `..` lexically (no symlink resolution: the user named
    // the path, and the output should mirror what they named).
    let mut norm = std::path::PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !norm.pop() {
                    return Err(format!(
                        "'{}' climbs above the filesystem root",
                        file.display()
                    ));
                }
            }
            c => norm.push(c),
        }
    }
    let rel = norm.strip_prefix(cwd).map_err(|_| {
        format!(
            "'{}' is outside the current directory ({}); --output-dirs mirror lays inputs \
             out by their path relative to it — run from a common parent, or use \
             --output-dirs flat",
            file.display(),
            cwd.display()
        )
    })?;
    if rel.as_os_str().is_empty() {
        return Err(format!(
            "'{}' is the current directory itself",
            file.display()
        ));
    }
    Ok((rel.to_path_buf(), std::path::PathBuf::new()))
}

/// `--document-timeout SECONDS`: a positive number of seconds (fractions
/// allowed, like Python docling's float).
fn parse_document_timeout(s: &str) -> Result<f64, String> {
    let secs: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("expected a number of seconds, got {s:?}"))?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!("expected a positive number of seconds, got {s:?}"));
    }
    Ok(secs)
}

/// Expand one source argument (#489) into (files, base): an existing file is
/// itself with its parent as base (so it lands in `--output` by stem, like
/// Python's `docling convert a.docx sub/b.docx`), a directory or a glob goes
/// through [`expand_glob`] and keeps its tree, and anything else is an error
/// naming the argument — a typo must not silently convert nothing.
fn expand_source(arg: &str) -> Result<(Vec<std::path::PathBuf>, std::path::PathBuf), String> {
    let p = Path::new(arg);
    if p.is_file() {
        let base = p.parent().map(Path::to_path_buf).unwrap_or_default();
        return Ok((vec![p.to_path_buf()], base));
    }
    if p.is_dir() || arg.contains(['*', '?', '[']) {
        return expand_glob(arg);
    }
    Err(format!("'{arg}': no such file or directory"))
}

/// How a source whose name decides nothing is typed (#668): `--from`
/// names the format outright, `--mime-type` stands in for a missing
/// `Content-Type`, `--no-url-fetch` refuses http(s) sources.
#[derive(Clone, Default)]
struct SourceHints {
    format: Option<InputFormat>,
    mime: Option<String>,
    no_url_fetch: bool,
}

/// `--from`'s value: an input-format identifier as `--list-input-formats`
/// prints them (an extension: `docx`, `pdf`, `md`, `html`, `txt`…) or the
/// format's own name (`markdown`, `image`, `json_docling`).
fn parse_input_format(value: &str) -> Option<InputFormat> {
    let v = value.trim().trim_start_matches('.');
    InputFormat::from_extension(v).or_else(|| {
        InputFormat::supported_extensions()
            .iter()
            .filter_map(|ext| InputFormat::from_extension(ext))
            .find(|f| f.as_str().eq_ignore_ascii_case(v))
    })
}

/// A positional source that is not a path (#668): `-` (stdin), an http(s)
/// URL, a `data:` URL, or a `file:` URL (which names a path, but through
/// the URL grammar).
fn is_special_source(arg: &str) -> bool {
    arg == "-"
        || arg.starts_with("http://")
        || arg.starts_with("https://")
        || arg.starts_with("data:")
        || arg.starts_with("file:")
}

/// Load one source — a path, `-`, a URL — under `hints`. `--from` wins over
/// everything; otherwise the name's extension, then the media type
/// (`--mime-type`, else the response's `Content-Type`), then the bytes
/// decide, as [`SourceDocument::from_named_bytes`] does. The error carries
/// the exit code: 2 when the source was named or typed wrong (a usage
/// error — no such file, no way to tell the format, URL fetch refused), 1
/// when reading or fetching it failed.
fn load_source(arg: &str, hints: &SourceHints) -> Result<SourceDocument, (u8, String)> {
    // `--from`: build straight from the bytes; nothing else is consulted.
    let typed = |name: &str, bytes: Vec<u8>, content_type: Option<&str>| {
        if let Some(format) = hints.format {
            let stem = Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("document");
            return Ok(SourceDocument::from_bytes(stem, format, bytes));
        }
        SourceDocument::from_named_bytes(name, bytes, hints.mime.as_deref().or(content_type))
            .map_err(|e| {
                let what = match e {
                    docling::ConversionError::UnknownFormat { hint } => hint,
                    other => other.to_string(),
                };
                // stdin has no name to speak of, and a data: URL's payload
                // is not worth repeating.
                let (label, what) = if arg == "-" {
                    (
                        "stdin".to_string(),
                        what.replace("of 'document': no file extension, ", "of stdin: "),
                    )
                } else if arg.starts_with("data:") {
                    ("data: URL".to_string(), what)
                } else {
                    (arg.to_string(), what)
                };
                (
                    2,
                    format!("{label}: {what} (pass --from FORMAT or --mime-type TYPE)"),
                )
            })
    };
    if arg == "-" {
        let mut bytes = Vec::new();
        io::stdin()
            .lock()
            .read_to_end(&mut bytes)
            .map_err(|e| (1, format!("reading stdin: {e}")))?;
        return typed("document", bytes, None);
    }
    if arg.starts_with("data:") {
        let data = docling::fetch::parse_data_url(arg).map_err(|e| (2, e.to_string()))?;
        let name = data.name.as_deref().unwrap_or("document").to_string();
        return typed(&name, data.bytes, data.media_type.as_deref());
    }
    if arg.starts_with("http://") || arg.starts_with("https://") {
        if hints.no_url_fetch {
            return Err((
                2,
                format!("'{arg}': URL sources are disabled (--no-url-fetch)"),
            ));
        }
        // A CLI follows the redirects a browser would; the SSRF guard sees
        // every hop (lift it with DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1 for a
        // local server). Bounds are the library's defaults.
        let policy = docling::FetchPolicy {
            max_redirects: 5,
            ..docling::FetchPolicy::default()
        };
        let fetched = docling::fetch::fetch(arg, &policy).map_err(|e| {
            let code = if e.to_string().starts_with("refusing to fetch") {
                2
            } else {
                1
            };
            (code, e.to_string())
        })?;
        let name =
            docling::fetch::fetched_name(&fetched.url, fetched.content_disposition.as_deref());
        return typed(&name, fetched.bytes, fetched.content_type.as_deref())
            .map(|s| s.with_base_url(fetched.url));
    }
    let path = if arg.starts_with("file:") {
        docling::fetch::file_url_path(arg).map_err(|e| (2, e))?
    } else {
        PathBuf::from(arg)
    };
    // A plain file: `from_file` unless a hint has to decide its format (no
    // extension, or `--from`).
    let has_ext = path.extension().is_some();
    if hints.format.is_none() && (has_ext || hints.mime.is_none()) {
        return SourceDocument::from_file(&path).map_err(|e| match e {
            docling::ConversionError::UnknownFormat { hint } => (
                2,
                format!(
                    "{}: {hint} (pass --from FORMAT or --mime-type TYPE)",
                    path.display()
                ),
            ),
            docling::ConversionError::Io(io) if io.kind() == io::ErrorKind::NotFound => (
                2,
                format!("'{}': no such file or directory", path.display()),
            ),
            other => (1, format!("{}: {other}", path.display())),
        });
    }
    let bytes = std::fs::read(&path).map_err(|e| (1, format!("{}: {e}", path.display())))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_string());
    let mut source = typed(&name, bytes, None)?;
    source.path = Some(path);
    Ok(source)
}

/// [`expand_sources`]' result: the `(file, base)` pairs of the path
/// sources, and the items of the loaded ones.
type SplitSources = (Vec<(PathBuf, PathBuf)>, Vec<BatchItem>);

/// Split a batch's sources (#668): paths, directories and globs expand to
/// files as before; stdin, URLs and `data:` URLs are loaded here and come
/// back as ready items named `<name>.<ext>` (so they land under `--output`
/// by stem like a plain file would).
fn expand_sources<'a>(
    patterns: impl Iterator<Item = &'a String>,
    hints: &SourceHints,
) -> Result<SplitSources, (u8, String)> {
    let mut files = Vec::new();
    let mut preloaded = Vec::new();
    for pattern in patterns {
        if is_special_source(pattern) && !pattern.starts_with("file:") {
            let source = load_source(pattern, hints)?;
            let ext = source.format.extensions().next().unwrap_or("bin");
            preloaded.push(BatchItem {
                file: PathBuf::from(format!("{}.{ext}", source.name)),
                base: PathBuf::new(),
                entry: None,
                preloaded: Some((pattern.clone(), source)),
                merged: false,
            });
            continue;
        }
        let pattern = if pattern.starts_with("file:") {
            docling::fetch::file_url_path(pattern)
                .map_err(|e| (2, e))?
                .to_string_lossy()
                .into_owned()
        } else {
            pattern.clone()
        };
        match expand_source(&pattern) {
            Ok((matched, base)) => files.extend(matched.into_iter().map(|f| (f, base.clone()))),
            Err(e) => return Err((2, e)),
        }
    }
    Ok((files, preloaded))
}

/// Two sources that would write the same output file (#489): `sub/b.docx`
/// and `other/b.docx` both land as `out/b.md` when passed as files. Python
/// docling lets the later one overwrite the earlier silently; refuse instead —
/// a converted document vanishing is worse than a usage error, and the fix
/// (pass a common parent directory, whose tree is mirrored, or separate
/// `--output` dirs) is one argument away.
fn check_output_collisions(
    files: &[(std::path::PathBuf, std::path::PathBuf)],
    output: &Path,
    formats: &[String],
) -> Result<(), String> {
    let mut seen: std::collections::HashMap<std::path::PathBuf, &Path> =
        std::collections::HashMap::new();
    for ((file, base), to) in files
        .iter()
        .flat_map(|fb| formats.iter().map(move |to| (fb, to)))
    {
        let out = batch_out_path(file, base, output, to);
        if let Some(first) = seen.insert(out.clone(), file) {
            if first != file {
                return Err(format!(
                    "'{}' and '{}' would both be written to '{}'; pass a common parent \
                     directory (its structure is kept), use --output-dirs mirror, or use \
                     separate --output directories",
                    first.display(),
                    file.display(),
                    out.display()
                ));
            }
        }
    }
    Ok(())
}

/// Expand an `--input` glob into (matched files, static base directory). The
/// base — every path component before the first one containing a glob
/// metacharacter — is what output paths are made relative to, so
/// `--input '/data/reports/**/*.pdf'` mirrors the tree under `/data/reports`
/// into `--output`.
fn expand_glob(pattern: &str) -> Result<(Vec<std::path::PathBuf>, std::path::PathBuf), String> {
    // A plain directory is the most natural thing to hand a flag named
    // `--input`: sweep it recursively, keeping only files whose extension maps
    // to a known input format (a stray `.log`/`.DS_Store` must not fail the
    // batch). A glob stays verbatim — the user chose the files explicitly.
    let dir = Path::new(pattern);
    if dir.is_dir() {
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let entries =
                std::fs::read_dir(&d).map_err(|e| format!("--input '{}': {e}", d.display()))?;
            for entry in entries {
                let p = entry.map_err(|e| format!("--input: {e}"))?.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| docling::InputFormat::from_extension(e).is_some())
                {
                    files.push(p);
                }
            }
        }
        if files.is_empty() {
            return Err(format!(
                "--input '{pattern}' contains no files with a convertible extension"
            ));
        }
        files.sort();
        return Ok((files, dir.to_path_buf()));
    }
    let base = glob_base(pattern);
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for entry in glob::glob(pattern).map_err(|e| format!("--input: {e}"))? {
        match entry {
            Ok(p) if p.is_file() => files.push(p),
            Ok(_) => {} // directories the pattern happens to match
            Err(e) => eprintln!("warning: {e}"),
        }
    }
    if files.is_empty() {
        return Err(format!("--input '{pattern}' matches no files"));
    }
    files.sort();
    Ok((files, base))
}

/// The static prefix of a glob pattern: every path component before the first
/// one containing a metacharacter. A metachar-free pattern is a literal file
/// path, whose base is its parent directory.
fn glob_base(pattern: &str) -> std::path::PathBuf {
    let mut base = std::path::PathBuf::new();
    for comp in Path::new(pattern).components() {
        let text = comp.as_os_str().to_string_lossy();
        if text.contains(['*', '?', '[']) {
            break;
        }
        base.push(comp);
    }
    if base == Path::new(pattern) {
        base.pop();
    }
    base
}

/// Where a converted file lands: `--output` + the input's path relative to the
/// glob base, with the extension swapped per `--to`.
/// One unit of batch work: a file, or a document inside a ZIP archive (#557).
struct BatchItem {
    /// The file — for an archive entry `<archive minus .zip>/<entry path>`,
    /// which places its outputs (`out/<archive>/<entry>.md`); for a loaded
    /// source (#668) `<name>.<ext>`, a stem carrier with no file behind it.
    file: std::path::PathBuf,
    base: std::path::PathBuf,
    /// The archive and directory index an entry is read from.
    entry: Option<(std::path::PathBuf, usize)>,
    /// A source already in memory — stdin, a fetched URL, a `data:` URL
    /// (#668) — with the argument that named it, for progress lines.
    preloaded: Option<(String, SourceDocument)>,
    /// `file` is a ZIP archive converted as one document (#677,
    /// `--archive-merge`): `<archive>.<ext>` holds every entry.
    merged: bool,
}

impl BatchItem {
    /// How progress and errors name the item: the path, `archive.zip:entry`,
    /// or the argument a loaded source was given as.
    fn label(&self) -> String {
        if let Some((label, _)) = &self.preloaded {
            return label.clone();
        }
        match &self.entry {
            None => self.file.display().to_string(),
            Some((zip, _)) => {
                let inner = self
                    .file
                    .strip_prefix(zip.with_extension(""))
                    .unwrap_or(&self.file);
                format!("{}:{}", zip.display(), inner.display())
            }
        }
    }
}

/// What archive expansion left out: entries skipped (unsupported, nested,
/// unsafe, over a limit) and archives that would not open.
#[derive(Default)]
struct ArchiveReport {
    skipped: usize,
    failed: usize,
}

/// Whether a path names a ZIP archive (by extension, as every input is).
fn is_zip(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Turn the batch's files into work items, a `.zip` into one item per
/// document it holds (#557). Its entries are listed from the central
/// directory — nothing is inflated yet — and the ones that do not convert are
/// reported here with the reason; an archive that cannot be read is an error
/// for that source only. Under `merge` (#677) a `.zip` is one item instead,
/// converted as one document when its turn comes (its skips are reported
/// then).
fn expand_archives(
    files: Vec<(std::path::PathBuf, std::path::PathBuf)>,
    merge: bool,
) -> (Vec<BatchItem>, ArchiveReport) {
    let mut items = Vec::new();
    let mut report = ArchiveReport::default();
    for (file, base) in files {
        if !is_zip(&file) || merge {
            let merged = is_zip(&file);
            items.push(BatchItem {
                file,
                base,
                entry: None,
                preloaded: None,
                merged,
            });
            continue;
        }
        let archive = std::fs::File::open(&file)
            .map_err(|e| e.to_string())
            .and_then(|f| {
                docling::archive::Archive::open(
                    std::io::BufReader::new(f),
                    &docling::ArchiveLimits::from_env(),
                )
                .map_err(|e| e.to_string())
            });
        let archive = match archive {
            Ok(a) => a,
            Err(e) => {
                eprintln!("error: {}: {e}", file.display());
                report.failed += 1;
                continue;
            }
        };
        let root = file.with_extension("");
        for entry in archive.entries() {
            if let Some(reason) = &entry.skipped {
                eprintln!("skip: {}:{}: {reason}", file.display(), entry.path);
                report.skipped += 1;
                continue;
            }
            items.push(BatchItem {
                // The entry's segments one by one: a ZIP path is
                // `/`-separated, and `join`ing it whole leaves that `/`
                // inside the Windows output path (`bundle\sub/b.md`).
                file: root.join(entry.path.split('/').collect::<std::path::PathBuf>()),
                base: base.clone(),
                entry: Some((file.clone(), entry.index)),
                preloaded: None,
                merged: false,
            });
        }
    }
    (items, report)
}

/// A `.zip` as one document (#677): every convertible entry under a
/// `## <entry path>` heading, in archive order, through the library's
/// [`convert_archive_merged`](DocumentConverter::convert_archive_merged).
/// What is left out is reported on stderr as the batch reports it (`skip:
/// bundle.zip:tool.exe: unsupported file type`, `error: bundle.zip:x.docx:
/// …`); an entry that failed leaves the others in. Errors when the archive
/// does not open or holds nothing that converted.
fn merge_archive(zip: &Path, converter: &DocumentConverter) -> Result<DoclingDocument, String> {
    let name = zip
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
    let file = std::fs::File::open(zip).map_err(|e| format!("{}: {e}", zip.display()))?;
    let merged = converter
        .convert_archive_merged(&name, std::io::BufReader::new(file))
        .map_err(|e| format!("{}: {e}", zip.display()))?;
    for (path, reason) in &merged.skipped {
        eprintln!("skip: {}:{path}: {reason}", zip.display());
    }
    for (path, e) in &merged.failed {
        eprintln!("error: {}:{path}: {e}", zip.display());
    }
    for e in &merged.errors {
        eprintln!(
            "warning: {}: partial document: {}",
            zip.display(),
            e.describe()
        );
    }
    if merged.converted == 0 {
        return Err(format!(
            "{}: the archive holds no document to convert",
            zip.display()
        ));
    }
    Ok(merged.document)
}

/// The archives a batch worker has open, so each is read once per worker
/// rather than once per entry.
#[derive(Default)]
struct ArchiveCache {
    open: std::collections::HashMap<
        std::path::PathBuf,
        docling::archive::Archive<std::io::BufReader<std::fs::File>>,
    >,
}

impl ArchiveCache {
    fn read(&mut self, zip: &Path, index: usize) -> Result<SourceDocument, String> {
        if !self.open.contains_key(zip) {
            let f = std::fs::File::open(zip).map_err(|e| e.to_string())?;
            let archive = docling::archive::Archive::open(
                std::io::BufReader::new(f),
                &docling::ArchiveLimits::from_env(),
            )
            .map_err(|e| e.to_string())?;
            self.open.insert(zip.to_path_buf(), archive);
        }
        self.open
            .get_mut(zip)
            .expect("inserted above")
            .read(index)
            .map_err(|e| e.to_string())
    }
}

fn batch_out_path(file: &Path, base: &Path, output: &Path, to: &str) -> std::path::PathBuf {
    let rel = file
        .strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| Path::new(file.file_name().unwrap_or_default()).to_path_buf());
    let ext = match to {
        "json" => "json",
        "html" => "html",
        "dclx" => "dclx",
        "chunks" => "chunks.json",
        "latex" => "tex",
        "text" => "txt",
        "vtt" => "vtt",
        // #515: Pandoc's own extension is `.json`; the double extension keeps
        // it apart from docling's JSON when both are requested.
        "pandoc" => "pandoc.json",
        "images" => "png", // a stem carrier: pages land as `<stem>_page_NNNN.png`
        _ => "md",
    };
    output.join(rel).with_extension(ext)
}

/// Mirror of the single-file converter construction for batch workers.
fn batch_converter(
    cfg: &BatchCfg,
    progress: Option<docling::ProgressCallback>,
) -> DocumentConverter {
    let converter = DocumentConverter::from_options(&cfg.opts)
        .expect("options validated before the batch")
        .archive_limits(docling::ArchiveLimits::from_env());
    match progress {
        Some(callback) => converter.progress_callback(callback),
        None => converter,
    }
}

/// The batch's one stderr printer (#783), shared by every worker so the bars
/// of documents converting at once share the terminal line; `None` without
/// `--progress`. The batch logs its own `start:` / `ok:` lines per document,
/// so the printer draws the bars alone.
fn batch_printer(cfg: &BatchCfg, total_documents: usize) -> Option<docling::ProgressCallback> {
    cfg.progress.then(|| {
        std::sync::Arc::new(
            docling::ProgressPrinter::new()
                .with_total_documents(total_documents)
                .with_document_lines(false),
        )
        .callback()
    })
}

/// The lazily-built warm PDF/image pipeline shared by every batch worker —
/// models load once and every subsequent PDF/image reuses the sessions, the
/// way docling-serve's warm pipeline does. Flags are frozen for the run, so
/// unlike serve there is nothing to rebuild per file.
fn batch_pipeline<'a>(
    slot: &'a mut Option<Pipeline>,
    cfg: &BatchCfg,
) -> Result<&'a mut Pipeline, String> {
    if slot.is_none() {
        // The typed readers of the validated option set (#577) — the same
        // values `DocumentConverter::from_options` applies on the
        // declarative path, here on the warm pipeline.
        let o = &cfg.opts;
        let err = |e: docling::OptionsError| e.cli_message();
        let mut p = Pipeline::new()
            .map_err(|e| e.to_string())?
            .no_table_former(o.no_table_former.unwrap_or(false))
            // docling-pdf's pre-2.0 names: its `no_ocr` is the text-layer
            // fast path, its `skip_ocr` docling's do_ocr=False (#611).
            .no_ocr(o.text_layer_only == Some(docling::TextLayerOnly::On))
            .text_layer_auto(o.text_layer_only == Some(docling::TextLayerOnly::Auto))
            .skip_ocr(o.ocr_disabled())
            .force_full_page_ocr(o.force_full_page_ocr.unwrap_or(false))
            .no_text_panels(o.no_text_panels.unwrap_or(false))
            .heading_hierarchy(docling::HeadingHierarchyOptions::enabled(
                o.heading_hierarchy.unwrap_or(false),
            ))
            .ocr_mode(o.ocr_mode().map_err(err)?)
            .ocr_engine(o.ocr_engine().map_err(err)?)
            .tesseract_lang(o.tesseract_lang().map_err(err)?)
            .ocr_scale(o.ocr_scale)
            .word_gap(o.pdf_word_gap().map_err(err)?)
            .keep_typography(o.pdf_keep_typography)
            .images_scale(o.images_scale)
            .generate_page_images(o.page_images.unwrap_or(false))
            .enrichments(o.enrichments())
            .document_timeout(o.document_timeout().map_err(err)?);
        p.set_pages(cfg.pages);
        p.set_ocr_lang(o.ocr_lang().map_err(err)?);
        p.set_ocr_textline_orientation(o.ocr_textline_orientation);
        // Progress (#783) is per document: `batch_convert_one` installs each
        // document's reporter as the hook (the pre-#783 dot progress is gone).
        *slot = Some(p);
    }
    Ok(slot.as_mut().expect("just filled"))
}

/// `--to images` (#243): rasterize a PDF to per-page PNGs,
/// `<dir>/<stem>_page_NNNN.png` — absolute 1-based page numbers, so a
/// `--pages` window keeps the source document's numbering. Returns the
/// written paths in page order.
fn write_page_images(
    bytes: &[u8],
    password: Option<&str>,
    pages: Option<(usize, usize)>,
    scale: f32,
    dir: &Path,
    stem: &str,
) -> Result<Vec<std::path::PathBuf>, String> {
    let rendered =
        docling::render_pdf_pages(bytes, password, pages, scale).map_err(|e| e.to_string())?;
    let mut written = Vec::with_capacity(rendered.len());
    for page in &rendered {
        let out = dir.join(format!("{stem}_page_{:04}.png", page.page_no));
        std::fs::write(&out, &page.png).map_err(|e| format!("writing {}: {e}", out.display()))?;
        written.push(out);
    }
    Ok(written)
}

/// Convert one batch file and write its output; returns the output path.
fn batch_convert_one(
    item: &BatchItem,
    output: &Path,
    cfg: &BatchCfg,
    converter: &DocumentConverter,
    pipe: &std::sync::Mutex<Option<Pipeline>>,
    archives: &mut ArchiveCache,
    document_index: usize,
) -> Result<BatchOutcome, String> {
    let (file, base) = (item.file.as_path(), item.base.as_path());
    let label = item.label();
    // A merged archive (#677) has no one source: its document is the
    // library's merge, written like any other document's.
    if item.merged {
        if cfg.to.iter().any(|t| t == "images") {
            return Err(format!(
                "--to images rasterizes PDF inputs only ({label} is an archive)"
            ));
        }
        if cfg.vlm.is_some() {
            return Err(format!(
                "--pipeline vlm takes one document at a time ({label} is an archive): \
                 drop --archive-merge"
            ));
        }
        eprintln!("start: {label}");
        let started = std::time::Instant::now();
        let document = merge_archive(file, converter)?;
        let formats: Vec<&str> = cfg.to.iter().map(String::as_str).collect();
        let mut written = Vec::new();
        write_batch_outputs(&document, file, base, output, cfg, &formats, &mut written)?;
        return Ok(BatchOutcome {
            written,
            secs: started.elapsed().as_secs_f64(),
            pages: None,
            partial: Vec::new(),
        });
    }
    let source = match (&item.preloaded, &item.entry) {
        (Some((_, source)), _) => source.clone(),
        (None, None) => SourceDocument::from_file(file).map_err(|e| e.to_string())?,
        (None, Some((zip, index))) => archives.read(zip, *index)?,
    };
    // Announce the document up front — with its page count for PDFs, so long
    // conversions are attributable while the bars fill.
    let pages = (source.format == InputFormat::Pdf)
        .then(|| docling::pdf_page_count(&source.bytes, cfg.opts.password.as_deref()).ok())
        .flatten()
        .map(|n| match cfg.pages {
            // A --pages window converts only its slice of the document.
            Some((first, last)) => (last.min(n) + 1).saturating_sub(first).min(n),
            None => n,
        });
    match pages {
        Some(1) => eprintln!("start: {label} (1 page)"),
        Some(n) => eprintln!("start: {label} ({n} pages)"),
        None => eprintln!("start: {label}"),
    }
    let started = std::time::Instant::now();
    let mut written: Vec<std::path::PathBuf> = Vec::new();
    // `--pdf-report-repairs` (#934): off the input bytes — the batch's warm
    // pipeline bypasses `convert`, whose result would carry them.
    if source.format == InputFormat::Pdf && converter.reports_pdf_repairs() {
        for repair in docling::pdf_repairs(&source.bytes) {
            eprintln!("{label}: repaired: {repair}");
        }
    }
    // `images` is rasterization, not conversion (#243): it runs on its own,
    // PDF-only like the serve endpoint (a non-PDF file fails its item, not
    // the batch), and the document formats, if any, follow from one
    // conversion below.
    if cfg.to.iter().any(|t| t == "images") {
        if source.format != InputFormat::Pdf {
            return Err(format!(
                "--to images rasterizes PDF inputs only ({label} is not a PDF)"
            ));
        }
        let out = batch_out_path(file, base, output, "images");
        let dir = out.parent().unwrap_or(Path::new("")).to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let stem = out
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        // The shared pipeline mutex serializes PDF work in this process
        // (pdfium, when the `pdfium` feature renders with it, is not
        // thread-safe), held here even though no models run — a render must
        // not race a concurrent PDF conversion.
        let pages_written = {
            let _pdf_owner = pipe.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let password = cfg.opts.password.as_deref();
            write_page_images(&source.bytes, password, cfg.pages, cfg.scale, &dir, &stem)?
        };
        written.push(pages_written.first().cloned().unwrap_or(out));
    }
    let formats: Vec<&str> = cfg
        .to
        .iter()
        .map(String::as_str)
        .filter(|t| *t != "images")
        .collect();
    if formats.is_empty() {
        return Ok(BatchOutcome {
            written,
            secs: started.elapsed().as_secs_f64(),
            pages,
            partial: Vec::new(),
        });
    }
    // Problems the conversion survived — docling's `ConversionResult.errors`:
    // a spent `--document-timeout` (#497) leaves the pages done so far, a
    // failed or cut-off VLM page (#762, #763) the pages around it.
    let mut partial: Vec<String> = Vec::new();
    // The PII pass's report (#621): the converter branch runs the pass
    // itself; the VLM and warm-pipeline branches bypass `convert`, so it
    // is applied to their document below.
    let mut redaction = None;
    let bypasses_converter =
        cfg.vlm.is_some() || matches!(source.format, InputFormat::Pdf | InputFormat::Image);
    let mut document = if let Some(vlm) = &cfg.vlm {
        let result = docling::vlm::convert_vlm_result(&source, vlm).map_err(|e| e.to_string())?;
        partial.extend(result.errors.iter().map(docling::ErrorItem::describe));
        result.document
    } else if matches!(source.format, InputFormat::Pdf | InputFormat::Image) {
        // One warm pipeline for the whole run: workers serialize on it (its
        // internal page workers already use the machine), declarative files
        // keep converting in parallel around it.
        let mut guard = pipe.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let p = batch_pipeline(&mut guard, cfg)?;
        // The document's progress events (#783) on the batch's own warm
        // pipeline: this document's reporter is the pipeline's hook for the
        // conversion, as `DocumentConverter::convert` would install it.
        let reporter = std::sync::Arc::new(converter.reporter(document_index, &source.name, None));
        reporter.document_started();
        p.set_progress(reporter.page_hook());
        let converted = match source.format {
            InputFormat::Pdf => p
                .convert_outcome(&source.bytes, cfg.opts.password.as_deref(), &source.name)
                .map(|c| {
                    if let docling::Completion::TimedOut { pages_selected, .. } = &c.completion {
                        reporter.fail_pages_cut_off(
                            cfg.pages.map_or(1, |(first, _)| first),
                            *pages_selected,
                        );
                    }
                    partial.extend(c.completion.message());
                    c.document
                }),
            _ => p.convert_image(&source.bytes, &source.name),
        };
        p.set_progress(None);
        match converted {
            Ok(mut document) => {
                // The picture enrichment `convert` runs (#896): without it
                // `--no-picture-images` reached a batch's PDFs only through
                // the single-file path.
                converter.enrich_pictures(&mut document, &source, &reporter);
                reporter.document_completed(if partial.is_empty() {
                    docling::ConversionStatus::Success
                } else {
                    docling::ConversionStatus::PartialSuccess
                });
                document
            }
            Err(e) => {
                reporter.document_completed(docling::ConversionStatus::Failure);
                return Err(e.to_string());
            }
        }
    } else {
        let result = converter
            .convert_with_progress(source, document_index, None)
            .map_err(|e| e.to_string())?;
        partial.extend(result.errors.iter().map(docling::ErrorItem::describe));
        redaction = result.redaction;
        result.document
    };
    if bypasses_converter {
        redaction = converter.redact(&mut document).map_err(|e| e.to_string())?;
    }
    if let Some(report) = &redaction {
        eprintln!("{}: redacted: {}", item.label(), redaction_summary(report));
    }
    document.strict_markdown = cfg.opts.strict.unwrap_or(false);
    document.page_break_placeholder = cfg.opts.page_break_placeholder.clone();
    write_batch_outputs(&document, file, base, output, cfg, &formats, &mut written)?;
    Ok(BatchOutcome {
        written,
        secs: started.elapsed().as_secs_f64(),
        pages,
        partial,
    })
}

/// Write `document` under `output` in every requested format (the batch's
/// `out/<rel>.<ext>` naming), the paths pushed onto `written`.
fn write_batch_outputs(
    document: &DoclingDocument,
    file: &Path,
    base: &Path,
    output: &Path,
    cfg: &BatchCfg,
    formats: &[&str],
    written: &mut Vec<std::path::PathBuf>,
) -> Result<(), String> {
    for &to in formats {
        let out = batch_out_path(file, base, output, to);
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        match to {
            // Streamed into the file: a long PDF's JSON runs to the better part
            // of a gigabyte, which `export_to_json` would hold as one `String`.
            "json" => std::fs::File::create(&out)
                .map(std::io::BufWriter::new)
                .and_then(|mut w| {
                    document.write_json_pretty(&mut w)?;
                    std::io::Write::flush(&mut w)
                })
                .map_err(|e| format!("writing {}: {e}", out.display()))?,
            "chunks" => std::fs::write(&out, chunks_json(document, &cfg.chunk)?)
                .map_err(|e| format!("writing {}: {e}", out.display()))?,
            // #317: the upstream CLI writes the serializer's text verbatim.
            "latex" => std::fs::write(&out, document.export_to_latex())
                .map_err(|e| format!("writing {}: {e}", out.display()))?,
            // #613: docling's `--to text` writes `export_to_text()` verbatim.
            "text" => std::fs::write(&out, cfg.export.text(document))
                .map_err(|e| format!("writing {}: {e}", out.display()))?,
            // #614: docling's `--to vtt` (`save_as_vtt`), verbatim.
            "vtt" => std::fs::write(&out, document.export_to_vtt())
                .map_err(|e| format!("writing {}: {e}", out.display()))?,
            "dclx" => docling::dclx::save_as_dclx(document, &out).map_err(|e| e.to_string())?,
            // #515: the Pandoc AST; pictures follow `--images` like HTML,
            // embedded when it is not given (#537).
            "pandoc" => {
                let stem = out
                    .file_stem()
                    .map(|s| s.to_string_lossy().trim_end_matches(".pandoc").to_string())
                    .unwrap_or_else(|| "document".into());
                let (json, artifacts) = pandoc_json(
                    document,
                    cfg.pandoc_image_mode,
                    &format!("{stem}_artifacts"),
                    cfg.export,
                );
                let parent = out.parent().unwrap_or(Path::new(""));
                for (rel, bytes) in &artifacts {
                    let target = parent.join(rel);
                    if let Some(dir) = target.parent() {
                        std::fs::create_dir_all(dir)
                            .map_err(|e| format!("creating {}: {e}", dir.display()))?;
                    }
                    std::fs::write(&target, bytes)
                        .map_err(|e| format!("writing {}: {e}", target.display()))?;
                }
                std::fs::write(&out, json)
                    .map_err(|e| format!("writing {}: {e}", out.display()))?;
            }
            // #492: docling-core's HTML serializer; pictures follow `--images`
            // exactly as the Markdown branch below — `referenced` writes the
            // same `<stem>_artifacts/` files and the page links to them.
            "html" => {
                let stem = out
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "document".into());
                let art = format!("{stem}_artifacts");
                let (html, artifacts) = cfg.export.html(document, cfg.image_mode, &art);
                let parent = out.parent().unwrap_or(Path::new(""));
                for (rel, bytes) in &artifacts {
                    let target = parent.join(rel);
                    if let Some(dir) = target.parent() {
                        std::fs::create_dir_all(dir)
                            .map_err(|e| format!("creating {}: {e}", dir.display()))?;
                    }
                    std::fs::write(&target, bytes)
                        .map_err(|e| format!("writing {}: {e}", target.display()))?;
                }
                std::fs::write(&out, html)
                    .map_err(|e| format!("writing {}: {e}", out.display()))?;
            }
            _ => {
                if cfg.image_mode == ImageMode::Placeholder {
                    let (md, _) = cfg.export.markdown(document, cfg.image_mode, "artifacts");
                    std::fs::write(&out, md)
                        .map_err(|e| format!("writing {}: {e}", out.display()))?;
                } else {
                    // `referenced` images land next to the output file, in a
                    // per-document `<stem>_artifacts/` dir the links point into.
                    let stem = out
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "document".into());
                    let art = format!("{stem}_artifacts");
                    let (md, artifacts) = cfg.export.markdown(document, cfg.image_mode, &art);
                    let parent = out.parent().unwrap_or(Path::new(""));
                    for (rel, bytes) in &artifacts {
                        let target = parent.join(rel);
                        if let Some(dir) = target.parent() {
                            std::fs::create_dir_all(dir)
                                .map_err(|e| format!("creating {}: {e}", dir.display()))?;
                        }
                        std::fs::write(&target, bytes)
                            .map_err(|e| format!("writing {}: {e}", target.display()))?;
                    }
                    std::fs::write(&out, md)
                        .map_err(|e| format!("writing {}: {e}", out.display()))?;
                }
            }
        }
        written.push(out);
    }
    Ok(())
}

/// One batch file's result: the files written, the wall time, the page
/// count (PDFs) and the problems the conversion survived (`partial` —
/// non-empty means docling's `PARTIAL_SUCCESS`, today a spent
/// `--document-timeout`).
struct BatchOutcome {
    written: Vec<std::path::PathBuf>,
    secs: f64,
    pages: Option<usize>,
    partial: Vec<String>,
}

/// `--output-file PATH` (#611): docling's checks first, in its order and
/// with its messages — exactly one input document (a directory, a glob or a ZIP that holds more
/// is several) and exactly one output format (`--to images` writes a PNG per
/// page, so it is not one file). The document then goes through the batch
/// writer in a scratch directory beside PATH; the result moves onto PATH and
/// whatever it wrote next to it — the `<stem>_artifacts/` of `--images
/// referenced`, which its links point into relatively — into PATH's
/// directory, where docling exports them too. Progress goes to stderr;
/// nothing is printed on stdout.
fn write_output_file(
    target: &Path,
    sources: &[&String],
    hints: &SourceHints,
    cfg: &BatchCfg,
    merge_archives: bool,
) -> ExitCode {
    let (files, preloaded) = match expand_sources(sources.iter().copied(), hints) {
        Ok(split) => split,
        Err((code, msg)) => {
            eprintln!("error: {msg}");
            return ExitCode::from(code);
        }
    };
    let (mut items, report) = expand_archives(files, merge_archives);
    items.extend(preloaded);
    if items.len() != 1 || report.failed + report.skipped > 0 {
        eprintln!(
            "error: --output-file requires exactly one input document ({} given)",
            items.len() + report.failed + report.skipped
        );
        return ExitCode::from(2);
    }
    if cfg.to.len() != 1 || cfg.to[0] == "images" {
        eprintln!("error: --output-file requires exactly one output format");
        if cfg.to.iter().any(|t| t == "images") {
            eprintln!("(--to images writes one PNG per page: use --output DIR)");
        }
        return ExitCode::from(2);
    }
    let item = items.remove(0);
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("error: creating {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }
    let scratch = dir.join(format!(".docling-rs-{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let converter = batch_converter(cfg, batch_printer(cfg, 1));
    let pipe = std::sync::Mutex::new(None);
    let mut archives = ArchiveCache::default();
    let result = batch_convert_one(&item, &scratch, cfg, &converter, &pipe, &mut archives, 1)
        .and_then(|outcome| {
            let written = outcome
                .written
                .first()
                .cloned()
                .ok_or_else(|| "the conversion wrote no output".to_string())?;
            place_output(&written, target, dir)?;
            Ok(outcome)
        });
    let _ = std::fs::remove_dir_all(&scratch);
    match result {
        Ok(outcome) => {
            for problem in &outcome.partial {
                eprintln!("warning: partial document: {problem}");
            }
            eprintln!(
                "ok: {} -> {} ({:.1}s)",
                item.label(),
                target.display(),
                outcome.secs
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {}: {e}", item.label());
            ExitCode::FAILURE
        }
    }
}

/// Move `written` onto `target`, and everything else in its directory into
/// `dir` — merged into an existing directory of the same name, a file
/// replaced, nothing else of the user's touched.
fn place_output(written: &Path, target: &Path, dir: &Path) -> Result<(), String> {
    fn move_into(src: &Path, dst: &Path) -> Result<(), String> {
        if src.is_dir() && dst.is_dir() {
            for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                move_into(&entry.path(), &dst.join(entry.file_name()))?;
            }
            return Ok(());
        }
        if dst.is_file() {
            std::fs::remove_file(dst).map_err(|e| format!("replacing {}: {e}", dst.display()))?;
        }
        std::fs::rename(src, dst).map_err(|e| format!("writing {}: {e}", dst.display()))
    }
    move_into(written, target)?;
    let Some(from) = written.parent() else {
        return Ok(());
    };
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        move_into(&entry.path(), &dir.join(entry.file_name()))?;
    }
    Ok(())
}

/// Convert every matched file, `--jobs` workers wide. Output paths print to
/// stdout (one per line, for scripts); progress and errors go to stderr. A
/// failed file is reported and skipped — the batch keeps going, and the exit
/// code is non-zero if anything failed.
fn run_batch(
    files: Vec<BatchItem>,
    archive_report: ArchiveReport,
    output: &Path,
    jobs: usize,
    abort_on_error: bool,
    cfg: &BatchCfg,
) -> ExitCode {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let succeeded = AtomicUsize::new(0);
    let partial = AtomicUsize::new(0);
    // Fail fast on a broken execution provider: an explicit DOCLING_RS_EP
    // whose runtime libraries are missing fails *every* PDF/image identically
    // — the first such error aborts the rest of the batch instead of
    // repeating itself per file.
    let abort = AtomicBool::new(false);
    let pipe: std::sync::Mutex<Option<Pipeline>> = std::sync::Mutex::new(None);
    let workers = jobs.min(files.len()).max(1);
    let printer = batch_printer(cfg, files.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                // Converter construction is cheap configuration; one per
                // worker keeps the loop borrow-free.
                let converter = batch_converter(cfg, printer.clone());
                // Archives this worker has opened, kept for their next entry.
                let mut archives = ArchiveCache::default();
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = files.get(i) else {
                        break;
                    };
                    let file = item.label();
                    // A backend that panics on one file must not take the
                    // batch down with it (#395/#396): the documented contract
                    // here is "a failed file is reported and skipped". The
                    // panic still prints its own message and backtrace from
                    // the unwind; this only decides what happens next. The
                    // shared pipeline slot already recovers from a lock
                    // poisoned by such a panic, and the per-worker converter
                    // is plain configuration, so the next file starts clean.
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        batch_convert_one(
                            item,
                            output,
                            cfg,
                            &converter,
                            &pipe,
                            &mut archives,
                            i + 1,
                        )
                    }))
                    .unwrap_or_else(|_| {
                        Err("the conversion panicked (its message and backtrace are above)".into())
                    });
                    match outcome {
                        Ok(BatchOutcome {
                            written: outs,
                            secs,
                            pages,
                            partial: problems,
                        }) => {
                            let shown = outs
                                .iter()
                                .map(|o| o.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            // Python docling's `PARTIAL_SUCCESS`: the files are
                            // written (the pages that fit the budget), the
                            // document counts as converted, and the reason
                            // is logged — `--abort-on-error` alone makes it
                            // end the batch (#497).
                            let tag = if problems.is_empty() { "ok" } else { "partial" };
                            match pages {
                                Some(n) if n > 0 => eprintln!(
                                    "{tag}: {file} -> {shown} ({secs:.1}s, {:.0} ms/page)",
                                    secs * 1000.0 / n as f64
                                ),
                                _ => eprintln!("{tag}: {file} -> {shown} ({secs:.1}s)"),
                            }
                            for problem in &problems {
                                eprintln!("warning: {file}: {problem}");
                            }
                            for out in &outs {
                                println!("{}", out.display());
                            }
                            if problems.is_empty() {
                                succeeded.fetch_add(1, Ordering::Relaxed);
                            } else {
                                partial.fetch_add(1, Ordering::Relaxed);
                                if abort_on_error {
                                    abort.store(true, Ordering::Relaxed);
                                    eprintln!(
                                        "aborting the batch (--abort-on-error: the document \
                                         was only partially converted)"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            eprintln!("error: {file}: {e}");
                            // Python's `--abort-on-error` (#489): the first
                            // failure ends the batch; the default keeps
                            // going and reports the failure in the exit code.
                            if abort_on_error {
                                abort.store(true, Ordering::Relaxed);
                                eprintln!("aborting the batch (--abort-on-error)");
                            }
                            if e.contains("execution provider") {
                                abort.store(true, Ordering::Relaxed);
                                eprintln!(
                                    "fatal: the requested execution provider is \
                                     unavailable — aborting the batch (fix the \
                                     DOCLING_RS_EP runtime libraries or unset it)"
                                );
                            }
                            // Missing models fail every PDF/image the
                            // same way — one report is enough (the error above
                            // already says how to install the assets).
                            if e.contains("pdfium library is not installed")
                                || e.contains("model not found at")
                            {
                                abort.store(true, Ordering::Relaxed);
                                eprintln!(
                                    "fatal: the PDF runtime assets are missing — \
                                     aborting the batch (every PDF/image would \
                                     fail identically)"
                                );
                            }
                        }
                    }
                }
            });
        }
    });
    let ran = failed.load(Ordering::Relaxed);
    let ok = succeeded.load(Ordering::Relaxed);
    let np = partial.load(Ordering::Relaxed);
    let skipped = files.len() - ok - ran - np;
    // An archive that would not open counts as a failed source.
    let nf = ran + archive_report.failed;
    let mut summary = format!("batch: {ok} converted, {nf} failed");
    if np > 0 {
        summary.push_str(&format!(", {np} partial"));
    }
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    if archive_report.skipped > 0 {
        summary.push_str(&format!(
            ", {} archive entries not converted",
            archive_report.skipped
        ));
    }
    eprintln!("{summary}");
    if nf > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The Pandoc AST per `--images` (#515) and `--pandoc-heading-depth`
/// (#848). The API version was checked when `--pandoc-api-version` was
/// parsed, so the default options apply.
fn pandoc_json(
    document: &docling::DoclingDocument,
    image_mode: ImageMode,
    artifacts_dir: &str,
    export: ExportLayers,
) -> (String, Vec<(String, Vec<u8>)>) {
    document
        .export_to_pandoc_json_with(&docling::pandoc::PandocExportOptions {
            image_mode,
            artifacts_dir: artifacts_dir.to_string(),
            heading_depth: export.pandoc_heading_depth,
            track_changes: export.track_changes,
            ..Default::default()
        })
        .expect("the default Pandoc API version is always supported")
}

fn output_document(
    document: docling::DoclingDocument,
    to: &str,
    image_mode: ImageMode,
    path: &str,
    chunk: &ChunkOptions,
    export: ExportLayers,
) -> ExitCode {
    if to == "json" {
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        let written = document
            .write_json_pretty(&mut out)
            .and_then(|()| std::io::Write::write_all(&mut out, b"\n"))
            .and_then(|()| std::io::Write::flush(&mut out));
        if let Err(e) = written {
            eprintln!("error: writing JSON: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    // #492: a complete HTML document on stdout; `--images referenced` writes
    // the pictures under ./artifacts/ like the Markdown path does.
    if to == "html" {
        let (html, artifacts) = export.html(&document, image_mode, "artifacts");
        for (rel, bytes) in &artifacts {
            let target = Path::new(rel);
            if let Some(dir) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    eprintln!("error: creating {}: {e}", dir.display());
                    return ExitCode::FAILURE;
                }
            }
            if let Err(e) = std::fs::write(target, bytes) {
                eprintln!("error: writing {}: {e}", target.display());
                return ExitCode::FAILURE;
            }
        }
        if !artifacts.is_empty() {
            eprintln!("referenced images written to ./artifacts/");
        }
        println!("{html}");
        return ExitCode::SUCCESS;
    }

    // #515: the Pandoc AST as JSON on stdout, for `| pandoc -f json -t …`;
    // `--images referenced` writes the pictures under ./artifacts/.
    if to == "pandoc" {
        let (json, artifacts) = pandoc_json(&document, image_mode, "artifacts", export);
        for (rel, bytes) in &artifacts {
            let target = Path::new(rel);
            if let Some(dir) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    eprintln!("error: creating {}: {e}", dir.display());
                    return ExitCode::FAILURE;
                }
            }
            if let Err(e) = std::fs::write(target, bytes) {
                eprintln!("error: writing {}: {e}", target.display());
                return ExitCode::FAILURE;
            }
        }
        if !artifacts.is_empty() {
            eprintln!("referenced images written to ./artifacts/");
        }
        println!("{json}");
        return ExitCode::SUCCESS;
    }

    // #317: docling 2.124's `--to latex`. The serializer's text carries no
    // trailing newline (upstream writes it verbatim to `<stem>.tex`); stdout
    // gets one so a shell prompt doesn't land on `\end{document}`.
    if to == "latex" {
        println!("{}", document.export_to_latex());
        return ExitCode::SUCCESS;
    }

    // #613: docling's `--to text` — the plain-text serializer, a trailing
    // newline added on stdout as for LaTeX.
    if to == "text" {
        println!("{}", export.text(&document));
        return ExitCode::SUCCESS;
    }

    // #614: docling's `--to vtt` — a cue per timed text item, the bare
    // `WEBVTT` header for a document without any.
    if to == "vtt" {
        println!("{}", document.export_to_vtt());
        return ExitCode::SUCCESS;
    }

    if to == "chunks" {
        // Chunking conformance/debug dump: a JSON object with the hierarchical
        // chunk records and, when a tokenizer is configured, the hybrid ones.
        // `DOCLING_CHUNK_TOKENIZER` points at a HuggingFace tokenizer.json
        // (`DOCLING_CHUNK_MAX_TOKENS` overrides the default budget of 256);
        // `--chunker`/`--chunk-*` override per run (#256).
        match chunks_json(&document, chunk) {
            Ok(json) => print!("{json}"),
            Err(e) => {
                eprintln!("error: chunks: {e}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    if to == "dclx" {
        // Binary OPC archive: written next to the CWD as `<input-stem>.dclx`
        // (stdout stays clean for terminals); the path is printed for scripts.
        let stem = Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".into());
        let out = std::path::PathBuf::from(format!("{stem}.dclx"));
        if let Err(e) = docling::dclx::save_as_dclx(&document, &out) {
            eprintln!("error: dclx: {e}");
            return ExitCode::FAILURE;
        }
        // Humans read stderr ("where did my file go?"); stdout stays the bare
        // path for scripts.
        eprintln!("dclx: archive written to {}", out.display());
        println!("{}", out.display());
        return ExitCode::SUCCESS;
    }

    let (md, artifacts) = export.markdown(&document, image_mode, "artifacts");
    for (rel, bytes) in &artifacts {
        let rel = Path::new(rel);
        if let Some(dir) = rel.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("error: creating {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }
        if let Err(e) = std::fs::write(rel, bytes) {
            eprintln!("error: writing {}: {e}", rel.display());
            return ExitCode::FAILURE;
        }
    }
    if !artifacts.is_empty() {
        eprintln!("wrote {} image(s) to ./artifacts/", artifacts.len());
    }
    print!("{md}");
    ExitCode::SUCCESS
}

/// `docling-rs serve …`: parse the serve flags and run the HTTP server.
#[cfg(feature = "serve")]
fn run_serve(args: Vec<String>) -> ExitCode {
    use docling_serve::ServeConfig;
    let mut cfg = ServeConfig::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--addr" => match it.next() {
                Some(v) => cfg.addr = v,
                None => return serve_usage("--addr needs HOST:PORT"),
            },
            "--concurrency" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) if v >= 1 => cfg.concurrency = v,
                _ => return serve_usage("--concurrency needs a positive integer"),
            },
            "--max-body-mb" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v >= 1 => cfg.max_body_bytes = v * 1024 * 1024,
                _ => return serve_usage("--max-body-mb needs a positive integer"),
            },
            "--queue-size" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) if v >= 1 => cfg.queue_size = v,
                _ => return serve_usage("--queue-size needs a positive integer"),
            },
            "--result-ttl" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) if v >= 1 => cfg.result_ttl_secs = v,
                _ => return serve_usage("--result-ttl needs a positive number of seconds"),
            },
            // #263: memory ceiling for admission control. 0 disables; unset =
            // auto-detect the container's cgroup limit.
            "--max-memory-mb" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => cfg.max_memory_mb = Some(v),
                None => return serve_usage("--max-memory-mb needs a number (0 disables)"),
            },
            "--warmup" => cfg.warmup = true,
            // #842: drop the warm pipeline (and its GPU memory) after N idle
            // seconds; 0 = never. Unset = DOCLING_RS_IDLE_UNLOAD_SECS.
            "--idle-unload-secs" => match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => cfg.idle_unload_secs = Some(v),
                None => {
                    return serve_usage("--idle-unload-secs needs a number of seconds (0 = never)")
                }
            },
            "--allow-url-fetch" => cfg.allow_url_fetch = true,
            "--no-url-fetch" => cfg.allow_url_fetch = false,
            "--strict" => cfg.strict = true,
            // #615: docling-serve's API key (`DOCLING_SERVE_API_KEY` when absent).
            "--api-key" => match it.next() {
                Some(v) if !v.is_empty() => cfg.api_key = Some(v),
                _ => return serve_usage("--api-key needs a key"),
            },
            // #892: the API without the web UI page (`DOCLING_RS_ENABLE_UI` /
            // `DOCLING_SERVE_ENABLE_UI` when absent); `--enable-ui` is
            // docling-serve's spelling of the default.
            "--no-ui" => cfg.ui = Some(false),
            "--enable-ui" => cfg.ui = Some(true),
            other => return serve_usage(&format!("unknown argument '{other}'")),
        }
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(docling_serve::serve(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "serve")]
fn serve_usage(err: &str) -> ExitCode {
    eprintln!("error: {err}");
    eprintln!("usage: docling-rs serve [--addr HOST:PORT] [--concurrency N] [--max-body-mb N] [--queue-size N] [--result-ttl SECS] [--max-memory-mb N] [--warmup] [--idle-unload-secs N] [--allow-url-fetch] [--strict] [--api-key KEY] [--no-ui | --enable-ui]");
    ExitCode::from(2)
}

/// Without the `serve` feature the subcommand explains how to get it.
#[cfg(not(feature = "serve"))]
fn run_serve(_args: Vec<String>) -> ExitCode {
    eprintln!(
        "error: this binary was built without the HTTP server.\n\
         Rebuild with `cargo build -p docling-cli --features serve`, or use the\n\
         standalone server: `cargo run -p docling-serve --release -- --help`."
    );
    ExitCode::from(2)
}

#[cfg(feature = "mcp")]
fn run_mcp(args: Vec<String>) -> ExitCode {
    docling_cli::mcp::run(args)
}

/// Without the `mcp` feature the subcommand explains how to get it.
#[cfg(not(feature = "mcp"))]
fn run_mcp(_args: Vec<String>) -> ExitCode {
    eprintln!(
        "error: this binary was built without the MCP server.\n\
         Rebuild with `cargo build -p docling-cli --features mcp` (it is in the\n\
         default feature set)."
    );
    ExitCode::from(2)
}

/// Build the PDF/image pipeline once (loading the ONNX models), then time `runs`
/// warm conversions and return the average seconds per conversion. The first
/// conversion is a discarded warm-up that triggers the lazy model loads, so the
/// timed runs reuse them — the startup-excluded figure comparable to docling's
/// in-process warm number.
fn bench_warm_conversion(
    source: &SourceDocument,
    runs: usize,
    no_table_former: bool,
    text_layer_only: docling::TextLayerOnly,
    no_ocr: bool,
    password: Option<&str>,
) -> Result<f64, String> {
    let mut pipeline = Pipeline::new()
        .map_err(|e| e.to_string())?
        .no_table_former(no_table_former)
        // docling-pdf's pre-2.0 names (see `batch_pipeline`).
        .no_ocr(text_layer_only == docling::TextLayerOnly::On)
        .text_layer_auto(text_layer_only == docling::TextLayerOnly::Auto)
        .skip_ocr(no_ocr);
    let once = |p: &mut Pipeline| -> Result<(), String> {
        match source.format {
            InputFormat::Pdf => p
                .convert(&source.bytes, password, &source.name)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            InputFormat::Image => p
                .convert_image(&source.bytes, &source.name)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            other => Err(format!(
                "--bench-warm supports PDF/image only, not {other:?}"
            )),
        }
    };
    once(&mut pipeline)?; // warm-up: load models, prime caches
    let mut total = 0.0f64;
    for _ in 0..runs {
        let t = std::time::Instant::now();
        once(&mut pipeline)?;
        total += t.elapsed().as_secs_f64();
    }
    Ok(total / runs as f64)
}

/// Serialize the chunk records `--to chunks` prints (see
/// [`docling::chunks::chunk_records_with`] for the tokenizer resolution
/// rules). Errors only when an explicitly requested configuration can't be
/// honored (`--chunker hybrid` without a usable tokenizer).
fn chunks_json(
    document: &docling::DoclingDocument,
    chunk: &ChunkOptions,
) -> Result<String, String> {
    let mut warn = |msg: String| eprintln!("warning: {msg}");
    let out = docling::chunks::chunk_records_with(document, chunk, &mut warn)?;
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(&out).expect("chunks are serializable")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--layers` / `--traverse-pictures` on a scanned letter's shape: a
    /// page footer (bank details) and a picture holding a form field. The
    /// defaults are the plain exports; each flag adds exactly its part.
    #[test]
    fn export_layers_add_the_footer_and_the_picture_text() {
        use docling::Node;
        let mut doc = DoclingDocument::new("letter");
        doc.add_paragraph("Invoice");
        doc.push(Node::Picture {
            caption: None,
            caption_href: None,
            image: None,
            classification: None,
            description: None,
            caption_parent: Default::default(),
            caption_location: None,
        });
        doc.push(Node::PictureChildren(vec![Node::Paragraph {
            text: "IBAN DE00".into(),
        }]));
        doc.push(Node::FurnitureText {
            label: "page_footer".into(),
            text: "Bank AG".into(),
        });
        let md = |e: ExportLayers| e.markdown(&doc, ImageMode::Placeholder, "artifacts").0;
        let html = |e: ExportLayers| e.html(&doc, ImageMode::Placeholder, "artifacts").0;
        let default = ExportLayers::default();
        assert_eq!(md(default), doc.export_to_markdown());
        assert_eq!(default.text(&doc), doc.export_to_text());
        assert_eq!(html(default), doc.export_to_html());
        assert!(!md(default).contains("Bank AG") && !md(default).contains("IBAN"));
        let all = ExportLayers {
            layers: ContentLayers::ALL,
            ..default
        };
        assert!(md(all).contains("Bank AG") && !md(all).contains("IBAN"));
        assert!(all.text(&doc).contains("Bank AG") && html(all).contains("Bank AG"));
        let traverse = ExportLayers {
            traverse_pictures: true,
            ..default
        };
        assert!(md(traverse).contains("IBAN DE00") && !md(traverse).contains("Bank AG"));
    }

    #[test]
    fn glob_base_stops_at_the_first_metachar_component() {
        assert_eq!(
            glob_base("/data/reports/**/*.pdf"),
            Path::new("/data/reports")
        );
        assert_eq!(glob_base("docs/*.md"), Path::new("docs"));
        assert_eq!(glob_base("*.md"), Path::new(""));
        assert_eq!(glob_base("a/b[12]/c/*.pdf"), Path::new("a"));
        // A literal file path (no metachars) bases at its parent, so a batch of
        // one lands directly under --output.
        assert_eq!(glob_base("dir/file.pdf"), Path::new("dir"));
    }

    /// `--output-dirs` (#496): `flat` drops every tree, `mirror` roots every
    /// input at the current directory and refuses what lies outside it.
    #[test]
    fn output_dirs_modes_rewrite_the_bases() {
        let p = |s: &str| std::path::PathBuf::from(s);
        let files = vec![
            (p("a/doc.docx"), p("a")),
            (p("b/sub/doc.docx"), p("b")),
            (p("c/README.md"), p("")),
        ];
        // auto: untouched.
        assert_eq!(
            apply_output_dirs(OutputDirs::Auto, files.clone()).unwrap(),
            files
        );
        // flat: every base is the file's own parent, so only the stem lands.
        let flat = apply_output_dirs(OutputDirs::Flat, files.clone()).unwrap();
        for (f, base) in &flat {
            assert_eq!(base, f.parent().unwrap());
            assert_eq!(
                batch_out_path(f, base, Path::new("out"), "md"),
                Path::new("out")
                    .join(f.file_stem().unwrap())
                    .with_extension("md")
            );
        }
        // mirror: the path relative to the current directory, folders kept.
        let cwd = std::env::current_dir().unwrap();
        let (f, base) = mirror_pair(Path::new("b/sub/doc.docx"), &cwd).unwrap();
        assert_eq!(f, Path::new("b/sub/doc.docx"));
        assert_eq!(base, Path::new(""));
        assert_eq!(
            batch_out_path(&f, &base, Path::new("out"), "json"),
            Path::new("out/b/sub/doc.json")
        );
        // `./` and an interior `..` normalize lexically.
        let (f, _) = mirror_pair(Path::new("./x/../b/doc.docx"), &cwd).unwrap();
        assert_eq!(f, Path::new("b/doc.docx"));
        // An absolute path below the current directory mirrors too.
        let (f, _) = mirror_pair(&cwd.join("c/README.md"), &cwd).unwrap();
        assert_eq!(f, Path::new("c/README.md"));
        // Outside the current directory: refused, not guessed at.
        let err = mirror_pair(Path::new("../elsewhere/doc.pdf"), &cwd).unwrap_err();
        assert!(err.contains("outside the current directory"), "{err}");
        let err = mirror_pair(Path::new("/nowhere/doc.pdf"), &cwd).unwrap_err();
        assert!(err.contains("outside the current directory"), "{err}");
        // The same stem under two folders collides flat, not mirrored.
        let flat = apply_output_dirs(
            OutputDirs::Flat,
            vec![(p("a/doc.docx"), p("a")), (p("b/doc.docx"), p("b"))],
        )
        .unwrap();
        assert!(check_output_collisions(&flat, Path::new("out"), &["md".to_string()]).is_err());
        let mirrored = apply_output_dirs(
            OutputDirs::Mirror,
            vec![(p("a/doc.docx"), p("a")), (p("b/doc.docx"), p("b"))],
        )
        .unwrap();
        assert!(check_output_collisions(&mirrored, Path::new("out"), &["md".to_string()]).is_ok());
    }

    /// `--document-timeout` (#497) takes a positive number of seconds.
    #[test]
    fn document_timeout_parses_positive_seconds() {
        assert_eq!(parse_document_timeout("90").unwrap(), 90.0);
        assert_eq!(parse_document_timeout(" 0.5 ").unwrap(), 0.5);
        assert!(parse_document_timeout("0").is_err());
        assert!(parse_document_timeout("-3").is_err());
        assert!(parse_document_timeout("soon").is_err());
        assert!(parse_document_timeout("inf").is_err());
    }

    #[test]
    fn batch_out_path_mirrors_structure_and_swaps_extension() {
        let out = |file: &str, to: &str| {
            batch_out_path(
                Path::new(file),
                Path::new("/data/reports"),
                Path::new("/out"),
                to,
            )
        };
        assert_eq!(
            out("/data/reports/a/b/x.pdf", "md"),
            Path::new("/out/a/b/x.md")
        );
        assert_eq!(out("/data/reports/x.pdf", "json"), Path::new("/out/x.json"));
        assert_eq!(out("/data/reports/x.pdf", "dclx"), Path::new("/out/x.dclx"));
        assert_eq!(out("/data/reports/x.pdf", "latex"), Path::new("/out/x.tex"));
        assert_eq!(
            out("/data/reports/talk.mp3", "vtt"),
            Path::new("/out/talk.vtt")
        );
        assert_eq!(
            out("/data/reports/a/x.pdf", "chunks"),
            Path::new("/out/a/x.chunks.json")
        );
        // A file outside the base still lands under --output by file name.
        assert_eq!(out("/elsewhere/y.docx", "md"), Path::new("/out/y.md"));
    }
}
