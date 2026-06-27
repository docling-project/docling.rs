# docling-crab 🦀

A Rust port of [docling](https://github.com/docling-project/docling): convert
documents into a unified `DoclingDocument` for downstream AI workflows.

This is an **early, in-progress** port. See [`MIGRATION.md`](./MIGRATION.md) for
the full architecture, the Python → Rust mapping, and the phased plan.

## Status

The public API works end to end across **Markdown, CSV, HTML, AsciiDoc, DOCX,
PPTX, XLSX, EPUB, ODF, WebVTT, Email, JATS, USPTO, XBRL, LaTeX, JSON, PDF,
images and METS** — plus Markdown / docling-JSON output and image extraction.
The discriminative PDF/image pipeline (pdfium + ONNX layout/OCR) lives in
`docling-crab-pdf`. Audio/ASR is the main format still on the roadmap (see
`MIGRATION.md`).

Output is checked against upstream Python docling — declarative formats
byte-for-byte against live docling, the ML pipeline against a deterministic
snapshot baseline. See [`COMPARING.md`](./COMPARING.md) and
`scripts/conformance.sh`.

## The API

```rust
use docling_crab::{DocumentConverter, SourceDocument};

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

> Note: docling-crab's model bakes inline formatting (bold, links, inline math)
> into the text, so for those spans the JSON carries the rendered text rather
> than docling's structured `formatting` / `hyperlink` fields. Block structure,
> headings, lists, tables, code and display equations match.

### Image extraction

Backends that have the image populate `Node::Picture { image }`: the PDF/image
pipeline crops figure regions, and the DOCX / PPTX backends pull embedded image
blobs. Pick how pictures render with an [`ImageMode`] — the analogue of
docling's `image_mode`:

```rust
use docling_crab::ImageMode;

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
> to docling's (different PNG encoder). HTML/EPUB pictures stay placeholders —
> like docling, external `<img src>` files aren't fetched.

### `strict` Markdown (Rust-only)

By default `export_to_markdown()` reproduces docling's output byte-for-byte,
quirks included (`***x*** .`, dropped code-fence languages, `\_` escaping). Set
`strict(true)` for cleaner, more conformant Markdown:

```rust
let converter = DocumentConverter::new().strict(true);
let result = converter.convert(source).unwrap();
println!("{}", result.document.export_to_markdown()); // ```rust kept, no `***x*** .`
```

```text
legacy:  Foo ***both*** .   |   ```          (language dropped)
strict:  Foo ***both***.    |   ```rust      (language kept)
```

`result.document.export_to_markdown_with(strict)` overrides the mode per call.
Python docling has no such switch.

## Testing

All commands run from the `docling-crab/` workspace root.

```bash
# everything — unit tests + the output-regression suite (pure Rust; no Python/models)
cargo test

# just the regression suite: re-convert every source under
# crates/docling-crab/tests/data/<fmt>/sources/ and assert that legacy Markdown,
# strict Markdown and docling JSON match the committed fixtures (catches drift)
cargo test -p docling-crab --test regression

# refresh the fixtures after an *intentional* output change, then review `git diff`
DOCLING_CRAB_REGEN=1 cargo test -p docling-crab --test regression

# a single crate / a single test (with output)
cargo test -p docling-crab-core
cargo test outputs_match_fixtures -- --nocapture
```

The ML formats (PDF, images, METS) need pdfium + the ONNX models, so they are
covered by a separate **deterministic snapshot** harness rather than `cargo test`:

```bash
bash scripts/pdf_setup.sh           # one-time: fetch pdfium + the ONNX models

export PDFIUM_DYNAMIC_LIB_PATH="$(pwd)/.pdfium/lib"
export DOCLING_LAYOUT_ONNX="$(pwd)/models/layout_heron.onnx"
export DOCLING_OCR_REC_ONNX="$(pwd)/models/ocr_rec.onnx"
export DOCLING_OCR_DICT="$(pwd)/models/ppocr_keys_v1.txt"
bash scripts/pdf_conformance.sh     # regenerate + diff the snapshot baseline (76/76)
```

## Try it

```bash
# convert a file from the CLI — Markdown to stdout (add --strict for cleaner MD)
cargo run -p docling-crab-cli -- crates/docling-crab/sample.html
cargo run -p docling-crab-cli -- --strict crates/docling-crab/sample.html

# emit docling's native DoclingDocument JSON instead (--to md is the default)
cargo run -p docling-crab-cli -- --to json crates/docling-crab/sample.html
cargo run -p docling-crab-cli -- --to json crates/docling-crab/sample.html > out.json

# extract pictures (PDF/image inputs): embed as data URIs, or write ./artifacts/*.png
cargo run -p docling-crab-cli -- --images embedded   document.pdf
cargo run -p docling-crab-cli -- --images referenced document.pdf > out.md

# or via the example
cargo run -p docling-crab --example convert -- crates/docling-crab/sample.md

# score HTML output vs docling's groundtruth (no Python), or vs live docling
scripts/conformance.sh html
scripts/conformance.sh html --live

# diff Python docling vs Rust on one file (loads docling from local sources)
scripts/compare.sh ../tests/data/html/sources/example_03.html

# benchmark time / CPU / memory: Python docling vs Rust
scripts/performance.sh ../tests/data/html/sources/wiki_duck.html 10
```

The comparison scripts load Python `docling` from this repo's own sources (an
editable install in `.venv-compare`, created automatically) — no
`pip install docling` required. See [`COMPARING.md`](./COMPARING.md).

## Layout

| Crate | Role | Python analogue |
|---|---|---|
| `docling-crab-core` | `DoclingDocument` model + serializers | `docling-core` |
| `docling-crab` | `DocumentConverter`, source loading, backends | `docling` |
| `docling-crab-cli` | command-line interface | `docling.cli` |

## License

MIT, matching upstream docling.
