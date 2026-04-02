#!/usr/bin/env bash
# Build a static zcash-key-derive binary for Linux x86_64 using Docker.
# Target triple: x86_64-unknown-linux-musl (fully static, no libc dependency).
# Requires: Docker
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
OUTPUT="$DIST_DIR/zcash-key-derive-linux-x86_64"

mkdir -p "$DIST_DIR"

echo "Building zcash-key-derive (release, Linux x86_64 musl, via Docker)..."

docker run --rm \
  --platform linux/amd64 \
  -v "$REPO_ROOT:/workspace" \
  -w /workspace \
  rust:latest \
  bash -c "
    set -euo pipefail
    rustup target add x86_64-unknown-linux-musl
    apt-get update -qq && apt-get install -y -qq musl-tools
    cargo build --release --bin zcash-key-derive --target x86_64-unknown-linux-musl
  "

cp "$REPO_ROOT/target/x86_64-unknown-linux-musl/release/zcash-key-derive" "$OUTPUT"

echo ""
echo "Done: $OUTPUT"
file "$OUTPUT"
