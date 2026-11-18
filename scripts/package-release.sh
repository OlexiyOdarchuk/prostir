#!/usr/bin/env bash
# Builds release artifacts into dist/:
#   prostir-<version>-x86_64-linux.tar.gz   PC part with install.sh
#   prostir-<version>.apk                   headset app
#   THIRD-PARTY-LICENSES.txt                licenses of everything bundled in both
# Signing of the APK: see scripts/build-apk.sh (KEYSTORE…), debug key otherwise.
# The .deb and .rpm are made afterwards from target/release/prostir (release workflow).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
DIST="$ROOT/dist"
NAME="prostir-$VERSION-x86_64-linux"
rm -rf "$DIST/$NAME" && mkdir -p "$DIST/$NAME"

# The APK first: build-apk.sh also writes build/THIRD-PARTY-LICENSES.txt, which every
# package below ships.
PROFILE=release ./scripts/build-apk.sh
cp build/prostir.apk "$DIST/prostir-$VERSION.apk"
cp build/THIRD-PARTY-LICENSES.txt "$DIST/"

cargo build --release --locked -p prostir-host --bin prostir
cp target/release/prostir "$DIST/$NAME/"
strip --strip-debug "$DIST/$NAME/prostir"
mkdir -p "$DIST/$NAME/packaging"
cp packaging/prostir.svg packaging/prostir.desktop "$DIST/$NAME/packaging/"
cp scripts/install.sh LICENSE LICENSE-EXCEPTION.md build/THIRD-PARTY-LICENSES.txt \
    README.md README.uk.md CHANGELOG.md "$DIST/$NAME/"
tar -C "$DIST" -czf "$DIST/$NAME.tar.gz" "$NAME"
rm -rf "$DIST/$NAME"

(cd "$DIST" && sha256sum "$NAME.tar.gz" "prostir-$VERSION.apk" THIRD-PARTY-LICENSES.txt > SHA256SUMS)
echo ">> $DIST"
ls -la "$DIST"
