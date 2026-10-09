#!/usr/bin/env bash
# Local release build: macOS universal binary + packaging + checksums.
# (Full multi-platform distribution: .github/workflows/release.yml)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')
DIST=dist
mkdir -p "$DIST"

has_target() { rustup target list --installed | grep -q "^$1$"; }

echo "== building targets"
cargo build --release --target aarch64-apple-darwin -p ev-server

if has_target x86_64-apple-darwin; then
    cargo build --release --target x86_64-apple-darwin -p ev-server
    echo "== creating macOS universal binary (arm64 + x86_64)"
    lipo -create \
        target/aarch64-apple-darwin/release/evolveroute \
        target/x86_64-apple-darwin/release/evolveroute \
        -output "$DIST/evolveroute"
else
    echo "(x86_64-apple-darwin target missing — arm64-only build)"
    cp target/aarch64-apple-darwin/release/evolveroute "$DIST/evolveroute"
fi

echo "== packaging"
STAGE="$DIST/evolveroute-$VERSION-macos-universal"
rm -rf "$STAGE"; mkdir -p "$STAGE"
cp "$DIST/evolveroute" "$STAGE/"
cp README.md LICENSE THIRD-PARTY-NOTICES.md "$STAGE/" 2>/dev/null || true
cp config/evolveroute.default.toml "$STAGE/evolveroute.default.toml"
tar -czf "$STAGE.tar.gz" -C "$DIST" "$(basename "$STAGE")"
shasum -a 256 "$STAGE.tar.gz" > "$STAGE.tar.gz.sha256"

echo "== done: $STAGE.tar.gz"
file "$DIST/evolveroute"
