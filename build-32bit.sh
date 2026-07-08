#!/bin/bash
set -e

IMAGE_NAME="tauri-x32-builder"

echo "📦 Creating local build target directories..."
mkdir -p .cargo_cache .npm_cache .rustup_cache src-tauri-32bit/target

echo "📦 Building the Docker compilation environment..."
docker build -t $IMAGE_NAME -f Dockerfile.i386-build .

# We are installing tauri in the container globally, so overwriting the CARGO home
# would also overwrite this installation of tauri. This is the reason for mounting
# the .cargo_cache/ into /usr/local/cargo/registry
# NOTE ON Virtual Isolation:
# 1. We mount the 32-bit Rust configuration.
# 2. We mount frontend configs & source files as Read-Only to protect your host.
# 3. We map specific caches to keep subsequent builds fast.

docker run --rm \
  --user "$(id -u):$(id -g)" \
  -e HOME=/app \
  -e RUSTUP_TOOLCHAIN=stable \
  -e APPIMAGE_EXTRACT_AND_RUN=1 \
  -e ARCH=i686 \
  --workdir /app \
  -v "$(pwd)/src-tauri-32bit:/app/src-tauri-32bit" \
  -v "$(pwd)/package.json:/app/package.json:ro" \
  -v "$(pwd)/vite.config.ts:/app/vite.config.ts:ro" \
  -v "$(pwd)/tsconfig.json:/app/tsconfig.json:ro" \
  -v "$(pwd)/index.html:/app/index.html:ro" \
  -v "$(pwd)/src:/app/src:ro" \
  -v "$(pwd)/assets:/app/assets:ro" \
  -v "$(pwd)/.cargo_cache:/usr/local/cargo/registry" \
  -v "$(pwd)/.npm_cache:/.npm" \
  -v "$(pwd)/package-lock.json:/app/package-lock.json.host:ro" \
  -e PKG_CONFIG_ALLOW_CROSS=1 \
  -e PKG_CONFIG_PATH=/usr/lib/i386-linux-gnu/pkgconfig \
  $IMAGE_NAME \
  bash -c "cp /app/package-lock.json.host /app/package-lock.json && npm install && npm run build && cd src-tauri-32bit && cargo tauri build --target i686-unknown-linux-gnu --verbose"

echo "✅ Build complete! Check your ./src-tauri-32bit/target/i686-unknown-linux-gnu/release/bundle/ directory."
