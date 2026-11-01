# IBM Z (s390x) and other targets without a prebuilt ONNX Runtime — `ort-load-dynamic`

Back to the [project README](../README.md) · models: [`MODELS.md`](./MODELS.md).


docling.rs builds for `s390x-unknown-linux-gnu` (#504). The code is
endian-clean — s390x is big-endian, and the docling-core, HTML-export and
pure-Rust PDF renderer/raster suites pass as s390x binaries under
`qemu-user` — so every declarative format and the PDF text layer work out of
the box. What pyke's `ort` cannot provide there is a prebuilt ONNX Runtime
(it ships binaries for x86_64/aarch64 only; upstream ONNX Runtime builds and
runs on s390x — IBM contributes the port — but publishes no s390x release).
The `ort-load-dynamic` cargo feature (CLI, serve, lib, Python, Node, RAG)
therefore `dlopen`s the runtime at the first ML session instead of linking
it:

```bash
# on the mainframe, or cross-compiled with gcc-s390x-linux-gnu as the release workflow does
cargo build --release -p docling-cli --features ort-load-dynamic --target s390x-unknown-linux-gnu
```

The library is looked up as `ORT_DYLIB_PATH` (a path), then
`.models/onnxruntime/libonnxruntime.so` (through the models-dir resolution,
so `DOCLING_RS_MODELS_DIR` covers it), then the dynamic linker's search path
(`LD_LIBRARY_PATH`, a distro package); ONNX Runtime 1.17 or newer, any
build — a `pip install onnxruntime`'s `capi/libonnxruntime.so.1.x.y` works
on x86_64. Without it the ML stages degrade exactly like a missing model:
PDF conversion fails with a message naming the library and the lookup, OCR
and enrichment warn and skip, DOCX/HTML/XLSX/… and `docling-rs --to md` on a
DOCX never touch it.

**The s390x runtime itself** ships in the models release as
`onnxruntime-linux-s390x.tar.gz`: ONNX Runtime 1.31.0 — the version the
linked builds carry since #523, built from upstream's `v1.31.0` tag —
cross-compiled by `.github/workflows/onnxruntime-s390x.yml` with zig against
glibc 2.28, so it loads on RHEL 8/9-era mainframe Linux. Its exact identity
is its digest in the release's `SHA256SUMS` / `models.json` and the
`VERSION`, `GIT_COMMIT_ID` and `BUILD_INFO` files inside it (see "Runtime
provenance & advisories" below). `download_dependencies.sh` fetches it
into `.models/onnxruntime/` on an s390x host (`--with-onnxruntime`
elsewhere fetches Microsoft's archive instead, below; `--no-onnxruntime`
opts out); the release attaches the s390x CLI tarball,
the FFI library, the Python wheel (`manylinux_2_28_s390x`) and the npm
platform package, `install.sh` picks them, and the container images publish
a `linux/s390x` variant. Every s390x artifact has the same glibc 2.28 floor.

Then either:

```bash
cargo run -p docling-cli -- document.pdf
```

or, in a Node.js/Bun app:

```bash
npm i docling.rs
```

```js
import { convertFileAsync } from 'docling.rs'
const { content } = await convertFileAsync('document.pdf', { to: 'markdown' })
console.log(content)
```

## Runtime provenance & advisories

A statically linked build carries the ONNX Runtime pyke's `ort-sys` pins in
`Cargo.lock` (1.31.0 since #523) — one set of bytes per docling.rs release,
identified by the release. An `ort-load-dynamic` build loads whatever
`libonnxruntime` the host provides, and *"ONNX Runtime 1.17 or newer, any
build"* above is the compatibility statement, not the support statement:
an operator who has to answer "which runtime binary is this process
running, and which advisories apply to it?" needs an exact-binary record,
and "1.31.0" alone is not one (#954 — Microsoft's release archives carry no
SBOM, no signature and no build attestation; the `onnxruntime` repository's
attestations page is empty; checked for v1.31.0). The record here is a
digest pin, kept in `scripts/install/download_dependencies.sh` beside the
code that fetches and checks it, and copied below.

**Supported runtime artifacts.** The runtime an `ort-load-dynamic` build is
supported with is one of these — the same ONNX Runtime version the linked
builds carry, so a docling.rs release has one runtime version across every
build flavour and an advisory against it is read once:

| Platform | Artifact | Archive sha256 | Library (sha256) | Upstream commit |
|---|---|---|---|---|
| linux-x64 | [`onnxruntime-linux-x64-1.31.0.tgz`](https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-linux-x64-1.31.0.tgz) (Microsoft's release) | `cc5c72baf5ae5c8238a6841f0897227be2d02826b9cf98eaf02fdefaeeb03a57` | `libonnxruntime.so.1.31.0` `469849977cb60744ea160927e1a3a0643fd4c55c0aff743251c6d293402e334e` | `402979ac1bef89dcd55add41eaa7277c407599eb` |
| linux-aarch64 | [`onnxruntime-linux-aarch64-1.31.0.tgz`](https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-linux-aarch64-1.31.0.tgz) (Microsoft's release) | `c5b8b3cca31f3d643a3b313b8f42e2d0f16a4fcefd02cda343d97bd5306afaab` | `libonnxruntime.so.1.31.0` `595c7f7a6775f5c7b3d567c3ed94c04f722092575abcc092560f5208a54daf99` | `402979ac1bef89dcd55add41eaa7277c407599eb` |
| osx-arm64 | [`onnxruntime-osx-arm64-1.31.0.tgz`](https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-osx-arm64-1.31.0.tgz) (Microsoft's release) | `1031b36dd3bdaa8d976a6130d9cf2a05e9be320b93ae804f8427b91dc3432afc` | `libonnxruntime.1.31.0.dylib` `2a50a63e7f41f8d4d551d3ee47248df3864f6540109fce8ad520ca3b51af4b70` | `402979ac1bef89dcd55add41eaa7277c407599eb` |
| win-x64 | [`onnxruntime-win-x64-1.31.0.zip`](https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-win-x64-1.31.0.zip) (Microsoft's release) | `3958d8a44984160692c1b4ffd0de3cb48f8c39f9d34bb31e433db06e30b82195` | `onnxruntime.dll` `0c42dc4e278f0968fdf9ebf1b1e54a74c657a9942e70803e453c91993451cf4c` | `402979ac1bef89dcd55add41eaa7277c407599eb` |
| linux-s390x | `onnxruntime-linux-s390x.tar.gz` (this repository's models release, built from source — above) | the release's `SHA256SUMS` / `models.json` entry (it changes with every rebuild of the asset) | `libonnxruntime.so.1.31.0`, sha256 in the tarball's `BUILD_INFO` | `v1.31.0` → the tarball's `GIT_COMMIT_ID` |

Microsoft publishes no osx-x86_64 archive for this version; there is no
supported runtime for an `ort-load-dynamic` build on Intel macOS (the
default build links the runtime in there).

`scripts/install/download_dependencies.sh --with-onnxruntime` installs the
row for the host: on s390x the docling.rs tarball, checked against the
release's `SHA256SUMS` like every other asset; on the four Microsoft
platforms the upstream archive, fetched from Microsoft's release
(`DOCLING_RS_ONNXRUNTIME_URL` points it at a mirror — the pin still has to
match), checked against the archive digest, unpacked so that the library,
its versioned name and the symlinks land flat in `.models/onnxruntime/`
together with upstream's `LICENSE`, `ThirdPartyNotices.txt`,
`GIT_COMMIT_ID` and `VERSION_NUMBER` (+ `VERSION`, the s390x tarball's
spelling), then checked again against the library digest and the commit.
A mismatch deletes the file and fails the run (`DOCLING_RS_MODELS_VERIFY=off`
is the only bypass, and it is the operator's). The macOS archive ships the
unversioned `libonnxruntime.dylib` as a second copy of the library differing
only in its install name; the installer replaces it with a symlink to the
versioned, pinned file, so the bytes `dlopen` reads are the bytes in the
table. Verifying an install by hand:

```bash
sha256sum .models/onnxruntime/libonnxruntime.so.1.31.0   # → the library digest above
cat .models/onnxruntime/GIT_COMMIT_ID                    # → 402979ac1bef89dcd55add41eaa7277c407599eb
cat .models/onnxruntime/BUILD_INFO                       # s390x only: tag, commit, sha256, compiler, glibc floor, CI run
```

**What the s390x tarball records.** `VERSION` (upstream's `VERSION_NUMBER`)
and `LICENSE` from the first build on; since #954 also `ThirdPartyNotices.txt`,
`GIT_COMMIT_ID` — the upstream commit the checkout of `ORT_TAG` resolved to —
and `BUILD_INFO`: tag, commit, the library's sha256, the compiler (`zig cc
-target s390x-linux-gnu.2.28`, zig's version), the glibc floor read from the
binary, whether it was stripped, the docling.rs commit of the recipe
(`scripts/install/build_onnxruntime_s390x.sh`) and the URL of the workflow
run that produced it. The tarball hosted at this writing was built on
2026-10-14 from `v1.31.0` and predates these records (it carries `VERSION`
`1.31.0` and `LICENSE`); the workflow's next run adds them. The release's
`models.json` records the tarball's digest, upstream repository, tag and
recipe (`scripts/install/models_catalog.json`, entry `onnxruntime_s390x`).

**Advisories.** ONNX Runtime is a C++ dependency outside the Rust
dependency tree, so `cargo audit` and the weekly `deps-update.yml` audit job
never see it; its advisories are Microsoft's, at
<https://github.com/microsoft/onnxruntime/security/advisories> (and the
CVE records against the `onnxruntime` product). Reading one against an
install is: the version and commit in the table (or the install's
`GIT_COMMIT_ID`) against the advisory's affected range; the archive and
library digests tell a reader whether the file on disk is that build at all.
There is no per-asset backport manifest upstream — a Microsoft release
archive is the tag it names, and the fix for an advisory is the next ORT
release — so a fix reaches an `ort-load-dynamic` install the same way it
reaches the linked builds: docling.rs moves `ort-sys` (and with it this
table and `build_onnxruntime_s390x.sh`'s `ORT_TAG`) to the fixed release,
and the installer fetches the new pin. A runtime outside the table — a
distro package, a `pip install onnxruntime` wheel's `capi/libonnxruntime.so`,
a self-built library — works when it is 1.17 or newer and is the operator's
to vouch for: docling.rs makes no provenance statement about it, and
`ORT_DYLIB_PATH` is how such a runtime is pointed at deliberately.
