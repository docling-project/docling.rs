# Security notes

docling.rs converts **untrusted documents** (PDF, DOCX, XLSX, PPTX, HTML,
EPUB, images, audio, …). The parsers are the primary attack surface: a
document is adversarial input, and a converter must fail gracefully rather
than crash the process or reach out to the network unexpectedly. This note
records the hardening in place, how to deploy the servers safely, and the
known residual items.

## Threat model

- **In scope:** a crafted document must not cause remote code execution,
  out-of-process crashes (uncatchable aborts — allocation failure, stack
  overflow), unbounded memory/CPU (DoS), server-side request forgery (SSRF),
  local-file disclosure, or path traversal.
- **Out of scope / operational:** an attacker who can already write to the
  process working directory or set its environment (they can plant a
  malicious `.models/*.onnx` (or, under the `pdfium` feature, a `.pdfium/lib`) that the loader would pick up —
  run with a fixed, non-writable CWD and absolute `DOCLING_*` model paths).
  The `web-browser` feature runs system Chromium **unsandboxed** on the input
  HTML and is off by default; enable it only for content you accept that risk
  for.

## Hardening in place

Resource limits that turn a crafted document from a process abort into a
recoverable error or a skipped part. Every one is overridable by environment
variable (or server flag) for the rare legitimate outlier; `docs/env-vars.csv`
is the registry of every variable the code reads, and
`crates/docling/tests/security_doc.rs` fails when a limit, subprocess,
secret or network variable is missing from this page.

| Surface | Guard (default) | On exceed | Override |
|---------|-----------------|-----------|----------|
| Standalone image decode (`convert_image`, METS) | ONNX-free decode with `image::Limits`: 30000 px per side, 256 MiB allocation — a few-KB image declaring 60000×60000 no longer allocates ~10 GB | the image fails with an error | `DOCLING_RS_MAX_IMAGE_PIXELS` |
| Rendered PDF page bitmap | 15000 px per side — a few-hundred-byte PDF declaring a huge `MediaBox` would otherwise ask for a multi-GB bitmap | the page fails with an error before anything is allocated | `DOCLING_RS_MAX_RENDER_PIXELS` |
| Resolved images (HTML/EPUB/MHTML/JATS/AsciiDoc/ODF/Markdown/email, #646) | 32 MiB per image (also the remote fetch's read cap); per document: images resolved, their total bytes, a size floor — unlimited / unlimited / 0 by default | the picture stays a placeholder, one warning per document; the conversion never fails | `DOCLING_RS_MAX_IMAGE_BYTES`, `DOCLING_RS_MAX_IMAGES`, `DOCLING_RS_MAX_IMAGE_TOTAL_MB`, `DOCLING_RS_MIN_IMAGE_BYTES` (and the `max_image_bytes` / `max_images` / `max_image_total_mb` / `min_image_bytes` options) |
| Table cells a document declares (docling-JSON `num_rows × num_cols` / `grid`, DocTags and DocLang OTSL streams — the dense grid every export materializes, #861; docling-core 2.103.1's `max_table_cells` / `max_total_table_cells`) | 1 000 000 cells per table (rows and columns bounded alone too) and 5 000 000 over all of a document's tables — a 10^6 × 10^6 `TableData` around one cell, or one OTSL row of a million tokens plus a million one-token rows, asked for 10^12 slots from a few MB | the document is **rejected** with the typed `LimitExceeded` on the error's source chain (`ConversionError::limit()`; serve answers 422) — docling-core's own load-time error; the tolerant `doctags::parse` leaves the table out instead | `DOCLING_RS_MAX_TABLE_CELLS`, `DOCLING_RS_MAX_TOTAL_TABLE_CELLS` |
| DjVu scan render (OCR fallback) | the page is rendered into a 2500 px box | never fails: the bitmap is scaled down | `DOCLING_RS_DJVU_RENDER_PX` |
| OOXML / EPUB part inflation (DOCX, PPTX, XLSX, EPUB) | 512 MiB per decompressed part (checked against the declared size, then while reading, so a lying header cannot get past it) | the part is **skipped** (never truncated, no message) and the conversion continues without it | `DOCLING_RS_MAX_PART_BYTES` |
| ZIP inputs (a `.zip` given to the CLI or uploaded to serve, #557) | 10 000 entries · 256 MiB per entry · 1024 MiB in total · compression ratio 200 for an entry over 1 MiB — checked on the central directory before anything is inflated; nested ZIPs (#677) are shut by default (depth 1) and read at most 3 levels down, every level drawing on the same entry count and size budget (the nested archive's own bytes included) | the entry is skipped with its reason in the log; the other entries convert | `DOCLING_RS_ZIP_MAX_ENTRIES`, `DOCLING_RS_ZIP_MAX_ENTRY_MB`, `DOCLING_RS_ZIP_MAX_TOTAL_MB`, `DOCLING_RS_ZIP_MAX_RATIO`, `DOCLING_RS_ZIP_MAX_DEPTH` |
| XML element nesting (`roxmltree` recurses per level) | 512 levels, counted by an iterative scan before parsing | an OOXML / EPUB part is skipped (`docling: …; part skipped` on stderr); a standalone XML input (JATS, USPTO, XBRL, DocLang, AbiWord, SVG) fails with an error | `DOCLING_RS_MAX_XML_DEPTH` |
| HTML / EPUB DOM walk | 2000 levels, checked iteratively | the document is emitted as flattened text instead of overflowing the recursion stack — without the item tree and the source blocks, which the conversion says (#912): a `Notice` on the document naming the variable and the effective cap, and a `partial_success` carrying the same message (the CLI's warning, serve's `X-Docling-Status` / `X-Docling-Errors`) | `DOCLING_RS_MAX_HTML_DEPTH` |
| Spreadsheet sheet size (calamine allocates the dense used area) | 10 000 000 cells in a sheet's used area — a 4 KB file with values in `A1` and `XFD1048576` asked for 17 billion | the sheet is skipped with a warning; the other sheets convert | `DOCLING_RS_SHEET_MAX_CELLS` |
| Remote VLM page request (`--pipeline vlm`) | 600 s per page request (connect: 10 s) | the page fails with an error naming the cap; a timeout is not retried | `DOCLING_RS_VLM_TIMEOUT` |
| Audio sample rate (ASR) | header-declared rate clamped to 8 kHz–768 kHz, so the resampler can't be steered into an OOM-sized upsample | clamped | — |
| Encrypted Office documents (#625) | the Agile spin count is capped at the spec's 10 000 000, key/salt/block/hash sizes are checked against the spec before any hashing or allocation, and the declared plaintext size of an `EncryptedPackage` may not exceed its ciphertext (the stream itself is under the part budget above) | the document fails with "encryption header is damaged" naming the field | — |
| TableFormer matching | `median()` guards the empty slice (a crafted table row/column with zero matched cells no longer panics) | — | — |
| CUDA arena per ONNX session (#842) | unbounded by default (ONNX Runtime's); every session shrinks its arena back after each run, and under a CUDA-class provider the page-worker pool and the OCR lanes default to one session each — a 16 GB T4 serving mixed PDFs for a day otherwise filled up and every conversion failed inside ONNX Runtime | with a limit set, a run that needs more fails with ONNX Runtime's allocation error for that document; the server stays up | `DOCLING_RS_CUDA_MEM_LIMIT_MB` (MiB per session), `DOCLING_RS_CUDA_ARENA_EXTEND=same_as_requested`, `DOCLING_RS_CUDA_ARENA_SHRINK=0` (keep the arena), `DOCLING_RS_CUDA_CONV_ALGO_SEARCH`, `DOCLING_RS_CUDA_CONV_MAX_WORKSPACE`, `DOCLING_RS_CUDA_DEVICE` |

`docling-serve` adds request-level bounds:

| Guard (default) | On exceed | Override |
|-----------------|-----------|----------|
| Request body: 256 MiB (multipart uploads and JSON bodies alike) | **413** before anything converts | `--max-body-mb` |
| URL fetch response: 256 MiB | the request fails (`exceeds N bytes`) | `DOCLING_RS_MAX_FETCH_BYTES` |
| `to=images` rasterization: 100 pages per request | **400** naming the variable (narrow with `pages=A-B`) | `DOCLING_RS_MAX_RASTER_PAGES` |
| Memory admission control: the container's cgroup limit, else none; new conversions stop at 85 % of it | **503** + `Retry-After` while the process RSS is above the watermark; in-flight conversions keep running | `--max-memory-mb` / `DOCLING_RS_MAX_MEMORY_MB` (`0` disables), `DOCLING_RS_MEMORY_WATERMARK_PCT` |
| Async jobs: 16 queued or unfetched at once, results kept 600 s | **429** | `--queue-size`, `--result-ttl` |
| Idle unload (#842): off by default — the warm PDF/image pipeline (its ONNX sessions and, on a GPU, their device memory) is dropped after N seconds without a PDF/image conversion and reloaded by the next one; `/ready` reports `models: lazy` meanwhile | — (a lazy reload, ~1 s on a GPU) | `--idle-unload-secs N` / `DOCLING_RS_IDLE_UNLOAD_SECS` (`0` = never) |

What is **not** bounded: there is no global page-count or input-size cap like
Python docling's `convert(max_num_pages=…, max_file_size=…)` (both default to
unlimited there); use `--page-range` to convert a window, and the server's
`--max-body-mb` for uploads. An OOXML package has a per-part cap but no
total or ratio cap across its parts (docling's strict OOXML path caps
512 MiB per member and 2 GiB in total).

### Hyperlinks in the output

A document's hyperlinks reach the Markdown, HTML, LaTeX and Pandoc exports
through `docling_core::hyperlink` (#860, docling-core 2.104's rules): a
destination whose scheme is not `http`, `https`, `ftp`, `ftps`, `sftp`,
`mailto`, `tel` or `file` — `javascript:`, `data:`, `vbscript:`, read the way
a browser reads it (leading control characters and spaces ignored, tabs and
newlines removed anywhere, any case) — is never written as a live link: the
link text stays, the destination is dropped. A live Markdown destination is
percent-escaped (whitespace, `()<>"\` and control characters) so it cannot
end the `[text](…)` early and leave its tail to be read as Markdown or raw
HTML, and an HTML `href` is attribute-escaped. The JSON export keeps the raw
`hyperlink` string, as docling's model does — a consumer rendering the JSON
applies its own policy.

### Hostile inputs

Hostile inputs degrade, they do not abort: block quotes and lists in
Markdown nest at most 100 deep (markdown-it's `maxNesting`, what docling's
Markdown backend runs on) and deeper structure is skipped; a docling-JSON
document's `children` references are each walked once and at most 256
levels deep, so a cycle or a fan-out graph converts like the tree it
pretends to be; an HTML page nested past `DOCLING_RS_MAX_HTML_DEPTH` is
recognized by a linear tag scan and emitted as text without ever building a
DOM (html5ever's tree builder is quadratic in nesting — 100 000 nested `<div>`
took 37 s); an ODS sheet's regions are found by visiting its cells, not the
box they span; OOXML parts inflate to at most `DOCLING_RS_MAX_PART_BYTES`;
a redirect on a `--fetch-images` download is checked against the
private-address block-list at every hop; and a PDF page whose declared size
would rasterize past `DOCLING_RS_MAX_RENDER_PIXELS` (15000 px/side, ~5000 pt —
above any real page, A0 at the pipeline's 3x supersample is ~10110 px) is
rejected before the renderer or the `image` crate tries to allocate the
multi-gigabyte bitmap, which a few-hundred-byte crafted `MediaBox` otherwise
forces (the `image` crate *panics* rather than erroring when that allocation
fails).

### Downloaded assets

The models, pdfium and the s390x ONNX Runtime come from this repository's
`models-v*` GitHub release, and two of them are shared libraries the process
loads. Every downloader (`scripts/install/download_dependencies.sh` / `.bat`,
the Python `download_models()`) verifies each asset it fetches from the
release against the release's `SHA256SUMS` before moving it into place; a
mismatch deletes the file and fails the run — never a silent fallback to an
upstream host. `DOCLING_RS_MODELS_VERIFY=require` (default `auto`, which
only warns about a release or asset without a digest) makes a missing
manifest fatal too, for a build that must not fetch anything unverified. The
manifest travels over the same TLS connection as the assets: it catches a
corrupted or truncated transfer, a tampered mirror behind
`DOCLING_RS_MODELS_URL` and a swapped release asset, not a peer able to
forge the certificate of both (that is the TLS stack's job: the bundled
Mozilla roots in `webpki-roots`, the distrusted-key list in `rustls-webpki`,
both pinned in `Cargo.lock` and bumped on a root-program incident). The `docling.rs-cuda` npm package
verifies its binaries the same way (`manifest.json` of its release).

### External programs

Some inputs and options run a program the converter does not ship, on
content derived from the untrusted document:

- **`ffmpeg`** — audio and video inputs. Audio decodes in-process
  (symphonia) first; what symphonia cannot read (Ogg Opus, AVI, …) is piped
  through `ffmpeg` (input on stdin, PCM on stdout). Video frames are always
  extracted by `ffmpeg`, from a temporary copy of the file.
  `DOCLING_FFMPEG` names the binary, else `ffmpeg` on `PATH`; without it
  those audio files fail and video converts as a transcript only.
- **`tesseract`** — only with `--ocr-engine tesseract` (`DOCLING_RS_OCR_ENGINE`):
  one process per page crop. `DOCLING_TESSERACT` names the binary,
  `DOCLING_RS_TESSDATA_DIR` its language data.
- **Chromium** — only with the `web-browser` Cargo feature and
  `--use-web-browser`, **unsandboxed** (see the threat model).
  `DOCLING_RS_CHROME` names the binary.

These calls carry **no timeout**: a decoder that hangs on a crafted file
blocks that conversion (and, in `docling-serve`, holds one of its
`--concurrency` slots) until the process is killed, and the PCM `ffmpeg`
writes for a long audio track is buffered in memory without a cap. The trust
assumption is the same as for Chromium: they run with the converter's
privileges, on input you accepted. Pin the binary paths to a directory the
service user cannot write, and when converting untrusted media at scale run
the converter (or the whole server) under an external CPU-time / memory
limit — a container cgroup or `systemd` `RuntimeMaxSec` / `MemoryMax`.

### Secrets

- `DOCLING_RS_VLM_API_KEY` is sent as a bearer token to the VLM endpoint —
  the operator's `DOCLING_RS_VLM_ENDPOINT`, or a request-supplied
  `vlm_endpoint` / `picture_description_endpoint` (behind `--allow-url-fetch`). A caller who may choose the
  endpoint therefore receives the server's key: pin the endpoint
  server-side when a key is set.
- `DOCLING_SERVE_API_KEY` (or `--api-key`) is docling-serve's own access key
  (below). Prefer the variable: a command-line flag shows up in the process
  list.
- `DOCLING_RS_MCP_TOKEN` (or `--bearer-token`) is the bearer token
  `docling-rs mcp --http` requires (below) — the variable for the same
  reason.
- A document password (`password`; CLI `--password`) is only handed to
  the decryption — never logged or echoed in an error. On a shared machine
  give the CLI `--password-file PATH` instead: `--password` shows up in the
  process list. Office documents also get their format's **published
  default password** tried after the given one (#625): PowerPoint's
  `/01Hannes Ruescher/01` and Excel's `VelvetSweatshop` — the passwords
  Office itself uses for files protected only against editing, which it
  opens without prompting. A file "protected" that way is therefore
  converted without a password, exactly as Office would show it; this
  includes such files sent as attachments.

XML safety (verified, no change needed): the DOM parser (`roxmltree`) never
resolves external entities (no XXE) and caps entity-reference depth/count (no
"billion laughs"); OOXML parts are read **into memory by name**, never
extracted to disk, so there is no zip-slip path.

### `docling-serve` (HTTP conversion API)

- **URL fetch is off by default** (`--allow-url-fetch` to enable). When
  enabled, the target host is resolved and refused if any address it maps to
  is not globally routable — loopback, private, link-local, unique-local,
  CGNAT, multicast, reserved, `0.0.0.0/8`, benchmarking `198.18/15`, the
  documentation and IETF ranges, NAT64 `64:ff9b::/96` and 6to4 (docling-core's
  `is_global` rule; blocks `169.254.169.254` cloud metadata and internal
  services). The check is enforced **at connect time** (#752): the connection
  goes only to the addresses validated in the same resolution, so a
  DNS-rebinding server cannot pass the check with a public answer and then
  point the connect at `127.0.0.1`. Behind an HTTP proxy (`HTTPS_PROXY`…) the
  proxy resolves the target, so a proxied request relies on the pre-flight
  check alone, as docling's does. HTTP redirects
  are disabled (no public→internal bounce); and the fetched response is size-
  capped (256 MiB, `DOCLING_RS_MAX_FETCH_BYTES`). For local development,
  `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1` disables the IP block-list so a
  `localhost` target can be fetched — leave it unset in production.
- A crafted PDF/image that panics inside the pipeline no longer **poisons**
  the shared mutex — the lock recovers, so one bad document can't turn into a
  permanent outage of the endpoint.
- **Image sources (`image_sources`, #646; `fetch_images` is its `remote`
  alias)** — which `<img src>` / `![…](…)` / `cid:` references the
  declarative backends resolve. `embedded` (`data:` URIs and parts of the
  same container — EPUB/MHTML entries, email `cid:` attachments) touches
  neither the filesystem nor the network and is the tier to run on untrusted
  input. `local` reads files only **under the source file's directory**: a
  relative path is canonicalized (so `..` and symlinks resolve) and must
  still be inside; absolute paths and `file://` are never read (a document
  naming `/etc/hosts` gets a placeholder). `remote` is outbound fetch, so it
  lives behind the **same `--allow-url-fetch` gate** as URL inputs: with the
  flag off (the default) the request is held to `embedded` (pictures that
  need the network stay placeholders) and the web UI greys the checkbox; and
  `local` is refused (400) unless the operator started the server with
  `--allow-local-images`. A remote fetch carries the same SSRF guard,
  enforced at connect time — a host resolving to any non-global address is
  skipped — plus an
  optional **host allow-list** (`image_hosts`: exact names or `*.suffix`)
  that every redirect hop is held to, a connect/overall timeout (5 s / 20 s)
  and a redirect cap, so one slow or hostile image can't hang the conversion
  (and thus the server). `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1` opts out of the
  IP block-list for local/intranet image servers, same as the URL fetch above;
  a host on the allow-list does **not** lift it.
- **`pipeline=vlm` with a request-supplied `vlm_endpoint`** (#304) — and
  `do_picture_description` with a request-supplied
  `picture_description_endpoint` (#670), which sends every picture of the
  document the same way — points the server's outbound traffic (page images
  included) wherever the caller says, so it sits behind the **same
  `--allow-url-fetch` gate** and the same resolve-and-refuse
  private-address check as URL inputs — as a pre-flight check only: the VLM
  client does not pin the connection to the validated addresses, so a
  rebinding DNS answer for a caller-supplied endpoint is not caught at
  connect time. The safer
  deployment is the operator-pinned mode: set `DOCLING_RS_VLM_ENDPOINT` /
  `DOCLING_RS_VLM_MODEL` on the server and let requests send only
  `pipeline=vlm` — the pinned endpoint is the operator's own choice, so it
  may be local (e.g. an Ollama on loopback) and needs no gate.
- **Authentication is optional and off by default.** With
  `DOCLING_SERVE_API_KEY` / `--api-key` set (#615, docling-serve's own
  scheme), every `/v1` and `/v1alpha` route requires a matching `X-Api-Key`
  header (401 otherwise); `/health`, `/ready`, `/metrics` and the docs page
  stay open. Without a key, bind to loopback (the default) or place an
  authenticating/policy proxy in front before exposing it.
- **The web UI can be left out.** `--no-ui` / `DOCLING_RS_ENABLE_UI=0`
  (#892; upstream's `DOCLING_SERVE_ENABLE_UI=0` too) serves no page at `/`
  — a server only machines call then shows a browser nothing but a 404
  naming the API. It removes no API surface: the page only calls `/v1`.

### `docling-rs mcp` (Model Context Protocol server, #669)

The MCP server converts documents **on behalf of a model**, whose tool calls
may be steered by the documents it reads (prompt injection) — so what a call
can reach is fixed by the operator's flags, not by the call. The same rules
hold however it is started: `docling-rs mcp`, the PyPI launcher
`uvx docling-rs-mcp` (the same Rust server, in the `docling-rs` wheel) or
the container image.

- **Local reads are confined to `--root DIR`** (repeatable; the working
  directory by default). A defaulted root that is the file-system root —
  where desktop clients often start servers — is refused: the server would
  otherwise read everything, so `/` takes an explicit `--root /`. A path
  is resolved against the first root, folded lexically (`..`) and refused if it leaves every root — before the file
  system is asked, so a probe learns nothing about what exists outside — and
  checked again after `canonicalize`, so a symlink inside a root cannot lead
  out. Options that name files (`xbrl_taxonomy`, a copybook path in
  `ebcdic_layout`) are held to the same roots; `asr_model` must be a preset
  name.
- **URLs are off by default** (`--allow-url-fetch`). Enabled, a fetch goes
  through `docling::fetch`: the docling-serve block-list (every non-global
  address, enforced at connect time) applied to **every redirect hop**, which
  are followed by hand (at most 5), the 256 MiB `DOCLING_RS_MAX_FETCH_BYTES`
  cap, and 10 s connect / 300 s overall timeouts.
  `DOCLING_RS_ALLOW_PRIVATE_IP_FETCH=1` lifts the block-list, as above.
  Remote images (`image_sources=remote` / `fetch_images`), a request-supplied
  `vlm_endpoint` and the headless browser are behind the same flag, with
  docling-serve's rules (remote images fall back to `embedded`).
- **`--http` binds loopback** (127.0.0.1:3001) and refuses any other
  address without `--allow-remote`. On loopback, rmcp's `Host` check stops
  DNS rebinding and an `Origin` allow-list (localhost only; requests
  without an `Origin` — every non-browser client — pass) stops a web page in
  the user's browser from driving the server. A bearer token
  (`--bearer-token` / `DOCLING_RS_MCP_TOKEN`) is compared in constant time;
  without one, a remote bind prints a warning: whoever reaches the port can
  read the files under the root.
- stdio is the JSON-RPC wire: nothing in the conversion path writes to
  stdout (diagnostics go to stderr), and child processes (ffmpeg, Tesseract)
  run with captured output.

### `docling-rag` (retrieval API)

Fail-closed API-key auth (`X-Api-Key` / `Bearer` / `?api_key=` for browser
links); all vector-store SQL is parameterized (no injection); uploaded
filenames are reduced to a single path segment and the markdown-dump path
keeps only `Normal` components (no traversal); served markdown is
`text/markdown` (not rendered HTML) and the UI escapes all interpolated
fields.

## Known residual

No open vulnerability advisory in either lockfile at this writing: the
`quick-xml` (RUSTSEC-2026-0194 / -0195) and `zip` 8.x
(RUSTSEC-2026-0367 / -0368 / -0369) residuals of earlier releases are gone —
`calamine` 0.37 moved to `quick-xml` 0.41 and `zip` 9, and the last
`quick-xml` 0.38 came through `object_store` 0.12, bumped to 0.14 (#952).

One *maintenance* warning (`INFO unmaintained`, not a vulnerability) remains
and is tracked, not suppressed (#952):

- **`paste` 1.0.15 (RUSTSEC-2024-0436).** A build-time proc-macro, reached
  only through `tokenizers` 0.20 (the `chunking` feature's token counter) —
  the OpenEXR / AVIF codecs that also pulled it in through `image`'s default
  features were dropped (#952; nothing here decodes either). It runs at
  compile time only and never sees input; `tokenizers` 1.0 (release
  candidates at this writing) drops it, and the bump follows its stable
  release.

`ttf-parser` (RUSTSEC-2026-0192, unmaintained since June 2026) left the tree
with #952 too: the page renderer's font programs — embedded TrueType /
OpenType and bare CFF (Type1C) — are read by `skrifa` / `read-fonts`
(Google Fonts' `fontations`, the advisory's recommendation), which is
maintained and fuzzed; it is reachable from a crafted PDF, like every parser
here, and bounded the same way (`DOCLING_RS_MAX_RENDER_PIXELS`, the
per-document timeouts).

`instant` and `rustls-pemfile` (unmaintained) left the tree with
`lapin` 2 → 4 (#952), whose rustls is pinned to `ring` so no `aws-lc-sys`
C build enters it.

## Running the audit

```sh
cargo audit         # known-CVE scan of the dependency tree
```

CI runs the same scan weekly over both committed lockfiles (the `audit` job in
`.github/workflows/deps-update.yml`; an accepted residual advisory goes into
its `AUDIT_IGNORES` *and* the "Known residual" section above — keep the two
in sync; the list is empty at this writing). A new finding fails
that job — the red run is the alarm — and triggers `audit-fix`, which runs
`cargo audit fix` on both lockfiles and opens a `fix(deps):` PR with the
semver-compatible bump when one exists; an advisory that needs a new major
version is reported in the job log and stays a manual change.
