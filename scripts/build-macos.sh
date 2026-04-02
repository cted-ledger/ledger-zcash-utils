#!/usr/bin/env bash
# Build a universal macOS binary for zcash-key-derive (arm64 + x86_64).
# Requires: Rust toolchain (rustup) + Xcode command line tools (lipo)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
OUTPUT="$DIST_DIR/zcash-key-derive-macos-universal"

cd "$REPO_ROOT"

echo "Installing required Rust targets..."
rustup target add aarch64-apple-darwin x86_64-apple-darwin

echo ""
echo "Building for aarch64-apple-darwin..."
cargo build --release --bin zcash-key-derive --target aarch64-apple-darwin

echo ""
echo "Building for x86_64-apple-darwin..."
cargo build --release --bin zcash-key-derive --target x86_64-apple-darwin

mkdir -p "$DIST_DIR"

echo ""
echo "Creating universal binary with lipo..."
lipo -create -output "$OUTPUT" \
  "$REPO_ROOT/target/aarch64-apple-darwin/release/zcash-key-derive" \
  "$REPO_ROOT/target/x86_64-apple-darwin/release/zcash-key-derive"

echo ""
echo "Done: $OUTPUT"
file "$OUTPUT"
"$OUTPUT" --help
