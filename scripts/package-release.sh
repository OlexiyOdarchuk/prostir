#!/usr/bin/env bash
# Builds release artifacts into dist/:
#   prostir-<version>-x86_64-linux.tar.gz   PC part with install.sh
#   prostir-<version>.apk                   headset app
# Signing of the APK: see scripts/build-apk.sh (KEYSTORE…), debug key otherwise.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
DIST="$ROOT/dist"
NAME="prostir-$VERSION-x86_64-linux"
rm -rf "$DIST/$NAME" && mkdir -p "$DIST/$NAME"

cargo build --release -p prostir-host --bin prostir --bin prostir-probe
cp target/release/prostir target/release/prostir-probe "$DIST/$NAME/"
strip --strip-debug "$DIST/$NAME/prostir" "$DIST/$NAME/prostir-probe"
mkdir -p "$DIST/$NAME/packaging"
cp packaging/prostir.svg packaging/prostir.desktop "$DIST/$NAME/packaging/"
cp scripts/install.sh LICENSE README.md README.uk.md CHANGELOG.md "$DIST/$NAME/"
tar -C "$DIST" -czf "$DIST/$NAME.tar.gz" "$NAME"
rm -rf "$DIST/$NAME"

PROFILE=release ./scripts/build-apk.sh
cp build/prostir.apk "$DIST/prostir-$VERSION.apk"

(cd "$DIST" && sha256sum "$NAME.tar.gz" "prostir-$VERSION.apk" > SHA256SUMS)
echo ">> $DIST"
ls -la "$DIST"
