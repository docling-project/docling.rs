# docling.rs on Windows (native)

The pipeline runs natively on Windows — no WSL required, and usually faster
than under it: ONNX Runtime's prebuilt Windows binaries (pulled automatically
by the `ort` crate at build time) use the same AVX2/VNNI kernels, without the
VM/filesystem overhead.

## Prerequisites

- **Rust (MSVC toolchain)** — `rustup default stable-x86_64-pc-windows-msvc`
  plus the *Visual Studio Build Tools* with the "Desktop development with C++"
  workload (the linker).
- **Windows 10 1803+** — `curl.exe` and `tar.exe` ship with the OS; the
  download script needs nothing else.

## Build & fetch the ML dependencies

```bat
git clone https://github.com/docling-project/docling.rs
cd docling.rs

scripts\install\download_dependencies.bat   :: models\ + .pdfium\lib\pdfium.dll
cargo build --release -p docling-cli
```

The script mirrors `download_dependencies.sh`: layout / OCR / TableFormer /
picture-classifier ONNX models (INT8 variants included — preferred
automatically; set `DOCLING_RS_FP32=1` to opt out), Whisper-tiny for audio
(`--no-asr` to skip), the chunker tokenizer, and `pdfium.dll` (fetched from
[pdfium-binaries](https://github.com/bblanchon/pdfium-binaries), which the
Linux-only asset in our models release doesn't cover).

## Run

Everything resolves relative to the current directory (or next to the
executable), so from the repo root no environment variables are needed:

```bat
target\release\docling-rs paper.pdf
target\release\docling-rs --to json report.docx
```

From another directory, point the two roots at the checkout — `cmd`:

```bat
set PDFIUM_DYNAMIC_LIB_PATH=C:\src\docling.rs\.pdfium\lib
set DOCLING_LAYOUT_ONNX=C:\src\docling.rs\models\layout_heron_int8.onnx
```

or PowerShell:

```powershell
$env:PDFIUM_DYNAMIC_LIB_PATH = "C:\src\docling.rs\.pdfium\lib"
```

Every `DOCLING_*` / `DOCLING_RS_*` variable from the main README works the
same way. The HTTP server is the same crate as on Linux:

```bat
cargo run --release -p docling-serve            :: 127.0.0.1:5001, docs at /
```

## Notes & limits

- **Model export / quantization / conformance scripts are bash** — run those
  under WSL or Git Bash; the prebuilt models from the release make this
  unnecessary for normal use.
- **Audio** decoding is pure Rust (symphonia), no ffmpeg needed on Windows
  either.
- The optional `--features web-browser` pre-render needs a Chromium install
  and is untested on Windows.
- Benchmark tip: `target\release\docling-rs --bench-warm 3 paper.pdf` prints
  warm seconds/conversion — handy for comparing against a WSL build of the
  same checkout.
