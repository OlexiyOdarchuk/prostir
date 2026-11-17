#!/usr/bin/env bash
# Builds build/prostir.apk for Meta Quest (arm64). Needs the Android SDK with NDK
# and build-tools, plus `rustup target add aarch64-linux-android`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
NDK="${ANDROID_NDK_HOME:-$(ls -d "$SDK"/ndk/[0-9]* | sort -V | tail -1)}"
BUILD_TOOLS="$(ls -d "$SDK"/build-tools/[0-9]* | grep -v -- -rc | sort -V | tail -1)"
PLATFORM="$(ls -d "$SDK"/platforms/android-3[4-9]* | sort -V | head -1)"
LOADER_VERSION="${OPENXR_LOADER_VERSION:-1.1.65}"
PROFILE="${PROFILE:-release}"
API=32

TC="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
OUT="$ROOT/build"
STAGE="$OUT/apk"
mkdir -p "$OUT"
rm -rf "$STAGE" && mkdir -p "$STAGE/lib/arm64-v8a"

echo ">> cargo build ($PROFILE) with NDK $(basename "$NDK")"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TC/aarch64-linux-android$API-clang"
export CC_aarch64_linux_android="$TC/aarch64-linux-android$API-clang"
export AR_aarch64_linux_android="$TC/llvm-ar"
cargo_flags=(-p prostir-client --target aarch64-linux-android)
[[ "$PROFILE" == release ]] && cargo_flags+=(--release)
(cd "$ROOT" && cargo build "${cargo_flags[@]}")
cp "$ROOT/target/aarch64-linux-android/$PROFILE/libprostir.so" "$STAGE/lib/arm64-v8a/"
"$TC/llvm-strip" --strip-debug "$STAGE/lib/arm64-v8a/libprostir.so"

AAR="$OUT/openxr_loader_for_android-$LOADER_VERSION.aar"
if [[ ! -f "$AAR" ]]; then
    echo ">> downloading Khronos OpenXR loader $LOADER_VERSION"
    curl -sfL -o "$AAR" "https://repo1.maven.org/maven2/org/khronos/openxr/openxr_loader_for_android/$LOADER_VERSION/openxr_loader_for_android-$LOADER_VERSION.aar"
fi
unzip -p "$AAR" jni/arm64-v8a/libopenxr_loader.so > "$STAGE/lib/arm64-v8a/libopenxr_loader.so"

echo ">> packaging with $(basename "$BUILD_TOOLS") / $(basename "$PLATFORM")"
"$BUILD_TOOLS/aapt2" link -o "$OUT/unaligned.apk" --manifest "$ROOT/client/AndroidManifest.xml" -I "$PLATFORM/android.jar"
(cd "$STAGE" && zip -q -r "$OUT/unaligned.apk" lib)
"$BUILD_TOOLS/zipalign" -f -p 4 "$OUT/unaligned.apk" "$OUT/aligned.apk"

# Signing key, first found wins:
#   1. KEYSTORE, KEYSTORE_PASS, KEY_ALIAS (and KEY_PASS) from the environment (CI);
#   2. a release key in ~/.config/prostir-release/ (prostir-release.jks, password, alias),
#      so local builds can update an installed release and the other way round;
#   3. the Android debug key (created if missing).
RELEASE_KEY_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/prostir-release"
if [[ -z "${KEYSTORE:-}" && -f "$RELEASE_KEY_DIR/prostir-release.jks" ]]; then
    KEYSTORE="$RELEASE_KEY_DIR/prostir-release.jks"
    KEYSTORE_PASS="$(cat "$RELEASE_KEY_DIR/password")"
    KEY_ALIAS="$(cat "$RELEASE_KEY_DIR/alias")"
    echo ">> signing with the release key"
fi
KEYSTORE="${KEYSTORE:-$HOME/.android/debug.keystore}"
KEYSTORE_PASS="${KEYSTORE_PASS:-android}"
KEY_ALIAS="${KEY_ALIAS:-androiddebugkey}"
KEY_PASS="${KEY_PASS:-$KEYSTORE_PASS}"
if [[ ! -f "$KEYSTORE" ]]; then
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass "$KEYSTORE_PASS" -keypass "$KEY_PASS" -alias "$KEY_ALIAS" \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US"
fi
"$BUILD_TOOLS/apksigner" sign --ks "$KEYSTORE" --ks-key-alias "$KEY_ALIAS" \
    --ks-pass "pass:$KEYSTORE_PASS" --key-pass "pass:$KEY_PASS" \
    --out "$OUT/prostir.apk" "$OUT/aligned.apk"
rm -f "$OUT/unaligned.apk" "$OUT/aligned.apk" "$OUT/prostir.apk.idsig"
echo ">> $OUT/prostir.apk"

if [[ "${1:-}" == "--install" ]]; then
    adb install -r "$OUT/prostir.apk"
    adb shell am start -n dev.prostir.client/android.app.NativeActivity
fi
