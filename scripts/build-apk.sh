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
# SHA-256 of openxr_loader_for_android-1.1.65.aar as published on Maven Central. For
# another OPENXR_LOADER_VERSION, pass its hash in OPENXR_LOADER_SHA256.
LOADER_SHA256_DEFAULT=a298c326085b09181c85235fb04d37510939bd30938ed4322c093c09780e8643
if [[ "$LOADER_VERSION" == 1.1.65 ]]; then
    LOADER_SHA256="${OPENXR_LOADER_SHA256:-$LOADER_SHA256_DEFAULT}"
else
    LOADER_SHA256="${OPENXR_LOADER_SHA256:?set OPENXR_LOADER_SHA256 for OpenXR loader $LOADER_VERSION}"
fi
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
# PROSTIR_STORE=1: Meta Horizon Store build, with the entitlement check. Needs the Meta
# Platform SDK unpacked at PROSTIR_PLATFORM_SDK (its libovrplatformloader.so goes into
# the APK). Builds from this repository leave both out.
if [[ "${PROSTIR_STORE:-}" == 1 ]]; then
    : "${PROSTIR_PLATFORM_SDK:?set PROSTIR_PLATFORM_SDK to the unpacked Meta Platform SDK}"
    cargo_flags+=(--features store)
fi
[[ "$PROFILE" == release ]] && cargo_flags+=(--release)
(cd "$ROOT" && cargo build "${cargo_flags[@]}")
cp "$ROOT/target/aarch64-linux-android/$PROFILE/libprostir.so" "$STAGE/lib/arm64-v8a/"
"$TC/llvm-strip" --strip-debug "$STAGE/lib/arm64-v8a/libprostir.so"

AAR="$OUT/openxr_loader_for_android-$LOADER_VERSION.aar"
if [[ ! -f "$AAR" ]]; then
    echo ">> downloading Khronos OpenXR loader $LOADER_VERSION"
    curl -sfL -o "$AAR" "https://repo1.maven.org/maven2/org/khronos/openxr/openxr_loader_for_android/$LOADER_VERSION/openxr_loader_for_android-$LOADER_VERSION.aar"
fi
if ! echo "$LOADER_SHA256  $AAR" | sha256sum -c --quiet -; then
    echo "!! $AAR does not match the expected SHA-256; delete it and try again" >&2
    exit 1
fi
unzip -p "$AAR" jni/arm64-v8a/libopenxr_loader.so > "$STAGE/lib/arm64-v8a/libopenxr_loader.so"
APK=prostir.apk
rm -f "$STAGE/lib/arm64-v8a/libovrplatformloader.so"
if [[ "${PROSTIR_STORE:-}" == 1 ]]; then
    cp "$PROSTIR_PLATFORM_SDK/Android/libs/arm64-v8a/libovrplatformloader.so" "$STAGE/lib/arm64-v8a/"
    APK=prostir-store.apk
    echo ">> store build: Meta Platform SDK included"
fi

# License texts go into the APK as assets/licenses/: Prostir's own (GPL and the
# exception) and the third-party notices for the crates, the font and the loader.
rm -rf "$OUT/assets" && mkdir -p "$OUT/assets/licenses"
python3 "$ROOT/scripts/third-party-licenses.py" --openxr-aar "$AAR" -o "$OUT/THIRD-PARTY-LICENSES.txt"
cp "$ROOT/LICENSE" "$ROOT/LICENSE-EXCEPTION.md" "$OUT/THIRD-PARTY-LICENSES.txt" "$OUT/assets/licenses/"

echo ">> packaging with $(basename "$BUILD_TOOLS") / $(basename "$PLATFORM")"
rm -rf "$OUT/res" && mkdir -p "$OUT/res"
"$BUILD_TOOLS/aapt2" compile --dir "$ROOT/client/res" -o "$OUT/res"
# The APK version follows the workspace version: 1.2.3 -> versionCode 10203. That only
# works for plain MAJOR.MINOR.PATCH with MINOR and PATCH below 100 (a pre-release such as
# 0.2.0-rc.1 would get the same code as 0.2.0, and 0.1.100 the same as 0.2.0).
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
if [[ ! "$VERSION" =~ ^([0-9]+)\.([0-9]|[1-9][0-9])\.([0-9]|[1-9][0-9])$ ]]; then
    echo "!! version $VERSION: the APK versionCode needs MAJOR.MINOR.PATCH with MINOR and PATCH below 100" >&2
    exit 1
fi
v_major="${BASH_REMATCH[1]}" v_minor="${BASH_REMATCH[2]}" v_patch="${BASH_REMATCH[3]}"
link_flags=(--version-name "$VERSION" --version-code "$((v_major * 10000 + v_minor * 100 + v_patch))" --replace-version)
# PROSTIR_DEBUG=1: debuggable build (adb run-as can read the app's files). Never for releases.
if [[ "${PROSTIR_DEBUG:-}" == 1 ]]; then
    link_flags+=(--debug-mode)
    echo ">> debuggable build"
fi
"$BUILD_TOOLS/aapt2" link -o "$OUT/unaligned.apk" --manifest "$ROOT/client/AndroidManifest.xml" \
    -I "$PLATFORM/android.jar" -A "$OUT/assets" "$OUT"/res/*.flat "${link_flags[@]}"
(cd "$STAGE" && zip -q -r "$OUT/unaligned.apk" lib)
"$BUILD_TOOLS/zipalign" -f -p 4 "$OUT/unaligned.apk" "$OUT/aligned.apk"

# Signing key, first found wins:
#   1. KEYSTORE, KEYSTORE_PASS, KEY_ALIAS (and KEY_PASS) from the environment (CI);
#   2. a release key in ~/.config/prostir-release/ (prostir-release.jks, password, alias),
#      so local builds can update an installed release and the other way round;
#   3. the Android debug key (created if missing).
# REQUIRE_RELEASE_KEY=1 (the release workflow) refuses step 3: an APK signed with a
# throwaway key could not update an installed Prostir.
RELEASE_KEY_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/prostir-release"
if [[ -z "${KEYSTORE:-}" && -f "$RELEASE_KEY_DIR/prostir-release.jks" ]]; then
    KEYSTORE="$RELEASE_KEY_DIR/prostir-release.jks"
    KEYSTORE_PASS="$(cat "$RELEASE_KEY_DIR/password")"
    KEY_ALIAS="$(cat "$RELEASE_KEY_DIR/alias")"
    echo ">> signing with the release key"
fi
if [[ -n "${KEYSTORE:-}" ]]; then
    if [[ ! -f "$KEYSTORE" ]]; then
        echo "!! KEYSTORE=$KEYSTORE does not exist" >&2
        exit 1
    fi
    : "${KEYSTORE_PASS:?KEYSTORE is set, so KEYSTORE_PASS must be too}"
    : "${KEY_ALIAS:?KEYSTORE is set, so KEY_ALIAS must be too}"
    debug_key=0
elif [[ "${REQUIRE_RELEASE_KEY:-}" == 1 ]]; then
    echo "!! REQUIRE_RELEASE_KEY=1 but no release key: set KEYSTORE, KEYSTORE_PASS and KEY_ALIAS" >&2
    exit 1
else
    debug_key=1
    echo ">> signing with the Android debug key (this APK cannot update a release install)"
fi
KEYSTORE="${KEYSTORE:-$HOME/.android/debug.keystore}"
KEYSTORE_PASS="${KEYSTORE_PASS:-android}"
KEY_ALIAS="${KEY_ALIAS:-androiddebugkey}"
KEY_PASS="${KEY_PASS:-$KEYSTORE_PASS}"
if (( debug_key )) && [[ ! -f "$KEYSTORE" ]]; then
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass "$KEYSTORE_PASS" -keypass "$KEY_PASS" -alias "$KEY_ALIAS" \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug,O=Android,C=US"
fi
"$BUILD_TOOLS/apksigner" sign --ks "$KEYSTORE" --ks-key-alias "$KEY_ALIAS" \
    --ks-pass "pass:$KEYSTORE_PASS" --key-pass "pass:$KEY_PASS" \
    --out "$OUT/$APK" "$OUT/aligned.apk"
rm -f "$OUT/unaligned.apk" "$OUT/aligned.apk" "$OUT/$APK.idsig"
# The certificate digest tells whether two APKs (say, GitHub and Store) can update each other.
"$BUILD_TOOLS/apksigner" verify --print-certs "$OUT/$APK" | grep -m1 'SHA-256' || true
echo ">> $OUT/$APK"

if [[ "${1:-}" == "--install" ]]; then
    adb install -r "$OUT/$APK"
    adb shell am start -n dev.prostir.client/android.app.NativeActivity
fi
