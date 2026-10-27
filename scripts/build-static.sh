#!/bin/sh
# Build the static binary that ships in bin/aerona-<platform>.
#
# Why a script: the launcher will happily install a toolchain and build this itself, but
# the *shipped* artifact has to be reproducible by whoever maintains the repo, and the
# exact invocation below is not obvious. Two details are load-bearing:
#
#   --target x86_64-unknown-linux-musl   the result is statically linked, so it runs on a
#                                        panel image of any glibc vintage (2.31 bookworm,
#                                        2.36 bullseye, alpine, whatever) - a glibc build
#                                        made here would demand the build box's glibc.
#   CARGO_PROFILE_RELEASE_CODEGEN_UNITS  the workspace release profile is tuned for size
#      =16, LTO off                     (codegen-units=1 + thin LTO) and needs well over
#                                        1 GB of RSS for `serenity` alone. On a 2 GB box
#                                        that is an OOM kill mid-build, so the packaging
#                                        build loosens it. The binary differs by a few
#                                        percent of throughput, nothing of behaviour.
#                                        Loosening further does not always help: measured on
#                                        such a box, `serenity` was killed at 1.2 GB of RSS
#                                        with `-C opt-level=1 -C codegen-units=128`, because
#                                        most of that memory is rustc's type arena rather
#                                        than LLVM's per-CGU work - so the ceiling is the
#                                        crate, not the flags. What cleared it was swap:
#                                        `fallocate -l 4G /swapfile && chmod 600 /swapfile
#                                        && mkswap /swapfile && swapon /swapfile`. An OOM
#                                        kill mid-build is not a build error; look at
#                                        `dmesg | tail` before changing code.
#
# Requires: a stable Rust toolchain, `musl-gcc` + `musl-tools`, and the target installed
# (`rustup target add x86_64-unknown-linux-musl`).
set -eu

TARGET="${AERONA_MUSL_TARGET:-x86_64-unknown-linux-musl}"
ARCH="${TARGET%%-*}"
OUT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/bin"
# The name `main.py` looks for is `aerona-<os>-<arch>`, and it is built from
# `sys.platform`/`uname -m` on the target - not from a Rust target triple, whose fields
# are in a different order (`x86_64-unknown-linux-musl` is arch-vendor-os-abi, and
# slicing it like a name yields "unknown"). Map the triples we can actually ship.
case "$TARGET" in
    x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) OS="linux" ;;
    # "macos", not "darwin": `main.py` derives the tag from `platform.system()`, which
    # says Darwin, and the launcher only ever looks for the name it would generate.
    x86_64-apple-darwin | aarch64-apple-darwin) OS="macos" ;;
    x86_64-pc-windows-msvc) OS="windows"; EXE=".exe" ;;
    *) OS=""; EXE="" ;;
esac
EXE="${EXE:-}"
if [ -n "$OS" ]; then
    NAME="aerona-${OS}-${ARCH}${EXE}"
else
    NAME="aerona-${TARGET}${EXE}"
fi

export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
export CC_x86_64_unknown_linux_musl=musl-gcc
export AR_x86_64_unknown_linux_musl=ar
# Packagers with less RAM still have one dial left: peak RSS for a single crate scales with how
# much work a codegen unit holds, so `AERONA_BUILD_CGU=128` trades a little cross-function inlining
# for fitting in memory at all. An OOM kill halfway through `serenity` ships nothing.
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${AERONA_BUILD_CGU:-16}"
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_DEBUG=0
# A packager with ~2 GB of RAM and no swap has to go further than loosening CGUs: an
# opt-level=3 `serenity` sits around 800 MB of RSS, which on such a box leaves no page
# cache, and the build spends its time in direct reclaim instead of compiling (observed:
# 5 minutes of CPU across 37 wall-clock minutes). Drop the optimisation level for the
# packaging build and it stops thrashing. Behaviour is identical; the difference is a few
# percent in JSON-heavy paths, which on one operator and one gateway is nothing.
if [ -n "${AERONA_BUILD_OPTLEVEL:-}" ]; then
    export CARGO_PROFILE_RELEASE_OPT_LEVEL="$AERONA_BUILD_OPTLEVEL"
    echo "release opt-level overridden to $AERONA_BUILD_OPTLEVEL"
fi

jobs="${AERONA_BUILD_JOBS:-1}"
echo "cargo build --release -j $jobs --target $TARGET -p aerona"
cargo build --release -j "$jobs" --target "$TARGET" -p aerona

mkdir -p "$OUT_DIR"
cp -f "target/$TARGET/release/aerona" "$OUT_DIR/$NAME"
chmod 755 "$OUT_DIR/$NAME"

echo
echo "wrote $OUT_DIR/$NAME"
ls -l "$OUT_DIR/$NAME"
if command -v file >/dev/null 2>&1; then file "$OUT_DIR/$NAME"; fi
# A static binary must not name a dynamic interpreter. If this prints a libc.so.6, the
# artifact is NOT portable and must not be shipped.
if command -v readelf >/dev/null 2>&1; then
    if readelf -lW "$OUT_DIR/$NAME" 2>/dev/null | grep -q "INTERP"; then
        echo "warning: binary has a PT_INTERP segment - it is dynamically linked" >&2
        exit 1
    fi
    echo "verified: no PT_INTERP, statically linked"
fi
"$OUT_DIR/$NAME" --version
