# Fleischwolf (meat grinder in German, [ˈflaɪ̯ˌʃvɔlf])

```
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢻⣿⣿⣿⣿⣿⣿⣿⣿⠇⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⢿⣿⣿⣿⣿⣿⣿⠏⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠻⣿⣿⣿⡿⠋⣀⣀⣀⣀⣀⣀⢰⣶⡆⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢸⣿⣦⣄⣉⣉⣤⣾⣿⣿⣿⣿⣿⣿⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⢠⣤⣤⠀⡇⢸⣿⣿⣿⣿⣿⣿⣿⣟⣛⣛⣛⣛⡋⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⠈⢉⡉⠀⠇⢸⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠈⢉⣉⡉⠉⠉⠉⠛⠛⠛⠛⠛⠛⠛⢸⣿⡇⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠀⢸⣿⡇⠀⠀⠸⠿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠀⢸⣿⡇⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⢸⡇⠀⠀⠀⢸⣿⡇⠀⠀⢠⣤⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠸⠇⠀⠀⠀⢸⣿⡇⠀⠀⢠⣤⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⣴⣶⣾⣿⣿⣷⣶⣦⠄⠀⠀⠀⠸⣿⣧⣤⣤⣾⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠉⠉⠉⠉⠉⠁⠀⠀⠀⠀⠀⠀⠈⠉⠉⠉⢉⣉⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀
```

A Rust port of [docling](https://github.com/docling-project/docling): convert
documents into a unified `DoclingDocument` for downstream AI workflows.

This is an **early, in-progress** port. See [`MIGRATION.md`](./MIGRATION.md) for
the full architecture, the Python → Rust mapping, and the phased plan.

## Status

The public API works end to end across **Markdown, CSV, HTML, AsciiDoc, DOCX,
PPTX, XLSX, EPUB, ODF, WebVTT, Email, JATS, USPTO, XBRL, LaTeX, JSON, PDF,
images and METS** — plus Markdown / docling-JSON output and image extraction.
The discriminative PDF/image pipeline lives in `fleischwolf-pdf`: a pure-Rust PDF
text parser, pdfium for page rasterization, and an ONNX layout/TableFormer/OCR
stack. Audio/ASR is the main format still on the roadmap (see `MIGRATION.md`).

Output is checked against upstream Python docling — declarative formats
byte-for-byte against live docling, the ML pipeline against a deterministic
snapshot baseline. See [`COMPARING.md`](./COMPARING.md) and
`scripts/conformance.sh`.

## The API

```rust
use fleischwolf::{DocumentConverter, SourceDocument};

let converter = DocumentConverter::new();
let result = converter
    .convert(SourceDocument::from_file("input.md").unwrap())
    .unwrap();

println!("{}", result.document.export_to_markdown()); // Markdown
println!("{}", result.document.export_to_json());     // docling DoclingDocument JSON
```

### JSON output

`export_to_json()` emits docling-core's native `DoclingDocument` wire format
(schema `1.10.0`) — the same shape Python docling's `export_to_dict()` /
`save_as_json()` produce: a `body` tree of `$ref`s into `texts` / `groups` /
`tables` / `pictures`, with labels (`title`, `section_header`, `list_item`,
`code`, `formula`, …), list grouping, and table grids. The output loads straight
back into Python docling-core (`DoclingDocument.load_from_json(...)`) and
round-trips to the same Markdown.

> Note: Fleischwolf's model bakes inline formatting (bold, links, inline math)
> into the text, so for those spans the JSON carries the rendered text rather
> than docling's structured `formatting` / `hyperlink` fields. Block structure,
> headings, lists, tables, code and display equations match.

### Image extraction

Backends that have the image populate `Node::Picture { image }`: the PDF/image
pipeline crops figure regions, the DOCX / PPTX backends pull embedded image
blobs, and — opt-in — the HTML / EPUB backends fetch `<img src>` (see below).
Pick how pictures render with an [`ImageMode`] — the analogue of docling's
`image_mode`:

```rust
use fleischwolf::ImageMode;

// self-contained Markdown: ![Image](data:image/png;base64,…)
let (md, _) = result.document.export_to_markdown_with_images(ImageMode::Embedded, "artifacts");

// referenced: ![Image](artifacts/image_000000.png) + the bytes to write
let (md, files) = result.document.export_to_markdown_with_images(ImageMode::Referenced, "artifacts");
for (path, bytes) in files { std::fs::write(path, bytes).unwrap(); }
```

`export_to_json()` always embeds extracted images as docling `ImageRef`s
(`data:` URIs + size). The default `export_to_markdown()` stays
`<!-- image -->`, like docling.

> The cropped/extracted pixels are real, but the base64 won't be byte-identical
> to docling's (different PNG encoder). HTML/EPUB pictures stay placeholders by
> default (like docling); enable fetching with `--fetch-images` /
> `DocumentConverter::fetch_images(true)` to resolve `<img src>` — `data:` URIs,
> local files, remote `http(s)` URLs, and EPUB archive entries — and embed the
> bytes. Remote URLs are fetched over the network, so enable it only for input
> you trust.

### `strict` Markdown (Rust-only)

By default `export_to_markdown()` reproduces docling's output byte-for-byte,
quirks included (`***x*** .`, dropped code-fence languages, `\_` escaping). Set
`strict(true)` for cleaner, more conformant Markdown:

```rust
let converter = DocumentConverter::new().strict(true);
let result = converter.convert(source).unwrap();
println!("{}", result.document.export_to_markdown()); // ```rust kept, no `***x*** .`, `_` not escaped
```

```text
legacy:  Foo ***both*** .   |   ``` (lang dropped)   |   Name: \_\_\_
strict:  Foo ***both***.    |   ```rust (lang kept)  |   Name: ___
```

`result.document.export_to_markdown_with(strict)` overrides the mode per call.
Python docling has no such switch.

### Streaming Markdown

For embedding in real apps, `convert_streaming` returns the document's Markdown
as an iterator of chunks instead of one big string — handy for piping a long
document straight to stdout, an HTTP response, or a socket as it is produced:

```rust
use std::io::Write;
use fleischwolf::{DocumentConverter, SourceDocument};

let source = SourceDocument::from_file("input.pdf").unwrap();
let mut out = std::io::stdout();
for chunk in DocumentConverter::new().convert_streaming(source).unwrap() {
    out.write_all(chunk.unwrap().as_bytes()).unwrap();
}
```

The headline win is PDF. The ML pipeline already processes pages **in parallel**;
streaming emits each page's Markdown **in document order, as soon as it is ready**
(with a one-page look-ahead so paragraphs that wrap across a page break still
merge), so output starts flowing before the last page is done. The conversion
runs on a background thread and the chunk iterator applies backpressure; dropping
it cancels the work. Concatenating every chunk is **byte-identical** to the
buffered `export_to_markdown()`.

Streaming is Markdown-only — JSON serializes docling-core's reference-based tree
and needs every node up front. Picture placeholders and `embedded` data-URI
images stream; the `referenced` mode writes sidecar files, so it stays on the
buffered `export_to_markdown_with_images` path. Use
`convert_streaming_images(source, ImageMode::Embedded)` to pick the image mode.

The CLI streams Markdown by default (`--no-stream` opts back into buffering;
`--to json` and `--images referenced` always buffer).

## Testing

All commands run from the `fleischwolf/` workspace root.

```bash
# everything — unit tests + the output-regression suite (pure Rust; no Python/models)
cargo test

# just the regression suite: re-convert every source under
# crates/fleischwolf/tests/data/<fmt>/sources/ and assert that legacy Markdown,
# strict Markdown and docling JSON match the committed fixtures (catches drift)
cargo test -p fleischwolf --test regression

# refresh the fixtures after an *intentional* output change, then review `git diff`
FLEISCHWOLF_REGEN=1 cargo test -p fleischwolf --test regression

# a single crate / a single test (with output)
cargo test -p fleischwolf-core
cargo test outputs_match_fixtures -- --nocapture
```

The ML formats (PDF, images, METS) need pdfium + the ONNX models, so they are
covered by a separate **deterministic snapshot** harness rather than `cargo test`:

```bash
bash scripts/pdf_setup.sh           # one-time: fetch pdfium + export the ONNX models
                                    # (layout + TableFormer; needs a torch/docling Python)
# Updating an existing checkout after a model-format change (e.g. the cached
# TableFormer decoder): `rm -rf models/tableformer && bash scripts/pdf_setup.sh`,
# or re-run `python scripts/export_tableformer.py models/tableformer` directly.

export PDFIUM_DYNAMIC_LIB_PATH="$(pwd)/.pdfium/lib"
export DOCLING_LAYOUT_ONNX="$(pwd)/models/layout_heron.onnx"
export DOCLING_OCR_REC_ONNX="$(pwd)/models/ocr_rec.onnx"
export DOCLING_OCR_DICT="$(pwd)/models/ppocr_keys_v1.txt"
bash scripts/pdf_conformance.sh     # regenerate + diff the snapshot baseline (91/91)
```

## Try it

```bash
# convert a file from the CLI — Markdown to stdout (add --strict for cleaner MD)
cargo run -p fleischwolf-cli -- crates/fleischwolf/sample.html
cargo run -p fleischwolf-cli -- --strict crates/fleischwolf/sample.html

# emit docling's native DoclingDocument JSON instead (--to md is the default)
cargo run -p fleischwolf-cli -- --to json crates/fleischwolf/sample.html
cargo run -p fleischwolf-cli -- --to json crates/fleischwolf/sample.html > out.json

# extract pictures (PDF/image inputs): embed as data URIs, or write ./artifacts/*.png
cargo run -p fleischwolf-cli -- --images embedded   document.pdf
cargo run -p fleischwolf-cli -- --images referenced document.pdf > out.md

# stream Markdown to stdout page by page (the CLI's default; --no-stream to buffer)
cargo run -p fleischwolf-cli -- document.pdf
cargo run -p fleischwolf-cli -- --no-stream document.pdf

# or via the examples
cargo run -p fleischwolf --example convert -- crates/fleischwolf/sample.md
cargo run -p fleischwolf --example stream  -- crates/fleischwolf/sample.md

# score HTML output against the latest published docling (installed from PyPI)
scripts/conformance.sh html

# diff Python docling vs Rust on one file (installs published docling from PyPI)
scripts/compare.sh tests/data/html/sources/example_03.html

# benchmark time / CPU / memory: Python docling vs Rust
scripts/performance.sh tests/data/html/sources/wiki_duck.html 10
```

The comparison scripts install the latest published Python `docling` from PyPI
into `.venv-compare` automatically on first run. See
[`COMPARING.md`](./COMPARING.md).

## Deploy in a container

For a real-world service, bake the binary, native libs, and models into one image
so the runtime needs no Python. [`examples/Dockerfile`](./examples/Dockerfile) is a
3-stage build that does exactly this — a `models` stage exports the layout +
**TableFormer** (KV-cached decoder) ONNX with torch and fetches the OCR model +
pdfium, a `builder` stage compiles the CLI, and a slim `runtime` stage carries just
the binary, `libonnxruntime`, pdfium, and the models, with the `DOCLING_*` env vars
preset:

```bash
docker build -f examples/Dockerfile -t fleischwolf .
docker run --rm -v "$PWD:/data" fleischwolf /data/input.pdf          # Markdown to stdout
docker run --rm -v "$PWD:/data" fleischwolf /data/input.pdf --to json
```

The image converts PDFs/images fully offline; the model export (torch +
`docling-ibm-models`) happens only at build time, never at runtime.

## Performance

`scripts/performance.sh` runs the **largest fixture of each supported type** through
both engines (published Python `docling` vs the Rust release binary) and reports
peak RSS, CPU utilization, and conversion time. Ratios below are docling ÷
fleischwolf — bigger means Rust wins by more.

| File | Size | Peak-memory ratio | CPU ratio | Warm-conversion speedup |
|---|---:|---:|---:|---:|
| `2203.01017v2.pdf` (PDF, 47 pp) | 6.9 MB | **2.2× less** | 1.3× | 1.2× |
| `docx_rich_tables_01.docx` (DOCX) | 3.1 MB | **41× less** | 2.7× | 21× |
| `wiki_duck.html` (HTML) | 240 KB | **57× less** | 3.2× | 46× |
| `elife-56337.nxml` (JATS XML) | 180 KB | **61× less** | 2.9× | 10× |
| `xlsx_04_inflated.xlsx` (XLSX) | 168 KB | **59× less** | 2.9× | 12× |
| `powerpoint_with_image.pptx` (PPTX) | 80 KB | **57× less** | 2.8× | 4.4× |
| `wiki.md` (Markdown) | 8 KB | **58× less** | 2.9× | 1.3× |
| `csv-comma.csv` (CSV) | 4 KB | **66× less** | 2.9× | 0.6× † |

- **Peak memory** is where Rust wins decisively: a declarative conversion holds a
  few MB versus docling's ~750 MB (it imports torch even for non-ML formats). The
  PDF runs the full ML pipeline in both engines (torch vs ONNX), so the gap there
  is 2.2× rather than 50×+, but Rust still peaks at 1.4 GB vs docling's 3.1 GB.
- **CPU**: docling spreads across 2.7–3.2 cores for declarative work that Rust does
  on a single core (~100%); on the PDF both go multi-core (Rust 525% vs docling
  674%).
- **Warm-conversion speedup** isolates the parse/convert work — it times docling
  *in-process* (excluding its ~3 s interpreter + import startup) against the Rust
  whole-process figure. Rust wins on substantial inputs (HTML 46×, DOCX 21×); the
  end-to-end figure, which re-pays docling's startup every invocation, is **377–
  1190× faster** for the declarative formats.
- † For trivial inputs (a 4 KB CSV) the conversion itself is microseconds, so Rust's
  own process startup dominates its number while warm-Python excludes startup — the
  warm metric understates Rust there. End-to-end, the CSV is **1190× faster** in Rust.

## Layout

| Crate | Role | Python analogue |
|---|---|---|
| `fleischwolf-core` | `DoclingDocument` model + serializers | `docling-core` |
| `fleischwolf` | `DocumentConverter`, source loading, backends | `docling` |
| `fleischwolf-cli` | command-line interface | `docling.cli` |

## License

MIT, matching upstream docling.
