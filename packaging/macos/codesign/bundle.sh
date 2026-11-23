#!/usr/bin/env bash
# Build Collate.app in a fresh staging directory. No signing credentials are used.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
cd "$root"

stage="${1:?usage: bundle.sh NEW_OUTPUT_DIRECTORY [universal|arm64|x86_64] [--skip-build]}"
architecture="${2:-universal}"
case "$architecture" in
  universal) targets=(aarch64-apple-darwin x86_64-apple-darwin) ;;
  arm64) targets=(aarch64-apple-darwin) ;;
  x86_64) targets=(x86_64-apple-darwin) ;;
  *) echo "Unrecognized architecture: $architecture" >&2; exit 2 ;;
esac
case "${3:-}" in
  "") skip_build=0 ;;
  --skip-build) skip_build=1 ;;
  *) echo "Unrecognized option: $3" >&2; exit 2 ;;
esac

export MACOSX_DEPLOYMENT_TARGET=12.0
export CODESIGN_VERSION="${CODESIGN_VERSION:-0.1.0-dev}"
export CODESIGN_BUILD_NUMBER="${CODESIGN_BUILD_NUMBER:-1}"
export CODESIGN_UPDATE_CHANNEL="${CODESIGN_UPDATE_CHANNEL:-stable}"
export CODESIGN_UPDATE_FEED="${CODESIGN_UPDATE_FEED:?Set CODESIGN_UPDATE_FEED to the appcast URL}"
export CODESIGN_SPARKLE_PUBLIC_KEY="${CODESIGN_SPARKLE_PUBLIC_KEY:?Set CODESIGN_SPARKLE_PUBLIC_KEY}"
export CODESIGN_COMMIT="$(git rev-parse HEAD)"
export CODESIGN_UPSTREAM_SHA="${CODESIGN_UPSTREAM_SHA:-unknown}"
export SPARKLE_FRAMEWORK_PATH="${SPARKLE_FRAMEWORK_PATH:?Set SPARKLE_FRAMEWORK_PATH to the directory containing Sparkle.framework}"
profile="${CODESIGN_BUILD_PROFILE:-release}"
build_args=()
case "$profile" in
  release) build_args+=(--release) ;;
  debug) ;;
  *) echo "Unrecognized build profile: $profile" >&2; exit 2 ;;
esac

mkdir "$stage"
app="$stage/Collate.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$app/Contents/Frameworks"
python3 "$here/write-plist.py" "$root/packaging/macos/Info.plist.in" "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

binaries=()
for target in "${targets[@]}"; do
  build_args+=(--target "$target")
  binaries+=("${CARGO_TARGET_DIR:-$root/target}/$target/$profile/designcraft")
done
if [[ "$skip_build" = 0 ]]; then
  cargo build --locked -p designcraft --features sparkle "${build_args[@]}"
fi
lipo -create "${binaries[@]}" -output "$app/Contents/MacOS/Collate"
lipo -info "$app/Contents/MacOS/Collate"
ditto "$SPARKLE_FRAMEWORK_PATH/Sparkle.framework" "$app/Contents/Frameworks/Sparkle.framework"
cp assets/app-icon/designcraft.icns "$app/Contents/Resources/Collate.icns"
cp LICENSE-MIT LICENSE-APACHE NOTICE "$app/Contents/Resources/"
cp "$SPARKLE_FRAMEWORK_PATH/LICENSE" "$app/Contents/Resources/LICENSE-Sparkle.txt"
if [[ -n "${CRAFT_FONTS_DIR:-}" ]]; then
  for licence in "$CRAFT_FONTS_DIR"/fonts/*/OFL.txt; do
    if [[ -f "$licence" ]]; then
      cp "$licence" "$app/Contents/Resources/OFL-$(basename "$(dirname "$licence")").txt"
    fi
  done
fi
