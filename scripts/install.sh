#!/usr/bin/env bash
# Local / CI installer for fleischwolf: build from source and install a
# self-contained tree under /usr/local/fleischwolf with a `fleischwolf`
# command on PATH. Designed for one-liner use in dev boxes and pipelines:
#
#   curl -fsSL https://raw.githubusercontent.com/artiz/fleischwolf/master/scripts/install.sh | bash
#
# or from a checkout:
#
#   scripts/install.sh
#
# What it does:
#   1. Checks for a Rust toolchain (>= 1.82 for the PDF crate); installs one
#      via rustup (non-interactive) if `cargo` is missing.
#   2. Builds the CLI in release mode (`cargo build --release -p fleischwolf-cli`).
#   3. Installs to $FLEISCHWOLF_PREFIX (default /usr/local/fleischwolf):
#        bin/fleischwolf          the CLI (ONNX Runtime statically linked)
#        models/…, .pdfium/…      fetched by scripts/download_dependencies.sh
#      and symlinks it as /usr/local/bin/fleischwolf.
#   4. Writes /etc/profile.d/fleischwolf.sh exporting the DOCLING_*/PDFIUM_*
#      paths. This is belt-and-braces only: the binary also resolves models
#      and pdfium relative to its own (symlink-resolved) location, so the
#      symlink works in pipelines that never source profile.d.
#
# Options (env vars):
#   FLEISCHWOLF_PREFIX=/opt/fleischwolf   install tree (default /usr/local/fleischwolf)
#   FLEISCHWOLF_BIN_DIR=/usr/local/bin    where the symlink goes
#   FLEISCHWOLF_REF=master                git ref to build when cloning
#   FLEISCHWOLF_NO_ASR=1                  skip the Whisper ASR models (~150 MB)
#   FLEISCHWOLF_SUDO=0                    never invoke sudo (fail instead)
set -euo pipefail

REPO_URL="https://github.com/artiz/fleischwolf"
PREFIX="${FLEISCHWOLF_PREFIX:-/usr/local/fleischwolf}"
BIN_DIR="${FLEISCHWOLF_BIN_DIR:-/usr/local/bin}"
REF="${FLEISCHWOLF_REF:-master}"

say() { printf '\033[1m[fleischwolf]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[fleischwolf]\033[0m %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"

# Privilege helper: only used for the install/copy steps, never for the build.
SUDO=""
if [ ! -w "$(dirname "$PREFIX")" ] || { [ -e "$PREFIX" ] && [ ! -w "$PREFIX" ]; }; then
  if [ "${FLEISCHWOLF_SUDO:-1}" = "0" ]; then
    die "$PREFIX is not writable and FLEISCHWOLF_SUDO=0"
  fi
  command -v sudo >/dev/null 2>&1 || die "$PREFIX is not writable and sudo is unavailable"
  SUDO="sudo"
fi

# --- 1. Rust toolchain -------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  # Respect an existing rustup installation that just isn't on PATH yet.
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
fi
if ! command -v cargo >/dev/null 2>&1; then
  say "Rust toolchain not found — installing via rustup (stable, non-interactive)"
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || command -v clang >/dev/null 2>&1 \
  || die "no C compiler (cc/gcc/clang) — install build-essential / clang first"
say "using $(cargo --version)"

# --- 2. Sources: use the checkout we're in, else clone ------------------------
if [ -f Cargo.toml ] && [ -d crates/fleischwolf-cli ]; then
  SRC_DIR="$(pwd)"
  say "building from existing checkout: $SRC_DIR"
else
  SRC_DIR="$(mktemp -d)/fleischwolf"
  say "cloning $REPO_URL ($REF) to $SRC_DIR"
  if command -v git >/dev/null 2>&1; then
    git clone --depth 1 --branch "$REF" "$REPO_URL" "$SRC_DIR"
  else
    mkdir -p "$SRC_DIR"
    curl -fsSL "$REPO_URL/archive/refs/heads/$REF.tar.gz" | tar -xz -C "$SRC_DIR" --strip-components=1
  fi
  cd "$SRC_DIR"
fi

# --- 3. Build ------------------------------------------------------------------
say "building the CLI (release)"
cargo build --release -p fleischwolf-cli

# --- 4. Install tree -----------------------------------------------------------
say "installing to $PREFIX"
$SUDO mkdir -p "$PREFIX/bin"
$SUDO cp target/release/fleischwolf "$PREFIX/bin/fleischwolf"

# Models + pdfium land inside the prefix; download_dependencies.sh fetches into
# the *current* directory, so run it from there. It is idempotent (skips files
# already present), so re-running the installer only fetches what's missing.
DL_ARGS=""
[ "${FLEISCHWOLF_NO_ASR:-0}" = "1" ] && DL_ARGS="--no-asr"
say "fetching models + pdfium into $PREFIX (idempotent)"
# shellcheck disable=SC2086
(cd "$PREFIX" && $SUDO sh "$SRC_DIR/scripts/download_dependencies.sh" $DL_ARGS)

say "linking $BIN_DIR/fleischwolf -> $PREFIX/bin/fleischwolf"
$SUDO mkdir -p "$BIN_DIR"
$SUDO ln -sfn "$PREFIX/bin/fleischwolf" "$BIN_DIR/fleischwolf"

# --- 5. Environment (optional convenience) --------------------------------------
# The binary resolves models/.pdfium next to its own location through the
# symlink, so these exports are not required for `fleischwolf` itself — they
# help other tools (scripts, the Node bindings) find the same assets.
if [ -d /etc/profile.d ] && [ -n "$SUDO" -o -w /etc/profile.d ]; then
  say "writing /etc/profile.d/fleischwolf.sh"
  $SUDO tee /etc/profile.d/fleischwolf.sh >/dev/null <<EOF
# fleischwolf (installed by scripts/install.sh) — not required by the CLI
# itself (it resolves these relative to its own binary), provided for other
# consumers of the same model tree.
export PDFIUM_DYNAMIC_LIB_PATH="$PREFIX/.pdfium/lib"
export DOCLING_OCR_REC_ONNX="$PREFIX/models/ocr_rec.onnx"
export DOCLING_OCR_DICT="$PREFIX/models/ppocr_keys_v1.txt"
export DOCLING_TABLEFORMER_ENCODER="$PREFIX/models/tableformer/encoder.onnx"
export DOCLING_TABLEFORMER_BBOX="$PREFIX/models/tableformer/bbox.onnx"
EOF
fi

# --- 6. Smoke test ---------------------------------------------------------------
say "smoke test: converting a trivial Markdown document"
TMP_MD="$(mktemp --suffix=.md 2>/dev/null || mktemp -t fleischwolf.XXXXXX.md)"
printf '# fleischwolf\n\ninstalled.\n' > "$TMP_MD"
"$BIN_DIR/fleischwolf" "$TMP_MD" >/dev/null || die "smoke test failed"
rm -f "$TMP_MD"

say "done. Try:  fleischwolf your.pdf > out.md"
say "layout: $PREFIX  (binary, models/, .pdfium/); uninstall: rm -rf $PREFIX $BIN_DIR/fleischwolf /etc/profile.d/fleischwolf.sh"
