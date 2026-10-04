#!/bin/bash
# build-libltc-static.sh — build a static-only libltc prefix from a pinned
# source tag. One recipe, three consumers: CI release jobs, the local
# release script (build-all-rust-targets.sh), and the README instructions.
#
# Usage:
#   build-libltc-static.sh <prefix-dir> [<host-triple>]
#     <host-triple>  GNU configure --host triple when cross-building
#                    (e.g. x86_64-w64-mingw32); omit for native builds.
#   build-libltc-static.sh --help
#
# Idempotent: exits 0 without rebuilding if <prefix>/lib/libltc.a exists.
#
# Linking contract (see plans/PHASE6-tag-release-pipeline-plan.md §4.3):
# the prefix contains ONLY libltc.a (no .so), so `-lltc` resolves to the
# static archive with no libltc-rs build.rs changes. Pair with:
#   RUSTFLAGS="-L native=<prefix>/lib"
#   BINDGEN_EXTRA_CLANG_ARGS_<target>=-I<prefix>/include
#
# Exit codes: 0 = built (or already present); 2 = usage error; 1 = build failure.
set -euo pipefail

# Pinned libltc release; must match LIBLTC_TAG in ci.yml (windows-tests)
# and release.yml. Overridable via env for tests.
LIBLTC_TAG="${LIBLTC_TAG:-v1.3.2}"
LIBLTC_REPO_URL="https://github.com/x42/libltc.git"

usage() {
    echo "usage: $0 <prefix-dir> [<host-triple>]" >&2
    echo "       $0 --help" >&2
}

[ "${1:-}" = "--help" ] && { usage; exit 0; }
if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    usage >&2
    exit 2
fi

prefix=$1
host=${2:-}

if [ -f "$prefix/lib/libltc.a" ]; then
    echo "libltc-static: cache hit — $prefix/lib/libltc.a present, nothing to do"
    exit 0
fi

srcdir=$(mktemp -d /tmp/libltc-static.XXXXXX)
trap 'rm -rf "$srcdir"' EXIT

echo "libltc-static: cloning ${LIBLTC_REPO_URL}"
git clone --depth 1 --branch "$LIBLTC_TAG" "$LIBLTC_REPO_URL" "$srcdir/libltc"

hostargs=""
[ -n "$host" ] && hostargs="--host=$host"

(
    cd "$srcdir/libltc"
    autoreconf -vfi
    ./configure $hostargs --prefix="$prefix" --enable-static --disable-shared
    make -j"$(nproc)"
    make install
)

if [ ! -f "$prefix/lib/libltc.a" ]; then
    echo "libltc-static: build finished but $prefix/lib/libltc.a is missing" >&2
    exit 1
fi
if [ ! -f "$prefix/include/ltc.h" ]; then
    echo "libltc-static: ERROR — $prefix/include/ltc.h is missing" >&2
    exit 1
fi
# Shadow shim: a libltc.so symlink to the archive so `-lltc` resolves
# statically even on hosts that also ship a shared libltc (e.g. a dev
# machine with libltc-dev installed). lld/ld treat the file by content —
# it is an archive, so the link is static.
ln -sf libltc.a "$prefix/lib/libltc.so"
echo "libltc-static: ok — $prefix (static libltc.a + ltc.h + .so shadow shim)"
