#!/usr/bin/env bash
# Cross-compile ONNX Runtime as a shared library for IBM Z (#504).
#
# pyke's `ort` ships prebuilt runtimes for x86_64/aarch64 only and Microsoft
# publishes no s390x release, so the `ort-load-dynamic` build of docling.rs
# dlopens a libonnxruntime.so that has to come from somewhere: this script
# builds it from source — cross-compiled on an x86_64 host (minutes), not
# natively under QEMU (hours, how upstream's own s390x CI does it). The
# default compiler is zig (`zig cc -target s390x-linux-gnu.2.28`): the
# library then needs glibc 2.28 and carries its own libc++, so it loads on
# RHEL 8/9 era mainframe Linux, where the distro gcc cross toolchain
# (ORT_TOOLCHAIN=gcc) pins it to glibc 2.38 and GCC 13's libstdc++.
# .github/workflows/onnxruntime-s390x.yml runs it and publishes the result
# into the models release as onnxruntime-linux-s390x.tar.gz, which
# scripts/install/download_dependencies.sh fetches into .models/onnxruntime/
# on an s390x host (or with --with-onnxruntime).
#
# Host prerequisites (Ubuntu 24.04): cmake (>= 3.28) ninja-build python3 curl
# git binutils-s390x-linux-gnu (strip), and either zig on PATH (`pip install
# ziglang` provides one — the script links it) or, for ORT_TOOLCHAIN=gcc,
# gcc-s390x-linux-gnu g++-s390x-linux-gnu; qemu-user-static plus
# libc6-s390x-cross (the target's loader and libc under /usr/s390x-linux-gnu,
# what `qemu-s390x-static -L` resolves against) to run the result.
#
#   scripts/install/build_onnxruntime_s390x.sh
#   ORT_TAG=v1.31.0 JOBS=8 OUT_DIR=$PWD/onnxruntime-s390x scripts/install/build_onnxruntime_s390x.sh
#
# Environment:
#   ORT_TAG              ONNX Runtime tag to build (default v1.31.0 — the
#                        version the linked builds carry since #523, ort
#                        2.0.0-rc.14; anything from 1.29 up is free of the
#                        NCHWc picture-classifier bug, #517, and ort accepts
#                        any runtime from 1.17).
#   ORT_TOOLCHAIN        zig (default) | gcc — see above.
#   ORT_SRC_DIR          an existing checkout to build instead of cloning.
#   JOBS                 parallel compile jobs (default: nproc).
#   OUT_DIR              where lib/, LICENSE, ThirdPartyNotices.txt, VERSION,
#                        GIT_COMMIT_ID and BUILD_INFO land (default ./onnxruntime-s390x).
#   PROTOC               a host protoc of the protobuf release the tag pins in
#                        cmake/deps.txt (default: downloaded from that release —
#                        ONNX Runtime needs the host's protoc when
#                        cross-compiling, its own would be an s390x binary).
#   CMAKE_EXTRA_DEFINES  extra KEY=VALUE cmake definitions appended (e.g.
#                        FETCHCONTENT_SOURCE_DIR_ONNX=… to feed pre-fetched deps).
set -euo pipefail

ORT_TAG="${ORT_TAG:-v1.31.0}"
ORT_TOOLCHAIN="${ORT_TOOLCHAIN:-zig}"
JOBS="${JOBS:-$(nproc)}"
OUT_DIR="${OUT_DIR:-$PWD/onnxruntime-s390x}"
WORK="${WORK_DIR:-$PWD/.onnxruntime-build}"
HERE="$(cd "$(dirname "$0")" && pwd)"

for tool in cmake ninja python3 curl git; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required (apt-get install cmake ninja-build python3 curl git)" >&2; exit 1; }
done
mkdir -p "$WORK" "$OUT_DIR"

TOOLCHAIN_DEFINES=()
case "$ORT_TOOLCHAIN" in
  zig)
    # `zig` from PATH, else the ziglang wheel's binary (pip/pipx/uv installs).
    ZIG="$(command -v zig || true)"
    if [ -z "$ZIG" ]; then
      ZIG="$(python3 -c 'import ziglang,os;print(os.path.join(os.path.dirname(ziglang.__file__),"zig"))' 2>/dev/null || true)"
    fi
    [ -n "$ZIG" ] && [ -x "$ZIG" ] || { echo "error: zig is required for ORT_TOOLCHAIN=zig (pip install ziglang, or put zig on PATH); ORT_TOOLCHAIN=gcc uses the distro cross toolchain" >&2; exit 1; }
    # Wrapper scripts: cmake wants a compiler path, zig wants its target.
    # MLAS's s390x kernels are built for z15 (onnxruntime_mlas.cmake adds
    # `-mvx -mzvector -march=z15`); zig ignores `-march` and emits
    # `-target-feature -vector` from its own CPU model, so the vector
    # facility and its z14/z15 enhancements are enabled explicitly.
    ZIGW="$WORK/zigw"; mkdir -p "$ZIGW"
    ZIG_FLAGS='-target s390x-linux-gnu.2.28 -mcpu=z15 -Xclang -target-feature -Xclang +vector -Xclang -target-feature -Xclang +vector-enhancements-1 -Xclang -target-feature -Xclang +vector-enhancements-2'
    printf '#!/bin/sh\nexec "%s" cc %s "$@"\n' "$ZIG" "$ZIG_FLAGS" > "$ZIGW/zig-cc"
    printf '#!/bin/sh\nexec "%s" c++ %s "$@"\n' "$ZIG" "$ZIG_FLAGS" > "$ZIGW/zig-cxx"
    printf '#!/bin/sh\nexec "%s" ar "$@"\n' "$ZIG" > "$ZIGW/zig-ar"
    printf '#!/bin/sh\nexec "%s" ranlib "$@"\n' "$ZIG" > "$ZIGW/zig-ranlib"
    chmod +x "$ZIGW"/zig-*
    "$ZIGW/zig-cc" --version | head -1
    TOOLCHAIN="$HERE/cmake/s390x-linux-gnu-zig.toolchain.cmake"
    TOOLCHAIN_DEFINES=("ZIG_WRAPPER_DIR=$ZIGW")
    ;;
  gcc)
    for tool in s390x-linux-gnu-gcc s390x-linux-gnu-g++; do
      command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required for ORT_TOOLCHAIN=gcc (apt-get install gcc-s390x-linux-gnu g++-s390x-linux-gnu)" >&2; exit 1; }
    done
    TOOLCHAIN="$HERE/cmake/s390x-linux-gnu.toolchain.cmake"
    ;;
  *) echo "error: ORT_TOOLCHAIN must be zig or gcc" >&2; exit 1 ;;
esac
STRIP="$(command -v s390x-linux-gnu-strip || true)"

SRC="${ORT_SRC_DIR:-$WORK/onnxruntime}"
if [ ! -d "$SRC/.git" ]; then
  git clone --depth 1 --branch "$ORT_TAG" --recurse-submodules --shallow-submodules https://github.com/microsoft/onnxruntime.git "$SRC"
fi

# The host protoc must be the release of the protobuf this ONNX Runtime builds
# against — its generated code only compiles with that runtime's headers
# (1.31 moved to 33.6, and 21.x output fails with "undeclared identifier
# 'Arena'"). Read it from the checkout's cmake/deps.txt; 21.12 for a tag whose
# entry has no version in it.
PROTOC_VERSION="$(grep -m1 '^protobuf;' "$SRC/cmake/deps.txt" 2>/dev/null | grep -oE '/v[0-9]+(\.[0-9]+)+' | head -n1 | cut -c3- || true)"
PROTOC_VERSION="${PROTOC_VERSION:-21.12}"
if [ -z "${PROTOC:-}" ]; then
  PROTOC="$WORK/protoc-$PROTOC_VERSION/bin/protoc"
  if [ ! -x "$PROTOC" ]; then
    case "$(uname -m)" in
      x86_64 | amd64) PROTOC_ARCH=x86_64 ;;
      aarch64 | arm64) PROTOC_ARCH=aarch_64 ;;
      *) echo "error: no prebuilt protoc for the host arch $(uname -m); set PROTOC" >&2; exit 1 ;;
    esac
    mkdir -p "$WORK/protoc-$PROTOC_VERSION"
    curl -fsSL -o "$WORK/protoc-$PROTOC_VERSION.zip" "https://github.com/protocolbuffers/protobuf/releases/download/v$PROTOC_VERSION/protoc-$PROTOC_VERSION-linux-$PROTOC_ARCH.zip"
    python3 -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$WORK/protoc-$PROTOC_VERSION.zip" "$WORK/protoc-$PROTOC_VERSION"
    chmod +x "$PROTOC"
  fi
fi
"$PROTOC" --version

# clang (zig) wants a builtin type after `__vector` where gcc also takes a
# typedef: MLAS's s390x quantized kernels spell `__vector int32_t`. Patched
# in place (idempotent; `int32_t` is `int` on s390x). gcc builds take it too.
#
# From 1.31 the s390x SGEMM/DGEMM kernels splat a lane with
# `vec_splat(AElements[Row], Lane)`, `Lane` a template parameter. clang
# resolves that call while parsing the template — its vec_splat overloads
# take the index through enable_if ("a constant integer from 0 to 3") — before
# `Lane` has a value, and rejects every overload; gcc, what Microsoft builds
# s390x with, defers it. `vec_splats(AElements[Row][Lane])` is the same
# operation and compiles to the same vrepf/vrepg (checked with zig 0.17 /
# clang 22 under qemu).
if [ "$ORT_TOOLCHAIN" = zig ]; then
  sed -i -E 's/__vector int32_t\b/__vector int/g' \
    "$SRC/onnxruntime/core/mlas/lib/qlmul.cpp" \
    "$SRC/onnxruntime/core/mlas/lib/s390x/qgemm_kernel_zvector.cpp"
  FGEMM="$SRC/onnxruntime/core/mlas/lib/s390x/FgemmKernelZVECTOR.h"
  if [ -f "$FGEMM" ]; then
    sed -i -E 's/vec_splat\(AElements\[Row\], Lane\)/vec_splats(AElements[Row][Lane])/' "$FGEMM"
  fi
fi

# --skip_tests only skips *running* the tests; onnxruntime_BUILD_UNIT_TESTS=OFF
# keeps build.py from compiling them too (onnxruntime_test_all alone is
# thousands of objects — a third of the wall time — and drags googletest in).
# --skip_submodule_sync: the clone above is complete. onnxruntime_USE_KLEIDIAI=OFF:
# ARM-only, and build.py turns it on unconditionally. The toolchain file
# carries the Eigen flag (see there).
#
# --no_telemetry (1.29+): build.py turns ONNX Runtime's 1DS telemetry on by
# default for Linux from 1.29 — cpp_client_telemetry with a bundled curl,
# mbedTLS and sqlite compiled into libonnxruntime.so, uploading usage events
# to Microsoft. A runtime we ship to mainframes must not phone home, and the
# bundle does not link with zig anyway (`--exclude-libs`, which hides the
# embedded curl/mbedTLS symbols, is an unsupported linker arg there). 1.28's
# build.py has no such flag — telemetry was opt-in (`--use_telemetry`) — so it
# is passed only when this checkout knows it.
NO_TELEMETRY=()
if grep -q -- '"--no_telemetry"' "$SRC/tools/ci_build/build_args.py" 2>/dev/null; then
  NO_TELEMETRY=(--no_telemetry)
fi
cd "$SRC"
# shellcheck disable=SC2086
./build.sh --config Release --build_shared_lib --parallel "$JOBS" \
  --skip_tests --skip_submodule_sync --allow_running_as_root \
  --compile_no_warning_as_error "${NO_TELEMETRY[@]}" \
  --path_to_protoc_exe "$PROTOC" \
  --cmake_extra_defines "CMAKE_TOOLCHAIN_FILE=$TOOLCHAIN" onnxruntime_CROSS_COMPILING=ON \
    onnxruntime_USE_KLEIDIAI=OFF onnxruntime_BUILD_UNIT_TESTS=OFF \
    "${TOOLCHAIN_DEFINES[@]}" ${CMAKE_EXTRA_DEFINES:-}

BUILD="$SRC/build/Linux/Release"
rm -rf "$OUT_DIR/lib" && mkdir -p "$OUT_DIR/lib"
# libonnxruntime.so.<ver> plus the unversioned name ort dlopens; the SONAME
# keeps the versioned one resolvable from the same directory.
cp -a "$BUILD"/libonnxruntime.so* "$OUT_DIR/lib/"
[ -n "$STRIP" ] && "$STRIP" --strip-unneeded "$OUT_DIR"/lib/libonnxruntime.so.*.*.* || echo "(no s390x-linux-gnu-strip on the host — the library is left unstripped)"
cp "$SRC/LICENSE" "$OUT_DIR/LICENSE"
cp "$SRC/ThirdPartyNotices.txt" "$OUT_DIR/ThirdPartyNotices.txt"
tr -d '\n' < "$SRC/VERSION_NUMBER" > "$OUT_DIR/VERSION"; echo >> "$OUT_DIR/VERSION"
# Provenance records (#954), the same files Microsoft's own release archives
# carry plus what they lack: GIT_COMMIT_ID is the upstream commit the library
# was compiled from, BUILD_INFO the exact-binary record — tag, commit, the
# library's sha256, the compiler, the glibc floor, the docling.rs recipe
# commit and the CI run — an operator takes to upstream's advisories.
git -C "$SRC" rev-parse HEAD > "$OUT_DIR/GIT_COMMIT_ID"
LIB="$(ls "$OUT_DIR"/lib/libonnxruntime.so.*.*.*)"
GLIBC_FLOOR=""
if command -v s390x-linux-gnu-objdump >/dev/null 2>&1; then
  GLIBC_FLOOR="$(s390x-linux-gnu-objdump -T "$LIB" | grep -o 'GLIBC_2\.[0-9]*' | sort -t. -k2 -n | uniq | tail -1)"
fi
case "$ORT_TOOLCHAIN" in
  zig) COMPILER="zig $("$ZIG" version) ($ZIG_FLAGS)" ;;
  *) COMPILER="$(s390x-linux-gnu-gcc --version | head -1)" ;;
esac
{
  echo "artifact=onnxruntime-linux-s390x.tar.gz"
  echo "onnxruntime_tag=$ORT_TAG"
  echo "onnxruntime_commit=$(cat "$OUT_DIR/GIT_COMMIT_ID")"
  echo "onnxruntime_version=$(cat "$OUT_DIR/VERSION")"
  echo "library=$(basename "$LIB")"
  echo "library_sha256=$(sha256sum "$LIB" | cut -d' ' -f1)"
  echo "target=s390x-unknown-linux-gnu"
  echo "compiler=$COMPILER"
  echo "glibc_floor=${GLIBC_FLOOR:-unknown}"
  echo "stripped=$([ -n "$STRIP" ] && echo yes || echo no)"
  echo "recipe=scripts/install/build_onnxruntime_s390x.sh"
  echo "recipe_commit=$(git -C "$HERE" rev-parse HEAD 2>/dev/null || echo unknown)"
  echo "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [ -n "${GITHUB_RUN_ID:-}" ]; then
    echo "workflow_run=${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-}/actions/runs/$GITHUB_RUN_ID${GITHUB_RUN_ATTEMPT:+/attempts/$GITHUB_RUN_ATTEMPT}"
  fi
} > "$OUT_DIR/BUILD_INFO"
echo "built ONNX Runtime $(cat "$OUT_DIR/VERSION") @ $(cat "$OUT_DIR/GIT_COMMIT_ID") for s390x ($ORT_TOOLCHAIN toolchain):"
ls -la "$OUT_DIR/lib"
file "$LIB" | sed 's/,.*//'
echo "glibc floor: ${GLIBC_FLOOR:-unknown}"
cat "$OUT_DIR/BUILD_INFO"
