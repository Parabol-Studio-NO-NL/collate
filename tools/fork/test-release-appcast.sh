#!/usr/bin/env bash
# Exercise the release appcast script with Sparkle and a disposable signing key.
set -euo pipefail
umask 077

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
: "${SPARKLE_FRAMEWORK_PATH:?Fetch the pinned Sparkle tools before running this test.}"

work="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/codesign-feed-test.XXXXXX")"
trap 'rm -rf -- "$work"' EXIT
export CFFIXED_USER_HOME="$work/profile"
mkdir "$CFFIXED_USER_HOME"
openssl genpkey -algorithm ED25519 -out "$work/key.pem"
export SPARKLE_PRIVATE_KEY="$(openssl pkey -in "$work/key.pem" -outform DER | tail -c 32 | openssl base64 -A)"
export CODESIGN_SPARKLE_PUBLIC_KEY="$(openssl pkey -in "$work/key.pem" -pubout -outform DER | tail -c 32 | openssl base64 -A)"
unset CODESIGN_SPARKLE_KEYCHAIN_ACCOUNT

base=https://updates.example.invalid/updates
export CODESIGN_COMMIT="$(git rev-parse HEAD)"
export CODESIGN_UPSTREAM_SHA="$CODESIGN_COMMIT"
printf 'Disposable Collate release notes.\n' > "$work/notes.md"

check_feed() {
  local channel="$1" version="$2" number="$3" prefix archive bundle
  prefix="$base/$channel/releases/codesign-v$version/"
  archive="$work/$channel/Collate-$version.zip"
  bundle="$work/$channel/Collate.app"
  mkdir -p "$bundle/Contents/MacOS"
  cp /usr/bin/true "$bundle/Contents/MacOS/Collate"
  CODESIGN_UPDATE_CHANNEL="$channel" CODESIGN_VERSION="$version" CODESIGN_BUILD_NUMBER="$number" \
    CODESIGN_UPDATE_FEED="$base/$channel/appcast.xml" \
    python3 packaging/macos/codesign/write-plist.py packaging/macos/Info.plist.in "$bundle/Contents/Info.plist"
  codesign --force --sign - --timestamp=none "$bundle"
  ditto -c -k --keepParent "$bundle" "$archive"
  packaging/macos/codesign/appcast.sh "$archive" "$work/notes.md" "$prefix" "$channel"

  python3 - "$work/$channel/appcast.xml" "$archive" "$prefix" "$channel" "$number" <<'PY'
from pathlib import Path
import sys
import xml.etree.ElementTree as ET

feed, path, prefix, channel, build_number = sys.argv[1:]
archive = Path(path)
sparkle = {"sparkle": "http://www.andymatuschak.org/xml-namespaces/sparkle"}
item, = ET.parse(feed).findall("./channel/item")
assert item.findtext("sparkle:channel", namespaces=sparkle) == ("beta" if channel == "beta" else None)
assert item.findtext("sparkle:version", namespaces=sparkle) == build_number
assert "Disposable Collate release notes" in item.findtext("description")
enclosure = item.find("enclosure")
assert enclosure.get("url") == prefix + archive.name
assert enclosure.get("length") == str(archive.stat().st_size)
PY
}

expect_rejection() {
  local expected="$1" archive="$2" prefix="$3" channel="$4"
  if packaging/macos/codesign/appcast.sh "$archive" "$work/notes.md" "$prefix" "$channel" > "$work/rejection.log" 2>&1; then
    echo "Expected appcast generation to fail for $channel at $prefix." >&2
    return 1
  fi
  if ! grep -Fq -- "$expected" "$work/rejection.log"; then
    cat "$work/rejection.log" >&2
    return 1
  fi
}

check_feed stable 1.0.0 100
check_feed beta 1.1.0-beta.1 101
beta="$work/beta/Collate-1.1.0-beta.1.zip"
cp "$work/beta/appcast.xml" "$work/verified-beta.xml"
expect_rejection 'different update channel' "$beta" "$base/stable/releases/codesign-v1.1.0-beta.1/" stable
expect_rejection 'match the packaged appcast' "$beta" 'https://other.example.invalid/updates/beta/releases/codesign-v1.1.0-beta.1/' beta
CODESIGN_SPARKLE_PUBLIC_KEY='+KXPELklrHmCOimjQG+mlzrdQ3R1WepJunQ/BuNeKjI=' \
  expect_rejection 'signed with the configured Collate key' "$beta" "$base/beta/releases/codesign-v1.1.0-beta.1/" beta
cmp "$work/verified-beta.xml" "$work/beta/appcast.xml"
printf 'Signed stable and beta appcasts passed. Invalid channel, host, and key preserved the existing feed.\n'

preflight=packaging/macos/codesign/check-build-number.py
stable_feed="$work/stable/appcast.xml"
beta_feed="$work/beta/appcast.xml"
python3 "$preflight" "$stable_feed" stable - -
python3 "$preflight" "$beta_feed" beta "$stable_feed" -

expect_build_rejection() {
  local expected="$1"
  shift
  if python3 "$preflight" "$@" > "$work/build-rejection.log" 2>&1; then
    echo "Expected the build number check to fail." >&2
    return 1
  fi
  if ! grep -Fq -- "$expected" "$work/build-rejection.log"; then
    cat "$work/build-rejection.log" >&2
    return 1
  fi
}

expect_build_rejection 'Build 100 must be greater than deployed build 101' "$stable_feed" stable - "$beta_feed"
expect_build_rejection 'Build 101 must be greater than deployed build 101' "$beta_feed" beta "$stable_feed" "$beta_feed"
expect_build_rejection 'outside the stable channel' "$beta_feed" beta "$beta_feed" -
expect_build_rejection 'No such file' "$beta_feed" beta "$work/missing.xml" -
python3 - "$stable_feed" "$work/stable-history.xml" "$work/invalid-build.xml" <<'PY'
from copy import deepcopy
import sys
import xml.etree.ElementTree as ET

source, history, invalid = sys.argv[1:]
tree = ET.parse(source)
channel = tree.find("channel")
item = deepcopy(channel.find("item"))
version = item.find("{http://www.andymatuschak.org/xml-namespaces/sparkle}version")
channel.append(item)
version.text = "102"
tree.write(history)
version.text = "1.0"
tree.write(invalid)
PY
expect_build_rejection 'Build 101 must be greater than deployed build 102' "$beta_feed" beta "$work/stable-history.xml" -
expect_build_rejection 'invalid Sparkle build number' "$beta_feed" beta "$work/invalid-build.xml" -
printf 'Appcast build preflight passed. It checked every deployed build and rejected stale or invalid input.\n'
