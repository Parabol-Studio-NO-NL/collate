#!/usr/bin/env bash
# Download the official universal framework and tools into a new directory.
set -euo pipefail

version=2.10.1
sha256=1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27
destination="${1:?usage: fetch-sparkle.sh NEW_DIRECTORY}"
mkdir "$destination"
archive="$(mktemp "${TMPDIR:-/tmp}/codesign-sparkle.XXXXXX")"
trap 'rm -f "$archive"' EXIT
curl --fail --silent --show-error --location --retry 3 --proto '=https' --proto-redir '=https' \
  "https://github.com/sparkle-project/Sparkle/releases/download/$version/Sparkle-$version.tar.xz" --output "$archive"
printf '%s  %s\n' "$sha256" "$archive" | shasum -a 256 --check
tar -xf "$archive" -C "$destination"
lipo "$destination/Sparkle.framework/Sparkle" -verify_arch arm64
lipo "$destination/Sparkle.framework/Sparkle" -verify_arch x86_64
