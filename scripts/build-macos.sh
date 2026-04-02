#!/usr/bin/env bash
# Build zcash-key-derive for the current macOS host architecture.
# Requires: Rust toolchain (rustup)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

echo "Building zcash-key-derive (release, macOS)..."
cargo build --release --bin zcash-key-derive

BINARY="$REPO_ROOT/target/release/zcash-key-derive"
echo ""
echo "Done: $BINARY"
file "$BINARY"
"$BINARY" --help
