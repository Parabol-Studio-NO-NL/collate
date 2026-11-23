#!/usr/bin/env bash
# Sign a staged bundle and create its Sparkle archive. Release mode also notarizes the app.
set -euo pipefail
umask 077

mode="${1:?usage: sign-bundle.sh adhoc|release STAGE_DIRECTORY ARCHIVE}"
stage="${2:?Specify the bundle staging directory}"
archive="${3:?Specify the ZIP output path}"
app="$stage/Collate.app"
framework="$app/Contents/Frameworks/Sparkle.framework"
test -d "$framework"
test ! -e "$archive"
test ! -L "$archive"

case "$mode" in
  adhoc)
    signing=(--sign - --timestamp=none)
    runtime=()
    ;;
  release)
    signing=(--sign "${MACOS_SIGN_IDENTITY:?}" --timestamp)
    if [[ -n "${MACOS_KEYCHAIN:-}" ]]; then signing+=(--keychain "$MACOS_KEYCHAIN"); fi
    runtime=(--options runtime)
    : "${APPLE_TEAM_ID:?}"
    if [[ -z "${CODESIGN_NOTARY_PROFILE:-}" ]]; then
      : "${APPLE_NOTARY_KEY_BASE64:?}" "${APPLE_NOTARY_KEY_ID:?}" "${APPLE_NOTARY_ISSUER_ID:?}"
    fi
    lipo "$app/Contents/MacOS/Collate" -verify_arch arm64
    lipo "$app/Contents/MacOS/Collate" -verify_arch x86_64
    ;;
  *) echo "Unknown signing mode: $mode" >&2; exit 2 ;;
esac

archive_dir="$(dirname "$archive")"
mkdir -p "$archive_dir"
scratch="$(mktemp -d "$archive_dir/.codesign-archive.XXXXXX")"
notary_scratch=
trap 'if [[ -n "$notary_scratch" ]]; then rm -rf -- "$notary_scratch"; fi; rm -rf -- "$scratch"' EXIT

sign() {
  codesign --force "${signing[@]}" ${runtime[@]+"${runtime[@]}"} "$@"
}

sign "$framework/Versions/B/XPCServices/Installer.xpc"
sign --preserve-metadata=entitlements "$framework/Versions/B/XPCServices/Downloader.xpc"
sign "$framework/Versions/B/Autoupdate"
sign "$framework/Versions/B/Updater.app"
sign "$framework"
sign "$app"
codesign --verify --strict --deep --verbose=2 "$app"

if [[ "$mode" == release ]]; then
  signature="$(codesign --display --verbose=4 "$app" 2>&1)"
  [[ "$(printf '%s\n' "$signature" | sed -n 's/^TeamIdentifier=//p')" == "$APPLE_TEAM_ID" ]]
  [[ "$signature" == *"Authority=Developer ID Application:"* ]]
  notary_scratch="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/codesign-notary.XXXXXX")"
  if [[ -n "${CODESIGN_NOTARY_PROFILE:-}" ]]; then
    credentials=(--keychain-profile "$CODESIGN_NOTARY_PROFILE")
  else
    key="$notary_scratch/AuthKey.p8"
    printf '%s' "$APPLE_NOTARY_KEY_BASE64" | base64 --decode > "$key"
    unset APPLE_NOTARY_KEY_BASE64
    credentials=(--key "$key" --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID")
  fi
  ditto -c -k --sequesterRsrc --keepParent "$app" "$notary_scratch/Collate.zip"
  xcrun notarytool submit "$notary_scratch/Collate.zip" "${credentials[@]}" \
    --wait --timeout 45m --output-format json > "$notary_scratch/status.json"
  status="$(plutil -extract status raw -o - "$notary_scratch/status.json")"
  if [[ "$status" != Accepted ]]; then
    echo "Apple notarization returned $status for $archive." >&2
    exit 1
  fi
  if [[ -n "${key:-}" ]]; then rm -f "$key"; fi
  for delay in 1 5 20; do
    sleep "$delay"
    if xcrun stapler staple "$app"; then break; fi
  done
  xcrun stapler validate "$app"
fi

zip="$scratch/Collate.zip"
ditto -c -k --sequesterRsrc --keepParent "$app" "$zip"
ditto -x -k "$zip" "$scratch/extracted"
packaged="$scratch/extracted/Collate.app"
codesign --verify --strict --deep --verbose=2 "$packaged"
if [[ "$mode" == release ]]; then
  xcrun stapler validate "$packaged"
  syspolicy_check distribution "$packaged"
fi

# Publish the complete ZIP atomically and refuse a destination created during verification.
python3 - "$zip" "$archive" <<'PY'
import os
import sys

try:
    os.link(sys.argv[1], sys.argv[2])
except OSError as error:
    sys.exit(f"Could not publish the verified archive: {error}")
PY
