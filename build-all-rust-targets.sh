#!/bin/bash
# Local release build: the official ship matrix minus i686 (Phase 6, D3 —
# the Medion tablet build is a documented local speciality via
# Dockerfile.gui-build, re-introduced to this script only on user demand).
#
# Static libltc everywhere (D1) and a glibc 2.31 floor for linux x64 (D2),
# so locally built binaries match the CI release artifacts:
#   linux x64    target/x86_64-unknown-linux-gnu/release/ltc-gui
#   windows x64  target/x86_64-pc-windows-gnu/release/ltc-gui.exe
#
# Prerequisites: cargo-zigbuild + zig (linux), rustup target
# x86_64-pc-windows-gnu, mingw-w64 linker (see README.org "Releases").
set -euo pipefail

cd "$(dirname "$0")"

prefix_dir="$PWD/libltc-prefix"   # gitignored; safe to delete any time

echo "Provisioning static libltc prefixes (idempotent)..."
bash scripts/build-libltc-static.sh "$prefix_dir/linux"
bash scripts/build-libltc-static.sh "$prefix_dir/win" x86_64-w64-mingw32

echo "Building linux x64 release (glibc 2.31 floor)..."
# zigbuild passes TARGET=x86_64-unknown-linux-gnu.2.31, so bindgen's
# target-specific env lookup carries the .2.31 suffix — the plain var is
# the one that reliably reaches every bindgen invocation.
RUSTFLAGS="-L native=$prefix_dir/linux/lib -C link-arg=-Wl,--allow-shlib-undefined" \
BINDGEN_EXTRA_CLANG_ARGS="-I$prefix_dir/linux/include" \
BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_gnu="-I$prefix_dir/linux/include" \
    cargo zigbuild --target x86_64-unknown-linux-gnu.2.31 --release --bin ltc-gui

echo "Building windows x64 release..."
RUSTFLAGS="-L native=$prefix_dir/win/lib" \
BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I$prefix_dir/win/include" \
    cargo build --target x86_64-pc-windows-gnu --release --bin ltc-gui

echo "Verifying static libltc + glibc floor..."
if ldd target/x86_64-unknown-linux-gnu/release/ltc-gui | grep libltc; then
    echo "ERROR: linux binary links libltc dynamically" >&2
    exit 1
fi
max_glibc=$(objdump -T target/x86_64-unknown-linux-gnu/release/ltc-gui | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1 | sed 's/^GLIBC_//')
echo "linux binary: max GLIBC symbol version $max_glibc"
if [ "$(printf '2.31\n%s\n' "$max_glibc" | sort -V | tail -1)" != "2.31" ]; then
    echo "ERROR: linux binary requires GLIBC $max_glibc > 2.31" >&2
    exit 1
fi

echo "Done. Artifacts:"
ls -la target/x86_64-unknown-linux-gnu/release/ltc-gui target/x86_64-pc-windows-gnu/release/ltc-gui.exe
