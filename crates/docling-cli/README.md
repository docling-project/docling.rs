# docling-rs

The command-line interface of [docling.rs](https://github.com/docling-project/docling.rs)
— a Rust port of IBM's [docling](https://github.com/docling-project/docling).
One static binary converts PDF, Office, HTML, scans, audio and 40-odd other
formats to Markdown, docling JSON, HTML, text, DocLang, LaTeX, Pandoc AST,
WebVTT or RAG chunks; `docling-rs serve` is the HTTP API and
`docling-rs mcp` a Model Context Protocol server for AI agents.

```bash
curl -fsSL https://raw.githubusercontent.com/docling-project/docling.rs/master/scripts/install/install.sh | bash
# or: cargo install docling-cli   (models: scripts/install/download_dependencies.sh)
```

## Usage

```bash
docling-rs report.pdf > report.md                # Markdown, streamed page by page (--no-stream to buffer)
docling-rs --strict report.docx                  # the cleaner Markdown dialect
docling-rs report.pdf --to json                  # docling JSON; also html, text, dclx, latex, pandoc, typst, vtt, chunks, images
docling-rs --images referenced report.pdf > out.md   # pictures to ./artifacts/*.png (embedded = data: URIs)
docling-rs recording.mp3                         # Whisper transcript; --asr-model whisper_tiny_en for a preset
cat report.docx | docling-rs --from docx -       # stdin (#668)
docling-rs https://example.com/whitepaper.pdf    # a URL; --mime-type when it has no extension
docling-rs "data:text/html;base64,PGgxPkhpPC9oMT4="
docling-rs --progress report.pdf > report.md     # progress bars on stderr (#783; the default on a terminal, --no-progress hides them)
docling-rs --help                                # every flag; --version lists the compiled-in features
docling-rs --list-input-formats                  # what this build converts, one per line
```

Every option — OCR engine and language, page ranges, image handling,
enrichment, PII redaction, the VLM pipeline — is in
[`docs/OPTIONS.md`](../../docs/OPTIONS.md); the library guide
([`crates/docling`](../docling/README.md)) explains what each does.

## Batch conversion — several sources, `--input` / `--output`

One warm process converts many documents (#205, #489): any number of
positional sources — files, directories, quoted globs, ZIP archives — or
`--input GLOB|DIR`, into `--output DIR`. The ML models load once; a failing
file is reported and skipped (non-zero exit at the end, `--abort-on-error`
to stop at the first); output paths print to stdout, progress to stderr.

```bash
docling-rs a.docx sub/b.docx other/c.pdf --output ./converted   # a.md, b.md, c.md
docling-rs --to md,json report.pdf --output ./converted          # report.md + report.json
docling-rs --input '/data/reports/**/*.pdf' --output ./out --to json   # tree kept: out/2024/q1/a.json
docling-rs --input /data/reports --output ./out                  # recursive sweep, convertible files only
docling-rs bundle.zip --output out/                              # every document inside: out/bundle/<entry>.md (#557)
docling-rs bundle.zip > bundle.md                                # one document: `## <entry path>` + each entry (#677)
docling-rs bundle.zip --archive-merge --output out/              # the same, as out/bundle.md
```

| Flag | Meaning |
|---|---|
| `--to FMT[,FMT]` (repeatable) | every format for each document; several need `--output` |
| `--output DIR` · `--output-file PATH` | the output directory · one input, one format, exactly that path (#611) |
| `--output-dirs auto\|flat\|mirror` | layout under `--output` (#496): mirror a directory's / glob's tree, land plain files by stem (default); everything flat as `<stem>.<ext>`; everything by its path relative to the CWD |
| `--images referenced` | each document's pictures in a sibling `<stem>_artifacts/` |
| `--jobs N` | declarative formats in parallel (PDFs share the one warm pipeline) |
| `--abort-on-error` | stop at the first failed file (Python's flag) |
| `--list-input-formats` · `--list-output-formats` | what this build converts, one per line (#603) — ask the binary instead of hard-coding a list |
| `-` · `http(s)://…` · `data:…` · `file:…` | stdin, a fetched URL, an inline document, a path through the URL grammar — as sources anywhere a path goes (#668); `--from FORMAT` / `--mime-type TYPE` type what carries no extension, `--no-url-fetch` refuses URLs |

Every other flag (`--strict`, `--pages`, `--ocr-lang`, `--pipeline vlm`,
enrichment, …) applies to the whole batch. Collision rules, the ZIP limits
and outcomes, email attachments (`EmailAttachments`, Python
`email_attachments()`, Node `emailAttachments()`, #561) and the progress /
exit-code details: [`docs/OPTIONS.md` § CLI batch
mode](../../docs/OPTIONS.md#cli-batch-mode).


## MCP server — `docling-rs mcp`

`docling-rs mcp` (#669) is a [Model Context Protocol](https://modelcontextprotocol.io)
server, so agents — Claude Desktop, Claude Code, Cursor, VS Code, any MCP
client — convert documents themselves. It is in the default build: the
release binaries, `cargo install docling-cli` and the `ghcr.io/docling-project/docling-rs`
image all carry it, and `uvx docling-rs-mcp` runs the same server from PyPI
(below). Three tools:

| Tool | What it returns |
|---|---|
| `convert_to_markdown(uri)` | Markdown of the document behind an `http:`/`https:`/`file:`/`data:` URI or a path — **MarkItDown's MCP tool, same name and contract**, so a `markitdown-mcp` setup switches by changing the command |
| `convert(uri, to?, images?, options?)` | `to` = `md` (default), `json` (docling's `DoclingDocument`), `html`, `text`, `latex`, `chunks`, `vtt`, `pandoc`; `options` = the [shared conversion options](../../docs/OPTIONS.md) (`{"pages": "1-5"}`, `{"text_layer_only": true}`, `{"ocr_lang": "ch"}`, `{"password": "…"}`, …; a value may be text — `{"text_layer_only": "true"}`, `{"ocr_scale": "1.5"}` — as MCP Inspector's form sends it) |
| `list_input_formats()` | the extensions this server converts |

Claude Desktop (`claude_desktop_config.json`) or any client that launches
stdio servers:

```json
{
  "mcpServers": {
    "docling": {
      "command": "docling-rs",
      "args": ["mcp", "--root", "/Users/me/Documents"]
    }
  }
}
```

```bash
claude mcp add docling -- docling-rs mcp --root ~/Documents     # Claude Code
```

With only Python and [uv](https://docs.astral.sh/uv/) at hand —
MarkItDown's `uvx markitdown-mcp` route — the PyPI launcher `docling-rs-mcp`
runs the same server, compiled into the `docling-rs` wheel:

```json
{
  "mcpServers": {
    "docling": { "command": "uvx", "args": ["docling-rs-mcp", "--root", "/Users/me/Documents"] }
  }
}
```

`uvx docling-rs-mcp --download-models` fetches the layout, OCR and table
models (~720 MB) into the docling.rs cache once (`=all` adds the code/formula
enrichment models); the launcher finds them there from any directory. With
the `docling-rs` package installed the server is also
`python -m docling_rs.mcp`, and on NVIDIA GPUs
`uv run --with docling-rs-cuda python -m docling_rs.mcp`. PyPI's macOS wheel
is Apple Silicon only, so on an Intel mac uvx compiles the package on first
start (a Rust toolchain is required) — run the `--download-models` line once
before adding the server to a client. Each release published to PyPI is also published to
the official MCP registry as `io.github.docling-project/docling-rs` and gets
the MCP bundle Smithery lists ([`artem-kustikov/docling-rs`](https://smithery.ai/servers/artem-kustikov/docling-rs),
`docling-rs-mcp-<version>.mcpb`) attached; Glama builds the launcher
from the build spec on its admin page and reads `glama.json`; the root
`Dockerfile` is the launcher on a slim Python image
([`docs/MCP_REGISTRIES.md`](../../docs/MCP_REGISTRIES.md)).

The container image has the models baked in; mount the folder at the same
path inside so the paths the agent sees are the paths the server reads:

```json
{
  "mcpServers": {
    "docling": {
      "command": "docker",
      "args": ["run", "-i", "--rm", "-v", "/Users/me/Documents:/Users/me/Documents",
               "ghcr.io/docling-project/docling-rs", "mcp", "--root", "/Users/me/Documents"]
    }
  }
}
```

Streamable HTTP instead of stdio, for clients that connect to a URL:
`docling-rs mcp --http` serves `http://127.0.0.1:3001/mcp` (`--host`,
`--port`; `--bearer-token` or `DOCLING_RS_MCP_TOKEN` to require
`Authorization: Bearer …`).

What a tool call may reach is the operator's decision, not the model's:
local paths and `file:` URIs are confined to `--root` (repeatable; the
working directory by default — unless that is the file-system root, where
desktop clients often start servers: there `--root` is required — checked
before and after resolving symlinks);
`http(s)` URLs — and remote images, a request-supplied `vlm_endpoint` — only
with `--allow-url-fetch`, through the SSRF block-list on every redirect hop
with a 256 MiB / 300 s cap; `--http` refuses a non-loopback bind without
`--allow-remote` and checks `Host`/`Origin` on loopback. Details in
[docs/SECURITY.md](../../docs/SECURITY.md#docling-rs-mcp-model-context-protocol-server-669).
Markdown comes with compact tables (fewer tokens; `"compact_tables": false`
for docling's padded ones). Without the layout models (a bare release
binary — fetch them with `scripts/install/download_dependencies.sh`, or point
`DOCLING_RS_MODELS_DIR` at a `.models` directory in the client's `env`), PDFs
convert from their text layer, as MarkItDown's do, instead of failing.
The PDF/image models load on the first call that needs them and stay loaded
— one warm pipeline per server, as in `docling-serve` — so later calls skip
the load (a 5-page PDF: 5.4 s first, 2.6 s after), and memory stays flat
between calls (~0.6 GB with the models resident; ONNX Runtime's CPU arena is
off and freed buffers go back to the OS — `DOCLING_RS_NO_ARENA=0` restores
the arena). Calls carrying a progress token get a heartbeat every 10 s while
a long PDF converts. Flags take `--flag value` or `--flag=value`
(`--allow-url-fetch=false`), the form catalog configs generate.


## HTTP server

`docling-rs serve` (built with `--features serve`) runs the HTTP conversion
API, the same as the standalone `docling-serve` binary:
[`crates/docling-serve`](../docling-serve/README.md).

## License

MIT, like the rest of docling.rs.
