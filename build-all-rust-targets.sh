#!/bin/bash

echo "Building linux x64 release..."
cargo zigbuild --release --bin ltc-gui
echo "Building windows x64 release..."
cargo zigbuild --target x86_64-pc-windows-gnu --release --bin ltc-gui
echo "Building linux x32 release..."
docker run --rm \
         -v "$(pwd)":/app \
         -v "$(pwd)/.cargo_cache:/usr/local/cargo/registry" \
         -w /app/ltc-gui \
         ltc-debian11-builder \
         cargo zigbuild --target i686-unknown-linux-gnu.2.31 --release --bin ltc-gui
