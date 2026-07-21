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
  malicious `models/*.onnx` or `.pdfium/lib` that the loader would pick up —
  run with a fixed, non-writable CWD and absolute `DOCLING_*` model paths).
  The `web-browser` feature runs system Chromium **unsandboxed** on the input
  HTML and is off by default; enable it only for content you accept that risk
  for.

## Hardening in place

Resource limits that turn a crafted document from a process abort into a
recoverable error (all overridable by env var for the rare legitimate
outlier):

| Surface | Guard | Override |
|---------|-------|----------|
| Standalone image decode (`convert_image`, METS) | ONNX-free decode with `image::Limits` — 256 MiB alloc / 30000-px per side. A few-KB image declaring 60000×60000 no longer allocates ~10 GB. | `DOCLING_RS_MAX_IMAGE_PIXELS` |
| OOXML/ZIP part inflation (DOCX/PPTX/XLSX/EPUB) | Per-part decompression capped (512 MiB); an oversized "zip bomb" part is rejected, not truncated. | `DOCLING_RS_MAX_PART_BYTES` |
| HTML/EPUB DOM walk | Nesting-depth ceiling (2000), checked iteratively; a document with tens of thousands of nested tags falls back to flattened text instead of overflowing the recursion stack. | `DOCLING_RS_MAX_HTML_DEPTH` |
| Audio sample rate (ASR) | Header-declared rate clamped to 8 kHz–768 kHz, so the resampler can't be steered into an OOM-sized upsample. | — |
| TableFormer matching | `median()` guards the empty slice (a crafted table row/column with zero matched cells no longer panics). | — |

XML safety (verified, no change needed): the DOM parser (`roxmltree`) never
resolves external entities (no XXE) and caps entity-reference depth/count (no
"billion laughs"); OOXML parts are read **into memory by name**, never
extracted to disk, so there is no zip-slip path.

### `docling-serve` (HTTP conversion API)

- **URL fetch is off by default** (`--allow-url-fetch` to enable). When
  enabled, the target host is resolved and refused if it maps to a
  loopback / private / link-local / unique-local / CGNAT address (blocks
  `169.254.169.254` cloud metadata and internal services); HTTP redirects
  are disabled (no public→internal bounce); and the fetched response is size-
  capped (256 MiB).
- A crafted PDF/image that panics inside the pipeline no longer **poisons**
  the shared mutex — the lock recovers, so one bad document can't turn into a
  permanent outage of the endpoint.
- **No authentication.** Bind to loopback (the default) or place an
  authenticating/policy proxy in front before exposing it.

### `docling-rag` (retrieval API)

Fail-closed API-key auth (`X-Api-Key` / `Bearer` / `?api_key=` for browser
links); all vector-store SQL is parameterized (no injection); uploaded
filenames are reduced to a single path segment and the markdown-dump path
keeps only `Normal` components (no traversal); served markdown is
`text/markdown` (not rendered HTML) and the UI escapes all interpolated
fields.

## Known residual

- **`quick-xml` DoS advisories (RUSTSEC-2026-0194 / -0195) via `calamine`.**
  Our direct XML parsing is on `quick-xml` ≥ 0.41 (patched). The XLSX reader
  `calamine` still pins `quick-xml` 0.31 transitively; until `calamine`
  upstream bumps, a crafted **XLSX** can still trigger the quadratic-attribute
  / namespace-allocation DoS. Impact is bounded by the process/deployment
  (single-document DoS, not RCE); mitigate by not exposing XLSX conversion to
  untrusted callers without a per-request resource bound, or run conversions
  in a resource-limited sandbox.

## Running the audit

```sh
cargo audit         # known-CVE scan of the dependency tree
```
